//! The Cursor `agent.v1.AgentService` wire codec.
//!
//! # Why this is not a JSON dialect
//!
//! Cursor's agent endpoint is a **bidirectional** Connect-RPC stream. The client writes one
//! protobuf message inside a 5-byte-prefixed frame and leaves the request body open; the server
//! answers with a stream of framed protobuf messages that are *not* a finished answer — several of
//! them are questions the client must answer before the turn can proceed (request-context, blob
//! store reads and writes, interaction queries). A port that only decoded the text deltas would
//! appear to work and then stall on the first request-context message, with the server sending
//! nothing but heartbeats forever.
//!
//! That is why this module holds the whole conversation state machine, and why the relay calls it
//! from a dedicated branch rather than from the JSON translate pipeline: the request body is
//! binary, so `serde_json::from_slice` on the outgoing body would reject it before a byte was sent.
//!
//! # What lives here, and what does not
//!
//! * Framing, gzip, and the minimal protobuf reader/writer — here, so every byte can be unit-tested
//!   against canned frames.
//! * The OpenAI Chat Completions ↔ Cursor mapping, including the SSE rendering — here.
//! * Sockets, TLS, HTTP/2 and the credential — **not** here. `manarouter-relay::cursor_transport`
//!   owns the exchange behind a trait, and `mr-core` owns the token.
//!
//! # What this port deliberately does not carry
//!
//! The verified schema has no place for these, and inventing one would be worse than refusing:
//!
//! * **Tool calling.** `mcp_tools` is sent present-but-empty, exactly as the reference minimal port
//!   does. A client request carrying `tools` is refused with an actionable error instead of being
//!   silently run without them: the model would answer as if the tools did not exist, and the
//!   caller would never learn why.
//! * **Images and other non-text content parts.** `SelectedContext.selected_images` exists but its
//!   shape is not in the verified subset, so a multimodal request is refused rather than flattened
//!   into a prompt that drops the picture.
//! * **Generation parameters** (`temperature`, `top_p`, `max_tokens`, `stop`, `n`, …). The agent
//!   frame carries no such fields in the verified subset. They are dropped, which is visible here
//!   and in the report rather than pretended: the parameter is not "applied approximately", it is
//!   absent.
//! * **Reasoning tiers.** No effort field exists; [`mr_core::Dialect::Cursor`] advertises only
//!   `none`, so a level the operator asked for is rerouted by the ordinary `-<level>` rule instead
//!   of being sent as something else.

use std::collections::{HashMap, HashSet};

use bytes::Bytes;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{TranslateCtx, TranslateError, SSE_DONE};

/// Largest frame payload this decoder will buffer.
///
/// The reference implementation caps at 16 MiB. A declared length above the cap is a hard error
/// rather than a reason to grow the buffer: the length is attacker-controlled, and "allocate what
/// the peer asked for" is how a stream decoder becomes a memory-exhaustion bug.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// The reference port's safety timeout: five minutes of wall clock, not of idleness.
///
/// It is deliberately *not* an idle timer. Cursor sends heartbeats while a turn is alive, so an
/// idle timer would never fire and a stuck turn would hold the client connection open forever.
pub const STREAM_TIMEOUT_MS: u64 = 300_000;

/// How long a composer turn is given to send anything else after a blob-store write.
///
/// Some composer models never send the authoritative `turn_ended`; their last act is a blob write.
/// Ending on it immediately would be wrong for every other model, which is why this is bounded to
/// composer models and to a short grace window.
pub const KV_GRACE_MS: u64 = 2_000;

/// The HTTP/2 request path of the agent stream.
pub const RUN_PATH: &str = "/agent.v1.AgentService/Run";

/// The unary endpoint that reports which host serves agent turns.
pub const SERVER_CONFIG_PATH: &str = "/aiserver.v1.ServerConfigService/GetServerConfig";

/// The host that serves [`SERVER_CONFIG_PATH`].
///
/// It is *not* an agent host: the agent origin arrives in the response and is allow-listed by
/// [`validate_origin`]. Dialling this host for a turn would be rejected.
pub const DISCOVERY_ORIGIN: &str = "https://api2.cursor.sh";

/// The Connect protocol version the endpoint expects.
pub const CONNECT_PROTOCOL_VERSION: &str = "1";

/// The user agent the reference client sends. Cursor fingerprints the CLI's shape; this one is the
/// pinned value from the spec, not a guess at a browser identity.
pub const CONNECT_USER_AGENT: &str = "connect-es/1.6.1";

/// The CLI version sent as `x-cursor-client-version` when the environment does not override it.
pub const PINNED_CLIENT_VERSION: &str = "cli-2026.07.08-0c04a8a";

/// The environment variable that overrides [`PINNED_CLIENT_VERSION`].
pub const CLIENT_VERSION_ENV: &str = "CURSOR_AGENT_CLI_VERSION";

/// Origin used when the server-config call cannot be made or names no host.
///
/// The `agent.` host is the ghost-mode (privacy) one; see [`AgentUrls::select`].
pub const FALLBACK_AGENT_ORIGIN: &str = "https://agent.api5.cursor.sh";

/// Origin for a non-ghost turn when discovery is unavailable.
pub const FALLBACK_AGENTN_ORIGIN: &str = "https://agentn.api5.cursor.sh";

const FLAG_GZIP: u8 = 0x01;
const FLAG_END_STREAM: u8 = 0x02;

/// A Connect-RPC or protobuf failure.
///
/// Kept separate from [`TranslateError`] because the relay needs to distinguish "the upstream said
/// something we could not decode" from "we refused to build the request", and because the unit
/// tests assert on these values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CursorError {
    /// A frame header declared more bytes than the cap allows.
    #[error("cursor frame declares {declared} bytes; the cap is {cap}")]
    FrameTooLarge {
        /// The declared payload length.
        declared: usize,
        /// The cap that was exceeded.
        cap: usize,
    },

    /// A frame declared gzip but did not contain a valid gzip stream.
    #[error("cursor frame is not valid gzip")]
    Gzip,

    /// A protobuf message was truncated or used a wire type this codec does not read.
    #[error("malformed cursor protobuf: {0}")]
    Protobuf(&'static str),

    /// The server asked us to execute something this transport cannot answer.
    ///
    /// Reported immediately rather than after the reference implementation's 15-second watchdog:
    /// there is nothing to wait for once the variant is known to be unanswerable, and a silent
    /// stall is the failure mode this whole module exists to avoid.
    #[error(
        "cursor asked this transport to execute protobuf field {field} of ExecServerMessage, which \
         a text-only transport cannot answer; the turn would stall here"
    )]
    UnanswerableExec {
        /// The `ExecServerMessage` field number of the request variant.
        field: u32,
    },

    /// The server ended the stream with an error.
    #[error("cursor reported an error: {0}")]
    Server(String),

    /// A header value this transport refused to send.
    #[error("refusing to send cursor header {0}: the value is empty or would forge a new header")]
    Header(&'static str),
}

impl From<CursorError> for TranslateError {
    fn from(e: CursorError) -> Self {
        TranslateError::MalformedResponse {
            dialect: "cursor",
            detail: e.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------------------------

/// One decoded Connect frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The payload, with gzip already removed.
    pub payload: Vec<u8>,
    /// Whether this frame is the end-of-stream trailer (`flags & 0x02`).
    pub trailer: bool,
}

/// Build an uncompressed frame: 1 flag byte, 4-byte big-endian length, payload.
pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(0);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Build a gzip-compressed frame. Used for the request when the payload is large enough that the
/// compressor earns its keep; the endpoint accepts either form on the same stream.
pub fn encode_gzip_frame(payload: &[u8]) -> Result<Vec<u8>, CursorError> {
    use std::io::Write as _;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(payload).map_err(|_| CursorError::Gzip)?;
    let compressed = encoder.finish().map_err(|_| CursorError::Gzip)?;
    let mut out = Vec::with_capacity(5 + compressed.len());
    out.push(FLAG_GZIP);
    out.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
    out.extend_from_slice(&compressed);
    Ok(out)
}

/// A streaming frame decoder.
///
/// Bytes arrive in arbitrary chunks; frames do not. The decoder buffers until a whole frame is
/// present, which is why the declared length is checked *before* anything is allocated for it.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    /// Set once a frame header was rejected; the stream cannot be resynchronised after that, and a
    /// second error would replace the first, more useful one.
    poisoned: Option<CursorError>,
}

impl FrameDecoder {
    /// A decoder with an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes buffered but not yet part of a complete frame.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Feed bytes and take every complete frame they complete.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, CursorError> {
        if let Some(e) = &self.poisoned {
            return Err(e.clone());
        }
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            if self.buf.len() < 5 {
                break;
            }
            let flags = match self.buf.first() {
                Some(f) => *f,
                None => break,
            };
            let mut len_bytes = [0u8; 4];
            len_bytes.copy_from_slice(&self.buf[1..5]);
            let declared = u32::from_be_bytes(len_bytes) as usize;
            if declared > MAX_FRAME_BYTES {
                let e = CursorError::FrameTooLarge {
                    declared,
                    cap: MAX_FRAME_BYTES,
                };
                self.poisoned = Some(e.clone());
                // Drop the buffer: it can never be decoded, and holding it keeps the memory the
                // cap exists to protect.
                self.buf = Vec::new();
                return Err(e);
            }
            if self.buf.len() < 5 + declared {
                break;
            }
            let payload = self.buf[5..5 + declared].to_vec();
            self.buf.drain(..5 + declared);
            let payload = if flags & FLAG_GZIP != 0 {
                gunzip(&payload)?
            } else {
                payload
            };
            frames.push(Frame {
                payload,
                trailer: flags & FLAG_END_STREAM != 0,
            });
        }
        Ok(frames)
    }

    /// Finish the stream, rejecting a trailing partial frame.
    ///
    /// A partial frame at `END_STREAM` means the peer truncated a message. Returning `Ok` would
    /// turn a broken answer into a short one, which the client cannot tell from a complete answer.
    pub fn finish(&mut self) -> Result<(), CursorError> {
        if let Some(e) = &self.poisoned {
            return Err(e.clone());
        }
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(CursorError::Protobuf("truncated frame at end of stream"))
        }
    }
}

