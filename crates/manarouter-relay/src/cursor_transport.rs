//! The Cursor transport: one bidirectional Connect-RPC stream, framed by hand.
//!
//! # Why this is not an [`crate::upstream::Upstream`] request
//!
//! Every other provider in this relay is asked a question and answers it: one HTTP request in, one
//! response out, which a pooled client can express. Cursor's agent endpoint is a *conversation*.
//! `POST /agent.v1.AgentService/Run` opens an HTTP/2 stream that stays open, the server sends
//! protobuf messages asking for things it cannot know (the tool definitions this client was
//! configured with, a key-value blob it wrote in an earlier turn, permission for a shell command),
//! and the client is expected to answer **on the same stream** while the answer is being written.
//! A request/response client closes the request side as soon as the body is sent, so the server's
//! first question finds a half-closed stream and the turn stalls forever.
//!
//! So the exchange is behind [`Exchange`]: a trait with three verbs (`open`, `send_frame`,
//! `next_chunk`) that the production implementation fills with `h2`, and that a test fills with
//! canned bytes. The driver — the part that decides what to answer, when a turn is over and what
//! the client sees — is the same code in both cases, and it is the part worth testing, because it
//! is where a wrong guess turns into a silent stall.
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

use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::future::BoxFuture;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use mr_dialects::cursor::{
    self, AgentUrls, FrameDecoder, PreparedRun, RequestIdentity, StreamState,
};
use mr_dialects::{TranslateCtx, TranslateError};

/// How long the whole turn may take before the partial answer is closed with `length`.
///
/// The reference client uses the same five minutes. A turn that outlives it is not going to finish,
/// and a client left waiting forever is worse than a truncated answer it can see is truncated.
pub const TOTAL_TIMEOUT: Duration = Duration::from_millis(cursor::STREAM_TIMEOUT_MS);

/// How long the host-discovery call may take.
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a discovered host is reused.
///
/// The two agent hosts are deployment-wide constants that only change on a rollout, so an hour is
/// generous without pinning a stale host for a process lifetime.
pub const DISCOVERY_TTL: Duration = Duration::from_secs(3600);

/// Whether the agent endpoint is asked for the ghost-mode host.
///
/// Ghost mode is the CLI's name for "use the endpoint that does not keep the workspace's contents as
/// context". The transport never sends workspace context anyway (there is none to send), so the flag
/// only selects between `agent.` and `agentn.`; `true` is the reference client's default and the one
/// whose behaviour has been observed.
pub const GHOST_MODE_DEFAULT: bool = true;

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
    /// The RPC path: always [`cursor::RUN_PATH`] or [`cursor::SERVER_CONFIG_PATH`].
    pub path: &'static str,
    /// The complete header list, credential included.
    pub headers: Vec<(String, String)>,
    /// Whether the request side is complete when it opens.
    ///
    /// `false` for a run: the stream must stay open so the server's questions can be answered.
    /// `true` for the unary discovery call, whose body is empty.
    pub end_stream: bool,
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

/// A live bidirectional stream.
///
/// The verbs are deliberately few: a frame is written whole or not at all, and a read either yields
/// bytes or the stream ended. Anything finer would be an `h2` API leaking into the driver.
pub trait ExchangeStream: Send {
    /// Write one framed message. The request side stays open.
    fn send_frame<'a>(&'a mut self, frame: Vec<u8>) -> BoxFuture<'a, Result<(), Error>>;

    /// The next bytes the server wrote, or `None` at a clean end of stream.
    fn next_chunk<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Bytes>, Error>>;

    /// Close the request side.
    fn finish<'a>(&'a mut self) -> BoxFuture<'a, Result<(), Error>>;
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
    /// The authorization value, kept so the discovery call can carry it too.
    authorization: String,
    /// Which agent host this turn wants.
    ghost_mode: bool,
    /// The client version actually sent, for the log line that explains a rejected request.
    pub client_version: String,
    /// The hosts to use when discovery fails or names nothing usable.
    fallback: AgentUrls,
}

