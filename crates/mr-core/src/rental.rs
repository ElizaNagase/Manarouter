//! The two rental entities: a platform **account**, and a deployment — one model on one GPU.
//!
//! ## Why two entities and not one
//!
//! The operator's requirement 8 is explicit: *"Multiple models may be hosted under the same
//! account; account creation should be separate from model setup."* So a [`RentalAccount`] is a
//! workspace credential with no model attached, and many deployments may share it. Creating the
//! credential and choosing the model are two different acts, made at two different times, and
//! the types keep them apart.
//!
//! ## Why an account is not a [`crate::model::Provider`]
//!
//! A provider requires a `base_url`. A Modal account has none: each app gets its own
//! `https://<workspace>--<app>-<fn>.modal.run` URL, and there is no workspace-level base URL to
//! put in the field. An account modelled as a provider would therefore have to carry a lie in a
//! mandatory field. [`crate::config::Config::rental_accounts`] is the honest home instead, and it
//! is the seam the later GPU-rental issues plug into.
//!
//! ## Why a deployment *is* a provider
//!
//! The inverse reasoning applies. A deployment — one model, reachable at a URL, with a KV-token
//! budget, a per-second price and a cold-start tolerance — is exactly what
//! [`crate::model::Provider`] already is, and the relay already enforces those budget fields on
//! the request path. Re-modelling it would duplicate all of that. So a deployment is **stored in
//! `Provider.extra["rental"]`** and parsed into [`RentalDeployment`] on demand.
//!
//! That choice is not merely convenient, it is required: `RoutingSnapshot` builds its own
//! `ProviderPlan` struct, so `Provider.extra` is not part of the compiled snapshot layout and
//! adding keys to it cannot shift a byte in the file the relay reads. A new `Provider` field
//! would force a [`crate::SNAPSHOT_VERSION`] bump for no runtime benefit, because the relay never
//! reads a GPU name. See `docs/research/ARCHITECTURE-gpu-rentals.md` §3.2. **[facts]**
//!
//! ## Storage is flexible; the code that reads it is not
//!
//! `Provider.extra` is an untyped map, so it will happily hold `"gpu": 42` or a `rental` object
//! with the GPU name misspelled. [`RentalDeployment::from_provider_extra`] therefore returns a
//! [`Result`] and a malformed value is a validation *error* the panel can report against the
//! offending field — never a silent default that quietly rents the wrong card.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::gpu::arch::ArchFacts;
use crate::gpu::sizing::SizingResult;

/// The key a [`RentalDeployment`] is stored under inside `Provider.extra`.
pub const RENTAL_EXTRA_KEY: &str = "rental";

/// Which platform a rental account bills against.
///
/// The known platforms are variants rather than plain strings so the panel can offer them, but the
/// set is deliberately open: a cheap GPU host appears every few months, and refusing to record one
/// until this crate ships a new release would make the feature useless for exactly the operators
/// it is for. [`RentalPlatform::Custom`] carries the platform's own identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RentalPlatform {
    /// Modal — the platform this feature was researched against.
    Modal,
    /// Cerebrium.
    Cerebrium,
    /// Kaggle notebooks.
    Kaggle,
    /// Saturn Cloud.
    Saturn,
    /// molab.
    Molab,
    /// Any other platform, by its own slug.
    #[serde(untagged)]
    Custom(String),
}

impl RentalPlatform {
    /// The stable identifier written to config and shown in the panel.
    pub fn as_str(&self) -> String {
        match self {
            Self::Modal => "modal".to_string(),
            Self::Cerebrium => "cerebrium".to_string(),
            Self::Kaggle => "kaggle".to_string(),
            Self::Saturn => "saturn".to_string(),
            Self::Molab => "molab".to_string(),
            Self::Custom(name) => format!("custom:{name}"),
        }
    }

    /// Parse a platform identifier, accepting the `custom:<x>` spelling written by [`Self::as_str`].
    pub fn parse(s: &str) -> Self {
        let trimmed = s.trim();
        let lower = trimmed.to_ascii_lowercase();
        match lower.as_str() {
            "modal" => Self::Modal,
            "cerebrium" => Self::Cerebrium,
            "kaggle" => Self::Kaggle,
            "saturn" => Self::Saturn,
            "molab" => Self::Molab,
            _ => match lower.strip_prefix("custom:") {
                Some(rest) => Self::Custom(rest.to_string()),
                None => Self::Custom(trimmed.to_string()),
            },
        }
    }

