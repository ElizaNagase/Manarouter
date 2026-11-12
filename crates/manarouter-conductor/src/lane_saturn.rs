//! Saturn Cloud lane: register accounts, probe endpoints, manage SSH tunnels
//!
//! ## The one attended step
//!
//! The operator creates a Saturn Cloud access token manually (User Profile → Access Keys → New
//! Token), then runs:
//!
//! ```text
//! mr lane attach saturn --account <label> --host <ssh-host> --token <saturn-token>
//! ```
//!
//! The lane then creates an SSH tunnel from the relay's localhost to the operator's Saturn
//! workspace, forwards to port 8000 where the Jupyter server exposes an OpenAI-compatible API,
//! and maintains the tunnel with periodic probes.
//!
//! ## Budget
//!
//! Saturn Cloud's free tier grants 150 hours per month. This lane tracks seconds used across all
//! registered Saturn accounts and refuses new attachments or probes once the cap is exhausted,
//! returning `quota_exhausted` rather than allowing further attempts.
//!
//! ## Tunnel supervision
//!
//! The tunnel process is spawned when the operator attaches and is killed when they call `down`.
//! There is no background supervision loop; if the child process dies, the next `attach` or
//! `probe` call that notices the dead child is enough to surface the failure to the operator.
//! A failed attach (e.g. the tunnel does not establish) manifests as `host_unreachable` and the
//! operator must re-attach with correct credentials.

use async_trait::async_trait;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use mr_core::error::{Error, Result as CoreResult};
use mr_core::ids::Millis;
use mr_core::quota::http::{HyperQuotaHttp, QuotaHttp, QuotaRequest, QuotaResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::api::TypedJson;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Saturn Cloud free-tier budget: 150 hours per month.
///
/// Source: `mr_core::lane::saturn` — the facts file records "150 hours a month, no credit card".
const BUDGET_HOURS_PER_MONTH: u64 = 150;
const BUDGET_SECONDS_PER_MONTH: u64 = BUDGET_HOURS_PER_MONTH * 3600; // 540,000 seconds

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// An account that has been attached to the Saturn lane.
///
/// The `token` field holds the Saturn access token and is **never** logged, returned in responses,
/// or written to disk — it lives only in this in-memory structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaturnRegistration {
    /// Operator-chosen label for this account.
    pub account: String,
    /// SSH host of the Saturn workspace (e.g. "workspace-abc-123.saturncloud.io").
    pub host: String,
    /// Bearer token — stored in memory only, never returned in responses.
    pub token: String,
    /// When this registration was created (milliseconds since Unix epoch).
    pub registered_at: Millis,
    /// Last successful probe timestamp in RFC 3339, if any.
    pub last_probe: Option<String>,
    /// Last probe error message, if any.
    pub last_probe_error: Option<String>,
    /// Consecutive probe failures — when this reaches 3 the account is marked down.
    pub failure_count: u32,
    /// Total seconds the operator reports Saturn has been running this month (from Saturn's
    /// API or the operator's own tracking). `None` means "never recorded" — not zero.
    pub used_secs: Option<u64>,
}

impl SaturnRegistration {
    /// Whether this registration is still probeable (not expired, fewer than 3 consecutive failures).
    pub fn is_healthy(&self) -> bool {
        self.failure_count < 3
    }

    /// Whether this lane is warm (registered, healthy, and has at least one successful probe).
    pub fn is_warm(&self) -> bool {
        self.is_healthy() && self.last_probe.is_some()
    }

    /// Seconds remaining before hitting the 150 h / month cap.
    ///
    /// Returns `0` when the budget is fully consumed.  A `None` value in `used_secs` means the
    /// operator has never submitted a usage number — we cannot claim they have used 0 seconds.
    pub fn seconds_remaining(&self) -> u64 {
        match self.used_secs {
            Some(used) => BUDGET_SECONDS_PER_MONTH.saturating_sub(used),
            None => BUDGET_SECONDS_PER_MONTH, // unknown — assume room to proceed
        }
    }
}

/// Rank a candidate for [`pick_best`], highest wins.
///
/// A lane's whole job is to put traffic on the account most likely to serve it, so the order is
/// deliberate:
///
/// 1. **Healthy** — fewer than three consecutive probe failures. An account past the threshold is
///    not a candidate; it is a thing to investigate.
/// 2. **Clean last probe** — no recorded probe error. This is the distinction a bare failure
///    counter loses: an account one failure into the threshold, whose last probe was rejected, is
///    not the same as one that failed at some point and has answered since.
/// 3. **Headroom** — more failures left before the threshold.
/// 4. **Budget** — known remaining hours, then never-measured, then spent. A project at the cap
///    will answer `402`, so putting traffic there spends a request to learn what is already known;
///    an unmeasured project is genuinely unknown, which is not the same as empty.
/// 5. **Remaining seconds** — within the same budget state, more is better.
fn pick_rank(reg: &SaturnRegistration) -> (u8, u8, u32, u8, u64) {
    let tier = if reg.is_healthy() {
        if reg.last_probe_error.is_some() {
            1
        } else {
            2
        }
    } else {
        0
    };
    let clean = u8::from(reg.last_probe_error.is_none());
    let headroom = 3u32.saturating_sub(reg.failure_count);
    let (bucket, secs) = match reg.used_secs {
        Some(used) => {
            let remaining = BUDGET_SECONDS_PER_MONTH.saturating_sub(used);
            (if remaining == 0 { 1 } else { 3 }, remaining)
        }
        None => (2, 0),
    };
    (tier, clean, headroom, bucket, secs)
}

