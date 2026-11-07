//! OpenCode Go plan usage.
//!
//! # Only the Go tier reports usage
//!
//! `opencode.ai` serves three products from one host: the free tier and the metered Zen tier both
//! under `/zen/v1`, and the Go subscription under `/zen/go/v1`. Only the Go host answers
//! `GET /usage`; the other two return 404, and the metered tier's real accounting lives on the
//! console's budgets page, which has no public endpoint. So this reader matches the Go provider id
//! exactly and deliberately does **not** claim `opencode.ai` through
//! [`QuotaFetcher::claims_host`]: a host claim would fire for the free and Zen providers as well
//! and put a 404 in front of an operator as though it were a rejected key.
//!
//! # Wire shape
//!
//! ```text
//! {"usage":{"rolling":{"status":"ok","percent":0,"resetsAt":"2026-11-07T04:43:42.000Z"},
//!           "weekly":{...},"monthly":{...}}}
//! ```
//!
//! `percent` is the share of that window **spent**, and each window resets on its own schedule.
//! A missing window is left out rather than invented, and a window with no `percent` is skipped:
//! the plan always answers with all three today, but an upstream is free to omit a field it has no
//! number for, and a fabricated `0` reads as a full allowance.
//!
//! # One quota, three windows
//!
//! [`UsageQuota`] describes one window, so [`OpenCodeGo::fetch`] reports the most-binding of the
//! three and lists all of them as [`QuotaDetail`] rows — the same shape the GLM reader uses, so
//! the panel renders a Go account the way it renders every other multi-window plan.

use async_trait::async_trait;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::QuotaRequest;
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::{parse_reset_time, QuotaDetail, UsageQuota};

/// Fetcher id, also stamped onto [`UsageQuota::source`].
pub const ID: &str = "opencode-go";

/// Where the Go plan reports its windows, used when the provider carries no base URL.
pub const DEFAULT_USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";

/// The windows this plan meters, shortest first.
pub const WINDOW_KEYS: [&str; 3] = ["rolling", "weekly", "monthly"];

/// The OpenCode Go subscription quota reader.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenCodeGo;

impl OpenCodeGo {
    /// A stateless fetcher; the credential comes from the account on each fetch.
    pub fn new() -> Self {
        Self
    }
}

/// The usage endpoint for a provider's configured base URL.
///
/// Appending `/usage` is the whole derivation because the base URL the operator stores is the same
/// host and prefix the chat endpoint lives under (`https://opencode.ai/zen/go/v1`), so no vendor
/// constant has to be kept in step with a config value.
pub fn usage_url(base_url: Option<&str>) -> String {
    let Some(base) = base_url.map(str::trim).filter(|base| !base.is_empty()) else {
        return DEFAULT_USAGE_URL.to_owned();
    };
    let base = base.trim_end_matches('/');
    if base.ends_with("/usage") {
        return base.to_owned();
    }
    format!("{base}/usage")
}

/// The window keys the payload actually carries, with their used percentage.
///
/// Pure, so tests drive it with captured JSON instead of a clock. `percent` is read as *used*; a
/// value outside `0..=100` is clamped rather than trusted, because a negative remainder would
/// render as more than the whole allowance.
pub fn parse_windows(body: &Value) -> Vec<(&'static str, UsageQuota)> {
    let usage = body.get("usage").unwrap_or(body);
    let mut windows = Vec::new();
    for key in WINDOW_KEYS {
        let Some(window) = usage.get(key) else {
            continue;
        };
        let Some(percent) = window.get("percent").and_then(Value::as_f64) else {
            continue;
        };
        if !percent.is_finite() {
            continue;
        }
        let reset = window.get("resetsAt").cloned().unwrap_or(Value::Null);
        windows.push((
            key,
            UsageQuota::from_counts(percent.clamp(0.0, 100.0), 100.0, parse_reset_time(&reset)),
        ));
    }
    windows
}

/// The duration label for a window key.
///
/// Taken from the key, not measured from the reset instant: the distance to a reset stops being a
/// lower bound on the window once the window is nearly spent, so a monthly allowance with three
/// days left would be presented as a weekly one. The key is what upstream declares it meters.
fn window_label(key: &str) -> &'static str {
    match key {
        "rolling" => "5h",
        "weekly" => "1w",
        "monthly" => "30d",
        _ => "other",
    }
}

/// The operator-facing name of the window the account is closest to exhausting.
fn display_name(key: &str) -> &'static str {
    match key {
        "rolling" => "Rolling (5 hours)",
        "weekly" => "Weekly",
        "monthly" => "Monthly",
        _ => "Quota",
    }
}

