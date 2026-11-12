//! Modal account quota through an operator-configured endpoint.
//!
//! Manarouter has no verified Modal account-quota API contract in this repository, so this
//! fetcher deliberately does not call an assumed Modal URL or infer the shape of an assumed
//! response. Instead, a Modal provider account can declare a quota endpoint in the account's
//! non-secret `modalQuotaEndpoint` metadata.
//!
//! The endpoint must return JSON containing numeric `spend` and `remaining` values, either at
//! the object root or under a root `data` object. `limit` is optional. An operator can also add
//! `total` as an alias for `limit`, `reset_at` or `resetAt`, `currency`, and `display_name`.
//! All amounts are validated as finite and non-negative. Auth is bearer by default; an operator
//! can set `"auth": "x-api-key"` or `"auth": "none"` in the same metadata object.
//!
//! No Modal URL is fabricated here. The URL is entirely supplied by the operator.

use async_trait::async_trait;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::QuotaRequest;
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::UsageQuota;

const CONFIG_KEY: &str = "modalQuotaEndpoint";
const DEFAULT_DISPLAY_NAME: &str = "Modal account spending";

/// A Modal account spend/remaining fetcher backed by an operator-declared endpoint.
#[derive(Debug, Clone, Copy, Default)]
pub struct Modal;

