//! ZCode: the brokered CLI login the ZCode desktop app starts, and the bearer it produces.
//!
//! ## Why this is not an ordinary device-code flow
//!
//! ZCode has no user code to read aloud. `POST /api/v1/oauth/cli/init` creates a **flow**, the
//! browser completes a normal web sign-in at `zcode.z.ai`, and the CLI learns the outcome by
//! polling `/api/v1/oauth/cli/poll/<flow_id>`. It is the device-code *shape* — create, poll,
//! receive — without RFC 8628's `user_code`, which is why
//! [`DeviceAuthorization::user_code`](super::DeviceAuthorization::user_code) is empty here: the
//! human opens a URL, they never type a code. The URL is the flow's `authorize_url` **verbatim** —
//! it already carries the CLI callback the poll depends on, so nothing is appended to it.
//!
//! ## The client id is generated, not registered
//!
//! `init` authenticates the *client*, not the account: the app sends
//! `Authorization: Bearer <32 random bytes, hex>`, generated per process. There is no OAuth
//! client id or secret in this flow, so nothing here comes from `crate::published`. The same
//! bearer must accompany every poll of that flow — a poll authenticated as a different client
//! would tell us nothing, so the pair travels together in
//! [`DeviceAuthorization::device_code`] and is split again by [`split_device_code`].
//!
//! ## What the tokens are
//!
//! The ready payload carries up to three credentials, and only one of them is what a request
//! carries:
//!
//! * `data.zai.access_token` (or `data.bigmodel.access_token`) — the access token, sent as
//!   `x-api-key` on model requests ([`apply_credential`](OAuthPlatform::apply_credential)).
//! * `data.token` — a long-lived ZCode JWT. It is **not** the request credential; it is what the
//!   business-login endpoint below trades for an access token, so it rides as the *refresh*
//!   credential.
//! * `data.bigmodel.refresh_token` — present on BigModel accounts only; when it is there it is a
//!   real refresh grant and takes precedence over the JWT trade.
//!
//! `// UNCERTAIN:` the ready payload carries no expiry for the access token. [`ACCESS_TTL_SECS`]
//! assumes an hour, which is the observed lifetime of the tokens the app stores. If it is
//! shorter, requests fail with a 401 one refresh-lead early and the refresher repairs them; if
//! it is longer, we trade a still-valid JWT for a fresh token more often than necessary — cheap,
//! and the JWT is reusable (the app itself keeps it across launches).
//!
//! ## Signing is not part of the credential
//!
//! The app's `zhipu-account` requests are signed (`X-Client-Sig` and friends, see
//! `docs/providers/zcode.md`). This module produces the *account* credential only; the signature
//! layer is a documented gap in `mr-dialects::zcode`, and these tokens are sent unsigned.

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::time::Duration;

use super::flow::Http;
use super::refresh::{oauth_failure, FailureClass};
use super::{
    header, oauth_error, AccountIdentity, DeviceAuthorization, DevicePoll, OAuthPlatform,
    RedirectPorts, TokenSet,
};
use crate::error::Result;
use crate::ids::Millis;

/// The ZCode OAuth platform.
pub struct Zcode;

impl Zcode {
    /// Registry id, matching `oauth_platform` in `catalog/providers.toml`.
    pub const ID: &'static str = "zcode";
}

/// The origin every CLI-flow endpoint lives on.
const ORIGIN: &str = "https://zcode.z.ai";

/// Creates a login flow. `POST`, JSON body `{"provider":"zai"}`, client bearer.
const INIT_PATH: &str = "/api/v1/oauth/cli/init";

/// Polls a flow by id. `GET`, same client bearer.
const POLL_PATH: &str = "/api/v1/oauth/cli/poll";

/// Trades the ZCode JWT for an access token (`{"token": <jwt>}` → `data.access_token`).
const BUSINESS_LOGIN: &str = "https://api.z.ai/api/auth/z/login";

/// Which provider row of the app's runtime table this flow logs in. `bigmodel` is the other one;
/// this module implements `zai` (the international account), which is what `zcode.z.ai` serves.
const PROVIDER: &str = "zai";

/// Flow lifetime when `init` omits `expires_at`. Five minutes is the app's own window.
const DEFAULT_FLOW_SECS: u64 = 300;

/// Poll spacing when `init` omits `poll_interval_sec`. The session layer enforces a 1 s floor.
const DEFAULT_INTERVAL_SECS: u64 = 2;

