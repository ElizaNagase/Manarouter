//! The Cursor transport: one server-streaming Connect-RPC call, framed by hand, fed by appends.
//!
//! # Why this is not an [`crate::upstream::Upstream`] request
//!
//! Every other provider in this relay is asked a question and answers it: one HTTP request in, one
//! response out, which a pooled client can express. Cursor's agent endpoint is a *conversation*.
//! `RunSSE` opens an HTTP/2 stream that stays open for the whole turn, the server sends protobuf
//! messages asking for things it cannot know (the tool definitions this client was configured with,
//! a key-value blob it wrote in an earlier turn, permission for a shell command), and the client
//! has to answer those questions while the answer is being written.
//!
//! Those answers do **not** travel on the run stream. The stream's only request body is a
//! `BidiRequestId` ([`cursor::bidi_request_id`]), sent whole and closed; every later client message
//! is a unary `BidiAppend` naming that id ([`cursor::bidi_append_request`]). That shape is the
//! installed CLI's, and it is visible on the wire: the bidi `Run` path answers a request head with
//! `200` and then never grants flow-control capacity for a body, so a client that writes its turn
//! onto that stream waits forever and the turn never starts.
//!
//! So the exchange is behind [`Exchange`] (one verb: `open`), the client's side of the conversation
//! is behind [`ClientSink`] (one verb: `append`), and the production implementations fill both with
//! `h2` while a test fills them with canned bytes. The driver — the part that decides what to
//! answer, when a turn is over and what the client sees — is the same code in both cases, and it is
//! the part worth testing, because it is where a wrong guess turns into a silent stall.
//!
//! # What is deliberately *not* here
//!
//! - **Proxies.** The relay can egress through a CONNECT proxy, but the pool that does it is
//!   hyper's, and hyper's client cannot hand out a raw `h2` stream to write to. Rather than bypass
//!   an operator's egress policy, the relay refuses a Cursor provider that has a proxy configured
//!   (see `server.rs`), with a message that says exactly that.
//! - **Client-version probing.** The reference client reads its version from its own install and
//!   caches it on disk for six hours; this transport reads [`cursor::CLIENT_VERSION_ENV`] and
//!   otherwise sends the pinned version. A wrong pin is a rejected request, so the pin is checked
//!   against the shape the server accepts before it is used (see [`cursor::is_client_version`]).
//! - **Tool execution.** The server can ask the client to run `read_file`, `grep`, a shell command
//!   and so on. This transport answers the obligations a text-only client genuinely has (the
//!   request context and the KV blob) and reports anything else as an error instead of guessing,
//!   because a wrong `exec` result is worse than a failed turn.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::future::BoxFuture;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use mr_dialects::cursor::{self, FrameDecoder, PreparedRun, RequestIdentity, StreamState};
use mr_dialects::{TranslateCtx, TranslateError};

/// How long the whole turn may take before the partial answer is closed with `length`.
///
/// The reference client uses the same five minutes. A turn that outlives it is not going to finish,
/// and a client left waiting forever is worse than a truncated answer it can see is truncated.
pub const TOTAL_TIMEOUT: Duration = Duration::from_millis(cursor::STREAM_TIMEOUT_MS);

/// Whether a turn asks the server not to keep its contents as workspace context.
///
/// Ghost mode is the CLI's name for that choice. The transport never sends workspace context anyway
/// (there is none to send), so the flag changes nothing about what a turn contains; `true` is the
/// reference client's default and the one whose behaviour has been observed.
pub const GHOST_MODE_DEFAULT: bool = true;

/// How long the response head is held back, waiting for the turn's first decision.
///
/// This is not a timeout for the *turn*; it bounds only how long a client is made to wait for a
/// refusal it will never see. Cursor's in-band refusals arrive within a few hundred milliseconds of
/// the append, so five seconds is generous for them, and a turn whose first token is slower has
/// already committed: it streams exactly as it did before this gate existed. The cost of holding the
/// head is a slightly later first byte; the cost of not holding it is a refusal the relay cannot act
/// on, because by then the client is already reading a `200`.
#[cfg(not(test))]
const FIRST_ANSWER_GRACE: Duration = Duration::from_secs(5);

/// The same window in tests, short enough that a test scripting a silent server does not spend
/// seconds waiting for a refusal that is never coming. Only the wait shrinks; what the pump decides
/// and what the relay does with it are identical.
#[cfg(test)]
const FIRST_ANSWER_GRACE: Duration = Duration::from_millis(50);

/// The largest non-200 body this transport will buffer.
///
/// A refusal is a few hundred bytes of JSON. Reading an unbounded refusal body would let a
/// misbehaving host spend the relay's memory, and the relay's error path only looks at the text.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// Anything that stops a Cursor turn.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The client's request cannot be expressed as a Cursor run.
    ///
    /// Becomes a `400` for the client: the request is what is wrong, not the provider.
    #[error("{0}")]
    Request(String),

    /// The provider is configured in a way this transport cannot honour.
    ///
    /// Reported instead of sending a request that silently ignores the configuration.
    #[error("{0}")]
    Config(String),

    /// The exchange could not be opened, or died mid-turn.
    #[error("cursor transport: {0}")]
    Transport(String),

    /// The server sent bytes this codec cannot read.
    #[error(transparent)]
    Codec(#[from] cursor::CursorError),
}

/// One request to open a stream.
#[derive(Debug, Clone)]
pub struct ExchangeRequest {
    /// The validated `https://host[:port]` origin.
    pub origin: String,
    /// The RPC path: [`cursor::RUN_SSE_PATH`] or [`cursor::BIDI_APPEND_PATH`].
    pub path: &'static str,
    /// The complete header list, credential included.
    pub headers: Vec<(String, String)>,
    /// The whole request body, sent with the head and closed with it.
    ///
    /// One message and then end-of-stream: every call this transport makes is unary in the request
    /// direction, including the run, whose later client messages go through [`ClientSink`] instead.
    pub body: Bytes,
}

impl ExchangeRequest {
    fn header_map(&self) -> Result<HeaderMap, Error> {
        let mut map = HeaderMap::new();
        for (name, value) in &self.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Error::Transport("a header name is not valid".to_owned()))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| Error::Transport("a header value is not valid".to_owned()))?;
            map.insert(name, value);
        }
        Ok(map)
    }
}

/// A live response stream.
///
/// One verb: the read. The request direction is closed as the stream opens, so there is nothing to
/// write here — the client's messages go through [`ClientSink`] — and a test that wants to prove
/// the read side is all it needs to fake.
pub trait ExchangeStream: Send {
    /// The next bytes the server wrote, or `None` at a clean end of stream.
    fn next_chunk<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Bytes>, Error>>;
}

/// The client's side of a turn: the messages the server waits for.
///
/// Separate from [`ExchangeStream`] because it is a different HTTP exchange with a different
/// lifetime — a unary call per message, each on the same connection to the same host, all naming the
/// id the run was opened with. Folding it into the stream would mean the driver had to know which
/// of two shapes each write needs.
pub trait ClientSink: Send {
    /// Deliver one client message. The bytes are the bare protobuf message, unframed.
    fn append<'a>(&'a mut self, payload: Vec<u8>) -> BoxFuture<'a, Result<(), Error>>;
}