/// One [`UsageQuota`] for the account: the most-binding window, every window as a detail row.
///
/// "Most binding" is the largest used percentage. Ties go to the shorter window, because the same
/// percentage of a five-hour allowance blocks sooner than of a monthly one.
fn assemble(windows: Vec<(&'static str, UsageQuota)>) -> UsageQuota {
    let primary = windows
        .iter()
        .enumerate()
        .max_by(|(left_index, (_, left)), (right_index, (_, right))| {
            left.used
                .partial_cmp(&right.used)
                .unwrap_or(std::cmp::Ordering::Equal)
                // `max_by` keeps the later element on a tie, so an equal percentage resolves
                // towards the earlier (shorter) window by inverting the index comparison.
                .then_with(|| right_index.cmp(left_index))
        })
        .map(|(_, (key, quota))| (*key, quota.clone()))
        .expect("assemble is only called with at least one window");

    let mut quota = primary.1.with_source(ID);
    quota.display_name = Some(display_name(primary.0).to_owned());
    for (key, detail) in &windows {
        quota = quota.with_detail(QuotaDetail {
            name: format!("{key} ({})", window_label(key)),
            used: detail.used,
        });
    }
    quota
}

#[async_trait]
impl QuotaFetcher for OpenCodeGo {
    fn id(&self) -> &'static str {
        ID
    }

    fn supports(&self, _dialect: Dialect, provider_id: &str, _account: &QuotaAccount) -> bool {
        provider_id == ID
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let credential = ctx
            .credential
            .as_ref()
            .filter(|credential| !credential.is_empty())
            .ok_or_else(|| {
                Error::Config(format!("provider {:?} has no credential; the OpenCode /usage endpoint needs a bearer key", ctx.provider_id))
            })?;

        let url = usage_url(ctx.base_url.as_deref());
        let request = QuotaRequest::get(url.clone())
            .bearer(credential.expose())
            .header("Accept", "application/json");

        let response = ctx.http.request(request).await?;
        if !response.is_success() {
            return Err(Error::InvalidState(format!(
                "OpenCode usage endpoint {url} returned HTTP {}",
                response.status
            )));
        }

        let payload: Value = serde_json::from_str(&response.body_str()).map_err(|error| {
            Error::InvalidState(format!(
                "OpenCode usage endpoint {url} did not answer with JSON: {error}"
            ))
        })?;

        let windows = parse_windows(&payload);
        if windows.is_empty() {
            return Err(Error::NotFound(format!(
                "OpenCode usage endpoint {url} reported no usage windows"
            )));
        }
        Ok(assemble(windows))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;

    use super::*;
    use crate::quota::http::{QuotaHttp, QuotaResponse};

    /// One rolling/weekly/monthly answer, captured from the live Go endpoint.
    const PAYLOAD: &str = r#"{"usage":{"rolling":{"status":"ok","percent":0,"resetsAt":"2026-11-07T04:43:42.000Z"},"weekly":{"status":"ok","percent":12,"resetsAt":"2026-11-09T00:00:00.000Z"},"monthly":{"status":"ok","percent":40,"resetsAt":"2026-12-06T22:50:30.000Z"}}}"#;

    #[derive(Debug, Default)]
    struct FakeHttp {
        replies: Mutex<Vec<QuotaResponse>>,
        requests: Mutex<Vec<QuotaRequest>>,
    }

    impl FakeHttp {
        fn new(reply: QuotaResponse) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(vec![reply]),
                requests: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl QuotaHttp for FakeHttp {
        async fn request(&self, request: QuotaRequest) -> Result<QuotaResponse> {
            self.requests.lock().unwrap().push(request);
            Ok(self.replies.lock().unwrap().remove(0))
        }
    }

    fn response(status: u16, body: &str) -> QuotaResponse {
        QuotaResponse {
            status,
            headers: vec![],
            body: Bytes::from(body.to_owned()),
        }
    }

    fn context(http: Arc<dyn QuotaHttp>) -> QuotaFetchContext {
        QuotaFetchContext::new(ID, Dialect::OpenAi, http)
            .with_base_url("https://opencode.ai/zen/go/v1")
            .with_credential("oc_test_key")
    }

    #[test]
    fn the_usage_url_is_the_base_url_plus_usage() {
        assert_eq!(
            usage_url(Some("https://opencode.ai/zen/go/v1")),
            "https://opencode.ai/zen/go/v1/usage"
        );
        assert_eq!(
            usage_url(Some("https://opencode.ai/zen/go/v1/")),
            "https://opencode.ai/zen/go/v1/usage"
        );
        assert_eq!(
            usage_url(Some("https://opencode.ai/zen/go/v1/usage")),
            "https://opencode.ai/zen/go/v1/usage"
        );
        assert_eq!(usage_url(None), DEFAULT_USAGE_URL);
        assert_eq!(usage_url(Some("   ")), DEFAULT_USAGE_URL);
    }

    #[test]
    fn percent_is_read_as_used_and_every_window_keeps_its_own_reset() {
        let windows = parse_windows(&serde_json::from_str(PAYLOAD).unwrap());
        assert_eq!(
            windows.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            vec!["rolling", "weekly", "monthly"]
        );
        let (_, weekly) = windows.iter().find(|(key, _)| *key == "weekly").unwrap();
        assert_eq!(weekly.used, 12.0);
        assert_eq!(weekly.total, 100.0);
        assert_eq!(weekly.remaining, 88.0);
        assert_eq!(weekly.remaining_percentage, 88.0);
        assert!(weekly.fraction_reported);
        assert_eq!(
            weekly.reset_at,
            parse_reset_time(&serde_json::json!("2026-11-09T00:00:00.000Z"))
        );
    }

    #[test]
    fn an_absent_or_numberless_window_is_not_invented() {
        let body = serde_json::json!({"usage": {"rolling": {"status": "ok", "percent": 5}}});
        let windows = parse_windows(&body);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].0, "rolling");
        assert_eq!(windows[0].1.reset_at, None);

        let body = serde_json::json!({"usage": {"weekly": {"status": "ok"}}});
        assert!(parse_windows(&body).is_empty());
    }

    #[test]
    fn a_percent_outside_the_range_is_clamped_not_trusted() {
        let body = serde_json::json!({"usage": {"monthly": {"percent": 140}}});
        let quota = &parse_windows(&body)[0].1;
        assert_eq!(quota.used, 100.0);
        assert_eq!(quota.remaining, 0.0);
    }

    #[test]
    fn the_most_spent_window_is_the_one_reported_and_all_three_stay_in_details() {
        let windows = parse_windows(&serde_json::from_str(PAYLOAD).unwrap());
        let quota = assemble(windows);
        assert_eq!(quota.display_name.as_deref(), Some("Monthly"));
        assert_eq!(quota.used, 40.0);
        assert_eq!(quota.remaining_percentage, 60.0);
        assert_eq!(quota.source.as_deref(), Some(ID));
        assert_eq!(
            quota
                .details
                .iter()
                .map(|detail| detail.name.as_str())
                .collect::<Vec<_>>(),
            vec!["rolling (5h)", "weekly (1w)", "monthly (30d)"]
        );
        assert_eq!(
            quota
                .details
                .iter()
                .map(|detail| detail.used)
                .collect::<Vec<_>>(),
            vec![0.0, 12.0, 40.0]
        );
    }

    #[test]
    fn an_equal_percentage_resolves_to_the_shorter_window() {
        let body = serde_json::json!({"usage": {
            "rolling": {"percent": 30},
            "weekly": {"percent": 30},
            "monthly": {"percent": 30},
        }});
        let quota = assemble(parse_windows(&body));
        assert_eq!(quota.display_name.as_deref(), Some("Rolling (5 hours)"));
        assert_eq!(quota.used, 30.0);
    }

    #[tokio::test]
    async fn fetch_presents_the_credential_as_a_bearer_token_to_the_base_url() {
        let fake = FakeHttp::new(response(200, PAYLOAD));
        let quota = OpenCodeGo::new()
            .fetch(&context(fake.clone()))
            .await
            .unwrap();
        assert_eq!(quota.source.as_deref(), Some(ID));
        let requests = fake.requests.lock().unwrap();
        assert_eq!(requests[0].url, "https://opencode.ai/zen/go/v1/usage");
        assert!(requests[0]
            .headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
                && value == "Bearer oc_test_key"));
    }

    #[tokio::test]
    async fn a_missing_credential_fails_before_any_request() {
        let fake = FakeHttp::new(response(200, PAYLOAD));
        let ctx = QuotaFetchContext::new(ID, Dialect::OpenAi, fake.clone());
        assert!(matches!(
            OpenCodeGo::new().fetch(&ctx).await,
            Err(Error::Config(_))
        ));
        assert!(fake.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_error_status_is_an_error_and_never_a_fabricated_quota() {
        let fake = FakeHttp::new(response(401, r#"{"error":{"type":"AuthError"}}"#));
        assert!(matches!(
            OpenCodeGo::new().fetch(&context(fake)).await,
            Err(Error::InvalidState(_))
        ));
    }

    #[tokio::test]
    async fn an_answer_with_no_windows_is_an_error() {
        let fake = FakeHttp::new(response(200, r#"{"usage":{}}"#));
        assert!(matches!(
            OpenCodeGo::new().fetch(&context(fake)).await,
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn only_the_go_provider_is_claimed_and_never_by_host() {
        let account = QuotaAccount::new();
        assert!(OpenCodeGo::new().supports(Dialect::OpenAi, ID, &account));
        // The free and Zen tiers share this host and 404 the usage endpoint, so a host claim
        // would present a working key as a failed one.
        assert!(!OpenCodeGo::new().supports(Dialect::OpenAi, "opencode-zen", &account));
        assert!(!OpenCodeGo::new().supports(Dialect::OpenAi, "opencode", &account));
        assert!(!OpenCodeGo::new().claims_host("opencode.ai"));
    }
}
