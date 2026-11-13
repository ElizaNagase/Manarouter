//! Cursor: the "deep control" login, and the refresh grant that follows it.
//!
//! Ground truth: OmniRoute's `src/lib/oauth/services/cursorLogin.ts` and
//! `open-sse/services/tokenRefresh/providers/cursor.ts` (endpoints in [`crate::published::cursor`]).
//! Cursor publishes no OAuth documentation, so every fact here is reverse-engineered from a
//! working client, and the ones that were guessed carry an `UNCERTAIN` note.
//!
//! Why this is neither the loopback flow nor the device flow: the human signs in on cursor.com and
//! the page finishes the exchange server-side. Nothing is posted back to this router, and there is
//! no short code to retype — completion is *discovered* by polling `/auth/poll` with the same PKCE
//! verifier whose challenge the browser URL carried. So the verifier is simultaneously the login's
//! secret and its ticket, which is why it travels inside [`ScreenLogin`] rather than in the URL.
//!
//! Shape of the credential: the poll hands back an access token (a JWT) and an opaque refresh
//! token, and the refresh grant rotates both. Only one refresh for a credential may ever be in
//! flight — [`Refresher`](super::refresh::Refresher) guarantees that — because a rotated refresh
//! token that loses a race is a dead login.

use std::time::Duration;

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use rand::Rng as _;
use serde_json::Value;
use uuid::Uuid;

use super::flow::{Http, OAuthHttp};
use super::refresh::{endpoint_failure, oauth_failure, FailureClass};
use super::{
    header, jwt_payload, oauth_error, AccountIdentity, DevicePoll, OAuthPlatform, Pkce,
    RedirectPorts, Result, ScreenLogin, TokenSet,
};
use crate::error::Error;
use crate::ids::Millis;
use crate::published::cursor;

/// The Cursor platform.
pub struct Cursor;

impl Cursor {
    /// Registry id, matching `oauth_platform` in `catalog/providers.toml`.
    pub const ID: &'static str = "cursor";
}

/// Attempts per refresh (`RETRY_ATTEMPTS` in OmniRoute's `providers/cursor.ts`).
const REFRESH_ATTEMPTS: u32 = 3;

/// Backoff base: `300 ms * 2^attempt * (0.8 + rand() * 0.4)`.
const REFRESH_BACKOFF_BASE: Duration = Duration::from_millis(300);

/// Statuses worth another attempt. Every other status is either success or a verdict, and retrying
/// a verdict only delays the message the operator needs.
const RETRYABLE: [u16; 5] = [429, 500, 502, 503, 504];

/// Refresh this long before expiry — the same five-minute skew OmniRoute derives the stored expiry
/// with, so the token is already inside the refresh window the moment it arrives.
const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;

/// Expiry to assume when the access token has no decodable `exp`. An unknown expiry must read as
/// "refresh soon"; an hour is the observed lifetime, never "never".
const FALLBACK_TTL_MS: i64 = 60 * 60 * 1000;

/// The CLI build the chat transport impersonates. Here because the catalogue's fingerprint headers
/// must stay byte-identical to it: a mismatch between what we store and what we send is what makes
/// a client-shaped endpoint start rejecting requests.
const CLIENT_VERSION: &str = "cli-2026.07.08-0c04a8a";

/// Cursor's own page plus the query the editor opens: challenge, session id, mode, target.
///
/// The order is `URLSearchParams`' insertion order in the reference client, and it is reproduced
/// rather than approximated because a provider that fingerprints its login URL would notice a
/// reshuffle. Both values are URL-safe by construction (base64url and a UUID v4), so the plain
/// formatting is byte-identical to the encoded form.
fn login_url(session: &str, challenge: &str) -> String {
    format!(
        "{}?challenge={challenge}&uuid={session}&mode=login&redirectTarget=cli",
        cursor::LOGIN
    )
}

/// The poll query: the session id (public) and the PKCE verifier (secret).
fn poll_url(login: &ScreenLogin) -> String {
    format!(
        "{}?uuid={}&verifier={}",
        cursor::POLL,
        login.session(),
        login.ticket()
    )
}

/// What one poll told us.
///
/// A three-way answer rather than a `Result`, because "the human has not finished yet" is the
/// normal state of a poll and must never surface as an error: the panel would report a failure
/// every time somebody is still typing their password.
enum Polled {
    /// `404` — the documented "not finished".
    Pending,
    /// `2xx` with both tokens.
    Complete { access: String, refresh: String },
    /// The provider answered, and the answer was no.
    Failed { message: String },
}