/// An opened stream, with the response head.
pub struct ExchangeResponse {
    /// The HTTP status.
    pub status: StatusCode,
    /// The response headers.
    pub headers: HeaderMap,
    /// The body, as frames.
    pub stream: Box<dyn ExchangeStream>,
}

/// How a connection to Cursor is opened.
///
/// A trait rather than a concrete client so the driver can be tested against canned frames, and so
/// the proxy limitation has one obvious place to live.
pub trait Exchange: Send + Sync {
    /// Open a stream. The response head is awaited; body frames are not.
    fn open<'a>(
        &'a self,
        request: ExchangeRequest,
    ) -> BoxFuture<'a, Result<ExchangeResponse, Error>>;
}

/// A request that has been shaped but not sent.
pub struct Prepared {
    /// The encoded run request and the model id the frame names.
    pub run: PreparedRun,
    /// Headers for the run request.
    headers: Vec<(String, String)>,
    /// Headers for each append: the same credential and fingerprint, a unary content type.
    append_headers: Vec<(String, String)>,
    /// Where the turn is opened, and where every one of its appends goes.
    ///
    /// One origin for both halves: the append names a stream that this server is holding, so the
    /// two must not be split across hosts.
    pub origin: String,
    /// The client version actually sent, for the log line that explains a rejected request.
    pub client_version: String,
    /// The id the run is opened with, which every later client message has to name.
    ///
    /// The same UUID the request headers already carry as `x-request-id`: the server correlates the
    /// stream, the appends and the trace by it, so minting a second one would split the turn.
    pub request_id: String,
}

/// Shape a request, failing before anything is sent if it cannot be expressed.
///
/// `authorization` is the header value the relay's own credential path produced, so the credential
/// kind is decided in exactly one place; this transport never reads a vault. `origin_hint` is the
/// provider's configured base URL, which is where the turn goes: this transport is the reference
/// client's bidi-disabled shape, and in that mode the client points its agent client at its own
/// backend rather than at a discovered agent host. See [`cursor::backend_origin`].
pub fn prepare(
    ctx: &TranslateCtx<'_>,
    body: &[u8],
    authorization: &str,
    ghost_mode: bool,
    origin_hint: Option<&str>,
) -> Result<Prepared, Error> {
    if authorization.trim().is_empty() {
        return Err(Error::Config(
            "no credential was attached for this Cursor provider; its account needs a `bearer` \
             credential before a turn can be opened"
                .to_owned(),
        ));
    }
    let run =
        cursor::prepare_run(ctx, body, &mr_core::ids::new_id()).map_err(|error| match error {
            TranslateError::MalformedRequest { detail, .. } => Error::Request(detail),
            TranslateError::MalformedResponse { detail, .. } => Error::Request(detail),
            other => Error::Request(other.to_string()),
        })?;

    let client_version = resolve_client_version();
    let identity = new_identity()?;
    let headers = cursor::request_headers(authorization, ghost_mode, &client_version, &identity)?;
    let append_headers =
        cursor::append_headers(authorization, ghost_mode, &client_version, &identity)?;

    Ok(Prepared {
        run,
        headers,
        append_headers,
        origin: cursor::backend_origin(origin_hint),
        client_version,
        request_id: identity.request_id,
    })
}

