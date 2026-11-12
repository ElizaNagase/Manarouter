//! Cerebrium: deploy a serverless inference app and reach it
//!
//! Owned by one work item, so the file is the unit of review: nothing here is shared with the
//! other lane modules and nothing outside it needs to know how Cerebrium works.
//!
//! The facts — what the platform's documents actually say — live in
//! `mr_core::lane::cerebrium` and `mr_core::gpu::cerebrium`. This module is the half that talks to
//! the network, which is why it lives in the conductor (the admin path) rather than in the relay
//! (the request path): a deployment is an operator action, not a request.
//!
//! ## Two hosts, and the mistake issue #10 made
//!
//! `rest.cerebrium.ai` is the **control plane**: create an app, list it, delete it. Inference goes
//! to `api.cerebrium.ai`. A *custom* runtime — the only kind that runs `vllm serve` — has no
//! function name in the path; everything after `/{app_name}/` is proxied verbatim to the
//! container's port. So the base URL an OpenAI client should be pointed at is
//!
//! ```text
//! https://api.cerebrium.ai/v4/{project_id}/{app_name}/v1
//! ```
//!
//! `…/{app}/run` is the Cortex *function* API — a function literally named `run` — which a custom
//! runtime does not serve. Pointing a provider at it yields a 404, not a completion.
//!
//! ## Two credentials that both look like "the key"
//!
//! A **service-account token** authenticates the control plane. A project **API key**
//! authenticates inference. They are different key types and neither works on the other host, so
//! retrying with the other value just moves the 401. This lane holds the service-account token
//! only, which is why [`probe_for`] reports control-plane state and does **not** claim to prove
//! that inference works. Minting an inference key is
//! `POST /v2/projects/{project_id}/api-keys` (it answers with the secret in the body); that is a
//! separate, deliberate act, not something a status poll does behind the operator's back.
//!
//! ## The trap this module exists to make impossible
//!
//! `disableAuth` defaults to **`true`**. A deploy that omits the field publishes the endpoint to
//! the public internet with no credential — on a vLLM server the operator pays for, serving
//! whatever model they loaded. So the field is **required** here: omitting it is a 400 that says
//! exactly that, and asking for `true` needs an explicit acknowledgement.
//!
//! Two more documented defaults are worth knowing, and both are reported rather than assumed:
//!
//! * `cooldownPeriodSeconds` defaults to **10 s** (range 0–3600) — one sixth of Modal's 60 s
//!   `scaledown_window`, so a warm window copied from a Modal recipe is gone before the next
//!   request. A shorter cooldown costs less and misses more.
//! * `replicaConcurrency` defaults to **1 on GPU apps** (100 on CPU), which caps a container at one
//!   in-flight request and makes the platform scale out 1:1 instead of batching. It must be raised
//!   to match the server's `--max-num-seqs` or the GPU does one request at a time.
//!
//! ## What this file does
//!
//! - `POST /attach` — verify the token and project by listing apps, then register the account and
//!   learn the app id if an app of that name already exists.
//! - `POST /deploy` — **step 1 only**: create the app record and report the `build_id` and
//!   pre-signed `upload_url`. Uploading the source zip is step 2 and is not done here, because the
//!   bundle renderer owns the bytes; a lane that uploaded something else would deploy the wrong
//!   thing.
//! - `POST /down` — delete the app, resolving its id first if this process never learned it. A lane
//!   that cannot tear down is a bill that never stops.
//! - `POST /probe` — read the app's build state from the control plane.
//! - `GET /status` — what is registered, and what is merely claimed.
//!
//! Nothing here runs in the background. Each route does one operator-visible thing and answers with
//! what the platform said.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use http::Method;
use mr_core::error::Result as CoreResult;
use mr_core::gpu::cerebrium::{app_name_is_safe as renderer_app_name_is_safe, compute_by_id, COOLDOWN_RANGE, PORT};
use mr_core::ids::Millis;
use mr_core::quota::http::{HyperQuotaHttp, QuotaHttp, QuotaRequest};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::info;

use crate::api::error_body;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Where the platform lives — verified against the published OpenAPI document
// ---------------------------------------------------------------------------

/// Control plane: app lifecycle, builds, secrets, billing.
pub const CONTROL_PLANE: &str = "https://rest.cerebrium.ai";

/// Inference router. A custom runtime is reached at `/{project}/{app}/{anything}`.
pub const INFERENCE_HOST: &str = "https://api.cerebrium.ai";

/// Sent as the required `cliVersion` field. The API documents it as "version of the client
/// initiating the deployment", so the honest value is this build's version.
pub const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Hobby Free includes three apps. Reported, never enforced: the field that would tell us the plan
/// is not something this lane reads, and a guess would refuse a deploy on a project that has room.
pub const FREE_PLAN_MAX_APPS: usize = 3;

/// One control-plane call should not outlive a person's patience. Deploys answer quickly because
/// the build happens after the upload, not inside the create call.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of an upstream error body is repeated back. Enough to be diagnosable, short enough that
/// a stray HTML error page cannot swamp the panel.
const MAX_DETAIL: usize = 512;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// A Cerebrium lane registration: one project, one app, one service-account token.
///
/// The token is held in memory so the operator can deploy twice without re-pasting it. It is
/// **never** serialised into a response — see [`CerebriumRegistration::view`] — and **never**
/// logged.
#[derive(Debug, Clone)]
pub struct CerebriumRegistration {
    /// Operator label. The map key, and the only name the panel ever uses.
    pub account: String,
    /// Cerebrium project id (`prj_…`).
    pub project_id: String,
    /// App name within the project.
    pub app_name: String,
    /// Service-account token. Never serialised, never logged.
    pub token: String,
    /// App id, once known. The create response does not carry it, so it is learned by listing apps.
    pub app_id: Option<String>,
    /// Build id from the last create.
    pub build_id: Option<String>,
    /// Compute id requested for the last deploy.
    pub compute: Option<String>,
    /// What `disableAuth` was sent as — `None` until a deploy has happened.
    pub disable_auth: Option<bool>,
    /// Project plan name, when the platform volunteered one. Never invented.
    pub plan: Option<String>,
    /// When this registration was created.
    pub registered_at: Millis,
    /// Last successful control-plane read (RFC 3339).
    pub last_probe: Option<String>,
    /// Last control-plane complaint, if any.
    pub last_probe_error: Option<String>,
}