impl Modal {
    pub const ID: &str = "modal";
    pub const CONFIG_KEY: &str = CONFIG_KEY;

    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl QuotaFetcher for Modal {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn supports(&self, _dialect: Dialect, provider_id: &str, account: &QuotaAccount) -> bool {
        provider_id.eq_ignore_ascii_case(Self::ID)
            && account
                .extra
                .get(CONFIG_KEY)
                .is_some_and(|value| !value.is_null())
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let endpoint = endpoint(&ctx.account)?;
        let credential = credential(ctx, &endpoint.auth)?;
        let request = QuotaRequest::get(endpoint.url).header("Accept", "application/json");
        let request = match endpoint.auth.as_str() {
            "none" => request,
            "x-api-key" => request.header("x-api-key", credential),
            _ => request.bearer(&credential),
        };
        let response = ctx.http.request(request).await?;
        if !response.is_success() {
            return Err(Error::InvalidState(format!(
                "Modal quota endpoint answered {}",
                response.status
            )));
        }

        let body = response.json()?;
        let root = body.get("data").unwrap_or(&body);
        let Some(root) = root.as_object() else {
            return Err(Error::NotFound(
                "Modal quota endpoint did not return a JSON object".to_owned(),
            ));
        };

        let spend = number_field(root, &["spend", "used", "usage"])?.ok_or_else(|| {
            Error::NotFound("Modal quota endpoint reported no spend value".to_owned())
        })?;
        let remaining = number_field(root, &["remaining", "remaining_credits", "balance"])?
            .ok_or_else(|| {
                Error::NotFound("Modal quota endpoint reported no remaining value".to_owned())
            })?;
        let limit = number_field(root, &["limit", "total"])?;
        let reset = root
            .get("reset_at")
            .or_else(|| root.get("resetAt"))
            .cloned()
            .unwrap_or(Value::Null);
        let display_name = root
            .get("display_name")
            .or_else(|| root.get("displayName"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                root.get("currency")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(|currency| format!("Modal account spending ({currency})"))
                    .unwrap_or_else(|| DEFAULT_DISPLAY_NAME.to_owned())
            });

        let total = limit.or_else(|| Some(spend + remaining));
        let quota = UsageQuota::from_usage(spend, total.unwrap_or(0.0), &reset)
            .with_source(Self::ID)
            .with_display_name(display_name);
        Ok(quota)
    }
}

#[derive(Debug, Clone)]
struct Endpoint {
    url: String,
    auth: String,
}

fn endpoint(account: &QuotaAccount) -> Result<Endpoint> {
    let Some(value) = account.extra.get(CONFIG_KEY) else {
        return Err(Error::Config(format!(
            "Modal quota requires the account's `{CONFIG_KEY}` field"
        )));
    };

    let (raw_url, auth) = match value {
        Value::String(url) => (url.as_str(), "bearer"),
        Value::Object(object) => {
            let url = object.get("url").and_then(Value::as_str).unwrap_or("");
            let auth = object
                .get("auth")
                .and_then(Value::as_str)
                .unwrap_or("bearer");
            (url, auth)
        }
        _ => {
            return Err(Error::Config(format!(
                "the account's `{CONFIG_KEY}` must be a URL string or an object with a `url`"
            )))
        }
    };

    let url = raw_url.trim().to_owned();
    if url.is_empty() {
        return Err(Error::Config(format!(
            "the account's `{CONFIG_KEY}` is an empty url"
        )));
    }
    if !url.starts_with("https://") {
        return Err(Error::Config(format!(
            "the account's `{CONFIG_KEY}` must use an HTTPS URL"
        )));
    }

    let auth = match auth.to_ascii_lowercase().as_str() {
        "bearer" => "bearer",
        "x-api-key" => "x-api-key",
        "none" => "none",
        _ => "unknown",
    };
    if auth == "unknown" {
        return Err(Error::Config(format!(
            "the account's `{CONFIG_KEY}` has an unknown auth (use `bearer`, `x-api-key` or `none`)"
        )));
    }

    Ok(Endpoint {
        url,
        auth: auth.to_owned(),
    })
}

fn credential(ctx: &QuotaFetchContext, auth: &str) -> Result<String> {
    if auth == "none" {
        return Ok(String::new());
    }
    ctx.credential
        .as_ref()
        .map(|credential| credential.expose().to_owned())
        .filter(|credential| !credential.is_empty())
        .ok_or_else(|| Error::Config("Modal quota requires an account API key".to_owned()))
}

fn number_field(root: &serde_json::Map<String, Value>, names: &[&str]) -> Result<Option<f64>> {
    for name in names {
        if let Some(value) = root.get(*name) {
            let Some(number) = value.as_f64() else {
                return Err(Error::InvalidState(format!(
                    "Modal quota endpoint field `{name}` is not numeric"
                )));
            };
            if !number.is_finite() || number < 0.0 {
                return Err(Error::InvalidState(format!(
                    "Modal quota endpoint field `{name}` must be finite and non-negative"
                )));
            }
            return Ok(Some(number));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use bytes::Bytes;
    use http::Method;
    use serde_json::{json, Value};

    use crate::dialect::Dialect;
    use crate::error::{Error, Result};
    use crate::quota::http::{QuotaHttp, QuotaRequest, QuotaResponse};
    use crate::quota::registry::{QuotaAccount, QuotaCredential, QuotaFetchContext, QuotaFetcher};

    use super::Modal;

    #[derive(Debug)]
    struct FakeHttp {
        responses: Vec<QuotaResponse>,
        requests: std::sync::Mutex<Vec<QuotaRequest>>,
    }

    #[async_trait]
    impl QuotaHttp for FakeHttp {
        async fn request(&self, request: QuotaRequest) -> Result<QuotaResponse> {
            self.requests.lock().unwrap().push(request);
            Ok(self.responses.first().cloned().unwrap())
        }
    }

    impl FakeHttp {
        fn new(status: u16, body: &str) -> Arc<Self> {
            Arc::new(Self {
                responses: vec![QuotaResponse {
                    status,
                    headers: Vec::new(),
                    body: Bytes::copy_from_slice(body.as_bytes()),
                }],
                requests: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<QuotaRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn context(http: Arc<FakeHttp>, endpoint: Value) -> QuotaFetchContext {
        let mut extra = BTreeMap::new();
        extra.insert(super::CONFIG_KEY.to_owned(), endpoint);
        QuotaFetchContext::new(Modal::ID, Dialect::OpenAi, http)
            .with_account(QuotaAccount {
                extra,
                ..Default::default()
            })
            .with_credential(QuotaCredential::new("test-token"))
    }

    #[tokio::test]
    async fn fetches_spend_and_remaining_as_a_normal_quota_window() {
        let http = FakeHttp::new(
            200,
            &json!({
                "data": {
                    "spend": 2.5,
                    "remaining": 7.5,
                    "currency": "USD"
                }
            })
            .to_string(),
        );
        let ctx = context(http.clone(), json!("https://example.invalid/modal/quota"));
        let quota = Modal::new().fetch(&ctx).await.unwrap();

        assert_eq!(quota.used, 2.5);
        assert_eq!(quota.total, 10.0);
        assert_eq!(quota.remaining, 7.5);
        assert_eq!(quota.remaining_percentage, 75.0);
        assert_eq!(quota.source.as_deref(), Some(Modal::ID));
        assert!(quota.display_name.unwrap().contains("USD"));

        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, Method::GET);
        assert_ne!(requests[0].url.as_str(), "test-token");
    }

    #[tokio::test]
    async fn requires_spend_and_remaining_without_fabricating_a_quota() {
        let http = FakeHttp::new(200, r#"{"spend": 2.5}"#);
        let ctx = context(http, json!("https://example.invalid/modal/quota"));
        let error = Modal::new().fetch(&ctx).await.unwrap_err();
        assert!(matches!(error, Error::NotFound(message) if message.contains("remaining")));
    }

    #[tokio::test]
    async fn rejects_missing_or_non_https_endpoints() {
        for endpoint in [json!(""), json!("http://example.invalid/quota")] {
            let http = FakeHttp::new(200, "{}");
            let ctx = context(http, endpoint);
            let error = Modal::new().fetch(&ctx).await.unwrap_err();
            assert!(matches!(error, Error::Config(_)));
        }
    }
}
