//! The ZCode desktop app's plan surface, and the headers that make a request look like it.
//!
//! ZCode ("Agentic Development Environment", [`APP_VERSION`]) is an Electron app around a bundled
//! agent harness. Its model traffic is ordinary **Anthropic Messages**: the app builds the body with
//! the AI SDK's Anthropic client, so `thinking`, cache markers, `tool_use` blocks and the SSE event
//! names are the ones [`crate::anthropic`] already writes. That is why there is no body translator
//! here — a request from an Anthropic client is forwarded verbatim, and a request from an OpenAI
//! client goes through `anthropic::from_openai_request` exactly as it would for
//! [`mr_core::Dialect::Anthropic`].
//!
//! What is ZCode's own is the *address* and a thin layer of per-request headers:
//!
//! * the base is the plan endpoint, `https://zcode.z.ai/api/v1/zcode-plan/anthropic`, and the
//!   request lands on `…/anthropic/v1/messages` (the app appends `/v1` when its configured base
//!   URL lacks it, then the client appends `/messages`);
//! * `x-request-id`, `x-zcode-trace-id`, `x-session-id` and `x-zcode-session-type` are attached to
//!   every model request by the app's attribution layer ([`REQUEST_ID_HEADER`] and friends). They
//!   are correlation ids, not credentials — but `x-session-id` is the one the signing layer refuses
//!   to work without, so a request that omits it is a request the app would never send.
//!
//! ## Headers are not here
//!
//! [`crate::TranslatedRequest`] carries a body content type, a forced-stream flag and a path — not
//! headers. The relay must add the credential (`x-api-key`, as the AI SDK puts it there),
//! [`ANTHROPIC_VERSION_HEADER`] ([`ANTHROPIC_VERSION`]) and the attribution headers itself; the
//! constants are exposed for exactly that purpose.
//!
//! The app also sends a block of *identity* headers — `User-Agent: ZCode/3.18.2`,
//! `X-ZCode-App-Version`, `X-Title: Z Code@electron`, `X-Platform`, `X-Os-Category`,
//! `X-Client-Language`, `X-Client-Timezone`, `HTTP-Referer` — but, read off the harness, those ride
//! its **configuration** requests (`GET /api/v1/agent/configs`), not its model requests. The relay
//! therefore does not invent them; `catalog/providers.toml` sets a `user-agent` for the shipped
//! `zcode` provider because naming the client is what the whole dialect is for, and an operator can
//! change or drop it.
//!
//! ## Signing is not implemented
//!
//! The app signs model requests (`X-Client-Ts`, `X-Client-Version`, `X-Client-Sig`, `X-Client-Nonce`,
//! `X-Client-Pow`, `X-App-Id`) after a handshake at `/api/paas/c1f3a7e2/v2/client`, keyed by an API
//! key or project access token. This crate does not reproduce it. Two facts make that a defensible
//! gap rather than a broken adapter: the app itself falls back to an **unsigned** send whenever the
//! signing path fails (`bypass_entered`, `origin_mismatch`, and a retry-once-then-unsigned rule for
//! `VERIFY_APIKEY_EXPIRED` / `VERIFY_SIGNATURE_INVALID` / `3007`), and signing is switched off
//! entirely for the account modes this project's OAuth platform targets (`start-plan`, `off-peak`,
//! `highspeed` send no signature). `docs/providers/zcode.md` records the protocol for whoever wants
//! to close the gap.

/// The ZCode release whose wire behaviour these constants record.
///
/// The app pins its own version into `User-Agent` and `X-ZCode-App-Version` and sends it as
/// `app_version` on the login URL. The plan endpoint accepts other versions; the value is here so
/// the catalog's `user-agent` and the documentation cannot drift apart.
pub const APP_VERSION: &str = "3.18.2";

/// `anthropic-version` header. The AI SDK's Anthropic client always sends it, whatever the base URL,
/// so a request without it is not a request the app could have made.
pub const ANTHROPIC_VERSION_HEADER: &str = "anthropic-version";

/// The value of [`ANTHROPIC_VERSION_HEADER`]: the only version the app is built against.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Per-request correlation id. One fresh value per attempt, alongside [`TRACE_ID_HEADER`].
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Trace id shared by the requests that belong to one turn of the agent loop.
pub const TRACE_ID_HEADER: &str = "x-zcode-trace-id";

/// Conversation id. The signing layer requires it, and the app derives it from the session — with a
/// leading `sess_` or `subagent_agent_` prefix stripped, since neither is part of the id the endpoint
/// sees.
pub const SESSION_ID_HEADER: &str = "x-session-id";

/// What kind of turn the request is: [`SESSION_TYPE_MAIN`], `other`, `side_chat` or `subagent`.
pub const SESSION_TYPE_HEADER: &str = "x-zcode-session-type";

/// A user's own turn in the main conversation. A relay request is a turn somebody asked for, so this
/// is the truthful classification — `subagent` and `side_chat` would describe requests the app makes
/// on its own behalf.
pub const SESSION_TYPE_MAIN: &str = "main";

/// The path the app's Anthropic client posts to, relative to the plan base URL.
///
/// The base in `catalog/providers.toml` stops before the version segment, so the relay's URL builder
/// is what turns this into `…/zcode-plan/anthropic/v1/messages`. Exposed for tests and docs.
pub const MESSAGES_PATH: &str = "v1/messages";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_the_recorded_app() {
        assert_eq!(APP_VERSION, "3.18.2");
        assert_eq!(ANTHROPIC_VERSION, "2023-06-01");
        assert_eq!(MESSAGES_PATH, "v1/messages");
        assert_eq!(SESSION_TYPE_MAIN, "main");
    }
}
