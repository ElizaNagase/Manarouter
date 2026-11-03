//! Live combo activity: the relay's own view of what its combos are doing.
//!
//! The decision "which step did this combo take" is made in the data plane, from a snapshot the
//! relay holds, so the control plane cannot reconstruct it. This module asks the relay for its
//! `/control/activity` ring and hands the answer to the panel unchanged.
//!
//! # Why an unreachable relay is a 502 and not an empty list
//!
//! An empty list means "no combo has run". A relay that is down means "nobody knows". Answering
//! the second with the first would make the panel say "no combo activity yet" while requests were
//! failing at the gateway, which is precisely the moment the operator needs the truth. The 502
//! names the port so the fix is obvious.
//!
//! # Why it is registered inside the guarded router
//!
//! The payload names the operator's combos and which provider/model each one chose. That is
//! operational detail about the gateway, not public data, so it sits behind the same session check
//! as every other `/api/*` route.

use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::Request;
use serde_json::Value;

use crate::api::{sync_client, ApiError, ApiResult};
use crate::state::AppState;

/// Cap on the relay body this proxy will read.
///
/// The real answer is a bounded ring of at most a few dozen short entries, so this is generous by
/// two orders of magnitude. The bound exists so a relay bug or a wrong process on the port cannot
/// exhaust the control plane's memory.
const MAX_ACTIVITY_BYTES: usize = 256 * 1024;

/// The routes this module contributes, merged into the authenticated API surface.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/activity", get(activity))
}

/// `GET /api/activity` — the relay's combo activity, proxied.
async fn activity(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let port = state.current_config().await.relay.port;
    let url = format!("http://127.0.0.1:{port}/control/activity");
    let client = sync_client().map_err(ApiError::internal)?;
    let request = Request::builder()
        .method(http::Method::GET)
        .uri(&url)
        .header(http::header::ACCEPT, "application/json")
        .body(Full::new(Bytes::new()))
        .map_err(|e| ApiError::internal(format!("could not build the activity request: {e}")))?;

    let response = tokio::time::timeout(Duration::from_secs(5), client.request(request))
        .await
        .map_err(|_| relay_unreachable(port, "it did not answer within 5s"))?
        .map_err(|e| relay_unreachable(port, &format!("the connection failed: {e}")))?;

    let status = response.status();
    let body = tokio::time::timeout(
        Duration::from_secs(5),
        Limited::new(response.into_body(), MAX_ACTIVITY_BYTES).collect(),
    )
    .await
    .map_err(|_| relay_unreachable(port, "it did not finish answering within 5s"))?
    .map_err(|e| relay_unreachable(port, &format!("its answer could not be read: {e}")))?
    .to_bytes();

    if !status.is_success() {
        return Err(ApiError::upstream(
            StatusCode::BAD_GATEWAY,
            format!(
                "the relay answered {status} for its activity endpoint: {}",
                snippet(&body)
            ),
        ));
    }

    // Parsed rather than forwarded byte-for-byte: a relay answering with something that is not the
    // documented object would otherwise reach the panel as a JSON parse failure with no context.
    let payload: Value = serde_json::from_slice(&body).map_err(|_| {
        ApiError::upstream(
            StatusCode::BAD_GATEWAY,
            "the relay's activity answer is not JSON; it is not the endpoint this panel expects",
        )
    })?;
    Ok(Json(payload))
}

/// The error for a relay that is not answering, with the port in the message.
///
/// `detail` is deliberately mechanical (timeout, connection refused) and the sentence is the same
/// for all of them: the operator's action does not depend on which way the socket failed.
fn relay_unreachable(port: u16, detail: &str) -> ApiError {
    ApiError::upstream(
        StatusCode::BAD_GATEWAY,
        format!(
            "the relay is not answering on 127.0.0.1:{port} ({detail}); combo activity is unknown \
             until it is running"
        ),
    )
}

/// The start of a body, for an error message. Never the whole thing: it may be large.
fn snippet(body: &[u8]) -> String {
    const LIMIT: usize = 200;
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if text.chars().count() <= LIMIT {
        return text.to_owned();
    }
    let head: String = text.chars().take(LIMIT).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_relay_body_is_shortened_rather_than_echoed() {
        let long = "x".repeat(500);
        let shown = snippet(long.as_bytes());
        assert_eq!(
            shown.chars().count(),
            201,
            "200 characters plus the ellipsis"
        );
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn a_short_relay_body_is_reported_verbatim() {
        assert_eq!(snippet(b"  not json  "), "not json");
    }

    use crate::auth::PanelAuth;
    use crate::state::{ServeDirs, StateFiles};
    use axum::body::Body;
    use axum::http::{Method, Request as HttpRequest};
    use tower::ServiceExt;

    #[tokio::test]
    async fn a_relay_that_is_not_answering_is_a_502_that_names_the_port() {
        // The distinction this pins: "no combo has run" and "nobody can tell" must not look the
        // same in the panel. Binding a port and releasing it gives an address nothing listens on,
        // which is the state a stopped relay leaves behind.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
            listener.local_addr().expect("an address").port()
        };

        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("config.json");
        let mut config = mr_core::config::Config::default();
        config.relay.port = port;
        config.save(&config_path).expect("write the config");

        let state = AppState::new(
            config,
            StateFiles {
                config_path: config_path.clone(),
                snapshot_path: dir.path().join("routes.snapshot"),
                database_path: dir.path().join("state.sqlite"),
                dirs: ServeDirs {
                    panel: None,
                    docs: None,
                },
            },
            PanelAuth::resolve("127.0.0.1", None, false).expect("loopback panel"),
            port,
            20901,
        )
        .expect("build state");

        let response = crate::api::router(state)
            .oneshot(
                HttpRequest::builder()
                    .method(Method::GET)
                    .uri("/api/activity")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("router responded");

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json: Value = serde_json::from_slice(&bytes).expect("the error envelope is JSON");
        let message = json["error"]["message"].as_str().expect("a message");
        assert!(
            message.contains(&port.to_string()),
            "the message must name the port: {message}"
        );
        assert!(
            message.contains("combo activity is unknown"),
            "the message must not read as an empty list: {message}"
        );
    }
}
