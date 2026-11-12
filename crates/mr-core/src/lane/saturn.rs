//! Saturn Cloud: an OpenAI-compatible endpoint whose control plane is a REST API, behind one
//! hand-minted token.
//!
//! The part of Saturn Cloud that is actually usable as a lane is **Token Factory inference
//! endpoints**, not the Jupyter workspace or the managed Deployment the issue described. An inference
//! endpoint "serves an open-weight model over an OpenAI-compatible HTTP API": the base URL is
//! `https://<endpoint>/v1`, requests carry `Authorization: Bearer <saturn-token>`, the `model` field
//! is the endpoint's name, and `GET /v1/models` lists the names the endpoint accepts. The lifecycle
//! is API-creatable — `POST /api/orgs/{org_id}/token-factory/inference-endpoints` with a name and a
//! GPU `instance_size`, plus start, stop, list, get and delete — so nothing in the serving path
//! needs a browser. That is why [`LaneShape::Serving`], [`Inbound::PublicEndpoint`] and
//! [`Submission::RestApi`] can all be claimed at once.
//!
//! ## Why the alternatives are strictly worse
//!
//! Jupyter workspaces and managed Deployments remain documented fallbacks, not the design:
//!
//! * A Deployment's HTTPS URL is **not public**. An internal authentication layer verifies the
//!   user's Saturn Cloud session and prompts for sign-in; programmatic access needs
//!   `Authorization: token {USER_TOKEN}`. Its container must also listen on port 8000 — "no other
//!   ports will be open" — and bind `0.0.0.0`, so the application has to be shaped for the platform.
//! * The issue's tunnelled-`:8000` design — an SSH `-L` tunnel from a Jupyter workspace to the relay
//!   — is **our invention, not the platform's**. Saturn's SSH page documents an interactive shell
//!   and file copy, never port forwarding, and no page says a forwarded port survives their
//!   gateway. Building the lane on it would be building on an unstated assumption.
//!
//! ## The one attended step
//!
//! `submission` is [`Submission::RestApi`] because endpoints are created and started over REST, but
//! the token that authorises those calls is minted once by hand: User Profile → Access Keys → New
//! Token, shown once and never retrievable. No token-free bootstrap is documented — no CLI login, no
//! OAuth device flow — so the first step of setting up this lane is attended even though every step
//! after it is not. There is no second credential and no path that avoids the first.
//!
//! ## One hour, and what counts as activity
//!
//! Jupyter servers default to a **1 hour** auto-shutoff; the API enum offers 1 hour, 6 hours, 24
//! hours, 3 days, 7 days or Never. A Jupyter server is active while **either** a JupyterLab browser
//! window is open **or** an SSH connection is open, so an abandoned tab or a half-open shell resets
//! the timer and a warm lane can stay warm by accident. A keep-warm strategy therefore has to reason
//! about sessions that look idle to the router but busy to Saturn, and `idle_shutdown_secs` here is a
//! platform setting rather than a guarantee the router controls.
//!
//! ## Two spellings of one token
//!
//! The deployment docs send `Authorization: token {TOKEN}`; the versioned OpenAPI document declares
//! the `bearerAuth` scheme (`http bearer`) for the same API, and the Token Factory docs say
//! `Authorization: Bearer <token>`. Same platform, same user token, two spellings. An implementer
//! should probe a discovery call on each surface rather than trust either document to be the whole
//! truth.
//!
//! ## The free-plan premise is stale
//!
//! The issue's "free plan, 150 hours a month, no credit card" survives only in a 2025-05-12 Wayback
//! copy of `/try/`. Live `/try/` is now a dedicated-inference sales page, and no live page states
//! the hours, a GPU entitlement or a credit-card requirement; `/plans/hosted/` still says "Start
//! with the Free Plan" but publishes no quota with it. The real allowance is per-plan data:
//! `GET /api/limits` returns `hours_per_day`, `hours_per_month`, `hours_forever`, `instance_sizes`,
//! `num_instances`, `auto_shutoff` and `storage_in_gb`, and none of those values is published.
//! Reading them needs a token, so this descriptor claims no free tier and no hour count.
//!
//! ## Prices live elsewhere, and the two live pages disagree
//!
//! No per-endpoint price is published anywhere, and the two live plan pages publish different
//! per-hour figures for the same machine: `/plans/hosted/` and `/plans/hosted-orgs/` disagree, and
//! neither says which one checkout uses. This module records the conflict and does not resolve it,
//! and it copies no price at all: `gpu::catalog` owns card prices, and a second copy is a second
//! thing to be wrong.

use super::{
    AccountPolicy, Credential, Inbound, LaneFacts, LaneKind, LaneShape, QuotaRule, Submission,
};