fn gunzip(payload: &[u8]) -> Result<Vec<u8>, CursorError> {
    use std::io::Read as _;
    let mut decoder = flate2::read::GzDecoder::new(payload);
    let mut out = Vec::new();
    // Bounded by the frame cap already: the decoder reads from a slice whose length was checked
    // against `MAX_FRAME_BYTES`, and a gzip bomb is still bounded by that input size only if the
    // output is bounded too. `take` is what bounds it.
    let limit = MAX_FRAME_BYTES as u64 + 1;
    decoder
        .by_ref()
        .take(limit)
        .read_to_end(&mut out)
        .map_err(|_| CursorError::Gzip)?;
    if out.len() > MAX_FRAME_BYTES {
        return Err(CursorError::FrameTooLarge {
            declared: out.len(),
            cap: MAX_FRAME_BYTES,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Protobuf: just enough to read and write the messages in the verified schema
// ---------------------------------------------------------------------------------------------

/// A hand-written protobuf subset.
///
/// `prost` would be the obvious dependency, and it was rejected deliberately: the schema here is
/// fifteen messages with field numbers the spec calls authoritative, and a generated module would
/// hide the exact bytes this port has to be tested against. The reader is total — every short read
/// is an error, never a panic — because it runs on the request path with `panic = "abort"`.
pub mod pb {
    use super::CursorError;

    /// Append a base-128 varint.
    pub fn put_varint(out: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    /// Append a tag for `field` with the given wire type.
    pub fn put_tag(out: &mut Vec<u8>, field: u32, wire: u8) {
        put_varint(out, (u64::from(field) << 3) | u64::from(wire));
    }

    /// Append a `uint64`/`bool`/`enum` field.
    pub fn put_uint(out: &mut Vec<u8>, field: u32, value: u64) {
        put_tag(out, field, 0);
        put_varint(out, value);
    }

    /// Append a `bool` field, including when it is `false`.
    ///
    /// Proto3 would normally omit a `false` value. `exclude_workspace_context` is written
    /// explicitly anyway because the reference client does, and because "the field was considered
    /// and set false" is what tells the server not to attach workspace context.
    pub fn put_bool(out: &mut Vec<u8>, field: u32, value: bool) {
        put_uint(out, field, u64::from(value));
    }

    /// Append a length-delimited field (`bytes`, `string`, or an embedded message).
    pub fn put_bytes(out: &mut Vec<u8>, field: u32, value: &[u8]) {
        put_tag(out, field, 2);
        put_varint(out, value.len() as u64);
        out.extend_from_slice(value);
    }

    /// Append a `string` field.
    pub fn put_str(out: &mut Vec<u8>, field: u32, value: &str) {
        put_bytes(out, field, value.as_bytes());
    }

    /// A decoded protobuf value.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Value<'a> {
        /// Wire type 0.
        Varint(u64),
        /// Wire type 1, always eight bytes.
        Fixed64(&'a [u8]),
        /// Wire type 2, the raw bytes (not necessarily UTF-8).
        Bytes(&'a [u8]),
        /// Wire type 5, always four bytes.
        Fixed32(&'a [u8]),
    }

    impl<'a> Value<'a> {
        /// The value as a length-delimited byte slice, if it is one.
        pub fn bytes(self) -> Option<&'a [u8]> {
            match self {
                Self::Bytes(b) => Some(b),
                _ => None,
            }
        }

        /// The value as a varint, if it is one.
        pub fn uint(self) -> Option<u64> {
            match self {
                Self::Varint(v) => Some(v),
                _ => None,
            }
        }

        /// The value as UTF-8, if it is a length-delimited field containing text.
        pub fn str(self) -> Option<&'a str> {
            std::str::from_utf8(self.bytes()?).ok()
        }
    }

    /// A cursor over one protobuf message.
    #[derive(Debug, Clone)]
    pub struct Reader<'a> {
        buf: &'a [u8],
        pos: usize,
    }

    impl<'a> Reader<'a> {
        /// Read `buf` as one message.
        pub fn new(buf: &'a [u8]) -> Self {
            Self { buf, pos: 0 }
        }

        /// Bytes not yet consumed.
        pub fn remaining(&self) -> usize {
            self.buf.len().saturating_sub(self.pos)
        }

        /// The next field, or `None` at the end.
        ///
        /// Named for what it returns rather than `next`: this is a wire reader, not an iterator —
        /// it borrows from the buffer it was built over and stops at a malformed field with an
        /// error, which is not a shape `Iterator` can express.
        pub fn next_field(&mut self) -> Result<Option<(u32, Value<'a>)>, CursorError> {
            if self.pos >= self.buf.len() {
                return Ok(None);
            }
            let key = self.varint()?;
            let field = key >> 3;
            let wire = (key & 0x07) as u8;
            let field = u32::try_from(field)
                .map_err(|_| CursorError::Protobuf("field number out of range"))?;
            if field == 0 {
                return Err(CursorError::Protobuf("field number zero"));
            }
            let value = match wire {
                0 => Value::Varint(self.varint()?),
                1 => Value::Fixed64(self.take(8)?),
                2 => {
                    let len = self.varint()?;
                    let len = usize::try_from(len)
                        .map_err(|_| CursorError::Protobuf("length out of range"))?;
                    Value::Bytes(self.take(len)?)
                }
                5 => Value::Fixed32(self.take(4)?),
                // Groups were removed from proto3 and nothing in this schema uses them.
                3 | 4 => return Err(CursorError::Protobuf("group wire type")),
                _ => return Err(CursorError::Protobuf("unknown wire type")),
            };
            Ok(Some((field, value)))
        }

        fn varint(&mut self) -> Result<u64, CursorError> {
            let mut value = 0u64;
            for shift in 0..10u32 {
                let byte = *self
                    .buf
                    .get(self.pos)
                    .ok_or(CursorError::Protobuf("truncated varint"))?;
                self.pos += 1;
                value |= u64::from(byte & 0x7f) << (shift * 7);
                if byte & 0x80 == 0 {
                    return Ok(value);
                }
            }
            Err(CursorError::Protobuf("varint longer than ten bytes"))
        }

        fn take(&mut self, len: usize) -> Result<&'a [u8], CursorError> {
            let end = self
                .pos
                .checked_add(len)
                .ok_or(CursorError::Protobuf("length overflow"))?;
            let slice = self
                .buf
                .get(self.pos..end)
                .ok_or(CursorError::Protobuf("truncated field"))?;
            self.pos = end;
            Ok(slice)
        }
    }

    /// The first length-delimited occurrence of `field`, for the many messages that have exactly
    /// one.
    pub fn message(buf: &[u8], field: u32) -> Result<Option<&[u8]>, CursorError> {
        let mut r = Reader::new(buf);
        while let Some((f, v)) = r.next_field()? {
            if f == field {
                return match v {
                    Value::Bytes(b) => Ok(Some(b)),
                    _ => Err(CursorError::Protobuf("field has the wrong wire type")),
                };
            }
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------------------------------------
// Request assembly
// ---------------------------------------------------------------------------------------------

/// A run request ready to be framed onto the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRun {
    /// The model id the frame names.
    pub model: String,
    /// The conversation id, also used as `conversation_group_id`.
    pub conversation_id: String,
    /// The user message id.
    pub message_id: String,
    /// The flattened prompt actually sent.
    pub text: String,
    /// The framed `AgentRunRequest` (header included).
    pub frame: Vec<u8>,
}

/// The OpenAI request as Cursor's `UserMessage` sees it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Conversation {
    system: Option<String>,
    text: String,
}

/// Build the run frame from an OpenAI Chat Completions body.
///
/// `message_id` is supplied by the caller because it must be unique per turn; the codec has no
/// random source and generating one here would make the unit tests non-deterministic.
pub fn prepare_run(
    ctx: &TranslateCtx<'_>,
    body: &[u8],
    message_id: &str,
) -> Result<PreparedRun, TranslateError> {
    // The body is expected to be OpenAI-shaped regardless of which dialect the client spoke: the
    // relay translates an Anthropic request first (`mr_dialects::translate_request`), so by the time
    // a frame is built there is one shape. What is rejected here is a body that is not chat-shaped
    // at all, which `flatten` reports with an actionable detail.
    let value: Value =
        serde_json::from_slice(body).map_err(|e| TranslateError::MalformedRequest {
            dialect: "openai",
            detail: e.to_string(),
        })?;
    let conversation = flatten(&value).map_err(flatten_error)?;

    let model = wire_model(ctx.upstream_model);
    let conversation_id = wire_conversation_id(ctx.session_id, &conversation, message_id);
    let frame = encode_frame(&run_request(
        &model,
        &conversation,
        &conversation_id,
        message_id,
    ));
    Ok(PreparedRun {
        model,
        conversation_id,
        message_id: message_id.to_owned(),
        text: conversation.text,
        frame,
    })
}

/// `auto` is Cursor's automatic model picker; the agent endpoint spells it `default`.
fn wire_model(model: &str) -> String {
    if model.is_empty() || model.eq_ignore_ascii_case("auto") {
        "default".to_owned()
    } else {
        model.to_owned()
    }
}

/// Why a request could not be flattened.
enum FlattenError {
    Unsupported(String),
    Malformed(String),
}

/// Both refusals reach the operator as a message.
///
/// `TranslateError::Unsupported` carries only a dialect pairing, which would leave a caller holding
/// "unsupported" and no idea what to remove; the detail is what makes the refusal actionable, and
/// `MalformedRequest` is the variant that preserves it.
fn flatten_error(e: FlattenError) -> TranslateError {
    match e {
        FlattenError::Unsupported(detail) | FlattenError::Malformed(detail) => {
            TranslateError::MalformedRequest {
                dialect: "openai",
                detail,
            }
        }
    }
}

fn flatten(value: &Value) -> Result<Conversation, FlattenError> {
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| FlattenError::Malformed("`messages` must be an array".to_owned()))?;

    if let Some(tools) = value.get("tools").and_then(Value::as_array) {
        if !tools.is_empty() {
            return Err(FlattenError::Unsupported(
                "the Cursor transport does not implement tool calling yet; remove `tools` from the \
                 request or route this model to a provider that does"
                    .to_owned(),
            ));
        }
    }

    let mut system: Vec<String> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut user_texts: Vec<String> = Vec::new();
    let mut user_messages = 0usize;
    let mut turns = 0usize;

    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| FlattenError::Malformed("every message needs a `role`".to_owned()))?;
        if !matches!(role, "system" | "developer") {
            turns += 1;
        }
        match role {
            "system" | "developer" => {
                let text = content_text(message)?;
                if !text.is_empty() {
                    system.push(text);
                }
            }
            "user" => {
                user_messages += 1;
                let text = content_text(message)?;
                user_texts.push(text.clone());
                lines.push(format!("User: {text}"));
            }
            "assistant" => {
                let text = content_text(message)?;
                if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        lines.push(format_tool_call(call));
                    }
                }
                if !text.is_empty() {
                    lines.push(format!("Assistant: {text}"));
                }
            }
            "tool" => {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                lines.push(format!("Tool result ({id}): {}", content_text(message)?));
            }
            "function" => {
                let name = message
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                lines.push(format!(
                    "Function result ({name}): {}",
                    content_text(message)?
                ));
            }
            other => {
                return Err(FlattenError::Malformed(format!(
                    "unsupported message role `{other}`"
                )));
            }
        }
    }

    // The agent frame has exactly one `text` field and no role structure, so history becomes text.
    // A single user message is sent verbatim, because prefixing it with "User: " would change the
    // prompt for the common case; anything longer gets the `User:`/`Assistant:` labels so the model
    // can tell the turns apart.
    let text = if turns == 1 && user_messages == 1 {
        user_texts.first().cloned().unwrap_or_default()
    } else {
        lines.join("\n\n")
    };
    if text.is_empty() && system.is_empty() {
        return Err(FlattenError::Malformed(
            "the request contains no text to send".to_owned(),
        ));
    }
    let system = if system.is_empty() {
        None
    } else {
        Some(system.join("\n\n"))
    };
    // The system prompt travels twice: as the digest in `conversation_state` (so the server can
    // recognise a prompt it has cached) and prepended to the user text. The digest alone is not
    // enough — Cursor is observed not to apply it — and the text is what actually reaches the model.
    let text = match &system {
        Some(system) => format!("{system}\n\n{text}"),
        None => text,
    };
    Ok(Conversation { system, text })
}

fn format_tool_call(call: &Value) -> String {
    let name = call
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let id = call.get("id").and_then(Value::as_str).unwrap_or("unknown");
    let arguments = call
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(Value::as_str)
        .unwrap_or("{}");
    format!("Assistant called tool {name} ({id}) with arguments: {arguments}")
}

/// The text of a message's `content`, accepting the string and text-parts forms.
fn content_text(message: &Value) -> Result<String, FlattenError> {
    match message.get("content") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(parts)) => {
            let mut out = Vec::with_capacity(parts.len());
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str).ok_or_else(|| {
                    FlattenError::Malformed("a content part needs a `type`".to_owned())
                })?;
                match kind {
                    "text" | "input_text" => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            out.push(text.to_owned());
                        }
                    }
                    other => {
                        return Err(FlattenError::Unsupported(format!(
                            "the Cursor transport is text-only and cannot carry a `{other}` content \
                             part; remove it or route this model to a multimodal provider"
                        )));
                    }
                }
            }
            Ok(out.join("\n"))
        }
        Some(_) => Err(FlattenError::Malformed(
            "`content` must be a string or an array of parts".to_owned(),
        )),
    }
}

/// The conversation id, stable for a session and identical to `conversation_group_id`.
///
/// Cursor keys the agent's memory on this id. It must therefore be stable across the turns of one
/// conversation, **and** must not be shared between conversations that merely happen to start with
/// the same words. The two inputs are used in that order:
///
/// * `session_id` when the relay could derive one. It is the only per-conversation value available
///   without state, and hashing it means a change of model or system prompt does not silently start
///   a new conversation.
/// * otherwise a fingerprint that includes the message id, which is unique per turn: no
///   continuity, but also no state leaking between two callers with an identical first prompt.
///
/// The reference client's fallback is a fingerprint of the system prompt, first user message and
/// tools with *no* per-turn value, which buys a prompt-cache hit at the cost of two unrelated
/// callers who open with the same words sharing one Cursor conversation — and therefore one
/// memory. Given the choice between a cache miss and answering one person with another person's
/// context, this takes the miss. A caller that wants continuity supplies `session_id`.
fn wire_conversation_id(
    session_id: Option<&str>,
    conversation: &Conversation,
    message_id: &str,
) -> String {
    match session_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(session) => uuid_from_hash(&format!("cursor-session:{session}")),
        None => {
            let mut hasher = Sha256::new();
            hasher.update(b"cursor-turn:");
            hasher.update(conversation.system.as_deref().unwrap_or("").as_bytes());
            hasher.update(b"\0");
            hasher.update(conversation.text.as_bytes());
            hasher.update(b"\0");
            hasher.update(message_id.as_bytes());
            hex_or_zero(&hasher.finalize(), 16)
        }
    }
}

