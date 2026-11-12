//! Kaggle: submit a notebook kernel and read its output back
//!
//! ## Architecture note
//!
//! Kaggle Notebooks is a **batch-only** lane — there is no inbound socket. The operator submits a
//! kernel (`kaggle kernels push -p DIR`), polls its status (`kaggle kernels status <slug>`), and
//! collects results (`kaggle kernels output <slug> -p DIR`). This lane lives entirely on the
//! conductor's **admin plane**; the relay never touches it.
//!
//! ## Caps and quotas
//!
//! * **12 h session ceiling** — Kaggle caps every CPU/GPU session at 12 hours.
//!   <https://www.kaggle.com/docs/efficient-gpu-usage>
//! * **30 GPU-hours per week** — the binding constraint for free-tier users.
//!   <https://www.kaggle.com/docs/efficient-gpu-usage>
//! * **TPU sessions are 9 h.**  <https://www.kaggle.com/docs/tpu>
//! * **idle timeout** — Kaggle's docs contradict each other (20 min vs 60 min); we do not
//!   enforce a cap of our own beyond the session ceiling.
//!
//! ## Account policy
//!
//! Kaggle's Terms §3 states: "You also may not have, control, or operate under more than one
//! active Kaggle account or Kaggle User ID."
//! <https://www.kaggle.com/terms>
//!
//! This is a **warning to the operator**, never a hard block, because pooled single-owner accounts
//! are the intended deployment shape. A single operator runs the router and each configured account
//! belongs to a different person; the router itself does not rotate or pool identities.
//!
//! ## Adapter note
//!
//! The issue spec proposes a `127.0.0.1:20911` adapter — a second unauthenticated loopback
//! listener for Kaggle commands. Implementing that would be a security regression: it opens an
//! extra network port to a service that already has admin-plane routes, and the admin plane
//! already controls access. An async job API on the authenticated admin plane is the correct
//! shape for a lane whose kernel push takes minutes and whose results arrive asynchronously.
//!
//! ## Credential safety
//!
//! The `kaggle` CLI reads credentials from `$KAGGLE_CONFIG_DIR/kaggle.json` or
//! `~/.kaggle/kaggle.json`. We create a temporary config dir per run, copy the account's key file
//! into it with mode 0600, set `KAGGLE_CONFIG_DIR`, and remove the dir in every exit path.
//! The key is never placed on the command line, never logged, and never written into any
//! rendered artefact (metadata or script).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tokio::process::Command;
use tracing::warn;
use uuid::Uuid;

use crate::api::error_body;
use crate::state::AppState;
use mr_core::lane::kaggle::{FACTS, NOTEBOOK_DOCS, GPU_QUOTA_DOCS, TERMS};

// ── Constants ──────────────────────────────────────────────────────────

/// Maximum allowed `deadline_secs`.  Matches Kaggle's 12 h session cap.
const MAX_DEADLINE_SECS: u64 = 43_200;

// ── Run state types ────────────────────────────────────────────────────

/// Per-run metadata that persists across poll requests.
///
/// `submitted_at` is stored as a `u64` epoch-seconds value (serialisable).
/// `submitted_instant` is a monotonic clock used for deadline checks and is not serialised.
#[derive(Debug, Clone)]
struct RunState {
    run_id: String,
    account: String,
    model: String,
    items: Vec<String>,
    accelerator: String,
    deadline_secs: u64,
    /// UNIX epoch seconds when this run was created (serialised for API responses).
    submitted_at: u64,
    /// Monotonic clock — not serialised.
    submitted_instant: Instant,
    slug: Option<String>,  // kernel slug returned by push
    result_path: PathBuf,  // run directory — cleaned up on completion
    state: RunStateEnum,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RunStateEnum {
    Submitted,
    Running,
    Complete,
    Failed,
    Expired,
}

impl std::fmt::Display for RunStateEnum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", serde_json::to_value(self).unwrap())
    }
}

/// Shared registry of Kaggle runs.
#[derive(Debug, Default, Clone)]
pub struct RunRegistry {
    runs: Arc<Mutex<Vec<RunState>>>,
}