impl CerebriumRegistration {
    /// The registration as the panel may see it. Deliberately built field by field: an added field
    /// is a decision, and a `#[derive(Serialize)]` on the struct would put the token in every
    /// response the day someone forwarded the whole record.
    pub fn view(&self) -> Value {
        json!({
            "account": self.account,
            "project_id": self.project_id,
            "app_name": self.app_name,
            "app_id": self.app_id,
            "build_id": self.build_id,
            "compute": self.compute,
            "disable_auth": self.disable_auth,
            "plan": self.plan,
            "registered_at": self.registered_at.to_rfc3339(),
            "last_probe": self.last_probe,
            "last_probe_error": self.last_probe_error,
            "has_app_id": self.app_id.is_some(),
        })
    }

    /// `…/v2/projects/{project_id}/apps`, the collection this registration lives in.
    fn apps_url(&self) -> String {
        format!("{CONTROL_PLANE}/v2/projects/{}/apps", self.project_id)
    }

    /// The inference base URL a provider should be pointed at once the app is running.
    fn inference_base_url(&self) -> String {
        format!("{INFERENCE_HOST}/v4/{}/{}/v1", self.project_id, self.app_name)
    }
}

/// Lane state: registrations by account label.
///
/// A plain `Mutex<HashMap<…>>` rather than a handle per account, because every route here is
/// "list, then act on one", which is a map read followed by a map write.
#[derive(Debug, Default)]
pub struct CerebriumState {
    inner: Mutex<HashMap<String, CerebriumRegistration>>,
}

impl CerebriumState {
    /// Empty state. `AppState` is `Clone`, so this is always reached through an `Arc`.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    async fn lock(&self) -> tokio::sync::MutexGuard<'_, HashMap<String, CerebriumRegistration>> {
        self.inner.lock().await
    }
}

/// The process-wide HTTP client, as a trait object so tests can substitute one.
///
/// `HyperQuotaHttp` is shared because it loads the webpki roots once; building a client per request
/// would re-read the root store on every panel refresh.
pub fn shared_cerebrium_http() -> CoreResult<Arc<dyn QuotaHttp>> {
    let client = HyperQuotaHttp::shared()?;
    Ok(Arc::new(SharedCerebriumHttp(client)))
}

/// Adapter that lets the `&'static` shared client be moved into an `Arc<dyn QuotaHttp>`.
#[derive(Debug)]
struct SharedCerebriumHttp(&'static HyperQuotaHttp);

#[async_trait::async_trait]
impl QuotaHttp for SharedCerebriumHttp {
    async fn request(
        &self,
        request: QuotaRequest,
    ) -> CoreResult<mr_core::quota::http::QuotaResponse> {
        self.0.request(request).await
    }
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

/// `POST /api/gpu/lanes/cerebrium/attach` body.
#[derive(Debug, Deserialize)]
pub struct AttachRequest {
    /// Operator label for this account.
    pub account: String,
    /// Cerebrium project id.
    pub project_id: String,
    /// Service-account token. Comes from `--secret-stdin` in the CLI, never argv.
    pub token: String,
    /// App name inside the project.
    pub app_name: String,
}

/// `POST /api/gpu/lanes/cerebrium/deploy` body — step 1 of two.
#[derive(Debug, Deserialize)]
pub struct DeployRequest {
    /// Which registration to deploy.
    pub account: String,
    /// Verbatim contents of `cerebrium.toml`. A deploy **replaces** the app record, so every key
    /// omitted here takes its default; send the complete file.
    pub cerebrium_toml: String,
    /// Compute id from `mr_core::gpu::cerebrium::COMPUTE_TYPES`, e.g. `ADA_L4`.
    pub compute: String,
    /// Number of GPUs. Defaults to 1 — a vLLM server on zero GPUs is not a thing.
    pub gpu_count: Option<u32>,
    /// **Required.** Cerebrium defaults this to `true`, which publishes the endpoint.
    pub disable_auth: Option<bool>,
    /// Required when `disable_auth` is `true`: the operator has read what that means.
    pub acknowledge_public_endpoint: Option<bool>,
    /// Scale-down window in seconds. Defaults to the platform's 10 s; range 0–3600.
    pub cooldown_secs: Option<u32>,
    /// Requests one container may serve at once. The platform defaults to 1 on GPU apps, which
    /// serialises vLLM; set it to the server's `--max-num-seqs`.
    pub replica_concurrency: Option<u32>,
    /// Ceiling on replicas.
    pub max_replicas: Option<u32>,
    /// Custom-runtime command, as argv. `vllm serve …` for an inference app.
    pub entrypoint: Option<Vec<String>>,
}

/// `POST /api/gpu/lanes/cerebrium/down` body.
#[derive(Debug, Deserialize)]
pub struct DownRequest {
    /// Which registration to tear down.
    pub account: String,
}

/// `POST /api/gpu/lanes/cerebrium/probe` body.
#[derive(Debug, Deserialize)]
pub struct ProbeRequest {
    /// Which registration to check.
    pub account: String,
}

// ---------------------------------------------------------------------------
// Errors — one envelope, four classes, plus the two "you named a thing that is not there"s
// ---------------------------------------------------------------------------

/// Everything this lane can refuse with.
///
/// The four transport classes are the ones an operator can act on differently: a rejected token is
/// the operator's to fix, a spent quota is money, an unreachable host is the network, and anything
/// else is the platform. Collapsing them into one "cerebrium failed" is what makes a lane
/// undebuggable at 2 a.m.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CerebriumError {
    /// 401/403 from the control plane: the token is wrong, expired or not a service account.
    CredentialRejected,
    /// 402/429: the plan is out of allowance.
    QuotaExhausted,
    /// Nothing answered, or the answer never arrived.
    HostUnreachable(String),
    /// The platform answered, but not with anything usable.
    Unavailable { status: u16, detail: String },
    /// The project id does not exist for this token.
    ProjectNotFound,
    /// The app is not in the project (any more).
    AppNotFound,
    /// The request never left: a field is missing or impossible.
    Invalid(String),
    /// No such registration in this process.
    NotRegistered(String),
    /// The HTTP client could not be built.
    ClientUnavailable(String),
}

impl CerebriumError {
    /// The stable machine-readable code the panel and the CLI both read.
    pub fn code(&self) -> &'static str {
        match self {
            Self::CredentialRejected => "credential_rejected",
            Self::QuotaExhausted => "quota_exhausted",
            Self::HostUnreachable(_) => "host_unreachable",
            Self::Unavailable { .. } => "unavailable",
            Self::ProjectNotFound => "project_not_found",
            Self::AppNotFound => "app_not_found",
            Self::Invalid(_) => "invalid_request",
            Self::NotRegistered(_) => "account_not_registered",
            Self::ClientUnavailable(_) => "http_client_unavailable",
        }
    }

    /// The status this fault deserves. A caller error is a 4xx; a platform fault is a 502.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::NotRegistered(_) => StatusCode::NOT_FOUND,
            Self::ProjectNotFound | Self::AppNotFound => StatusCode::NOT_FOUND,
            Self::CredentialRejected => StatusCode::UNAUTHORIZED,
            Self::QuotaExhausted => StatusCode::PAYMENT_REQUIRED,
            Self::HostUnreachable(_) | Self::Unavailable { .. } => StatusCode::BAD_GATEWAY,
            Self::ClientUnavailable(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// One sentence a person can act on.
    pub fn message(&self) -> String {
        match self {
            Self::CredentialRejected => "Cerebrium rejected the service-account token. It is not an \
                 inference API key: the control plane wants a service account, and the two are not \
                 interchangeable."
                .to_owned(),
            Self::QuotaExhausted => {
                "Cerebrium refused the call on allowance: the project is out of plan quota, out of \
                 credit, or rate-limited."
                    .to_owned()
            }
            Self::HostUnreachable(detail) => {
                format!("No answer from {CONTROL_PLANE}: {detail}")
            }
            Self::Unavailable { status, detail } => {
                format!("Cerebrium answered {status}: {detail}")
            }
            Self::ProjectNotFound => {
                "Cerebrium does not know that project id for this token. Check the id, and that the \
                 service account belongs to that project."
                    .to_owned()
            }
            Self::AppNotFound => {
                "Cerebrium has no such app in that project. Register one with /attach, or deploy it \
                 first."
                    .to_owned()
            }
            Self::Invalid(detail) => detail.clone(),
            Self::NotRegistered(account) => format!(
                "No Cerebrium account registered as {account:?} in this process. Attach it first."
            ),
            Self::ClientUnavailable(detail) => {
                format!("The HTTP client could not be built: {detail}")
            }
        }
    }

    /// Structured extras, if any.
    pub fn details(&self) -> Vec<Value> {
        match self {
            Self::Unavailable { status, .. } => vec![json!({ "upstream_status": status })],
            Self::Invalid(_) => Vec::new(),
            _ => Vec::new(),
        }
    }
}

impl IntoResponse for CerebriumError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status(),
            Json(error_body(self.code(), self.message(), self.details())),
        )
            .into_response()
    }
}