/// A UUID-shaped id from a SHA-256 of the seed.
///
/// Version 4 and the RFC 4122 variant bits are set so the value is a *valid* UUID rather than merely
/// a 36-character string: the endpoint parses these ids as UUIDs.
fn uuid_from_hash(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    hex_or_zero(&digest, 16)
}

/// Render the first sixteen bytes of a digest as a version-4-shaped UUID.
fn hex_or_zero(digest: &[u8], take: usize) -> String {
    let mut bytes = [0u8; 16];
    for (slot, byte) in bytes.iter_mut().zip(digest.iter().take(take)) {
        *slot = *byte;
    }
    // Version 4, variant RFC 4122.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut out = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The bytes of a framed `AgentRunRequest`.
fn run_request(
    model: &str,
    conversation: &Conversation,
    conversation_id: &str,
    message_id: &str,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(512);

    // 1: conversation_state — the digest of the JSON prompt blob, and no turns.
    //
    // The field is a sha256 *digest*, not the JSON. The server uses it to recognise a system prompt
    // it has already cached for this conversation; sending the blob itself would be a different
    // value and the cache would never hit.
    if let Some(system) = &conversation.system {
        let blob = serde_json::json!({
            "role": "system",
            "content": system,
        });
        let bytes = serde_json::to_vec(&blob).unwrap_or_default();
        let digest = Sha256::digest(&bytes);
        let mut state = Vec::new();
        pb::put_bytes(&mut state, 1, &digest);
        pb::put_bytes(&mut out, 1, &state);
    }

    // 2: action -> user_message_action -> user_message
    let mut user_message = Vec::new();
    pb::put_str(&mut user_message, 1, &conversation.text);
    pb::put_str(&mut user_message, 2, message_id);
    // 3: selected_context — an empty *message*, present but zero-length. Required.
    pb::put_bytes(&mut user_message, 3, &[]);
    // 4: mode = 1 (the CLI's "agent" mode).
    pb::put_uint(&mut user_message, 4, 1);
    let mut action = Vec::new();
    pb::put_bytes(&mut action, 1, &user_message);
    pb::put_bytes(&mut out, 2, &action);

    // 3: model_details
    let mut details = Vec::new();
    pb::put_str(&mut details, 1, model);
    pb::put_str(&mut details, 3, model);
    pb::put_str(&mut details, 4, model);
    pb::put_bytes(&mut out, 3, &details);

    // 4: mcp_tools — present and empty. Omitting the message entirely is not the same thing: the
    // server reads "no tool list was sent" as a client that did not implement tools.
    pb::put_bytes(&mut out, 4, &[]);

    // 5: conversation_id
    pb::put_str(&mut out, 5, conversation_id);

    // 9: requested_model — model id only. No parameters: the verified subset has no parameter id
    // the server is documented to accept, and an unknown one is a rejection.
    let mut requested = Vec::new();
    pb::put_str(&mut requested, 1, model);
    pb::put_bytes(&mut out, 9, &requested);

    // 12: exclude_workspace_context = false, written explicitly.
    pb::put_bool(&mut out, 12, false);

    // 16: conversation_group_id — the same id, deliberately.
    pb::put_str(&mut out, 16, conversation_id);

    out
}

// ---------------------------------------------------------------------------------------------
// Host discovery
// ---------------------------------------------------------------------------------------------

/// The two agent origins the server-config endpoint reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentUrls {
    /// The `agent.` origin, used when ghost mode is on.
    pub agent: String,
    /// The `agentn.` origin, used when ghost mode is off.
    pub agentn: String,
}

impl AgentUrls {
    /// Pick the origin for a turn, and whether it came from the server's configuration.
    ///
    /// Ghost mode defaults to **on**, which is the privacy-preserving choice: a turn that does not
    /// explicitly ask for training-visible data should not get it.
    ///
    /// A configured url that fails [`validate_origin`] falls back to the known constant rather than
    /// failing the turn. The allow-list still does its job — the bad url is never dialled — and a
    /// corrupt or hostile discovery response degrades to the host the reference client ships with
    /// instead of turning every request into an outage. The `bool` is what lets the caller log the
    /// fallback.
    pub fn select(&self, ghost_mode: bool) -> (String, bool) {
        let (chosen, fallback) = if ghost_mode {
            (&self.agent, FALLBACK_AGENT_ORIGIN)
        } else {
            (&self.agentn, FALLBACK_AGENTN_ORIGIN)
        };
        if let Ok(origin) = validate_origin(chosen) {
            return (origin, true);
        }
        (fallback.to_owned(), false)
    }
}

/// Read `AgentUrlConfig` out of a `GetServerConfig` response.
///
/// Field 27 of `ServerConfig` is `AgentUrlConfig{1: agent_url, 2: agentn_url}`. A response without
/// it is not an error: the caller falls back, and the fallback is a constant that is known to work.
pub fn parse_agent_url_config(payload: &[u8]) -> Option<AgentUrls> {
    let config = pb::message(payload, 27).ok().flatten()?;
    let agent = pb::message(config, 1)
        .ok()
        .flatten()
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or_default()
        .to_owned();
    let agentn = pb::message(config, 2)
        .ok()
        .flatten()
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or_default()
        .to_owned();
    Some(AgentUrls { agent, agentn })
}

/// Validate a discovered agent url and return its origin.
///
/// The allow-list is the security boundary, not a formality: the url arrives in a response body and
/// the relay is about to attach a bearer token to it. `api5.cursor.sh` and its subdomains are the
/// only hosts the real service uses; anything else — including a lookalike such as
/// `api5.cursor.sh.evil.example` — is refused. Userinfo is refused because `https://user@host/`
/// would send the *url's* userinfo, and a query or fragment has no meaning on an origin.
pub fn validate_origin(url: &str) -> Result<String, &'static str> {
    let rest = url
        .strip_prefix("https://")
        .ok_or("an agent url must use https")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return Err("the agent url has no host");
    }
    if authority.contains('@') {
        return Err("an agent url must not carry userinfo");
    }
    let lower = authority.to_ascii_lowercase();
    let host = match lower.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => lower.as_str(),
    };
    let allowed = host == "api5.cursor.sh" || host.ends_with(".api5.cursor.sh");
    if !allowed {
        return Err("an agent url must point at api5.cursor.sh");
    }
    // Rebuilt from the lowercased authority so a mixed-case host does not defeat the SNI.
    Ok(format!("https://{lower}"))
}

// ---------------------------------------------------------------------------------------------
// Response rendering
// ---------------------------------------------------------------------------------------------

/// The two identifiers that must agree with each other on every request.
///
/// `request_id` is a fresh UUID v4 per turn and `traceparent` is a W3C trace context whose trace id
/// is unrelated to it. The reference client sends the *same* request id in both `x-request-id` and
/// `x-original-request-id`, which is what makes a retry deduplicable server-side; sending different
/// values makes the server treat a retry as a second turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestIdentity {
    /// UUID v4, lowercase, hyphenated.
    pub request_id: String,
    /// `00-<32 hex trace id>-<16 hex span id>-01`.
    pub traceparent: String,
}

impl RequestIdentity {
    /// Validate both halves, returning the offending value's name on failure.
    ///
    /// The point is not tidiness: these strings are interpolated into headers, so a value carrying
    /// a newline would forge additional headers.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !is_uuid_v4(&self.request_id) {
            return Err("x-request-id");
        }
        if !is_traceparent(&self.traceparent) {
            return Err("traceparent");
        }
        Ok(())
    }
}

pub fn is_uuid_v4(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        let ok = match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            14 => *byte == b'4',
            19 => matches!(*byte, b'8' | b'9' | b'a' | b'b'),
            _ => byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase(),
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Whether `value` is a W3C trace context this transport would send.
pub fn is_traceparent(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 4 {
        return false;
    }
    if parts[0] != "00" || parts[3] != "01" {
        return false;
    }
    let hex = |part: &str, len: usize| {
        part.len() == len
            && part
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    };
    hex(parts[1], 32) && hex(parts[2], 16)
}

/// A client version this transport will put in `x-cursor-client-version`.
///
/// The server is known to reject unknown build dates, so anything that is not shaped like
/// `cli-YYYY.MM.DD-<hex>` is replaced by [`PINNED_CLIENT_VERSION`] rather than sent.
pub fn is_client_version(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("cli-") else {
        return false;
    };
    let Some((date, build)) = rest.split_at_checked(10) else {
        return false;
    };
    let date_bytes = date.as_bytes();
    if date_bytes.len() != 10
        || date_bytes[4] != b'.'
        || date_bytes[7] != b'.'
        || !build.starts_with('-')
    {
        return false;
    }
    if !date_bytes
        .iter()
        .enumerate()
        .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit())
    {
        return false;
    }
    let build = &build[1..];
    (4..=16).contains(&build.len())
        && build
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// The headers a run request carries, in a fixed order so a test can compare the whole list.
///
/// Only gzip is advertised. The reference client sends no `x-cursor-checksum`, no `x-client-key`,
/// no `x-session-id`, no timezone and no `Host` override; adding any of them changes the request
/// fingerprint the server sees, so their absence is asserted rather than left to chance.
pub fn request_headers(
    authorization: &str,
    ghost_mode: bool,
    client_version: &str,
    identity: &RequestIdentity,
) -> Result<Vec<(String, String)>, CursorError> {
    identity.validate().map_err(CursorError::Header)?;
    if authorization.is_empty() || authorization.contains(['\r', '\n']) {
        return Err(CursorError::Header("authorization"));
    }
    let version = if is_client_version(client_version) {
        client_version
    } else {
        PINNED_CLIENT_VERSION
    };
    Ok(vec![
        ("authorization".to_owned(), authorization.to_owned()),
        (
            "content-type".to_owned(),
            "application/connect+proto".to_owned(),
        ),
        (
            "connect-protocol-version".to_owned(),
            CONNECT_PROTOCOL_VERSION.to_owned(),
        ),
        ("connect-accept-encoding".to_owned(), "gzip".to_owned()),
        ("user-agent".to_owned(), CONNECT_USER_AGENT.to_owned()),
        ("x-cursor-client-type".to_owned(), "cli".to_owned()),
        ("x-cursor-client-version".to_owned(), version.to_owned()),
        (
            "x-ghost-mode".to_owned(),
            if ghost_mode { "true" } else { "false" }.to_owned(),
        ),
        ("x-request-id".to_owned(), identity.request_id.clone()),
        (
            "x-original-request-id".to_owned(),
            identity.request_id.clone(),
        ),
        (
            "backend-traceparent".to_owned(),
            identity.traceparent.clone(),
        ),
        ("traceparent".to_owned(), identity.traceparent.clone()),
    ])
}

/// The header list for the unary `GetServerConfig` call.
///
/// Deliberately shorter than [`request_headers`]: the call is unary protobuf, not a Connect stream,
/// so it carries no `connect-protocol-version`, no `x-ghost-mode` and no trace context. Sending the
/// stream headers here would make the request look like something it is not.
pub fn discovery_headers(authorization: &str) -> Result<Vec<(String, String)>, CursorError> {
    if authorization.is_empty() || authorization.contains(['\r', '\n']) {
        return Err(CursorError::Header("authorization"));
    }
    Ok(vec![
        ("authorization".to_owned(), authorization.to_owned()),
        ("content-type".to_owned(), "application/proto".to_owned()),
        ("accept".to_owned(), "application/proto".to_owned()),
        ("user-agent".to_owned(), CONNECT_USER_AGENT.to_owned()),
    ])
}

/// Token counts from `turn_ended`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Generated tokens.
    pub output_tokens: u64,
    /// Prompt tokens served from cache.
    pub cache_read: u64,
    /// Prompt tokens written to cache.
    pub cache_write: u64,
    /// Reasoning tokens, a subset of the output count.
    pub reasoning: u64,
}