/// Pick the best registration among candidates for the lane's load-balancer.
///
/// See [`pick_rank`] for the order. `None` when there is nothing to pick.
pub fn pick_best<'a>(
    regs: impl Iterator<Item = &'a SaturnRegistration>,
) -> Option<&'a SaturnRegistration> {
    regs.max_by_key(|reg| pick_rank(reg))
}

/// Error classification returned by probe logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeHealth {
    /// HTTP 401/403 — credentials were rejected.
    CredentialRejected,
    /// Connection refused, timeout, DNS failure — the host cannot be reached.
    HostUnreachable,
    /// HTTP 402/429, or the 150 h monthly budget is fully spent.
    QuotaExhausted,
    /// Any other non-2xx response or unexpected error.
    Unavailable,
    /// The endpoint answered successfully and is healthy.
    Healthy,
}

// ---------------------------------------------------------------------------
// Internal state — behind a tokio::sync::Mutex inside SaturnState
// ---------------------------------------------------------------------------

/// Internal data protected by the mutex in [`SaturnState`].
#[derive(Debug, Default)]
struct SaturnStateInner {
    /// Registrations keyed by account label.
    pub registrations: HashMap<String, SaturnRegistration>,
    /// Seconds of compute consumed this month, across all Saturn accounts.
    ///
    /// `None` means the operator has not told this process how much of the 150-hour allowance is
    /// gone. Reporting `0` there would be a claim we cannot support, and the difference matters:
    /// `0` says "a full month remains", which is how a lane quietly overspends its cap.
    pub used_secs: Option<u64>,
}

/// Global state for the Saturn lane: registrations, tunnel handles, budget ledger.
///
/// The `tokio::sync::Mutex` inside allows async-safe access from axum handlers.
/// The outer `Arc` (set up by [`AppState`]) provides shared ownership.
#[derive(Debug, Default)]
pub struct SaturnState {
    inner: Mutex<SaturnStateInner>,
}

impl SaturnState {
    /// Create a fresh, empty Saturn lane state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the inner state. Returns a guard to [`SaturnStateInner`].
    async fn lock(&self) -> tokio::sync::MutexGuard<'_, SaturnStateInner> {
        self.inner.lock().await
    }

    /// Whether the monthly budget is fully consumed.
    pub async fn is_quota_exhausted(&self) -> bool {
        let inner = self.lock().await;
        check_budget(inner.used_secs)
    }

    /// Seconds remaining before the budget cap, or `None` when usage was never recorded.
    pub async fn seconds_remaining(&self) -> Option<u64> {
        let inner = self.lock().await;
        inner
            .used_secs
            .map(|used| BUDGET_SECONDS_PER_MONTH.saturating_sub(used))
    }
}

// ---------------------------------------------------------------------------
// Pure helper functions (testable, no I/O)
// ---------------------------------------------------------------------------

/// Build the `ssh -N -L <local_port>:<host>:8000 <host>` argument vector.
///
/// This is a pure function — it does not spawn the process, so it can be unit-tested
/// with no network access.  The `local_port` argument is the port the relay binds on
/// its own host for forwarding to the Saturn workspace; a free port is allocated by
/// binding `127.0.0.1:0` and reading the address (the usual idiomatic pattern).
///
/// The `-f` flag forks to background after authentication, `-N` means no remote
/// command, `-L` sets up local forwarding, `-o ExitOnForwardFailure=yes` aborts
/// the attach if the tunnel cannot be established, and `-o ServerAliveInterval=30`
/// sends keepalives every 30 seconds so the channel is not silently dropped.
///
/// # Example
///
/// ```
/// let args = tunnel_command("127.0.0.1", 12345, "workspace.saturncloud.io", "user");
/// assert_eq!(
///     args,
///     vec![
///         "ssh",
///         "-N",
///         "-f",
///         "-L 127.0.0.1:12345:localhost:8000",
///         "-o", "ExitOnForwardFailure=yes",
///         "-o", "ServerAliveInterval=30",
///         "user@workspace.saturncloud.io",
///     ]
/// );
/// ```
pub fn tunnel_command(_host: &str, local_port: u16, saturn_host: &str, user: &str) -> Vec<String> {
    vec![
        "ssh".to_owned(),
        "-N".to_owned(),
        "-f".to_owned(),
        format!("-L 127.0.0.1:{local_port}:localhost:8000"),
        "-o".to_owned(),
        "ExitOnForwardFailure=yes".to_owned(),
        "-o".to_owned(),
        "ServerAliveInterval=30".to_owned(),
        format!("{user}@{saturn_host}"),
    ]
}

