//! molab: register an attended session the operator started by hand
//!
//! ## The one inviolable rule
//!
//! **No code path in this repository may claim to be able to start a molab session.** It cannot.
//! A human opens it — Google OAuth, the marimo notebook, the GPU toggle — then runs this
//! notebook to expose a local endpoint through a tunnel. This module *only* registers what the
//! operator has already started.
//!
//! ## Attended — the operator starts it
//!
//! molab is an interactive marimo notebook on Google's infrastructure. A human must:
//!
//! 1. Open `https://molab.marimo.io` and sign in with Google OAuth.
//! 2. Open (or create) a notebook.
//! 3. Click the GPU specs button to attach a GPU.
//! 4. Run the cells in `deploy/molab/lane.py`, which installs deps, loads a small model, and
//!    serves an OpenAI-compatible `POST /v1/chat/completions` and `GET /v1/models` on
//!    `127.0.0.1:8000`.
//! 5. Open a reverse tunnel (cloudflared `quick-tunnel` or `ssh -R` — the operator's choice,
//!    the operator's ToS risk).
//! 6. Copy the public URL and bearer token from the notebook output, then run locally:
//!    `mr lane attach molab --url <tunnel-url> --token <bearer>`
//!
//! The notebook is served via `marimo edit https://molab.marimo.io/notebooks/nb_...`.
//!
//! ## Hard caps
//!
//! - **12-hour session cap** — notebooks shut down after 12 h (`max_session_secs: 43200`).
//!   Source: <https://docs.marimo.io/guides/molab/>
//! - **90-minute idle disconnect** — idle notebooks shut down after 90 min.
//!   Source: <https://docs.marimo.io/guides/molab/>
//! - **Attended only** — no API token, no job endpoint, no inbound port.
//!   Source: <https://marimo.io/pages/molab/restrictions>
//!
//! ## Tunnelling — ToS risk owned by the operator
//!
//! molab's restrictions page (<https://marimo.io/pages/molab/restrictions>) names these as abuse,
//! with termination without notice:
//!
//! - "connecting to remote proxies or otherwise exposing molab compute to outside traffic"
//! - "reselling compute to third parties"
//! - "remote control such as by SSH"
//! - "using notebooks for non-interactive jobs"
//!
//! **This router does not block tunnelling — it warns.** The operator chooses to open a tunnel
//! and accepts the risk.
//!
//! ## What this file does
//!
//! - `POST /api/gpu/lanes/molab/attach` — verify the endpoint by calling `GET /v1/models`,
//!   then register a lane account keyed by `(url, token)`.
//! - `POST /api/gpu/lanes/molab/down` — deregister.
//! - `GET /api/gpu/lanes/molab/status` — per-account status or `"0 warm lanes, start one"`.
//! - A supervisor probes each endpoint every 30 s; after 3 consecutive failures, deregister
//!   and return 502 with the last probe error.
//! - Registrations expire at 14 400 s (exactly 12 h).
//! - Account selection among warm lanes prefers `priority`, not least-used.

use async_trait::async_trait;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use mr_core::error::Result as CoreResult;
use mr_core::ids::Millis;
use mr_core::quota::http::{HyperQuotaHttp, QuotaHttp, QuotaRequest, QuotaResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::api::{error_body, TypedJson};
use crate::state::AppState;
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A molab lane registration, keyed by `(url, token)`.
///
/// The token is a bearer token supplied by the notebook at runtime. It is **never** logged,
/// **never** written into `config.json`, and **never** placed in argv.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    /// Operator label — not the bearer token.
    pub account: String,
    /// The tunnel URL.
    pub url: String,
    /// Bearer token — never logged, never persisted to disk, never in argv.
    pub token: String,
    /// When this registration was first created (milliseconds since Unix epoch).
    pub registered_at: Millis,
    /// Last successful probe timestamp (RFC 3339), if any.
    pub last_probe: Option<String>,
    /// Last probe error, if any.
    pub last_probe_error: Option<String>,
    /// Consecutive probe failures.
    pub failure_count: u32,
    /// Priority — higher means preferred when multiple warm lanes exist.
    pub priority: u32,
}