/// Why the turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The authoritative `turn_ended` field.
    TurnEnded,
    /// The HTTP/2 stream ended, or the trailer carried no error.
    ServerEnd,
    /// The wall clock expired; the partial answer is finished with `length`.
    Timeout,
    /// A composer model wrote a blob and sent nothing else within the grace window.
    KvAfterText,
}

/// What one decoded frame produced.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Effects {
    /// Fully framed client messages to write back on the same HTTP/2 stream.
    pub outbound: Vec<Vec<u8>>,
    /// OpenAI SSE bytes to hand to the client.
    pub sse: Vec<u8>,
    /// Whether the turn is over.
    pub ended: bool,
    /// When set, a composer turn may be finished if nothing else arrives by this deadline
    /// (milliseconds on the caller's monotonic clock). The caller arms a timer; any later frame
    /// cancels it by simply being processed first.
    pub kv_grace_deadline_ms: Option<u64>,
}

/// The per-turn response state machine.
///
/// One instance per HTTP/2 stream. It owns the blob store, the set of exec messages already
/// answered (Cursor retries them), and the SSE bytes' shape — all of which are per-turn, which is
/// why none of it lives in a thread-local like the Command Code translator's state.
#[derive(Debug)]
pub struct StreamState {
    model: String,
    response_id: String,
    created: u64,
    role_sent: bool,
    text_seen: bool,
    thinking_open: bool,
    ended: bool,
    end_reason: Option<EndReason>,
    usage: Option<Usage>,
    blobs: HashMap<Vec<u8>, Vec<u8>>,
    answered_exec: HashSet<String>,
    answered_queries: HashSet<u64>,
    last_text_at_ms: Option<u64>,
    tool_activity: bool,
    composer: bool,
}

impl StreamState {
    /// A state machine for one turn.
    pub fn new(model: impl Into<String>, response_id: impl Into<String>, created: u64) -> Self {
        let model = model.into();
        let composer = is_composer_model(&model);
        Self {
            model,
            response_id: response_id.into(),
            created,
            role_sent: false,
            text_seen: false,
            thinking_open: false,
            ended: false,
            end_reason: None,
            usage: None,
            blobs: HashMap::new(),
            answered_exec: HashSet::new(),
            answered_queries: HashSet::new(),
            last_text_at_ms: None,
            tool_activity: false,
            composer,
        }
    }

    /// Whether any answer text has been rendered yet.
    ///
    /// The relay uses this to decide how to report a mid-stream failure: an error before any text
    /// can still be an HTTP-level error object, while one after text must be a graceful finish so
    /// the client keeps the answer it already rendered.
    pub fn has_text(&self) -> bool {
        self.text_seen
    }

    /// Whether the turn is over.
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Why the turn ended, once it has.
    pub fn end_reason(&self) -> Option<EndReason> {
        self.end_reason
    }

    /// The usage the server reported, if the turn reached `turn_ended`.
    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Decode one frame and produce everything it implies.
    ///
    /// `at_ms` is the caller's monotonic clock in milliseconds. It is passed in rather than read
    /// here so the state machine stays testable and so the 300-second budget is measured against
    /// the same clock the relay's timer uses.
    pub fn on_frame(&mut self, frame: &Frame, at_ms: u64) -> Result<Effects, CursorError> {
        let mut effects = Effects::default();
        if self.ended {
            return Ok(effects);
        }
        if frame.trailer {
            return Ok(self.on_trailer(&frame.payload, at_ms, effects));
        }

        // 1. A JSON error body can arrive as an ordinary frame on some failure paths. It is checked
        // before protobuf decoding because the protobuf reader would either fail with a useless
        // message or, worse, find a plausible-looking field in the JSON text's bytes.
        if frame.payload.first() == Some(&b'{') {
            if let Ok(value) = serde_json::from_slice::<Value>(&frame.payload) {
                if let Some(message) = json_error_message(&value) {
                    return Ok(self.on_server_error(&message, effects));
                }
                // `{}` or unrelated JSON: not a message this port knows; ignore it.
                return Ok(effects);
            }
        }

        // 2. AgentServerMessage oneof.
        let mut reader = pb::Reader::new(&frame.payload);
        while let Some((field, value)) = reader.next_field()? {
            match (field, value) {
                // 1: InteractionUpdate
                (1, pb::Value::Bytes(body)) => {
                    self.on_interaction_update(body, at_ms, &mut effects)?
                }
                // 2: ExecServerMessage
                (2, pb::Value::Bytes(body)) => self.on_exec(body, &mut effects)?,
                // 4: KvServerMessage
                (4, pb::Value::Bytes(body)) => self.on_kv(body, at_ms, &mut effects)?,
                // 7: InteractionQuery
                (7, pb::Value::Bytes(body)) => self.on_interaction_query(body, &mut effects)?,
                // Anything else is either a new server message this port does not know (ignore it:
                // the stream continues) or a non-message field. Neither is fatal.
                _ => {}
            }
        }
        Ok(effects)
    }

    /// The end-of-stream trailer.
    ///
    /// `{}` means "clean end" — but the end is the HTTP/2 `END_STREAM` that follows, so nothing is
    /// emitted here. `{"error": {...}}` is not a clean end, and is reported the same way a
    /// mid-stream JSON error is.
    fn on_trailer(&mut self, payload: &[u8], _at_ms: u64, effects: Effects) -> Effects {
        match serde_json::from_slice::<Value>(payload) {
            Ok(value) => match json_error_message(&value) {
                Some(message) => self.on_server_error(&message, effects),
                None => effects,
            },
            // Lenient by design: a trailer that is not JSON ends the stream cleanly, exactly as the
            // reference implementation treats it.
            Err(_) => effects,
        }
    }

    /// A failure the server reported in-band, or a transport failure the relay passes through.
    fn on_server_error(&mut self, message: &str, mut effects: Effects) -> Effects {
        if self.text_seen {
            // The client already has text. Cutting it off with an error object would discard an
            // answer the user can see, so the turn is finished normally and the stream ends.
            effects.sse.extend(self.finish("stop"));
        } else {
            effects
                .sse
                .extend(self.error_chunk(message, "invalid_request_error"));
            effects.sse.extend(self.done());
        }
        effects.ended = true;
        self.ended = true;
        self.end_reason = Some(EndReason::ServerEnd);
        effects
    }

    /// Finish the turn because the HTTP/2 stream ended.
    ///
    /// This is the `server_end` end condition. A turn that already ended (via `turn_ended`) is left
    /// alone, so a clean trailer after a completed turn emits nothing.
    pub fn on_server_end(&mut self) -> Effects {
        let mut effects = Effects::default();
        if self.ended {
            return effects;
        }
        effects.sse.extend(self.finish("stop"));
        effects.ended = true;
        self.ended = true;
        self.end_reason = Some(EndReason::ServerEnd);
        effects
    }

    /// Finish a partial turn because the safety timeout expired.
    ///
    /// The answer is *not* discarded and no error object is emitted: the client receives the text it
    /// already has with `finish_reason: "length"`, which is what OpenAI uses for a truncated
    /// completion. An in-band error would tell the client to throw the partial answer away.
    pub fn on_timeout(&mut self) -> Effects {
        let mut effects = Effects::default();
        if self.ended {
            return effects;
        }
        effects.sse.extend(self.finish("length"));
        effects.ended = true;
        self.ended = true;
        self.end_reason = Some(EndReason::Timeout);
        effects
    }

    /// Finish the turn because the transport failed.
    pub fn on_transport_error(&mut self, message: &str) -> Effects {
        let mut effects = Effects::default();
        if self.ended {
            return effects;
        }
        effects
            .sse
            .extend(self.error_chunk(message, "upstream_error"));
        effects.sse.extend(self.done());
        effects.ended = true;
        self.ended = true;
        self.end_reason = Some(EndReason::ServerEnd);
        effects
    }

    /// Apply the composer grace deadline, if the caller armed one and it has passed.
    pub fn on_kv_grace_expired(&mut self) -> Effects {
        let mut effects = Effects::default();
        if self.ended {
            return effects;
        }
        effects.sse.extend(self.finish("stop"));
        effects.ended = true;
        self.ended = true;
        self.end_reason = Some(EndReason::KvAfterText);
        effects
    }

    fn on_interaction_update(
        &mut self,
        body: &[u8],
        at_ms: u64,
        effects: &mut Effects,
    ) -> Result<(), CursorError> {
        let mut reader = pb::Reader::new(body);
        while let Some((field, value)) = reader.next_field()? {
            match (field, value) {
                // 1: text_delta -> {"content": "..."}
                (1, pb::Value::Bytes(delta)) => {
                    let text = pb::message(delta, 1)?
                        .and_then(|b| std::str::from_utf8(b).ok())
                        .unwrap_or_default();
                    if !text.is_empty() {
                        self.ensure_role(effects);
                        self.text_seen = true;
                        self.last_text_at_ms = Some(at_ms);
                        effects.sse.extend(self.content_chunk(text));
                    }
                }
                // 4: thinking_delta -> {"reasoning_content": "..."}
                (4, pb::Value::Bytes(delta)) => {
                    let text = pb::message(delta, 1)?
                        .and_then(|b| std::str::from_utf8(b).ok())
                        .unwrap_or_default();
                    if !text.is_empty() {
                        self.ensure_role(effects);
                        self.thinking_open = true;
                        effects.sse.extend(self.reasoning_chunk(text));
                    }
                }
                // 5: thinking_completed — nothing to emit; the block simply stops.
                (5, _) => self.thinking_open = false,
                // 8: token_delta — an advisory running count. Deliberately ignored: it is not the
                // authoritative usage, and guessing from it would put wrong numbers in the ledger.
                (8, _) => {}
                // 13: heartbeat — ignored, but its presence is what makes a wall-clock timeout the
                // right choice instead of an idle one.
                (13, _) => {}
                // 14: turn_ended — the authoritative terminal signal, with usage.
                (14, pb::Value::Bytes(ended)) => {
                    self.usage = Some(parse_turn_usage(ended)?);
                    effects.sse.extend(self.finish("stop"));
                    effects.ended = true;
                    self.ended = true;
                    self.end_reason = Some(EndReason::TurnEnded);
                }
                // Unknown interaction fields (partial tool calls and future additions) are ignored:
                // the turn continues, and the authoritative end will still arrive.
                _ => {}
            }
        }
        Ok(())
    }

    fn on_exec(&mut self, body: &[u8], effects: &mut Effects) -> Result<(), CursorError> {
        let mut id = 0u64;
        let mut exec_id = String::new();
        let mut request_context: Option<&[u8]> = None;
        let mut mcp_state: Option<&[u8]> = None;
        let mut unknown: Option<u32> = None;

        let mut reader = pb::Reader::new(body);
        while let Some((field, value)) = reader.next_field()? {
            match (field, value) {
                (1, pb::Value::Varint(v)) => id = v,
                (15, pb::Value::Bytes(v)) => {
                    exec_id = std::str::from_utf8(v).unwrap_or_default().to_owned();
                }
                (10, pb::Value::Bytes(v)) => request_context = Some(v),
                (36, pb::Value::Bytes(v)) => mcp_state = Some(v),
                // Any other variant is a tool execution request. A text-only transport has no
                // result to give it, and silence stalls the turn: report it by field number so the
                // operator sees exactly which request could not be answered.
                (other, _) if other != 1 && other != 15 => unknown = Some(other),
                _ => {}
            }
        }

        if let Some(field) = unknown {
            return Err(CursorError::UnanswerableExec { field });
        }

        if request_context.is_some() {
            let key = format!("{id}:{exec_id}:10");
            if self.answered_exec.insert(key) {
                // RequestContextResult{success: {request_context: {}}} — an empty tool list, which
                // is the correct answer for a transport that offers no tools.
                let success = vec![0x0a, 0x00];
                let mut result = Vec::new();
                pb::put_bytes(&mut result, 1, &success);
                let mut exec = Vec::new();
                pb::put_uint(&mut exec, 1, id);
                pb::put_str(&mut exec, 15, &exec_id);
                pb::put_bytes(&mut exec, 10, &result);
                effects
                    .outbound
                    .push(encode_frame(&client_message(2, &exec)));
            }
        }

        if mcp_state.is_some() {
            let key = format!("{id}:{exec_id}:36");
            if self.answered_exec.insert(key) {
                let mut exec = Vec::new();
                pb::put_uint(&mut exec, 1, id);
                pb::put_str(&mut exec, 15, &exec_id);
                pb::put_bytes(&mut exec, 36, &[]);
                effects
                    .outbound
                    .push(encode_frame(&client_message(2, &exec)));
            }
        }
        Ok(())
    }