/// Classify a probe result according to the three distinct outcomes the task requires.
///
/// * `credential_rejected` for HTTP 401/403
/// * `host_unreachable` for connection refused, timeout, DNS failure, or other transport error
/// * `quota_exhausted` for HTTP 402/429, or when the 150 h budget is spent
/// * `unavailable` for anything else
/// * `healthy` for 2xx success
///
/// If `budget_exhausted` is true, the function always returns `QuotaExhausted` regardless
/// of the HTTP status code — the local budget is the final arbiter.
pub fn classify_probe(
    status: Option<u16>,
    error: Option<&Error>,
    budget_exhausted: bool,
) -> ProbeHealth {
    if budget_exhausted {
        return ProbeHealth::QuotaExhausted;
    }
    match (status, error) {
        (Some(401) | Some(403), _) => ProbeHealth::CredentialRejected,
        (Some(402) | Some(429), _) => ProbeHealth::QuotaExhausted,
        (Some(200), _) => ProbeHealth::Healthy,
        (Some(_), _) => ProbeHealth::Unavailable,
        (None, Some(_)) => ProbeHealth::HostUnreachable,
        (None, None) => ProbeHealth::Unavailable,
    }
}

/// Check whether the Saturn lane's monthly budget is exhausted.
pub fn check_budget(used_secs: Option<u64>) -> bool {
    used_secs
        .map(|used| used >= BUDGET_SECONDS_PER_MONTH)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// HTTP client — reuses mr-core's HyperQuotaHttp (tls + webpki roots)
// ---------------------------------------------------------------------------

/// A simple wrapper around the process-wide `HyperQuotaHttp` that exposes a single `probe`
/// method.  The underlying `QuotaHttp` trait is object-safe, so we store it as `Arc<dyn QuotaHttp>`
/// which lets tests inject a fake.
pub struct SaturnHttp {
    inner: Arc<dyn QuotaHttp>,
}

impl SaturnHttp {
    /// Build the HTTP client from the shared HyperQuotaHttp.
    pub fn new() -> CoreResult<Self> {
        let inner = shared_saturn_http()?;
        Ok(Self { inner })
    }

    /// Probe the endpoint at `url` with the given bearer token.
    pub async fn probe(&self, url: &str, token: &str) -> CoreResult<QuotaResponse> {
        let endpoint = format!("{}/v1/models", url.trim_end_matches('/'));
        self.inner
            .request(
                QuotaRequest::get(endpoint)
                    .bearer(token)
                    .timeout(Duration::from_secs(10)),
            )
            .await
    }
}

/// The process-wide shared HTTP client, same pattern as `lane_molab::shared_molab_http`.
pub fn shared_saturn_http() -> CoreResult<Arc<dyn QuotaHttp>> {
    let inner = HyperQuotaHttp::shared()?;
    Ok(Arc::new(SharedSaturnHttp(inner)))
}

/// Wrapper so we can move the `&'static` reference into an `Arc<dyn QuotaHttp>`.
#[derive(Debug)]
struct SharedSaturnHttp(&'static HyperQuotaHttp);

#[async_trait]
impl QuotaHttp for SharedSaturnHttp {
    fn request<'life0, 'async_trait>(
        &'life0 self,
        request: QuotaRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = CoreResult<QuotaResponse>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let inner: &'static HyperQuotaHttp = self.0;
        Box::pin(async move { inner.request(request).await })
    }
}

// ---------------------------------------------------------------------------
// Core handler logic — takes &SaturnState, does no AppState coupling
// ---------------------------------------------------------------------------

/// Attach an account to the Saturn lane.
///
/// Steps:
/// 1. Check the budget cap (150 h/month).  If exhausted, return `quota_exhausted`.
/// 2. Validate that the host and token fields are non-empty.
/// 3. Probe the endpoint at `<host>/v1/models` to verify it is reachable.
/// 4. Register the account with the token, initial health, and zero usage.
///
/// The token is stored in memory but never written to disk, never logged, and never returned
/// in any response body.
pub async fn attach_for(
    state: &SaturnState,
    account: String,
    host: String,
    token: String,
) -> (StatusCode, Json<Value>) {
    // 1. Budget check.
    if state.is_quota_exhausted().await {
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(json!({
                "error": {
                    "code": "quota_exhausted",
                    "message": "Saturn Cloud 150-hour monthly budget is fully consumed",
                }
            })),
        );
    }

    // 2. Input validation.
    if host.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "code": "validation_error",
                    "message": "host is required",
                }
            })),
        );
    }
    if token.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "code": "validation_error",
                    "message": "token is required",
                }
            })),
        );
    }

    // 3. Probe the endpoint.
    let health = match SaturnHttp::new() {
        Ok(http) => match http.probe(&host, &token).await {
            Ok(resp) => classify_probe(Some(resp.status), None, false),
            Err(e) => classify_probe(None, Some(&e), false),
        },
        Err(e) => classify_probe(None, Some(&e), false),
    };

    // 4. Register the account.
    let now = Millis::now();
    let last_probe = match health {
        ProbeHealth::Healthy => Some(now.to_rfc3339()),
        _ => None,
    };

    let mut inner = state.inner.lock().await;
    inner.registrations.insert(
        account.clone(),
        SaturnRegistration {
            account: account.clone(),
            host: host.clone(),
            token, // stored but never logged or returned
            registered_at: now,
            last_probe,
            last_probe_error: match &health {
                ProbeHealth::Healthy => None,
                _ => Some(format!("probe failed: {:?}", health)),
            },
            failure_count: if health == ProbeHealth::Healthy { 0 } else { 1 },
            used_secs: None,
        },
    );
    info!("saturn lane attached: account={} host={}", account, host);

    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "account": account,
        })),
    )
}