    /// Every platform this build knows by name, for a panel's dropdown.
    pub const KNOWN: [&'static str; 5] = ["modal", "cerebrium", "kaggle", "saturn", "molab"];
}

/// What an account is expected to cost.
///
/// [`RentalPlan::Unknown`] is a **real state and must not be rendered as `Free`**. The distinction
/// is the whole reason requirement 10's `(Paid)` marker can be trusted: a `(Paid)` marker is only
/// truthful when the plan is known *and* the GPU is known to be gated. A UI that collapses
/// `unknown` into either answer is guessing with the operator's money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RentalPlan {
    /// A documented free tier.
    Free,
    /// A metered plan.
    Paid,
    /// Not known. The default, because it is the truthful answer before anyone checks.
    #[default]
    Unknown,
}

impl RentalPlan {
    /// The stable identifier written to config.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Paid => "paid",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a plan identifier.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "free" => Some(Self::Free),
            "paid" => Some(Self::Paid),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// A `"free" | "paid" | "unknown"` triple, serialised as one of those three words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FreePlan {
    /// Documented as usable on a free plan.
    Available,
    /// Documented as not usable on a free plan.
    Unavailable,
    /// No documentation either way. The default, and the honest answer for almost every card.
    #[default]
    Unknown,
}

impl FreePlan {
    /// The stable identifier shown in the panel and carried in `GET /api/gpu/catalog`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable => "unavailable",
            Self::Unknown => "unknown",
        }
    }
}

/// How well a catalog fact is established.
///
/// Three values, not two, and the middle one is not decoration. A number the operator measured on
/// their own hardware is not the same kind of claim as a number read off a vendor's pricing page,
/// and neither is the same as a number nobody has checked. The panel is allowed to display all
/// three *without re-deriving them*, which only works if the number arrives with its own
/// confidence attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FactSource {
    /// Verified against primary documentation.
    Documented,
    /// Measured by the operator.
    Observed,
    /// Recorded but not verified. Never presented as fact.
    Unverified,
}

impl FactSource {
    /// The stable identifier written into the catalog.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Documented => "documented",
            Self::Observed => "observed",
            Self::Unverified => "unverified",
        }
    }
}

/// A platform credential for renting GPUs, with no model attached.
///
/// Secrets are never stored here. `secret_ref` and `hf_secret_ref` are **vault entry ids**
/// ([`crate::secret`]) pointing at the platform's one combined credential string and, where a model is gated, a
/// HuggingFace token. A config file is meant to be readable and diffable; a credential in it is
/// not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RentalAccount {
    /// Slug, operator-visible, unique among accounts.
    pub id: String,
    /// Human label, for example "Mana on Modal".
    #[serde(default)]
    pub label: String,
    /// Which platform bills for this credential.
    #[serde(default = "default_platform")]
    pub platform: RentalPlatform,
    /// What the credential is expected to cost.
    ///
    /// Defaults to [`RentalPlan::Unknown`] rather than `Free`: an account whose plan nobody has
    /// recorded yet must not read as free, because that is the reading that produces a surprise
    /// invoice.
    #[serde(default)]
    pub plan: RentalPlan,
    /// Vault entry id holding the platform's one combined credential string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    /// Vault entry id holding a HuggingFace token, when a chosen repo needs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hf_secret_ref: Option<String>,
    /// The platform-side ceiling on concurrent containers.
    ///
    /// Modal Starter documents **10 GPU concurrency** [facts]; other platforms document nothing, so
    /// this is `None` rather than a guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_containers: Option<u32>,
    /// Operator note, shown in the panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Fields this binary does not know about, preserved verbatim on round-trip.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_platform() -> RentalPlatform {
    RentalPlatform::Modal
}