/// Assumed access-token lifetime; see the module docs. Never longer than the JWT's usefulness.
const ACCESS_TTL_SECS: i64 = 3600;

/// Proactive refresh window: refresh five minutes before expiry (the app's own tokens are
/// refreshed on use, so there is no documented lead; five minutes is the conventional choice
/// for an hour-long token).
const REFRESH_LEAD_SECS: u64 = 300;

#[async_trait]
impl OAuthPlatform for Zcode {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn label(&self) -> &'static str {
        "Z Code"
    }

    fn description(&self) -> &'static str {
        "Sign in at zcode.z.ai: the app's CLI flow creates a login link, then the router polls \
         until the browser finishes."
    }

    fn begins_with_loopback(&self) -> bool {
        false
    }

    fn redirect_ports(&self) -> RedirectPorts {
        // Inert: nothing is bound locally. Callers must check `begins_with_loopback()` first.
        RedirectPorts {
            host: "",
            primary: 0,
            fallback: None,
            path: "",
        }
    }

    fn requires_pkce(&self) -> bool {
        // The flow has no redirect back to us and no code we redeem.
        false
    }

    fn validates_redirect_port(&self) -> bool {
        false
    }

    fn authorization_url(
        &self,
        _state: &str,
        _pkce: Option<&super::Pkce>,
        _port: u16,
    ) -> Result<String> {
        Err(oauth_error(format!(
            "{}: no authorize URL from a port — the login link comes from the CLI init call",
            Self::ID
        )))
    }

    async fn exchange_code(
        &self,
        _code: &str,
        _pkce: Option<&super::Pkce>,
        _port: u16,
    ) -> Result<TokenSet> {
        Err(oauth_error(format!(
            "{}: there is no authorization code to redeem; the flow is polled instead",
            Self::ID
        )))
    }

    /// Create the flow: a client bearer, `{"provider":"zai"}`, and a reply naming the browser
    /// page and how often to poll it.
    async fn device_start(&self) -> Result<DeviceAuthorization> {
        let http = Http::shared()?;
        let client = client_id();
        let reply = http
            .post_json(
                &format!("{ORIGIN}{INIT_PATH}"),
                &[
                    ("Accept", "application/json"),
                    ("Authorization", &format!("Bearer {client}")),
                ],
                init_request_body(),
            )
            .await?;
        parse_init(reply.status, &reply.body, &client)
    }

    /// One poll. A transport blip is progress-neutral ([`DevicePoll::Pending`]): the flow lives
    /// for minutes and the next tick will ask again.
    async fn device_poll(&self, authorization: &DeviceAuthorization) -> Result<DevicePoll> {
        let (flow_id, client) = split_device_code(&authorization.device_code)?;
        let http = Http::shared()?;
        let reply = match http
            .get(
                &poll_url(flow_id),
                &[
                    ("Accept", "application/json"),
                    ("Authorization", &format!("Bearer {client}")),
                ],
            )
            .await
        {
            Ok(reply) => reply,
            Err(_) => return Ok(DevicePoll::Pending),
        };
        parse_poll(reply.status, &reply.body)
    }

    /// Trade the stored credential for a fresh access token.
    ///
    /// Two grants exist and the token set says which one this account has: a BigModel
    /// `refresh_token` is a real OAuth grant, while every other account stores the ZCode JWT,
    /// which the business-login endpoint exchanges for an access token. The JWT is not
    /// single-use — the app keeps it across launches — so a failed trade does not destroy the
    /// stored credential, only this attempt.
    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet> {
        let http = Http::shared()?;
        let body = json!({ "token": refresh_token }).to_string();
        let reply = http
            .post_json(
                BUSINESS_LOGIN,
                &[("Accept", "application/json")],
                body.to_owned(),
            )
            .await
            .map_err(|error| {
                oauth_failure(
                    FailureClass::Transient,
                    Self::ID,
                    format!("the Z.ai token exchange is unreachable: {error}"),
                )
            })?;
        let class = match reply.status {
            401 | 403 => FailureClass::Permanent,
            status if (500..600).contains(&status) || status == 408 || status == 429 => {
                FailureClass::Transient
            }
            _ => FailureClass::Permanent,
        };
        match parse_business_login(reply.status, &reply.body) {
            Ok(access_token) => Ok(TokenSet {
                access_token,
                // The JWT stays the refresh credential: the trade does not rotate it.
                refresh_token: Some(refresh_token.to_owned()),
                expires_at: Millis::now().plus_secs(ACCESS_TTL_SECS),
                scopes: Vec::new(),
                token_type: "Bearer".to_string(),
                extra: serde_json::Map::new(),
            }),
            Err(detail) => Err(oauth_failure(class, Self::ID, detail)),
        }
    }

    /// Keep the identity fields across a refresh. The trade returns a token and nothing else, so
    /// without this the panel would lose the account's name every hour.
    async fn refresh_with_context(
        &self,
        refresh_token: &str,
        previous: &TokenSet,
    ) -> Result<TokenSet> {
        let mut tokens = self.refresh(refresh_token).await?;
        for key in ["user_id", "user_name", "email", "plan", "provider"] {
            if let Some(value) = previous.extra.get(key) {
                tokens.extra.insert(key.to_string(), value.clone());
            }
        }
        Ok(tokens)
    }

    /// Identity from the ready payload, with no extra request: a failed lookup here would fail
    /// the whole login *after* the human already approved it, which is the worst place to be
    /// strict. Missing fields degrade to the account id.
    async fn account_identity(&self, tokens: &TokenSet) -> Result<AccountIdentity> {
        Ok(identity_from_tokens(tokens))
    }

    fn apply_credential(&self, tokens: &TokenSet) -> Vec<(HeaderName, HeaderValue)> {
        // `x-api-key`, not `Authorization`: this is the header the Anthropic client sends for an
        // API credential, and the one `mr-dialects::zcode` documents for this surface.
        header("x-api-key", &tokens.access_token).into_iter().collect()
    }

    fn fingerprint_headers(&self) -> &'static [(&'static str, &'static str)] {
        &[("anthropic-version", "2023-06-01")]
    }

    fn refresh_lead(&self) -> Duration {
        Duration::from_secs(REFRESH_LEAD_SECS)
    }
}

