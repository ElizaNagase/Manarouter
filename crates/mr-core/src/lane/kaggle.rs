//! Kaggle Notebooks: a batch lane that works headlessly and must never be pooled.
//!
//! ## Why this descriptor exists
//!
//! Kaggle looks like a serving lane from the outside — notebooks, GPUs, a Python API — and is not
//! one. A kernel is *pushed* (`kaggle kernels push -p DIR`), which creates or updates it and asks
//! Kaggle to run it; the caller then polls `kernels status`, optionally reads `kernels logs`, and
//! collects `kernels output` as files. The kernel API has no port, ingress, webhook or public URL,
//! so nothing can connect to a finished run. The shape is [`LaneShape::Batch`] with
//! [`Inbound::None`], and [`crate::lane::plan_lane`] refuses synchronous traffic against it.
//!
//! Two facts decide everything a future caller does with this lane:
//!
//! 1. **The API works headlessly.** Once a `KGAT_` token is in the subprocess environment — made
//!    once in a browser at `kaggle.com/settings/api` — push, status and output need no browser and
//!    no interactive session. That is why the submission is [`Submission::KernelPush`] and
//!    [`LaneFacts::automatable`] is true. It is a queued job, not an endpoint: queueing, image pull,
//!    weight download and model load put a floor of minutes under every call.
//! 2. **The account model is forbidden.** Kaggle's Terms §3 prohibit more than one active account
//!    per person, so the "pool two accounts and rotate" design is a ban with extra steps.
//!    [`AccountPolicy::Prohibited`] records the clause and [`crate::lane::plan_lane`] refuses a
//!    second account. One account per deployment; no identity failover.
//!
//! ## Credentials
//!
//! [`FACTS`] asks for one [`Credential::ApiToken`]: the `KGAT_…` bearer token, read from
//! `KAGGLE_API_TOKEN` or `~/.kaggle/access_token`. The legacy `~/.kaggle/kaggle.json`
//! (`{"username","key"}`, or `KAGGLE_USERNAME` + `KAGGLE_KEY`) still exists and is a **different
//! thing**: it authenticates with HTTP Basic, and a `KGAT_` token placed in it is rejected. Do not
//! treat the two as spellings of the same credential when building the spawn environment.
//!
//! The SDK fixes its config directory (`KAGGLE_CONFIG_DIR`, else `~/.kaggle`, else
//! `$XDG_CONFIG_HOME/kaggle`) at import and keeps one credential per process. A fresh config
//! directory per spawn is therefore good hygiene; it is **not** account rotation and does not make
//! pooling permissible.
//!
//! ## What has no single answer
//!
//! Kaggle's own docs disagree about the idle timeout for an interactive session: 20 minutes in the
//! notebook docs, 60 minutes in the GPU-usage docs. [`LaneFacts::idle_shutdown_secs`] is `None`
//! rather than a guess. The run ceiling is not in doubt: 12 h for CPU and GPU sessions and 9 h for
//! TPU, and `kernels push -t SECONDS` can ask for less. The binding constraint is the weekly quota
//! — 30 GPU-hours, "sometimes higher depending on demand and resources" — not session length.

use super::{
    AccountPolicy, Credential, Inbound, LaneFacts, LaneKind, LaneShape, QuotaRule, Restriction,
    Submission,
};

/// Where the API surface and the `KGAT_` bearer token were read.
pub const API_DOCS: &str = "https://www.kaggle.com/docs/api";
/// Where the token environment variable, the config-dir lookup and the legacy Basic pair were read.
pub const AUTH_REFERENCE: &str =
    "https://github.com/Kaggle/kaggle-cli/blob/main/skills/references/auth.md";
/// Where create-or-update-and-run, status, logs, output and `-t` were read.
pub const KERNELS_DOCS: &str = "https://github.com/Kaggle/kaggle-cli/blob/main/docs/kernels.md";
/// Where the 12 h/9 h session lengths, the 20 min idle claim and `/kaggle/working` were read.
pub const NOTEBOOK_DOCS: &str = "https://www.kaggle.com/docs/notebooks#technical-specifications";
/// Where the weekly 30-hour GPU quota and the 60 min idle claim were read.
pub const GPU_QUOTA_DOCS: &str = "https://www.kaggle.com/docs/efficient-gpu-usage";
/// Where the TPU hours and the 9 h TPU session cap were read.
pub const TPU_DOCS: &str = "https://www.kaggle.com/docs/tpu";
/// Where the account and own-use clauses were read.
pub const TERMS: &str = "https://www.kaggle.com/terms";