/// Classify one poll reply. Pure, so the interesting cases are testable without a socket.
fn classify_poll(status: u16, body: &str) -> Polled {
    // The only status that means "keep asking". Cursor answers 404 until the page finishes.
    if status == 404 {
        return Polled::Pending;
    }
    if !(200..300).contains(&status) {
        // The status is the diagnosis; the body is not echoed, because an error body can quote the
        // request back and the request carried the verifier.
        return Polled::Failed {
            message: format!("Cursor auth poll failed: HTTP {status}"),
        };
    }
    match token_pair(body) {
        Ok((access, refresh)) => Polled::Complete { access, refresh },
        Err(error) => Polled::Failed {
            message: format!("Cursor auth response missing tokens ({error})"),
        },
    }
}

/// The `accessToken`/`refreshToken` pair a poll must return.
fn token_pair(body: &str) -> Result<(String, String)> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| oauth_error(format!("Cursor auth response is not JSON: {e}")))?;
    let access = value
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| oauth_error("accessToken missing"))?;
    let refresh = value
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| oauth_error("refreshToken missing"))?;
    Ok((access.to_string(), refresh.to_string()))
}

/// The pair a refresh returns. `refreshToken` may be absent there, and absent means "keep the one
/// you have" — dropping it would sign the account out on the first refresh that omits it.
fn refreshed_pair(body: &str, previous_refresh: &str) -> Result<(String, String)> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| oauth_error(format!("Cursor refresh response is not JSON: {e}")))?;
    let access = value
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| oauth_error("accessToken missing from refresh response"))?;
    let refresh = value
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .unwrap_or(previous_refresh);
    Ok((access.to_string(), refresh.to_string()))
}

/// Absolute expiry from the access token's own `exp`, minus the skew.
///
/// An already-expired `exp` yields a past instant on purpose: the token really is dead, and the
/// refresher should treat it that way rather than be handed a comfortable-looking future.
fn expiry_of(access_token: &str, now: Millis) -> Millis {
    let exp_secs = jwt_payload(access_token).ok().and_then(|claims| {
        claims.get("exp").and_then(|exp| {
            exp.as_i64()
                .or_else(|| exp.as_f64().map(|secs| secs as i64))
        })
    });
    match exp_secs {
        Some(secs) => Millis(secs.saturating_mul(1000).saturating_sub(EXPIRY_SKEW_MS)),
        None => Millis(now.as_millis().saturating_add(FALLBACK_TTL_MS)),
    }
}

/// A finished token set, with the expiry derived once, here.
fn token_set(access: String, refresh: String, now: Millis) -> TokenSet {
    let expires_at = expiry_of(&access, now);
    let mut set = TokenSet::new(access);
    set.refresh_token = Some(refresh);
    set.expires_at = expires_at;
    set
}

/// Who logged in, from the JWT Cursor hands back.
///
/// `sub` is the account id; `email` is the only human-readable name the token carries. A token
/// with neither is not usable as an account — the panel would have two rows it cannot tell apart —
/// so that is an error rather than a guess.
fn identity_of(access_token: &str) -> Result<AccountIdentity> {
    let claims = jwt_payload(access_token)?;
    let email = claims
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .filter(|email| !email.is_empty());
    let id = claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|sub| !sub.is_empty())
        .or_else(|| email.clone())
        .ok_or_else(|| {
            oauth_error("Cursor access token carries neither `sub` nor `email`; log in again")
        })?;
    Ok(AccountIdentity {
        label: email.clone().unwrap_or_else(|| id.clone()),
        id,
        email,
        // The deep-control token response reports no plan, and Cursor's plan lives behind another
        // call; claiming one here would be a guess in the panel.
        plan: None,
    })
}

/// Jittered exponential backoff for one retry.
///
/// The jitter is not decoration: every rejected refresh in a fleet retries on the same schedule,
/// and a synchronised retry storm against Cursor's API is a self-inflicted outage.
fn backoff(attempt: u32, base: Duration) -> Duration {
    let factor = 0.8 + rand::thread_rng().gen::<f64>() * 0.4;
    let scaled = base.as_secs_f64() * 2f64.powi(attempt as i32) * factor;
    Duration::from_secs_f64(scaled)
}