/// Map a non-success control-plane status onto a fault class.
///
/// `404` is deliberately absent: only the caller knows whether "not found" meant the project or the
/// app, and a lane that guessed would send the operator to the wrong screen.
fn classify(status: u16, body: &str) -> CerebriumError {
    let detail = truncate(body);
    match status {
        401 | 403 => CerebriumError::CredentialRejected,
        402 | 429 => CerebriumError::QuotaExhausted,
        500..=599 => CerebriumError::Unavailable { status, detail },
        _ => CerebriumError::Unavailable { status, detail },
    }
}

/// Keep an upstream body diagnosable without letting it take over the response.
fn truncate(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "(empty body)".to_owned();
    }
    if trimmed.chars().count() <= MAX_DETAIL {
        return trimmed.to_owned();
    }
    let cut: String = trimmed.chars().take(MAX_DETAIL).collect();
    format!("{cut}…")
}

/// One control-plane request with the token attached — and never in a URL or a body.
fn control_request(method: Method, url: String, token: &str) -> QuotaRequest {
    QuotaRequest {
        method,
        url,
        headers: vec![("accept".to_owned(), "application/json".to_owned())],
        body: Default::default(),
        timeout: Some(CONTROL_TIMEOUT),
    }
    .bearer(token)
}

/// Send, and turn a transport failure into the class that names it.
async fn send(
    http: &dyn QuotaHttp,
    request: QuotaRequest,
) -> Result<mr_core::quota::http::QuotaResponse, CerebriumError> {
    http.request(request)
        .await
        .map_err(|error| CerebriumError::HostUnreachable(error.to_string()))
}

/// `POST /attach` — prove the token works, then remember it.
///
/// The verification is `GET /v2/projects/{project_id}/apps`, which needs the token *and* the
/// project: a 200 proves both. It also answers "is there already an app of this name?", which is
/// how the app id is learned, since the create response does not return one.
pub async fn attach_for(
    state: &CerebriumState,
    http: &dyn QuotaHttp,
    request: AttachRequest,
) -> Result<Json<Value>, CerebriumError> {
    let account = request.account.trim().to_owned();
    let project_id = request.project_id.trim().to_owned();
    let app_name = request.app_name.trim().to_owned();
    let token = request.token.trim().to_owned();

    if account.is_empty() {
        return Err(CerebriumError::Invalid(
            "account is required: it is the label every other route refers to this project by."
                .to_owned(),
        ));
    }
    if project_id.is_empty() {
        return Err(CerebriumError::Invalid("project_id is required.".to_owned()));
    }
    if token.is_empty() {
        return Err(CerebriumError::Invalid(
            "token is required: a Cerebrium service-account token, not an inference API key."
                .to_owned(),
        ));
    }
    if let Some(problem) = app_name_problem(&app_name) {
        return Err(CerebriumError::Invalid(problem));
    }

    let url = format!("{CONTROL_PLANE}/v2/projects/{project_id}/apps");
    let response = send(http, control_request(Method::GET, url, &token)).await?;

    match response.status {
        200..=299 => {}
        404 => return Err(CerebriumError::ProjectNotFound),
        status => return Err(classify(status, &String::from_utf8_lossy(&response.body))),
    }

    let body = response
        .json()
        .map_err(|error| CerebriumError::Unavailable {
            status: response.status,
            detail: format!("the app list was not JSON: {error}"),
        })?;
    let app_id = find_app_id(&body, &app_name);

    // The plan name is a courtesy, not a requirement: if the subscription call fails, the
    // registration is still correct and the operator still gets a working lane.
    let plan = read_plan(http, &project_id, &token).await;

    let registration = CerebriumRegistration {
        account: account.clone(),
        project_id,
        app_name,
        token,
        app_id,
        build_id: None,
        compute: None,
        disable_auth: None,
        plan,
        registered_at: Millis::now(),
        last_probe: Some(Millis::now().to_rfc3339()),
        last_probe_error: None,
    };
    let view = registration.view();
    state.lock().await.insert(account, registration);

    Ok(Json(json!({
        "lane": "cerebrium",
        "attached": view,
        "note": "The control plane answered, which proves the token and the project. It does not \
                 prove inference works: that needs a project API key, which this lane does not \
                 hold.",
    })))
}