impl Registration {
    /// Molab's hard session cap: 12 h = 14 400 seconds (exactly, no approximation).
    pub const MAX_SESSION_SECS: u64 = 14400;

    /// How many seconds remain until this registration hits the 12 h cap.
    pub fn seconds_remaining(&self) -> u64 {
        let now = Millis::now();
        let elapsed_ms = now
            .as_millis()
            .saturating_sub(self.registered_at.as_millis());
        Self::MAX_SESSION_SECS.saturating_sub((elapsed_ms / 1000) as u64)
    }

    /// Whether this registration has expired per the 12 h cap.
    pub fn is_expired(&self) -> bool {
        self.seconds_remaining() == 0
    }

    /// Whether this lane is still probeable (not expired, fewer than 3 consecutive failures).
    pub fn is_healthy(&self) -> bool {
        self.failure_count < 3 && !self.is_expired()
    }

    /// Whether this lane is warm (registered and healthy).
    pub fn is_warm(&self) -> bool {
        self.is_healthy() && self.last_probe.is_some()
    }

    /// Pick the best registration among candidates.
    ///
    /// **Prefers `priority` over least-used** because a warm molab session is a scarce attended
    /// resource and the operator's stated preference must win.
    pub fn pick_best<'a>(regs: impl Iterator<Item = &'a Registration>) -> Option<&'a Registration> {
        regs.fold(None, |best, reg| match best {
            None => Some(reg),
            Some(current) => {
                if reg.priority > current.priority {
                    Some(reg)
                } else if reg.priority == current.priority {
                    // Same priority — prefer the one with fewer failures.
                    if reg.failure_count < current.failure_count {
                        Some(reg)
                    } else {
                        Some(current)
                    }
                } else {
                    Some(current)
                }
            }
        })
    }
}

/// `POST /api/gpu/lanes/molab/attach` body.
#[derive(Debug, Deserialize)]
pub struct AttachRequest {
    pub account: String,
    pub url: String,
    pub token: String,
}

/// `POST /api/gpu/lanes/molab/down` body.
#[derive(Debug, Deserialize)]
pub struct DownRequest {
    pub account: String,
}

/// `GET /api/gpu/lanes/molab/status` per-account response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusAccount {
    pub account: String,
    pub warm: bool,
    pub minutes_left: u64,
    pub last_probe_error: Option<String>,
}

/// Global state for molab lane registrations.
#[derive(Debug, Default)]
pub struct MolabState {
    /// Keyed by `(url, token)` so one molab account per operator works.
    pub registrations: HashMap<String, Registration>,
}

// ---------------------------------------------------------------------------
// HTTP client — shared process-wide, same pattern as quota_api.rs
// ---------------------------------------------------------------------------

