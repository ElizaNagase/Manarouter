//! Winner recovery for the hedged race.
//!
//! # What this is for
//!
//! A race commits its winner when the upstream *response* arrives, which is before the body
//! streams. So there is a window in which the committed winner can die: the headers are on the
//! wire, and then the body stops halfway through, or stops before it starts. When that happens the
//! client is left with a truncated answer and no explanation.
//!
//! This module owns the answer to that. It wraps the winner's body so that the relay owns the
//! stream, notices the failure, and — when it is allowed to — continues with a participant that
//! already passed the race.
//!
//! # The rule
//!
//! The replacement is the **highest-priority participant that passed**: it produced a valid reply
//! inside the eligibility window, and its answer is already in the race's result set. Nothing is
//! re-dispatched, so a recovery costs no extra upstream request and no extra latency beyond the
//! time the client spends reading the replacement's body.
//!
//! # The three shapes
//!
//! * **Nothing on the wire yet** — the switch is invisible. The client sees one coherent answer,
//!   the replacement's. Nothing is announced, because there is nothing to reconcile.
//! * **Bytes on the wire, `restart`** — the relay emits the `manarouter.retry` boundary and then
//!   continues with the replacement's body. A client that ignores unknown events still receives a
//!   valid, terminated response.
//! * **Bytes on the wire, `terminate`** — the stream ends with an error. The client decides.
//!
//! # The hard guard
//!
//! Once any tool-call delta has been emitted the relay **refuses to splice**, under both
//! policies. Half an argument from one model plus half an argument from another is a function call
//! that may be perfectly well-formed and completely wrong, and a client that has already seen a
//! tool call opened cannot unsee it. A refused splice ends the stream with an error, which is
//! worse for the answer and much better for the caller's state machine.
//!
//! # What this costs the ordinary path
//!
//! Nothing. The wrapper is only ever installed on the response of a request that was actually in a
//! race, so a request that is not hedged is byte-for-byte what it was before this module existed.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body as AxumBody;
use axum::response::Response;
use bytes::Bytes;
use futures_util::Future;
use http_body::Frame;
use mr_core::dialect::{Dialect, MidstreamPolicy, WinnerFailMode};

/// A participant that passed the race and can answer in the winner's place.
///
/// The body is the participant's **client-facing** body, already translated into the client's
/// dialect and already on its way from the upstream. It is unread: the race committed another
/// participant, so this one is only ever pulled if the winner dies.
pub struct Replacement {
    /// How the participant is named in the retry boundary — `provider/model`.
    pub id: String,
    /// The participant's client-facing body, not yet read.
    pub body: AxumBody,
}

/// What the relay does when the winner's body fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Nothing has reached the client: switch without saying anything.
    Silent,
    /// Bytes have reached the client and the policy allows a switch: announce it, then continue.
    Restart,
    /// The switch is not allowed: end the stream with an error.
    Refuse,
}

/// Decide what to do when the winner's body fails.
///
/// The inputs are the three facts that matter: whether anything reached the client, whether a
/// tool-call delta was emitted, and whether there is a participant to fall back to.
///
/// This is a pure function on purpose. It is the whole policy, and a policy that can only be
/// tested through a live HTTP path is a policy that is not tested.
pub fn decide(
    written: u64,
    tool_call: bool,
    has_replacement: bool,
    policy: MidstreamPolicy,
    announceable: bool,
) -> RecoveryAction {
    // The hard guard, under both policies. A false positive here costs a retry; a false negative
    // costs the caller a function call that may be valid and wrong.
    if tool_call {
        return RecoveryAction::Refuse;
    }
    // Nobody to fall back to: the failure is the answer.
    if !has_replacement {
        return RecoveryAction::Refuse;
    }
    // Nothing has reached the client, so the switch is invisible and both policies allow it.
    if written == 0 {
        return RecoveryAction::Silent;
    }
    match policy {
        MidstreamPolicy::Terminate => RecoveryAction::Refuse,
        MidstreamPolicy::Restart => {
            if announceable {
                RecoveryAction::Restart
            } else {
                // The client's body is not an event stream, so there is no frame in which a
                // boundary could be announced — and two documents spliced together are not a
                // document. This is the `terminate` shape with a worse reason.
                RecoveryAction::Refuse
            }
        }
    }
}

/// A factory that spawns the fallback work for a winner-fail mode that needs new upstream
/// dispatches (`rerun-excluding`, `stable-fallback`). It returns a fresh future each call so the
/// recovery body can poll it to completion in [`RecoveryBody::poll_frame`]. `recovery` does not
/// depend on the relay's request-context types: the factory is built in `server.rs`, which owns
/// `attempt_step`, and invoked here only when the committed winner actually fails.
pub type RerunFactory = Box<
    dyn FnMut() -> Pin<Box<dyn Future<Output = Result<Vec<Replacement>, axum::Error>> + Send>>
        + Send,
>;

/// The in-flight fallback future type, factored out so clippy doesn't complain about the
/// nested `Pin<Box<dyn Future<...>>>` type in field declarations.
type RerunFuture = Pin<Box<dyn Future<Output = Result<Vec<Replacement>, axum::Error>> + Send>>;

