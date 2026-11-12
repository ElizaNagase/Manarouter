//! Saturn Cloud: a Jupyter server reached over a supervised SSH tunnel
//!
//! Owned by one work item, so the file is the unit of review: nothing here is shared with the
//! other lane modules and nothing outside it needs to know how Saturn Cloud works.
//!
//! The facts — what the platform's documents actually say — live in
//! `mr_core::lane::saturn`. This module is the half that talks to the network and writes
//! artefacts, which is why it lives in the conductor (the admin path) rather than in the relay
//! (the request path): a deployment is an operator action, not a request.

use axum::{routing::get, Json, Router};
use serde_json::{json, Value};

use crate::state::AppState;

/// The routes this module owns.
///
/// Empty until the lane is wired: a route that answers `501` is worse than no route, because a
/// client cannot tell the difference between "not built yet" and "built and broken".
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/gpu/lanes/saturn/status", get(get_status))
}

/// What this lane can currently do, without contacting the platform.
async fn get_status() -> Json<Value> {
    Json(json!({
        "lane": "saturn",
        "ready": false,
        "detail": "the Saturn Cloud lane has no deploy path yet: this route reports that plainly \
                   rather than pretending to a capability the binary does not have",
    }))
}