/// One poll, on an injected transport so the classification is testable.
async fn poll_once(http: &dyn OAuthHttp, login: &ScreenLogin) -> Result<DevicePoll> {
    let reply = match http
        .get(&poll_url(login), &[("Accept", "application/json")])
        .await
    {
        Ok(reply) => reply,
        // A transport blip is not a verdict. The session lives fifteen minutes and the human may
        // well be signing in right now, so keep polling instead of failing the login on one
        // dropped connection.
        Err(_) => return Ok(DevicePoll::Pending),
    };
    Ok(match classify_poll(reply.status, &reply.body) {
        Polled::Pending => DevicePoll::Pending,
        Polled::Complete { access, refresh } => {
            DevicePoll::Success(token_set(access, refresh, Millis::now()))
        }
        Polled::Failed { message } => DevicePoll::Failed { message },
    })
}

/// Refresh with Cursor's retry policy: three attempts, jittered exponential backoff, and retries
/// only on the statuses that mean "the server is busy" rather than "the credential is dead".
///
/// `base` is a parameter so tests can drive the same code with a zero delay; the public path
/// passes [`REFRESH_BACKOFF_BASE`].
async fn refresh_with(
    http: &dyn OAuthHttp,
    refresh_token: &str,
    base: Duration,
) -> Result<TokenSet> {
    let authorization = format!("Bearer {refresh_token}");
    let mut last: Option<Error> = None;
    for attempt in 0..REFRESH_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(backoff(attempt - 1, base)).await;
        }
        let reply = http
            .post_json(
                cursor::REFRESH,
                &[
                    ("Authorization", authorization.as_str()),
                    ("Accept", "application/json"),
                ],
                "{}".to_string(),
            )
            .await;
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                last = Some(oauth_failure(
                    FailureClass::Transient,
                    Cursor::ID,
                    format!("refresh attempt {} failed: {error}", attempt + 1),
                ));
                continue;
            }
        };
        match reply.status {
            200..=299 => {
                let (access, refresh) = refreshed_pair(&reply.body, refresh_token)?;
                return Ok(token_set(access, refresh, Millis::now()));
            }
            // The credential itself was refused. No amount of retrying fixes that; the operator
            // has to log in again, and saying so immediately is the point.
            401 | 403 => {
                return Err(oauth_failure(
                    FailureClass::Permanent,
                    Cursor::ID,
                    format!("refresh rejected with HTTP {}", reply.status),
                ))
            }
            status if RETRYABLE.contains(&status) => {
                last = Some(oauth_failure(
                    FailureClass::Transient,
                    Cursor::ID,
                    format!(
                        "refresh attempt {}/{REFRESH_ATTEMPTS} got HTTP {status}",
                        attempt + 1
                    ),
                ));
            }
            status => {
                return Err(match endpoint_failure(status, &reply.body) {
                    Some((class, detail)) => oauth_failure(class, Cursor::ID, detail),
                    None => oauth_failure(
                        FailureClass::Transient,
                        Cursor::ID,
                        format!("refresh got HTTP {status}"),
                    ),
                });
            }
        }
    }
    Err(last
        .unwrap_or_else(|| oauth_failure(FailureClass::Transient, Cursor::ID, "refresh gave up")))
}