pub static FACTS: LaneFacts = LaneFacts {
    kind: LaneKind::Kaggle,
    id: "kaggle",
    display: "Kaggle Notebooks",
    shape: LaneShape::Batch,
    inbound: Inbound::None,
    // `kaggle kernels push -p DIR` creates-or-updates and runs; no browser is needed afterwards.
    submission: Submission::KernelPush,
    // The `KGAT_` bearer token. The legacy username+key pair is HTTP Basic and is not this.
    credentials: &[Credential::ApiToken],
    accounts: AccountPolicy::Prohibited {
        // Terms §3. The facts table quotes the clause from "you may not have…"; the full sentence
        // begins with "You also", and that is the wording used here so the quote is not a fragment.
        clause: "You also may not have, control, or operate under more than one active Kaggle \
                 account or Kaggle User ID",
        source: TERMS,
    },
    // 12 h for CPU and GPU sessions; TPU sessions are 9 h, recorded in `quota` below.
    max_session_secs: Some(12 * 60 * 60),
    // Kaggle's docs contradict each other: 20 min (notebook docs) vs 60 min (GPU-usage docs).
    // Storing either would turn a disagreement into a fact, so the field stays None.
    idle_shutdown_secs: None,
    quota: &[
        QuotaRule {
            what: "gpu-hours",
            limit:
                "The quota resets weekly and is 30 hours or sometimes higher depending on demand \
                    and resources",
            reset: Some("weekly"),
            source: GPU_QUOTA_DOCS,
        },
        QuotaRule {
            what: "tpu-hours",
            limit: "up to 20 hours per week of TPUs and up to 9h at a time in a single session",
            reset: Some("weekly"),
            source: TPU_DOCS,
        },
        QuotaRule {
            what: "session-length",
            limit: "12 h for CPU and GPU sessions, 9 h for TPU sessions",
            reset: None,
            source: NOTEBOOK_DOCS,
        },
    ],
    use_scope: Some(Restriction {
        text:
            "You will only use the Services for your own internal, personal, non-commercial use, \
               and not on behalf of or for the benefit of any third party",
        source: TERMS,
    }),
    headline: "Batch only: a submitted kernel takes minutes per call and returns files, never a \
               live response; a second account is a Terms §3 ban, and Kaggle's own docs disagree \
               on whether an idle session dies at 20 or 60 minutes.",
    sources: &[
        API_DOCS,
        AUTH_REFERENCE,
        KERNELS_DOCS,
        NOTEBOOK_DOCS,
        GPU_QUOTA_DOCS,
        TPU_DOCS,
        TERMS,
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_weekly_gpu_quota_is_present_and_cites_its_source() {
        let gpu = FACTS
            .quota
            .iter()
            .find(|rule| rule.what == "gpu-hours")
            .expect("Kaggle rations GPU time by the week");
        assert_eq!(gpu.reset, Some("weekly"), "the GPU quota resets weekly");
        assert!(
            gpu.limit.contains("30 hours"),
            "the limit must stay in Kaggle's words: {}",
            gpu.limit
        );
        assert_eq!(gpu.source, GPU_QUOTA_DOCS);
        assert!(gpu.source.starts_with("https://"));
    }

    #[test]
    fn the_account_policy_is_the_terms_prohibition_not_a_summary() {
        match FACTS.accounts {
            AccountPolicy::Prohibited { clause, source } => {
                assert!(
                    clause.len() > 100,
                    "the clause must be the sentence, not a fragment: {clause}"
                );
                assert!(
                    clause.contains("more than one active Kaggle account"),
                    "the quoted clause no longer states the prohibition"
                );
                assert_eq!(source, TERMS);
                assert!(source.starts_with("https://"));
            }
            AccountPolicy::Allowed => panic!("Kaggle forbids a second active account"),
        }
    }

    #[test]
    fn the_lane_is_automatable_but_cannot_serve_a_request() {
        assert!(
            !FACTS.serves_traffic(),
            "there is no inbound socket to answer a request"
        );
        assert!(
            FACTS.automatable(),
            "push, status and output run without a browser"
        );
        assert_eq!(
            FACTS.max_session_secs,
            Some(43_200),
            "12 h GPU session ceiling"
        );
    }
}
