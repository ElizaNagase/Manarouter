//! Model endpoints: testing a model, and syncing its price table.
//!
//! Task E2. Its routes are merged into the guarded router by `api::guarded_router`, so this
//! module owns its own file and nobody has to edit the shared route table to add one.
//!
//! `routes()` returns an empty router until the handlers land, which keeps the module list and the
//! merge site in `api.rs` stable while the handlers are written.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::Request;
use mr_core::model::{PricingPlan, PricingSource};
use serde_json::{json, Value};

use crate::api::{sync_client, ApiError, ApiResult, JsonBody};
use crate::state::AppState;

/// Cap on the upstream body read by `/api/models/test`, so a broken or hostile provider cannot
/// exhaust conductor memory by answering with an enormous (or endless) body.
const MAX_TEST_RESPONSE_BYTES: usize = 1 << 20;

/// The routes this module contributes, merged into the authenticated API surface.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/pricing/sync", post(sync_pricing))
        .route("/api/models/test", post(test_model))
}

/// Safe config roundtrip proving E5 fields remain readable/writable; the canonical config API is
/// deliberately generic and preserves fields defined by mr-core rather than duplicating DTOs.

#[derive(Debug, serde::Deserialize)]
struct ModelTestRequest {
    provider: String,
    model: String,
    #[serde(default = "default_test_prompt")]
    prompt: String,
}
fn default_test_prompt() -> String {
    "Reply with exactly: OK".into()
}

async fn test_model(
    State(state): State<AppState>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    // Deserialize through the envelope rather than axum's `Json` extractor: a malformed body would
    // otherwise answer 422 `text/plain`, which the panel cannot parse and shows as a bare status.
    let request: ModelTestRequest = serde_json::from_value(body).map_err(|e| {
        ApiError::bad(
            "invalid_body",
            format!("the request could not be read: {e}"),
        )
    })?;
    let config = state.current_config().await;
    let provider = config
        .providers
        .iter()
        .find(|p| p.id.as_str() == request.provider)
        .ok_or_else(|| ApiError::not_found(format!("no provider with id {}", request.provider)))?;
    provider
        .accounts
        .iter()
        .find(|a| a.enabled)
        .ok_or_else(|| ApiError::bad("no_account", "the provider has no enabled account"))?;
    provider
        .models
        .iter()
        .find(|m| m.id == request.model)
        .ok_or_else(|| {
            ApiError::bad(
                "unknown_model",
                "the model is not configured for this provider",
            )
        })?;
    // The test runs on the relay, not here. This window used to speak the OpenAI shape itself,
    // which meant it could only ever test OpenAI-shaped providers and refused every other dialect
    // — the exact routes an operator most wants to check (Codex, Antigravity, Command Code). The
    // relay's loopback probe runs the ordinary dispatch path instead: same routing table, same
    // dialect translation, same account selection and cooldowns a real request gets.
    let relay = format!("http://127.0.0.1:{}/control/probe", config.relay.port);
    let payload = serde_json::to_vec(&json!({
        "model": format!("{}/{}", request.provider, request.model),
        "prompt": request.prompt,
    }))
    .map_err(|e| ApiError::internal(format!("the test request could not be encoded: {e}")))?;
    let (status, response_body) = send_json(
        &relay,
        &[("content-type".to_owned(), "application/json".to_owned())],
        &payload,
    )
    .await
    .map_err(|_| {
        ApiError::upstream(
            axum::http::StatusCode::BAD_GATEWAY,
            format!(
                "the relay is not answering on 127.0.0.1:{}; start it and try again",
                config.relay.port
            ),
        )
    })?;
    let parsed: Value = serde_json::from_slice(&response_body)
        .map_err(|_| ApiError::bad_gateway("the model test returned invalid JSON"))?;
    if !status.is_success() {
        let message = parsed
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("the relay refused the test");
        return Err(probe_failure(status, message));
    }
    Ok(Json(
        json!({"provider": request.provider, "model": request.model, "ok": true, "response": parsed.get("choices").and_then(|c| c.get(0)).and_then(|c| c.get("message")).and_then(|m| m.get("content")).cloned().unwrap_or(Value::Null)}),
    ))
}

/// The failure to hand back when the probe's route refused.
///
/// The relay's status is passed through, with one exception: a `401`. On *this* API a `401` has
/// exactly one meaning — "the panel session is gone" — and `panel/api.js` acts on it globally by
/// dropping the operator to the sign-in screen. A provider that rejects a stored credential is not
/// a dead session: re-entering the panel password cannot fix it, and the bounce hides the model
/// list along with the verdict the operator was reading. So the refusal is reported as `502`
/// (the gateway did answer; its upstream refused) and the provider's own `401` stays in the
/// message, which is where the truth belongs. Everything else is passed through unchanged: a `429`
/// is a rate limit and a `403` a plan restriction, and the panel should say so with the route's
/// own words.
fn probe_failure(upstream: StatusCode, message: &str) -> ApiError {
    let status = if upstream == StatusCode::UNAUTHORIZED {
        StatusCode::BAD_GATEWAY
    } else {
        upstream
    };
    ApiError::upstream(status, format!("the relay answered {upstream}: {message}"))
}