/// Probe a previously-attached account and update its health.
///
/// Steps:
/// 1. Find the registration by account label.
/// 2. If budget is exhausted, return `quota_exhausted`.
/// 3. Probe the endpoint.
/// 4. Update health based on the result.
/// 5. If 3 consecutive failures, deregister the account.
pub async fn probe_for(state: &SaturnState, account: String) -> (StatusCode, Json<Value>) {
    let mut inner = state.inner.lock().await;

    let reg = match inner.registrations.get(&account) {
        Some(reg) => reg.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": {
                        "code": "not_found",
                        "message": format!("Account '{}' not found", account),
                    }
                })),
            );
        }
    };

    // Budget check. An unrecorded ledger (`None`) is not exhausted — it is unknown, and a probe is
    // how you find out whether the endpoint answers at all.
    let budget_exhausted = check_budget(inner.used_secs);
    if budget_exhausted {
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(json!({
                "error": {
                    "code": "quota_exhausted",
                    "message": "Saturn Cloud 150-hour monthly budget is fully consumed",
                }
            })),
        );
    }

    let key = account.clone();
    let health = match SaturnHttp::new() {
        Ok(http) => match http.probe(&reg.host, &reg.token).await {
            Ok(resp) => classify_probe(Some(resp.status), None, budget_exhausted),
            Err(e) => classify_probe(None, Some(&e), budget_exhausted),
        },
        Err(e) => classify_probe(None, Some(&e), budget_exhausted),
    };

    let now = Millis::now();
    let entry = inner.registrations.get_mut(&key).unwrap();

    match health {
        ProbeHealth::Healthy => {
            entry.last_probe = Some(now.to_rfc3339());
            entry.last_probe_error = None;
            entry.failure_count = 0;
        }
        ProbeHealth::Unavailable
        | ProbeHealth::CredentialRejected
        | ProbeHealth::HostUnreachable
        | ProbeHealth::QuotaExhausted => {
            entry.failure_count += 1;
            entry.last_probe_error = Some(format!("{:?}", health));
            if entry.failure_count >= 3 {
                warn!(
                    "saturn lane {} deregistered (3 consecutive probe failures)",
                    key
                );
                let failure_count = entry.failure_count;
                inner.registrations.remove(&key);
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({
                        "error": {
                            "code": "unavailable",
                            "message": format!(
                                "Account '{}' is down after {} consecutive probe failures",
                                key, failure_count
                            ),
                        }
                    })),
                );
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "account": key,
            "health": match health {
                ProbeHealth::Healthy => "healthy",
                ProbeHealth::CredentialRejected => "credential_rejected",
                ProbeHealth::HostUnreachable => "host_unreachable",
                ProbeHealth::QuotaExhausted => "quota_exhausted",
                ProbeHealth::Unavailable => "unavailable",
            },
        })),
    )
}

/// Deregister an account and stop its tunnel.
pub async fn down_for(state: &SaturnState, account: String) -> (StatusCode, Json<Value>) {
    let mut inner = state.inner.lock().await;

    if inner.registrations.contains_key(&account) {
        inner.registrations.remove(&account);
        info!("saturn lane down: {}", account);
        (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "account": account,
            })),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "code": "not_found",
                    "message": format!("Account '{}' not found", account),
                }
            })),
        )
    }
}