/// Where the OpenAI-compatible surface was read: `https://<endpoint>/v1`, the bearer token, the
/// endpoint name as `model`, and `GET /v1/models`.
pub const CALLING_ENDPOINTS: &str = "https://saturncloud.io/docs/inference/calling-endpoints/";
/// Where the Token Factory product and its OpenAI-compatible claim were read.
pub const INFERENCE_DOCS: &str = "https://saturncloud.io/docs/inference/";
/// The create operation: `POST /api/orgs/{org_id}/token-factory/inference-endpoints`.
pub const INFERENCE_API: &str = "https://api.saturncloud.io/reference/inferenceendpoints_create.md";
/// The API index, where start/stop/list/get/delete for inference endpoints were confirmed.
pub const API_INDEX: &str = "https://api.saturncloud.io/llms.txt";
/// Access Keys → New Token, shown once and never retrievable.
pub const TOKENS_DOCS: &str = "https://saturncloud.io/docs/user-guide/how-to/access/tokens/";
/// The 1-hour default and the Jupyter-window/SSH definition of activity.
pub const AUTOSHUTOFF_DOCS: &str =
    "https://saturncloud.io/docs/user-guide/how-to/access/autoshutoff/";
/// The `auto_shutoff` enum and its `"1 hour"` default, plus `start_ssh` and `ssh_url`.
pub const WORKSPACE_API: &str = "https://api.saturncloud.io/reference/workspaces_create-1.md";
/// The port-8000 rule and the internal authentication layer in front of Deployment URLs.
pub const DEPLOYMENTS_DOCS: &str =
    "https://saturncloud.io/docs/user-guide/how-to/resources/deployments/";
/// SSH as an interactive shell and file copy only; no port forwarding is documented.
pub const SSH_DOCS: &str = "https://saturncloud.io/docs/user-guide/how-to/access/ide_ssh/";
/// Where the self-service instance sizes and the 8-GPU ceiling were read.
pub const PLANS: &str = "https://saturncloud.io/plans/hosted/";
/// The other live rate table; cited only because it conflicts with [`PLANS`].
pub const PLANS_ORGS: &str = "https://saturncloud.io/plans/hosted-orgs/";
/// The `/api/limits` response fields: the quota is per-plan data, not prose.
pub const LIMITS_API: &str = "https://api.saturncloud.io/reference/limits_list-1.md";

pub static FACTS: LaneFacts = LaneFacts {
    kind: LaneKind::Saturn,
    id: "saturn",
    display: "Saturn Cloud",
    // Token Factory endpoints are an inbound HTTP API.
    shape: LaneShape::Serving,
    inbound: Inbound::PublicEndpoint,
    // Created, started and stopped over REST; no CLI and no browser are required after the token.
    submission: Submission::RestApi,
    // One user token authorises both the control plane and inference. It is minted by hand once,
    // which is why the lane is REST-automatable yet still starts with an attended step.
    credentials: &[Credential::ApiToken],
    accounts: AccountPolicy::Allowed,
    // No wall-clock session ceiling is stated; the 1-hour shutdown below is inactivity, not a max.
    max_session_secs: None,
    // `auto_shutoff` defaults to "1 hour". A JupyterLab window or an open SSH session counts as
    // activity, so the workspace can outlive the traffic that kept it warm.
    idle_shutdown_secs: Some(3600),
    quota: &[
        QuotaRule {
            what: "gpus-per-instance",
            limit: "8 GPUs per instance — reached by g4dn.metal, p3.16xlarge, g5.48xlarge and Nebius 8xH100",
            reset: None,
            source: PLANS,
        },
        QuotaRule {
            what: "auto-shutoff",
            limit: "1 hour, 6 hours, 24 hours, 3 days, 7 days or Never; Jupyter servers default to 1 hour",
            reset: None,
            source: WORKSPACE_API,
        },
        QuotaRule {
            what: "plan-quota",
            limit: "per-plan and machine-readable only: GET /api/limits reports hours_per_day, hours_per_month, hours_forever, instance_sizes, num_instances, auto_shutoff and storage_in_gb; the values are not published and were not read",
            reset: None,
            source: LIMITS_API,
        },
    ],
    // No own-work restriction comparable to Kaggle's was found, so nothing is claimed here.
    use_scope: None,
    headline: "An OpenAI-compatible Token Factory endpoint created and started entirely over the API, at the cost of one hand-minted token and with no verified free tier.",
    sources: &[
        CALLING_ENDPOINTS,
        INFERENCE_DOCS,
        INFERENCE_API,
        API_INDEX,
        TOKENS_DOCS,
        AUTOSHUTOFF_DOCS,
        WORKSPACE_API,
        DEPLOYMENTS_DOCS,
        SSH_DOCS,
        PLANS,
        PLANS_ORGS,
        LIMITS_API,
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_inference_endpoint_can_answer_a_request() {
        assert!(
            FACTS.serves_traffic(),
            "Token Factory endpoints are a public HTTPS OpenAI-compatible API"
        );
    }

    #[test]
    fn the_endpoint_lifecycle_is_automatable() {
        assert!(
            FACTS.automatable(),
            "inference endpoints are created and started over REST"
        );
    }

    #[test]
    fn one_hand_minted_user_token_is_the_only_credential() {
        assert_eq!(FACTS.credentials, &[Credential::ApiToken]);
    }
}