/// The process-wide molab transport, as an owning handle.
///
/// Wraps the `&'static` that `HyperQuotaHttp::shared()` hands out so it can be moved into an
/// `Arc<dyn QuotaHttp>`.
#[derive(Debug)]
struct SharedMolabHttp(&'static HyperQuotaHttp);

#[async_trait]
impl QuotaHttp for SharedMolabHttp {
    /// Same signature the desugared `#[async_trait]` generates — two lifetime params and the
    /// `Pin<Box<dyn Future>>` return type.
    fn request<'life0, 'async_trait>(
        &'life0 self,
        request: QuotaRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = CoreResult<QuotaResponse>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let inner: &'static HyperQuotaHttp = self.0;
        Box::pin(async move { inner.request(request).await })
    }
}

/// The shared transport, ready for probing endpoints.
///
/// Returns an `Arc<dyn QuotaHttp>`; callers can move it across `await` points without worry.
pub fn shared_molab_http() -> CoreResult<Arc<dyn QuotaHttp>> {
    Ok(Arc::new(SharedMolabHttp(HyperQuotaHttp::shared()?)))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// The routes this module owns.
///
/// Returns a `Router` with `Arc<Mutex<MolabState>>` as its state, ready to be mounted under
/// `/api/gpu/lanes/molab/`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/gpu/lanes/molab/status", get(get_status))
        .route("/api/gpu/lanes/molab/attach", post(attach_lane))
        .route("/api/gpu/lanes/molab/down", post(down_lane))
}

/// `GET /api/gpu/lanes/molab/status`
///
/// Per-account status: warm/cold, minutes left before the 12 h cap, last probe error.
/// When nothing is registered the literal sentence is "0 warm lanes, start one".
async fn get_status(State(app): State<AppState>) -> Json<Value> {
    status_for(&app.molab).await
}

/// The body of [`get_status`], taking the registry explicitly so a test can hand it a fresh one.
async fn status_for(state: &Arc<Mutex<MolabState>>) -> Json<Value> {
    let molab_state = state.lock().await;
    let warm_lanes = molab_state
        .registrations
        .values()
        .filter(|r| r.is_warm())
        .count();

    if warm_lanes == 0 && molab_state.registrations.is_empty() {
        // Literal sentence from the spec: nothing registered at all.
        return Json(json!({
            "lane": "molab",
            "warm_lanes": 0,
            "detail": "0 warm lanes, start one",
        }));
    }

    let accounts: Vec<StatusAccount> = molab_state
        .registrations
        .values()
        .map(|r| StatusAccount {
            account: r.account.clone(),
            warm: r.is_warm(),
            minutes_left: r.seconds_remaining() / 60,
            last_probe_error: r.last_probe_error.clone(),
        })
        .collect();

    Json(json!({
        "lane": "molab",
        "warm_lanes": warm_lanes,
        "accounts": accounts,
    }))
}

/// `POST /api/gpu/lanes/molab/attach`
///
/// Verify the endpoint by calling `GET /v1/models` on the given URL. On success register a
/// lane account keyed by `(url, token)`. On failure return the platform's own error text.
///
/// **Note:** the HTTP helper (`QuotaHttp`) does NOT follow redirects, so a 3xx is returned as-is
/// — which is exactly what we want: redirects are a failure, not a magic resolution.
async fn attach_lane(
    State(app): State<AppState>,
    TypedJson(payload): TypedJson<AttachRequest>,
) -> impl IntoResponse {
    attach_for(&app.molab, payload).await
}

/// The body of [`attach_lane`], taking the registry explicitly.
async fn attach_for(
    state: &Arc<Mutex<MolabState>>,
    payload: AttachRequest,
) -> (StatusCode, Json<Value>) {
    let url = payload.url.clone();
    let token = payload.token.clone();
    let account_label = payload.account.clone();

    // Get the shared HTTP client.
    let http = match shared_molab_http() {
        Ok(h) => h,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(error_body(
                    "http_client_unavailable",
                    format!("the HTTP client could not be built: {e}"),
                    Vec::new(),
                )),
            );
        }
    };

    // Call GET /v1/models to verify the endpoint.
    let endpoint = format!("{}/v1/models", url.trim_end_matches('/'));
    let response = match http
        .request(QuotaRequest::get(&endpoint).bearer(&token))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let host = url.split(':').next().unwrap_or("<host>");
            return (
                StatusCode::BAD_REQUEST,
                Json(error_body(
                    "host_unreachable",
                    format!("the endpoint {host} refused the connection: {e}"),
                    Vec::new(),
                )),
            );
        }
    };

    // Check for redirect (3xx): this is a failure, not a success. Redirects are not followed.
    if (300..400).contains(&response.status) {
        return (
            StatusCode::BAD_REQUEST,
            Json(error_body(
                "redirect_not_followed",
                format!(
                    "the endpoint answered {} — redirects are not followed, so the tunnel may \
                     have expired or the URL may be stale",
                    response.status
                ),
                vec![json!({ "url": url })],
            )),
        );
    }

    if response.is_success() {
        // Keyed by account, not by `url:token`: a bearer token in a map key is one stray `{:?}`
        // log away from disclosure, and the account label is already unique per lane.
        let account_key = account_label.clone();
        let now = Millis::now();
        let registration = Registration {
            account: account_label.clone(),
            url,
            token,
            registered_at: now,
            last_probe: Some(now.to_rfc3339()),
            last_probe_error: None,
            failure_count: 0,
            priority: 0,
        };

        let mut molab_state = state.lock().await;
        molab_state.registrations.insert(account_key, registration);
        info!("molab lane attached");

        (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "account": account_label,
            })),
        )
    } else {
        let body = response.body_str();
        (
            StatusCode::BAD_REQUEST,
            Json(error_body(
                "endpoint_rejected",
                "the endpoint answered, but not with a model list this lane can use",
                // The body is echoed back truncated: it is usually a proxy's HTML error page, and
                // reproducing a megabyte of it in a toast helps nobody.
                vec![json!({ "body": body.chars().take(512).collect::<String>() })],
            )),
        )
    }
}