async fn send_json(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> ApiResult<(StatusCode, Bytes)> {
    let client = sync_client().map_err(ApiError::internal)?;
    let mut builder = Request::builder()
        .method(if body.is_empty() {
            http::Method::GET
        } else {
            http::Method::POST
        })
        .uri(url);
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let request = builder
        .body(Full::new(Bytes::copy_from_slice(body)))
        .map_err(|e| ApiError::internal(format!("could not build model request: {e}")))?;
    let response =
        tokio::time::timeout(std::time::Duration::from_secs(30), client.request(request))
            .await
            .map_err(|_| ApiError::bad_gateway("model-test upstream timed out"))?
            .map_err(|e| {
                ApiError::bad_gateway(format!("model test could not reach upstream: {e}"))
            })?;
    let status = response.status();
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        Limited::new(response.into_body(), MAX_TEST_RESPONSE_BYTES).collect(),
    )
    .await
    .map_err(|_| ApiError::bad_gateway("model-test response timed out"))?
    .map_err(|e| ApiError::bad_gateway(format!("could not read model-test response: {e}")))?
    .to_bytes();
    Ok((status, body))
}

async fn sync_pricing(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    // `commit` replaces the whole config, so hold the mutation lock across the read-modify-write;
    // otherwise a concurrent panel edit landing between the read and the commit is reverted.
    let _mutation = state.mutation_lock.lock().await;
    let mut config = state.current_config().await;
    let bundled: Value = serde_json::from_str(include_str!("../../../catalog/pricing.json"))
        .map_err(|e| ApiError::internal(format!("bundled pricing table is invalid: {e}")))?;
    let table = bundled
        .get("models")
        .and_then(Value::as_object)
        .ok_or_else(|| ApiError::internal("bundled pricing table has no models map"))?;
    let mut updated = 0usize;
    let mut preserved = 0usize;
    for provider in &mut config.providers {
        for model in &mut provider.models {
            if model
                .pricing
                .as_ref()
                .is_some_and(|p| p.source == PricingSource::Manual)
            {
                preserved += 1;
                continue;
            }
            let qualified = format!("{}/{}", provider.id.as_str(), model.id);
            let candidate = table.get(&qualified).or_else(|| table.get(&model.id));
            let Some(candidate) = candidate else {
                continue;
            };
            let input = candidate.get("input").and_then(Value::as_f64);
            let output = candidate.get("output").and_then(Value::as_f64);
            if input.is_none() && output.is_none() {
                continue;
            }
            let plan = PricingPlan {
                input_per_mtok: input,
                output_per_mtok: output,
                cache_read_per_mtok: candidate.get("cache_read").and_then(Value::as_f64),
                cache_write_per_mtok: candidate.get("cache_write").and_then(Value::as_f64),
                source: PricingSource::Bundled,
            };
            model.input_price_per_mtok = input;
            model.output_price_per_mtok = output;
            model.pricing = Some(plan);
            updated += 1;
        }
    }
    state.commit_locked(config).await?;
    Ok(Json(
        json!({"updated": updated, "manual_overrides_preserved": true, "preserved": preserved, "message": format!("Updated {updated} models; preserved {preserved} manual overrides.")}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    /// A provider's refusal must reach the panel as a refusal, not as a dead session.
    ///
    /// `panel/api.js` treats any `401` as "your session died" and swaps the whole UI for the sign-in
    /// screen, so a provider that answers `401` to a generation request used to log the operator out
    /// and bury the model test's real verdict — the reason the panel said "Your session has expired"
    /// on a perfectly valid session. The status is translated to `502`; the provider's own status and
    /// words stay in the message, where they are actionable.
    #[tokio::test]
    async fn a_provider_refusal_keeps_its_verdict_but_never_borrows_the_sessions_401() {
        let refused = probe_failure(StatusCode::UNAUTHORIZED, "the key was revoked");
        let response = refused.into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body.contains("the relay answered 401 Unauthorized"),
            "{body}"
        );
        assert!(body.contains("the key was revoked"), "{body}");
        assert!(body.contains("\"code\":\"upstream\""), "{body}");

        // Everything else keeps the route's own status: a rate limit reading as a rate limit is the
        // point of passing the relay's answer through at all.
        for (upstream, expected) in [
            (StatusCode::TOO_MANY_REQUESTS, StatusCode::TOO_MANY_REQUESTS),
            (StatusCode::FORBIDDEN, StatusCode::FORBIDDEN),
            (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND),
            (StatusCode::PAYMENT_REQUIRED, StatusCode::PAYMENT_REQUIRED),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            let response = probe_failure(upstream, "x").into_response();
            assert_eq!(response.status(), expected, "{upstream}");
        }
    }
}