impl RentalAccount {
    /// An account with the given id, everything else defaulted.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: String::new(),
            platform: RentalPlatform::Modal,
            plan: RentalPlan::Unknown,
            secret_ref: None,
            hf_secret_ref: None,
            max_concurrent_containers: None,
            note: None,
            extra: Map::new(),
        }
    }

    /// Whether this account holds a credential reference at all.
    ///
    /// A GPU account with no credential cannot deploy anything; the panel says so before the
    /// operator fills in the rest of a wizard.
    pub fn has_credential(&self) -> bool {
        self.secret_ref.as_deref().is_some_and(|s| !s.trim().is_empty())
    }

    /// The single, non-negotiable rule: an account's plan is never inferred.
    ///
    /// `Unknown` stays `Unknown`. This function exists so that the rule has one home, rather than
    /// being re-decided (and eventually mis-decided) in the panel, the CLI and the API.
    pub fn effective_plan(&self) -> RentalPlan {
        self.plan
    }

    /// Whether a `(Paid)` marker may be drawn for this account at all.
    ///
    /// Requirement 10's marker is truthful only when the plan is known. Combined with the
    /// per-GPU [`FreePlan`], this is the whole rule: no marker from `Unknown` on either side.
    pub fn plan_is_known(&self) -> bool {
        self.plan != RentalPlan::Unknown
    }

    /// Reason the account is not usable yet, if any.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.id.trim().is_empty() {
            out.push("rental account id is empty".to_string());
        }
        if !self.has_credential() {
            out.push(format!(
                "rental account {} has no secret_ref: it cannot deploy anything",
                self.id
            ));
        }
        if let Some(n) = self.max_concurrent_containers {
            if n == 0 {
                out.push(format!(
                    "rental account {} allows zero concurrent containers",
                    self.id
                ));
            }
        }
        out
    }
}

/// How a deployment is meant to be used.
///
/// The distinction is requirement 9: one request on a chosen GPU, *or* batching when requests pile
/// up on another, cheaper one. It is recorded because the two modes want different cards, different
/// `scaledown_window` values and different cost arithmetic — a batch lane wants the card that is
/// cheapest per *token*, which the operator's own measurements show is not the one with the lowest
/// hourly rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RentalMode {
    /// A live endpoint an interactive request can reach.
    #[default]
    Interactive,
    /// A lane that fills up with queued work and is drained in bulk.
    Batch,
}

impl RentalMode {
    /// The stable identifier written to config.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Batch => "batch",
        }
    }

    /// Parse a mode identifier.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "interactive" => Some(Self::Interactive),
            "batch" => Some(Self::Batch),
            _ => None,
        }
    }
}

/// A recorded intent to re-quantise a larger checkpoint for a smaller card.
///
/// **Planned and recorded, not executed.** Requantisation needs a GPU, a calibration dataset and an
/// offline tool; a wizard cannot do it, and pretending otherwise would be the same class of lie as a
/// fake `modal deploy`. The plan itself lives in [`crate::gpu::requantize`], where the arithmetic,
/// the feasibility gates and the "never synthesise `--quantization`" rule are unit-tested; this
/// alias is what a deployment stores next to the deployment's own fields.
pub use crate::gpu::requantize::RequantizePlan;

/// One model on one GPU, as stored in `Provider.extra["rental"]`.
///
/// Field-for-field this is the JSON contract in `docs/research/ARCHITECTURE-gpu-rentals.md` §3.2.
/// Every optional field stays optional: a deployment is authored over several steps in a wizard,
/// and a half-filled deployment must be recordable (and resumable) rather than rejected because
/// the operator has not chosen a `scaledown_window` yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RentalDeployment {
    /// [`RentalAccount::id`] this deployment bills against.
    pub account: String,
    /// HuggingFace repo id, for example `orcarouter/Qwen3.8-27B-Uncensored-FP8`.
    pub repo: String,
    /// Commit, tag or branch. `None` means the repo's default revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Catalog GPU id, e.g. `RTX-PRO-6000`. Hyphenated, as the platform spells it.
    pub gpu: String,
    /// How many of that card. Defaults to 1.
    #[serde(default = "default_gpu_count")]
    pub gpu_count: u32,
    /// Catalog quantisation id: `fp8`, `awq`, … or `none` for unquantised weights.
    pub weight_quant: String,
    /// vLLM `--kv-cache-dtype` value: `auto` | `fp8` | `fp8_e4m3` | `fp8_e5m2` | `bfloat16`.
    pub kv_cache_dtype: String,
    /// vLLM `--max-model-len`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_len: Option<u64>,
    /// vLLM `--max-num-seqs`: sequences per scheduling step, **not** simultaneous sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_num_seqs: Option<u32>,
    /// Modal `scaledown_window` in seconds.
    ///
    /// Recorded because it is the difference between paying for a boot once and paying for it every
    /// time the operator looks away. Modal documents a default of **60 s** and a range of
    /// **2–1200 s** [facts]; the catalog carries those bounds so the wizard can refuse a value the
    /// platform would reject at deploy time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_cooldown_secs: Option<u32>,
    /// Interactive or batch lane.
    #[serde(default)]
    pub mode: RentalMode,
    /// Requirement 9: the second, cheaper card the batch lane runs on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_gpu: Option<String>,
    /// Identifier of the generated deployment bundle, once one has been produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    /// Recorded intent to requantise, when the chosen card cannot execute the repo's weights.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requantize: Option<RequantizePlan>,
    /// The architecture facts read from the repo's `config.json`, when a probe has run.
    ///
    /// Stored rather than re-fetched because it is the *evidence* for the sizing numbers next to
    /// it: a panel that shows 1.8 M KV tokens must be able to show which layer count and head dim
    /// produced that figure, and an operator re-reading the deployment a week later must not need
    /// the network to see it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<ArchFacts>,
    /// The sizing result the budget fields were derived from, kept for the same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sizing: Option<Box<SizingResult>>,
    /// Fields this binary does not know about, preserved verbatim on round-trip.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_gpu_count() -> u32 {
    1
}