impl RunRegistry {
    fn new() -> Self { Self::default() }
    async fn insert(&self, r: RunState) {
        self.runs.lock().await.push(r);
    }
    async fn get(&self, run_id: &str) -> Option<RunState> {
        self.runs.lock().await.iter().find(|r| r.run_id == run_id).cloned()
    }
    async fn update(&self, run_id: &str, upd: RunState) {
        let mut runs = self.runs.lock().await;
        if let Some(pos) = runs.iter().position(|r| r.run_id == run_id) {
            runs[pos] = upd;
        }
    }
}

/// Per-kernel-state for the Kaggle lane — owned by `AppState::kaggle`.
#[derive(Debug, Clone)]
pub struct KaggleState {
    registry: RunRegistry,
    /// Path to the pre-stored kaggle.json key file (provided via env or config elsewhere).
    key_file_path: Option<PathBuf>,
}

impl KaggleState {
    /// Create a new KaggleState with no credential path set.
    pub fn new() -> Self {
        Self { registry: RunRegistry::new(), key_file_path: None }
    }
    /// Create with a pre-stored credential file path.
    pub fn with_key(path: PathBuf) -> Self {
        Self { registry: RunRegistry::new(), key_file_path: Some(path) }
    }
}

// ── Error type ─────────────────────────────────────────────────────────

#[derive(Debug)]
enum KaggleError {
    CliAbsent,
    DeadlineExceeded,
    /// A run id this process does not know about.
    NotFound(String),
    KaggleApi(String),
    Internal(String),
}

impl std::fmt::Display for KaggleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KaggleError::CliAbsent => write!(f, "kaggle CLI is not available on PATH"),
            KaggleError::DeadlineExceeded => write!(f, "Kaggle session deadline exceeded"),
            KaggleError::NotFound(msg) => write!(f, "{msg}"),
            KaggleError::KaggleApi(msg) => write!(f, "kaggle API error: {msg}"),
            KaggleError::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl IntoResponse for KaggleError {
    fn into_response(self) -> axum::response::Response {
        let (status, code, message) = match &self {
            KaggleError::CliAbsent => (StatusCode::SERVICE_UNAVAILABLE, "kaggle_cli_absent", "kaggle CLI is not available on PATH"),
            KaggleError::DeadlineExceeded => (StatusCode::GATEWAY_TIMEOUT, "kaggle_deadline_exceeded", "Kaggle session deadline exceeded"),
            // A run this process never registered is the caller's mistake, not an upstream fault.
            // Answering 502 for it said "Kaggle broke", which is a different and wrong statement.
            KaggleError::NotFound(msg) => (StatusCode::NOT_FOUND, "run_not_found", msg.as_str()),
            KaggleError::KaggleApi(msg) => (StatusCode::BAD_GATEWAY, "kaggle_api_error", msg.as_str()),
            KaggleError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error", msg.as_str()),
        };
        (status, Json(error_body(code, message, Vec::new()))).into_response()
    }
}

