//! Modal: the one lane that is already implemented end to end.
//!
//! The facts here are the subset of `gpu::catalog` that decides lane eligibility, restated in the
//! lane vocabulary so that a caller comparing platforms does not have to know that Modal's prices
//! live in the card table and its concurrency ceilings in [`crate::gpu::catalog::PlanLimit`]. The
//! prices are deliberately *not* repeated: a second copy of a price is a second thing to be wrong.
//!
//! Two facts shape everything else. Modal publishes **no deployment REST API** — `modal deploy` is
//! the only supported way to create an app, which is why the panel renders a command and a bundle
//! instead of a button — and the proxy tokens that authenticate a call to `*.modal.run` are a pair,
//! not a bearer token, which is why the dialect exists at all.

use super::{
    AccountPolicy, Credential, Inbound, LaneFacts, LaneKind, LaneShape, QuotaRule, Submission,
};

/// Where the GPU ids, the `:n` suffix rules and the H100→H200 upgrade behaviour were read.
pub const GPU_DOCS: &str = "https://modal.com/docs/guide/gpu.md";
/// Where the per-second prices and the plan table were read.
pub const PRICING: &str = "https://modal.com/pricing";
/// Where `scaledown_window`, its 2–1200 s range and the 60 s default were read.
pub const SCALING_DOCS: &str = "https://modal.com/docs/guide/cold-start";
/// Where the `Modal-Key`/`Modal-Secret` proxy-token pair was read.
pub const PROXY_AUTH_DOCS: &str = "https://modal.com/docs/guide/webhook-urls";

pub static FACTS: LaneFacts = LaneFacts {
    kind: LaneKind::Modal,
    id: "modal",
    display: "Modal",
    shape: LaneShape::Serving,
    inbound: Inbound::PublicEndpoint,
    // No public deployment API: `modal deploy APP_REF` is the documented path, and the CLI drives it
    // unattended once a token is in the environment.
    submission: Submission::Cli,
    credentials: &[Credential::ProxyTokenPair],
    accounts: AccountPolicy::Allowed,
    // Serverless: no session to expire.
    max_session_secs: None,
    // The container dies after `scaledown_window` idle seconds. Default 60, range 2–1200.
    idle_shutdown_secs: Some(60),
    quota: &[
        QuotaRule {
            what: "gpu-concurrency",
            limit: "10 GPUs on Starter, 50 on Team",
            reset: None,
            source: PRICING,
        },
        QuotaRule {
            what: "containers",
            limit: "100 containers on Starter, 5000 on Team",
            reset: None,
            source: PRICING,
        },
    ],
    // Modal's terms carry no own-use restriction comparable to Kaggle's, so nothing is claimed here.
    use_scope: None,
    headline: "Serverless GPUs behind proxy tokens; a cold start per container and a 60 s idle shutdown by default.",
    sources: &[GPU_DOCS, PRICING, SCALING_DOCS, PROXY_AUTH_DOCS],
};