impl RentalDeployment {
    /// The smallest coherent deployment: an account, a repo and a card.
    pub fn new(account: impl Into<String>, repo: impl Into<String>, gpu: impl Into<String>) -> Self {
        Self {
            account: account.into(),
            repo: repo.into(),
            revision: None,
            gpu: gpu.into(),
            gpu_count: 1,
            weight_quant: "none".to_string(),
            kv_cache_dtype: "auto".to_string(),
            max_model_len: None,
            max_num_seqs: None,
            shutdown_cooldown_secs: None,
            mode: RentalMode::Interactive,
            batch_gpu: None,
            bundle_id: None,
            requantize: None,
            architecture: None,
            sizing: None,
            extra: Map::new(),
        }
    }

    /// Whether more than one card is rented, which multiplies both VRAM and the hourly bill.
    pub fn is_multi_gpu(&self) -> bool {
        self.gpu_count > 1
    }

    /// The total VRAM of the container, in bytes, given the catalog.
    ///
    /// `None` when the GPU id is not in the catalog — an unknown card has no VRAM to multiply, and
    /// substituting a plausible one is how a sizing figure becomes fiction.
    pub fn total_vram_bytes(&self) -> Option<u64> {
        let gpu = crate::gpu::catalog::gpu_by_id(&self.gpu)?;
        let count = u64::from(self.gpu_count.max(1));
        gpu.vram_bytes?.checked_mul(count)
    }

    /// The container's billed rate in USD per hour, given the catalog.
    pub fn price_usd_per_hour(&self) -> Option<f64> {
        let gpu = crate::gpu::catalog::gpu_by_id(&self.gpu)?;
        let count = f64::from(self.gpu_count.max(1));
        Some(gpu.price_usd_per_hour * count)
    }