    fn on_kv(&mut self, body: &[u8], at_ms: u64, effects: &mut Effects) -> Result<(), CursorError> {
        let mut id = 0u64;
        let mut get: Option<&[u8]> = None;
        let mut set: Option<&[u8]> = None;
        let mut metadata: Option<&[u8]> = None;

        let mut reader = pb::Reader::new(body);
        while let Some((field, value)) = reader.next_field()? {
            match (field, value) {
                (1, pb::Value::Varint(v)) => id = v,
                (2, pb::Value::Bytes(v)) => get = Some(v),
                (3, pb::Value::Bytes(v)) => set = Some(v),
                (4, pb::Value::Bytes(v)) => metadata = Some(v),
                _ => {}
            }
        }

        if let Some(get) = get {
            let blob_id = pb::message(get, 1)?.unwrap_or_default();
            // A missing blob is an empty blob, not an error: the server asks for its own conversation
            // state and "I have nothing" is a valid answer on the first turn.
            let data = self.blobs.get(blob_id).cloned().unwrap_or_default();
            let mut result = Vec::new();
            pb::put_bytes(&mut result, 1, &data);
            let mut kv = Vec::new();
            pb::put_uint(&mut kv, 1, id);
            pb::put_bytes(&mut kv, 2, &result);
            if let Some(metadata) = metadata {
                // Echoed verbatim: it is an opaque routing token the server expects back.
                pb::put_bytes(&mut kv, 4, metadata);
            }
            effects.outbound.push(encode_frame(&client_message(3, &kv)));
        }

        if let Some(set) = set {
            let blob_id = pb::message(set, 1)?.unwrap_or_default().to_vec();
            let data = pb::message(set, 2)?.unwrap_or_default().to_vec();
            self.blobs.insert(blob_id, data);
            let mut kv = Vec::new();
            pb::put_uint(&mut kv, 1, id);
            // An empty SetBlobResult message: the ack is the whole answer.
            pb::put_bytes(&mut kv, 3, &[]);
            if let Some(metadata) = metadata {
                pb::put_bytes(&mut kv, 4, metadata);
            }
            effects.outbound.push(encode_frame(&client_message(3, &kv)));

            // The ack of a blob write is the last thing some composer models ever send. Only those
            // models, only after text, and only when no tool activity has happened: for other models
            // ending here would cut off a tool call that is still coming.
            if self.composer && self.text_seen && !self.tool_activity {
                effects.kv_grace_deadline_ms = Some(at_ms.saturating_add(KV_GRACE_MS));
            }
        }
        Ok(())
    }

    fn on_interaction_query(
        &mut self,
        body: &[u8],
        effects: &mut Effects,
    ) -> Result<(), CursorError> {
        let mut query_id = 0u64;
        let mut variant = 0u32;
        let mut reader = pb::Reader::new(body);
        while let Some((field, value)) = reader.next_field()? {
            match (field, value) {
                (1, pb::Value::Varint(v)) => query_id = v,
                // The query payload is itself a oneof; its field number is the variant.
                (other, pb::Value::Bytes(_)) => variant = other,
                _ => {}
            }
        }
        if variant == 0 || query_id == 0 {
            return Ok(());
        }
        if !self.answered_queries.insert(query_id) {
            return Ok(());
        }

        // 2/5/6/9 are the search and fetch approvals; 3/4/7 are the question, mode-switch and
        // plan-creation requests; 8 is VM setup.
        //
        // UNCERTAIN: the inner shape of every response message is not in the verified schema, so the
        // response is sent as an empty message with only the oneof variant selected. If the server
        // requires a boolean field inside (rather than treating presence as assent), it will treat
        // these as unanswered and the turn will stall. The alternative — leaving them unanswered —
        // stalls for certain, so this is the better guess, and the relay logs the query so an
        // operator sees it happening.
        // The reply reuses the query's own field number. That is what the reference implementation
        // does, and the only reading that works: `InteractionResponse` also carries `query_id` at
        // field 1, so `approved` cannot be field 1 as the schema fragment claims — the two would
        // collide on one message field. This is a real contradiction in the source material.
        //
        // UNCERTAIN: the reply field number for an approval. If the server expects a dedicated
        // approval field instead of the query's number, the approval is silently dropped and the
        // turn stalls after the tool call.
        let answer_field = match variant {
            2 | 5 | 6 | 9 => variant,
            3 | 4 | 7 => variant,
            // 8 (setupVm) has no meaning for a transport with no VM; any other variant is unknown.
            _ => return Ok(()),
        };
        let mut response = Vec::new();
        pb::put_uint(&mut response, 1, query_id);
        pb::put_bytes(&mut response, answer_field, &[]);
        effects
            .outbound
            .push(encode_frame(&client_message(6, &response)));
        Ok(())
    }

    // -- SSE rendering -------------------------------------------------------------------------

    fn ensure_role(&mut self, effects: &mut Effects) {
        if self.role_sent {
            return;
        }
        self.role_sent = true;
        // Role only, no empty `content`: that is the shape OpenAI clients are written against, and
        // an empty string here makes some of them open an empty text block.
        effects
            .sse
            .extend(self.chunk(serde_json::json!({"role": "assistant"}), None));
    }

    fn content_chunk(&self, text: &str) -> Vec<u8> {
        self.chunk(serde_json::json!({"content": text}), None)
    }

    fn reasoning_chunk(&self, text: &str) -> Vec<u8> {
        self.chunk(serde_json::json!({"reasoning_content": text}), None)
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Vec<u8> {
        let body = serde_json::json!({
            "id": self.response_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish,
            }],
        });
        sse_data(&body)
    }

    /// The finish chunk, the usage chunk (when known) and `[DONE]`.
    fn finish(&mut self, reason: &str) -> Vec<u8> {
        let mut out = self.chunk(serde_json::json!({}), Some(reason));
        if let Some(usage) = self.usage {
            // `input` may or may not already include the cached part, depending on which flavour of
            // the protocol produced the count, so the prompt is the larger of the two readings. The
            // cached part is a subset of the prompt and can never exceed it.
            let prompt = usage
                .input_tokens
                .max(usage.cache_read.saturating_add(usage.cache_write));
            let cached = usage.cache_read.min(prompt);
            let body = serde_json::json!({
                "id": self.response_id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": [],
                "usage": {
                    "prompt_tokens": prompt,
                    "completion_tokens": usage.output_tokens,
                    "total_tokens": prompt.saturating_add(usage.output_tokens),
                    "prompt_tokens_details": { "cached_tokens": cached },
                    "completion_tokens_details": { "reasoning_tokens": usage.reasoning },
                },
            });
            out.extend(sse_data(&body));
        }
        out.extend(self.done());
        out
    }

    fn error_chunk(&self, message: &str, kind: &str) -> Vec<u8> {
        // The chunk keeps its `id`/`object`/`created`/`model` identity and closes with
        // `finish_reason: "error"`, because a client that keys on `finish_reason` would otherwise
        // sit on a stream that only ever produced an `error` object. The provider's HTTP status is
        // not part of this shape: by the time a stream is open the status is long spent, and
        // inventing one would report a 200 turn as a 502.
        let body = serde_json::json!({
            "id": self.response_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "error",
            }],
            "error": {
                "message": message,
                "type": kind,
            }
        });
        sse_data(&body)
    }

    fn done(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SSE_DONE.len() + 14);
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(SSE_DONE.as_bytes());
        out.extend_from_slice(b"\n\n");
        out
    }
}

/// Wrap a JSON value as one SSE `data:` frame.
fn sse_data(value: &Value) -> Vec<u8> {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned());
    let mut out = Vec::with_capacity(text.len() + 8);
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\n\n");
    out
}

/// Wrap a payload as an `AgentClientMessage` with the given oneof field.
fn client_message(field: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 4);
    pb::put_bytes(&mut out, field, body);
    out
}

fn parse_turn_usage(body: &[u8]) -> Result<Usage, CursorError> {
    let mut usage = Usage::default();
    let mut reader = pb::Reader::new(body);
    while let Some((field, value)) = reader.next_field()? {
        if let pb::Value::Varint(v) = value {
            match field {
                1 => usage.input_tokens = v,
                2 => usage.output_tokens = v,
                3 => usage.cache_read = v,
                4 => usage.cache_write = v,
                5 => usage.reasoning = v,
                _ => {}
            }
        }
    }
    Ok(usage)
}

/// The message of a `{"error": {"message": ...}}` object, if that is what this is.
fn json_error_message(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    if let Some(message) = error.get("message").and_then(Value::as_str) {
        return Some(message.to_owned());
    }
    if let Some(text) = error.as_str() {
        return Some(text.to_owned());
    }
    Some(value.to_string())
}

/// Whether a model id belongs to the Composer family, the one that may end a turn with a blob write.
fn is_composer_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("composer")
}

// ---------------------------------------------------------------------------------------------
// Non-streaming folding
// ---------------------------------------------------------------------------------------------

/// Fold the SSE this module renders into one `chat.completion` object.
///
/// Used for a non-streaming OpenAI client: the upstream exchange always streams, so the relay
/// renders SSE and then folds it here rather than asking Cursor for a JSON answer it cannot give.
pub fn to_pivot_response(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<Bytes, TranslateError> {
    let text = std::str::from_utf8(body).map_err(|e| TranslateError::MalformedResponse {
        dialect: "cursor",
        detail: e.to_string(),
    })?;

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish_reason = "stop".to_owned();
    let mut usage: Option<Value> = None;
    let mut error: Option<Value> = None;
    let mut id = String::new();
    let mut model = ctx.upstream_model.to_owned();
    let mut created = 0u64;

    for line in text.lines() {
        let candidate = line
            .trim()
            .strip_prefix("data:")
            .unwrap_or(line.trim())
            .trim();
        if candidate.is_empty() || candidate == SSE_DONE {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(candidate) else {
            continue;
        };
        if let Some(err) = value.get("error") {
            error = Some(err.clone());
            continue;
        }
        if let Some(u) = value.get("usage").filter(|u| !u.is_null()) {
            usage = Some(u.clone());
        }
        if let Some(chunk_id) = value.get("id").and_then(Value::as_str) {
            id = chunk_id.to_owned();
        }
        if let Some(chunk_model) = value.get("model").and_then(Value::as_str) {
            model = chunk_model.to_owned();
        }
        if let Some(c) = value.get("created").and_then(Value::as_u64) {
            created = c;
        }
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            continue;
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(piece) = delta.get("content").and_then(Value::as_str) {
                content.push_str(piece);
            }
            if let Some(piece) = delta.get("reasoning_content").and_then(Value::as_str) {
                reasoning.push_str(piece);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            finish_reason = reason.to_owned();
        }
    }

    if let Some(error) = error {
        let body = serde_json::json!({ "error": error });
        return serde_json::to_vec(&body).map(Bytes::from).map_err(|e| {
            TranslateError::MalformedResponse {
                dialect: "cursor",
                detail: e.to_string(),
            }
        });
    }

    if id.is_empty() {
        id = format!("chatcmpl-{}", uuid_from_hash(&content));
    }
    let mut message = serde_json::json!({
        "role": "assistant",
        "content": content,
    });
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    let mut response = serde_json::json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
    });
    if let Some(usage) = usage {
        response["usage"] = usage;
    }
    serde_json::to_vec(&response)
        .map(Bytes::from)
        .map_err(|e| TranslateError::MalformedResponse {
            dialect: "cursor",
            detail: e.to_string(),
        })
}