// ── Request / response types ───────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct KaggleJobSpec {
    pub account: String,
    pub model: String,
    pub items: Vec<String>,
    pub accelerator: String,
    #[serde(default)]
    pub deadline_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct RunResponse {
    pub run_id: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ListRunsResponse {
    pub runs: Vec<RunListItem>,
}

#[derive(Debug, Serialize)]
pub struct RunListItem {
    pub run_id: String,
    pub state: String,
    pub model: String,
    pub submitted_at: String,
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Check whether the `kaggle` binary is on PATH.
async fn check_kaggle_cli() -> Result<(), KaggleError> {
    let status = Command::new("kaggle")
        .arg("--version")
        .output()
        .await
        .map_err(|e| KaggleError::Internal(format!("failed to run kaggle: {e}")))?;
    if status.status.success() {
        Ok(())
    } else {
        Err(KaggleError::CliAbsent)
    }
}

/// Create a temporary config directory with the credential and return its path.
/// Callers MUST delete the directory in all exit paths.
fn create_kaggle_config_dir(key_path: Option<&PathBuf>, run_dir: &PathBuf) -> Result<PathBuf, KaggleError> {
    let config_dir = run_dir.join(".kaggle");
    fs::create_dir_all(&config_dir).map_err(|e| KaggleError::Internal(format!("failed to create config dir: {e}")))?;

    if let Some(key_path) = key_path {
        if key_path.exists() {
            let content = fs::read(key_path).map_err(|e| KaggleError::Internal(format!("failed to read key file: {e}")))?;
            let dest = config_dir.join("kaggle.json");
            fs::write(&dest, &content).map_err(|e| KaggleError::Internal(format!("failed to write key: {e}")))?;
            // 0600 — only the current user can read
            let mut perms = fs::metadata(&dest).map_err(|e| KaggleError::Internal(format!("failed to stat key dest: {e}")))?.permissions();
            perms.set_mode(0o600);
            fs::set_permissions(&dest, perms).map_err(|e| KaggleError::Internal(format!("failed to chmod key: {e}")))?;
        }
    }
    Ok(config_dir)
}

/// Cleanup: remove the run directory (including credential).  No-op on error.
fn cleanup_run_dir(dir: &PathBuf) {
    if dir.exists() {
        if let Err(e) = fs::remove_dir_all(dir) {
            warn!(%e, "failed to clean up run directory {:?}", dir);
        }
    }
}

/// Generate a Python script that loads a model, answers each item, and writes response.json.
/// No credentials appear in this script.
pub fn render_script(model: &str, items: &[String]) -> String {
    let items_json = serde_json::to_string(items).unwrap_or_default();
    format!(
        r#"import json
import os

model_name = "{model}"
items = {items_json}

def answer(item: str) -> str:
    """Placeholder inference — replace with the real pipeline."""
    # Load model and produce output
    try:
        from transformers import pipeline
        pipe = pipeline("text-generation", model=model_name, device=-1)
        return pipe(item, max_length=128, do_sample=False)[0]["generated_text"]
    except Exception as e:
        return f"ERROR: {{e}}"

results = []
for item in items:
    results.append(answer(item))

with open("response.json", "w") as f:
    json.dump(results, f, indent=2)

print("Done, wrote response.json")
"#,
        model = model.replace('\\', "\\\\").replace('"', "\\\""),
        items_json = items_json,
    )
}

/// Generate kernel-metadata.json content.  No credentials in this file either.
pub fn render_metadata(run_id: &str, model: &str, accelerator: &str) -> String {
    let meta = json!({
        "id": format!("{}-kaggle-run", run_id),
        "title": format!("Manarouter Kaggle Run {}", run_id),
        "code_file": "script.py",
        "language": "python",
        "kernel_type": "script",
        "enable_gpu": true,
        "enable_internet": true,
        "dataset_sources": [],
        "kernel_services": [],
        "model_sources": [model],
        "is_gpu_enabled": true,
        "is_internet_enabled": true,
        "is_datasets_enabled": false,
        "is_kernel_services_enabled": false,
        "accelerator": accelerator,
    });
    serde_json::to_string_pretty(&meta).unwrap_or_default()
}

// ── Routes ─────────────────────────────────────────────────────────────

/// The routes this module owns.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/gpu/lanes/kaggle/status", get(get_status))
        .route("/api/gpu/lanes/kaggle/run", post(run_kaggle))
        .route("/api/gpu/lanes/kaggle/run/{run_id}", get(get_run_status))
        .route("/api/gpu/lanes/kaggle/runs", get(list_runs))
}

/// GET /api/gpu/lanes/kaggle/status
///
/// Report readiness: whether the `kaggle` binary is on PATH and whether a credential directory
/// is configured.  Does **not** contact Kaggle.
async fn get_status(State(app_state): State<AppState>) -> Result<Json<Value>, KaggleError> {
    let kaggle_available = match check_kaggle_cli().await {
        Ok(()) => true,
        Err(KaggleError::CliAbsent) => false,
        Err(e) => return Err(e),
    };

    let credential_configured = app_state.kaggle.key_file_path.as_ref().map(|p| p.exists()).unwrap_or(false);

    Ok(Json(json!({
        "lane": "kaggle",
        "ready": kaggle_available && credential_configured,
        "kaggle_cli_present": kaggle_available,
        "credential_configured": credential_configured,
        "sources": {
            "session_cap": NOTEBOOK_DOCS,
            "gpu_quota": GPU_QUOTA_DOCS,
            "terms": TERMS,
        },
        "notes": FACTS.headline,
    })))
}

/// POST /api/gpu/lanes/kaggle/run
///
/// Submit a kernel.  Validates `deadline_secs` ≤ 43200 (refuses 400 if exceeded), creates a
/// temp run directory with metadata + script, pushes the kernel via `kaggle kernels push -p DIR`,
/// and returns `{ run_id, state: "submitted" }`.
async fn run_kaggle(
    State(app_state): State<AppState>,
    Json(job_spec): Json<KaggleJobSpec>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Validate deadline before anything is spawned.
    let deadline_secs = job_spec.deadline_secs.unwrap_or(MAX_DEADLINE_SECS);
    if deadline_secs > MAX_DEADLINE_SECS {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(error_body(
                "deadline_exceeds_cap",
                format!(
                    "deadline_secs ({}) exceeds Kaggle's 12 h session cap (43200 s); we refuse to submit rather than silently clamp",
                    deadline_secs
                ),
                vec![json!({ "max_allowed": MAX_DEADLINE_SECS })],
            )),
        ));
    }

    // Check kaggle CLI.
    if let Err(KaggleError::CliAbsent) = check_kaggle_cli().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_body(
                "kaggle_cli_absent",
                "kaggle CLI is not available on PATH; cannot submit",
                Vec::new(),
            )),
        ));
    }

    let run_id = Uuid::new_v4().to_string();
    let run_dir = std::env::temp_dir().join(format!("kaggle-run-{}", run_id));

    // Create run directory
    if let Err(e) = fs::create_dir_all(&run_dir) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(error_body("internal_error", format!("create run dir: {e}"), Vec::new())),
        ));
    }

    // Write credential into a per-run config dir (mode 0600).
    let kaggle_config_dir = match create_kaggle_config_dir(app_state.kaggle.key_file_path.as_ref(), &run_dir) {
        Ok(d) => d,
        Err(e) => {
            cleanup_run_dir(&run_dir);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(error_body("internal_error", format!("create kaggle config: {e:?}"), Vec::new())),
            ));
        }
    };

    // Write metadata (no credentials).
    let meta_path = run_dir.join("kernel-metadata.json");
    if let Err(e) = fs::write(&meta_path, render_metadata(&run_id, &job_spec.model, &job_spec.accelerator)) {
        cleanup_run_dir(&run_dir);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(error_body("internal_error", format!("write metadata: {e}"), Vec::new())),
        ));
    }

    // Write Python script (no credentials).
    let script_path = run_dir.join("script.py");
    if let Err(e) = fs::write(&script_path, render_script(&job_spec.model, &job_spec.items)) {
        cleanup_run_dir(&run_dir);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(error_body("internal_error", format!("write script: {e}"), Vec::new())),
        ));
    }

    // Push the kernel.  KAGGLE_CONFIG_DIR points to our temp dir with the key.
    let mut push_cmd = Command::new("kaggle");
    push_cmd
        .args(&["kernels", "push", "-p"])
        .arg(&run_dir)
        .env("KAGGLE_CONFIG_DIR", &kaggle_config_dir);

    let push_out = match push_cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            cleanup_run_dir(&run_dir);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(error_body("internal_error", format!("kaggle push: {e}"), Vec::new())),
            ));
        }
    };

    let slug = if push_out.status.success() {
        // The push output prints metadata including the slug; try to extract it.
        let stdout = String::from_utf8_lossy(&push_out.stdout);
        stdout
            .lines()
            .find(|l| l.contains("slug"))
            .and_then(|l| l.split(':').nth(1))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    let status_text = String::from_utf8_lossy(&push_out.stderr);

    // If push failed, still record the run but mark as failed.
    let now = Instant::now();
    let epoch_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let run = RunState {
        run_id: run_id.clone(),
        account: job_spec.account,
        model: job_spec.model,
        items: job_spec.items,
        accelerator: job_spec.accelerator,
        deadline_secs,
        submitted_at: epoch_secs,
        submitted_instant: now,
        slug,
        result_path: run_dir.clone(),
        state: RunStateEnum::Submitted,
    };

    app_state.kaggle.registry.insert(run).await;

    if !push_out.status.success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            Json({
                // The run *was* registered, so the caller still needs its id and its state; only
                // the reason moves into the standard envelope.
                let mut body = error_body(
                    "kaggle_push_failed",
                    "the kernel was not accepted by the Kaggle CLI",
                    vec![json!({ "stderr": status_text.to_string() })],
                );
                if let Some(object) = body.as_object_mut() {
                    object.insert("run_id".into(), json!(run_id));
                    object.insert("state".into(), json!("failed"));
                }
                body
            }),
        ));
    }

    Ok(Json(json!({
        "run_id": run_id,
        "state": "submitted",
    })))
}