/// A fresh client id: 32 random bytes, hex — the shape the app's CLI sends as its bearer.
fn client_id() -> String {
    use rand::RngCore as _;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The `init` body: exactly one field. The account is chosen in the browser, not here.
fn init_request_body() -> String {
    json!({ "provider": PROVIDER }).to_string()
}

/// `/poll/<flow_id>`, with the id validated rather than interpolated blindly: it becomes a path
/// segment, so a value carrying `/`, `?` or `..` must not reach the URL.
fn poll_url(flow_id: &str) -> String {
    format!("{ORIGIN}{POLL_PATH}/{flow_id}")
}

/// The pair travels as two lines in the opaque device code. Neither part can contain a newline:
/// the flow id is validated by [`valid_flow_id`], and the client id is hex.
fn device_code(flow_id: &str, client: &str) -> String {
    format!("{flow_id}\n{client}")
}

fn split_device_code(code: &str) -> Result<(&str, &str)> {
    match code.split_once('\n') {
        Some((flow_id, client)) if valid_flow_id(flow_id) && !client.is_empty() => {
            Ok((flow_id, client))
        }
        _ => Err(oauth_error(format!(
            "{}: the stored login flow is malformed; start the login again",
            Zcode::ID
        ))),
    }
}

/// A flow id is an opaque server-chosen token; accept only characters that cannot change the
/// meaning of the path they are placed in.
fn valid_flow_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Parse the `init` reply into the authorization the session layer polls.
///
/// Everything the app needs from the reply is checked here, once, so a poll can never run
/// against a flow that was already broken: an `authorize_url` that is not `https` (a redirect to
/// somewhere else), one without `state` (an authorize URL we cannot tie to this flow), an expiry
/// already past, or a poll interval the server would treat as abuse.
fn parse_init(status: u16, body: &str, client: &str) -> Result<DeviceAuthorization> {
    let value = envelope(status, body, "create a login flow")?;
    let data = value
        .get("data")
        .filter(|data| data.is_object())
        .ok_or_else(|| oauth_error(format!("{}: the login reply has no data", Zcode::ID)))?;
    let flow_id = string_field(data, "flow_id")?;
    if !valid_flow_id(&flow_id) {
        return Err(oauth_error(format!(
            "{}: the login flow id is not a usable identifier",
            Zcode::ID
        )));
    }
    let authorize_url = string_field(data, "authorize_url")?;
    if !authorize_url.starts_with("https://") {
        return Err(oauth_error(format!(
            "{}: the authorize URL is not https",
            Zcode::ID
        )));
    }
    if !authorize_url.contains("state=") {
        return Err(oauth_error(format!(
            "{}: the authorize URL carries no state",
            Zcode::ID
        )));
    }
    let expires_at = data
        .get("expires_at")
        .and_then(Value::as_i64)
        .ok_or_else(|| oauth_error(format!("{}: the login reply has no expiry", Zcode::ID)))?;
    let now_secs = Millis::now().as_millis() / 1000;
    if expires_at <= now_secs {
        return Err(oauth_error(format!(
            "{}: the login flow expired before it started",
            Zcode::ID
        )));
    }
    let flow_secs = (expires_at - now_secs) as u64;
    let interval_secs = data
        .get("poll_interval_sec")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_INTERVAL_SECS)
        .max(1);
    Ok(DeviceAuthorization {
        device_code: device_code(&flow_id, client),
        // No code to type: the human opens a URL. The panel shows the link, and this is empty
        // on purpose rather than filled with something that looks typeable.
        user_code: String::new(),
        verification_uri: authorize_url.clone(),
        // Nothing is appended: `authorize_url` already carries the CLI callback that the poll
        // depends on. Adding the desktop login as a second `redirect_uri` produced a URL with the
        // parameter twice — the server reads the first, the app's page is never reached, and the
        // flow would be abandoned while the panel waited.
        verification_uri_complete: None,
        expires_in: Duration::from_secs(flow_secs.min(DEFAULT_FLOW_SECS)),
        interval: Duration::from_secs(interval_secs),
    })
}