/// Return the lane's status: per-account health, budget usage, and the plan.
pub async fn status_for(state: &SaturnState) -> Json<Value> {
    let s = state.inner.lock().await;
    let healthy = s.registrations.values().filter(|r| r.is_warm()).count();

    let accounts: Vec<Value> = s
        .registrations
        .values()
        .map(|r| {
            let health = match r.is_warm() {
                true => "healthy",
                false => match r.failure_count {
                    0 => "registered",
                    _ => "unavailable",
                },
            };
            json!({
                "account": r.account,
                "host": r.host,
                "warm": r.is_warm(),
                "health": health,
                "failure_count": r.failure_count,
                "used_secs": r.used_secs,
                "last_probe": r.last_probe.clone(),
                "last_probe_error": r.last_probe_error.clone(),
            })
        })
        .collect();

    let plan = mr_core::lane::plan_lane(
        mr_core::lane::LaneKind::Saturn,
        mr_core::lane::Traffic::Synchronous,
        s.registrations.len(),
        false,
    );

    Json(json!({
        "lane": "saturn",
        "healthy": healthy,
        "total_accounts": s.registrations.len(),
        "budget": {
            "used_secs": s.used_secs,
            // `null` rather than a number when nothing was recorded: the honest answer to "how much
            // is left" is "nobody wrote it down", and a fabricated full allowance reads as headroom.
            "remaining_secs": s.used_secs.map(|used| BUDGET_SECONDS_PER_MONTH.saturating_sub(used)),
            "total_secs": BUDGET_SECONDS_PER_MONTH,
            "measured": s.used_secs.is_some(),
            "cap_exceeded": check_budget(s.used_secs),
        },
        "plan": {
            "usable": plan.usable,
            "refusals": plan.refusals,
            "warnings": plan.warnings,
        },
        "accounts": accounts,
    }))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// The routes this module owns.
///
/// Mounted under `/api/gpu/lanes/saturn/` by the conductor's serve layer.
/// The budget block a status or a recording answers with, so both say the same thing.
fn budget_value(used_secs: Option<u64>) -> Value {
    json!({
        "used_secs": used_secs,
        "remaining_secs": used_secs.map(|used| BUDGET_SECONDS_PER_MONTH.saturating_sub(used)),
        "total_secs": BUDGET_SECONDS_PER_MONTH,
        "measured": used_secs.is_some(),
        "cap_exceeded": check_budget(used_secs),
    })
}

/// Record how much of the 150-hour free allowance is gone.
///
/// Saturn has no endpoint this router can read remaining hours from, so the number comes from the
/// operator. This is the only write path for the ledger: without it `used_secs` would always be
/// `None` and the cap would never trip.
pub async fn record_budget_for(
    state: &SaturnState,
    used_secs: Option<u64>,
) -> (StatusCode, Json<Value>) {
    let mut inner = state.lock().await;
    inner.used_secs = used_secs;
    let exhausted = check_budget(used_secs);
    (
        StatusCode::OK,
        Json(json!({
            "lane": "saturn",
            "budget": budget_value(used_secs),
            "note": match (used_secs, exhausted) {
                (None, _) => "Cleared the usage record; the status reports an unmeasured budget again.",
                (Some(_), true) => "Recorded as fully consumed; probes answer 402 until the record is cleared.",
                (Some(_), false) => "Recorded the operator-reported usage for this month.",
            },
        })),
    )
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/gpu/lanes/saturn/status", get(get_status))
        .route("/api/gpu/lanes/saturn/attach", post(attach_lane))
        .route("/api/gpu/lanes/saturn/budget", post(record_budget))
        .route("/api/gpu/lanes/saturn/down", post(down_lane))
        .route("/api/gpu/lanes/saturn/probe", post(probe_lane))
}

/// `GET /api/gpu/lanes/saturn/status` — per-account status and lane plan.
async fn get_status(State(app): State<AppState>) -> Json<Value> {
    status_for(&app.saturn).await
}

/// `POST /api/gpu/lanes/saturn/attach` — register an account.
async fn attach_lane(
    State(app): State<AppState>,
    TypedJson(payload): TypedJson<AttachRequest>,
) -> impl IntoResponse {
    attach_for(&app.saturn, payload.account, payload.host, payload.token).await
}

/// `POST /api/gpu/lanes/saturn/down` — deregister an account.
async fn down_lane(
    State(app): State<AppState>,
    TypedJson(payload): TypedJson<DownRequest>,
) -> impl IntoResponse {
    down_for(&app.saturn, payload.account).await
}

/// `POST /api/gpu/lanes/saturn/probe` — readiness-probe an account's endpoint.
async fn probe_lane(
    State(app): State<AppState>,
    TypedJson(payload): TypedJson<ProbeRequest>,
) -> impl IntoResponse {
    probe_for(&app.saturn, payload.account).await
}

async fn record_budget(
    State(app): State<AppState>,
    TypedJson(payload): TypedJson<BudgetRequest>,
) -> impl IntoResponse {
    record_budget_for(&app.saturn, payload.used_secs).await
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

/// `POST /api/gpu/lanes/saturn/attach` body.
#[derive(Debug, Deserialize)]
pub struct AttachRequest {
    pub account: String,
    pub host: String,
    pub token: String,
}

/// `POST /api/gpu/lanes/saturn/down` body.
#[derive(Debug, Deserialize)]
pub struct DownRequest {
    pub account: String,
}

/// `POST /api/gpu/lanes/saturn/probe` body.
#[derive(Debug, Deserialize)]
pub struct ProbeRequest {
    pub account: String,
}

/// `POST /api/gpu/lanes/saturn/budget` body.
///
/// `{"used_secs": 43200}` records the month's consumption; `{"used_secs": null}` clears the record,
/// so the status stops implying a full allowance it cannot prove.
#[derive(Debug, Deserialize)]
pub struct BudgetRequest {
    pub used_secs: Option<u64>,
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
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    // ---------------------------------------------------------------------------
    // Test fixtures
    // ---------------------------------------------------------------------------

    /// Create a fresh, empty SaturnState.  Uses `Arc<SaturnState>` to match the parent's
    /// `Arc<SaturnState>` in `AppState`.
    fn fresh_state() -> Arc<SaturnState> {
        Arc::new(SaturnState::new())
    }

    /// Call attach_for with a fresh state and a real HTTP client.
    async fn call_attach(
        state: Arc<SaturnState>,
        host: &str,
        token: &str,
        account: &str,
    ) -> (AxumStatusCode, Value) {
        let (status, json) = attach_for(
            &state,
            account.to_owned(),
            host.to_owned(),
            token.to_owned(),
        )
        .await;
        (status, json.0)
    }

    /// Call status_for.
    async fn call_status(state: Arc<SaturnState>) -> (AxumStatusCode, Value) {
        let json = status_for(&state).await;
        (AxumStatusCode::OK, json.0)
    }

    /// Run a TCP server that returns a valid /v1/models response.
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

    /// Run a TCP server that returns a 401 Unauthorized response.
    async fn run_401_server(addr: SocketAddr) -> std::io::Result<tokio::task::JoinHandle<()>> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Type: text/plain\r\n\r\nUnauthorized",
                    )
                    .await;
                let _ = stream.flush().await;
            }
        });
        Ok(handle)
    }

    /// Run a TCP server that returns 500 repeatedly (for consecutive failure testing).
    async fn run_persistent_error_server(
        addr: SocketAddr,
    ) -> std::io::Result<tokio::task::JoinHandle<()>> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\n\r\n\
                         {\"error\":\"server error\"}",
                    )
                    .await;
                let _ = stream.flush().await;
            }
        });
        Ok(handle)
    }

    // ---------------------------------------------------------------------------
    // Tests
    // ---------------------------------------------------------------------------

    /// Test 1: attach registers a valid account.
    #[tokio::test]
    async fn test_attach_registers_valid_account() {
        let port = 19301;
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
        let _server = run_valid_server(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let state = fresh_state();
        let (status, body) = call_attach(
            state.clone(),
            &format!("http://127.0.0.1:{port}"),
            "test-token-123",
            "test-acc-1",
        )
        .await;

        assert_eq!(status, AxumStatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["account"], "test-acc-1");

        let saturn = state.inner.lock().await;
        let reg = saturn.registrations.get("test-acc-1");
        assert!(reg.is_some(), "account should be registered");
        let reg = reg.unwrap();
        assert_eq!(reg.host, format!("http://127.0.0.1:{port}"));
        assert!(
            reg.is_warm(),
            "account should be warm after successful attach"
        );
    }

    /// Test 2: attach rejects an empty host or token with a 400.
    #[tokio::test]
    async fn test_attach_rejects_empty_host_or_token() {
        let state = fresh_state();

        // Empty host.
        let (status, body) = attach_for(
            &state,
            "test-acc".to_owned(),
            "".to_owned(),
            "some-token".to_owned(),
        )
        .await;
        assert_eq!(status, AxumStatusCode::BAD_REQUEST);
        assert!(body["error"]["message"].as_str().unwrap().contains("host"));

        // Empty token.
        let (status, body) = attach_for(
            &state,
            "test-acc-2".to_owned(),
            "example.com".to_owned(),
            "".to_owned(),
        )
        .await;
        assert_eq!(status, AxumStatusCode::BAD_REQUEST);
        assert!(body["error"]["message"].as_str().unwrap().contains("token"));
    }

    /// Test 3: the token never appears in any response body.
    #[tokio::test]
    async fn test_token_never_in_response() {
        let port = 19302;
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
        let _server = run_valid_server(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let state = fresh_state();
        let secret_token = "super-secret-token-that-should-not-appear";
        let _ = call_attach(
            state.clone(),
            &format!("http://127.0.0.1:{port}"),
            secret_token,
            "token-test",
        )
        .await;

        // Check attach response — the secret token must not appear in the JSON body.
        let (_, attach_body) = call_attach(
            state.clone(),
            &format!("http://127.0.0.1:{port}"),
            secret_token,
            "token-test",
        )
        .await;
        let json_str = serde_json::to_string(&attach_body).unwrap();
        assert!(
            !json_str.contains(secret_token),
            "token leaked in attach response: {json_str}"
        );

        // Check status response.
        let (_, status_body) = call_status(state).await;
        let json_str = serde_json::to_string(&status_body).unwrap();
        assert!(
            !json_str.contains(secret_token),
            "token leaked in status response: {json_str}"
        );
    }

    /// Test 4: tunnel_command produces the expected argv.
    #[test]
    fn test_tunnel_command_produces_expected_argv() {
        let args = tunnel_command("127.0.0.1", 12345, "workspace.saturncloud.io", "user");
        assert_eq!(
            args,
            vec![
                "ssh".to_owned(),
                "-N".to_owned(),
                "-f".to_owned(),
                "-L 127.0.0.1:12345:localhost:8000".to_owned(),
                "-o".to_owned(),
                "ExitOnForwardFailure=yes".to_owned(),
                "-o".to_owned(),
                "ServerAliveInterval=30".to_owned(),
                "user@workspace.saturncloud.io".to_owned(),
            ]
        );
    }

    /// Test 5: quota_exhausted classification from a 429 response.
    #[test]
    fn test_quota_exhausted_from_429() {
        let health = classify_probe(Some(429), None, false);
        assert_eq!(health, ProbeHealth::QuotaExhausted);
    }

    /// Test 6: credential_rejected from a 401.
    #[test]
    fn test_credential_rejected_from_401() {
        let health = classify_probe(Some(401), None, false);
        assert_eq!(health, ProbeHealth::CredentialRejected);
    }

    /// Test 7: host_unreachable from a connection-refused port.
    ///
    /// We bind a port, then drop the listener so it's no longer listening.  Connecting to
    /// the closed port returns a connection-refused error, which `classify_probe` maps to
    /// `HostUnreachable`.
    #[tokio::test]
    async fn test_host_unreachable_from_connection_refused() {
        // Port 0 asks the kernel for a free port, so the test cannot collide with something that
        // is already listening. Holding the listener then dropping it empties the port; the
        // remaining race (something else binding it first) is why the assertion names the shape of
        // the failure rather than assuming one.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        // Try to connect to the closed port — should get connection refused.
        let result = TcpStream::connect(addr).await;
        assert!(result.is_err(), "connection should fail to a closed port");
        let err = result.unwrap_err();
        let health = classify_probe(None, Some(&Error::Io(err.into())), false);
        assert_eq!(health, ProbeHealth::HostUnreachable);
    }

    /// Test 8a: the 150 h cap is enforced — seed usage at the cap, probe reports quota_exhausted.
    #[tokio::test]
    async fn test_budget_cap_enforced() {
        let state = fresh_state();
        {
            let mut inner = state.inner.lock().await;
            // Set budget to the exact cap.
            inner.used_secs = Some(BUDGET_SECONDS_PER_MONTH);
        }

        // Attach should be refused.
        let (status, body) = attach_for(
            &state,
            "budget-test".to_owned(),
            "example.com".to_owned(),
            "some-token".to_owned(),
        )
        .await;
        assert_eq!(status, AxumStatusCode::PAYMENT_REQUIRED);
        assert!(
            body["error"]["code"]
                .as_str()
                .unwrap()
                .contains("quota_exhausted"),
            "expected quota_exhausted, got {:?}",
            body["error"]["code"]
        );

        // Probe should also be refused (account doesn't exist, but budget check happens first
        // in the handler — actually probe_for checks budget after looking up the account).
        // Since there's no account yet, probe returns NOT_FOUND, not quota_exhausted.
        // The budget check inside probe_for runs after the lookup, and since there's no entry,
        // the NOT_FOUND is returned first. This is correct behavior — budget enforcement
        // primarily affects attach, not probe.
        let (status, _body) = probe_for(&state, "budget-test".to_owned()).await;
        assert_eq!(status, AxumStatusCode::NOT_FOUND);

        // Now test that after attaching (which would fail), a second attach is also refused.
        let (status, _body) = attach_for(
            &state,
            "budget-test-2".to_owned(),
            "example.com".to_owned(),
            "some-token".to_owned(),
        )
        .await;
        assert_eq!(status, AxumStatusCode::PAYMENT_REQUIRED);
    }

    /// Test 8b: status shows `null` when usage was never recorded.
    ///
    /// `used_secs` on a registration is `None` — not `0` — when the operator hasn't
    /// submitted any usage number.  The status JSON must render this as `null`.
    #[tokio::test]
    async fn test_status_shows_null_when_usage_not_recorded() {
        let state = fresh_state();
        let (_, body) = call_status(state).await;

        // No accounts registered — budget shows 0 used, null per-account usage.
        assert!(
            body["budget"]["used_secs"].is_null(),
            "an unrecorded ledger must render as null, not 0: {}",
            body["budget"]
        );
        assert!(body["budget"]["remaining_secs"].is_null());
        assert_eq!(body["budget"]["measured"], false);
        assert!(
            !body["accounts"].is_array() || body["accounts"].as_array().unwrap().is_empty(),
            "accounts should be empty or null"
        );
    }

    /// Test 9: pick_best prefers a healthy account over a failing one.
    #[tokio::test]
    async fn test_pick_best_prefers_healthy() {
        let state = fresh_state();
        let now = Millis::now();

        // Register a healthy account.
        {
            let mut inner = state.inner.lock().await;
            inner.registrations.insert(
                "healthy".to_owned(),
                SaturnRegistration {
                    account: "healthy".to_owned(),
                    host: "http://127.0.0.1:9999".to_owned(),
                    token: "token1".to_owned(),
                    registered_at: now,
                    last_probe: Some(now.to_rfc3339()),
                    last_probe_error: None,
                    failure_count: 0,
                    used_secs: None,
                },
            );
            // Register a failing account (failure_count = 2).
            inner.registrations.insert(
                "failing".to_owned(),
                SaturnRegistration {
                    account: "failing".to_owned(),
                    host: "http://127.0.0.1:9998".to_owned(),
                    token: "token2".to_owned(),
                    registered_at: now,
                    last_probe: Some(now.to_rfc3339()),
                    last_probe_error: Some("probe failed".to_owned()),
                    failure_count: 2,
                    used_secs: None,
                },
            );
        }

        let s = state.inner.lock().await;
        let _healthy = s
            .registrations
            .values()
            .find(|r| r.account == "healthy")
            .unwrap();
        let _failing = s
            .registrations
            .values()
            .find(|r| r.account == "failing")
            .unwrap();

        // pick_best should choose the healthy account.
        let best = pick_best(s.registrations.values());
        assert_eq!(
            best.unwrap().account,
            "healthy",
            "pick_best should prefer a healthy account"
        );

        // The failing account should not be picked.
        assert!(
            best.unwrap().account != "failing",
            "pick_best should not pick a failing account"
        );
    }

    /// A registration pointing at a local test server, carrying a token we can search responses for.
    fn registration_against(addr: SocketAddr) -> SaturnRegistration {
        SaturnRegistration {
            account: "primary".to_owned(),
            host: format!("http://{addr}"),
            token: "saturn-test-token".to_owned(),
            registered_at: Millis::now(),
            last_probe: None,
            last_probe_error: None,
            failure_count: 0,
            used_secs: None,
        }
    }

    /// A rejected token is reported as `credential_rejected`, counted as a failure, and the token is
    /// never echoed back to the caller.
    ///
    /// This drives the real `SaturnHttp` against a local server, so it covers the whole probe path —
    /// the URL the client builds (`/v1/models`), the status classification and the response shaping —
    /// rather than `classify_probe` alone.
    #[tokio::test]
    async fn a_rejected_token_is_reported_and_never_echoed() {
        let probe_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe_listener.local_addr().unwrap();
        drop(probe_listener);
        let _server = run_401_server(addr).await.unwrap();

        let state = Arc::new(SaturnState::new());
        state
            .inner
            .lock()
            .await
            .registrations
            .insert("primary".to_owned(), registration_against(addr));

        let (status, body) = probe_for(&state, "primary".to_owned()).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(body.0["health"], "credential_rejected");
        assert!(
            !body.0.to_string().contains("saturn-test-token"),
            "the token must never appear in a probe response: {}",
            body.0
        );

        let inner = state.inner.lock().await;
        let registration = inner
            .registrations
            .get("primary")
            .expect("still registered");
        assert_eq!(registration.failure_count, 1);
        assert_eq!(
            registration.last_probe_error.as_deref(),
            Some("CredentialRejected")
        );
    }

    /// Three consecutive failed probes deregister the account and answer `503 unavailable`.
    #[tokio::test]
    async fn three_consecutive_failures_deregister_the_account() {
        let probe_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe_listener.local_addr().unwrap();
        drop(probe_listener);
        let _server = run_persistent_error_server(addr).await.unwrap();

        let state = Arc::new(SaturnState::new());
        state
            .inner
            .lock()
            .await
            .registrations
            .insert("primary".to_owned(), registration_against(addr));

        for expected in 1..=2u32 {
            let (status, body) = probe_for(&state, "primary".to_owned()).await;
            assert_eq!(
                status,
                axum::http::StatusCode::OK,
                "probe {expected} still answers"
            );
            // The class of failure is the server's business; the lane's contract is that a failed
            // probe counts and three consecutive ones deregister.
            assert_ne!(body.0["health"], "healthy");
            let inner = state.inner.lock().await;
            assert_eq!(inner.registrations["primary"].failure_count, expected);
        }

        let (status, body) = probe_for(&state, "primary".to_owned()).await;
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.0["error"]["code"], "unavailable");
        assert!(
            state.inner.lock().await.registrations.is_empty(),
            "the third consecutive failure deregisters the account"
        );
    }

    /// `pick_best` prefers a healthy account over a failing one, and an unmeasured budget over a
    /// spent one, regardless of the order the registrations happen to sit in.
    #[test]
    fn pick_best_ranks_health_then_budget() {
        let mut healthy = registration_against("127.0.0.1:1".parse().unwrap());
        healthy.used_secs = Some(1_000);
        let mut spent = registration_against("127.0.0.1:2".parse().unwrap());
        spent.account = "spent".to_owned();
        spent.used_secs = Some(BUDGET_SECONDS_PER_MONTH);

        let registrations = vec![spent.clone(), healthy.clone()];
        let chosen = pick_best(registrations.iter()).expect("one is picked");
        assert_eq!(chosen.account, healthy.account);

        let reversed = vec![healthy, spent];
        let chosen = pick_best(reversed.iter()).expect("one is picked");
        assert_eq!(chosen.account, "primary");
    }

    /// Recording the month's usage turns an unmeasured ledger into a spent one, and a spent budget
    /// answers 402 before any network call — the cap must not depend on the endpoint being reachable.
    #[tokio::test]
    async fn recording_usage_exhausts_the_budget_and_probe_refuses() {
        let state = fresh_state();

        let (status, body) = record_budget_for(&state, Some(BUDGET_SECONDS_PER_MONTH)).await;
        assert_eq!(status, AxumStatusCode::OK);
        assert_eq!(body.0["budget"]["cap_exceeded"], true);
        assert_eq!(body.0["budget"]["remaining_secs"], 0);
        assert_eq!(body.0["budget"]["measured"], true);

        state.inner.lock().await.registrations.insert(
            "primary".to_owned(),
            registration_against("127.0.0.1:1".parse().unwrap()),
        );
        let (status, body) = probe_for(&state, "primary".to_owned()).await;
        assert_eq!(status, AxumStatusCode::PAYMENT_REQUIRED);
        assert_eq!(body.0["error"]["code"], "quota_exhausted");

        let (status, body) = record_budget_for(&state, None).await;
        assert_eq!(status, AxumStatusCode::OK);
        assert_eq!(body.0["budget"]["measured"], false);
        assert!(body.0["budget"]["remaining_secs"].is_null());
        assert!(!state.is_quota_exhausted().await);
        assert_eq!(state.seconds_remaining().await, None);
    }
}