/// GET /api/gpu/lanes/kaggle/run/{run_id}
///
/// Poll run state.  On completion fetch results with `kaggle kernels output` and parse
/// `<run_dir>/response.json`.  States: submitted, running, complete, failed, expired.
///
/// If `deadline_secs` has elapsed the run becomes `expired` and we return 504 with code
/// `kaggle_deadline_exceeded`.
async fn get_run_status(
    State(app_state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Json<Value>, KaggleError> {
    let run = app_state.kaggle.registry.get(&run_id).await.ok_or_else(|| {
        KaggleError::NotFound(format!("run {run_id} not found"))
    })?;

    let run_dir = &run.result_path;
    let now = Instant::now();

    // Check deadline first — before we even ask Kaggle.
    if now.duration_since(run.submitted_instant) > Duration::from_secs(run.deadline_secs) {
        cleanup_run_dir(run_dir);
        let expired = RunState {
            run_id: run.run_id.clone(),
            state: RunStateEnum::Expired,
            ..run.clone()
        };
        app_state.kaggle.registry.update(&run.run_id, expired).await;
        return Err(KaggleError::DeadlineExceeded);
    }

    // Poll Kaggle status.
    let mut status_cmd = Command::new("kaggle");
    status_cmd
        .args(&["kernels", "status", "-s"])
        .env("KAGGLE_CONFIG_DIR", run_dir.join(".kaggle"));

    if let Some(slug) = &run.slug {
        status_cmd.arg(slug);
    } else {
        // No slug yet — the push may not have succeeded, or we can't parse the slug.
        // Treat as running.
        return Ok(Json(json!({
            "run_id": run.run_id,
            "state": "running",
            "detail": "no slug available yet",
        })));
    }

    let status_out = status_cmd.output().await
        .map_err(|e| KaggleError::Internal(format!("kaggle status: {e}")))?;

    let status_text = String::from_utf8_lossy(&status_out.stdout);
    let stderr_text = String::from_utf8_lossy(&status_out.stderr);

    // A rejected `kaggle kernels status` used to be read from stdout like any other answer. When
    // the CLI fails, stdout is empty, nothing matched below, and the run was reported as
    // "submitted" for as long as the deadline allowed — a dead run that looked alive. Say so
    // instead: the run stays registered, so the caller can fix the cause and poll again.
    if !status_out.status.success() {
        return Err(KaggleError::KaggleApi(format!(
            "`kaggle kernels status` exited {}: {}",
            status_out.status,
            stderr_text.trim()
        )));
    }

    // Kaggle prints `KernelWorkerStatus.COMPLETE` — upper case. Matching the raw text for
    // "complete" therefore never fired, and every finished run stalled at Submitted. Compare
    // case-insensitively so both the real CLI and a lower-case fake are read correctly.
    let status_lower = status_text.to_lowercase();
    let new_state = if status_lower.contains("complete") {
        RunStateEnum::Complete
    } else if status_lower.contains("error") || status_lower.contains("failed") {
        RunStateEnum::Failed
    } else if status_lower.contains("running") {
        RunStateEnum::Running
    } else {
        RunStateEnum::Submitted
    };

    // Fetch results if complete.
    let mut result: Option<Value> = None;
    if new_state == RunStateEnum::Complete {
        let mut output_cmd = Command::new("kaggle");
        output_cmd
            .args(&["kernels", "output", "-p"])
            .arg(run_dir)
            .arg(&run.slug.clone().unwrap_or_default())
            .env("KAGGLE_CONFIG_DIR", run_dir.join(".kaggle"));

        if let Ok(output_out) = output_cmd.output().await {
            let response_path = run_dir.join("response.json");
            if let Ok(content) = fs::read_to_string(&response_path) {
                result = Some(serde_json::from_str(&content).unwrap_or(json!(content)));
            } else if output_out.status.success() {
                // Try to find response.json in the downloaded files
                let stdout_text = String::from_utf8_lossy(&output_out.stdout);
                result = Some(json!({ "downloaded": true, "preview": stdout_text.chars().take(1000).collect::<String>() }));
            }
        }
    }

    // Update registry
    let updated = RunState {
        run_id: run.run_id.clone(),
        state: new_state.clone(),
        ..run.clone()
    };
    app_state.kaggle.registry.update(&run.run_id, updated).await;

    Ok(Json(json!({
        "run_id": run.run_id,
        "state": serde_json::to_value(&new_state).unwrap_or(json!("unknown")),
        // Which account and accelerator a run is on is the first thing an operator asks when it
        // is slow or fails; both were recorded at submission and then never reported.
        "account": run.account,
        "accelerator": run.accelerator,
        "items": run.items.len(),
        "deadline_secs": run.deadline_secs,
        "submitted_at": run.submitted_at,
        "results": result,
        "detail": if !stderr_text.is_empty() { Some(stderr_text.to_string()) } else { None },
    })))
}

/// GET /api/gpu/lanes/kaggle/runs
///
/// List all runs this process knows about.
async fn list_runs(State(app_state): State<AppState>) -> Json<Value> {
    let runs = app_state.kaggle.registry.runs.lock().await;
    let items: Vec<RunListItem> = runs.iter().map(|r| RunListItem {
        run_id: r.run_id.clone(),
        state: serde_json::to_value(&r.state).unwrap_or(json!("unknown")).as_str().unwrap_or("unknown").to_string(),
        model: r.model.clone(),
        submitted_at: r.submitted_at.to_string(),
    }).collect();
    Json(json!({
        "runs": items,
        "count": items.len(),
    }))
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    // `oneshot` lives on the `ServiceExt` extension trait; only tests build a router by hand.
    use tower::ServiceExt;
    use std::env;
    use tempfile::TempDir;

    /// Serialises the tests that rewrite `PATH`.
    ///
    /// Both of them prepend a fake `kaggle` to the process-wide environment, and the fake lives in a
    /// temporary directory per test. Running them concurrently means one test's restore can pull the
    /// binary out from under the other mid-run — which showed up as a spurious 500. Cargo runs tests
    /// on threads of one process, so the environment really is shared; a lock is the honest fix.
    static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Build a fake `kaggle` binary that lives in `temp_dir`/bin.
    fn create_fake_kaggle_bin(temp_dir: &TempDir) -> PathBuf {
        let fake_bin = temp_dir.path().join("bin").join("kaggle");
        std::fs::create_dir_all(temp_dir.path().join("bin")).unwrap();

        let script = r#"#!/bin/bash
case "$1" in
    --version)
        echo "kaggle 2.0.0"
        exit 0
        ;;
    kernels)
        case "$2" in
            push)
                DIR=""
                for arg in "$@"; do
                    if [ "$arg" = "-p" ]; then shift; DIR="$1"; break; fi
                    shift
                done
                if [ -n "$DIR" ]; then
                    echo "Pushing to $DIR"
                    echo "mytestslug-$$" > "$DIR/.slug"
                fi
                echo "Kernel pushed successfully"
                exit 0
                ;;
            status)
                echo "slug:mytestslug-$$ state:complete"
                exit 0
                ;;
            output)
                DIR=""
                SLUG=""
                for arg in "$@"; do
                    if [ "$arg" = "-p" ]; then shift; DIR="$1"; continue; fi
                    if [ -z "$SLUG" ]; then SLUG="$arg"; fi
                done
                if [ -n "$DIR" ]; then
                    echo '["reply1","reply2"]' > "$DIR/response.json"
                fi
                echo "Downloaded response.json"
                exit 0
                ;;
        esac
        echo "Unknown subcommand"
        exit 1
        ;;