/// Classify one poll reply.
///
/// The status code decides terminal-ness before the body is read: a 4xx other than 408/429 means
/// this flow will never complete (expired, cancelled, unknown id), so the login has to start
/// again. 408 and 429 are the two the server uses to say "not now", and they are *progress*.
fn parse_poll(status: u16, body: &str) -> Result<DevicePoll> {
    if status == 408 || status == 429 {
        return Ok(DevicePoll::Pending);
    }
    if (400..500).contains(&status) {
        return Ok(DevicePoll::ExpiredToken);
    }
    let value = envelope(status, body, "poll the login flow")?;
    let data = value.get("data").filter(|data| data.is_object());
    let state = data
        .and_then(|data| data.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match state {
        "pending" => Ok(DevicePoll::Pending),
        "failed" => Ok(DevicePoll::AccessDenied),
        "ready" => match data {
            Some(data) => Ok(DevicePoll::Success(token_set_from_ready(data)?)),
            None => Err(oauth_error(format!(
                "{}: the login says ready but carries no data",
                Zcode::ID
            ))),
        },
        // A reply with no status but with the JWT is a ready reply from a server that omits the
        // field; anything else is a state we do not know, and polling again is the safe reading.
        _ => match data.and_then(|data| data.get("token")).and_then(Value::as_str) {
            Some(_) => Ok(DevicePoll::Success(token_set_from_ready(
                data.expect("checked above"),
            )?)),
            None => Ok(DevicePoll::Pending),
        },
    }
}

/// Build the token set from a ready payload.
///
/// The access token is preferred over the JWT because the JWT is not what a request carries; it
/// is kept as the refresh credential. BigModel accounts additionally supply a real refresh
/// grant, which wins over the JWT trade.
fn token_set_from_ready(data: &Value) -> Result<TokenSet> {
    let zai = data.get("zai").and_then(Value::as_object);
    let bigmodel = data.get("bigmodel").and_then(Value::as_object);
    let jwt = data.get("token").and_then(Value::as_str);
    let access = zai
        .and_then(|zai| zai.get("access_token"))
        .and_then(Value::as_str)
        .or_else(|| bigmodel.and_then(|bm| bm.get("access_token")).and_then(Value::as_str))
        // BigModel's reply is camelCase in the app's own parser; accept both spellings.
        .or_else(|| bigmodel.and_then(|bm| bm.get("accessToken")).and_then(Value::as_str))
        .or(jwt)
        .ok_or_else(|| {
            oauth_error(format!(
                "{}: the login completed but no access token came back",
                Zcode::ID
            ))
        })?;
    let refresh = bigmodel
        .and_then(|bm| bm.get("refresh_token"))
        .and_then(Value::as_str)
        .or(jwt)
        .filter(|refresh| *refresh != access)
        .map(str::to_owned);
    let mut extra = serde_json::Map::new();
    extra.insert("provider".to_string(), Value::String(PROVIDER.to_string()));
    if let Some(user) = data.get("user").and_then(Value::as_object) {
        for (from, to) in [
            ("id", "user_id"),
            ("name", "user_name"),
            ("email", "email"),
            ("plan", "plan"),
        ] {
            if let Some(value) = user.get(from).and_then(Value::as_str) {
                extra.insert(to.to_string(), Value::String(value.to_string()));
            }
        }
    }
    Ok(TokenSet {
        access_token: access.to_string(),
        refresh_token: refresh,
        expires_at: Millis::now().plus_secs(ACCESS_TTL_SECS),
        scopes: Vec::new(),
        token_type: "Bearer".to_string(),
        extra,
    })
}

/// Parse the business-login reply: `{"code":0,"data":{"access_token":"…"}}`.
fn parse_business_login(status: u16, body: &str) -> std::result::Result<String, String> {
    if !(200..300).contains(&status) {
        return Err(format!("the Z.ai token exchange answered HTTP {status}"));
    }
    let value: Value = serde_json::from_str(body)
        .map_err(|_| "the Z.ai token exchange answered with something that is not JSON".to_string())?;
    if let Some(code) = value.get("code").and_then(Value::as_i64) {
        if code != 0 {
            return Err(format!("the Z.ai token exchange refused the login (code {code})"));
        }
    }
    value
        .pointer("/data/access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "the Z.ai token exchange returned no access token".to_string())
}

/// Decode the `{code, data}` envelope every ZCode CLI endpoint answers with, turning a non-zero
/// `code` or a non-2xx status into one attributable error.
fn envelope(status: u16, body: &str, action: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(body).map_err(|_| {
        oauth_error(format!(
            "{}: {action} answered with something that is not JSON",
            Zcode::ID
        ))
    })?;
    if !(200..300).contains(&status) {
        return Err(oauth_error(format!(
            "{}: {action} answered HTTP {status}",
            Zcode::ID
        )));
    }
    match value.get("code").and_then(Value::as_i64) {
        Some(0) | None => Ok(value),
        Some(code) => {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message");
            Err(oauth_error(format!(
                "{}: {action} was refused (code {code}: {})",
                Zcode::ID,
                clip(message)
            )))
        }
    }
}

fn string_field(data: &Value, name: &str) -> Result<String> {
    data.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            oauth_error(format!(
                "{}: the login reply has no {name}",
                Zcode::ID
            ))
        })
}