/// The version sent in `x-cursor-client-version`.
///
/// An override is honoured only when it is shaped like a real build id: an operator typo must not
/// become a rejected request, and the pin is known to be accepted.
fn resolve_client_version() -> String {
    match std::env::var(cursor::CLIENT_VERSION_ENV) {
        Ok(value) if cursor::is_client_version(&value) => value,
        Ok(_) => {
            tracing::warn!(
                variable = cursor::CLIENT_VERSION_ENV,
                "ignoring a Cursor client version that is not shaped like cli-YYYY.MM.DD-<hex>"
            );
            cursor::PINNED_CLIENT_VERSION.to_owned()
        }
        Err(_) => cursor::PINNED_CLIENT_VERSION.to_owned(),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

fn random_bytes<const N: usize>() -> [u8; N] {
    use rand::RngCore as _;
    let mut bytes = [0u8; N];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
}

/// A fresh request id and trace context.
///
/// The bits are set the way RFC 4122 and W3C trace-context require rather than by copying a template:
/// the server parses both, and a malformed id is a rejected request that looks like a credential
/// problem.
fn new_identity() -> Result<RequestIdentity, Error> {
    let mut uuid = random_bytes::<16>();
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    let request_id = format!(
        "{}-{}-{}-{}-{}",
        hex(&uuid[0..4]),
        hex(&uuid[4..6]),
        hex(&uuid[6..8]),
        hex(&uuid[8..10]),
        hex(&uuid[10..16])
    );
    let trace = random_bytes::<24>();
    let traceparent = format!("00-{}-{}-01", hex(&trace[0..16]), hex(&trace[16..24]));
    let identity = RequestIdentity {
        request_id,
        traceparent,
    };
    identity
        .validate()
        .map_err(|field| Error::Transport(format!("generated an invalid {field}")))?;
    Ok(identity)
}

/// What the pump decided about a turn before the client was shown anything.
///
/// The decision has to exist because a Cursor turn is accepted before the server has looked at it:
/// the refusal for a model the account's plan does not include arrives a fraction of a second later,
/// *inside* the stream. Handing the head to the client first would spend the relay's one chance to
/// fail the account over — the refusal could then only arrive as a `200` carrying an error object,
/// and the next attempt would be routed to the same account and refused in exactly the same way.
enum Verdict {
    /// The turn is producing an answer; the client may be shown it.
    Answer,
    /// The turn will produce nothing, and the client has seen nothing.
    Refused {
        status: StatusCode,
        /// The error body the relay reads its refusal out of.
        body: Bytes,
    },
}

/// Where the pump publishes the turn: the client's byte stream, and the first answer owed to the
/// caller while the response head is still unsent.
///
/// The two travel together because they are both the *output* half of the transport: the pump reads
/// from the server and writes here. Keeping the oneshot inside this value is also what makes
/// "already decided" a single state the pump owns, rather than a parameter every helper must be
/// handed separately.
struct Answers {
    tx: tokio::sync::mpsc::Sender<Bytes>,
    /// `None` once decided, or when the relay stopped waiting and the client already has the head.
    verdict: Option<tokio::sync::oneshot::Sender<Verdict>>,
}

impl Answers {
    /// Whether the relay is still waiting, so a refusal can still replace the response head.
    fn waiting(&self) -> bool {
        self.verdict.is_some()
    }

    /// Deliver the pump's first decision, if the relay is still waiting for one.
    ///
    /// `false` means the grace window closed first. The caller must then behave exactly as it did
    /// before the gate existed, because the client is already reading the stream and there is
    /// nothing left to take back.
    fn decide(&mut self, decision: Verdict) -> bool {
        match self.verdict.take() {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }
}

/// The error object a refusal becomes.
///
/// It is shaped like an OpenAI error because that is what the client's own error path renders, and it
/// keeps the server's sentence: an operator who sees "Model unavailable on Free" in a `429` knows
/// exactly what to change, which is the difference between this and a bare status code. The `type` is
/// the relay's own classification — `model_unavailable` means the plan lacks this model, not that the
/// account is spent — and it is what `server.rs` reads to park one model rather than the account.
fn refusal_body(refusal: &cursor::Refusal, status: StatusCode) -> Bytes {
    let kind = if refusal.model_unavailable {
        "model_unavailable"
    } else if status == StatusCode::BAD_REQUEST {
        "invalid_request_error"
    } else {
        "upstream_error"
    };
    Bytes::from(
        serde_json::json!({
            "error": {
                "type": kind,
                "code": refusal.code,
                "message": refusal.message,
            }
        })
        .to_string(),
    )
}

/// The error object for a turn that died before producing anything.
///
/// `upstream_error` and not `model_unavailable`: a connection that reset says nothing about the
/// account's plan, so the relay must fail over without narrowing the cooldown to one model.
fn failure_body(message: &str) -> Bytes {
    Bytes::from(
        serde_json::json!({
            "error": {
                "type": "upstream_error",
                "code": "transport_error",
                "message": message,
            }
        })
        .to_string(),
    )
}

fn json_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers
}

/// Run one turn and hand back a response the relay can render like any other.
///
/// The returned body is a channel: the caller gets the response head as soon as the server answers
/// it, and SSE bytes arrive as the turn produces them. Buffering would defeat the point of streaming
/// and would make the relay's first-byte timeout measure the whole answer.
///
/// The head is held for [`FIRST_ANSWER_GRACE`] anyway, for one reason: the server accepts the turn
/// before it validates it, so a refusal that the relay must act on arrives in-band. See [`Verdict`].
pub async fn run(
    exchange: Arc<dyn Exchange>,
    prepared: Prepared,
    total_timeout: Duration,
) -> Result<crate::upstream::UpstreamResponse, Error> {
    let started = Instant::now();
    let origin = prepared.origin.clone();

    // The whole request side: the id the appends will name, and nothing else. See
    // `cursor::bidi_request_id` for why the turn cannot be written onto this stream.
    let request = ExchangeRequest {
        origin: origin.clone(),
        path: cursor::RUN_SSE_PATH,
        headers: prepared.headers.clone(),
        body: Bytes::from(cursor::bidi_request_body(&prepared.request_id)),
    };
    tracing::debug!(
        origin = %request.origin,
        path = request.path,
        bytes = request.body.len(),
        "cursor: opening the turn stream"
    );
    let opened = exchange.open(request).await?;
    tracing::debug!(
        status = opened.status.as_u16(),
        "cursor: the turn stream answered"
    );
    if opened.status != StatusCode::OK {
        let (body, _) = collect(opened.stream, MAX_ERROR_BODY).await?;
        return Ok(crate::upstream::UpstreamResponse {
            status: opened.status,
            headers: opened.headers,
            body: crate::upstream::Body::Complete(body),
            ttfb: started.elapsed(),
            reused_connection: false,
        });
    }

    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let state = StreamState::new(
        prepared.run.model.clone(),
        format!("chatcmpl-{}", hex(&random_bytes::<12>())),
        created,
    );
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(16);
    let (verdict_tx, verdict_rx) = tokio::sync::oneshot::channel::<Verdict>();
    let ttfb = started.elapsed();
    let sink = AppendSink {
        exchange,
        origin,
        request_id: prepared.request_id.clone(),
        headers: prepared.append_headers.clone(),
        seqno: 0,
    };
    tokio::spawn(async move {
        pump(
            opened.stream,
            sink,
            prepared.run.payload,
            state,
            started,
            total_timeout,
            Answers {
                tx,
                verdict: Some(verdict_tx),
            },
        )
        .await;
    });

    // Hold the head until the turn has either produced something or been refused.
    //
    // A healthy turn pays nothing measurable for this: the pump signals on the first bytes it would
    // have flushed, and the head then goes out as before. The deadline is what keeps a silent server
    // from becoming a hang — on it the turn is committed and streams as it always did, because a slow
    // first token is not a refusal and must not be turned into one.
    if let Ok(Ok(Verdict::Refused { status, body })) =
        tokio::time::timeout(FIRST_ANSWER_GRACE, verdict_rx).await
    {
        tracing::warn!(
            status = status.as_u16(),
            body = %String::from_utf8_lossy(&body),
            "cursor: the turn was refused before it produced an answer; reporting it in place of the stream so the account can be failed over"
        );
        return Ok(crate::upstream::UpstreamResponse {
            status,
            headers: json_headers(),
            body: crate::upstream::Body::Complete(body),
            ttfb: started.elapsed(),
            reused_connection: false,
        });
    }

    Ok(crate::upstream::UpstreamResponse {
        status: StatusCode::OK,
        headers: sse_headers(),
        body: crate::upstream::Body::Channel(crate::upstream::ChannelBody::new(rx)),
        ttfb,
        reused_connection: false,
    })
}

/// The client's side of a turn: one unary `BidiAppend` per message.
///
/// Every message is a fresh HTTP/2 request to the same host, naming the id the run was opened with
/// and carrying the next `append_seqno`. That is more round trips than a bidi stream would need,
/// and it is the only shape the server accepts: a body written on the run stream is never granted
/// flow-control capacity, so the turn never starts.
struct AppendSink {
    exchange: Arc<dyn Exchange>,
    /// The agent host the run was opened against; appends must reach the same one.
    origin: String,
    request_id: String,
    headers: Vec<(String, String)>,
    /// The next `append_seqno`, starting at zero as the reference client does.
    seqno: u32,
}

impl ClientSink for AppendSink {
    fn append<'a>(&'a mut self, payload: Vec<u8>) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let seqno = self.seqno;
            self.seqno = self.seqno.saturating_add(1);
            let body = cursor::bidi_append_request(&self.request_id, seqno, &payload);
            let opened = self
                .exchange
                .open(ExchangeRequest {
                    origin: self.origin.clone(),
                    path: cursor::BIDI_APPEND_PATH,
                    headers: self.headers.clone(),
                    body: Bytes::from(body),
                })
                .await?;
            let status = opened.status;
            let (reply, _) = collect(opened.stream, MAX_ERROR_BODY).await?;
            if status.is_success() {
                tracing::debug!(
                    bytes = payload.len(),
                    seqno,
                    "cursor: the client message was appended"
                );
                return Ok(());
            }
            // The server's own words are the only way to tell a malformed append from a rejected
            // credential, and they are the same class of text the relay already returns to its own
            // client for a failed upstream. Bounded, because a reply is not trusted to be short.
            let detail: String = String::from_utf8_lossy(&reply)
                .trim()
                .chars()
                .take(300)
                .collect();
            Err(Error::Transport(format!(
                "the client message was refused with {} (append {seqno}): {detail}",
                status.as_u16()
            )))
        })
    }
}

fn sse_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    headers
}