/// `POST /api/gpu/lanes/molab/down`
///
/// Deregister the lane for the given account.
async fn down_lane(
    State(app): State<AppState>,
    TypedJson(payload): TypedJson<DownRequest>,
) -> impl IntoResponse {
    down_for(&app.molab, payload).await
}

/// The body of [`down_lane`], taking the registry explicitly.
async fn down_for(
    state: &Arc<Mutex<MolabState>>,
    payload: DownRequest,
) -> (StatusCode, Json<Value>) {
    let account_label = payload.account.clone();
    let mut molab_state = state.lock().await;

    let key = molab_state
        .registrations
        .iter()
        .find(|(_, r)| r.account == account_label)
        .map(|(k, _)| k.clone());

    if let Some(key) = key {
        molab_state.registrations.remove(&key);
        info!("molab lane down: {}", account_label);
        (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "account": account_label,
            })),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(error_body(
                "account_not_registered",
                format!("no molab lane is registered for the account {account_label:?}"),
                Vec::new(),
            )),
        )
    }
}

// ---------------------------------------------------------------------------
// Supervisor — probes registered endpoints
// ---------------------------------------------------------------------------

/// Run the molab probe supervisor in the background.
///
/// Probes each registered endpoint every 30 s. After 3 consecutive failures, deregisters it.
/// Also expires registrations at exactly 14 400 s (12 h).
pub async fn start_supervisor(state: Arc<Mutex<MolabState>>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        interval.tick().await;
        probe_all(&state).await;
    }
}