/// Best-effort plan name. `None` when the platform does not volunteer one — never a guess.
async fn read_plan(http: &dyn QuotaHttp, project_id: &str, token: &str) -> Option<String> {
    let url = format!("{CONTROL_PLANE}/v2/projects/{project_id}/subscription");
    let response = http
        .request(control_request(Method::GET, url, token))
        .await
        .ok()?;
    if !(200..=299).contains(&response.status) {
        return None;
    }
    let body = response.json().ok()?;
    body.get("name")
        .or_else(|| body.get("plan"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Find an app by name in a `{items: […]}`-shaped list.
///
/// The field names are taken from the published schema (`id`, `name`); the fallbacks exist because
/// a list item is a large record and this lane only needs two of its keys, not because the names
/// are in doubt.
fn find_app_id(body: &Value, app_name: &str) -> Option<String> {
    let items = body
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| body.as_array())?;
    items
        .iter()
        .find(|item| item.get("name").and_then(Value::as_str) == Some(app_name))
        .and_then(|item| {
            item.get("id")
                .or_else(|| item.get("appId"))
                .or_else(|| item.get("app_id"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

/// The API's own rule: 3–30 characters, lowercase alphanumerics and dashes.
///
/// `mr_core`'s `app_name_is_safe` is looser (up to 64, underscores allowed) because it guards the
/// *renderer*; this one guards the API call, and a name the platform rejects is better refused here
/// with a reason than turned into a 400 with no explanation.
fn app_name_problem(app_name: &str) -> Option<String> {
    if app_name.is_empty() {
        return Some("app_name is required.".to_owned());
    }
    if !renderer_app_name_is_safe(app_name) {
        return Some(format!(
            "app_name {app_name:?} is not usable: use letters, digits, dashes and underscores."
        ));
    }
    if !(3..=30).contains(&app_name.chars().count()) {
        return Some(format!(
            "app_name {app_name:?} is {} characters; Cerebrium accepts 3 to 30.",
            app_name.chars().count()
        ));
    }
    if app_name.chars().any(|c| c.is_ascii_uppercase()) {
        return Some(format!(
            "app_name {app_name:?} contains upper case; Cerebrium app names are lower case."
        ));
    }
    if !app_name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Some(format!(
            "app_name {app_name:?} contains a character Cerebrium does not allow; use lower-case \
             letters, digits and dashes."
        ));
    }
    None
}

/// `POST /deploy` — step 1 of two: create the app record and hand back the upload URL.
///
/// The body is built here, from the operator's request, so that the fields which must not be
/// defaulted (`disableAuth`) cannot be defaulted by omission.
pub async fn deploy_for(
    state: &CerebriumState,
    http: &dyn QuotaHttp,
    request: DeployRequest,
) -> Result<Json<Value>, CerebriumError> {
    let (token, url, account, app_name) = {
        let registrations = state.lock().await;
        let registration = registrations
            .get(&request.account)
            .ok_or_else(|| CerebriumError::NotRegistered(request.account.clone()))?;
        (
            registration.token.clone(),
            registration.apps_url(),
            registration.account.clone(),
            registration.app_name.clone(),
        )
    };
    let record = deploy_record(&app_name, &request)?;

    let mut outbound = control_request(Method::POST, url, &token);
    outbound.headers.push((
        "content-type".to_owned(),
        "application/json".to_owned(),
    ));
    outbound.body = serde_json::to_vec(&record)
        .map_err(|error| CerebriumError::Invalid(format!("the request did not serialise: {error}")))?
        .into();

    let response = send(http, outbound).await?;
    match response.status {
        200..=299 => {}
        404 => return Err(CerebriumError::ProjectNotFound),
        status => return Err(classify(status, &String::from_utf8_lossy(&response.body))),
    }

    let body = response
        .json()
        .map_err(|error| CerebriumError::Unavailable {
            status: response.status,
            detail: format!("the create response was not JSON: {error}"),
        })?;
    let build_id = body
        .get("buildId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let upload_url = body
        .get("uploadUrl")
        .and_then(Value::as_str)
        .map(str::to_owned);

    {
        let mut registrations = state.lock().await;
        if let Some(registration) = registrations.get_mut(&account) {
            registration.build_id = build_id.clone();
            registration.compute = request.compute.clone().into();
            registration.disable_auth = request.disable_auth;
            registration.last_probe = Some(Millis::now().to_rfc3339());
            registration.last_probe_error = None;
        }
    }

    info!(
        account = %account,
        build = build_id.as_deref().unwrap_or("(none)"),
        "cerebrium: step 1 complete, source upload is step 2"
    );

    Ok(Json(json!({
        "lane": "cerebrium",
        "account": account,
        "step": 1,
        "of": 2,
        "build_id": build_id,
        "upload_url": upload_url,
        "status": body.get("status").cloned().unwrap_or(Value::Null),
        "dashboard_url": body.get("dashboardUrl").cloned().unwrap_or(Value::Null),
        "next": "PUT the project zip to upload_url with Content-Type: application/zip. The upload \
                 URL is pre-signed: send the bytes, not the token. This lane does not perform the \
                 upload, because the bundle renderer owns what goes in it.",
        "record_sent": {
            "name": record.get("name").cloned().unwrap_or(Value::Null),
            "compute": record.get("compute").cloned().unwrap_or(Value::Null),
            "gpuCount": record.get("gpuCount").cloned().unwrap_or(Value::Null),
            "disableAuth": record.get("disableAuth").cloned().unwrap_or(Value::Null),
            "cooldownPeriodSeconds": record.get("cooldownPeriodSeconds").cloned().unwrap_or(Value::Null),
            "replicaConcurrency": record.get("replicaConcurrency").cloned().unwrap_or(Value::Null),
        },
    })))
}

/// Build and validate the create-app record. No network, so this is the part tests can pin.
///
/// `app_name` comes from the registration, not from the deploy body: the app an account belongs to
/// is decided when it is attached, and a deploy that could rename it would silently create a second
/// app — and a second bill.
fn deploy_record(app_name: &str, request: &DeployRequest) -> Result<Value, CerebriumError> {
    if let Some(problem) = app_name_problem(app_name) {
        return Err(CerebriumError::Invalid(problem));
    }
    let compute = request.compute.trim();
    let Some(compute_type) = compute_by_id(compute) else {
        let known: Vec<&str> = mr_core::gpu::cerebrium::COMPUTE_TYPES
            .iter()
            .map(|entry| entry.id)
            .collect();
        return Err(CerebriumError::Invalid(format!(
            "compute {compute:?} is not a Cerebrium compute id. Valid ids: {}.",
            known.join(", ")
        )));
    };

    let gpu_count = request.gpu_count.unwrap_or(1);
    if gpu_count == 0 {
        return Err(CerebriumError::Invalid(
            "gpu_count 0 would deploy a CPU app that cannot serve a model; use at least 1.".to_owned(),
        ));
    }
    if gpu_count > compute_type.max_gpu_count {
        return Err(CerebriumError::Invalid(format!(
            "gpu_count {gpu_count} exceeds the {} per-app maximum for {} ({}).",
            compute_type.max_gpu_count, compute_type.id, compute_type.label
        )));
    }

    let Some(disable_auth) = request.disable_auth else {
        return Err(CerebriumError::Invalid(
            "disable_auth is required, and it is not a formality: Cerebrium defaults it to true, so \
             omitting it publishes the endpoint to the public internet with no credential — on a \
             GPU you pay for. Send disable_auth: false for an authenticated endpoint."
                .to_owned(),
        ));
    };
    if disable_auth && request.acknowledge_public_endpoint != Some(true) {
        return Err(CerebriumError::Invalid(
            "disable_auth: true publishes an unauthenticated endpoint that anyone who learns the \
             URL can use at your expense. Send acknowledge_public_endpoint: true if that is what \
             you want."
                .to_owned(),
        ));
    }

    if request.cerebrium_toml.trim().is_empty() {
        return Err(CerebriumError::Invalid(
            "cerebrium_toml is required: a deploy replaces the whole app record, so every key \
             omitted from it is reset to its default."
                .to_owned(),
        ));
    }

    let (min_cooldown, max_cooldown) = COOLDOWN_RANGE;
    let cooldown = request.cooldown_secs.unwrap_or(10);
    if !(min_cooldown..=max_cooldown).contains(&cooldown) {
        return Err(CerebriumError::Invalid(format!(
            "cooldown_secs {cooldown} is outside Cerebrium's range {min_cooldown}–{max_cooldown}."
        )));
    }

    let replica_concurrency = request.replica_concurrency.unwrap_or(1);
    if replica_concurrency == 0 {
        return Err(CerebriumError::Invalid(
            "replica_concurrency 0 would refuse every request; the platform's GPU default is 1, \
             which already serialises the server."
                .to_owned(),
        ));
    }

    let mut record = json!({
        "name": app_name,
        "cliVersion": CLI_VERSION,
        "cerebrium_toml": request.cerebrium_toml,
        "compute": [compute_type.id],
        "gpuCount": gpu_count,
        "disableAuth": disable_auth,
        "cooldownPeriodSeconds": cooldown,
        "replicaConcurrency": replica_concurrency,
        "runtime": "custom",
        "port": PORT,
    });
    if let Some(max_replicas) = request.max_replicas {
        record["maxReplicaCount"] = json!(max_replicas);
    }
    if let Some(entrypoint) = &request.entrypoint {
        record["entrypoint"] = json!(entrypoint);
    }
    Ok(record)
}

/// `POST /down` — delete the app, so the bill stops.
pub async fn down_for(
    state: &CerebriumState,
    http: &dyn QuotaHttp,
    request: DownRequest,
) -> Result<Json<Value>, CerebriumError> {
    let (token, apps_url, app_name, app_id) = {
        let registrations = state.lock().await;
        let registration = registrations
            .get(&request.account)
            .ok_or_else(|| CerebriumError::NotRegistered(request.account.clone()))?;
        (
            registration.token.clone(),
            registration.apps_url(),
            registration.app_name.clone(),
            registration.app_id.clone(),
        )
    };

    // The create response does not carry the app id, so a registration made by deploy alone has to
    // look itself up before it can delete anything.
    let app_id = match app_id {
        Some(app_id) => app_id,
        None => {
            let response = send(http, control_request(Method::GET, apps_url.clone(), &token)).await?;
            match response.status {
                200..=299 => {}
                404 => return Err(CerebriumError::ProjectNotFound),
                status => return Err(classify(status, &String::from_utf8_lossy(&response.body))),
            }
            let body = response.json().map_err(|error| CerebriumError::Unavailable {
                status: response.status,
                detail: format!("the app list was not JSON: {error}"),
            })?;
            find_app_id(&body, &app_name).ok_or(CerebriumError::AppNotFound)?
        }
    };

    let url = format!("{apps_url}/{app_id}");
    let response = send(http, control_request(Method::DELETE, url, &token)).await?;
    match response.status {
        200..=299 | 404 => {}
        status => return Err(classify(status, &String::from_utf8_lossy(&response.body))),
    }

    let mut registrations = state.lock().await;
    if let Some(registration) = registrations.get_mut(&request.account) {
        // Kept registered, with the app id cleared: the account is still valid, the app is gone.
        registration.app_id = None;
        registration.build_id = None;
        registration.disable_auth = None;
        registration.last_probe = Some(Millis::now().to_rfc3339());
        registration.last_probe_error = None;
    }

    Ok(Json(json!({
        "lane": "cerebrium",
        "account": request.account,
        "app_id": app_id,
        "deleted": true,
        "note": "The app record is gone, so it can no longer be billed or serve traffic. The \
                 registration is kept: the project and token are still valid.",
    })))
}

/// `POST /probe` — read the app's state from the control plane.
pub async fn probe_for(
    state: &CerebriumState,
    http: &dyn QuotaHttp,
    request: ProbeRequest,
) -> Result<Json<Value>, CerebriumError> {
    let (token, apps_url, app_name, app_id, base_url) = {
        let registrations = state.lock().await;
        let registration = registrations
            .get(&request.account)
            .ok_or_else(|| CerebriumError::NotRegistered(request.account.clone()))?;
        (
            registration.token.clone(),
            registration.apps_url(),
            registration.app_name.clone(),
            registration.app_id.clone(),
            registration.inference_base_url(),
        )
    };

    let url = match &app_id {
        Some(app_id) => format!("{apps_url}/{app_id}"),
        None => apps_url,
    };
    let response = send(http, control_request(Method::GET, url, &token)).await?;

    let state_name = match response.status {
        200..=299 => {
            let body = response.json().unwrap_or(Value::Null);
            match &app_id {
                Some(_) => body
                    .get("status")
                    .or_else(|| body.get("buildStatus"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                None => find_app_id(&body, &app_name).map(|_| "found".to_owned()),
            }
        }
        404 if app_id.is_some() => {
            let mut registrations = state.lock().await;
            if let Some(registration) = registrations.get_mut(&request.account) {
                registration.app_id = None;
                registration.last_probe_error = Some("the app no longer exists".to_owned());
            }
            return Err(CerebriumError::AppNotFound);
        }
        404 => return Err(CerebriumError::ProjectNotFound),
        status => {
            let detail = truncate(&String::from_utf8_lossy(&response.body));
            let mut registrations = state.lock().await;
            if let Some(registration) = registrations.get_mut(&request.account) {
                registration.last_probe_error = Some(detail.clone());
            }
            return Err(classify(status, &detail));
        }
    };

    {
        let mut registrations = state.lock().await;
        if let Some(registration) = registrations.get_mut(&request.account) {
            registration.last_probe = Some(Millis::now().to_rfc3339());
            registration.last_probe_error = None;
        }
    }

    Ok(Json(json!({
        "lane": "cerebrium",
        "account": request.account,
        "app_name": app_name,
        "app_id": app_id,
        "state": state_name,
        "base_url": base_url,
        "note": "This is the control plane's view. It says the app exists and what its build is \
                 doing; it does not prove that inference answers, which needs a project API key.",
    })))
}

/// `GET /status` — local truth only, so a panel refresh never waits on Cerebrium.
pub async fn status_for(state: &CerebriumState) -> Json<Value> {
    let registrations = state.lock().await;
    let mut accounts: Vec<Value> = registrations.values().map(CerebriumRegistration::view).collect();
    accounts.sort_by(|a, b| {
        a.get("account")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .cmp(b.get("account").and_then(Value::as_str).unwrap_or_default())
    });

    let deployed = registrations
        .values()
        .filter(|registration| registration.build_id.is_some())
        .count();

    let detail = if registrations.is_empty() {
        "No Cerebrium project is attached. Attach one with a service-account token: the control \
         plane needs that token, and inference needs a different key, a project API key."
            .to_owned()
    } else {
        format!(
            "{} project(s) attached, {deployed} with a build recorded. Status is local: it reports \
             what this process was told, not what the platform is doing right now.",
            registrations.len()
        )
    };

    Json(json!({
        "lane": "cerebrium",
        "ready": !registrations.is_empty(),
        "detail": detail,
        "accounts": accounts,
        "active_deployments": deployed,
        "facts": {
            "control_plane": CONTROL_PLANE,
            "inference_host": INFERENCE_HOST,
            "inference_path": "/v4/{project_id}/{app_name}/v1",
            "custom_runtime_port": PORT,
            "cooldown_default_secs": 10,
            "cooldown_range_secs": [COOLDOWN_RANGE.0, COOLDOWN_RANGE.1],
            "replica_concurrency_default_on_gpu": 1,
            "disable_auth_default": true,
            "free_plan_max_apps_hint": FREE_PLAN_MAX_APPS,
        },
        "warnings": [
            {
                "code": "disable-auth-defaults-open",
                "message": "Cerebrium's disableAuth defaults to true. A deploy that omits it \
                            publishes the endpoint with no credential, which is why /deploy \
                            requires the field and refuses true without an acknowledgement.",
            },
            {
                "code": "two-credentials",
                "message": "A service-account token authenticates the control plane; a project API \
                            key authenticates inference. Neither works on the other host, and a \
                            401 from the wrong host looks exactly like an expired key.",
            },
            {
                "code": "free-plan-app-cap-reported-not-enforced",
                "message": "Hobby Free includes three apps. This lane reports the cap but does not \
                            enforce it: it does not read the project's plan, and refusing a deploy \
                            on a project with room would be worse than letting the platform say no.",
            },
        ],
        "note": "ready means an account is registered, not that a model is serving. Use /probe for \
                 the platform's view and a real completion to prove inference.",
    }))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// The routes this module owns. Merged into the admin router by `api::routes`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/gpu/lanes/cerebrium/status", get(get_status))
        .route("/api/gpu/lanes/cerebrium/attach", post(post_attach))
        .route("/api/gpu/lanes/cerebrium/deploy", post(post_deploy))
        .route("/api/gpu/lanes/cerebrium/down", post(post_down))
        .route("/api/gpu/lanes/cerebrium/probe", post(post_probe))
}

async fn get_status(State(app): State<AppState>) -> Json<Value> {
    status_for(&app.cerebrium).await
}

async fn post_attach(
    State(app): State<AppState>,
    Json(body): Json<AttachRequest>,
) -> Result<Json<Value>, CerebriumError> {
    let http = shared_cerebrium_http().map_err(|error| CerebriumError::ClientUnavailable(error.to_string()))?;
    attach_for(&app.cerebrium, http.as_ref(), body).await
}

async fn post_deploy(
    State(app): State<AppState>,
    Json(body): Json<DeployRequest>,
) -> Result<Json<Value>, CerebriumError> {
    let http = shared_cerebrium_http().map_err(|error| CerebriumError::ClientUnavailable(error.to_string()))?;
    deploy_for(&app.cerebrium, http.as_ref(), body).await
}

async fn post_down(
    State(app): State<AppState>,
    Json(body): Json<DownRequest>,
) -> Result<Json<Value>, CerebriumError> {
    let http = shared_cerebrium_http().map_err(|error| CerebriumError::ClientUnavailable(error.to_string()))?;
    down_for(&app.cerebrium, http.as_ref(), body).await
}

async fn post_probe(
    State(app): State<AppState>,
    Json(body): Json<ProbeRequest>,
) -> Result<Json<Value>, CerebriumError> {
    let http = shared_cerebrium_http().map_err(|error| CerebriumError::ClientUnavailable(error.to_string()))?;
    probe_for(&app.cerebrium, http.as_ref(), body).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A `QuotaHttp` that answers from a script and records what it was asked.
    #[derive(Debug, Default)]
    struct FakeHttp {
        replies: Mutex<VecDeque<(u16, String)>>,
        seen: Mutex<Vec<QuotaRequest>>,
    }

    impl FakeHttp {
        fn with(replies: &[(u16, &str)]) -> Self {
            Self {
                replies: Mutex::new(replies.iter().map(|(s, b)| (*s, (*b).to_owned())).collect()),
                seen: Mutex::new(Vec::new()),
            }
        }

        async fn requests(&self) -> Vec<QuotaRequest> {
            self.seen.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl QuotaHttp for FakeHttp {
        async fn request(
            &self,
            request: QuotaRequest,
        ) -> CoreResult<mr_core::quota::http::QuotaResponse> {
            self.seen.lock().await.push(request);
            let reply = self.replies.lock().await.pop_front().unwrap_or((
                500,
                "{\"message\":\"the test script ran out of replies\"}".to_owned(),
            ));
            Ok(mr_core::quota::http::QuotaResponse {
                status: reply.0,
                headers: Vec::new(),
                body: reply.1.into(),
            })
        }
    }

    /// A `QuotaHttp` that fails the way a refused connection does.
    #[derive(Debug)]
    struct DeadHttp;

    #[async_trait::async_trait]
    impl QuotaHttp for DeadHttp {
        async fn request(
            &self,
            _request: QuotaRequest,
        ) -> CoreResult<mr_core::quota::http::QuotaResponse> {
            Err(mr_core::error::Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "connection refused",
            )))
        }
    }

    fn attach(account: &str) -> AttachRequest {
        AttachRequest {
            account: account.to_owned(),
            project_id: "prj_test".to_owned(),
            token: "sa-secret-token".to_owned(),
            app_name: "qwen3-coder".to_owned(),
        }
    }

    fn deploy(account: &str) -> DeployRequest {
        DeployRequest {
            account: account.to_owned(),
            cerebrium_toml: "[cerebrium.runtime.custom]\nport = 8000\n".to_owned(),
            compute: "ADA_L4".to_owned(),
            gpu_count: Some(1),
            disable_auth: Some(false),
            acknowledge_public_endpoint: None,
            cooldown_secs: None,
            replica_concurrency: Some(64),
            max_replicas: Some(2),
            entrypoint: Some(vec!["vllm".to_owned(), "serve".to_owned()]),
        }
    }

    #[tokio::test]
    async fn attach_proves_the_token_and_learns_the_app_id() {
        let http = FakeHttp::with(&[
            (200, r#"{"items":[{"id":"app_123","name":"qwen3-coder"}]}"#),
            (200, r#"{"name":"Hobby Free"}"#),
        ]);
        let state = CerebriumState::new();
        let response = attach_for(&state, &http, attach("primary"))
            .await
            .expect("attach should succeed");
        assert_eq!(response.0["attached"]["app_id"], "app_123");
        assert_eq!(response.0["attached"]["plan"], "Hobby Free");

        let seen = http.requests().await;
        assert_eq!(
            seen[0].url,
            "https://rest.cerebrium.ai/v2/projects/prj_test/apps"
        );
        assert!(seen[0]
            .headers
            .iter()
            .any(|(n, v)| n == "authorization" && v == "Bearer sa-secret-token"));
    }

    #[tokio::test]
    async fn attach_reports_a_missing_field_rather_than_calling_out() {
        let http = FakeHttp::with(&[]);
        let state = CerebriumState::new();
        let mut request = attach("primary");
        request.token = "   ".to_owned();
        let error = attach_for(&state, &http, request).await.unwrap_err();
        assert_eq!(error.code(), "invalid_request");
        assert!(http.requests().await.is_empty(), "nothing should be sent");
    }

    #[tokio::test]
    async fn attach_maps_a_rejected_token_and_an_unknown_project_apart() {
        let state = CerebriumState::new();

        let http = FakeHttp::with(&[(401, r#"{"message":"invalid token"}"#)]);
        assert_eq!(
            attach_for(&state, &http, attach("a")).await.unwrap_err(),
            CerebriumError::CredentialRejected
        );

        let http = FakeHttp::with(&[(404, r#"{"message":"no such project"}"#)]);
        assert_eq!(
            attach_for(&state, &http, attach("b")).await.unwrap_err(),
            CerebriumError::ProjectNotFound
        );
    }

    #[tokio::test]
    async fn a_refused_connection_is_host_unreachable_not_unavailable() {
        let state = CerebriumState::new();
        let error = attach_for(&state, &DeadHttp, attach("primary"))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "host_unreachable");
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn the_token_appears_in_no_response() {
        let http = FakeHttp::with(&[(200, r#"{"items":[]}"#)]);
        let state = CerebriumState::new();
        let _ = attach_for(&state, &http, attach("primary")).await.unwrap();

        let status = status_for(&state).await;
        let rendered = status.0.to_string();
        assert!(
            !rendered.contains("sa-secret-token"),
            "the service-account token must never be serialised: {rendered}"
        );
        assert!(rendered.contains("primary"));
    }

    #[test]
    fn deploy_refuses_to_omit_disable_auth() {
        let mut request = deploy("primary");
        request.disable_auth = None;
        let error = deploy_record("qwen3-coder", &request).unwrap_err();
        assert_eq!(error.code(), "invalid_request");
        assert!(
            error.message().contains("publishes the endpoint"),
            "the reason must name the consequence: {}",
            error.message()
        );
    }

    #[test]
    fn deploy_refuses_a_public_endpoint_without_acknowledgement() {
        let mut request = deploy("primary");
        request.disable_auth = Some(true);
        assert_eq!(
            deploy_record("qwen3-coder", &request).unwrap_err().code(),
            "invalid_request"
        );

        request.acknowledge_public_endpoint = Some(true);
        let record = deploy_record("qwen3-coder", &request).expect("acknowledged public endpoint is allowed");
        assert_eq!(record["disableAuth"], true);
    }

    #[test]
    fn deploy_names_the_valid_compute_ids_instead_of_guessing() {
        let mut request = deploy("primary");
        request.compute = "H100".to_owned();
        let error = deploy_record("qwen3-coder", &request).unwrap_err();
        assert!(
            error.message().contains("ADA_L4"),
            "the message must list real ids: {}",
            error.message()
        );
    }

    #[test]
    fn deploy_carries_the_fields_the_platform_defaults_dangerously() {
        let request = deploy("primary");
        let record = deploy_record("qwen3-coder", &request).unwrap();
        assert_eq!(record["disableAuth"], false);
        assert_eq!(record["runtime"], "custom");
        assert_eq!(record["port"], 8000);
        assert_eq!(record["replicaConcurrency"], 64);
        assert_eq!(record["cooldownPeriodSeconds"], 10);
        assert_eq!(record["gpuCount"], 1);
        assert_eq!(record["compute"][0], "ADA_L4");
        assert_eq!(record["cliVersion"], CLI_VERSION);
        assert_eq!(record["name"], "qwen3-coder");
    }

    #[test]
    fn deploy_refuses_a_cooldown_outside_the_documented_range() {
        let mut request = deploy("primary");
        request.cooldown_secs = Some(3601);
        assert_eq!(
            deploy_record("qwen3-coder", &request).unwrap_err().code(),
            "invalid_request"
        );
    }

    #[test]
    fn deploy_refuses_more_gpus_than_the_compute_allows() {
        let mut request = deploy("primary");
        // Every Cerebrium GPU type caps below this; the message must name the real maximum.
        request.gpu_count = Some(9);
        let error = deploy_record("qwen3-coder", &request).unwrap_err();
        assert!(error.message().contains("maximum"), "{}", error.message());
    }

    #[tokio::test]
    async fn deploy_on_an_unregistered_account_is_a_404_naming_it() {
        let http = FakeHttp::with(&[]);
        let state = CerebriumState::new();
        let error = deploy_for(&state, &http, deploy("ghost")).await.unwrap_err();
        assert_eq!(error.code(), "account_not_registered");
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn deploy_posts_the_validated_record_and_reports_the_upload_url() {
        let http = FakeHttp::with(&[
            (200, r#"{"items":[]}"#),
            (200, r#"{"name":"Hobby Free"}"#),
            (
                200,
                r#"{"buildId":"bld_9","uploadUrl":"https://s3.example/put","status":"building","dashboardUrl":"https://dashboard/x","internalEndpoint":"https://api.cerebrium.ai/v4/prj_test/qwen3-coder"}"#,
            ),
        ]);
        let state = CerebriumState::new();
        let _ = attach_for(&state, &http, attach("primary")).await.unwrap();

        let request = deploy("primary");
        let response = deploy_for(&state, &http, request).await.unwrap();
        assert_eq!(response.0["build_id"], "bld_9");
        assert_eq!(response.0["upload_url"], "https://s3.example/put");
        assert_eq!(response.0["step"], 1);

        let seen = http.requests().await;
        let post = seen.last().expect("the create call was made");
        assert_eq!(post.method, Method::POST);
        assert_eq!(
            post.url,
            "https://rest.cerebrium.ai/v2/projects/prj_test/apps"
        );
        let sent: Value = serde_json::from_slice(&post.body).expect("the body is JSON");
        assert_eq!(sent["disableAuth"], false);
        assert_eq!(sent["name"], "qwen3-coder");
    }

    #[tokio::test]
    async fn down_resolves_the_app_id_before_deleting() {
        let http = FakeHttp::with(&[
            (200, r#"{"items":[]}"#),
            (200, r#"{"name":"Hobby Free"}"#),
            (200, r#"{"items":[{"id":"app_55","name":"qwen3-coder"}]}"#),
            (200, ""),
        ]);
        let state = CerebriumState::new();
        let _ = attach_for(&state, &http, attach("primary")).await.unwrap();

        let response = down_for(
            &state,
            &http,
            DownRequest {
                account: "primary".to_owned(),
            },
        )
        .await
        .expect("down should succeed");
        assert_eq!(response.0["app_id"], "app_55");
        assert_eq!(response.0["deleted"], true);

        let seen = http.requests().await;
        let delete = seen.last().unwrap();
        assert_eq!(delete.method, Method::DELETE);
        assert!(delete.url.ends_with("/apps/app_55"), "{}", delete.url);
    }

    #[tokio::test]
    async fn down_on_an_unregistered_account_is_a_404() {
        let state = CerebriumState::new();
        let error = down_for(
            &state,
            &FakeHttp::with(&[]),
            DownRequest {
                account: "ghost".to_owned(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "account_not_registered");
    }

    #[tokio::test]
    async fn an_empty_lane_says_so_instead_of_claiming_readiness() {
        let state = CerebriumState::new();
        let status = status_for(&state).await;
        assert_eq!(status.0["ready"], false);
        assert!(status.0["detail"]
            .as_str()
            .unwrap()
            .contains("No Cerebrium project"));
        assert_eq!(status.0["accounts"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn the_error_envelope_is_the_one_the_panel_reads() {
        let body = error_body(
            CerebriumError::QuotaExhausted.code(),
            CerebriumError::QuotaExhausted.message(),
            Vec::new(),
        );
        assert!(body["error"]["code"].is_string());
        assert!(body["error"]["message"].is_string());
        assert!(body["error"]["details"].is_array());
    }

    #[test]
    fn app_names_follow_the_apis_own_rule() {
        assert!(app_name_problem("qwen3-coder").is_none());
        assert!(app_name_problem("ab").is_some(), "too short");
        assert!(app_name_problem("Qwen3").is_some(), "upper case");
        assert!(app_name_problem("qwen_3").is_some(), "underscore");
        assert!(
            app_name_problem(&"a".repeat(31)).is_some(),
            "longer than 30"
        );
    }
}