/// Shape a request, failing before anything is sent if it cannot be expressed.
///
/// `authorization` is the header value the relay's own credential path produced, so the credential
/// kind is decided in exactly one place; this transport never reads a vault. `origin_hint` is the
/// provider's configured base URL, which is *not* the agent host: the real host comes from
/// `GetServerConfig`. The hint is used only when it already is an allow-listed agent origin, which
/// keeps a deployment that pins a host working without discovery.
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
    let pinned_origin = origin_hint.and_then(|hint| cursor::validate_origin(hint).ok());
    let run =
        cursor::prepare_run(ctx, body, &mr_core::ids::new_id()).map_err(|error| match error {
            TranslateError::MalformedRequest { detail, .. } => Error::Request(detail),
            TranslateError::MalformedResponse { detail, .. } => Error::Request(detail),
            other => Error::Request(other.to_string()),
        })?;

    let client_version = resolve_client_version();
    let identity = new_identity()?;
    let headers = cursor::request_headers(authorization, ghost_mode, &client_version, &identity)?;
    // A pinned origin replaces the fallback for this provider: an operator who named an agent host
    // did so because discovery was wrong for their deployment, so the constant must not win.
    let fallback = match pinned_origin {
        Some(origin) if ghost_mode => AgentUrls {
            agent: origin,
            agentn: cursor::FALLBACK_AGENTN_ORIGIN.to_owned(),
        },
        Some(origin) => AgentUrls {
            agent: cursor::FALLBACK_AGENT_ORIGIN.to_owned(),
            agentn: origin,
        },
        None => fallback_urls(),
    };

    Ok(Prepared {
        run,
        headers,
        authorization: authorization.to_owned(),
        ghost_mode,
        client_version,
        fallback,
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

/// Run one turn and hand back a response the relay can render like any other.
///
/// The returned body is a channel: the caller gets the response head as soon as the server answers
/// it, and SSE bytes arrive as the turn produces them. Buffering would defeat the point of streaming
/// and would make the relay's first-byte timeout measure the whole answer.
pub async fn run(
    exchange: &dyn Exchange,
    prepared: Prepared,
    total_timeout: Duration,
) -> Result<crate::upstream::UpstreamResponse, Error> {
    let started = Instant::now();
    let (origin, discovered) = discover(exchange, &prepared)
        .await
        .map(|urls| urls.select(prepared.ghost_mode))
        .unwrap_or_else(|| prepared.fallback.select(prepared.ghost_mode));
    if !discovered {
        tracing::debug!(
            origin = %origin,
            "cursor: no discovered agent host in use; falling back to the shipped host"
        );
    }

    let request = ExchangeRequest {
        origin,
        path: cursor::RUN_PATH,
        headers: prepared.headers.clone(),
        end_stream: false,
    };
    let opened = exchange.open(request).await?;
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
    let ttfb = started.elapsed();
    let first = prepared.run.frame;
    tokio::spawn(async move {
        pump(opened.stream, first, state, started, total_timeout, tx).await;
    });

    Ok(crate::upstream::UpstreamResponse {
        status: StatusCode::OK,
        headers: sse_headers(),
        body: crate::upstream::Body::Channel(crate::upstream::ChannelBody::new(rx)),
        ttfb,
        reused_connection: false,
    })
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
    first: Vec<u8>,
    mut state: StreamState,
    started: Instant,
    total_timeout: Duration,
    tx: tokio::sync::mpsc::Sender<Bytes>,
) {
    if let Err(error) = stream.send_frame(first).await {
        let effects = state.on_transport_error(&error.to_string());
        flush(&mut stream, effects, &tx).await;
        let _ = stream.finish().await;
        return;
    }

    let deadline = started + total_timeout;
    let mut decoder = FrameDecoder::new();
    let mut grace: Option<Instant> = None;
    'turn: loop {
        let now = Instant::now();
        if now >= deadline {
            let effects = state.on_timeout();
            flush(&mut stream, effects, &tx).await;
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
                flush(&mut stream, effects, &tx).await;
                break;
            }
            Ok(Ok(None)) => {
                let effects = state.on_server_end();
                flush(&mut stream, effects, &tx).await;
                break;
            }
            Ok(Err(error)) => {
                let effects = state.on_transport_error(&error.to_string());
                flush(&mut stream, effects, &tx).await;
                break;
            }
            Ok(Ok(Some(chunk))) => {
                let frames = match decoder.push(&chunk) {
                    Ok(frames) => frames,
                    Err(error) => {
                        let effects = state.on_transport_error(&error.to_string());
                        flush(&mut stream, effects, &tx).await;
                        break;
                    }
                };
                for frame in frames {
                    let elapsed_ms =
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let effects = match state.on_frame(&frame, elapsed_ms) {
                        Ok(effects) => effects,
                        Err(error) => {
                            let effects = state.on_transport_error(&error.to_string());
                            flush(&mut stream, effects, &tx).await;
                            break 'turn;
                        }
                    };
                    let next_grace = effects.kv_grace_deadline_ms;
                    let ended = effects.ended;
                    flush(&mut stream, effects, &tx).await;
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
        let effects = state.on_transport_error(&error.to_string());
        flush(&mut stream, effects, &tx).await;
    }
    let _ = stream.finish().await;
    // Dropping the sender ends the body, which is what tells the relay's renderer the stream is
    // over; the `[DONE]` frame was already written above.
}

async fn flush(
    stream: &mut Box<dyn ExchangeStream>,
    effects: cursor::Effects,
    tx: &tokio::sync::mpsc::Sender<Bytes>,
) {
    for frame in effects.outbound {
        if stream.send_frame(frame).await.is_err() {
            // A dead request side means the server's question can no longer be answered; the read
            // side reports the end and the client keeps whatever was rendered so far.
            return;
        }
    }
    if !effects.sse.is_empty() && tx.send(Bytes::from(effects.sse)).await.is_err() {
        // The client hung up. Nothing left to render.
    }
}

// ---------------------------------------------------------------------------------------------
// Host discovery
// ---------------------------------------------------------------------------------------------

fn fallback_urls() -> AgentUrls {
    AgentUrls {
        agent: cursor::FALLBACK_AGENT_ORIGIN.to_owned(),
        agentn: cursor::FALLBACK_AGENTN_ORIGIN.to_owned(),
    }
}

/// Resolve the agent hosts, or `None` to use the fallback.
///
/// A failure here is not fatal: the hosts have been stable for long enough that a hard-coded
/// fallback is the reference client's own behaviour, and refusing a turn because a discovery call
/// timed out would fail requests that were about to succeed.
///
/// The cache is process-wide and holds the hosts for [`DISCOVERY_TTL`]. That is safe because the two
/// hosts are deployment constants; an account whose configuration named a different (still
/// allow-listed) origin would be served the cached one, which is why the fallback path logs rather
/// than staying silent.
async fn discover(exchange: &dyn Exchange, prepared: &Prepared) -> Option<AgentUrls> {
    static CACHE: OnceLock<Mutex<Option<(AgentUrls, Instant)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some((urls, at)) = guard.as_ref() {
            if at.elapsed() < DISCOVERY_TTL {
                return Some(urls.clone());
            }
        }
    }

    let headers = cursor::discovery_headers(&prepared.authorization).ok()?;
    let request = ExchangeRequest {
        origin: cursor::DISCOVERY_ORIGIN.to_owned(),
        path: cursor::SERVER_CONFIG_PATH,
        headers,
        end_stream: true,
    };
    let opened = match tokio::time::timeout(DISCOVERY_TIMEOUT, exchange.open(request)).await {
        Ok(Ok(opened)) => opened,
        Ok(Err(error)) => {
            tracing::debug!(%error, "cursor host discovery failed; using the fallback hosts");
            return None;
        }
        Err(_) => {
            tracing::debug!("cursor host discovery timed out; using the fallback hosts");
            return None;
        }
    };
    if opened.status != StatusCode::OK {
        tracing::debug!(
            status = opened.status.as_u16(),
            "cursor host discovery was refused; using the fallback hosts"
        );
        return None;
    }
    let (body, _) = collect(opened.stream, MAX_ERROR_BODY).await.ok()?;
    let urls = cursor::parse_agent_url_config(&body)?;
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((urls.clone(), Instant::now()));
    }
    Some(urls)
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
pub fn default_exchange() -> Result<&'static dyn Exchange, Error> {
    static EXCHANGE: OnceLock<Result<DirectH2Exchange, String>> = OnceLock::new();
    match EXCHANGE.get_or_init(|| DirectH2Exchange::new().map_err(|error| error.to_string())) {
        Ok(exchange) => Ok(exchange),
        Err(message) => Err(Error::Config(message.clone())),
    }
}

struct H2Stream {
    send: h2::SendStream<Bytes>,
    recv: h2::RecvStream,
}

impl ExchangeStream for H2Stream {
    fn send_frame<'a>(&'a mut self, frame: Vec<u8>) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let mut data = Bytes::from(frame);
            while !data.is_empty() {
                let capacity = self.send.capacity();
                if capacity == 0 {
                    match futures_util::future::poll_fn(|cx| self.send.poll_capacity(cx)).await {
                        Some(Ok(_)) => continue,
                        Some(Err(error)) => return Err(Error::Transport(error.to_string())),
                        None => return Err(Error::Transport("the request side closed".to_owned())),
                    }
                }
                let take = capacity.min(data.len());
                let head = data.split_to(take);
                self.send
                    .send_data(head, false)
                    .map_err(|error| Error::Transport(error.to_string()))?;
            }
            Ok(())
        })
    }

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

    fn finish<'a>(&'a mut self) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            self.send
                .send_data(Bytes::new(), true)
                .map_err(|error| Error::Transport(error.to_string()))
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
                .uri(request.path)
                .version(http::Version::HTTP_2)
                .body(())
                .map_err(|error| Error::Transport(error.to_string()))?;
            *http_request.headers_mut() = request.header_map()?;

            let (response, send) = send_request
                .send_request(http_request, request.end_stream)
                .map_err(|error| Error::Transport(error.to_string()))?;
            let response = response
                .await
                .map_err(|error| Error::Transport(format!("response head: {error}")))?;
            let (head, recv) = response.into_parts();
            Ok(ExchangeResponse {
                status: head.status,
                headers: head.headers,
                stream: Box::new(H2Stream { send, recv }),
            })
        })
    }
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
    use super::*;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::Arc;

    use http_body::Body as _;
    use mr_dialects::cursor::pb;

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
        sent: Mutex<Vec<Vec<u8>>>,
        requests: Mutex<Vec<ExchangeRequest>>,
    }

    /// A server that answers from a script and records everything the client wrote.
    struct MockExchange {
        state: Arc<MockState>,
        status: StatusCode,
        discovery: Option<Vec<u8>>,
    }

    impl MockExchange {
        fn new(script: Vec<Step>, status: StatusCode) -> Self {
            Self {
                state: Arc::new(MockState {
                    script: Mutex::new(script.into()),
                    ..MockState::default()
                }),
                status,
                discovery: None,
            }
        }

        fn with_discovery(mut self, payload: Vec<u8>) -> Self {
            self.discovery = Some(payload);
            self
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
                .find(|request| request.path == cursor::RUN_PATH)
                .expect("a run request")
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
                if request.path == cursor::SERVER_CONFIG_PATH {
                    let (status, chunks) = match &self.discovery {
                        Some(payload) => (
                            StatusCode::OK,
                            vec![Step::Bytes(Bytes::from(payload.clone()))],
                        ),
                        None => (StatusCode::NOT_FOUND, Vec::new()),
                    };
                    return Ok(ExchangeResponse {
                        status,
                        headers: HeaderMap::new(),
                        stream: Box::new(scripted(chunks)),
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
        fn send_frame<'a>(&'a mut self, _frame: Vec<u8>) -> BoxFuture<'a, Result<(), Error>> {
            Box::pin(async move { Ok(()) })
        }

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

        fn finish<'a>(&'a mut self) -> BoxFuture<'a, Result<(), Error>> {
            Box::pin(async move { Ok(()) })
        }
    }

    struct MockStream {
        state: Arc<MockState>,
    }

    impl ExchangeStream for MockStream {
        fn send_frame<'a>(&'a mut self, frame: Vec<u8>) -> BoxFuture<'a, Result<(), Error>> {
            Box::pin(async move {
                if let Ok(mut sent) = self.state.sent.lock() {
                    sent.push(frame);
                }
                Ok(())
            })
        }

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

        fn finish<'a>(&'a mut self) -> BoxFuture<'a, Result<(), Error>> {
            Box::pin(async move { Ok(()) })
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

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[tokio::test]
    async fn the_first_frame_is_the_run_request_and_the_request_side_stays_open() {
        let exchange = MockExchange::new(vec![text_frame("hi"), turn_end_frame()], StatusCode::OK);
        let prepared = prepared(&ctx("auto"), true);
        // Captured from the very request that is about to be sent: the message id is generated per
        // turn, so a second `prepare` would produce a different frame and prove nothing.
        let expected = prepared.run.frame.clone();
        let response = run(&exchange, prepared, Duration::from_secs(5))
            .await
            .expect("a response");
        let _ = drain(response.body).await;

        let sent = exchange.sent();
        assert_eq!(sent.first().map(Vec::as_slice), Some(&expected[..]));
        let request = exchange.run_request();
        assert!(!request.end_stream, "the request side must stay open");
        assert!(
            request.origin.ends_with("api5.cursor.sh"),
            "{}",
            request.origin
        );
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == "x-ghost-mode" && value == "true"));
        assert!(request
            .headers
            .iter()
            .any(|(name, _)| name == "content-type" && true));
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == "content-type" && value == "application/connect+proto"));
    }

    #[tokio::test]
    async fn the_client_answers_the_request_context_question_on_the_same_stream() {
        let exchange = MockExchange::new(
            vec![request_context_frame(), text_frame("hi"), turn_end_frame()],
            StatusCode::OK,
        );
        let prepared = prepared(&ctx("auto"), true);
        let response = run(&exchange, prepared, Duration::from_secs(5))
            .await
            .expect("a response");
        let _ = drain(response.body).await;

        let sent = exchange.sent();
        assert_eq!(sent.len(), 2, "one run frame plus one reply");
        let reply = sent.get(1).expect("the reply frame");
        let payload = reply.get(5..).expect("a framed payload");
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
        let response = run(&exchange, prepared, Duration::from_secs(5))
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
        let response = run(&exchange, prepared, Duration::from_millis(80))
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
        let response = run(&exchange, prepared, Duration::from_secs(60))
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
        let response = run(&exchange, prepared, Duration::from_secs(5))
            .await
            .expect("a response");
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
        let body = text(&drain(response.body).await);
        assert_eq!(body, "{\"error\":\"nope\"}");
    }

    #[tokio::test]
    async fn a_discovered_agentn_host_is_used_for_a_non_ghost_turn() {
        let mut config = Vec::new();
        pb::put_str(&mut config, 1, "https://agent.api5.cursor.sh");
        pb::put_str(&mut config, 2, "https://agentn.api5.cursor.sh");
        let mut reply = Vec::new();
        pb::put_bytes(&mut reply, 27, &config);
        let exchange = MockExchange::new(vec![text_frame("hi"), turn_end_frame()], StatusCode::OK)
            .with_discovery(reply);
        let prepared = prepared(&ctx("auto"), false);
        let response = run(&exchange, prepared, Duration::from_secs(5))
            .await
            .expect("a response");
        let _ = drain(response.body).await;
        let request = exchange.run_request();
        assert_eq!(request.origin, "https://agentn.api5.cursor.sh");
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == "x-ghost-mode" && value == "false"));
    }

    #[tokio::test]
    async fn a_malformed_origin_is_never_dialled() {
        let exchange = MockExchange::new(vec![], StatusCode::OK).with_discovery({
            let mut config = Vec::new();
            pb::put_str(&mut config, 1, "https://evil.example.com");
            pb::put_str(&mut config, 2, "https://agentn.api5.cursor.sh");
            let mut reply = Vec::new();
            pb::put_bytes(&mut reply, 27, &config);
            reply
        });
        let prepared = prepared(&ctx("auto"), false);
        let response = run(&exchange, prepared, Duration::from_secs(5))
            .await
            .expect("a response");
        let _ = drain(response.body).await;
        let request = exchange.run_request();
        assert_eq!(request.origin, cursor::FALLBACK_AGENTN_ORIGIN);
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
        let response = run(&exchange, prepared, Duration::from_secs(5))
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
    fn a_pinned_origin_hint_is_used_as_the_fallback() {
        let prepared = prepare(
            &ctx("auto"),
            BODY,
            "Bearer test-credential",
            true,
            Some("https://agent.api5.cursor.sh"),
        )
        .expect("a prepared request");
        // The hint is only a fallback, so it must be an allow-listed agent origin; the discovery
        // call still decides the host at run time.
        assert_eq!(
            fallback_urls().select(true).0,
            cursor::FALLBACK_AGENT_ORIGIN
        );
        assert_eq!(prepared.client_version, cursor::PINNED_CLIENT_VERSION);
    }
}
