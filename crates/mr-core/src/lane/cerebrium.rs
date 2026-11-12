//! Cerebrium: a REST-deployed serverless GPU whose custom runtime passes paths through to `vllm
//! serve`.
//!
//! The endpoint is the fact issue #10 got wrong. A Cortex function is called as
//! `POST https://api.cerebrium.ai/v4/{project_id}/{app_name}/{function_name}` and answers inside a
//! `{run_id, run_time_ms, result}` envelope. A **custom** runtime — the only kind that runs
//! `vllm serve` — has no function name: everything after `/{app_name}/` is proxied verbatim to the
//! container's `port`. So the base URL a relay should store is
//!
//! ```text
//! https://api.cerebrium.ai/v4/{project_id}/{app_name}/v1
//! ```
//!
//! and the client appends `/chat/completions` itself, as it would for OpenAI. `…/{app}/run` is the
//! Cortex function API — a function name in the path, here `run` — not a route a custom runtime
//! has. Pointing Manarouter at it yields a 404, not a completion.
//!
//! ## Control plane
//!
//! `https://rest.cerebrium.ai` is a different host from the inference router. A deploy is two
//! requests: `POST /v2/projects/{project_id}/apps` with the whole app record (including
//! `cerebrium_toml` as verbatim text) answers `{buildId, uploadUrl, …}`; then `PUT <uploadUrl>`
//! carries the project zip with `Content-Type: application/zip`. The upload URL is pre-signed — send
//! the bytes, not the token. The OpenAPI document describes itself as "mainly used by the Cerebrium
//! CLI client": there is no `/deploy` path and no published breaking-change promise, so the CLI and
//! this two-step flow are the same API, not two.
//!
//! ## Two credentials, and the 401 they cause
//!
//! A **service-account token** authenticates the control plane (deploy, scale, logs, and minting
//! keys). A project **API key** authenticates inference on `api.cerebrium.ai`. They are different
//! key types and are not interchangeable, so a setup flow must keep two slots rather than try one
//! and then the other: sending the wrong one returns a 401 that looks exactly like a wrong or
//! expired key, and retrying with the other value just moves the 401 to the other host.
//!
//! `POST /v2/projects/{project_id}/api-keys` returns the new key's secret in the response body
//! (`apiKey`), so a setup flow that already holds the control-plane credential can mint its own
//! inference key over REST — no dashboard copy-paste, and no secret in argv. Note that
//! `disable_auth` defaults to **true**: an app that omits it is public, and the API key only means
//! anything once the generated `cerebrium.toml` sets `disable_auth = false`.
//!
//! ## Scaling and the two traps
//!
//! `[cerebrium.scaling] cooldown` is the scale-down window — seconds at reduced concurrency before
//! an idle container is killed. Its default is **10 s** (range 0–3600, so the maximum is 3600), one
//! sixth of Modal's 60 s `scaledown_window`; a caller who assumes Modal's default will size a warm
//! window that is gone before the next request. [`FACTS`] records the default.
//!
//! Two traps will bite whoever writes the deploy code:
//!
//! * A deploy replaces the whole app record rather than patching it. `cerebrium_toml` is sent
//!   verbatim and every key omitted from it takes its default, so an omitted key is **reset to
//!   default** on the next deploy — a generated `cerebrium.toml` must be complete, including the
//!   keys this descriptor depends on.
//! * `replica_concurrency` defaults to **1 on GPU apps** (100 on CPU), which is wrong for vLLM: it
//!   caps a container at one in-flight request and makes the platform scale out 1:1 instead of
//!   batching. Raise it to match the server's `--max-num-seqs`. And `response_grace_period`
//!   defaults to **900 s** and doubles as the per-request timeout, so it bounds a stream as well as
//!   a shutdown.
//!
//! Prices are not repeated here: the card table owns them, and a second copy is a second thing to
//! be wrong.

use super::{
    AccountPolicy, Credential, Inbound, LaneFacts, LaneKind, LaneShape, QuotaRule, Submission,
};