/// Fold the SSE into one Anthropic Messages response.
///
/// The fold happens on the OpenAI pivot first, because the Anthropic adapter already knows how to
/// rebuild a `chat.completion` object's reasoning blocks and usage. A turn that ended in an error
/// is returned as an error instead of an empty message: the Anthropic adapter would render the
/// error envelope as a normal end-of-turn with no content, which looks like a successful empty
/// answer.
pub fn to_anthropic_response(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<Bytes, TranslateError> {
    let pivot = to_pivot_response(ctx, body)?;
    if let Ok(value) = serde_json::from_slice::<Value>(&pivot) {
        if let Some(error) = value.get("error") {
            return Err(TranslateError::MalformedResponse {
                dialect: "cursor",
                detail: json_error_message(&value).unwrap_or_else(|| error.to_string()),
            });
        }
    }
    crate::openai::to_anthropic_response(ctx, &pivot)
}

// ---------------------------------------------------------------------------------------------
// Tests
//
// Every byte here is canned: no credential, no socket, and no request to Cursor. The framing and
// protobuf tests are the ones that must hold for a live turn to work at all; the exchange itself is
// exercised in `manarouter-relay` against a canned `H2Exchange`.
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mr_core::{Dialect, RouteKind};

    fn ctx<'a>(model: &'a str, session: Option<&'a str>) -> TranslateCtx<'a> {
        TranslateCtx {
            route: RouteKind::ChatCompletions,
            client_dialect: Dialect::OpenAi,
            upstream_dialect: Dialect::Cursor,
            upstream_model: model,
            client_streaming: true,
            session_id: session,
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn varint_field(payload: &[u8], field: u32) -> Option<u64> {
        let mut reader = pb::Reader::new(payload);
        loop {
            match reader.next_field() {
                Ok(Some((f, v))) if f == field => return v.uint(),
                Ok(Some(_)) => {}
                _ => return None,
            }
        }
    }

    /// One `AgentServerMessage` with `field` set to `body`.
    fn server_message(field: u32, body: &[u8]) -> Frame {
        Frame {
            payload: client_message(field, body),
            trailer: false,
        }
    }

    fn text_delta(text: &str) -> Frame {
        let mut delta = Vec::new();
        pb::put_str(&mut delta, 1, text);
        let mut update = Vec::new();
        pb::put_bytes(&mut update, 1, &delta);
        server_message(1, &update)
    }

    fn thinking_delta(text: &str) -> Frame {
        let mut delta = Vec::new();
        pb::put_str(&mut delta, 1, text);
        let mut update = Vec::new();
        pb::put_bytes(&mut update, 4, &delta);
        server_message(1, &update)
    }

    fn turn_ended(input: u64, output: u64, cache_read: u64, reasoning: u64) -> Frame {
        let mut usage = Vec::new();
        pb::put_uint(&mut usage, 1, input);
        pb::put_uint(&mut usage, 2, output);
        pb::put_uint(&mut usage, 3, cache_read);
        pb::put_uint(&mut usage, 5, reasoning);
        let mut update = Vec::new();
        pb::put_bytes(&mut update, 14, &usage);
        server_message(1, &update)
    }

    fn sse_text(effects: &Effects) -> String {
        String::from_utf8_lossy(&effects.sse).into_owned()
    }

    // -- framing ------------------------------------------------------------------------------

    #[test]
    fn frame_round_trip_survives_being_split_at_every_boundary() {
        let frame = encode_frame(b"hello");
        assert_eq!(hex(&frame), "000000000568656c6c6f");
        for split in 0..=frame.len() {
            let mut decoder = FrameDecoder::new();
            let mut frames = decoder.push(&frame[..split]).unwrap();
            frames.extend(decoder.push(&frame[split..]).unwrap());
            assert_eq!(frames.len(), 1, "split at {split}");
            assert_eq!(frames[0].payload, b"hello");
            assert!(!frames[0].trailer);
            decoder.finish().unwrap();
        }
    }

    #[test]
    fn gzip_frames_decode_and_the_flag_is_not_confused_with_the_trailer() {
        let compressed = encode_gzip_frame(b"a body long enough to compress").unwrap();
        assert_eq!(compressed[0], FLAG_GZIP);
        let mut decoder = FrameDecoder::new();
        let frames = decoder.push(&compressed).unwrap();
        assert_eq!(frames[0].payload, b"a body long enough to compress");
        assert!(!frames[0].trailer);

        // A gzip trailer frame is both flags at once: the payload is gunzipped *and* marks the end.
        let mut trailing = encode_gzip_frame(b"{}").unwrap();
        trailing[0] |= FLAG_END_STREAM;
        let mut decoder = FrameDecoder::new();
        let frames = decoder.push(&trailing).unwrap();
        assert_eq!(frames[0].payload, b"{}");
        assert!(frames[0].trailer);
    }

    #[test]
    fn a_declared_length_above_the_cap_is_refused_and_the_buffer_dropped() {
        let mut header = vec![0u8];
        header.extend_from_slice(&((MAX_FRAME_BYTES as u32) + 1).to_be_bytes());
        let mut decoder = FrameDecoder::new();
        assert_eq!(
            decoder.push(&header),
            Err(CursorError::FrameTooLarge {
                declared: MAX_FRAME_BYTES + 1,
                cap: MAX_FRAME_BYTES,
            })
        );
        assert_eq!(decoder.buffered(), 0);
        // The decoder stays poisoned: the stream cannot be resynchronised, and a second error must
        // not replace the first one.
        assert!(decoder.push(b"xxxxx").is_err());
    }

    #[test]
    fn a_truncated_frame_at_end_of_stream_is_an_error_not_a_short_answer() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&encode_frame(b"abc")[..4]).unwrap();
        assert!(matches!(decoder.finish(), Err(CursorError::Protobuf(_))));
    }

    #[test]
    fn a_clean_trailer_is_decoded_as_json() {
        let mut frame = encode_frame(b"{}");
        frame[0] |= FLAG_END_STREAM;
        let mut decoder = FrameDecoder::new();
        let frames = decoder.push(&frame).unwrap();
        assert_eq!(frames[0].payload, b"{}");
        assert!(frames[0].trailer);
    }

    // -- protobuf -----------------------------------------------------------------------------

    #[test]
    fn the_reader_handles_every_wire_type_this_schema_uses() {
        let mut buf = Vec::new();
        pb::put_uint(&mut buf, 1, 300);
        pb::put_bytes(&mut buf, 2, b"text");
        pb::put_tag(&mut buf, 3, 1);
        buf.extend_from_slice(&[0u8; 8]);
        pb::put_tag(&mut buf, 4, 5);
        buf.extend_from_slice(&[1u8; 4]);

        let mut reader = pb::Reader::new(&buf);
        let mut seen = Vec::new();
        while let Some((field, value)) = reader.next_field().unwrap() {
            seen.push(match value {
                pb::Value::Varint(v) => format!("{field}={v}"),
                pb::Value::Bytes(b) => format!("{field}={}", String::from_utf8_lossy(b)),
                pb::Value::Fixed64(b) => format!("{field}=f64({})", b.len()),
                pb::Value::Fixed32(b) => format!("{field}=f32({})", b.len()),
            });
        }
        assert_eq!(seen, ["1=300", "2=text", "3=f64(8)", "4=f32(4)"]);
    }

    #[test]
    fn the_reader_refuses_truncation_and_groups_rather_than_panicking() {
        // Field 1, wire type 2, declares five bytes and carries one.
        let truncated = [0x0a, 0x05, 0x41];
        assert!(pb::Reader::new(&truncated).next_field().is_err());
        // A start-group tag, which proto3 removed and this schema never uses.
        let group = [0x0b];
        assert!(pb::Reader::new(&group).next_field().is_err());
        // Field number zero is not a field.
        let zero = [0x00, 0x00];
        assert!(pb::Reader::new(&zero).next_field().is_err());
    }

    // -- request ------------------------------------------------------------------------------

    #[test]
    fn the_run_request_bytes_are_exact() {
        let conversation = Conversation {
            system: None,
            text: "hi".to_owned(),
        };
        let payload = run_request("m", &conversation, "c", "i");
        assert_eq!(
            hex(&payload),
            concat!(
                "120d",       // 2: action, 13 bytes
                "0a0b",       //   1: user_message_action, 11 bytes
                "0a026869",   //     1: text "hi"
                "120169",     //     2: message_id "i"
                "1a00",       //     3: selected_context, present and empty
                "2001",       //     4: mode = 1
                "1a09",       // 3: model_details, 9 bytes
                "0a016d",     //   1: model_id "m"
                "1a016d",     //   3: display_model_id "m"
                "22016d",     //   4: display_name "m"
                "2200",       // 4: mcp_tools, present and empty
                "2a0163",     // 5: conversation_id "c"
                "4a030a016d", // 9: requested_model { model_id: "m" }
                "6000",       // 12: exclude_workspace_context = false, explicit
                "82010163",   // 16: conversation_group_id "c"
            )
        );
    }

    #[test]
    fn the_system_prompt_is_sent_as_a_sha256_digest_of_its_json_blob() {
        let conversation = Conversation {
            system: Some("be brief".to_owned()),
            text: "hi".to_owned(),
        };
        let payload = run_request("m", &conversation, "c", "i");
        let state = pb::message(&payload, 1).unwrap().unwrap();
        let digest = pb::message(state, 1).unwrap().unwrap();
        let expected = Sha256::digest(br#"{"role":"system","content":"be brief"}"#);
        assert_eq!(digest, expected.as_slice());
        // Turns are not sent: there is no conversation history in this frame.
        assert!(pb::message(state, 8).unwrap().is_none());
    }

    #[test]
    fn prepare_run_marks_every_field_the_server_requires_present() {
        let body = br#"{"model":"cursor-auto","messages":[{"role":"user","content":"hello"}]}"#;
        let run = prepare_run(&ctx("auto", Some("sess-1")), body, "msg-1").unwrap();
        assert_eq!(
            run.model, "default",
            "`auto` is spelled `default` on the wire"
        );
        let payload = &run.frame[5..];
        assert_eq!(pb::message(payload, 4), Ok(Some(&[][..])), "mcp_tools");
        assert_eq!(
            varint_field(payload, 12),
            Some(0),
            "exclude_workspace_context must be written as an explicit false"
        );
        let conversation_id = pb::message(payload, 5).unwrap().unwrap();
        let group_id = pb::message(payload, 16).unwrap().unwrap();
        assert_eq!(conversation_id, group_id);
        assert_eq!(conversation_id, run.conversation_id.as_bytes());
        assert_eq!(
            conversation_id.len(),
            36,
            "the id is a UUID, not an arbitrary string"
        );
        assert!(!run.text.is_empty());
    }

    #[test]
    fn an_anthropic_client_reaches_a_run_request_through_the_openai_shape() {
        // The agent endpoint speaks one shape only, so an Anthropic client is converted to OpenAI
        // first and then to protobuf. This asserts the two halves meet: the conversion emits what
        // `prepare_run` reads, which no amount of testing either half alone can establish.
        let ctx = TranslateCtx {
            route: RouteKind::Messages,
            client_dialect: Dialect::Anthropic,
            upstream_dialect: Dialect::Cursor,
            upstream_model: "auto",
            client_streaming: true,
            session_id: Some("session-anthropic"),
        };
        let body = br#"{"model":"claude-sonnet-4","max_tokens":64,"messages":[{"role":"user","content":"what is in this file"}]}"#;
        let translated = crate::translate_request(&ctx, body)
            .expect("translate")
            .expect("a Cursor turn always needs a body of its own");
        let run = prepare_run(&ctx, &translated.body, "message-anthropic").expect("a run");
        assert_eq!(run.text, "what is in this file");
        // `auto` is this router's spelling; the wire wants `default`.
        assert_eq!(run.model, "default");
    }

    #[test]
    fn an_openai_client_reaches_a_run_request_without_a_rewrite() {
        // The request half of the OpenAI pairing is a *non*-translation: `prepare_run` reads the
        // Chat Completions shape, which is what an OpenAI client sends. The arm that says so was
        // missing, so the relay refused every OpenAI client with "no translation from openai to
        // cursor for route chat.completions" before opening a socket. Asserting `None` rather than
        // merely "not an error" is the point: inventing a translation here would re-serialise the
        // body and lose whatever the client sent that this crate does not know about.
        let ctx = TranslateCtx {
            route: RouteKind::ChatCompletions,
            client_dialect: Dialect::OpenAi,
            upstream_dialect: Dialect::Cursor,
            upstream_model: "grok-4.7",
            client_streaming: false,
            session_id: None,
        };
        let body = br#"{"model":"grok-4.7","messages":[{"role":"user","content":"what is in this file"}]}"#;
        assert!(
            crate::translate_request(&ctx, body)
                .expect("the OpenAI pairing must not be unsupported")
                .is_none(),
            "an OpenAI chat body is already what the run frame reads"
        );
        let run = prepare_run(&ctx, body, "message-openai").expect("a run");
        assert_eq!(run.model, "grok-4.7");
        assert_eq!(run.text, "what is in this file");
    }

    #[test]
    fn the_system_prompt_reaches_the_model_in_the_user_text_as_well_as_the_digest() {
        // `conversation_state` carries a digest the server is observed not to apply, so the prompt
        // itself has to travel in the one text field the frame has. Losing it here would silently
        // drop every system prompt without any error.
        let body = br#"{"messages":[{"role":"system","content":"be terse"},{"role":"user","content":"hi"}]}"#;
        let run = prepare_run(&ctx("auto", None), body, "message-system").unwrap();
        assert_eq!(run.text, "be terse\n\nhi");
        let payload = &run.frame[5..];
        assert!(
            payload
                .windows(run.text.len())
                .any(|window| window == run.text.as_bytes()),
            "the text sent to the model has to be the text in the framed request"
        );
    }

    #[test]
    fn a_multi_turn_history_is_labelled_so_the_model_can_tell_the_turns_apart() {
        let body = br#"{"messages":[{"role":"user","content":"one"},{"role":"assistant","content":"two"},{"role":"user","content":"three"}]}"#;
        let run = prepare_run(&ctx("auto", None), body, "message-history").unwrap();
        assert_eq!(run.text, "User: one\n\nAssistant: two\n\nUser: three");
    }

    #[test]
    fn a_tool_call_in_the_history_is_rendered_with_its_arguments() {
        let body = br#"{"messages":[{"role":"user","content":"run it"},{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","function":{"name":"shell","arguments":"{\"cmd\":\"ls\"}"}}]},{"role":"tool","tool_call_id":"call_1","content":"file.txt"}]}"#;
        let run = prepare_run(&ctx("auto", None), body, "message-tool").unwrap();
        let expected = concat!(
            "User: run it\n\n",
            "Assistant called tool shell (call_1) with arguments: {\"cmd\":\"ls\"}\n\n",
            "Tool result (call_1): file.txt"
        );
        assert_eq!(run.text, expected);
    }

    #[test]
    fn tools_are_refused_with_a_message_naming_the_way_out() {
        let body = br#"{"messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f"}}]}"#;
        let err = prepare_run(&ctx("m", None), body, "msg").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("tool calling"), "{text}");
        assert!(text.contains("remove `tools`"), "{text}");
    }

    #[test]
    fn a_non_text_content_part_is_refused_rather_than_dropped() {
        let body = br#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:,"}}]}]}"#;
        let err = prepare_run(&ctx("m", None), body, "msg").unwrap_err();
        assert!(err.to_string().contains("text-only"), "{err}");
    }

    #[test]
    fn a_conversation_id_is_stable_within_a_session_and_grouped_with_itself() {
        let first = prepare_run(
            &ctx("m", Some("session-a")),
            br#"{"messages":[{"role":"user","content":"one"}]}"#,
            "msg-1",
        )
        .unwrap();
        let second = prepare_run(
            &ctx("other-model", Some("session-a")),
            br#"{"messages":[{"role":"user","content":"two"}]}"#,
            "msg-2",
        )
        .unwrap();
        assert_eq!(first.conversation_id, second.conversation_id);
        let other = prepare_run(
            &ctx("m", Some("session-b")),
            br#"{"messages":[{"role":"user","content":"one"}]}"#,
            "msg-1",
        )
        .unwrap();
        assert_ne!(first.conversation_id, other.conversation_id);
    }

    #[test]
    fn without_a_session_two_identical_prompts_do_not_share_a_conversation() {
        let body = br#"{"messages":[{"role":"user","content":"same"}]}"#;
        let first = prepare_run(&ctx("m", None), body, "msg-1").unwrap();
        let second = prepare_run(&ctx("m", None), body, "msg-2").unwrap();
        assert_ne!(
            first.conversation_id, second.conversation_id,
            "a message id is unique per turn; sharing an id would leak one caller's history into \
             another's"
        );
    }

    // -- host discovery -----------------------------------------------------------------------

    #[test]
    fn host_validation_admits_only_the_agent_allow_list() {
        assert_eq!(
            validate_origin("https://agent.api5.cursor.sh").unwrap(),
            "https://agent.api5.cursor.sh"
        );
        assert_eq!(
            validate_origin("https://AGENT.api5.cursor.sh").unwrap(),
            "https://agent.api5.cursor.sh"
        );
        assert_eq!(
            validate_origin("https://api5.cursor.sh").unwrap(),
            "https://api5.cursor.sh"
        );
        assert_eq!(
            validate_origin("https://agent.api5.cursor.sh:443/ignored/path?x=1").unwrap(),
            "https://agent.api5.cursor.sh:443"
        );
        for refused in [
            "http://agent.api5.cursor.sh",
            "https://agent.api5.cursor.sh.evil.example",
            "https://evil.example",
            "https://user:pw@agent.api5.cursor.sh",
            "https://",
            "agent.api5.cursor.sh",
        ] {
            assert!(validate_origin(refused).is_err(), "accepted {refused}");
        }
    }

    #[test]
    fn host_selection_prefers_ghost_mode_and_falls_back_when_discovery_is_empty_or_bad() {
        let urls = AgentUrls {
            agent: "https://agent.api5.cursor.sh".to_owned(),
            agentn: "https://agentn.api5.cursor.sh".to_owned(),
        };
        assert_eq!(
            urls.select(true),
            ("https://agent.api5.cursor.sh".to_owned(), true)
        );
        assert_eq!(
            urls.select(false),
            ("https://agentn.api5.cursor.sh".to_owned(), true)
        );

        let empty = AgentUrls::default();
        assert_eq!(empty.select(true).0, FALLBACK_AGENT_ORIGIN);
        assert_eq!(empty.select(false).0, FALLBACK_AGENTN_ORIGIN);
        assert!(!empty.select(true).1);

        let hostile = AgentUrls {
            agent: "https://attacker.example".to_owned(),
            agentn: String::new(),
        };
        assert_eq!(hostile.select(true).0, FALLBACK_AGENT_ORIGIN);
        assert!(!hostile.select(true).1);
    }

    #[test]
    fn the_server_config_field_27_yields_both_origins() {
        let mut config = Vec::new();
        pb::put_str(&mut config, 1, "https://agent.api5.cursor.sh");
        pb::put_str(&mut config, 2, "https://agentn.api5.cursor.sh");
        let mut response = Vec::new();
        pb::put_bytes(&mut response, 27, &config);
        let urls = parse_agent_url_config(&response).unwrap();
        assert_eq!(urls.agent, "https://agent.api5.cursor.sh");
        assert_eq!(urls.agentn, "https://agentn.api5.cursor.sh");

        // No field 27 is not an error: the caller falls back.
        assert!(parse_agent_url_config(&[]).is_none());
    }

    // -- headers ------------------------------------------------------------------------------

    fn identity() -> RequestIdentity {
        RequestIdentity {
            request_id: "0f8fad5b-d9cb-469f-a165-70867728950e".to_owned(),
            traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_owned(),
        }
    }

    #[test]
    fn the_header_list_is_exactly_what_the_reference_client_sends() {
        let headers =
            request_headers("Bearer t", true, PINNED_CLIENT_VERSION, &identity()).unwrap();
        let pairs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("authorization", "Bearer t"),
                ("content-type", "application/connect+proto"),
                ("connect-protocol-version", "1"),
                ("connect-accept-encoding", "gzip"),
                ("user-agent", "connect-es/1.6.1"),
                ("x-cursor-client-type", "cli"),
                ("x-cursor-client-version", PINNED_CLIENT_VERSION),
                ("x-ghost-mode", "true"),
                ("x-request-id", "0f8fad5b-d9cb-469f-a165-70867728950e"),
                (
                    "x-original-request-id",
                    "0f8fad5b-d9cb-469f-a165-70867728950e"
                ),
                (
                    "backend-traceparent",
                    "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                ),
                (
                    "traceparent",
                    "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                ),
            ]
        );
        // Headers the reference client deliberately does not send. Any of these would change the
        // request fingerprint, and a checksum we cannot compute would be a wrong value rather than
        // a missing one.
        for forbidden in [
            "x-cursor-checksum",
            "x-client-key",
            "x-session-id",
            "x-cursor-timezone",
            "x-amzn-trace-id",
            "host",
        ] {
            assert!(
                !pairs.iter().any(|(name, _)| *name == forbidden),
                "sent {forbidden}"
            );
        }
    }

    #[test]
    fn ghost_mode_is_the_only_switching_header_and_defaults_nowhere() {
        let off = request_headers("Bearer t", false, PINNED_CLIENT_VERSION, &identity()).unwrap();
        let ghost = off
            .iter()
            .find(|(name, _)| name == "x-ghost-mode")
            .map(|(_, value)| value.as_str());
        assert_eq!(ghost, Some("false"));
    }

    #[test]
    fn an_unshaped_client_version_falls_back_to_the_pin_rather_than_being_sent() {
        for bad in [
            "",
            "1.0.0",
            "cli-",
            "cli-2026.07.08",
            "cli-2026.07.08-",
            "cli-2026.07.08-xyz",
        ] {
            let headers = request_headers("Bearer t", true, bad, &identity()).unwrap();
            let version = headers
                .iter()
                .find(|(name, _)| name == "x-cursor-client-version")
                .map(|(_, value)| value.as_str());
            assert_eq!(version, Some(PINNED_CLIENT_VERSION), "accepted {bad}");
        }
        let good = "cli-2026.08.01-abcdef0";
        assert!(is_client_version(good));
        let headers = request_headers("Bearer t", true, good, &identity()).unwrap();
        assert!(headers
            .iter()
            .any(|(name, value)| name == "x-cursor-client-version" && value == good));
    }

    #[test]
    fn the_discovery_call_carries_unary_proto_headers_and_nothing_else() {
        let headers = discovery_headers("Bearer session-token").expect("headers");
        let pairs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("authorization", "Bearer session-token"),
                ("content-type", "application/proto"),
                ("accept", "application/proto"),
                ("user-agent", CONNECT_USER_AGENT),
            ]
        );
        // A unary protobuf call is not a Connect stream: sending the stream headers would describe
        // the request as something `GetServerConfig` is not.
        for (name, _) in &pairs {
            assert_ne!(*name, "connect-protocol-version");
            assert_ne!(*name, "x-ghost-mode");
            assert_ne!(*name, "traceparent");
        }
        // A missing credential, and one that could inject a second header, are both refused rather
        // than sent.
        assert!(discovery_headers("").is_err());
        assert!(discovery_headers("Bearer ok\r\nx-evil: 1").is_err());
    }

    #[test]
    fn a_header_identity_that_could_forge_a_header_is_refused() {
        let mut bad = identity();
        bad.request_id = "0f8fad5b-d9cb-469f-a165-70867728950E".to_owned();
        assert!(request_headers("Bearer t", true, PINNED_CLIENT_VERSION, &bad).is_err());
        let mut bad = identity();
        bad.traceparent =
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01\r\nx: y".to_owned();
        assert!(request_headers("Bearer t", true, PINNED_CLIENT_VERSION, &bad).is_err());
        assert!(
            request_headers("Bearer t\r\nx: y", true, PINNED_CLIENT_VERSION, &identity()).is_err()
        );
        assert!(request_headers("", true, PINNED_CLIENT_VERSION, &identity()).is_err());
        assert!(!is_traceparent(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00"
        ));
        assert!(!is_traceparent(
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        ));
    }

    // -- response -----------------------------------------------------------------------------

    fn state() -> StreamState {
        StreamState::new("cursor-auto", "chatcmpl-test", 1_700_000_000)
    }

    #[test]
    fn a_text_delta_opens_the_role_then_renders_content() {
        let mut state = state();
        let effects = state.on_frame(&text_delta("Hey"), 10).unwrap();
        let text = sse_text(&effects);
        assert!(text.contains(r#""role":"assistant""#), "{text}");
        assert!(text.contains(r#""content":"Hey""#), "{text}");
        assert!(
            text.contains(r#""object":"chat.completion.chunk""#),
            "{text}"
        );
        assert!(!effects.ended);
        assert!(state.has_text());
    }

    #[test]
    fn thinking_arrives_as_reasoning_content_before_the_answer() {
        let mut state = state();
        let effects = state.on_frame(&thinking_delta("hmm"), 10).unwrap();
        let text = sse_text(&effects);
        assert!(text.contains(r#""reasoning_content":"hmm""#), "{text}");
        assert!(!text.contains(r#""content":"#), "{text}");
    }

    #[test]
    fn turn_ended_closes_with_finish_reason_usage_and_done() {
        let mut state = state();
        state.on_frame(&text_delta("Hey"), 10).unwrap();
        let effects = state.on_frame(&turn_ended(10, 5, 2, 1), 20).unwrap();
        let text = sse_text(&effects);
        assert!(text.contains(r#""finish_reason":"stop""#), "{text}");
        assert!(text.contains(r#""prompt_tokens":10"#), "{text}");
        assert!(text.contains(r#""completion_tokens":5"#), "{text}");
        assert!(text.contains(r#""cached_tokens":2"#), "{text}");
        assert!(text.contains(r#""reasoning_tokens":1"#), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
        assert!(effects.ended);
        assert_eq!(state.end_reason(), Some(EndReason::TurnEnded));
        assert_eq!(state.usage().unwrap().output_tokens, 5);
    }

    #[test]
    fn a_cached_prompt_larger_than_the_reported_input_is_counted_once() {
        // A cache-aware flavour reports the cached part separately, so `input` alone would report a
        // nearly free turn. The larger reading is the prompt, and the cached part stays a subset.
        let mut state = state();
        state.on_frame(&text_delta("Hey"), 10).unwrap();
        let effects = state.on_frame(&turn_ended(0, 5, 100, 0), 20).unwrap();
        let text = sse_text(&effects);
        assert!(text.contains(r#""prompt_tokens":100"#), "{text}");
        assert!(text.contains(r#""cached_tokens":100"#), "{text}");
        assert!(text.contains(r#""total_tokens":105"#), "{text}");
    }

    #[test]
    fn the_safety_timeout_finishes_with_length_and_keeps_the_partial_answer() {
        let mut state = state();
        state.on_frame(&text_delta("partial"), 10).unwrap();
        let effects = state.on_timeout();
        let text = sse_text(&effects);
        assert!(text.contains(r#""finish_reason":"length""#), "{text}");
        assert!(
            !text.contains(r#""error""#),
            "an error would discard the answer"
        );
        assert!(text.ends_with("data: [DONE]\n\n"));
        assert_eq!(state.end_reason(), Some(EndReason::Timeout));
    }

    #[test]
    fn an_error_before_any_text_is_an_error_object_but_one_after_text_is_a_finish() {
        let mut fresh = state();
        let effects = fresh
            .on_frame(
                &Frame {
                    payload: br#"{"error":{"code":13,"message":"boom"}}"#.to_vec(),
                    trailer: false,
                },
                10,
            )
            .unwrap();
        let text = sse_text(&effects);
        assert!(text.contains(r#""message":"boom""#), "{text}");
        assert!(effects.ended);

        let mut used = state();
        used.on_frame(&text_delta("kept"), 10).unwrap();
        let effects = used
            .on_frame(
                &Frame {
                    payload: br#"{"error":{"message":"late failure"}}"#.to_vec(),
                    trailer: false,
                },
                20,
            )
            .unwrap();
        let text = sse_text(&effects);
        assert!(!text.contains("late failure"), "{text}");
        assert!(text.contains(r#""finish_reason":"stop""#), "{text}");
    }

    #[test]
    fn a_request_context_request_is_answered_with_an_empty_tool_list_once() {
        let mut args = Vec::new();
        pb::put_bytes(&mut args, 1, &[]);
        let mut exec = Vec::new();
        pb::put_uint(&mut exec, 1, 7);
        pb::put_str(&mut exec, 15, "e1");
        pb::put_bytes(&mut exec, 10, &args);
        let frame = server_message(2, &exec);

        let mut state = state();
        let effects = state.on_frame(&frame, 10).unwrap();
        assert_eq!(effects.outbound.len(), 1);
        let message = &effects.outbound[0][5..];
        let exec = pb::message(message, 2).unwrap().unwrap();
        assert_eq!(varint_field(exec, 1), Some(7));
        assert_eq!(pb::message(exec, 15).unwrap().unwrap(), b"e1");
        let result = pb::message(exec, 10).unwrap().unwrap();
        let success = pb::message(result, 1).unwrap().unwrap();
        assert_eq!(success, b"\x0a\x00");

        // The server retries these; a second identical request must not be answered twice.
        let again = state.on_frame(&frame, 11).unwrap();
        assert!(again.outbound.is_empty());
    }

    #[test]
    fn an_unanswerable_exec_names_the_field_it_could_not_answer() {
        let mut exec = Vec::new();
        pb::put_uint(&mut exec, 1, 1);
        pb::put_str(&mut exec, 15, "e1");
        pb::put_bytes(&mut exec, 5, b"read file");
        let mut state = state();
        assert_eq!(
            state.on_frame(&server_message(2, &exec), 10),
            Err(CursorError::UnanswerableExec { field: 5 })
        );
    }

    #[test]
    fn a_blob_read_for_an_unknown_blob_is_an_empty_result_with_the_metadata_echoed() {
        let mut get = Vec::new();
        pb::put_bytes(&mut get, 1, b"blob-1");
        let mut kv = Vec::new();
        pb::put_uint(&mut kv, 1, 3);
        pb::put_bytes(&mut kv, 2, &get);
        pb::put_bytes(&mut kv, 4, b"meta");

        let mut state = state();
        let effects = state.on_frame(&server_message(4, &kv), 10).unwrap();
        let message = &effects.outbound[0][5..];
        let kv = pb::message(message, 3).unwrap().unwrap();
        assert_eq!(varint_field(kv, 1), Some(3));
        let result = pb::message(kv, 2).unwrap().unwrap();
        assert_eq!(pb::message(result, 1).unwrap().unwrap(), b"");
        assert_eq!(pb::message(kv, 4).unwrap().unwrap(), b"meta");
    }

    #[test]
    fn a_blob_write_is_stored_acked_and_readable_by_the_next_turn_of_the_stream() {
        let mut set = Vec::new();
        pb::put_bytes(&mut set, 1, b"blob-1");
        pb::put_bytes(&mut set, 2, b"state-bytes");
        let mut kv = Vec::new();
        pb::put_uint(&mut kv, 1, 4);
        pb::put_bytes(&mut kv, 3, &set);

        let mut state = state();
        let effects = state.on_frame(&server_message(4, &kv), 10).unwrap();
        let message = &effects.outbound[0][5..];
        let reply = pb::message(message, 3).unwrap().unwrap();
        assert_eq!(varint_field(reply, 1), Some(4));
        assert_eq!(pb::message(reply, 3).unwrap().unwrap(), b"");

        let mut get = Vec::new();
        pb::put_bytes(&mut get, 1, b"blob-1");
        let mut kv = Vec::new();
        pb::put_uint(&mut kv, 1, 5);
        pb::put_bytes(&mut kv, 2, &get);
        let effects = state.on_frame(&server_message(4, &kv), 11).unwrap();
        let message = &effects.outbound[0][5..];
        let kv = pb::message(message, 3).unwrap().unwrap();
        let result = pb::message(kv, 2).unwrap().unwrap();
        assert_eq!(pb::message(result, 1).unwrap().unwrap(), b"state-bytes");
    }

    #[test]
    fn only_a_composer_model_arms_the_blob_write_grace_window() {
        let blob_write = || {
            let mut set = Vec::new();
            pb::put_bytes(&mut set, 1, b"b");
            pb::put_bytes(&mut set, 2, b"d");
            let mut kv = Vec::new();
            pb::put_uint(&mut kv, 1, 1);
            pb::put_bytes(&mut kv, 3, &set);
            server_message(4, &kv)
        };

        let mut composer = StreamState::new("composer-1", "id", 0);
        composer.on_frame(&text_delta("done"), 10).unwrap();
        let effects = composer.on_frame(&blob_write(), 20).unwrap();
        assert_eq!(effects.kv_grace_deadline_ms, Some(20 + KV_GRACE_MS));

        let mut other = StreamState::new("gpt-5", "id", 0);
        other.on_frame(&text_delta("done"), 10).unwrap();
        let effects = other.on_frame(&blob_write(), 20).unwrap();
        assert_eq!(effects.kv_grace_deadline_ms, None);

        // A composer that has not produced text yet is still waiting for its answer.
        let mut early = StreamState::new("composer-1", "id", 0);
        let effects = early.on_frame(&blob_write(), 20).unwrap();
        assert_eq!(effects.kv_grace_deadline_ms, None);
    }

    #[test]
    fn an_interaction_query_is_answered_once_and_unknown_variants_are_left_alone() {
        let mut query = Vec::new();
        pb::put_uint(&mut query, 1, 42);
        pb::put_bytes(&mut query, 2, b"");
        let frame = server_message(7, &query);

        let mut state = state();
        let effects = state.on_frame(&frame, 10).unwrap();
        assert_eq!(effects.outbound.len(), 1);
        let message = &effects.outbound[0][5..];
        let reply = pb::message(message, 6).unwrap().unwrap();
        assert_eq!(varint_field(reply, 1), Some(42));
        // The reply carries the query's own variant field (2, here: a web-search approval). It
        // cannot be a dedicated field 1 — that number is already `query_id`.
        assert_eq!(pb::message(reply, 2).unwrap().unwrap(), b"");
        assert!(state.on_frame(&frame, 11).unwrap().outbound.is_empty());

        // Field 8 is VM setup, which a transport with no VM cannot answer; the turn continues and
        // the authoritative end still arrives.
        let mut vm = Vec::new();
        pb::put_uint(&mut vm, 1, 7);
        pb::put_bytes(&mut vm, 8, b"");
        assert!(state
            .on_frame(&server_message(7, &vm), 12)
            .unwrap()
            .outbound
            .is_empty());
    }

    #[test]
    fn the_stream_ends_exactly_once() {
        let mut state = state();
        state.on_frame(&text_delta("Hey"), 10).unwrap();
        let first = state.on_server_end();
        assert!(first.ended);
        assert!(sse_text(&first).contains(r#""finish_reason":"stop""#));
        assert!(
            state.on_server_end().sse.is_empty(),
            "second end emits nothing"
        );
        assert!(state.on_timeout().sse.is_empty());

        let mut done = state_with_done();
        assert!(done.on_server_end().sse.is_empty());
    }

    fn state_with_done() -> StreamState {
        let mut state = state();
        state.on_frame(&text_delta("Hey"), 10).unwrap();
        state.on_frame(&turn_ended(1, 1, 0, 0), 20).unwrap();
        state
    }

    // -- non-streaming folds ------------------------------------------------------------------

    fn canned_sse() -> Vec<u8> {
        let mut state = state();
        let mut out = Vec::new();
        out.extend(state.on_frame(&text_delta("Hel"), 1).unwrap().sse);
        out.extend(state.on_frame(&text_delta("lo"), 2).unwrap().sse);
        out.extend(state.on_frame(&turn_ended(4, 2, 1, 0), 3).unwrap().sse);
        out
    }

    #[test]
    fn the_pivot_fold_rebuilds_one_chat_completion() {
        let folded = to_pivot_response(&ctx("cursor-auto", None), &canned_sse()).unwrap();
        let value: Value = serde_json::from_slice(&folded).unwrap();
        assert_eq!(value["object"], "chat.completion");
        assert_eq!(value["choices"][0]["message"]["content"], "Hello");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
        assert_eq!(value["usage"]["prompt_tokens"], 4);
        assert_eq!(value["usage"]["completion_tokens"], 2);
        assert_eq!(value["usage"]["prompt_tokens_details"]["cached_tokens"], 1);
    }

    #[test]
    fn the_anthropic_fold_produces_a_messages_response_and_refuses_to_hide_an_error() {
        let ctx = TranslateCtx {
            route: RouteKind::Messages,
            client_dialect: Dialect::Anthropic,
            upstream_dialect: Dialect::Cursor,
            upstream_model: "cursor-auto",
            client_streaming: false,
            session_id: None,
        };
        let folded = to_anthropic_response(&ctx, &canned_sse()).unwrap();
        let value: Value = serde_json::from_slice(&folded).unwrap();
        assert_eq!(value["type"], "message");
        assert_eq!(value["content"][0]["text"], "Hello");
        assert_eq!(value["stop_reason"], "end_turn");

        let mut failed = state();
        let error = failed
            .on_frame(
                &Frame {
                    payload: br#"{"error":{"message":"nope"}}"#.to_vec(),
                    trailer: false,
                },
                1,
            )
            .unwrap();
        assert!(to_anthropic_response(&ctx, &error.sse).is_err());
    }
}