    /// Problems that make this deployment unusable, as leaf field names plus a sentence.
    ///
    /// The panel maps each entry back to the field named first, so the operator sees the problem on
    /// the input that caused it rather than in a banner.
    pub fn problems(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if self.account.trim().is_empty() {
            out.push((
                "account".to_string(),
                "a rental deployment must name the account that pays for it".to_string(),
            ));
        }
        if self.repo.trim().is_empty() {
            out.push((
                "repo".to_string(),
                "a rental deployment must name a HuggingFace repo".to_string(),
            ));
        }
        if crate::gpu::catalog::gpu_by_id(&self.gpu).is_none() {
            out.push((
                "gpu".to_string(),
                format!("unknown GPU id {:?}; see GET /api/gpu/catalog", self.gpu),
            ));
        }
        if let Some(batch) = self.batch_gpu.as_deref() {
            if crate::gpu::catalog::gpu_by_id(batch).is_none() {
                out.push((
                    "batch_gpu".to_string(),
                    format!("unknown GPU id {batch:?}; see GET /api/gpu/catalog"),
                ));
            }
        }
        if self.gpu_count == 0 {
            out.push((
                "gpu_count".to_string(),
                "gpu_count must be at least 1".to_string(),
            ));
        }
        if crate::gpu::catalog::quant_method_by_id(&self.weight_quant).is_none() {
            out.push((
                "weight_quant".to_string(),
                format!(
                    "unknown quantisation {:?}; see the `quants` array in GET /api/gpu/catalog",
                    self.weight_quant
                ),
            ));
        }
        if crate::gpu::catalog::kv_cache_dtype_by_id(&self.kv_cache_dtype).is_none() {
            out.push((
                "kv_cache_dtype".to_string(),
                format!(
                    "unknown KV cache dtype {:?}; see the `kv_dtypes` array in GET /api/gpu/catalog",
                    self.kv_cache_dtype
                ),
            ));
        }
        if let Some(len) = self.max_model_len {
            if len == 0 {
                out.push((
                    "max_model_len".to_string(),
                    "max_model_len must be greater than zero".to_string(),
                ));
            }
        }
        if let Some(cooldown) = self.shutdown_cooldown_secs {
            let range = crate::gpu::catalog::modal_scaledown_window();
            if cooldown < range.min_seconds || cooldown > range.max_seconds {
                out.push((
                    "shutdown_cooldown_secs".to_string(),
                    format!(
                        "scaledown_window must be between {} and {} seconds; Modal rejects \
                         anything outside that range",
                        range.min_seconds, range.max_seconds
                    ),
                ));
            }
        }
        out
    }

    /// Problems that exist only because a *specific card* was chosen.
    ///
    /// Kept separate from [`Self::problems`] because these depend on the hardware legality matrix
    /// rather than on the shape of the request, and the panel renders them differently: a missing
    /// repo is a form error, whereas "this card cannot execute FP8" is a fact about the world the
    /// operator has to make a decision about.
    pub fn hardware_problems(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(gpu) = crate::gpu::catalog::gpu_by_id(&self.gpu) {
            let method = crate::gpu::catalog::quant_method_by_id(&self.weight_quant);
            let verdict = match method {
                Some(method) => crate::gpu::catalog::resolve_quant_for(gpu, method.id),
                None => None,
            };
            if let Some(verdict) = verdict {
                if !verdict.runnable() {
                    out.push((
                        "weight_quant".to_string(),
                        format!(
                            "{}: {} on {} — {}",
                            verdict.support.as_str(),
                            method.map(|m| m.label).unwrap_or(self.weight_quant.as_str()),
                            gpu.label,
                            verdict.reason
                        ),
                    ));
                }
            }
        }
        if let Some(batch) = self.batch_gpu.as_deref() {
            if let Some(gpu) = crate::gpu::catalog::gpu_by_id(batch) {
                if let Some(method) = crate::gpu::catalog::quant_method_by_id(&self.weight_quant) {
                    if let Some(verdict) = crate::gpu::catalog::resolve_quant_for(gpu, method.id) {
                        if !verdict.runnable() {
                            out.push((
                                "batch_gpu".to_string(),
                                format!(
                                    "batch card {} cannot run {}: {}",
                                    gpu.label, method.label, verdict.reason
                                ),
                            ));
                        }
                    }
                }
            }
        }
        out
    }
}

/// The wrapper stored under `Provider.extra["rental"]`.
///
/// The extra map is flat, so the deployment needs one key to live under; this type is what makes
/// that key explicit in one place instead of in every caller that parses it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RentalExtra {
    /// The deployment itself.
    pub rental: RentalDeployment,
}

/// One deployment attached to a provider: the provider id, the parsed deployment, and whether the
/// provider is enabled.
///
/// `GET /api/gpu/rentals` returns these alongside the accounts. They are built from config plus the
/// usage store, and this type carries only the config half.
#[derive(Debug, Clone, PartialEq)]
pub struct RentalDeploymentView {
    /// The [`crate::model::Provider`] id this deployment is stored on.
    pub provider_id: String,
    /// Whether that provider is enabled.
    pub enabled: bool,
    /// The parsed deployment.
    pub deployment: RentalDeployment,
}