async fn collect(
    mut stream: Box<dyn ExchangeStream>,
    cap: usize,
) -> Result<(Bytes, Duration), Error> {
    let started = Instant::now();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next_chunk().await? {
        let room = cap.saturating_sub(out.len());
        if room == 0 {
            break;
        }
        if chunk.len() > room {
            out.extend_from_slice(&chunk[..room]);
            break;
        }
        out.extend_from_slice(&chunk);
    }
    Ok((Bytes::from(out), started.elapsed()))
}

/// The frame pump: read, answer, render, until the turn ends.
async fn pump(
    mut stream: Box<dyn ExchangeStream>,
    mut sink: AppendSink,
    first: Vec<u8>,
    mut state: StreamState,
    started: Instant,
    total_timeout: Duration,
    mut answers: Answers,
) {
    tracing::debug!(bytes = first.len(), "cursor: appending the run request");
    if let Err(error) = sink.append(first).await {
        // Without the run request the turn cannot start, so there is nothing to read: report it as
        // a transport failure rather than waiting out the timeout on a stream that will stay silent.
        let message = error.to_string();
        failed_before_answer(&mut state, &mut sink, &mut answers, &message).await;
        return;
    }
    let deadline = started + total_timeout;
    let mut decoder = FrameDecoder::new();
    let mut grace: Option<Instant> = None;
    'turn: loop {
        let now = Instant::now();
        if now >= deadline {
            let effects = state.on_timeout();
            flush(&mut sink, effects, &answers.tx).await;
            break;
        }
        // A composer turn can end without a `turn_ended`: the answer arrives, then the client is
        // asked to write a blob and nothing else follows. The grace window is what turns that into
        // an end instead of a five-minute wait.
        let read_by = grace.map_or(deadline, |at| at.min(deadline));
        let read_by = tokio::time::Instant::from_std(read_by);
        match tokio::time::timeout_at(read_by, stream.next_chunk()).await {
            Err(_) => {
                let effects = match grace {
                    Some(at) if at < deadline => state.on_kv_grace_expired(),
                    _ => state.on_timeout(),
                };
                flush(&mut sink, effects, &answers.tx).await;
                break;
            }
            Ok(Ok(None)) => {
                if answers.waiting() && !state.has_text() {
                    // The server closed the stream without writing an answer and without saying
                    // why. That is not something the client should be handed as a `200`: another
                    // account, or the same one later, may well answer.
                    let body = failure_body("the turn ended before it produced an answer");
                    if answers.decide(Verdict::Refused {
                        status: StatusCode::BAD_GATEWAY,
                        body,
                    }) {
                        break;
                    }
                }
                let effects = state.on_server_end();
                flush(&mut sink, effects, &answers.tx).await;
                break;
            }
            Ok(Err(error)) => {
                let message = error.to_string();
                failed_before_answer(&mut state, &mut sink, &mut answers, &message).await;
                break;
            }
            Ok(Ok(Some(chunk))) => {
                // A stall is the failure this transport is most likely to meet, and it is
                // indistinguishable from a slow model unless the bytes that did arrive are
                // visible: a server that answers the head and then nothing is waiting for
                // something this client never sent.
                tracing::trace!(
                    bytes = chunk.len(),
                    elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "cursor: the server sent bytes"
                );
                let frames = match decoder.push(&chunk) {
                    Ok(frames) => frames,
                    Err(error) => {
                        let message = error.to_string();
                        failed_before_answer(&mut state, &mut sink, &mut answers, &message).await;
                        break;
                    }
                };
                for frame in frames {
                    tracing::trace!(
                        bytes = frame.payload.len(),
                        trailer = frame.trailer,
                        tag = frame.payload.first().copied().unwrap_or(0),
                        "cursor: frame decoded"
                    );
                    // A refusal that lands before any answer is the account's, not the client's, and
                    // the relay can only act on it while the head is still unsent.
                    if answers.waiting() && !state.has_text() {
                        if let Some(refusal) = cursor::refusal(&frame) {
                            let status = StatusCode::from_u16(refusal.status())
                                .unwrap_or(StatusCode::BAD_GATEWAY);
                            let body = refusal_body(&refusal, status);
                            if answers.decide(Verdict::Refused { status, body }) {
                                break 'turn;
                            }
                        }
                    }
                    let elapsed_ms =
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let effects = match state.on_frame(&frame, elapsed_ms) {
                        Ok(effects) => effects,
                        Err(error) => {
                            let message = error.to_string();
                            failed_before_answer(&mut state, &mut sink, &mut answers, &message)
                                .await;
                            break 'turn;
                        }
                    };
                    let next_grace = effects.kv_grace_deadline_ms;
                    let ended = effects.ended;
                    // The first bytes that would have reached the client commit the turn: from here
                    // on a failure is rendered in-stream, exactly as it always was.
                    if !effects.sse.is_empty() {
                        answers.decide(Verdict::Answer);
                    }
                    flush(&mut sink, effects, &answers.tx).await;
                    if ended {
                        break 'turn;
                    }
                    if let Some(ms) = next_grace {
                        grace = Some(started + Duration::from_millis(ms));
                    }
                }
            }
        }
    }

    if let Err(error) = decoder.finish() {
        let message = error.to_string();
        failed_before_answer(&mut state, &mut sink, &mut answers, &message).await;
    }
    // Dropping the sender ends the body, which is what tells the relay's renderer the stream is
    // over; the `[DONE]` frame was already written above.
}

/// Report a turn that died before it produced anything.
///
/// While the relay is still waiting for a verdict the failure *becomes* the response, which is what
/// lets the account be failed over. Once it has stopped waiting the old in-stream rendering applies,
/// because the client is already reading the stream and the failure has to be visible there.
async fn failed_before_answer(
    state: &mut StreamState,
    sink: &mut AppendSink,
    answers: &mut Answers,
    message: &str,
) {
    let refused = Verdict::Refused {
        status: StatusCode::BAD_GATEWAY,
        body: failure_body(message),
    };
    if answers.decide(refused) {
        return;
    }
    let effects = state.on_transport_error(message);
    flush(sink, effects, &answers.tx).await;
}

async fn flush(
    sink: &mut dyn ClientSink,
    effects: cursor::Effects,
    tx: &tokio::sync::mpsc::Sender<Bytes>,
) {
    for payload in effects.outbound {
        if sink.append(payload).await.is_err() {
            // An answer that cannot be delivered means the server's question is unanswered; the
            // read side reports the end and the client keeps whatever was rendered so far.
            return;
        }
    }
    if !effects.sse.is_empty() && tx.send(Bytes::from(effects.sse)).await.is_err() {
        // The client hung up. Nothing left to render.
    }
}

// ---------------------------------------------------------------------------------------------
// Production exchange: direct HTTP/2 over TLS
// ---------------------------------------------------------------------------------------------

/// The `h2` exchange, direct connections only.
///
/// # The proxy limitation, spelled out
///
/// An operator can route a provider through a CONNECT proxy. That path is implemented by this
/// crate's own connector, which produces a hyper client; hyper's client owns its connection and does
/// not expose a bidirectional stream. Bypassing the proxy would break an egress policy the operator
/// deliberately configured, so the relay refuses a Cursor provider that has one rather than quietly
/// going direct. Fixing it properly means teaching the egress connector to hand back a raw stream,
/// which is a change to the proxy layer, not to this file.
pub struct DirectH2Exchange {
    tls: tokio_rustls::TlsConnector,
}