/// Where the plan cards — apps per plan — and the per-second GPU rates were read. Prices are not
/// repeated in this descriptor.
pub const PRICING: &str = "https://cerebrium.ai/pricing";
/// Where the compute types, the `Plan required` column and the deploy-time rejection were read.
pub const GPU_DOCS: &str = "https://cerebrium.ai/docs/hardware/using-gpus";
/// Where `cooldown`, `replica_concurrency`, `response_grace_period` and their defaults were read.
pub const SCALING_DOCS: &str = "https://cerebrium.ai/docs/scaling/scaling-apps";
/// Where each `cerebrium.toml` key and its default were read — the generated file must be complete.
pub const TOML_REFERENCE: &str = "https://cerebrium.ai/docs/toml-reference/toml-reference";
/// Where the `/v4/{project}/{app}/…` endpoint, the JWT bearer and the status codes were read.
pub const INFERENCE_DOCS: &str = "https://cerebrium.ai/docs/endpoints/inference-api";
/// The official `vllm serve` example: a custom runtime answering at `…/{app}/v1/chat/completions`.
pub const VLLM_EXAMPLE: &str = "https://cerebrium.ai/docs/v4/examples/gpt-oss";
/// The control-plane OpenAPI: `POST …/apps` → `{buildId, uploadUrl}`, and `POST …/api-keys`.
pub const OPENAPI_SPEC: &str =
    "https://s3.eu-west-1.amazonaws.com/www.cerebrium.ai/openapi_spec.json";
/// The CLI's API client: the pre-signed `PUT <uploadUrl>` with `Content-Type: application/zip`.
pub const CLI_CLIENT: &str =
    "https://raw.githubusercontent.com/CerebriumAI/cerebrium/main/internal/api/client.go";
/// The terms behind `accounts: Allowed`: a generic template with no clause on account multiplicity,
/// resale or proxying. The absence of a prohibition is not a permission — read the Q9 row in
/// `docs/research/issue-10-platform-facts.md` before treating this lane as one that welcomes
/// other people's traffic.
pub const TERMS: &str = "https://cerebrium.ai/terms-of-service";

pub static FACTS: LaneFacts = LaneFacts {
    kind: LaneKind::Cerebrium,
    id: "cerebrium",
    display: "Cerebrium",
    shape: LaneShape::Serving,
    inbound: Inbound::PublicEndpoint,
    // A deploy is REST end to end: `POST …/apps`, then `PUT <uploadUrl>` with the zip.
    submission: Submission::RestApi,
    // Control plane first (service-account token), inference second (project API key).
    credentials: &[Credential::ServiceAccountToken, Credential::ApiKey],
    accounts: AccountPolicy::Allowed,
    // Serverless: no session to expire, only the idle cooldown below.
    max_session_secs: None,
    // `[cerebrium.scaling] cooldown`, default 10 s, range 0–3600.
    idle_shutdown_secs: Some(10),
    quota: &[
        QuotaRule {
            what: "apps",
            limit: "Up to 3 deployed apps on Hobby; unlimited on Standard and Enterprise",
            reset: None,
            source: PRICING,
        },
        QuotaRule {
            what: "gpu-types",
            limit: "A100, H100, H200, B200 and RTX PRO 6000 require Standard or Enterprise; a \
                    compute type outside the project's plan is rejected at deploy time",
            reset: None,
            source: GPU_DOCS,
        },
    ],
    // Cerebrium's terms carry no own-use restriction comparable to Kaggle's, so nothing is claimed.
    use_scope: None,
    headline:
        "REST-deployed serverless GPUs with OpenAI path passthrough; every card worth running \
               vLLM on is Standard or above.",
    sources: &[
        PRICING,
        GPU_DOCS,
        SCALING_DOCS,
        TOML_REFERENCE,
        INFERENCE_DOCS,
        VLLM_EXAMPLE,
        OPENAPI_SPEC,
        CLI_CLIENT,
        TERMS,
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cooldown_default_is_ten_seconds() {
        assert_eq!(FACTS.idle_shutdown_secs, Some(10));
        assert_eq!(
            FACTS.max_session_secs, None,
            "serverless has no session to expire"
        );
    }

    #[test]
    fn credentials_are_the_control_plane_token_then_the_inference_key() {
        assert_eq!(
            FACTS.credentials,
            &[Credential::ServiceAccountToken, Credential::ApiKey],
            "the order is control plane first, inference second"
        );
    }

    #[test]
    fn a_public_rest_endpoint_is_a_usable_lane() {
        assert!(FACTS.serves_traffic());
        assert!(FACTS.automatable());
    }
}