/// Parse the `rental` object out of a provider's `extra` map.
///
/// The provider is handed in whole rather than as a raw map so the error message can name it: "the
/// GPU on provider `modal-qwen27b` is not a card I know" is actionable, "unknown GPU id" is not.
///
/// A provider with no `rental` key at all is simply not a rental deployment, which is `Ok(None)`,
/// **not** an error. A provider *with* a malformed `rental` key is an error, because a value that
/// is present but unreadable means the feature is misconfigured, and defaulting it would hide that.
pub fn parse_deployment(
    provider_id: &str,
    extra: &BTreeMap<String, Value>,
) -> Result<Option<RentalDeployment>> {
    let Some(raw) = extra.get(RENTAL_EXTRA_KEY) else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let parsed: RentalDeployment = serde_json::from_value(raw.clone()).map_err(|e| {
        Error::Config(format!(
            "provider {provider_id} has a malformed `{RENTAL_EXTRA_KEY}` object: {e}"
        ))
    })?;
    Ok(Some(parsed))
}

impl RentalDeployment {
    /// Serialise this deployment into a `Provider.extra` entry.
    ///
    /// The inverse of [`parse_deployment`], kept next to it so the storage shape has exactly one
    /// definition.
    pub fn to_extra_value(&self) -> Result<Value> {
        serde_json::to_value(self).map_err(|e| {
            Error::Config(format!(
                "could not serialise rental deployment for {}: {e}",
                self.repo
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_defaults_to_unknown_never_to_free() {
        // The whole point: an unrecorded plan must not read as free. If this ever flips, some
        // operator gets a surprise invoice and the panel was the one that said it was free.
        let account = RentalAccount::new("mana-modal");
        assert_eq!(account.plan, RentalPlan::Unknown);
        assert_eq!(account.effective_plan(), RentalPlan::Unknown);
        assert!(!account.plan_is_known());

        let from_json: RentalAccount = serde_json::from_str(r#"{"id":"x"}"#).unwrap();
        assert_eq!(from_json.plan, RentalPlan::Unknown);
    }

    #[test]
    fn a_plan_round_trips_through_its_config_spelling() {
        for plan in [RentalPlan::Free, RentalPlan::Paid, RentalPlan::Unknown] {
            assert_eq!(RentalPlan::parse(plan.as_str()), Some(plan));
            let json = serde_json::to_string(&plan).unwrap();
            assert_eq!(json, format!("{:?}", plan.as_str()));
        }
        assert_eq!(RentalPlan::parse("PAID"), Some(RentalPlan::Paid));
        assert_eq!(RentalPlan::parse("enterprise"), None);
    }

    #[test]
    fn custom_platforms_round_trip_through_the_custom_prefix() {
        for platform in [
            RentalPlatform::Modal,
            RentalPlatform::Cerebrium,
            RentalPlatform::Kaggle,
            RentalPlatform::Saturn,
            RentalPlatform::Molab,
            RentalPlatform::Custom("runpod".to_string()),
        ] {
            assert_eq!(RentalPlatform::parse(&platform.as_str()), platform);
        }
        // A bare unknown word is kept as the platform's own slug rather than silently rewritten
        // to Modal.
        assert_eq!(
            RentalPlatform::parse("vast.ai"),
            RentalPlatform::Custom("vast.ai".to_string())
        );
        assert_eq!(
            RentalPlatform::parse("  Custom:RunPod "),
            RentalPlatform::Custom("runpod".to_string())
        );
    }

    #[test]
    fn platform_and_plan_serialise_to_their_documented_spellings() {
        assert_eq!(
            serde_json::to_string(&RentalPlatform::Modal).unwrap(),
            "\"modal\""
        );
        assert_eq!(
            serde_json::to_string(&RentalPlan::Unknown).unwrap(),
            "\"unknown\""
        );
        // kebab-case, so the acronym-heavy platforms do not come back as `cerebri-um`.
        assert_eq!(
            serde_json::to_string(&RentalPlatform::Cerebrium).unwrap(),
            "\"cerebrium\""
        );
    }

    #[test]
    fn an_account_without_a_credential_reports_a_problem() {
        let mut account = RentalAccount::new("mana-modal");
        let problems = account.problems();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("no secret_ref"));

        account.secret_ref = Some("rental:mana-modal".to_string());
        assert!(account.has_credential());
        assert!(account.problems().is_empty());

        // Whitespace is not a credential.
        account.secret_ref = Some("   ".to_string());
        assert!(!account.has_credential());
    }

    #[test]
    fn a_zero_container_ceiling_is_a_problem_not_a_default() {
        let mut account = RentalAccount::new("a");
        account.secret_ref = Some("s".to_string());
        account.max_concurrent_containers = Some(0);
        assert!(account.problems().iter().any(|p| p.contains("zero concurrent")));
    }

    #[test]
    fn unknown_account_keys_are_preserved_on_round_trip() {
        let raw = r#"{"id":"a","label":"A","platform":"modal","future_field":{"x":1}}"#;
        let account: RentalAccount = serde_json::from_str(raw).unwrap();
        assert_eq!(account.extra["future_field"]["x"], 1);
        let back = serde_json::to_string(&account).unwrap();
        assert!(back.contains("future_field"));
    }

    #[test]
    fn deployment_defaults_are_single_card_unquantised_interactive() {
        let d = RentalDeployment::new("a", "org/repo", "L40S");
        assert_eq!(d.gpu_count, 1);
        assert!(!d.is_multi_gpu());
        assert_eq!(d.mode, RentalMode::Interactive);
        assert_eq!(d.weight_quant, "none");
        assert_eq!(d.kv_cache_dtype, "auto");
        assert!(d.problems().is_empty(), "{:?}", d.problems());
    }

    #[test]
    fn a_deployment_with_a_missing_gpu_count_still_deserialises_to_one_card() {
        // Hand-edited config and older files both omit it; a missing `gpu_count` must not mean
        // "zero cards", which would price and size as nothing.
        let d: RentalDeployment = serde_json::from_str(
            r#"{"account":"a","repo":"org/repo","gpu":"L40S","weight_quant":"fp8",
                "kv_cache_dtype":"fp8"}"#,
        )
        .unwrap();
        assert_eq!(d.gpu_count, 1);
    }

    #[test]
    fn an_unknown_gpu_is_named_in_the_problem() {
        let mut d = RentalDeployment::new("a", "org/repo", "A100-40");
        let problems = d.problems();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].0, "gpu");
        assert!(problems[0].1.contains("A100-40"));

        // The spelled-and-cased wrong id is still wrong: the catalog is exact.
        d.gpu = "a100-40gb".to_string();
        assert_eq!(d.problems().len(), 1);
        d.gpu = "A100-40GB".to_string();
        assert!(d.problems().is_empty());
    }

    #[test]
    fn scaledown_window_outside_the_platform_range_is_a_problem() {
        let mut d = RentalDeployment::new("a", "org/repo", "L40S");
        d.shutdown_cooldown_secs = Some(1);
        let problems = d.problems();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].0, "shutdown_cooldown_secs");
        assert!(problems[0].1.contains("2 and 1200"));

        d.shutdown_cooldown_secs = Some(1200);
        assert!(d.problems().is_empty());
        d.shutdown_cooldown_secs = Some(1201);
        assert_eq!(d.problems().len(), 1);
    }