impl DirectH2Exchange {
    /// Build the TLS client.
    ///
    /// Only `h2` is offered: a server that does not negotiate it cannot serve this RPC, and
    /// advertising HTTP/1.1 would let a connection succeed and then fail on the first frame.
    pub fn new() -> Result<Self, Error> {
        static CONFIG: OnceLock<Result<std::sync::Arc<rustls::ClientConfig>, String>> =
            OnceLock::new();
        let config = CONFIG.get_or_init(|| {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let mut config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            config.alpn_protocols = vec![b"h2".to_vec()];
            config.resumption = rustls::client::Resumption::in_memory_sessions(256);
            Ok(std::sync::Arc::new(config))
        });
        match config {
            Ok(config) => Ok(Self {
                tls: tokio_rustls::TlsConnector::from(std::sync::Arc::clone(config)),
            }),
            Err(message) => Err(Error::Config(message.clone())),
        }
    }
}

/// The exchange the relay uses, built once per process.
///
/// Handed out as an [`Arc`] because a turn's pump outlives the call that opened it: the appends it
/// makes happen after `run` has already returned the response head, so the exchange has to be owned
/// by the task, not borrowed from the caller.
pub fn default_exchange() -> Result<Arc<dyn Exchange>, Error> {
    static EXCHANGE: OnceLock<Result<Arc<DirectH2Exchange>, String>> = OnceLock::new();
    let built = EXCHANGE.get_or_init(|| {
        DirectH2Exchange::new()
            .map(Arc::new)
            .map_err(|error| error.to_string())
    });
    match built {
        Ok(exchange) => Ok(Arc::clone(exchange) as Arc<dyn Exchange>),
        Err(message) => Err(Error::Config(message.clone())),
    }
}

/// The read half of an opened stream, holding the write half alive.
///
/// Nothing writes through `send` again: the request body went out whole with the head. It is kept
/// rather than dropped because dropping a send side that the connection has not finished flushing
/// resets the stream, and a reset run stream is a turn that never starts.
struct H2Stream {
    _send: h2::SendStream<Bytes>,
    recv: h2::RecvStream,
}

impl ExchangeStream for H2Stream {
    fn next_chunk<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Bytes>, Error>> {
        Box::pin(async move {
            match futures_util::future::poll_fn(|cx| self.recv.poll_data(cx)).await {
                None => Ok(None),
                Some(Err(error)) => Err(Error::Transport(error.to_string())),
                Some(Ok(data)) => {
                    let len = data.len();
                    let _ = self.recv.flow_control().release_capacity(len);
                    Ok(Some(data))
                }
            }
        })
    }
}

impl Exchange for DirectH2Exchange {
    fn open<'a>(
        &'a self,
        request: ExchangeRequest,
    ) -> BoxFuture<'a, Result<ExchangeResponse, Error>> {
        Box::pin(async move {
            let authority = request.origin.clone();
            let (host, port) = split_origin(&authority)?;
            let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
                .await
                .map_err(|error| Error::Transport(format!("connect to {authority}: {error}")))?;
            // Without this, a small frame split across segments waits on a delayed ACK — the same
            // latency trap the pooled client avoids.
            let _ = tcp.set_nodelay(true);
            let server_name =
                rustls::pki_types::ServerName::try_from(host.clone()).map_err(|_| {
                    Error::Transport("the host is not a valid TLS server name".to_owned())
                })?;
            let tls = self
                .tls
                .connect(server_name, tcp)
                .await
                .map_err(|error| Error::Transport(format!("tls to {authority}: {error}")))?;

            let (send_request, connection) = h2::client::handshake(tls)
                .await
                .map_err(|error| Error::Transport(format!("h2 handshake: {error}")))?;
            tokio::spawn(async move {
                // The connection future must be driven or no frame moves in either direction.
                if let Err(error) = connection.await {
                    tracing::debug!(%error, "cursor h2 connection ended");
                }
            });
            let mut send_request = send_request;

            let mut http_request = http::Request::builder()
                .method(http::Method::POST)
                .uri(request_uri(&request.origin, request.path)?)
                .version(http::Version::HTTP_2)
                .body(())
                .map_err(|error| Error::Transport(error.to_string()))?;
            *http_request.headers_mut() = request.header_map()?;

            let empty = request.body.is_empty();
            let (response, mut send) = send_request
                .send_request(http_request, empty)
                .map_err(|error| Error::Transport(error.to_string()))?;
            if !empty {
                // One message, then end of stream: every call this transport makes is unary in the
                // request direction. `send_data` buffers, so a small body is queued even when the
                // server has not granted capacity yet — the run's `BidiRequestId` is tens of bytes.
                let bytes = request.body.len();
                send.send_data(request.body, true).map_err(|error| {
                    Error::Transport(format!("request body ({bytes} bytes): {error}"))
                })?;
            }
            let response = response
                .await
                .map_err(|error| Error::Transport(format!("response head: {error}")))?;
            let (head, recv) = response.into_parts();
            Ok(ExchangeResponse {
                status: head.status,
                headers: head.headers,
                stream: Box::new(H2Stream { _send: send, recv }),
            })
        })
    }
}

/// The absolute URI an HTTP/2 request needs.
///
/// HTTP/2 sends no request line: the destination travels in the `:scheme`, `:authority` and `:path`
/// pseudo-headers, which `h2` derives from the URI. A path-only URI therefore has no scheme or
/// authority to derive them from, and `h2` refuses it with "request URI missing scheme and
/// authority" *after* the connection is already open — a failure that reads like a provider fault
/// and, worse, cools the account down for a request this router never managed to send. Every Cursor
/// turn died there until the origin was joined to the path.
fn request_uri(origin: &str, path: &str) -> Result<http::Uri, Error> {
    let uri: http::Uri = format!("{}{}", origin.trim_end_matches('/'), path)
        .parse()
        .map_err(|error| {
            Error::Config(format!("a Cursor request URI could not be built: {error}"))
        })?;
    if uri.scheme_str() != Some("https") || uri.authority().is_none() {
        return Err(Error::Config(
            "a Cursor request URI needs an https scheme and an authority".to_owned(),
        ));
    }
    Ok(uri)
}