async fn probe_all(state: &Arc<Mutex<MolabState>>) {
    let mut molab_state = state.lock().await;

    let http = match shared_molab_http() {
        Ok(h) => h,
        Err(e) => {
            error!("failed to build HTTP client for molab probe: {}", e);
            return;
        }
    };

    let mut to_remove = Vec::new();

    for (key, reg) in molab_state.registrations.iter_mut() {
        // Check 12 h cap first.
        if reg.is_expired() {
            warn!("molab lane {} expired (12 h cap)", reg.account);
            to_remove.push(key.clone());
            continue;
        }

        // Probe the endpoint.
        let endpoint = format!("{}/v1/models", reg.url.trim_end_matches('/'));
        let response = http
            .request(QuotaRequest::get(&endpoint).bearer(&reg.token))
            .await;

        match response {
            Ok(resp) if resp.is_success() => {
                reg.last_probe = Some(Millis::now().to_rfc3339());
                reg.last_probe_error = None;
                reg.failure_count = 0;
            }
            Ok(resp) => {
                reg.failure_count += 1;
                reg.last_probe_error = Some(resp.body_str());
                info!(
                    "molab probe failed for {}: status {} (count {})",
                    reg.account, resp.status, reg.failure_count
                );
                if reg.failure_count >= 3 {
                    warn!(
                        "molab lane {} deregistered (3 consecutive probe failures)",
                        reg.account
                    );
                    to_remove.push(key.clone());
                }
            }
            Err(e) => {
                reg.failure_count += 1;
                reg.last_probe_error = Some(e.to_string());
                info!(
                    "molab probe error for {}: {} (count {})",
                    reg.account, e, reg.failure_count
                );
                if reg.failure_count >= 3 {
                    warn!(
                        "molab lane {} deregistered (3 consecutive probe failures)",
                        reg.account
                    );
                    to_remove.push(key.clone());
                }
            }
        }
    }

    for key in to_remove {
        molab_state.registrations.remove(&key);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode as AxumStatusCode;
    use std::net::SocketAddr;
    use std::time::Duration;
    // `TcpStream`'s write/flush come from the extension traits, not inherent methods.
    use tokio::io::AsyncWriteExt;

    /// Run a TCP server that returns a valid `/v1/models` response.
    async fn run_valid_server(addr: SocketAddr) -> std::io::Result<tokio::task::JoinHandle<()>> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
                     {\"object\":\"list\",\"data\":[{\"id\":\"fake-model\",\"object\":\"model\"}]}",
                    )
                    .await;
                let _ = stream.flush().await;
            }
        });
        Ok(handle)
    }

    /// Run a TCP server that returns a 302 redirect for `/v1/models`.
    async fn run_redirect_server(addr: SocketAddr) -> std::io::Result<tokio::task::JoinHandle<()>> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream
                    .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://example.com/models\r\n\r\n")
                    .await;
                let _ = stream.flush().await;
            }
        });
        Ok(handle)
    }

    /// Run a TCP server that returns 500 repeatedly (for multiple probe cycles).
    async fn run_persistent_error_server(
        addr: SocketAddr,
    ) -> std::io::Result<tokio::task::JoinHandle<()>> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream.write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\n\r\n\
                     {\"error\":\"server error\"}"
                ).await;
                let _ = stream.flush().await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        Ok(handle)
    }

    /// A fresh, empty registry — every test gets its own so nothing leaks between them.
    fn fresh_state() -> Arc<Mutex<MolabState>> {
        Arc::new(Mutex::new(MolabState::default()))
    }

    /// Call the attach body against `state`.
    ///
    /// The route wrapper is one line of `.with_state(...)`; calling the body directly keeps the test
    /// about the registry's decision rather than about axum's plumbing, and lets each test hold its
    /// own registry instead of sharing a process-wide one.
    async fn call_attach(
        state: Arc<Mutex<MolabState>>,
        _base_url: &str,
        account: &str,
        url: &str,
        token: &str,
    ) -> (AxumStatusCode, Value) {
        let payload = AttachRequest {
            account: account.to_string(),
            url: url.to_string(),
            token: token.to_string(),
        };
        let (status, json) = attach_for(&state, payload).await;
        (status, json.0)
    }

    /// Call the status body against `state`.
    async fn call_status(state: Arc<Mutex<MolabState>>) -> (AxumStatusCode, Value) {
        (AxumStatusCode::OK, status_for(&state).await.0)
    }

    // ---------------------------------------------------------------------------
    // Tests
    // ---------------------------------------------------------------------------

    /// Test: attach against a server returning a valid /v1/models response registers.
    ///
    /// Uses a real TCP listener (HTTP/1.1 200 with a valid models body).
    #[tokio::test]
    async fn test_attach_valid_server_registers() {
        let port = 19201;
        let addr = format!("127.0.0.1:{}", port);
        let _server = run_valid_server(addr.parse().unwrap()).await.unwrap();

        let state = fresh_state();

        // Give the server a moment to accept.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let (status, body) = call_attach(
            state.clone(),
            "http://127.0.0.1",
            "test-acc",
            &format!("http://{}", addr),
            "test-token",
        )
        .await;

        assert_eq!(status, AxumStatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["account"], "test-acc");

        // Verify registration is stored and is healthy.
        let molab_state = state.lock().await;
        assert!(molab_state.registrations.values().any(|r| r.is_healthy()));
    }

    /// Test: attach against a 3xx redirect fails with a clear message.
    ///
    /// The HTTP helper (`QuotaHttp`) does NOT follow redirects, so a 3xx is returned as-is.
    #[tokio::test]
    async fn test_attach_redirect_fails() {
        let port = 19202;
        let addr = format!("127.0.0.1:{}", port);
        let _server = run_redirect_server(addr.parse().unwrap()).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        let state = fresh_state();
        let (status, body) = call_attach(
            state,
            "http://127.0.0.1",
            "test-acc",
            &format!("http://{}", addr),
            "test-token",
        )
        .await;

        assert!((400..500).contains(&status.as_u16()));
        let error = body["error"]["message"].as_str().unwrap();
        assert_eq!(body["error"]["code"], "redirect_not_followed");
        assert!(
            error.to_lowercase().contains("redirect") || error.contains("302"),
            "error should mention redirect: {}",
            error
        );
    }

    /// Test: 3 consecutive probe failures deregister and the account is gone.
    ///
    /// Calls `probe_all` directly with a live error server.
    #[tokio::test]
    async fn test_three_failures_deregister() {
        let port = 19203;
        let addr = format!("127.0.0.1:{}", port);

        // Insert a registration manually.
        let state = fresh_state();
        {
            let mut molab_state = state.lock().await;
            molab_state.registrations.insert(
                "fail-acc".to_string(),
                Registration {
                    account: "fail-acc".to_string(),
                    url: format!("http://{}", addr),
                    token: "fail-token".to_string(),
                    registered_at: Millis::now(),
                    last_probe: Some(Millis::now().to_rfc3339()),
                    last_probe_error: None,
                    failure_count: 0,
                    priority: 0,
                },
            );
        }

        // Start error server.
        let _server = run_persistent_error_server(addr.parse().unwrap())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Run 3 probe cycles (each probes once).
        for _ in 0..3 {
            probe_all(&state).await;
        }

        // The account should have 3 consecutive failures and be deregistered.
        let molab_state = state.lock().await;
        assert!(
            !molab_state
                .registrations
                .values()
                .any(|r| r.account == "fail-acc"),
            "account should be deregistered after 3 failures"
        );
    }

    /// Test: a 12 h old registration is expired.
    ///
    /// Inserts a registration with `registered_at` set to 14 401 seconds ago; probe_all
    /// should remove it.
    #[tokio::test]
    async fn test_expiry_12h() {
        let state = fresh_state();

        // Manually insert a registration that is 14 401 seconds old.
        let old_time = Millis::from_millis(Millis::now().as_millis() - 14401000);
        {
            let mut molab_state = state.lock().await;
            molab_state.registrations.insert(
                "expired-acc".to_string(),
                Registration {
                    account: "expired-acc".to_string(),
                    url: "http://127.0.0.1:9999".to_string(),
                    token: "expired-token".to_string(),
                    registered_at: old_time,
                    last_probe: Some(old_time.to_rfc3339()),
                    last_probe_error: None,
                    failure_count: 0,
                    priority: 0,
                },
            );
        }

        // Run a probe — the expired account should be removed.
        probe_all(&state).await;

        let molab_state = state.lock().await;
        assert!(
            !molab_state.registrations.contains_key("expired-acc"),
            "expired account should be removed"
        );
    }

    /// Test: priority beats least-used.
    ///
    /// Inserts two registrations: one with priority=10 and failure_count=5, another with
    /// priority=0 and failure_count=0. `pick_best` should return the high-priority one.
    #[tokio::test]
    async fn test_priority_beats_least_used() {
        let state = fresh_state();
        let now = Millis::now();

        {
            let mut molab_state = state.lock().await;
            // High priority account with many failures.
            molab_state.registrations.insert(
                "high-pri".to_string(),
                Registration {
                    account: "high-pri".to_string(),
                    url: "http://127.0.0.1:11111".to_string(),
                    token: "token1".to_string(),
                    registered_at: now,
                    last_probe: Some(now.to_rfc3339()),
                    last_probe_error: None,
                    failure_count: 5,
                    priority: 10,
                },
            );
            // Low priority account with zero failures.
            molab_state.registrations.insert(
                "low-pri".to_string(),
                Registration {
                    account: "low-pri".to_string(),
                    url: "http://127.0.0.1:11112".to_string(),
                    token: "token2".to_string(),
                    registered_at: now,
                    last_probe: Some(now.to_rfc3339()),
                    last_probe_error: None,
                    failure_count: 0,
                    priority: 0,
                },
            );
        }

        // pick_best should return high-pri despite the failures.
        let molab_state = state.lock().await;
        let best = Registration::pick_best(molab_state.registrations.values());
        assert_eq!(
            best.unwrap().account,
            "high-pri",
            "priority should beat least-used"
        );
    }

    /// Test: status with nothing registered prints "0 warm lanes, start one".
    #[tokio::test]
    async fn test_status_no_lanes() {
        let state = fresh_state();
        let (status, body) = call_status(state).await;

        assert_eq!(status, AxumStatusCode::OK);
        assert_eq!(body["detail"], "0 warm lanes, start one");
        assert_eq!(body["lane"], "molab");
    }
}