    #[test]
    fn a_card_that_cannot_run_the_quant_is_a_hardware_problem_not_a_form_error() {
        // T4 tops out at compute capability 7.5, below the 8.9 that native FP8 GEMM needs.
        let mut d = RentalDeployment::new("a", "org/repo", "T4");
        d.weight_quant = "fp8".to_string();
        assert!(d.problems().is_empty(), "the form itself is fine");
        let hardware = d.hardware_problems();
        assert_eq!(hardware.len(), 1);
        assert_eq!(hardware[0].0, "weight_quant");
        assert!(hardware[0].1.contains("T4"), "{:?}", hardware[0]);
    }

    #[test]
    fn a_batch_card_that_cannot_run_the_quant_is_reported_against_that_field() {
        let mut d = RentalDeployment::new("a", "org/repo", "L40S");
        d.weight_quant = "fp8".to_string();
        d.mode = RentalMode::Batch;
        d.batch_gpu = Some("T4".to_string());
        let hardware = d.hardware_problems();
        assert_eq!(hardware.len(), 1);
        assert_eq!(hardware[0].0, "batch_gpu");
    }

    #[test]
    fn total_vram_and_price_scale_with_the_card_count() {
        let mut d = RentalDeployment::new("a", "org/repo", "H100");
        let one = d.total_vram_bytes().unwrap();
        let hour = d.price_usd_per_hour().unwrap();
        d.gpu_count = 4;
        assert!(d.is_multi_gpu());
        assert_eq!(d.total_vram_bytes().unwrap(), one * 4);
        assert!((d.price_usd_per_hour().unwrap() - hour * 4.0).abs() < 1e-9);

        d.gpu = "not-a-card".to_string();
        assert_eq!(d.total_vram_bytes(), None);
        assert_eq!(d.price_usd_per_hour(), None);
    }