/// Truncate a provider-supplied message so a hostile one cannot flood the panel.
fn clip(text: &str) -> &str {
    if text.len() <= 200 {
        return text;
    }
    let mut end = 200;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Identity from the fields the ready payload carried, falling back to the provider name.
fn identity_from_tokens(tokens: &TokenSet) -> AccountIdentity {
    let field = |key: &str| {
        tokens
            .extra
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let id = field("user_id").unwrap_or_else(|| "zcode-account".to_string());
    let label = field("user_name")
        .or_else(|| field("email"))
        .unwrap_or_else(|| format!("Z Code ({id})"));
    AccountIdentity {
        id,
        label,
        email: field("email"),
        plan: field("plan"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_payload() -> String {
        json!({
            "code": 0,
            "data": {
                "status": "ready",
                "token": "jwt-token-value",
                "user": {"id": "u-1", "name": "Ada", "email": "ada@example.com"},
                "zai": {"access_token": "access-token-value"}
            }
        })
        .to_string()
    }

    // ----- the client id and the init call --------------------------------

    #[test]
    fn the_client_id_is_the_shape_the_app_sends() {
        let id = client_id();
        assert_eq!(id.len(), 64, "32 bytes hex");
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(id, client_id(), "a fresh id per flow");
    }

    #[test]
    fn the_init_body_names_exactly_one_provider() {
        let body: Value = serde_json::from_str(&init_request_body()).unwrap();
        assert_eq!(body, json!({"provider": "zai"}));
    }

    #[test]
    fn a_good_init_reply_becomes_a_pollable_authorization() {
        let body = json!({
            "code": 0,
            "data": {
                "flow_id": "flow-abc_123",
                "authorize_url": "https://chat.z.ai/api/oauth/authorize?state=xyz",
                "expires_at": Millis::now().as_millis() / 1000 + 120,
                "poll_interval_sec": 3
            }
        })
        .to_string();
        let authorization = parse_init(200, &body, "client-1").unwrap();
        assert_eq!(authorization.device_code, "flow-abc_123\nclient-1");
        assert!(authorization.user_code.is_empty(), "no code to type");
        assert_eq!(
            authorization.verification_uri,
            "https://chat.z.ai/api/oauth/authorize?state=xyz"
        );
        // The flow's own URL is the whole instruction: it already names the CLI callback the poll
        // listens on, and a second `redirect_uri` would have been read *first* by the server.
        assert!(authorization.verification_uri_complete.is_none());
        assert_eq!(authorization.interval, Duration::from_secs(3));
        assert!(authorization.expires_in <= Duration::from_secs(DEFAULT_FLOW_SECS));
    }

    #[test]
    fn an_init_reply_that_cannot_be_polled_is_refused() {
        let base = |patch: Value| {
            let mut data = json!({
                "flow_id": "flow-1",
                "authorize_url": "https://chat.z.ai/api/oauth/authorize?state=x",
                "expires_at": Millis::now().as_millis() / 1000 + 120
            });
            for (key, value) in patch.as_object().unwrap() {
                data[key] = value.clone();
            }
            json!({"code": 0, "data": data}).to_string()
        };
        assert!(parse_init(200, &base(json!({"authorize_url": "http://chat.z.ai/x?state=y"})), "c")
            .is_err());
        assert!(parse_init(200, &base(json!({"authorize_url": "https://chat.z.ai/x"})), "c").is_err());
        assert!(parse_init(
            200,
            &base(json!({"expires_at": Millis::now().as_millis() / 1000 - 1})),
            "c"
        )
        .is_err());
        assert!(parse_init(200, &base(json!({"flow_id": "../etc/passwd"})), "c").is_err());
        assert!(parse_init(401, &base(json!({})), "c").is_err());
        assert!(parse_init(200, r#"{"code":1001,"message":"bad provider"}"#, "c").is_err());
    }

    // ----- the poll ------------------------------------------------------

    #[test]
    fn a_pending_poll_is_progress() {
        let body = json!({"code": 0, "data": {"status": "pending"}}).to_string();
        assert_eq!(parse_poll(200, &body).unwrap(), DevicePoll::Pending);
        // "not now" must not end the login, and must not be mistaken for a refusal either.
        assert_eq!(parse_poll(429, "{}").unwrap(), DevicePoll::Pending);
        assert_eq!(parse_poll(408, "{}").unwrap(), DevicePoll::Pending);
        // A reply with neither status nor token is not a state we can act on.
        assert_eq!(parse_poll(200, r#"{"code":0,"data":{}}"#).unwrap(), DevicePoll::Pending);
    }

    #[test]
    fn a_terminal_poll_ends_the_login() {
        assert_eq!(parse_poll(404, "{}").unwrap(), DevicePoll::ExpiredToken);
        assert_eq!(parse_poll(401, "{}").unwrap(), DevicePoll::ExpiredToken);
        let failed = json!({"code": 0, "data": {"status": "failed"}}).to_string();
        assert_eq!(parse_poll(200, &failed).unwrap(), DevicePoll::AccessDenied);
    }

    #[test]
    fn a_ready_poll_carries_the_access_token_and_keeps_the_jwt_for_refresh() {
        let outcome = parse_poll(200, &ready_payload()).unwrap();
        let DevicePoll::Success(tokens) = outcome else {
            panic!("expected success, got {outcome:?}");
        };
        assert_eq!(tokens.access_token, "access-token-value");
        assert_eq!(tokens.refresh_token.as_deref(), Some("jwt-token-value"));
        assert_eq!(tokens.extra["user_id"], "u-1");
        let identity = identity_from_tokens(&tokens);
        assert_eq!(identity.id, "u-1");
        assert_eq!(identity.label, "Ada");
        assert_eq!(identity.email.as_deref(), Some("ada@example.com"));
    }

    #[test]
    fn a_ready_poll_without_an_access_token_falls_back_to_the_jwt() {
        let body = json!({"code": 0, "data": {"token": "jwt-only"}}).to_string();
        let DevicePoll::Success(tokens) = parse_poll(200, &body).unwrap() else {
            panic!("expected success");
        };
        assert_eq!(tokens.access_token, "jwt-only");
        // The same value must not be both credentials: there is nothing to refresh with.
        assert_eq!(tokens.refresh_token, None);
    }

    #[test]
    fn a_bigmodel_account_uses_its_real_refresh_grant() {
        let body = json!({
            "code": 0,
            "data": {
                "status": "ready",
                "token": "jwt",
                "bigmodel": {"accessToken": "bm-access", "refresh_token": "bm-refresh"}
            }
        })
        .to_string();
        let DevicePoll::Success(tokens) = parse_poll(200, &body).unwrap() else {
            panic!("expected success");
        };
        assert_eq!(tokens.access_token, "bm-access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("bm-refresh"));
    }

    #[test]
    fn a_ready_poll_with_no_credential_at_all_is_an_error_not_a_login() {
        let body = json!({"code": 0, "data": {"status": "ready"}}).to_string();
        assert!(parse_poll(200, &body).is_err());
    }

    // ----- the stored device code ---------------------------------------

    #[test]
    fn the_device_code_round_trips_and_refuses_a_malformed_one() {
        let code = device_code("flow-1", "client-1");
        assert_eq!(split_device_code(&code).unwrap(), ("flow-1", "client-1"));
        assert!(split_device_code("flow-1").is_err(), "no client");
        assert!(split_device_code("\nclient-1").is_err(), "no flow");
        assert!(split_device_code("flow-1\n").is_err(), "no client");
        assert!(split_device_code("../x\nclient").is_err(), "path traversal");
        assert!(split_device_code(&format!("{}\nclient", "f".repeat(129))).is_err());
    }

    #[test]
    fn the_poll_url_is_built_from_a_validated_id() {
        assert_eq!(
            poll_url("flow-1"),
            "https://zcode.z.ai/api/v1/oauth/cli/poll/flow-1"
        );
    }

    // ----- the credential and its refresh --------------------------------

    #[test]
    fn the_credential_is_the_x_api_key_header_the_surface_expects() {
        let tokens = TokenSet::new("access-token-value");
        let headers = Zcode.apply_credential(&tokens);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "x-api-key");
        assert_eq!(headers[0].1, "access-token-value");
        // The token must not also ride as a bearer: one credential, one header.
        assert!(!headers.iter().any(|(name, _)| name == "authorization"));
    }

    #[test]
    fn the_business_login_reply_is_read_and_its_failures_are_classified() {
        assert_eq!(
            parse_business_login(200, r#"{"code":0,"data":{"access_token":"a"}}"#).unwrap(),
            "a"
        );
        assert!(parse_business_login(200, r#"{"code":1001,"message":"nope"}"#).is_err());
        assert!(parse_business_login(500, "{}").is_err());
        assert!(parse_business_login(200, "not json").is_err());
        assert!(parse_business_login(200, r#"{"code":0,"data":{}}"#).is_err());
    }

    #[test]
    fn a_refresh_keeps_the_identity_the_trade_does_not_return() {
        // The trade returns a token and nothing else; `refresh_with_context` restores the labels.
        let mut previous = TokenSet::new("old");
        previous.extra.insert("user_id".into(), json!("u-1"));
        previous.extra.insert("user_name".into(), json!("Ada"));
        previous.extra.insert("plan".into(), json!("individual-coding-plan"));
        let mut next = TokenSet::new("new");
        for key in ["user_id", "user_name", "plan"] {
            next.extra.insert(key.to_string(), previous.extra[key].clone());
        }
        let identity = identity_from_tokens(&next);
        assert_eq!(identity.label, "Ada");
        assert_eq!(identity.plan.as_deref(), Some("individual-coding-plan"));
    }

    #[test]
    fn the_fingerprint_is_the_anthropic_version_header() {
        assert_eq!(
            Zcode.fingerprint_headers(),
            &[("anthropic-version", "2023-06-01")]
        );
        assert_eq!(Zcode.id(), "zcode");
        assert!(!Zcode.begins_with_loopback());
        assert!(Zcode.supports_device_flow());
        assert!(!Zcode.requires_pkce());
    }

    #[test]
    fn the_flow_has_no_redirect_to_bind() {
        let authorization: DeviceAuthorization = DeviceAuthorization {
            device_code: device_code("flow-1", "client-1"),
            user_code: String::new(),
            verification_uri: "https://chat.z.ai/api/oauth/authorize?state=x".into(),
            verification_uri_complete: None,
            expires_in: Duration::from_secs(60),
            interval: Duration::from_secs(1),
        };
        // The debug line must never print the client bearer.
        let debug = format!("{authorization:?}");
        assert!(!debug.contains("client-1"), "{debug}");
    }
}