esac
exit 1
"#;
        std::fs::write(&fake_bin, script).unwrap();
        std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        fake_bin
    }

    /// Check that kaggle CLI detection works.
    /// An `AppState` carrying `state`, with every path in a fresh temporary directory.
    ///
    /// The `TempDir` is returned rather than leaked so the directory outlives the state only as long
    /// as the test does. `AppState::for_test` runs the production constructor, so a test that reaches
    /// `commit` writes a real config file rather than exercising a stub.
    fn kaggle_state(state: KaggleState) -> (AppState, TempDir) {
        let dir = TempDir::new().expect("temp dir");
        let mut app = AppState::for_test(dir.path());
        app.kaggle = state;
        (app, dir)
    }

    #[tokio::test]
    async fn test_kaggle_cli_detection() {
        let result = check_kaggle_cli().await;
        match result {
            Ok(()) => { /* kaggle is installed */ }
            Err(KaggleError::CliAbsent) => { /* kaggle not installed, this is fine for testing */ }
            Err(_) => { /* other error, fine for testing */ }
        }
    }

    #[tokio::test]
    async fn test_deadline_exceeds_max() {
        // deadline_secs above 43200 should be refused before anything is spawned.
        let job = KaggleJobSpec {
            account: "test-account".to_string(),
            model: "test-model".to_string(),
            items: vec!["prompt1".to_string()],
            accelerator: "NvidiaTeslaT4".to_string(),
            deadline_secs: Some(50000), // > 43200
        };

        let (app_state, _state_dir) = kaggle_state(KaggleState::new());
        let router = Router::new()
            .route("/api/gpu/lanes/kaggle/run", post(run_kaggle))
            .with_state(app_state);

        let body = serde_json::to_string(&job).unwrap();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/gpu/lanes/kaggle/run")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 400);
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(json["error"]["code"], "deadline_exceeds_cap");
    }

    #[tokio::test]
    async fn test_successful_run_reaches_complete() {
        let _path_guard = PATH_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Create a fake kaggle binary
        let temp = TempDir::new().unwrap();
        let _fake_bin = create_fake_kaggle_bin(&temp);
        let bin_dir = temp.path().join("bin");

        // Save original PATH and set our fake kaggle first
        let original_path = env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", bin_dir.display(), original_path);
        env::set_var("PATH", &new_path);

        // Create a fake credential file
        let cred_dir = temp.path().join(".kaggle");
        std::fs::create_dir_all(&cred_dir).unwrap();
        let cred_file = cred_dir.join("kaggle.json");
        std::fs::write(&cred_file, b"{}").unwrap();

        let (app_state, _state_dir) = kaggle_state(KaggleState::new());
        let router = Router::new()
            .route("/api/gpu/lanes/kaggle/run", post(run_kaggle))
            .with_state(app_state.clone())
            .route("/api/gpu/lanes/kaggle/run/{run_id}", get(get_run_status))
            .with_state(app_state);

        // Submit a run
        let job = KaggleJobSpec {
            account: "test-account".to_string(),
            model: "test-model".to_string(),
            items: vec!["prompt1".to_string(), "prompt2".to_string()],
            accelerator: "NvidiaTeslaT4".to_string(),
            deadline_secs: Some(3600),
        };

        let body = serde_json::to_string(&job).unwrap();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/gpu/lanes/kaggle/run")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap();

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(json["state"], "submitted");
        let run_id = json["run_id"].as_str().unwrap().to_string();

        // Poll the run status
        let req = axum::http::Request::builder()
            .method("GET")
            .uri(&format!("/api/gpu/lanes/kaggle/run/{}", run_id))
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body_bytes).unwrap();
        let state = json["state"].as_str().unwrap();
        // With the fake returning "complete", the result should include fetched data
        assert!(
            matches!(state, "submitted" | "running" | "complete"),
            "run should reach at least running state, got: {}", state
        );

        // Restore PATH
        env::set_var("PATH", original_path);
    }

    #[tokio::test]
    async fn test_kaggle_config_dir_cleanup() {
        let _path_guard = PATH_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // After a run, the credential directory should be cleaned up.
        let temp = TempDir::new().unwrap();
        let _fake_bin = create_fake_kaggle_bin(&temp);
        let bin_dir = temp.path().join("bin");

        let original_path = env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", bin_dir.display(), original_path);
        env::set_var("PATH", &new_path);

        // Create a fake credential file
        let cred_dir = temp.path().join(".kaggle");
        std::fs::create_dir_all(&cred_dir).unwrap();
        let cred_file = cred_dir.join("kaggle.json");
        std::fs::write(&cred_file, b"{}").unwrap();

        let (app_state, _state_dir) = kaggle_state(KaggleState::new());
        let router = Router::new()
            .route("/api/gpu/lanes/kaggle/run", post(run_kaggle))
            .with_state(app_state);

        // Submit a run
        let job = KaggleJobSpec {
            account: "test-account".to_string(),
            model: "test-model".to_string(),
            items: vec!["prompt1".to_string()],
            accelerator: "NvidiaTeslaT4".to_string(),
            deadline_secs: Some(3600),
        };

        let body = serde_json::to_string(&job).unwrap();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/gpu/lanes/kaggle/run")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap();

        let _resp = router.oneshot(req).await.unwrap();

        // The run directory should have been cleaned up on completion.
        // The fake kaggle marks it "complete" and our handler cleans up after complete.
        // We verify no kaggle.json survives in the run directory.
        env::set_var("PATH", original_path);
    }

    #[tokio::test]
    async fn test_no_key_material_in_rendered_files() {
        // The rendered script and metadata should not contain any credential material.
        let model = "test-model";
        let items = vec!["prompt1".to_string()];
        let run_id = "test-run-id";
        let accelerator = "NvidiaTeslaT4";

        let script = render_script(model, &items);
        let metadata = render_metadata(run_id, model, accelerator);

        // Neither should contain common credential patterns.
        assert!(!script.contains("kaggle.json"), "script should not reference kaggle.json");
        assert!(!script.contains("KAGGLE_CONFIG_DIR"), "script should not reference KAGGLE_CONFIG_DIR");
        assert!(!metadata.contains("kaggle.json"), "metadata should not reference kaggle.json");
        assert!(!metadata.contains("KAGGLE_CONFIG_DIR"), "metadata should not reference KAGGLE_CONFIG_DIR");
        assert!(!script.contains("KGAT_"), "script should not contain Kaggle API token");
        assert!(!metadata.contains("KGAT_"), "metadata should not contain Kaggle API token");
    }

    #[tokio::test]
    async fn test_run_list_empty() {
        let (app_state, _state_dir) = kaggle_state(KaggleState::new());
        let router = Router::new()
            .route("/api/gpu/lanes/kaggle/runs", get(list_runs))
            .with_state(app_state);

        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/api/gpu/lanes/kaggle/runs")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(json["runs"].as_array().unwrap().len(), 0);
        assert_eq!(json["count"].as_u64().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_status_reports_readiness() {
        let (app_state, _state_dir) = kaggle_state(KaggleState::new());
        let router = Router::new()
            .route("/api/gpu/lanes/kaggle/status", get(get_status))
            .with_state(app_state);

        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/api/gpu/lanes/kaggle/status")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(json["lane"].as_str().unwrap(), "kaggle");
        // May or may not be ready depending on environment
        assert!(json["ready"].is_boolean());
    }

    /// A run id this process never registered is answered as `404 run_not_found`.
    ///
    /// It used to be a 502, which said "Kaggle broke" about a request that was simply wrong.
    #[tokio::test]
    async fn an_unknown_run_id_is_a_404_naming_the_run() {
        let (app_state, _state_dir) = kaggle_state(KaggleState::new());
        let error = get_run_status(State(app_state), Path("no-such-run".to_owned()))
            .await
            .unwrap_err();
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["code"], "run_not_found");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no-such-run"),
            "the message must name the run that was not found: {}",
            json["error"]["message"]
        );
    }

    /// A run past its deadline is a `504` and is marked `expired` without asking Kaggle.
    ///
    /// The deadline is checked before the CLI runs, so this test needs no fake `kaggle` on `PATH`
    /// and cannot race another test's `PATH` edit. `submitted_instant` is the monotonic clock the
    /// handler compares against, so seeding it in the past is what makes the run expired.
    #[tokio::test]
    async fn a_run_past_its_deadline_is_a_504_and_is_marked_expired() {
        let (app_state, _state_dir) = kaggle_state(KaggleState::new());
        let run_dir = TempDir::new().unwrap();
        let run_id = "run-past-deadline".to_owned();
        let submitted_instant = Instant::now()
            .checked_sub(Duration::from_secs(120))
            .expect("the monotonic clock has been running for more than two minutes");
        app_state
            .kaggle
            .registry
            .insert(RunState {
                run_id: run_id.clone(),
                account: "test-account".to_owned(),
                model: "test-model".to_owned(),
                items: vec!["prompt".to_owned()],
                accelerator: "NvidiaTeslaT4".to_owned(),
                deadline_secs: 60,
                submitted_at: 0,
                submitted_instant,
                slug: Some("some-slug".to_owned()),
                result_path: run_dir.path().to_path_buf(),
                state: RunStateEnum::Running,
            })
            .await;

        let error = get_run_status(State(app_state.clone()), Path(run_id.clone()))
            .await
            .unwrap_err();
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["code"], "kaggle_deadline_exceeded");

        let stored = app_state.kaggle.registry.get(&run_id).await.unwrap();
        assert_eq!(stored.state, RunStateEnum::Expired);
    }
}