    #[test]
    fn a_provider_with_no_rental_key_is_not_a_rental_deployment() {
        let extra = BTreeMap::new();
        assert_eq!(parse_deployment("p", &extra).unwrap(), None);

        let mut extra = BTreeMap::new();
        extra.insert(RENTAL_EXTRA_KEY.to_string(), Value::Null);
        assert_eq!(parse_deployment("p", &extra).unwrap(), None);
    }

    #[test]
    fn a_malformed_rental_object_is_an_error_that_names_the_provider() {
        let mut extra = BTreeMap::new();
        // `gpu` is required and absent.
        extra.insert(
            RENTAL_EXTRA_KEY.to_string(),
            serde_json::json!({"account": "a", "repo": "r"}),
        );
        let err = parse_deployment("modal-qwen27b", &extra).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("modal-qwen27b"), "{text}");
        assert!(text.contains("gpu"), "{text}");
    }

    #[test]
    fn a_deployment_round_trips_through_provider_extra() {
        let mut d = RentalDeployment::new("mana-modal", "orcarouter/x-FP8", "RTX-PRO-6000");
        d.revision = Some("b32584cc".to_string());
        d.weight_quant = "fp8".to_string();
        d.kv_cache_dtype = "fp8".to_string();
        d.max_model_len = Some(262_144);
        d.max_num_seqs = Some(64);
        d.shutdown_cooldown_secs = Some(60);
        d.mode = RentalMode::Batch;
        d.batch_gpu = Some("L40S".to_string());
        d.requantize = Some(crate::gpu::requantize::plan_requantize(
            &crate::gpu::requantize::RecipeRequest::default(),
        ));

        let mut extra = BTreeMap::new();
        extra.insert(RENTAL_EXTRA_KEY.to_string(), d.to_extra_value().unwrap());
        assert_eq!(parse_deployment("p", &extra).unwrap(), Some(d));
    }

    #[test]
    fn an_unknown_key_inside_a_deployment_survives_a_round_trip() {
        // A v2 panel may add a deployment field this binary has never heard of; loading and saving
        // must not delete it.
        let raw = r#"{"account":"a","repo":"r","gpu":"L40S","weight_quant":"fp8",
                      "kv_cache_dtype":"fp8","future_knob":true}"#;
        let d: RentalDeployment = serde_json::from_str(raw).unwrap();
        assert_eq!(d.extra["future_knob"], true);
        let value = d.to_extra_value().unwrap();
        assert_eq!(value["future_knob"], true);
    }

    #[test]
    fn a_mode_round_trips_through_its_config_spelling() {
        for mode in [RentalMode::Interactive, RentalMode::Batch] {
            assert_eq!(RentalMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(RentalMode::parse("batch"), Some(RentalMode::Batch));
        assert_eq!(RentalMode::parse("streaming"), None);
    }

    #[test]
    fn fact_sources_use_the_spellings_the_catalog_contract_names() {
        // §5 of the architecture doc fixes these three words on the wire.
        assert_eq!(FactSource::Documented.as_str(), "documented");
        assert_eq!(FactSource::Observed.as_str(), "observed");
        assert_eq!(FactSource::Unverified.as_str(), "unverified");
        assert_eq!(
            serde_json::to_string(&FactSource::Observed).unwrap(),
            "\"observed\""
        );
    }

    #[test]
    fn free_plan_uses_the_spellings_the_catalog_contract_names() {
        assert_eq!(FreePlan::Available.as_str(), "available");
        assert_eq!(FreePlan::Unavailable.as_str(), "unavailable");
        assert_eq!(FreePlan::Unknown.as_str(), "unknown");
        assert_eq!(FreePlan::default(), FreePlan::Unknown);
        assert_eq!(
            serde_json::to_string(&FreePlan::Unavailable).unwrap(),
            "\"unavailable\""
        );
    }
}