/// What `fail` should do next.
enum FailOutcome {
    /// Switched to a replacement body; keep polling.
    Switched,
    /// No replacement is ready and the mode may re-dispatch: the caller should spawn/poll the
    /// rerun factory.
    NeedRerun,
    /// End the request with an error.
    Refused(axum::Error),
}

/// The frames that mark a mid-stream switch, in the client's dialect.
///
/// The retry event is informational: a client that ignores unknown SSE events — as the
/// specification requires — still receives a valid, terminated response. The frame after it closes
/// the dead model's turn, so a client that is waiting for more content from it stops waiting.
pub fn retry_boundary(client_dialect: Dialect, from: &str, to: &str) -> Vec<Bytes> {
    let data = serde_json::json!({
        "reason": "upstream_failed",
        "from": from,
        "to": to,
    })
    .to_string();
    vec![
        Bytes::from(mr_dialects::sse::named_frame(
            mr_dialects::RETRY_EVENT,
            &data,
        )),
        Bytes::from(terminating_frame(client_dialect)),
    ]
}

/// The frame that ends the dead model's turn, in the client's dialect.
fn terminating_frame(client_dialect: Dialect) -> String {
    match client_dialect {
        // The Anthropic Messages stream ends its turn with `message_stop`.
        Dialect::Anthropic => {
            mr_dialects::sse::named_frame("message_stop", r#"{"type":"message_stop"}"#)
        }
        // The OpenAI Chat Completions stream ends its turn with a final choice carrying
        // `finish_reason` and an empty delta.
        _ => mr_dialects::sse::data_frame(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
    }
}

/// The index just past the last complete event in `bytes`, if there is one.
///
/// An event ends at a blank line, and the blank line can be spelled either way — Google's own
/// streams use CRLF, OpenAI's use LF — so both are looked for and the later of the two wins. A
/// buffer with no blank line holds no complete event.
fn last_event_end(bytes: &[u8]) -> Option<usize> {
    let crlf = bytes
        .windows(4)
        .rposition(|window| window == b"\r\n\r\n")
        .map(|at| at + 4);
    let lf = bytes
        .windows(2)
        .rposition(|window| window == b"\n\n")
        .map(|at| at + 2);
    match (crlf, lf) {
        (Some(crlf), Some(lf)) => Some(crlf.max(lf)),
        (crlf, lf) => crlf.or(lf),
    }
}

/// Watches the bytes that reach the client for a tool-call delta.
///
/// The search is a byte-level one on purpose. Parsing every frame as JSON would cost more on the
/// hot path than the guard is worth, and the guard only runs for a request that was in a race. A
/// false positive is safe — the relay refuses to splice — and a false negative is not, so the
/// search is deliberately generous: it looks for the OpenAI spelling and the Anthropic spelling
/// anywhere in the bytes.
///
/// The tail is kept between chunks because a marker can straddle two of them, and a marker that
/// straddles two chunks is exactly the one that would be missed.
pub struct ToolCallGuard {
    /// The last bytes of the previous chunk.
    tail: [u8; ToolCallGuard::TAIL],
    /// How many of them are valid.
    len: usize,
}

impl ToolCallGuard {
    /// How many bytes of the previous chunk are kept. The longest marker is `tool_calls` (ten
    /// bytes), so nine would do; sixteen is comfortably more and costs nothing.
    const TAIL: usize = 16;

    /// The spellings a tool-call delta can have.
    const NEEDLES: [&'static [u8]; 2] = [b"tool_calls", b"tool_use"];

    /// A guard that has seen nothing.
    pub fn new() -> Self {
        Self {
            tail: [0u8; Self::TAIL],
            len: 0,
        }
    }

    /// Feed a chunk of client-facing bytes. Returns `true` if a tool-call marker was seen.
    pub fn observe(&mut self, chunk: &[u8]) -> bool {
        let mut found = false;
        for needle in Self::NEEDLES {
            if contains(chunk, needle) || straddles(&self.tail[..self.len], chunk, needle) {
                found = true;
            }
        }

        // Keep the tail of this chunk for the next one. When the chunk is shorter than the tail,
        // the bytes before it are carried forward too: a marker split over three chunks (`tool`,
        // `_cal`, `ls`) has its first half in an earlier chunk, and dropping it here is exactly the
        // false negative the guard exists to prevent.
        if chunk.len() >= Self::TAIL {
            self.tail
                .copy_from_slice(&chunk[chunk.len() - Self::TAIL..]);
            self.len = Self::TAIL;
        } else {
            let carry = (Self::TAIL - chunk.len()).min(self.len);
            self.tail.copy_within(self.len - carry..self.len, 0);
            self.tail[carry..carry + chunk.len()].copy_from_slice(chunk);
            self.len = carry + chunk.len();
        }
        found
    }
}

impl Default for ToolCallGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `needle` occurs in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Whether `needle` straddles the boundary between `tail` and `chunk`.
///
/// The marker may be split anywhere: the first *k* bytes of the marker are the last *k* bytes of
/// the tail, and the rest are the first bytes of the chunk.
fn straddles(tail: &[u8], chunk: &[u8], needle: &[u8]) -> bool {
    let max = needle.len().saturating_sub(1).min(tail.len());
    for k in 1..=max {
        if tail[tail.len() - k..] != needle[..k] {
            continue;
        }
        let rest = &needle[k..];
        if chunk.len() >= rest.len() && &chunk[..rest.len()] == rest {
            return true;
        }
    }
    false
}

/// Which body is being streamed right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The committed winner.
    Winner,
    /// A replacement, by index into the replacement list.
    Replacement(usize),
}

/// Configuration for a `RecoveryBody`, grouped so neither `new` nor `wrap` exceeds clippy's
/// argument limit.
pub struct RecoveryConfig {
    /// The combo's mid-stream policy.
    pub policy: MidstreamPolicy,
    /// The winner-fail mode: what to do when replacements run out.
    pub mode: WinnerFailMode,
    /// The dialect the client speaks.
    pub client_dialect: Dialect,
    /// The id of the committed winner.
    pub winner_id: String,
    /// Whether the client-facing body is an event stream, and can therefore carry a boundary.
    pub announceable: bool,
    /// A factory that re-dispatches fallback work when replacements run out.
    pub rerun_factory: Option<RerunFactory>,
    /// The maximum number of re-dispatch rounds allowed.
    pub rerun_max_rounds: u8,
}

/// The winner's body, watched.
///
/// It forwards every chunk unchanged, notices when the upstream body errors, remembers whether
/// anything has reached the client, and — when the policy allows it — continues with a
/// participant that passed the race.
pub struct RecoveryBody {
    /// The committed winner's client-facing body.
    winner: AxumBody,
    /// The participants that passed the race, in priority order.
    replacements: Vec<Replacement>,
    /// The next replacement to try.
    next: usize,
    /// The combo's mid-stream policy.
    policy: MidstreamPolicy,
    /// The winner-fail mode: what to do when replacements run out.
    mode: WinnerFailMode,
    /// The dialect the client speaks.
    client_dialect: Dialect,
    /// The id of the participant whose body is being streamed.
    current_id: String,
    /// The id of the committed winner, used in error messages.
    winner_id: String,
    /// Whether the client-facing body is an event stream, and can therefore carry a boundary.
    announceable: bool,
    /// The last bytes written, kept to know whether the wire is at an event boundary.
    carry: Vec<u8>,
    /// Whether the bytes written so far end at an event boundary, so a boundary frame can follow.
    boundary_safe: bool,
    /// How many bytes have reached the client.
    written: u64,
    /// Whether a tool-call delta has been seen.
    tool_call: bool,
    /// The guard that spots a tool-call delta.
    guard: ToolCallGuard,
    /// The frames waiting to be written — the retry boundary.
    pending: VecDeque<Bytes>,
    /// Bytes from the current source that do not yet form a complete event.
    ///
    /// Held back so that nothing partial is ever on the wire: a fragment cannot be parsed, and once
    /// the relay splices, a fragment is concatenated with the boundary that follows it.
    tail: Vec<u8>,
    /// The current source ended cleanly and its held tail has been dealt with; the next poll ends
    /// the body.
    finished: bool,
    /// Whether the body has ended.
    ended: bool,
    /// Which body is being streamed.
    source: Source,
    /// A factory that re-dispatches fallback work when replacements run out, for modes that need
    /// new upstream requests. `None` for `pick-highest-passed` and
    /// `PickHighestPassedDifferentProvider` (their replacements are pre-built).
    rerun_factory: Option<RerunFactory>,
    /// The in-flight fallback future, once spawned.
    rerun_future: Option<RerunFuture>,
    /// How many re-dispatch rounds have been used so far (bounds `rerun-excluding`).
    rerun_rounds: u32,
    /// The maximum number of re-dispatch rounds allowed. `1` for `stable-fallback`
    /// (one fallback step), `max_rounds` for `rerun-excluding`.
    rerun_max_rounds: u8,
}

impl RecoveryBody {
    /// Build the wrapper.
    pub fn new(winner: AxumBody, replacements: Vec<Replacement>, config: RecoveryConfig) -> Self {
        let RecoveryConfig {
            policy,
            mode,
            client_dialect,
            winner_id,
            announceable,
            rerun_factory,
            rerun_max_rounds,
        } = config;
        Self {
            current_id: winner_id.clone(),
            winner,
            replacements,
            next: 0,
            policy,
            mode,
            client_dialect,
            written: 0,
            tool_call: false,
            guard: ToolCallGuard::new(),
            pending: VecDeque::new(),
            tail: Vec::new(),
            finished: false,
            announceable,
            carry: Vec::with_capacity(4),
            // Nothing has been written yet, which is the one state a boundary may always follow.
            boundary_safe: true,
            ended: false,
            source: Source::Winner,
            winner_id,
            rerun_factory,
            rerun_future: None,
            rerun_rounds: 0,
            rerun_max_rounds,
        }
    }

    /// How much of an unterminated event is held before it is forwarded anyway.
    ///
    /// Eight mebibytes is far more than any real event and small enough that a misbehaving upstream
    /// costs memory instead of hanging the answer.
    const TAIL_LIMIT: usize = 8 * 1024 * 1024;

    /// Whether there is still a participant to fall back to.
    fn has_replacement(&self) -> bool {
        self.next < self.replacements.len()
    }

    /// Note the bytes that reached the client, and look for a tool-call delta in them.
    fn observe_bytes(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.written += data.len() as u64;
        if self.guard.observe(data) {
            self.tool_call = true;
        }
        // Whether the wire now sits at an event boundary decides whether the boundary frame can be
        // written at all: anything else would be spliced into the middle of a line or an event.
        self.carry.extend_from_slice(data);
        if self.carry.len() > 4 {
            let excess = self.carry.len() - 4;
            self.carry.drain(..excess);
        }
        self.boundary_safe = self.carry.ends_with(b"\n\n") || self.carry.ends_with(b"\r\n\r\n");
    }

    /// Whether an event on this body ends at a blank line.
    ///
    /// The OpenAI and Anthropic event streams are blank-line delimited. Command Code's
    /// newline-delimited JSON surface is not — there one line is one event — so holding bytes until
    /// a blank line would hold the answer until the stream ended.
    ///
    /// `announceable` is used rather than the stricter `stream_framed` because some upstreams serve
    /// SSE without declaring `text/event-stream` (plain text, no content type). Buffering those to
    /// blank lines is safe: `TAIL_LIMIT` caps the hold, and any stream that announces itself as an
    /// event stream is line-oriented enough that a blank line eventually arrives.
    fn events_are_blank_line_delimited(&self) -> bool {
        self.announceable && self.client_dialect != Dialect::CommandCodeGo
    }

    /// Split the bytes that may be forwarded now from an event that is still arriving.
    ///
    /// An event stream is delimited, not framed: a chunk boundary can fall anywhere, and a fragment
    /// forwarded as-is is harmless only until the next byte is written by *somebody else*. The
    /// relay does exactly that when a step fails mid-stream, so the fragment is held instead: the
    /// client never sees an event that is not complete, and the boundary that follows always starts
    /// on a line of its own.
    fn take_complete_events(&mut self, data: &[u8]) -> Bytes {
        if !self.events_are_blank_line_delimited() {
            return Bytes::copy_from_slice(data);
        }
        self.tail.extend_from_slice(data);
        let Some(end) = last_event_end(&self.tail) else {
            // An upstream that never terminates an event must not be able to stall the answer
            // indefinitely: past the limit the held bytes are forwarded and the count restarts.
            if self.tail.len() > Self::TAIL_LIMIT {
                tracing::warn!(
                    held = self.tail.len(),
                    "forwarding an event that has not been terminated"
                );
                return Bytes::from(std::mem::take(&mut self.tail));
            }
            return Bytes::new();
        };
        let ready: Vec<u8> = self.tail.drain(..end).collect();
        Bytes::from(ready)
    }

    /// A frame from the current source, as far as it can be forwarded now.
    ///
    /// `None` means the bytes are only part of an event that has not finished arriving: they are
    /// held, and the caller should poll again rather than write anything.
    fn forwardable(&mut self, frame: Frame<Bytes>) -> Option<Frame<Bytes>> {
        let Some(data) = frame.data_ref() else {
            // Trailers carry no event payload and cannot be half of one.
            return Some(frame);
        };
        if !self.events_are_blank_line_delimited() {
            // Nothing is being re-framed, so the bytes go straight through as they arrived.
            self.observe_bytes(data);
            return Some(frame);
        }
        let ready = self.take_complete_events(data);
        if ready.is_empty() {
            return None;
        }
        self.observe_bytes(&ready);
        Some(Frame::data(ready))
    }

    /// Drop whatever the source that just failed had half-written.
    ///
    /// It belongs to a stream that is gone and can never be completed, so it is not an event and
    /// nothing parseable is lost by dropping it.
    fn discard_tail(&mut self, from: &str) {
        if self.tail.is_empty() {
            return;
        }
        tracing::warn!(
            from,
            dropped = self.tail.len(),
            "dropping the partly written event of the stream that failed"
        );
        self.tail.clear();
    }

    /// The bytes still held when the source ended cleanly.
    ///
    /// A whole line is forwarded: the event simply arrived without its terminating blank line, and
    /// a client at end of stream can still use it. A fragment is dropped, because a truncated JSON
    /// payload is the very thing this module now refuses to hand a client.
    fn finish_tail(&mut self) -> Option<Bytes> {
        if self.tail.is_empty() {
            return None;
        }
        let held = std::mem::take(&mut self.tail);
        if held.ends_with(b"\n") {
            return Some(Bytes::from(held));
        }
        tracing::warn!(
            dropped = held.len(),
            "an event stream ended in the middle of an event; the fragment was dropped"
        );
        None
    }

    /// The winner's body failed. Decide what to do, and set up the next body.
    ///
    /// `from` is the participant whose body just died: the winner, or a replacement that failed in
    /// its turn.
    ///
    /// Returns [`FailOutcome::NeedRerun`] when the configured winner-fail mode may re-dispatch new
    /// upstream requests (`rerun-excluding` or `stable-fallback`) and no replacement is ready yet.
    fn fail(&mut self, err: axum::Error, from: &str) -> FailOutcome {
        // Nothing of the failed source is still in flight: a half-written event dies with it.
        self.discard_tail(from);
        let action = decide(
            self.written,
            self.tool_call,
            self.has_replacement(),
            self.policy,
            // A boundary can only be written where an event ends. When the wire is mid-line the
            // splice is refused instead of corrupting the client's parse — the whole reason this
            // module exists is to fail legibly rather than hand out a document that cannot be read.
            self.announceable && self.boundary_safe,
        );
        match action {
            RecoveryAction::Refuse => {
                // A winner-fail mode that re-dispatches can recover even when there is no
                // pre-built replacement: it asks for new upstream requests instead. That is only
                // sane when no tool-call delta has been emitted, nothing has already been switched
                // to, and we are not refusing because bytes are on the wire under `terminate`.
                let can_rerun = !self.tool_call
                    && !self.has_replacement()
                    && !(self.written > 0 && self.policy == MidstreamPolicy::Terminate)
                    // New requests replace the answer, so either nothing is on the wire yet or the
                    // switch can be announced where the client is listening for an event boundary.
                    && (self.written == 0 || (self.announceable && self.boundary_safe))
                    && self.rerun_rounds < self.rerun_max_rounds as u32
                    && matches!(
                        self.mode,
                        WinnerFailMode::RerunExcluding { .. }
                            | WinnerFailMode::StableFallback { .. }
                    );
                if can_rerun {
                    return FailOutcome::NeedRerun;
                }
                let reason = if self.tool_call {
                    "a tool-call delta was already emitted, and splicing two models' tool calls \
                     would assemble a call that may be valid and wrong"
                } else if !self.has_replacement() {
                    "no participant passed the race that could answer in the winner's place"
                } else {
                    match self.policy {
                        MidstreamPolicy::Terminate => {
                            "the combo's mid-stream policy is terminate, and bytes were already \
                             on the wire"
                        }
                        MidstreamPolicy::Restart => {
                            if self.announceable {
                                "the client's stream stopped in the middle of an event, so a \
                                 switch cannot be announced without breaking its parse"
                            } else {
                                "the client's body is not an event stream, so a switch cannot be \
                                 announced and two documents cannot be spliced"
                            }
                        }
                    }
                };
                tracing::warn!(
                    from,
                    written = self.written,
                    tool_call = self.tool_call,
                    "the winning model's stream failed and the request could not be recovered"
                );
                FailOutcome::Refused(axum::Error::new(std::io::Error::other(format!(
                    "the upstream stream failed and the request could not be recovered: {reason}: \
                     {err}"
                ))))
            }
            RecoveryAction::Silent => {
                let index = self.next;
                self.next += 1;
                self.source = Source::Replacement(index);
                self.current_id = self.replacements[index].id.clone();
                tracing::warn!(
                    from,
                    to = %self.current_id,
                    "the upstream stream failed before anything reached the client; continuing \
                     with the participant that passed"
                );
                FailOutcome::Switched
            }
            RecoveryAction::Restart => {
                let index = self.next;
                self.next += 1;
                let to = self.replacements[index].id.clone();
                self.pending
                    .extend(retry_boundary(self.client_dialect, from, &to));
                self.source = Source::Replacement(index);
                self.current_id = to;
                tracing::warn!(
                    from,
                    to = %self.current_id,
                    "the upstream stream failed mid-stream; continuing with the participant \
                     that passed"
                );
                FailOutcome::Switched
            }
        }
    }
}

/// The terminal error for a response whose fallback chain could not continue.
///
/// The original upstream error has already been consumed by `fail`; what matters to the client is
/// that the stream ended because every option was exhausted, not that it merely stopped.
fn fallback_gave_up() -> axum::Error {
    axum::Error::new(std::io::Error::other(
        "every upstream that passed the race has now failed, and no fallback round remains",
    ))
}

impl RecoveryBody {
    /// Start the next fallback round.
    ///
    /// Returns `None` when a round was started and the caller should keep polling. Returns the
    /// error to end the body with when it cannot: no fallback was configured, or the configured
    /// round budget is spent. The factory is deliberately *not* taken — a replacement can die as
    /// well, and a body that keeps polling a dead source with no factory left either spins inside
    /// `poll_frame` (pinning a worker) or ends the stream with no error and no retry boundary.
    fn start_rerun(&mut self) -> Option<axum::Error> {
        let rounds = self.rerun_rounds;
        if rounds >= u32::from(self.rerun_max_rounds) {
            tracing::warn!(
                rounds,
                limit = self.rerun_max_rounds,
                "the fallback retry budget is spent; ending the response with an error"
            );
            return Some(fallback_gave_up());
        }
        let Some(factory) = self.rerun_factory.as_mut() else {
            return Some(fallback_gave_up());
        };
        self.rerun_rounds = rounds + 1;
        let future = factory();
        self.rerun_future = Some(future);
        None
    }
}

impl http_body::Body for RecoveryBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let me = self.get_mut();
        loop {
            if me.ended {
                return Poll::Ready(None);
            }
            // The source ended cleanly and its held tail has already been written: this poll is the
            // end of the body, one step after the last frame so the frame above is not lost.
            if me.finished {
                me.ended = true;
                return Poll::Ready(None);
            }

            // Drive an in-flight fallback future (rerun-excluding / stable-fallback) to
            // completion before streaming anything else: its replacements become the body.
            if let Some(fut) = me.rerun_future.as_mut() {
                match Pin::new(fut).poll(cx) {
                    Poll::Ready(Ok(replacements)) => {
                        me.rerun_future = None;
                        if replacements.is_empty() {
                            me.ended = true;
                            return Poll::Ready(Some(Err(axum::Error::new(
                                std::io::Error::other(
                                    "the winning model's stream failed and no replacement \
                                 could be dispatched",
                                ),
                            ))));
                        }
                        let from = me.current_id.clone();
                        let to = replacements[0].id.clone();
                        me.replacements = replacements;
                        me.next = 1;
                        me.source = Source::Replacement(0);
                        me.current_id = to.clone();
                        // `fail` only allows a rerun once something is on the wire when the switch
                        // can be announced, so this is the same condition, stated where it matters.
                        if me.written > 0 && me.announceable && me.boundary_safe {
                            me.pending
                                .extend(retry_boundary(me.client_dialect, &from, &to));
                        }
                        continue;
                    }
                    Poll::Ready(Err(e)) => {
                        me.ended = true;
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            // The boundary goes out before the replacement's first byte: it is what tells the
            // client that the model it was talking to just changed. `fail` has already dropped
            // anything the failed source had half-written, so this always starts a fresh event.
            if let Some(frame) = me.pending.pop_front() {
                me.written += frame.len() as u64;
                return Poll::Ready(Some(Ok(Frame::data(frame))));
            }

            match me.source {
                Source::Winner => match Pin::new(&mut me.winner).poll_frame(cx) {
                    Poll::Ready(Some(Ok(frame))) => match me.forwardable(frame) {
                        Some(frame) => return Poll::Ready(Some(Ok(frame))),
                        None => continue,
                    },
                    Poll::Ready(Some(Err(err))) => {
                        let from = me.winner_id.clone();
                        match me.fail(err, &from) {
                            FailOutcome::Switched => continue,
                            FailOutcome::NeedRerun => match me.start_rerun() {
                                None => continue,
                                Some(err) => {
                                    me.ended = true;
                                    return Poll::Ready(Some(Err(err)));
                                }
                            },
                            FailOutcome::Refused(err) => {
                                me.ended = true;
                                return Poll::Ready(Some(Err(err)));
                            }
                        }
                    }
                    Poll::Ready(None) => {
                        me.finished = true;
                        if let Some(frame) = me.finish_tail() {
                            me.observe_bytes(&frame);
                            return Poll::Ready(Some(Ok(Frame::data(frame))));
                        }
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                },
                Source::Replacement(index) => {
                    match Pin::new(&mut me.replacements[index].body).poll_frame(cx) {
                        Poll::Ready(Some(Ok(frame))) => match me.forwardable(frame) {
                            Some(frame) => return Poll::Ready(Some(Ok(frame))),
                            None => continue,
                        },
                        Poll::Ready(Some(Err(err))) => {
                            // The replacement died too: fall through to the next participant that
                            // passed, in priority order.
                            let from = me.current_id.clone();
                            match me.fail(err, &from) {
                                FailOutcome::Switched => continue,
                                FailOutcome::NeedRerun => match me.start_rerun() {
                                    None => continue,
                                    Some(err) => {
                                        me.ended = true;
                                        return Poll::Ready(Some(Err(err)));
                                    }
                                },
                                FailOutcome::Refused(err) => {
                                    me.ended = true;
                                    return Poll::Ready(Some(Err(err)));
                                }
                            }
                        }
                        Poll::Ready(None) => {
                            me.finished = true;
                            if let Some(frame) = me.finish_tail() {
                                me.observe_bytes(&frame);
                                return Poll::Ready(Some(Ok(Frame::data(frame))));
                            }
                            continue;
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.ended
    }

    fn size_hint(&self) -> http_body::SizeHint {
        // A splice changes the length of the answer, so the winner's length is only trustworthy
        // when there is nothing to splice — and then the answer is exactly what it was.
        match self.source {
            Source::Winner if self.replacements.is_empty() => self.winner.size_hint(),
            _ => http_body::SizeHint::default(),
        }
    }
}

/// Wrap a committed winner's response so that a mid-stream failure can be recovered.
///
/// The response's headers are the winner's and stay the winner's: they are on the wire by the time
/// the body streams, and a replacement is not allowed to change them.
pub fn wrap(
    response: Response,
    replacements: Vec<Replacement>,
    config: RecoveryConfig,
) -> Response {
    let (parts, body) = response.into_parts();

    // Whether the client's body is an event stream. It decides whether a switch can be announced:
    // the boundary is an SSE frame, and a JSON document cannot carry one. A body whose type is
    // unknown is given the benefit of the doubt — the relay sets the type itself on the translated
    // path, and an upstream that streams without saying so is streaming events.
    let announceable = match parts
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) => {
            let value = value.to_ascii_lowercase();
            value.contains("text/event-stream") || !value.contains("json")
        }
        None => true,
    };

    let config = RecoveryConfig {
        announceable,
        ..config
    };
    let body = RecoveryBody::new(body, replacements, config);
    Response::from_parts(parts, AxumBody::new(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> ToolCallGuard {
        ToolCallGuard::new()
    }

    #[test]
    fn nothing_on_the_wire_switches_silently_under_both_policies() {
        for policy in [MidstreamPolicy::Restart, MidstreamPolicy::Terminate] {
            assert_eq!(
                decide(0, false, true, policy, true),
                RecoveryAction::Silent,
                "{policy:?}"
            );
        }
    }

    #[test]
    fn bytes_on_the_wire_follow_the_policy() {
        assert_eq!(
            decide(10, false, true, MidstreamPolicy::Restart, true),
            RecoveryAction::Restart
        );
        assert_eq!(
            decide(10, false, true, MidstreamPolicy::Terminate, true),
            RecoveryAction::Refuse
        );
    }

    #[test]
    fn a_tool_call_delta_refuses_the_splice_under_both_policies() {
        for policy in [MidstreamPolicy::Restart, MidstreamPolicy::Terminate] {
            assert_eq!(
                decide(0, true, true, policy, true),
                RecoveryAction::Refuse,
                "no bytes on the wire, but a tool call was emitted: {policy:?}"
            );
            assert_eq!(
                decide(10, true, true, policy, true),
                RecoveryAction::Refuse,
                "{policy:?}"
            );
        }
    }

    #[test]
    fn no_passed_participant_means_no_recovery() {
        for policy in [MidstreamPolicy::Restart, MidstreamPolicy::Terminate] {
            assert_eq!(
                decide(0, false, false, policy, true),
                RecoveryAction::Refuse,
                "{policy:?}"
            );
            assert_eq!(
                decide(10, false, false, policy, true),
                RecoveryAction::Refuse,
                "{policy:?}"
            );
        }
    }

    #[test]
    fn a_body_that_is_not_an_event_stream_cannot_carry_the_boundary() {
        assert_eq!(
            decide(10, false, true, MidstreamPolicy::Restart, false),
            RecoveryAction::Refuse
        );
        // Nothing on the wire is still an invisible switch: the client sees one document.
        assert_eq!(
            decide(0, false, true, MidstreamPolicy::Restart, false),
            RecoveryAction::Silent
        );
    }

    /// Both spellings of the blank line that ends an event, and no complete event without one.
    #[test]
    fn an_event_ends_at_a_blank_line_in_either_spelling() {
        assert_eq!(last_event_end(b"data: {}\n\n"), Some(10));
        assert_eq!(last_event_end(b"data: {}\r\n\r\n"), Some(12));
        // A fragment holds no complete event, however many lines it has.
        assert_eq!(last_event_end(b"data: {\"a\":"), None);
        assert_eq!(last_event_end(b"data: {}\n"), None);
        // The last event wins, so a buffer of several is forwarded up to the final blank line.
        assert_eq!(last_event_end(b"data: {}\n\ndata: {}\n\nrest"), Some(20));
    }

    /// A configuration for a body whose framing the test controls.
    fn body_config(announceable: bool) -> RecoveryConfig {
        RecoveryConfig {
            policy: MidstreamPolicy::Restart,
            mode: WinnerFailMode::PickHighestPassed,
            client_dialect: Dialect::OpenAi,
            winner_id: "first/race-model".to_owned(),
            announceable,
            rerun_factory: None,
            rerun_max_rounds: 1,
        }
    }

    /// A body that yields the given frames in order.
    fn body_of(frames: Vec<Result<Bytes, std::io::Error>>) -> AxumBody {
        AxumBody::from_stream(futures_util::stream::iter(frames))
    }

    /// Everything the body writes, and the error that ended it.
    async fn drain_body(mut body: RecoveryBody) -> (String, Option<String>) {
        use http_body_util::BodyExt as _;
        let mut text = String::new();
        loop {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        text.push_str(&String::from_utf8_lossy(data));
                    }
                }
                Some(Err(err)) => return (text, Some(err.to_string())),
                None => return (text, None),
            }
        }
    }

    /// A chunk boundary in the middle of an event delays the event, it does not split it.
    #[tokio::test]
    async fn an_event_reaches_the_client_whole() {
        let winner = body_of(vec![
            Ok(Bytes::from_static(b"data: {\"delta\":")),
            Ok(Bytes::from_static(b"\"one\"}\n")),
            Ok(Bytes::from_static(b"\ndata: {\"delta\":\"two\"}\n\n")),
        ]);
        let (text, error) =
            drain_body(RecoveryBody::new(winner, Vec::new(), body_config(true))).await;

        assert_eq!(error, None);
        assert_eq!(
            text,
            "data: {\"delta\":\"one\"}\n\ndata: {\"delta\":\"two\"}\n\n"
        );
    }

    /// A document that is not an event stream is never held back waiting for a blank line.
    #[tokio::test]
    async fn a_document_that_is_not_an_event_stream_is_forwarded_as_it_arrives() {
        let winner = body_of(vec![
            Ok(Bytes::from_static(b"{\"a\":1}")),
            Ok(Bytes::from_static(b"{\"b\":2}")),
        ]);
        let (text, error) =
            drain_body(RecoveryBody::new(winner, Vec::new(), body_config(false))).await;

        assert_eq!(error, None);
        assert_eq!(text, "{\"a\":1}{\"b\":2}");
    }

    /// An event-stream-like body that doesn't declare `text/event-stream` is still buffered to
    /// blank lines: `announceable` alone is enough. A whole trailing line is forwarded at end of
    /// stream, a fragment is dropped.
    #[tokio::test]
    async fn an_announceable_stream_buffers_to_blank_lines() {
        let winner = body_of(vec![
            Ok(Bytes::from_static(b"data: {\"delta\":\"whole\"}\n\n")),
            Ok(Bytes::from_static(b"data: {\"delta\":\"partial")),
        ]);
        let (text, error) =
            drain_body(RecoveryBody::new(winner, Vec::new(), body_config(true))).await;

        assert_eq!(error, None);
        // The whole event went out; the partial line was held and dropped at end of stream.
        assert_eq!(text, "data: {\"delta\":\"whole\"}\n\n");
    }

    /// When the wire stops mid-line, a splice would corrupt the client's parse: refuse instead.
    ///
    /// This is the case the reported failure came from. Nothing can be un-written, so the answer has
    /// to end with a legible error rather than with a line that is half one document and half the
    /// next.
    #[tokio::test]
    async fn a_splice_into_a_half_written_line_is_refused() {
        let winner = body_of(vec![
            Ok(Bytes::from_static(b"data: {\"delta\":\"half")),
            Err(std::io::Error::other("the upstream died mid-frame")),
        ]);
        let replacement = body_of(vec![Ok(Bytes::from_static(
            b"data: {\"delta\":\"second\"}\n\n",
        ))]);
        let (text, error) = drain_body(RecoveryBody::new(
            winner,
            vec![Replacement {
                id: "second/race-model".to_owned(),
                body: replacement,
            }],
            body_config(false),
        ))
        .await;

        assert!(
            error.is_some(),
            "the splice should have been refused: {text}"
        );
        assert!(text.contains("half"), "{text}");
        assert!(
            !text.contains("second"),
            "the splice happened anyway: {text}"
        );
        assert!(!text.contains("manarouter.retry"), "{text}");
    }

    #[test]
    fn the_guard_finds_the_openai_spelling() {
        let mut g = guard();
        assert!(g.observe(b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[]}}]}"));
    }

    #[test]
    fn the_guard_finds_the_anthropic_spelling() {
        let mut g = guard();
        assert!(g.observe(b"event: content_block_start\ndata: {\"type\":\"tool_use\"}"));
    }

    #[test]
    fn the_guard_ignores_an_ordinary_answer() {
        let mut g = guard();
        assert!(!g.observe(b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}"));
        assert!(!g.observe(b"data: [DONE]"));
    }

    #[test]
    fn the_guard_finds_a_marker_split_across_two_chunks() {
        let mut g = guard();
        assert!(!g.observe(b"data: {\"delta\":{\"tool_"));
        assert!(g.observe(b"calls\":[]}}"));
    }

    #[test]
    fn the_guard_finds_a_marker_split_at_every_point() {
        for split in 1.."tool_calls".len() {
            let mut g = guard();
            let (head, tail) = "tool_calls".split_at(split);
            assert!(!g.observe(head.as_bytes()), "split at {split}");
            assert!(g.observe(tail.as_bytes()), "split at {split}");
        }
    }

    #[test]
    fn the_boundary_names_the_dead_model_and_the_replacement() {
        let frames = retry_boundary(Dialect::OpenAi, "openai/a", "openai/b");
        assert_eq!(frames.len(), 2);

        let retry = String::from_utf8(frames[0].to_vec()).unwrap();
        assert!(retry.starts_with("event: manarouter.retry\n"), "{retry}");
        assert!(retry.contains(r#""reason":"upstream_failed""#), "{retry}");
        assert!(retry.contains(r#""from":"openai/a""#), "{retry}");
        assert!(retry.contains(r#""to":"openai/b""#), "{retry}");

        let terminator = String::from_utf8(frames[1].to_vec()).unwrap();
        assert_eq!(
            terminator,
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
        );
    }

    #[test]
    fn the_boundary_terminates_in_the_clients_dialect() {
        let frames = retry_boundary(Dialect::Anthropic, "anthropic/a", "anthropic/b");
        let terminator = String::from_utf8(frames[1].to_vec()).unwrap();
        assert_eq!(
            terminator,
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
    }
}