fn split_origin(origin: &str) -> Result<(String, u16), Error> {
    let rest = origin
        .strip_prefix("https://")
        .ok_or_else(|| Error::Config("a Cursor origin must be https".to_owned()))?;
    let rest = rest.trim_end_matches('/');
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => (
            host.to_owned(),
            port.parse::<u16>().map_err(|_| {
                Error::Config("the Cursor origin's port is not a number".to_owned())
            })?,
        ),
        _ => (rest.to_owned(), 443),
    };
    if host.is_empty() {
        return Err(Error::Config("the Cursor origin has no host".to_owned()));
    }
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use std::collections::VecDeque;
    use std::pin::Pin;

    use http_body::Body as _;
    use mr_dialects::cursor::pb;

    /// The inverse of the append encoder: `data` carries hex, and the test asserts on the bytes.
    fn unhex(text: &str) -> Option<Vec<u8>> {
        if !text.len().is_multiple_of(2) {
            return None;
        }
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
            .collect()
    }

    /// What the fake server does next.
    enum Step {
        Bytes(Bytes),
        /// Nothing, forever: the turn is stuck.
        Hang,
        Error(String),
    }

    /// What the fake server and the driver both touch.
    ///
    /// Behind an `Arc` because an opened stream outlives the borrow of the exchange: the driver owns
    /// it as a `'static` boxed trait object, exactly as it owns a real one.
    #[derive(Default)]
    struct MockState {
        script: Mutex<VecDeque<Step>>,
        /// Every client message the driver delivered, in order: the run request, then the answers.
        sent: Mutex<Vec<Vec<u8>>>,
        requests: Mutex<Vec<ExchangeRequest>>,
    }

    /// A server that answers from a script and records everything the client wrote.
    ///
    /// Behind an `Arc` from the start because the driver owns the exchange for the life of the turn:
    /// `run` hands it to the pump, and the pump outlives the call. Handing it back afterwards is what
    /// lets a test assert on what the server was told.
    struct MockExchange {
        state: Arc<MockState>,
        status: StatusCode,
    }

    impl MockExchange {
        fn new(script: Vec<Step>, status: StatusCode) -> Arc<Self> {
            Arc::new(Self {
                state: Arc::new(MockState {
                    script: Mutex::new(script.into()),
                    ..MockState::default()
                }),
                status,
            })
        }

        fn sent(&self) -> Vec<Vec<u8>> {
            self.state
                .sent
                .lock()
                .map(|sent| sent.clone())
                .unwrap_or_default()
        }

        fn requests(&self) -> Vec<ExchangeRequest> {
            self.state
                .requests
                .lock()
                .map(|requests| requests.clone())
                .unwrap_or_default()
        }

        fn run_request(&self) -> ExchangeRequest {
            let requests = self.requests();
            requests
                .into_iter()
                .find(|request| request.path == cursor::RUN_SSE_PATH)
                .expect("a run request")
        }

        fn appends(&self) -> Vec<ExchangeRequest> {
            let requests = self.requests();
            requests
                .into_iter()
                .filter(|request| request.path == cursor::BIDI_APPEND_PATH)
                .collect()
        }
    }

    impl Exchange for MockExchange {
        fn open<'a>(
            &'a self,
            request: ExchangeRequest,
        ) -> BoxFuture<'a, Result<ExchangeResponse, Error>> {
            Box::pin(async move {
                if let Ok(mut requests) = self.state.requests.lock() {
                    requests.push(request.clone());
                }
                if request.path == cursor::BIDI_APPEND_PATH {
                    // A unary append: the whole body is a `BidiAppendRequest`. The message it carries
                    // is field 1, hex-encoded, so the recording is of what the server was actually
                    // told rather than of the request around it. The reply is an empty message.
                    let payload = pb::message(&request.body, 1)
                        .ok()
                        .flatten()
                        .and_then(|hex| unhex(std::str::from_utf8(hex).ok()?))
                        .unwrap_or_default();
                    if let Ok(mut sent) = self.state.sent.lock() {
                        sent.push(payload);
                    }
                    return Ok(ExchangeResponse {
                        status: StatusCode::OK,
                        headers: HeaderMap::new(),
                        stream: Box::new(scripted(Vec::new())),
                    });
                }
                Ok(ExchangeResponse {
                    status: self.status,
                    headers: HeaderMap::new(),
                    stream: Box::new(MockStream {
                        state: Arc::clone(&self.state),
                    }),
                })
            })
        }
    }

    fn scripted(steps: Vec<Step>) -> ScriptedStream {
        ScriptedStream {
            steps: steps.into(),
        }
    }

    struct ScriptedStream {
        steps: VecDeque<Step>,
    }

    impl ExchangeStream for ScriptedStream {
        fn next_chunk<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Bytes>, Error>> {
            Box::pin(async move {
                match self.steps.pop_front() {
                    None => Ok(None),
                    Some(Step::Bytes(bytes)) => Ok(Some(bytes)),
                    Some(Step::Hang) => std::future::pending().await,
                    Some(Step::Error(message)) => Err(Error::Transport(message)),
                }
            })
        }
    }

    struct MockStream {
        state: Arc<MockState>,
    }

    impl ExchangeStream for MockStream {
        fn next_chunk<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Bytes>, Error>> {
            Box::pin(async move {
                let next = self
                    .state
                    .script
                    .lock()
                    .ok()
                    .and_then(|mut script| script.pop_front());
                match next {
                    None => Ok(None),
                    Some(Step::Bytes(bytes)) => Ok(Some(bytes)),
                    Some(Step::Hang) => std::future::pending().await,
                    Some(Step::Error(message)) => Err(Error::Transport(message)),
                }
            })
        }
    }

    fn ctx(model: &'static str) -> TranslateCtx<'static> {
        TranslateCtx {
            route: mr_core::RouteKind::ChatCompletions,
            client_dialect: mr_core::Dialect::OpenAi,
            upstream_dialect: mr_core::Dialect::Cursor,
            upstream_model: model,
            client_streaming: true,
            session_id: None,
        }
    }

    const BODY: &[u8] = br#"{"model":"auto","messages":[{"role":"user","content":"hi"}]}"#;

    fn text_frame(text: &str) -> Step {
        let mut interaction = Vec::new();
        pb::put_str(&mut interaction, 1, text);
        let mut update = Vec::new();
        pb::put_bytes(&mut update, 1, &interaction);
        let mut message = Vec::new();
        pb::put_bytes(&mut message, 1, &update);
        Step::Bytes(Bytes::from(cursor::encode_frame(&message)))
    }

    fn turn_end_frame() -> Step {
        let mut usage = Vec::new();
        pb::put_uint(&mut usage, 1, 7);
        pb::put_uint(&mut usage, 2, 3);
        let mut update = Vec::new();
        pb::put_bytes(&mut update, 14, &usage);
        let mut message = Vec::new();
        pb::put_bytes(&mut message, 1, &update);
        Step::Bytes(Bytes::from(cursor::encode_frame(&message)))
    }

    /// A `request_context` question: the server asking what tools this client has.
    fn request_context_frame() -> Step {
        let mut exec = Vec::new();
        pb::put_uint(&mut exec, 1, 1);
        pb::put_bytes(&mut exec, 10, &[]);
        let mut message = Vec::new();
        pb::put_bytes(&mut message, 2, &exec);
        Step::Bytes(Bytes::from(cursor::encode_frame(&message)))
    }

    fn prepared(ctx: &TranslateCtx<'_>, ghost_mode: bool) -> Prepared {
        prepare(ctx, BODY, "Bearer test-credential", ghost_mode, None).expect("a prepared request")
    }

    async fn drain(mut body: crate::upstream::Body) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let next = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
            match next {
                None => break,
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        out.extend_from_slice(&data);
                    }
                }
                Some(Err(error)) => panic!("the body failed: {error}"),
            }
        }
        out
    }

    /// Open a turn against the fake server, keeping the fake to inspect afterwards.
    async fn run_turn(
        exchange: &Arc<MockExchange>,
        prepared: Prepared,
        total_timeout: Duration,
    ) -> Result<crate::upstream::UpstreamResponse, Error> {
        run(
            Arc::clone(exchange) as Arc<dyn Exchange>,
            prepared,
            total_timeout,
        )
        .await
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[tokio::test]
    async fn the_turn_is_opened_with_a_bidi_request_id_and_delivered_by_append() {
        let exchange = MockExchange::new(vec![text_frame("hi"), turn_end_frame()], StatusCode::OK);
        let prepared = prepared(&ctx("auto"), true);
        // Captured from the very request that is about to be sent: the message id is generated per
        // turn, so a second `prepare` would produce a different message and prove nothing.
        let expected_payload = prepared.run.payload.clone();
        let expected_id = prepared.request_id.clone();
        let response = run_turn(&exchange, prepared, Duration::from_secs(5))
            .await
            .expect("a response");
        let _ = drain(response.body).await;

        let sent = exchange.sent();
        assert_eq!(
            sent.first().map(Vec::as_slice),
            Some(&expected_payload[..]),
            "the first appended payload is the client message, unframed"
        );

        let request = exchange.run_request();
        assert_eq!(request.path, cursor::RUN_SSE_PATH);
        assert_eq!(
            request.body,
            Bytes::from(cursor::bidi_request_body(&expected_id)),
            "the stream body is the id the appends name, and nothing else"
        );
        // The backend the provider is configured with, not a discovered agent host: this is the
        // reference client's bidi-disabled shape, and the appends go to the same place.
        assert_eq!(request.origin, cursor::BACKEND_ORIGIN);
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == "x-ghost-mode" && value == "true"));
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == "content-type" && value == "application/connect+proto"));

        let appends = exchange.appends();
        assert_eq!(appends.len(), 1, "one append for the run request");
        let append = appends.first().expect("the append");
        assert_eq!(
            append.body,
            Bytes::from(cursor::bidi_append_request(
                &expected_id,
                0,
                &expected_payload
            )),
            "the run request travels as the first append, with seqno 0"
        );
        assert!(append
            .headers
            .iter()
            .any(|(name, value)| name == "content-type" && value == "application/proto"));
    }

    #[tokio::test]
    async fn the_client_answers_the_request_context_question_with_an_append() {
        let exchange = MockExchange::new(
            vec![request_context_frame(), text_frame("hi"), turn_end_frame()],
            StatusCode::OK,
        );
        let prepared = prepared(&ctx("auto"), true);
        let response = run_turn(&exchange, prepared, Duration::from_secs(5))
            .await
            .expect("a response");
        let _ = drain(response.body).await;

        let sent = exchange.sent();
        assert_eq!(sent.len(), 2, "the run request plus one reply");
        let payload = sent.get(1).expect("the reply");
        // `AgentClientMessage.exec_client_message` is field 2; the result is nested one level
        // deeper, which is the part a wrong field number would break.
        let exec = pb::message(payload, 2)
            .expect("a well-formed reply")
            .expect("an exec client message");
        let result = pb::message(exec, 10)
            .expect("a well-formed result")
            .expect("a request-context result");
        assert!(pb::message(result, 1)
            .expect("well-formed success")
            .is_some());
    }

    #[tokio::test]
    async fn the_answer_is_rendered_as_openai_chunks_ending_with_done() {
        let exchange = MockExchange::new(
            vec![text_frame("hello"), text_frame(" world"), turn_end_frame()],
            StatusCode::OK,
        );
        let prepared = prepared(&ctx("auto"), true);
        let response = run_turn(&exchange, prepared, Duration::from_secs(5))
            .await
            .expect("a response");
        let body = text(&drain(response.body).await);
        assert!(
            body.contains(r#""object":"chat.completion.chunk""#),
            "{body}"
        );
        assert!(body.contains("hello"), "{body}");
        assert!(body.contains(" world"), "{body}");
        assert!(body.contains(r#""finish_reason":"stop""#), "{body}");
        assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
    }

    #[tokio::test]
    async fn a_stuck_server_is_closed_with_length_rather_than_left_open() {
        let exchange = MockExchange::new(vec![text_frame("partial"), Step::Hang], StatusCode::OK);
        let prepared = prepared(&ctx("auto"), true);
        let started = Instant::now();
        let response = run_turn(&exchange, prepared, Duration::from_millis(80))
            .await
            .expect("a response");
        let body = text(&drain(response.body).await);
        assert!(body.contains(r#""finish_reason":"length""#), "{body}");
        assert!(body.contains("partial"), "{body}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout must fire"
        );
    }

    #[tokio::test]
    async fn a_composer_blob_write_starts_a_short_grace_not_a_five_minute_wait() {
        // `KvServerMessage.set_blob_args` (field 3 on the client message, field 4 here because the
        // server's message carries its own numbering) then silence: the composer shape.
        let mut request_metadata = Vec::new();
        pb::put_str(&mut request_metadata, 1, "turn");
        let mut kv = Vec::new();
        pb::put_uint(&mut kv, 1, 9);
        pb::put_bytes(&mut kv, 3, &[]);
        pb::put_bytes(&mut kv, 4, &request_metadata);
        let mut message = Vec::new();
        pb::put_bytes(&mut message, 4, &kv);
        let step = Step::Bytes(Bytes::from(cursor::encode_frame(&message)));

        let exchange = MockExchange::new(
            vec![text_frame("done thinking"), step, Step::Hang],
            StatusCode::OK,
        );
        let prepared = prepared(&ctx("composer-1"), true);
        let started = Instant::now();
        let response = run_turn(&exchange, prepared, Duration::from_secs(60))
            .await
            .expect("a response");
        let body = text(&drain(response.body).await);
        let elapsed = started.elapsed();
        assert!(body.contains("done thinking"), "{body}");
        assert!(body.contains("[DONE]"), "{body}");
        assert!(
            elapsed < Duration::from_secs(20),
            "the grace window, not the total timeout, must end the turn (took {elapsed:?})"
        );
        // The blob write was answered on the same stream, which is the obligation that matters.
        assert!(
            exchange.sent().len() >= 2,
            "the blob write must be answered"
        );
    }

    #[tokio::test]
    async fn a_refusal_is_returned_with_its_status_and_body() {
        let exchange = MockExchange::new(
            vec![Step::Bytes(Bytes::from_static(b"{\"error\":\"nope\"}"))],
            StatusCode::UNAUTHORIZED,
        );
        let prepared = prepared(&ctx("auto"), true);
        let response = run_turn(&exchange, prepared, Duration::from_secs(5))
            .await
            .expect("a response");
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
        let body = text(&drain(response.body).await);
        assert_eq!(body, "{\"error\":\"nope\"}");
    }

    #[tokio::test]
    async fn every_append_goes_to_the_same_origin_the_stream_was_opened_on() {
        let exchange = MockExchange::new(
            vec![request_context_frame(), text_frame("hi"), turn_end_frame()],
            StatusCode::OK,
        );
        let prepared = prepared(&ctx("auto"), false);
        let response = run_turn(&exchange, prepared, Duration::from_secs(5))
            .await
            .expect("a response");
        let _ = drain(response.body).await;
        let streamed = exchange.run_request();
        let appends = exchange.appends();
        assert_eq!(appends.len(), 2, "the run request and the answer");
        for append in &appends {
            // The append names a stream this one server is holding, so a second host would be
            // naming a stream it has never heard of.
            assert_eq!(append.origin, streamed.origin);
        }
        assert!(streamed
            .headers
            .iter()
            .any(|(name, value)| name == "x-ghost-mode" && value == "false"));
    }

    #[test]
    fn a_base_url_that_is_not_cursor_is_never_dialled() {
        // The credential is about to be attached to this origin, and a typo in a provider's base
        // url must not become a request to somewhere else: the turn goes to the shipped backend.
        for refused in [
            "https://evil.example.com",
            "http://api2.cursor.sh",
            "https://api2.cursor.sh.evil.example",
            "https://user:pw@api2.cursor.sh",
        ] {
            let prepared = prepare(
                &ctx("auto"),
                BODY,
                "Bearer test-credential",
                true,
                Some(refused),
            )
            .expect("a prepared request");
            assert_eq!(prepared.origin, cursor::BACKEND_ORIGIN, "{refused}");
        }
    }

    #[tokio::test]
    async fn an_in_band_refusal_before_any_answer_becomes_the_response_instead_of_a_stream() {
        // The turn is accepted (`200`) and then refused a few milliseconds later, inside the stream:
        // the shape a model the account's plan does not include arrives as. Reporting that as a `200`
        // would leave the relay unable to do the one thing that helps — try another account — so the
        // refusal has to become the response, with the server's sentence intact.
        let payload = br#"{"error":{"code":"resource_exhausted","message":"Model unavailable on Free",
            "details":[{"debug":{"details":{"buttons":[{"switchModel":{"suggestedModel":"default"}}]}}}]}}"#;
        let exchange = MockExchange::new(
            vec![Step::Bytes(Bytes::from(cursor::encode_frame(payload)))],
            StatusCode::OK,
        );
        let prepared = prepared(&ctx("grok-4.7"), true);
        let response = run_turn(&exchange, prepared, Duration::from_secs(5))
            .await
            .expect("a response");
        assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
        let body = text(&drain(response.body).await);
        assert!(
            body.contains(r#""type":"model_unavailable""#),
            "the relay's own classification must reach it: {body}"
        );
        assert!(body.contains(r#""code":"resource_exhausted""#), "{body}");
        assert!(
            body.contains("Model unavailable on Free"),
            "the server's sentence is kept: {body}"
        );
        assert!(
            !body.contains("chat.completion.chunk"),
            "no SSE frame may reach the client on this path: {body}"
        );
    }

    #[tokio::test]
    async fn a_refusal_with_no_message_still_replaces_the_stream_instead_of_answering_200() {
        // The shape the server actually sent once: an `error` member whose only message is the
        // placeholder, and no code. It used to fall through as an ordinary in-band error, because
        // the refusal parser had no sentence to keep and returned `None` — so the head was committed
        // as `200` and a non-streaming client was handed an error object under a success status. The
        // account that refused kept the traffic instead of being failed over.
        let payload = br#"{"error":{"message":"Error","type":"invalid_request_error"}}"#;
        let exchange = MockExchange::new(
            vec![Step::Bytes(Bytes::from(cursor::encode_frame(payload)))],
            StatusCode::OK,
        );
        let prepared = prepared(&ctx("gemini-4-hydrogen-high"), true);
        let response = run_turn(&exchange, prepared, Duration::from_secs(5))
            .await
            .expect("a response");
        assert_eq!(
            response.status,
            StatusCode::BAD_GATEWAY,
            "a refusal the server did not explain is still a refusal"
        );
        let body = text(&drain(response.body).await);
        assert!(body.contains(r#""type":"upstream_error""#), "{body}");
        assert!(body.contains("refused the turn"), "{body}");
        assert!(
            !body.contains(r#""message":"Error""#),
            "the placeholder must not be repeated as if it were the cause: {body}"
        );
        assert!(!body.contains("chat.completion.chunk"), "{body}");
    }

    #[tokio::test]
    async fn a_turn_that_ends_before_any_answer_is_a_bad_gateway_rather_than_an_empty_stream() {
        // A silent close with no refusal and no text: the account may well answer next time, so the
        // client must see a failure it can retry, not a `200` with nothing in it.
        let exchange = MockExchange::new(vec![], StatusCode::OK);
        let prepared = prepared(&ctx("auto"), true);
        let response = run_turn(&exchange, prepared, Duration::from_secs(5))
            .await
            .expect("a response");
        assert_eq!(response.status, StatusCode::BAD_GATEWAY);
        let body = text(&drain(response.body).await);
        assert!(body.contains(r#""type":"upstream_error""#), "{body}");
        assert!(body.contains("before it produced an answer"), "{body}");
    }

    #[tokio::test]
    async fn a_stream_that_dies_mid_turn_still_closes_the_client_answer() {
        let exchange = MockExchange::new(
            vec![
                text_frame("half an answer"),
                Step::Error("connection reset".to_owned()),
            ],
            StatusCode::OK,
        );
        let prepared = prepared(&ctx("auto"), true);
        let response = run_turn(&exchange, prepared, Duration::from_secs(5))
            .await
            .expect("a response");
        let body = text(&drain(response.body).await);
        // The partial answer is kept and the failure is reported *in band*: a client that already
        // received text must not be left with a body that simply stops.
        assert!(body.contains("half an answer"), "{body}");
        assert!(body.contains(r#""error":{"message""#), "{body}");
        assert!(body.contains("upstream_error"), "{body}");
        assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
    }

    #[test]
    fn an_identity_is_a_uuid_v4_and_a_w3c_traceparent() {
        let identity = new_identity().expect("an identity");
        assert!(
            cursor::is_uuid_v4(&identity.request_id),
            "{}",
            identity.request_id
        );
        assert!(
            cursor::is_traceparent(&identity.traceparent),
            "{}",
            identity.traceparent
        );
        let second = new_identity().expect("an identity");
        assert_ne!(identity.request_id, second.request_id);
    }

    #[test]
    fn the_configured_base_url_becomes_the_one_origin_the_whole_turn_uses() {
        let prepared = prepare(
            &ctx("auto"),
            BODY,
            "Bearer test-credential",
            true,
            Some("https://api2.cursor.sh/v1"),
        )
        .expect("a prepared request");
        // One origin for the stream and for every append: the append names a stream this server is
        // holding, so it cannot live on a second host.
        assert_eq!(prepared.origin, "https://api2.cursor.sh");
        assert_eq!(prepared.client_version, cursor::PINNED_CLIENT_VERSION);

        // A provider configured with something that is not Cursor still gets a turn; it goes to the
        // shipped backend rather than sending the credential to whatever the typo named.
        let unset = prepare(&ctx("auto"), BODY, "Bearer test-credential", true, None)
            .expect("a prepared request");
        assert_eq!(unset.origin, cursor::BACKEND_ORIGIN);
    }
}