#[async_trait]
impl OAuthPlatform for Cursor {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn label(&self) -> &'static str {
        "Cursor"
    }

    fn description(&self) -> &'static str {
        "Sign in on cursor.com; the CLI polls until that page reports the login is done."
    }

    fn begins_with_loopback(&self) -> bool {
        // Nothing ever connects back to this router: the browser talks to cursor.com only.
        false
    }

    fn begins_with_screen_login(&self) -> bool {
        true
    }

    fn redirect_ports(&self) -> RedirectPorts {
        // Inert: there is no redirect URI. Callers check `begins_with_screen_login()` first.
        RedirectPorts {
            host: "",
            primary: 0,
            fallback: None,
            path: "",
        }
    }

    fn requires_pkce(&self) -> bool {
        // PKCE here does not protect a redirect; it binds the poll to the challenge the browser was
        // opened with, so an observed poll URL cannot be replayed into a login by someone else.
        true
    }

    fn validates_redirect_port(&self) -> bool {
        false
    }

    fn authorization_url(&self, _state: &str, _pkce: Option<&Pkce>, _port: u16) -> Result<String> {
        Err(oauth_error(format!(
            "{}: no authorize URL — the browser-completion flow builds its own at {}",
            Self::ID,
            cursor::LOGIN
        )))
    }

    async fn exchange_code(
        &self,
        _code: &str,
        _pkce: Option<&Pkce>,
        _port: u16,
    ) -> Result<TokenSet> {
        Err(oauth_error(format!(
            "{}: there is no authorization code; the poll at {} delivers the tokens",
            Self::ID,
            cursor::POLL
        )))
    }

    /// Build the deep-control URL. The challenge goes into the URL; the verifier stays in the
    /// returned [`ScreenLogin`] and is only ever sent to Cursor's own poll endpoint.
    async fn screen_login_start(&self) -> Result<ScreenLogin> {
        let pkce = Pkce::generate();
        let session = Uuid::new_v4().to_string();
        Ok(ScreenLogin::new(
            login_url(&session, pkce.challenge()),
            session,
            pkce.verifier(),
        ))
    }

    async fn screen_login_poll(&self, login: &ScreenLogin) -> Result<DevicePoll> {
        let http = Http::shared()?;
        poll_once(http, login).await
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet> {
        let http = Http::shared()?;
        refresh_with(http, refresh_token, REFRESH_BACKOFF_BASE).await
    }

    async fn account_identity(&self, tokens: &TokenSet) -> Result<AccountIdentity> {
        identity_of(&tokens.access_token)
    }

    fn apply_credential(&self, tokens: &TokenSet) -> Vec<(HeaderName, HeaderValue)> {
        let mut out = Vec::new();
        if let Some(h) = header("authorization", &format!("Bearer {}", tokens.access_token)) {
            out.push(h);
        }
        out
    }

    fn fingerprint_headers(&self) -> &'static [(&'static str, &'static str)] {
        // Byte-identical to the catalogue's `[providers.headers]`; a golden test holds them together.
        //
        // These four are the *stable* half of the CLI's header set. The volatile half —
        // `x-request-id`, `x-original-request-id`, `backend-traceparent`/`traceparent` — is built
        // per request by the transport, because a replayed request id is a fingerprint the provider
        // can correlate.
        //
        // UNCERTAIN: `x-cursor-client-type: cli` is what the newer `agent.v1` path sends, while the
        // older `api2` model-list path uses `ide` with a checksum this build does not compute. If a
        // request is rejected here, the first thing to try is the other client type — the two paths
        // want different headers and nothing documents which accepts which.
        &[
            ("connect-protocol-version", "1"),
            ("user-agent", "connect-es/1.6.1"),
            ("x-cursor-client-type", "cli"),
            ("x-cursor-client-version", CLIENT_VERSION),
        ]
    }

    fn refresh_lead(&self) -> Duration {
        // The observed lifetime is an hour and Cursor's own client refreshes minutes early; 300 s
        // matches the other short-session platforms here and leaves room for one retry cycle.
        Duration::from_secs(300)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::flow::HttpReply;
    use std::sync::Mutex;

    /// A transport that replays a script and records what was asked of it.
    struct FakeHttp {
        replies: Mutex<Vec<std::result::Result<HttpReply, String>>>,
        urls: Mutex<Vec<String>>,
        bodies: Mutex<Vec<String>>,
    }

    impl FakeHttp {
        fn new(replies: Vec<std::result::Result<HttpReply, String>>) -> Self {
            Self {
                replies: Mutex::new(replies),
                urls: Mutex::new(Vec::new()),
                bodies: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.urls.lock().map(|urls| urls.len()).unwrap_or(0)
        }
    }

    #[async_trait]
    impl OAuthHttp for FakeHttp {
        async fn post_form(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
            _pairs: &[(&str, &str)],
        ) -> Result<HttpReply> {
            Err(oauth_error("the Cursor flow never posts a form"))
        }

        async fn post_json(
            &self,
            url: &str,
            _headers: &[(&str, &str)],
            body: String,
        ) -> Result<HttpReply> {
            if let Ok(mut urls) = self.urls.lock() {
                urls.push(url.to_string());
            }
            if let Ok(mut bodies) = self.bodies.lock() {
                bodies.push(body);
            }
            match self.replies.lock().ok().and_then(|mut queue| {
                if queue.is_empty() {
                    None
                } else {
                    Some(queue.remove(0))
                }
            }) {
                Some(Ok(reply)) => Ok(reply),
                Some(Err(message)) => Err(oauth_error(message)),
                None => Err(oauth_error("script exhausted")),
            }
        }

        async fn get(&self, url: &str, _headers: &[(&str, &str)]) -> Result<HttpReply> {
            if let Ok(mut urls) = self.urls.lock() {
                urls.push(url.to_string());
            }
            match self.replies.lock().ok().and_then(|mut queue| {
                if queue.is_empty() {
                    None
                } else {
                    Some(queue.remove(0))
                }
            }) {
                Some(Ok(reply)) => Ok(reply),
                Some(Err(message)) => Err(oauth_error(message)),
                None => Err(oauth_error("script exhausted")),
            }
        }
    }

    fn reply(status: u16, body: &str) -> HttpReply {
        HttpReply {
            status,
            body: body.to_string(),
            content_type: Some("application/json".to_string()),
        }
    }

    fn ok(body: &str) -> std::result::Result<HttpReply, String> {
        Ok(reply(200, body))
    }

    fn failed(status: u16) -> std::result::Result<HttpReply, String> {
        Ok(reply(status, "{}"))
    }

    fn blip() -> std::result::Result<HttpReply, String> {
        Err("connection reset".to_string())
    }

    /// A JWT-shaped token whose payload is `claims` (no signature: nothing here verifies one).
    fn jwt(claims: &str) -> String {
        use base64::Engine as _;
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.{}",
            engine.encode("{\"alg\":\"HS256\"}"),
            engine.encode(claims),
            engine.encode("signature")
        )
    }

    fn login() -> ScreenLogin {
        ScreenLogin::new(
            "https://cursor.com/loginDeepControl?challenge=abc&uuid=def&mode=login&redirectTarget=cli",
            "def",
            "verifier-secret",
        )
    }

    #[test]
    fn the_browser_url_carries_the_challenge_and_session_in_the_documented_order() {
        assert_eq!(
            login_url("11111111-2222-4333-8444-555555555555", "challenge-value"),
            "https://cursor.com/loginDeepControl?challenge=challenge-value\
             &uuid=11111111-2222-4333-8444-555555555555&mode=login&redirectTarget=cli"
        );
    }

    #[test]
    fn the_poll_url_carries_the_session_and_the_verifier() {
        assert_eq!(
            poll_url(&login()),
            "https://api2.cursor.sh/auth/poll?uuid=def&verifier=verifier-secret"
        );
    }

    #[test]
    fn a_started_screen_login_never_leaks_the_verifier_into_the_url() {
        let started = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(Cursor.screen_login_start())
            .expect("screen login start");
        assert!(started.url.contains("challenge="));
        assert!(!started.url.contains(started.ticket()));
        assert_eq!(started.session().len(), 36, "session is a UUID");
        assert_eq!(
            started.ticket().len(),
            43,
            "verifier is 32 bytes of base64url"
        );
        // The challenge in the URL is exactly the S256 of the verifier we kept.
        assert!(started.url.contains(Pkce::s256(started.ticket()).as_str()));
    }

    #[test]
    fn the_debug_form_redacts_the_verifier() {
        let printed = format!("{:?}", login());
        assert!(!printed.contains("verifier-secret"));
        assert!(printed.contains("***"));
    }

    #[test]
    fn a_404_is_pending_and_nothing_else_is() {
        assert!(matches!(classify_poll(404, ""), Polled::Pending));
        assert!(matches!(classify_poll(400, ""), Polled::Failed { .. }));
        assert!(matches!(classify_poll(500, ""), Polled::Failed { .. }));
        assert!(matches!(
            classify_poll(200, r#"{"accessToken":"a","refreshToken":"r"}"#),
            Polled::Complete { .. }
        ));
    }

    #[test]
    fn an_incomplete_token_pair_fails_without_quoting_the_body() {
        let body = r#"{"accessToken":"secret-access","detail":"nope"}"#;
        match classify_poll(200, body) {
            Polled::Failed { message } => {
                assert!(message.contains("refreshToken missing"));
                assert!(!message.contains("secret-access"));
            }
            _ => panic!("a missing refresh token must fail"),
        }
    }

    #[tokio::test]
    async fn a_successful_poll_yields_tokens_and_an_expiry_five_minutes_early() {
        let access = jwt(r#"{"exp":2000000000}"#);
        let http = FakeHttp::new(vec![ok(&format!(
            r#"{{"accessToken":"{access}","refreshToken":"refresh-1"}}"#
        ))]);
        match poll_once(&http, &login()).await.expect("poll") {
            DevicePoll::Success(tokens) => {
                assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
                assert_eq!(
                    tokens.expires_at.as_millis(),
                    2_000_000_000_000 - EXPIRY_SKEW_MS
                );
            }
            other => panic!("expected success, got {other:?}"),
        }
        assert_eq!(
            http.urls.lock().map(|urls| urls[0].clone()).ok(),
            Some(poll_url(&login()))
        );
    }

    #[tokio::test]
    async fn a_transport_blip_during_a_poll_stays_pending() {
        let http = FakeHttp::new(vec![blip()]);
        assert!(matches!(
            poll_once(&http, &login()).await.expect("poll"),
            DevicePoll::Pending
        ));
    }

    #[test]
    fn an_opaque_access_token_falls_back_to_an_hour() {
        let now = Millis(1_000_000_000_000);
        assert_eq!(
            expiry_of("not-a-jwt", now).as_millis(),
            now.as_millis() + FALLBACK_TTL_MS
        );
    }

    #[test]
    fn an_already_expired_jwt_stays_expired() {
        let now = Millis(1_000_000_000_000);
        let token = jwt(r#"{"exp":1000}"#);
        assert!(expiry_of(&token, now).as_millis() < now.as_millis());
    }

    #[test]
    fn identity_comes_from_the_jwt_claims() {
        let token = jwt(r#"{"sub":"user_123","email":"Someone@Example.COM"}"#);
        let identity = identity_of(&token).expect("identity");
        assert_eq!(identity.id, "user_123");
        assert_eq!(identity.email.as_deref(), Some("someone@example.com"));
        assert_eq!(identity.label, "someone@example.com");
    }

    #[test]
    fn an_identity_less_token_is_an_error_rather_than_a_guess() {
        let token = jwt(r#"{"iat":1}"#);
        assert!(identity_of(&token).is_err());
    }

    #[tokio::test]
    async fn a_401_refresh_is_permanent_and_does_not_retry() {
        let http = FakeHttp::new(vec![failed(401)]);
        let error = refresh_with(&http, "refresh-1", Duration::ZERO)
            .await
            .expect_err("401 must fail");
        assert!(format!("{error}").contains("oauth[permanent]"));
        assert_eq!(http.calls(), 1, "a verdict must not be retried");
    }

    #[tokio::test]
    async fn a_busy_server_is_retried_and_then_succeeds() {
        let http = FakeHttp::new(vec![
            failed(503),
            failed(502),
            ok(r#"{"accessToken":"access-2","refreshToken":"refresh-2"}"#),
        ]);
        let tokens = refresh_with(&http, "refresh-1", Duration::ZERO)
            .await
            .expect("refresh");
        assert_eq!(http.calls(), 3);
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-2"));
        assert_eq!(
            http.bodies.lock().map(|bodies| bodies[0].clone()).ok(),
            Some("{}".to_string()),
            "the body is the literal empty JSON object"
        );
    }

    #[tokio::test]
    async fn a_refresh_without_a_new_refresh_token_keeps_the_old_one() {
        let http = FakeHttp::new(vec![ok(r#"{"accessToken":"access-2"}"#)]);
        let tokens = refresh_with(&http, "refresh-1", Duration::ZERO)
            .await
            .expect("refresh");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
    }

    #[tokio::test]
    async fn three_transient_failures_give_up_with_a_transient_error() {
        let http = FakeHttp::new(vec![failed(500), failed(500), failed(500)]);
        let error = refresh_with(&http, "refresh-1", Duration::ZERO)
            .await
            .expect_err("three failures must give up");
        assert!(format!("{error}").contains("oauth[transient]"));
        assert_eq!(http.calls(), 3);
    }

    #[test]
    fn backoff_doubles_and_stays_within_twenty_percent() {
        for attempt in 0..3 {
            let plain = REFRESH_BACKOFF_BASE.as_secs_f64() * 2f64.powi(attempt as i32);
            for _ in 0..8 {
                let delay = backoff(attempt, REFRESH_BACKOFF_BASE).as_secs_f64();
                assert!(
                    delay >= plain * 0.8 - f64::EPSILON && delay <= plain * 1.2 + f64::EPSILON,
                    "attempt {attempt}: {delay} outside 0.8..=1.2 of {plain}"
                );
            }
        }
    }

    #[test]
    fn the_fingerprint_headers_are_the_catalogue_ones() {
        let catalogue = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../catalog/providers.toml"
        ))
        .expect("catalogue");
        let start = catalogue
            .find("id = \"cursor\"")
            .expect("the catalogue must declare cursor");
        let entry = &catalogue[start..];
        let entry = &entry[..entry.find("\n[[providers]]").unwrap_or(entry.len())];
        for (name, value) in Cursor.fingerprint_headers() {
            assert!(
                entry.contains(&format!("\"{name}\" = \"{value}\"")),
                "catalogue cursor entry is missing {name}: {value}"
            );
        }
    }
}
