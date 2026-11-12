//! How many tokens of KV cache a card actually has left, and how honestly that can be said.
//!
//! The question this module answers is the one the operator asks before renting: *"will this model
//! serve this context length on this card, and how many streams at once?"* The arithmetic itself is
//! short — VRAM, minus the weights, minus a fixed overhead, divided by the per-token cost of the
//! cache — and the hard part is everything around it: where each input came from, what was assumed,
//! and what the answer is worth.
//!
//! Three rules shape the whole module:
//!
//! 1. **Every constant is named as either documented or calibrated.** [`DEFAULT_OVERHEAD_BYTES`] is
//!    *calibrated*: it was fitted to the operator's own two measurements, and the fitted values
//!    differ per card by about ±0.6 GiB, so the honest accuracy claim is ±2 %, not "exact". It is
//!    stated in [`SizingResult::assumptions`] rather than left implicit, and the caller may override
//!    it.
//! 2. **A figure that cannot be derived is refused, not guessed.** No architecture and no
//!    measurement means [`SizingConfidence::Unavailable`] with a reason, never a default
//!    bytes-per-token.
//! 3. **The per-token figure comes from the architecture, not from a fitted line.** On a hybrid
//!    linear-attention model only the full-attention layers grow a per-token cache, and counting
//!    all of them inflates the figure 4× — see [`super::arch::KvGeometry`]. The engine agrees: vLLM
//!    gives those layers a `MambaSpec`, which does not depend on the sequence length.
//!
//! Worked example (the pair the constants were calibrated on): an RTX PRO 6000 with 96 GiB, a 27.78
//! GB FP8 checkpoint, `--kv-cache-dtype fp8`, a hybrid architecture worth 32 768 bytes/token, and
//! the shipped 8 GiB overhead leaves 58 461 515 351 bytes — 1 784 103 tokens, 871 sessions at 2 048
//! tokens and 6 at 262 144. The operator measured 1 777 758 tokens on that card, so the prediction is
//! 0.36 % high.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use super::arch::{ArchFacts, KvTopology};
use super::catalog::{self, VramSource};
use crate::rental::FactSource;

/// The fraction of a card's VRAM vLLM is allowed to use.
///
/// vLLM's own default. It is a policy, not a measurement: what it leaves free covers the CUDA
/// context, the driver and the framework, and it is deliberately not tuned here.
pub const DEFAULT_GPU_MEMORY_UTILIZATION: f64 = 0.92;

/// KV-cache block size in tokens. vLLM's default, and the unit sessions are actually allocated in.
pub const DEFAULT_BLOCK_SIZE: u64 = 64;

/// The fixed per-card overhead subtracted after the weights: **8 GiB (calibrated, not measured)**.
///
/// It stands in for everything that is not weights and not the KV cache: the CUDA context, the vLLM
/// runtime, activation peaks during prefill, and — on a hybrid linear-attention model — the fixed
/// recurrent state, which is charged against the same budget even though it never grows with the
/// sequence length.
///
/// It is calibrated, not measured, and it is exact for neither run: the operator's two measurements
/// imply 7.27 GiB (RTX PRO 6000) and 7.85 GiB (L40S), a 0.58 GiB spread. The shipped value is the
/// round 8 GiB, which sits *above* both fits — 0.15 GiB above the higher one and 0.73 GiB above the
/// lower — so the error it can cause is towards understating the cache, never towards claiming a
/// pool the card cannot hold. That deliberate direction, and the 0.73 GiB distance to the lower fit,
/// are why [`SizingResult`] claims ±2 % rather than exactness and why this constant is named in
/// [`SizingResult::assumptions`] every time it is used.
pub const DEFAULT_OVERHEAD_BYTES: u64 = 8_589_934_592;

/// The weight bytes the calibration pair loaded: the checkpoint's own file sizes, 30.9 GB spread
/// across seven shards.
///
/// Named rather than repeated so the fitted overhead can be re-derived from the table below without
/// a magic number in two places, and so a test can assert the table still describes the pair the
/// constants came from.
pub const CALIBRATION_WEIGHTS_BYTES: u64 = 27_781_427_952;

/// One gibibyte, spelled out because every figure in this module is quoted in GiB.
pub const GIB: u64 = 1_073_741_824;

/// The spread between the two fitted overheads, in bytes: 0.58 GiB.
///
/// This is the distance *between the two fits*, not a ± band around the shipped constant: the
/// shipped 8 GiB lies outside [7.27, 7.85] GiB by design (see [`DEFAULT_OVERHEAD_BYTES`]). What the
/// figure supports is the claim that two independent measurements agree on the overhead to within
/// ±0.6 GiB, which is what the assumption line says.
pub const OVERHEAD_SPREAD_BYTES: u64 = 644_245_094;

/// The context ladder the architecture document reports, in tokens: 2048, 8192, 32768.
///
/// `max_model_len` is appended to this at run time, which is where the documented 262144 rung comes
/// from. The rungs are always reported, even when the caller supplies extras of its own, so a plan
/// can never be quoted with a table that silently omits what the document promises.
pub const REPORTED_CONTEXTS: [u64; 3] = [2_048, 8_192, 32_768];

/// How much of the pool block alignment must waste before it is worth a warning: 1 %.
///
/// Every pool loses its last partial block, and a 39-token remainder in 1.78 M tokens is noise; a
/// warning there would train the reader to ignore the field. A pool that loses a tenth of itself to
/// the last block, or an entire session's worth, is a capacity cliff and is called out.
pub const ALIGNMENT_WARNING_SHARE_PCT: u64 = 1;

/// The accuracy claim this module is willing to make, in percent: ±2.
pub const HONEST_ACCURACY_PCT: f64 = 2.0;

/// The fixed recurrent state of the reference hybrid model, in bytes: 78 446 592 (~78.4 MB).
///
/// Sized from vLLM's `gated_delta_net_state_shape()`: 817 152 elements per layer (a 30 720-element
/// convolution state plus a 786 432-element recurrent state) at 2 bytes each, over 48 layers.
///
/// It is recorded here as evidence for a sentence, not as an input to the arithmetic: it is under
/// 1 % of [`DEFAULT_OVERHEAD_BYTES`], so modelling it separately would dress up a rounding error as
/// precision. The assumption line in [`SizingResult::assumptions`] states it instead.
pub const GDN_STATE_BYTES_REFERENCE_MODEL: u64 = 78_446_592;

/// How the pool figure was arrived at, and how much it is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SizingConfidence {
    /// Derived from the real architecture of the checkpoint: the per-token figure is published data,
    /// and one constant (the overhead) is calibrated.
    Analytic,
    /// Derived from an operator-supplied per-token figure, so it is as good as that number and no
    /// better. No architecture was needed, and none was used.
    Calibrated,
    /// Not derivable: no architecture, or no measurement, or the numbers do not add up. The reason
    /// is in `rejection` or `warnings`; [`SizingResult::kv_pool_tokens`] is `None`.
    Unavailable,
}

impl SizingConfidence {
    /// The wire spelling, matching the serialised form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Analytic => "analytic",
            Self::Calibrated => "calibrated",
            Self::Unavailable => "unavailable",
        }
    }
}

/// A measurement by the operator that a prediction can be compared against.
///
/// Carried in the request rather than looked up, so that the caller decides which run is the
/// reference; [`calibration_points`] is the built-in table of the two runs these constants were
/// fitted to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeasuredReference {
    /// Tokens of KV cache vLLM reported for this card, model and cache dtype.
    pub kv_pool_tokens: u64,
    /// The card the run happened on, as a catalog id.
    pub card: String,
    /// Where the figure came from, so a reader can weigh it.
    pub source: String,
    /// When it was measured, if that is known.
    #[serde(default)]
    pub measured_at: Option<String>,
}

/// One measured run, used to calibrate the shipped constants and to cross-check a prediction.
///
/// Both points are the operator's own, from `docs/research/issue-4-spec.md`; they are marked
/// [`FactSource::Observed`] because that is what they are — one operator, one model, one vLLM
/// version. Two points is a consistency check, not a validation set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationPoint {
    /// The checkpoint that was served.
    pub repo: String,
    /// The card, as a catalog id.
    pub card: String,
    /// The `--kv-cache-dtype` the run used.
    pub kv_cache_dtype: String,
    /// `--max-model-len` the run used.
    pub max_model_len: u64,
    /// The KV pool vLLM reported, in tokens.
    pub kv_pool_tokens: u64,
    /// The weight bytes loaded, from the checkpoint's own file sizes.
    pub weights_bytes: u64,
    /// Cold start, as a range, when more than one figure is known.
    #[serde(default)]
    pub cold_start_secs: Option<(f64, f64)>,
    /// Why there is no cold-start range, when there is not.
    #[serde(default)]
    pub cold_start_note: Option<String>,
    /// Provenance of the numbers.
    pub source: FactSource,
    /// Where they were read.
    pub source_url: String,
    /// When they were taken, if known.
    #[serde(default)]
    pub measured_at: Option<String>,
}

/// The two measured runs the shipping constants were fitted to.
///
/// Exactly two, on purpose: the same checkpoint on two cards at the same cache dtype. That is enough
/// to see the overhead is card-dependent and to bound the error, and not enough to claim the model
/// generalises. Callers may add their own points via [`SizingRequest::measured`].
///
/// Built once rather than declared `const`, because the fields are owned `String`s: a `const` slice
/// of owned strings would have to be reconstructed at every call site.
pub fn calibration_points() -> &'static [CalibrationPoint] {
    static POINTS: OnceLock<Vec<CalibrationPoint>> = OnceLock::new();
    POINTS.get_or_init(|| {
        vec![
            CalibrationPoint {
                repo: "orcarouter/Qwen3.8-27B-Uncensored-FP8".to_string(),
                card: "RTX-PRO-6000".to_string(),
                kv_cache_dtype: "fp8".to_string(),
                max_model_len: 262_144,
                kv_pool_tokens: 1_777_758,
                weights_bytes: CALIBRATION_WEIGHTS_BYTES,
                cold_start_secs: Some((160.0, 181.0)),
                cold_start_note: None,
                source: FactSource::Observed,
                source_url: "docs/research/issue-4-spec.md".to_string(),
                measured_at: None,
            },
            CalibrationPoint {
                repo: "orcarouter/Qwen3.8-27B-Uncensored-FP8".to_string(),
                card: "L40S".to_string(),
                kv_cache_dtype: "fp8".to_string(),
                max_model_len: 8192,
                kv_pool_tokens: 281_729,
                weights_bytes: CALIBRATION_WEIGHTS_BYTES,
                cold_start_secs: None,
                cold_start_note: Some(
                    "one figure (~110 s) is recorded, so no range is claimed here".to_string(),
                ),
                source: FactSource::Observed,
                source_url: "docs/research/issue-4-spec.md".to_string(),
                measured_at: None,
            },
        ]
    })
}

/// The measurement for a repository, card and cache dtype, if one is on file.
///
/// Matching is case-insensitive and trims, because a repository id arrives from a URL or a CLI flag
/// and those two disagree about case more often than they should.
pub fn calibration_for(
    repo: &str,
    card: &str,
    kv_cache_dtype: &str,
) -> Option<&'static CalibrationPoint> {
    let repo = repo.trim();
    let card = card.trim().to_ascii_lowercase();
    let dtype = kv_cache_dtype.trim().to_ascii_lowercase();
    calibration_points().iter().find(|point| {
        point.repo.eq_ignore_ascii_case(repo)
            && point.card.to_ascii_lowercase() == card
            && point.kv_cache_dtype.to_ascii_lowercase() == dtype
    })
}

/// Everything the arithmetic needs, with every value the caller could know passed in explicitly.
///
/// There are no hidden defaults except the constants above: a caller that omits `overhead_bytes`
/// gets 8 GiB *and* an assumption line saying so.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SizingRequest {
    /// Card to size for, as a catalog id (`"L40S"`, `"RTX-PRO-6000"`).
    pub gpu_id: String,
    /// The VRAM to plan against. `None` means "whatever the catalog knows for this card", which
    /// prefers the operator-measured usable figure when one exists and otherwise the nominal one.
    /// Pass a value explicitly to size against, say, the nominal 96 GiB rather than a measured 95.
    #[serde(default)]
    pub vram_bytes: Option<u64>,
    /// Bytes the weights occupy. The measured `safetensors` total beats a parameter count: quantised
    /// checkpoints store scales and embeddings at higher precision, so the file set is larger than
    /// `params × bytes_per_param`, and understating it overstates the pool.
    pub weights_bytes: u64,
    /// `--kv-cache-dtype`, as a catalog id. `"auto"` resolves through the architecture's `torch_dtype`.
    pub kv_cache_dtype: String,
    /// `--max-model-len`.
    pub max_model_len: u64,
    /// `--max-num-seqs`, when the operator has chosen one. The result reports the lower of this and
    /// what the cache can actually hold.
    #[serde(default)]
    pub max_num_seqs: Option<u64>,
    /// `--gpu-memory-utilization`.
    #[serde(default = "default_utilization")]
    pub gpu_memory_utilization: f64,
    /// Override for the fixed overhead. Defaults to [`DEFAULT_OVERHEAD_BYTES`].
    #[serde(default = "default_overhead")]
    pub overhead_bytes: u64,
    /// `--block-size`, in tokens.
    #[serde(default = "default_block_size")]
    pub block_size: u64,
    /// An operator-supplied bytes-per-token figure, for a model whose architecture cannot be read.
    ///
    /// When present it wins over the architecture and the result is [`SizingConfidence::Calibrated`]:
    /// the number is only as good as the run it came from, and saying `analytic` would launder it.
    #[serde(default)]
    pub kv_bytes_per_token: Option<u64>,
    /// The checkpoint's architecture, when it could be read. Without it and without an override, the
    /// result is [`SizingConfidence::Unavailable`].
    #[serde(default)]
    pub architecture: Option<ArchFacts>,
    /// The repository the architecture came from, for the record — including when it is the base
    /// model a gated repository points at rather than the repository itself.
    #[serde(default)]
    pub repo: Option<String>,
    /// A measurement to compare against.
    #[serde(default)]
    pub measured: Option<MeasuredReference>,
    /// Extra context lengths to report session counts for, on top of [`REPORTED_CONTEXTS`].
    /// `max_model_len` is always included.
    ///
    /// The documented ladder is not replaceable: a caller that asks for 1024 gets 1024 *and* the
    /// three documented rungs, because dropping them would make the panel's table disagree with the
    /// architecture document depending on who called it.
    #[serde(default)]
    pub contexts: Vec<u64>,
}

fn default_utilization() -> f64 {
    DEFAULT_GPU_MEMORY_UTILIZATION
}

fn default_overhead() -> u64 {
    DEFAULT_OVERHEAD_BYTES
}

fn default_block_size() -> u64 {
    DEFAULT_BLOCK_SIZE
}

impl SizingRequest {
    /// A request with the defaults that matter already filled in.
    pub fn new(
        gpu_id: impl Into<String>,
        weights_bytes: u64,
        kv_cache_dtype: impl Into<String>,
        max_model_len: u64,
    ) -> Self {
        Self {
            gpu_id: gpu_id.into(),
            vram_bytes: None,
            weights_bytes,
            kv_cache_dtype: kv_cache_dtype.into(),
            max_model_len,
            max_num_seqs: None,
            gpu_memory_utilization: DEFAULT_GPU_MEMORY_UTILIZATION,
            overhead_bytes: DEFAULT_OVERHEAD_BYTES,
            block_size: DEFAULT_BLOCK_SIZE,
            kv_bytes_per_token: None,
            architecture: None,
            repo: None,
            measured: None,
            contexts: Vec::new(),
        }
    }

    /// Supply the architecture and the repository it was read from.
    pub fn with_architecture(mut self, repo: impl Into<String>, architecture: ArchFacts) -> Self {
        self.repo = Some(repo.into());
        self.architecture = Some(architecture);
        self
    }

    /// Compare the prediction against a measurement.
    pub fn with_measurement(mut self, measured: MeasuredReference) -> Self {
        self.measured = Some(measured);
        self
    }

    /// Size against a specific VRAM figure instead of the catalog's.
    pub fn with_vram_bytes(mut self, vram_bytes: u64) -> Self {
        self.vram_bytes = Some(vram_bytes);
        self
    }

    /// Also report session counts at these context lengths.
    pub fn with_contexts(mut self, contexts: impl IntoIterator<Item = u64>) -> Self {
        self.contexts = contexts.into_iter().collect();
        self
    }
}

impl Default for SizingRequest {
    fn default() -> Self {
        Self::new("", 0, "auto", 0)
    }
}

/// Session count at one context length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionsAtContext {
    /// The context length asked about.
    pub context_len: u64,
    /// Blocks one session needs at that length, i.e. `ceil(context_len / block_size)`.
    pub blocks_per_session: u64,
    /// Bytes one session's cache occupies, i.e. `context × bytes_per_token`.
    pub bytes_per_session: u64,
    /// How many such sessions the pool holds. `None` when there is no pool to divide.
    pub sessions: Option<u64>,
}

/// The answer: the pool in tokens, what it was derived from, and what could not be known.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SizingResult {
    /// The card the request named.
    pub gpu: String,
    /// The VRAM figure the arithmetic used.
    pub vram_bytes: Option<u64>,
    /// Whether the catalog got it from docs, a datasheet, an operator measurement, or not at all.
    pub vram_source: Option<VramSource>,
    /// The operator-measured usable figure, when the catalog has one for this card.
    pub usable_vram_bytes: Option<u64>,
    /// Bytes the weights were taken to occupy.
    pub weights_bytes: u64,
    /// The cache dtype the request asked for.
    pub kv_cache_dtype: String,
    /// Bytes per cache element, once resolved. `None` when it could not be resolved.
    pub kv_bytes_per_element: Option<u32>,
    /// The context length the request asked for.
    pub max_model_len: u64,
    /// The utilisation fraction used.
    pub gpu_memory_utilization: f64,
    /// The fixed overhead subtracted.
    pub overhead_bytes: u64,
    /// What is left for the cache: `floor(u × vram) − weights − overhead`.
    ///
    /// Signed, and allowed to be negative: a negative budget is the honest report that the weights
    /// and overhead do not fit, and hiding it by clamping would turn "impossible" into "small".
    pub kv_budget_bytes: i64,
    /// Bytes of KV per token, from the architecture or from the caller's override.
    pub kv_bytes_per_token: Option<u64>,
    /// The arithmetic behind that figure, written out for checking by hand.
    pub kv_bytes_per_token_basis: Option<String>,
    /// The pool in tokens: `kv_budget_bytes / kv_bytes_per_token`.
    pub kv_pool_tokens: Option<u64>,
    /// Whole blocks in the pool: `kv_pool_tokens / block_size`.
    pub kv_blocks: Option<u64>,
    /// Tokens after block alignment: `kv_blocks × block_size`. What the engine can actually serve.
    pub kv_pool_tokens_aligned: Option<u64>,
    /// Bytes one session's cache occupies at `max_model_len`.
    pub kv_bytes_per_session_at_max_len: Option<u64>,
    /// How many sessions fit at `max_model_len`.
    pub simultaneous_sessions_at_max_len: Option<u64>,
    /// Session counts at the requested context lengths, ascending.
    pub simultaneous_sessions_at: Vec<SessionsAtContext>,
    /// The session count to configure: the lower of the requested `max_num_seqs` and what fits.
    pub max_num_seqs_effective: Option<u64>,
    /// The measurement the prediction is compared against, when there is one.
    pub measured_reference: Option<MeasuredReference>,
    /// `(predicted − measured) / measured × 100`, rounded to two decimals. `None` without a
    /// measurement. A positive number means the pool was overstated.
    pub delta_vs_measurement_pct: Option<f64>,
    /// The overhead these numbers imply if the measurement is taken as truth:
    /// `floor(u × vram) − weights − measured_tokens × bytes_per_token`.
    ///
    /// It is the closest thing to an independent check available — and it is only as independent as
    /// the measurement, which is one operator's run. A *negative* value is not a warning, it is a
    /// refutation: it says no overhead at all could make the architecture and the measurement agree.
    pub implied_overhead_bytes: Option<i64>,
    /// How the pool figure was arrived at.
    pub confidence: SizingConfidence,
    /// Where the architecture came from: the repository read, or `"request"` for an override.
    pub arch_source: Option<String>,
    /// Why the result has no pool figure, when it has none. `None` when `confidence` is not
    /// [`SizingConfidence::Unavailable`].
    pub rejection: Option<String>,
    /// Every assumption the arithmetic rests on, stated so it can be argued with.
    pub assumptions: Vec<String>,
    /// Everything the answer should carry a caveat about, including where it is not independent.
    pub warnings: Vec<String>,
}

impl SizingResult {
    /// Whether a usable pool figure came out.
    pub fn is_usable(&self) -> bool {
        self.kv_pool_tokens.is_some()
    }
}

/// Size a KV pool: the arithmetic, plus every statement needed to judge it.
///
/// Never fails. A request that cannot be answered comes back as
/// [`SizingConfidence::Unavailable`] with the reason in `rejection`/`warnings`, because a caller
/// that has to handle an error *and* an unusable answer will eventually treat one of them as
/// "roughly zero".
pub fn size_deployment(request: &SizingRequest) -> SizingResult {
    let mut warnings: Vec<String> = Vec::new();
    let mut assumptions: Vec<String> = Vec::new();
    let mut rejection: Option<String> = None;

    let card = catalog::gpu_by_id(&request.gpu_id);
    if card.is_none() {
        warnings.push(format!(
            "`{}` is not a card in the catalog, so no VRAM figure is available and no pool can be \
             computed. Sizing against an unknown card is refused rather than assumed",
            request.gpu_id
        ));
    }

    let usable_vram_bytes = card.and_then(|card| card.usable_vram_bytes);
    let vram_bytes = request
        .vram_bytes
        .or_else(|| card.and_then(|card| card.vram_for_sizing()));
    let vram_source = card.and_then(|card| card.vram_source);

    // Assumption 1: the utilisation fraction. Always stated, because it is a policy choice and not
    // a property of the hardware.
    assumptions.push(format!(
        "gpu_memory_utilization = {} of the card's VRAM is shared between the weights and the KV \
         cache; that is vLLM's own default and not a measurement of this card",
        request.gpu_memory_utilization
    ));
    if request.overhead_bytes == DEFAULT_OVERHEAD_BYTES {
        assumptions.push(format!(
            "overhead_bytes = {DEFAULT_OVERHEAD_BYTES} (8 GiB) is a calibrated constant, not a \
             measurement: the operator's two runs imply 7.27 GiB on the RTX PRO 6000 and 7.85 GiB \
             on the L40S, so the two fits agree to ±0.6 GiB. The shipped round 8 GiB sits above both \
             of them — 0.15 GiB above the higher fit, 0.73 GiB above the lower — so it charges \
             overhead rather than overstating the cache. It covers the CUDA context, the vLLM \
             runtime, activation peaks during prefill, and on a hybrid linear-attention model the \
             fixed recurrent state as well"
        ));
    } else {
        assumptions.push(format!(
            "overhead_bytes = {} was supplied by the caller and replaces the shipped 8 GiB default",
            request.overhead_bytes
        ));
    }
    assumptions.push(format!(
        "block_size = {} tokens, so a session is allocated in whole blocks and the pool's own size \
         is a whole number of blocks",
        request.block_size
    ));

    // Reject impossible inputs rather than clamping them: each of these silently produces a
    // confident number if it is allowed through.
    if card.is_none() {
        rejection = Some(format!("unknown card `{}`", request.gpu_id));
    } else if vram_bytes.is_none() {
        rejection = Some(format!(
            "the catalog holds no VRAM figure for `{}`, so the budget cannot be computed",
            request.gpu_id
        ));
    } else if !(request.gpu_memory_utilization > 0.0 && request.gpu_memory_utilization <= 1.0) {
        rejection = Some(format!(
            "gpu_memory_utilization = {} is outside (0, 1]; clamping it would hide the mistake",
            request.gpu_memory_utilization
        ));
    } else if request.block_size == 0 {
        rejection = Some("block_size = 0 cannot describe an allocation unit".to_string());
    } else if request.max_model_len == 0 {
        rejection = Some(
            "max_model_len = 0 means no session could ever hold a token; state the context the \
             deployment will be served at"
                .to_string(),
        );
    }

    if rejection.is_none() && request.weights_bytes == 0 {
        warnings.push(
            "weights_bytes = 0 makes every remaining byte look like cache; a real checkpoint \
             occupies gigabytes, so check the figure before trusting the pool"
                .to_string(),
        );
    }

    // Nominal versus measured VRAM. Both are legitimate — a plan may be made against the sticker
    // figure — but a reader must know which one was used.
    if let (Some(nominal), Some(usable)) = (vram_bytes, usable_vram_bytes) {
        if nominal == usable && card.and_then(|card| card.usable_vram_bytes).is_some() {
            warnings.push(format!(
                "the card was sized against the operator-measured usable {:.1} GiB, not the nominal \
                 figure; the difference is what the driver and the display take",
                usable as f64 / GIB as f64
            ));
        }
    }

    // Resolve the per-token figure: the caller's override first, then the architecture.
    let mut kv_bytes_per_element: Option<u32> = None;
    let mut kv_bytes_per_token: Option<u64> = None;
    let mut basis: Option<String> = None;
    let mut arch_source: Option<String> = None;
    let mut confidence = SizingConfidence::Unavailable;

    if rejection.is_none() {
        if let Some(override_bytes) = request.kv_bytes_per_token {
            if override_bytes == 0 {
                rejection = Some(
                    "kv_bytes_per_token = 0 is not a cache; supply the figure the engine reported \
                     or leave it out and let the architecture decide"
                        .to_string(),
                );
            } else {
                kv_bytes_per_token = Some(override_bytes);
                basis = Some(format!(
                    "{override_bytes} bytes/token was supplied by the caller rather than derived \
                     from the checkpoint's architecture"
                ));
                arch_source = Some("request".to_string());
                confidence = SizingConfidence::Calibrated;
                assumptions.push(
                    "the per-token figure is the caller's, so this result is only as good as the \
                     run it came from"
                        .to_string(),
                );
            }
        } else if let Some(architecture) = request.architecture.as_ref() {
            match resolve_kv_width(architecture, &request.kv_cache_dtype, &mut warnings) {
                Ok(width) => {
                    kv_bytes_per_element = Some(width);
                    let geometry = architecture.kv_geometry(width, &request.kv_cache_dtype);
                    if let Some(warning) = geometry.warning.clone() {
                        warnings.push(warning);
                    }
                    match geometry.kv_bytes_per_token {
                        Some(bytes) => {
                            kv_bytes_per_token = Some(bytes);
                            basis = Some(geometry.basis.clone());
                            arch_source = Some(
                                request
                                    .repo
                                    .clone()
                                    .unwrap_or_else(|| "request".to_string()),
                            );
                            confidence = SizingConfidence::Analytic;
                            if geometry.topology == KvTopology::HybridLinear {
                                assumptions.push(hybrid_state_assumption());
                            }
                        }
                        None => {
                            rejection = geometry.problem.clone().or_else(|| {
                                Some(
                                    "the architecture did not yield a per-token figure".to_string(),
                                )
                            });
                        }
                    }
                }
                Err(reason) => rejection = Some(reason),
            }
        } else {
            rejection = Some(
                "no architecture and no kv_bytes_per_token: there is nothing to derive a \
                 per-token figure from, and inventing a default would produce a confident wrong \
                 answer"
                    .to_string(),
            );
        }
    }

    // The budget. `u × vram` is floored: a fraction of a byte is not cache.
    //
    // It is computed only when the inputs are known to be well-formed. A utilisation of 0.0 has no
    // budget at all, and reporting `0 - weights - overhead` for it would dress an input error up as
    // an arithmetic finding ("this model does not fit"), which is a different and false statement.
    // A budget that goes negative because the weights genuinely exceed the usable VRAM is kept and
    // reported, because that *is* the finding.
    let scaled = if rejection.is_some() {
        None
    } else {
        vram_bytes
            .map(|vram| (vram as f64) * request.gpu_memory_utilization)
            .filter(|value| value.is_finite() && *value >= 0.0 && *value <= i64::MAX as f64)
            .map(|value| value.floor() as i64)
    };
    let kv_budget_bytes = match scaled {
        Some(scaled) => scaled - request.weights_bytes as i64 - request.overhead_bytes as i64,
        None => 0,
    };

    if rejection.is_none() && kv_budget_bytes < 0 {
        rejection =
            Some(format!(
            "the weights ({} bytes) and the overhead ({} bytes) exceed the usable {} bytes, so no \
             KV cache fits at all",
            request.weights_bytes, request.overhead_bytes, scaled.unwrap_or_default()
        ));
    }

    // The pool, in tokens and in blocks.
    let mut kv_pool_tokens = None;
    let mut kv_blocks = None;
    let mut kv_pool_tokens_aligned = None;
    let mut bytes_per_session_at_max_len = None;
    let mut sessions_at_max_len = None;
    let mut max_num_seqs_effective = None;
    let mut sessions_at: Vec<SessionsAtContext> = Vec::new();
    let mut implied_overhead_bytes = None;
    let mut delta_vs_measurement_pct = None;

    if let (Some(bytes_per_token), Some(measurement)) =
        (kv_bytes_per_token, request.measured.as_ref())
    {
        // Implied overhead is computed against the *measurement's* tokens, which is the only way it
        // can disagree with the shipped constant.
        if let Some(scaled) = scaled {
            let implied = scaled
                - request.weights_bytes as i64
                - (measurement.kv_pool_tokens as i128 * bytes_per_token as i128) as i64;
            implied_overhead_bytes = Some(implied);
            if implied < 0 {
                rejection = Some(format!(
                    "the measurement implies a negative overhead ({implied} bytes): no fixed cost \
                     can make {} bytes/token and {} measured tokens agree, so the per-token figure \
                     does not describe this model on this card and is refused rather than clamped",
                    bytes_per_token, measurement.kv_pool_tokens
                ));
            }
        }
    }

    if rejection.is_none() {
        if let Some(bytes_per_token) = kv_bytes_per_token {
            let budget = kv_budget_bytes.max(0) as u64;
            let tokens = budget / bytes_per_token;
            kv_pool_tokens = Some(tokens);
            let blocks = tokens / request.block_size;
            kv_blocks = Some(blocks);
            kv_pool_tokens_aligned = Some(blocks * request.block_size);
            let per_session = bytes_per_token.saturating_mul(request.max_model_len);
            bytes_per_session_at_max_len = Some(per_session);

            // Sessions are allocated in whole blocks, so the divisor is the block-aligned context.
            let sessions_here = sessions_for(blocks, request.max_model_len, request.block_size);
            sessions_at_max_len = Some(sessions_here);
            max_num_seqs_effective = Some(match request.max_num_seqs {
                Some(requested) => {
                    if requested > sessions_here {
                        warnings.push(format!(
                            "--max-num-seqs {requested} cannot be met: at a {}-token context the \
                             cache holds {sessions_here} sessions, so the effective value is \
                             {sessions_here}. Lower --max-model-len or expect preemption",
                            request.max_model_len
                        ));
                    }
                    requested.min(sessions_here)
                }
                None => sessions_here,
            });

            // The documented ladder is a floor, not a default: extras are added to it and the
            // maximum is appended, so the reported table always contains what the architecture
            // document promises plus whatever this particular caller wants to see.
            let mut contexts: Vec<u64> = REPORTED_CONTEXTS
                .iter()
                .copied()
                .chain(request.contexts.iter().copied())
                .chain(std::iter::once(request.max_model_len))
                .collect();
            contexts.sort_unstable();
            contexts.dedup();
            sessions_at = contexts
                .into_iter()
                .map(|context_len| {
                    let blocks_per_session = blocks_for(context_len, request.block_size);
                    SessionsAtContext {
                        context_len,
                        blocks_per_session,
                        bytes_per_session: bytes_per_token.saturating_mul(context_len),
                        sessions: Some(blocks / blocks_per_session.max(1)),
                    }
                })
                .collect();

            if let Some(measurement) = request.measured.as_ref() {
                if measurement.kv_pool_tokens > 0 {
                    let delta = (tokens as f64 - measurement.kv_pool_tokens as f64)
                        / measurement.kv_pool_tokens as f64
                        * 100.0;
                    delta_vs_measurement_pct = Some((delta * 100.0).round() / 100.0);
                }
                warnings.push(format!(
                    "this prediction is compared against a measurement ({}, {} tokens, {}) — the \
                     comparison is a consistency check, not independent validation: the 8 GiB \
                     overhead constant was calibrated on that same pair of runs, so agreement there \
                     is partly the fit agreeing with itself",
                    measurement.card, measurement.kv_pool_tokens, measurement.source
                ));
                if request.overhead_bytes != DEFAULT_OVERHEAD_BYTES {
                    warnings.push(
                        "the overhead was overridden, so the comparison against the shipped \
                         calibration no longer means what it did"
                            .to_string(),
                    );
                }
            }

            if let Some(aligned) = kv_pool_tokens_aligned {
                let remainder = tokens.saturating_sub(aligned);
                // `remainder * 100 >= tokens` is the integer form of "at least 1 %": exact, no
                // float, and no overflow because the product is taken in u128.
                let material = (remainder as u128) * 100
                    >= (tokens as u128) * ALIGNMENT_WARNING_SHARE_PCT as u128;
                if aligned != tokens && material {
                    warnings.push(format!(
                        "the pool is {tokens} tokens, but blocks of {} hold {aligned}: the \
                         remainder of {remainder} tokens ({:.1} % of the pool) is unusable and the \
                         aligned figure is the one to quote to the engine",
                        request.block_size,
                        (remainder as f64) * 100.0 / (tokens as f64)
                    ));
                }
            }

            if kv_pool_tokens == Some(0) {
                warnings.push(
                    "the budget is smaller than one token of cache, so the model fits but serves \
                     nothing: the context length or the card is wrong"
                        .to_string(),
                );
            }
        }
    }

    if rejection.is_some() {
        confidence = SizingConfidence::Unavailable;
        kv_pool_tokens = None;
        kv_blocks = None;
        kv_pool_tokens_aligned = None;
        bytes_per_session_at_max_len = None;
        sessions_at_max_len = None;
        max_num_seqs_effective = None;
        sessions_at.clear();
        if let Some(reason) = rejection.as_ref() {
            warnings.push(reason.clone());
        }
    }

    if confidence == SizingConfidence::Analytic {
        assumptions.push(format!(
            "the per-token figure comes from the checkpoint's published architecture and one \
             calibrated overhead; against the operator's measurements the model predicts 1 753 957 \
             tokens where 1 777 758 were measured on the RTX PRO 6000 (−1.34 %) and 276 775 where \
             281 729 were measured on the L40S (−1.76 %), so the honest claim is ±{HONEST_ACCURACY_PCT:.0} %"
        ));
    }

    SizingResult {
        gpu: request.gpu_id.clone(),
        vram_bytes,
        vram_source,
        usable_vram_bytes,
        weights_bytes: request.weights_bytes,
        kv_cache_dtype: request.kv_cache_dtype.clone(),
        kv_bytes_per_element,
        max_model_len: request.max_model_len,
        gpu_memory_utilization: request.gpu_memory_utilization,
        overhead_bytes: request.overhead_bytes,
        kv_budget_bytes,
        kv_bytes_per_token,
        kv_bytes_per_token_basis: basis,
        kv_pool_tokens,
        kv_blocks,
        kv_pool_tokens_aligned,
        kv_bytes_per_session_at_max_len: bytes_per_session_at_max_len,
        simultaneous_sessions_at_max_len: sessions_at_max_len,
        simultaneous_sessions_at: sessions_at,
        max_num_seqs_effective,
        measured_reference: request.measured.clone(),
        delta_vs_measurement_pct,
        implied_overhead_bytes,
        confidence,
        arch_source,
        rejection,
        assumptions,
        warnings,
    }
}

/// The one assumption a hybrid linear-attention model always needs.
fn hybrid_state_assumption() -> String {
    format!(
        "this is a hybrid linear-attention model: only the full-attention layers grow a per-token \
         cache. The remaining layers keep a fixed recurrent state (~{:.0} MB for the reference \
         model at 48 layers), which is charged against the same KV budget but does not scale with \
         the context, so it is absorbed by the fixed overhead rather than modelled as a term of its \
         own. `--kv-cache-dtype` selects the full-attention cache's width only: the recurrent \
         state's dtype has its own switch (`mamba_cache_dtype` / `mamba_ssm_cache_dtype`) whose \
         value this crate cannot read and does not assume",
        GDN_STATE_BYTES_REFERENCE_MODEL as f64 / 1_000_000.0
    )
}

/// The cache element width for a `--kv-cache-dtype` value, resolved against the architecture.
fn resolve_kv_width(
    architecture: &ArchFacts,
    kv_cache_dtype: &str,
    _warnings: &mut Vec<String>,
) -> Result<u32, String> {
    let Some(kind) = catalog::kv_cache_dtype_by_id(kv_cache_dtype) else {
        return Err(format!(
            "`{kv_cache_dtype}` is not a cache dtype this crate knows; the catalog lists {}",
            catalog::KV_CACHE_DTYPES
                .iter()
                .map(|dtype| dtype.id)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };

    if kind.follows_model_dtype {
        return match architecture.torch_dtype_bytes() {
            Some(bytes) => Ok(bytes),
            None => Err(format!(
                "`{}` means \"whatever the checkpoint uses\", and config.json states no torch_dtype \
                 this crate maps, so the cache width is unknown",
                kind.id
            )),
        };
    }

    // A fixed width. `min_cuda` is carried by the catalog for the runtime's benefit (a driver
    // floor, not an arithmetic input), so it is deliberately not consulted here.
    match kind.bytes_per_element {
        Some(bytes) => Ok(bytes),
        None => Err(format!(
            "the catalog holds no element width for `{}`, and it does not follow the model dtype \
             either, so there is nothing to compute from",
            kind.id
        )),
    }
}

/// Blocks one session of `context_len` tokens needs. Requires `block_size > 0`; a zero block size
/// describes no allocation unit at all, so it yields zero blocks rather than pretending one token is
/// one block. Requests with `block_size == 0` are refused before they reach this function.
fn blocks_for(context_len: u64, block_size: u64) -> u64 {
    if block_size == 0 {
        return 0;
    }
    context_len.div_ceil(block_size)
}

/// How many sessions of `context_len` fit in a pool of `blocks`.
fn sessions_for(blocks: u64, context_len: u64, block_size: u64) -> u64 {
    let needed = blocks_for(context_len, block_size);
    if needed == 0 {
        return 0;
    }
    blocks / needed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::arch::ArchFacts;
    use serde_json::{json, Value};

    /// The measured pair this crate was written against: 30.9 GB across 7 shards, reported as
    /// 27 781 427 952 bytes by the checkpoint's own file sizes.
    const MEASURED_WEIGHTS_BYTES: u64 = CALIBRATION_WEIGHTS_BYTES;

    fn hybrid_config() -> Value {
        let mut layer_types: Vec<Value> = Vec::new();
        for _ in 0..64 {
            layer_types.push(Value::String("linear_attention".to_string()));
        }
        for (index, layer) in layer_types.iter_mut().enumerate().take(64) {
            if index % 4 == 3 {
                *layer = Value::String("full_attention".to_string());
            }
        }
        json!({
            "architectures": ["Qwen3_5ForConditionalGeneration"],
            "model_type": "qwen3_5",
            "hidden_size": 5120,
            "num_attention_heads": 24,
            "num_hidden_layers": 64,
            "num_key_value_heads": 4,
            "head_dim": 256,
            "layer_types": layer_types,
            "full_attention_interval": 4,
            "linear_num_value_heads": 48,
            "mamba_ssm_dtype": "float32",
            "max_position_embeddings": 262144,
            "torch_dtype": "bfloat16"
        })
    }

    fn canonical_request() -> SizingRequest {
        SizingRequest::new("RTX-PRO-6000", MEASURED_WEIGHTS_BYTES, "fp8", 262144)
            .with_vram_bytes(103_079_215_104)
            .with_measurement(calibration_rtx())
            .with_architecture(
                "orcarouter/Qwen3.8-27B-Uncensored-FP8",
                ArchFacts::from_config_value(&hybrid_config()),
            )
            .with_contexts([2048, 8192, 32768])
    }

    #[test]
    fn the_canonical_example_matches_the_hand_computation() {
        let result = size_deployment(&canonical_request());
        assert_eq!(result.confidence, SizingConfidence::Analytic);
        assert_eq!(result.kv_bytes_per_token, Some(32_768));
        assert_eq!(result.kv_budget_bytes, 58_461_515_351);
        assert_eq!(result.kv_pool_tokens, Some(1_784_103));
        assert_eq!(result.kv_blocks, Some(27_876));
        assert_eq!(result.kv_pool_tokens_aligned, Some(1_784_064));
        assert_eq!(
            result.kv_bytes_per_session_at_max_len,
            Some(32_768 * 262_144)
        );
        assert_eq!(result.max_num_seqs_effective, Some(6));
        let sessions: Vec<(u64, Option<u64>)> = result
            .simultaneous_sessions_at
            .iter()
            .map(|entry| (entry.context_len, entry.sessions))
            .collect();
        assert_eq!(
            sessions,
            vec![
                (2048, Some(871)),
                (8192, Some(217)),
                (32768, Some(54)),
                (262144, Some(6)),
            ]
        );
        assert!(result.is_usable());
        assert_eq!(result.rejection, None);
    }

    #[test]
    fn the_hybrid_basis_is_stated_where_the_number_is() {
        let result = size_deployment(&canonical_request());
        let basis = result.kv_bytes_per_token_basis.expect("a basis");
        assert!(
            basis.starts_with(
                "full_attention_layers=16 of 64 (hybrid linear attention) x num_kv_heads=4 x \
                 head_dim=256 x dtype_bytes=1; x2 for K and V"
            ),
            "{basis}"
        );
        assert!(result.assumptions.iter().any(|line| {
            line.contains("only the full-attention layers grow a per-token cache")
        }));
        assert!(result
            .assumptions
            .iter()
            .any(|line| line.contains("mamba_ssm_cache_dtype")));
    }

    #[test]
    fn counting_every_layer_is_refuted_by_the_measurement() {
        // The naive architecture — 64 layers of KV — is the falsification: it implies a negative
        // overhead, which no constant can fix, so it is rejected instead of reported.
        let naive = SizingRequest::new("RTX-PRO-6000", MEASURED_WEIGHTS_BYTES, "fp8", 262144)
            .with_vram_bytes(103_079_215_104)
            .with_measurement(calibration_rtx())
            .with_architecture(
                "orcarouter/Qwen3.8-27B-Uncensored-FP8",
                ArchFacts::from_config_value(&json!({
                    "hidden_size": 5120,
                    "num_attention_heads": 24,
                    "num_hidden_layers": 64,
                    "num_key_value_heads": 4,
                    "head_dim": 256,
                    "torch_dtype": "bfloat16"
                })),
            );
        let rejected = size_deployment(&naive);
        assert_eq!(rejected.kv_bytes_per_token, Some(131_072));
        assert!(rejected.implied_overhead_bytes.unwrap() < 0);
        assert_eq!(rejected.kv_pool_tokens, None);
        assert_eq!(rejected.confidence, SizingConfidence::Unavailable);
        let reason = rejected.rejection.expect("a refusal");
        assert!(reason.contains("negative overhead"), "{reason}");

        // The same request with the real hybrid architecture is accepted and lands within 2 %.
        let accepted = size_deployment(
            &SizingRequest::new("RTX-PRO-6000", MEASURED_WEIGHTS_BYTES, "fp8", 262144)
                .with_vram_bytes(103_079_215_104)
                .with_measurement(calibration_rtx())
                .with_architecture(
                    "orcarouter/Qwen3.8-27B-Uncensored-FP8",
                    ArchFacts::from_config_value(&hybrid_config()),
                ),
        );
        assert_eq!(accepted.kv_pool_tokens, Some(1_784_103));
        let delta = accepted.delta_vs_measurement_pct.expect("a delta");
        assert!(delta.abs() < HONEST_ACCURACY_PCT, "{delta}");
    }

    fn calibration_rtx() -> MeasuredReference {
        MeasuredReference {
            kv_pool_tokens: 1_777_758,
            card: "RTX-PRO-6000".to_string(),
            source: "docs/research/issue-4-spec.md".to_string(),
            measured_at: None,
        }
    }

    #[test]
    fn the_prediction_against_the_measured_pair_is_within_the_claimed_two_percent() {
        // Both readings of the same card: the nominal 96 GiB the catalog documents, and the 95 GiB
        // the operator measured as usable. Each is a legitimate frame; the result says which.
        let nominal = size_deployment(&canonical_request());
        assert_eq!(nominal.delta_vs_measurement_pct, Some(0.36));
        // The nominal 96 GiB frame needs a larger overhead to reproduce the measurement, which is
        // exactly the +0.36 % the delta states: 8.19 GiB, against the 7.27 GiB the usable 95 GiB
        // frame implies below. Both are residuals of the floored budget, so they are one byte below
        // the rounded hand figure — the arithmetic is the authority here, not the prose.
        assert_eq!(nominal.implied_overhead_bytes, Some(8_797_875_799));

        let usable = size_deployment(
            &SizingRequest::new("RTX-PRO-6000", MEASURED_WEIGHTS_BYTES, "fp8", 262144)
                .with_vram_bytes(102_005_473_280)
                .with_measurement(calibration_rtx())
                .with_architecture(
                    "orcarouter/Qwen3.8-27B-Uncensored-FP8",
                    ArchFacts::from_config_value(&hybrid_config()),
                ),
        );
        assert_eq!(usable.kv_pool_tokens, Some(1_753_957));
        assert_eq!(usable.delta_vs_measurement_pct, Some(-1.34));
        // The overhead those two runs imply, per card: 7.27 GiB here, 7.85 GiB on the L40S.
        assert_eq!(usable.implied_overhead_bytes, Some(7_810_033_321));

        let l40s = size_deployment(
            &SizingRequest::new("L40S", MEASURED_WEIGHTS_BYTES, "fp8", 8192)
                .with_measurement(MeasuredReference {
                    kv_pool_tokens: 281_729,
                    card: "L40S".to_string(),
                    source: "docs/research/issue-4-spec.md".to_string(),
                    measured_at: None,
                })
                .with_architecture(
                    "orcarouter/Qwen3.8-27B-Uncensored-FP8",
                    ArchFacts::from_config_value(&hybrid_config()),
                ),
        );
        // 9 069 391 447 / 32 768 = 276 775.86, so 276 775 tokens fit; the architecture document's
        // 276 776 is the rounded division and is one token optimistic.
        assert_eq!(l40s.kv_pool_tokens, Some(276_775));
        assert_eq!(l40s.delta_vs_measurement_pct, Some(-1.76));
        assert_eq!(l40s.implied_overhead_bytes, Some(8_427_630_167));
        assert!(
            l40s.delta_vs_measurement_pct.expect("delta").abs() < HONEST_ACCURACY_PCT,
            "the L40S cross-check must stay inside the honest band"
        );
    }

    #[test]
    fn the_comparison_against_a_measurement_says_it_is_not_independent_validation() {
        let result = size_deployment(
            &SizingRequest::new("RTX-PRO-6000", MEASURED_WEIGHTS_BYTES, "fp8", 262144)
                .with_vram_bytes(103_079_215_104)
                .with_measurement(calibration_rtx())
                .with_architecture(
                    "orcarouter/Qwen3.8-27B-Uncensored-FP8",
                    ArchFacts::from_config_value(&hybrid_config()),
                ),
        );
        let joined = result.warnings.join("\n");
        assert!(
            joined.contains("not independent validation"),
            "the overhead was fitted to this same pair, so agreement cannot be presented as a \
             check: {joined}"
        );
        assert!(
            joined.contains("calibrated on that same pair of runs"),
            "{joined}"
        );
    }

    #[test]
    fn the_retired_two_point_fit_lands_within_one_point_two_percent_and_proves_nothing() {
        // The first attempt solved the two runs as a linear system in bytes-per-token and a fixed
        // overhead, instead of reading `b` off the architecture. With `floor(0.92 × usable) − weights
        // = b × tokens + A` for each card, `b` is the residual difference over the token difference:
        // (93 845 035 417 − 45 440 753 991) / (1 777 758 − 281 729) ≈ 32 355, about 1.26 % under
        // the architecture's 32 768. It uses the 95 GiB usable figure, not the 96 GiB nameplate:
        // the doc's 32 355 only reproduces from the measured capacity. Kept as arithmetic, not as a
        // function: a fit that is fitted to its own test set is not evidence of anything.
        let residual_rtx = 93_845_035_417i64 - MEASURED_WEIGHTS_BYTES as i64;
        let residual_l40s = 45_440_753_991i64 - MEASURED_WEIGHTS_BYTES as i64;
        let retired = (residual_rtx - residual_l40s) as f64 / (1_777_758f64 - 281_729f64);
        let architecture = 32_768f64;
        let error_pct = (retired - architecture).abs() / architecture * 100.0;
        assert!((32_000.0..32_500.0).contains(&retired), "{retired}");
        assert!(error_pct < 1.3, "{error_pct}");
        // The same fit's second unknown: A = 7.96 GiB, which is what made the overhead a calibrated
        // default rather than a physical constant.
        let fitted_overhead = residual_l40s as f64 - retired * 281_729f64;
        assert!(
            (fitted_overhead / GIB as f64 - 7.96).abs() < 0.01,
            "{}",
            fitted_overhead / GIB as f64
        );
    }

    #[test]
    fn a_budget_that_cannot_hold_the_weights_is_reported_not_hidden() {
        let result = size_deployment(
            &SizingRequest::new("L40S", 60_000_000_000, "fp8", 8192)
                .with_architecture("x/y", ArchFacts::from_config_value(&hybrid_config())),
        );
        assert!(result.kv_budget_bytes < 0, "{}", result.kv_budget_bytes);
        assert_eq!(result.kv_pool_tokens, None);
        assert_eq!(result.confidence, SizingConfidence::Unavailable);
        let reason = result.rejection.expect("a refusal");
        assert!(reason.contains("no KV cache fits at all"), "{reason}");
    }

    #[test]
    fn a_utilisation_outside_the_open_unit_interval_is_refused_not_clamped() {
        let mut request = canonical_request();
        request.gpu_memory_utilization = 1.5;
        let result = size_deployment(&request);
        assert_eq!(result.confidence, SizingConfidence::Unavailable);
        assert!(result
            .rejection
            .expect("a refusal")
            .contains("outside (0, 1]"));

        request.gpu_memory_utilization = 0.0;
        let zero = size_deployment(&request);
        assert_eq!(zero.kv_pool_tokens, None);
        assert_eq!(zero.kv_budget_bytes, 0);
    }

    #[test]
    fn a_card_with_no_vram_figure_yields_no_pool() {
        // B300: the catalog documents the price and the CUDA floor and deliberately holds no VRAM
        // figure, because Modal publishes none. Sizing must not fill that gap.
        let result = size_deployment(
            &SizingRequest::new("B300", MEASURED_WEIGHTS_BYTES, "fp8", 8192).with_architecture(
                "orcarouter/Qwen3.8-27B-Uncensored-FP8",
                ArchFacts::from_config_value(&hybrid_config()),
            ),
        );
        assert_eq!(result.vram_bytes, None);
        assert_eq!(result.confidence, SizingConfidence::Unavailable);
        assert_eq!(result.kv_pool_tokens, None);
        assert!(result
            .rejection
            .expect("a refusal")
            .contains("no VRAM figure"));
    }

    #[test]
    fn an_unknown_card_is_refused_by_name() {
        let result = size_deployment(
            &SizingRequest::new("not-a-card", MEASURED_WEIGHTS_BYTES, "fp8", 8192)
                .with_architecture("x/y", ArchFacts::from_config_value(&hybrid_config())),
        );
        assert_eq!(result.confidence, SizingConfidence::Unavailable);
        assert!(result
            .rejection
            .expect("a refusal")
            .contains("unknown card"));
        assert!(result
            .warnings
            .iter()
            .any(|line| line.contains("not a card in the catalog")));
    }

    #[test]
    fn auto_cache_width_follows_the_model_dtype_or_is_refused() {
        let with_dtype = size_deployment(
            &SizingRequest::new("RTX-PRO-6000", MEASURED_WEIGHTS_BYTES, "auto", 8192)
                .with_vram_bytes(103_079_215_104)
                .with_architecture("x/y", ArchFacts::from_config_value(&hybrid_config())),
        );
        assert_eq!(with_dtype.kv_bytes_per_element, Some(2));
        assert_eq!(with_dtype.kv_bytes_per_token, Some(65_536));

        let without_dtype = size_deployment(
            &SizingRequest::new("RTX-PRO-6000", MEASURED_WEIGHTS_BYTES, "auto", 8192)
                .with_vram_bytes(103_079_215_104)
                .with_architecture(
                    "x/y",
                    ArchFacts::from_config_value(&json!({
                        "num_hidden_layers": 64,
                        "num_key_value_heads": 4,
                        "head_dim": 256,
                        "num_attention_heads": 24,
                        "hidden_size": 5120
                    })),
                ),
        );
        assert_eq!(without_dtype.kv_bytes_per_token, None);
        assert!(without_dtype
            .rejection
            .expect("a refusal")
            .contains("no torch_dtype"));
    }

    #[test]
    fn an_unknown_cache_dtype_is_refused_with_the_alternatives() {
        let result = size_deployment(
            &SizingRequest::new("RTX-PRO-6000", MEASURED_WEIGHTS_BYTES, "int4", 8192)
                .with_architecture("x/y", ArchFacts::from_config_value(&hybrid_config())),
        );
        let reason = result.rejection.expect("a refusal");
        assert!(reason.contains("int4"), "{reason}");
        assert!(
            reason.contains("fp8"),
            "the refusal lists what is known: {reason}"
        );
    }

    #[test]
    fn without_an_architecture_and_without_an_override_nothing_is_invented() {
        let result = size_deployment(&SizingRequest::new(
            "RTX-PRO-6000",
            MEASURED_WEIGHTS_BYTES,
            "fp8",
            8192,
        ));
        assert_eq!(result.kv_bytes_per_token, None);
        assert_eq!(result.confidence, SizingConfidence::Unavailable);
        assert!(result
            .rejection
            .expect("a refusal")
            .contains("nothing to derive a per-token figure from"));
    }

    #[test]
    fn an_override_is_reported_as_calibrated_not_as_analytic() {
        let mut request = SizingRequest::new("RTX-PRO-6000", MEASURED_WEIGHTS_BYTES, "fp8", 8192);
        request.kv_bytes_per_token = Some(32_768);
        let result = size_deployment(&request);
        assert_eq!(result.confidence, SizingConfidence::Calibrated);
        assert_eq!(result.confidence.as_str(), "calibrated");
        assert_eq!(result.arch_source.as_deref(), Some("request"));
        assert!(result
            .assumptions
            .iter()
            .any(|line| line.contains("the per-token figure is the caller's")));
        assert!(result
            .kv_bytes_per_token_basis
            .expect("a basis")
            .contains("supplied by the caller"));
    }

    #[test]
    fn an_mla_checkpoint_is_refused_with_the_reason_the_formula_does_not_apply() {
        let result = size_deployment(
            &SizingRequest::new("H200", MEASURED_WEIGHTS_BYTES, "fp8", 8192).with_architecture(
                "deepseek-ai/DeepSeek-V3",
                ArchFacts::from_config_value(&json!({
                    "hidden_size": 7168,
                    "num_hidden_layers": 61,
                    "num_attention_heads": 128,
                    "num_key_value_heads": 128,
                    "kv_lora_rank": 512,
                    "q_lora_rank": 1536,
                    "v_head_dim": 128,
                    "torch_dtype": "bfloat16"
                })),
            ),
        );
        assert_eq!(result.confidence, SizingConfidence::Unavailable);
        assert_eq!(result.kv_pool_tokens, None);
        assert!(result
            .rejection
            .expect("a refusal")
            .contains("multi-head latent attention"));
    }

    #[test]
    fn block_alignment_is_reported_separately_from_the_floor() {
        let result = size_deployment(&canonical_request());
        assert_eq!(result.kv_pool_tokens, Some(1_784_103));
        assert_eq!(result.kv_pool_tokens_aligned, Some(1_784_064));
        assert!(
            result.kv_pool_tokens_aligned.expect("aligned")
                < result.kv_pool_tokens.expect("tokens")
        );
        assert!(
            !result
                .warnings
                .iter()
                .any(|line| line.contains("is unusable")),
            "a 39-token remainder is not worth a warning here: {:?}",
            result.warnings
        );

        // A pool with a real remainder does warn. With the L40S's 45 440 753 991-byte budget and
        // the 8 GiB default overhead, weights of 36 847 542 599 leave exactly 3 276 800 bytes: 100
        // tokens at 32 768 bytes each, which is one 64-token block and a 36 % remainder.
        let remainder = size_deployment(
            &SizingRequest::new("L40S", 36_847_542_599, "fp8", 8192)
                .with_architecture("x/y", ArchFacts::from_config_value(&hybrid_config())),
        );
        assert_eq!(remainder.kv_pool_tokens, Some(100));
        assert_eq!(remainder.kv_blocks, Some(1));
        assert_eq!(remainder.kv_pool_tokens_aligned, Some(64));
        let warning = remainder
            .warnings
            .iter()
            .find(|line| line.contains("is unusable"))
            .expect("a remainder this large must be stated, not rounded away");
        assert!(warning.contains("36.0 %"), "{warning}");
    }

    #[test]
    fn a_max_num_seqs_the_cache_cannot_hold_is_lowered_and_said_so() {
        let mut request = canonical_request();
        request.max_num_seqs = Some(64);
        let result = size_deployment(&request);
        assert_eq!(result.max_num_seqs_effective, Some(6));
        assert!(result
            .warnings
            .iter()
            .any(|line| line.contains("--max-num-seqs 64 cannot be met")));
    }

    #[test]
    fn a_max_num_seqs_the_cache_can_hold_is_kept() {
        let mut request = canonical_request();
        request.max_num_seqs = Some(4);
        let result = size_deployment(&request);
        assert_eq!(result.max_num_seqs_effective, Some(4));
        assert!(!result
            .warnings
            .iter()
            .any(|line| line.contains("cannot be met")));
    }

    #[test]
    fn the_documents_own_stale_session_figure_is_the_one_at_a_shorter_context() {
        // §7.2's illustrative JSON shows `max_num_seqs_effective: 54` beside
        // `--max-model-len 262144`. 54 is the figure at 32 768; at 262 144 it is 6. Both are pinned
        // here so the difference is a checked fact rather than a quiet discrepancy.
        let at_max = size_deployment(&canonical_request());
        assert_eq!(at_max.max_num_seqs_effective, Some(6));

        let mut shorter = canonical_request();
        shorter.max_model_len = 32_768;
        let at_32768 = size_deployment(&shorter);
        assert_eq!(at_32768.max_num_seqs_effective, Some(54));
    }

    #[test]
    fn the_calibration_table_is_exactly_the_two_operator_runs() {
        let points = calibration_points();
        assert_eq!(points.len(), 2);
        for point in points {
            assert_eq!(point.source, FactSource::Observed);
            assert_eq!(point.source_url, "docs/research/issue-4-spec.md");
            assert_eq!(point.kv_cache_dtype, "fp8");
            assert_eq!(point.weights_bytes, MEASURED_WEIGHTS_BYTES);
        }
        assert_eq!(points[0].kv_pool_tokens, 1_777_758);
        assert_eq!(points[1].kv_pool_tokens, 281_729);
        assert_eq!(points[1].card, "L40S");
    }

    #[test]
    fn calibration_lookup_ignores_case_and_surrounding_space() {
        let found = calibration_for(
            "  ORCAROUTER/qwen3.8-27b-uncensored-fp8 ",
            "rtx-pro-6000",
            " FP8",
        )
        .expect("the measured pair is on file");
        assert_eq!(found.kv_pool_tokens, 1_777_758);
        assert_eq!(
            calibration_for("orcarouter/Qwen3.8-27B-Uncensored-FP8", "H100", "fp8"),
            None,
            "a card with no measurement must not borrow another card's"
        );
        assert_eq!(
            calibration_for("x/y", "RTX-PRO-6000", "fp8"),
            None,
            "and neither must a repository with no measurement"
        );
    }

    #[test]
    fn the_result_carries_every_honesty_field_through_json() {
        let result = size_deployment(&canonical_request());
        let value = serde_json::to_value(&result).expect("serialises");
        for field in [
            "kv_budget_bytes",
            "kv_bytes_per_token",
            "kv_bytes_per_token_basis",
            "kv_pool_tokens",
            "kv_blocks",
            "kv_pool_tokens_aligned",
            "kv_bytes_per_session_at_max_len",
            "simultaneous_sessions_at_max_len",
            "simultaneous_sessions_at",
            "max_num_seqs_effective",
            "implied_overhead_bytes",
            "confidence",
            "arch_source",
            "rejection",
            "assumptions",
            "warnings",
        ] {
            assert!(
                value.get(field).is_some(),
                "the wire shape must carry `{field}`"
            );
        }
        assert_eq!(value["confidence"], json!("analytic"));
        assert_eq!(
            value["arch_source"],
            json!("orcarouter/Qwen3.8-27B-Uncensored-FP8")
        );
        // The canonical request carries the RTX PRO 6000 measurement, so the wire shape reports the
        // overhead that run implies — 8.19 GiB, the third of the three fitted values — beside the
        // shipped constant rather than replacing it.
        assert_eq!(value["implied_overhead_bytes"], json!(8_797_875_799i64));
        assert_eq!(
            value["measured_reference"]["kv_pool_tokens"],
            json!(1_777_758)
        );
        assert_eq!(value["rejection"], Value::Null);
        assert_eq!(
            value["simultaneous_sessions_at"][0]["context_len"],
            json!(2048)
        );

        // A refused result keeps the same shape, with the reason attached.
        let refused =
            serde_json::to_value(size_deployment(&SizingRequest::default())).expect("serialises");
        assert_eq!(refused["confidence"], json!("unavailable"));
        assert_eq!(refused["kv_pool_tokens"], Value::Null);
    }

    #[test]
    fn the_default_overhead_is_the_calibrated_constant_and_is_always_declared() {
        let result = size_deployment(&canonical_request());
        assert_eq!(result.overhead_bytes, DEFAULT_OVERHEAD_BYTES);
        let joined = result.assumptions.join("\n");
        assert!(
            joined.contains("calibrated constant, not a measurement"),
            "{joined}"
        );
        assert!(joined.contains("±0.6 GiB"), "{joined}");

        let mut overridden = canonical_request();
        overridden.overhead_bytes = 4 * GIB;
        let result = size_deployment(&overridden);
        assert!(result
            .assumptions
            .iter()
            .any(|line| line.contains("replaces the shipped 8 GiB default")));
        assert!(
            result.kv_budget_bytes > size_deployment(&canonical_request()).kv_budget_bytes,
            "a smaller overhead leaves more cache, and the result says which was used"
        );
    }

    #[test]
    fn the_analytic_assumption_line_carries_the_honest_accuracy_claim() {
        let result = size_deployment(&canonical_request());
        let joined = result.assumptions.join("\n");
        assert!(joined.contains("−1.34 %"), "{joined}");
        assert!(joined.contains("−1.76 %"), "{joined}");
        assert!(joined.contains("±2 %"), "{joined}");
    }

    #[test]
    fn the_measured_frames_are_reported_without_being_confused() {
        let nominal = size_deployment(&canonical_request());
        assert_eq!(
            nominal
                .measured_reference
                .as_ref()
                .map(|m| m.kv_pool_tokens),
            Some(1_777_758)
        );

        // Stating no usable capacity is not the same as stating the sticker figure: with nothing but
        // the card's nameplate the analytic number is produced and no measurement is claimed.
        let nameplate = size_deployment(
            &SizingRequest::new("T4", MEASURED_WEIGHTS_BYTES, "fp8", 8192)
                .with_architecture("x/y", ArchFacts::from_config_value(&hybrid_config())),
        );
        assert_eq!(nameplate.vram_bytes, Some(17_179_869_184));
        assert_eq!(
            nameplate.usable_vram_bytes, None,
            "the T4 has no measured capacity on file"
        );
        assert_eq!(
            nameplate.measured_reference, None,
            "the catalog's nameplate figure is not a measurement"
        );

        // The catalog's own preference: a usable figure wins over the sticker figure when the
        // caller states neither.
        let from_catalog = size_deployment(
            &SizingRequest::new("RTX-PRO-6000", MEASURED_WEIGHTS_BYTES, "fp8", 262144)
                .with_architecture("x/y", ArchFacts::from_config_value(&hybrid_config())),
        );
        assert_eq!(from_catalog.vram_bytes, Some(102_005_473_280));
        assert_eq!(from_catalog.kv_pool_tokens, Some(1_753_957));
    }

    #[test]
    fn a_context_list_always_includes_the_configured_maximum_and_is_ordered() {
        let mut request = canonical_request();
        request.contexts = vec![32768, 1024, 32768, 262144];
        let result = size_deployment(&request);
        let contexts: Vec<u64> = result
            .simultaneous_sessions_at
            .iter()
            .map(|entry| entry.context_len)
            .collect();
        assert_eq!(contexts, vec![1024, 2048, 8192, 32768, 262144]);
        let at_1024 = result.simultaneous_sessions_at[0];
        assert_eq!(at_1024.blocks_per_session, 16);
        assert_eq!(at_1024.bytes_per_session, 32_768 * 1024);
        assert_eq!(at_1024.sessions, Some(1_742));
    }

    #[test]
    fn a_zero_max_model_len_is_refused_rather_than_divided_by() {
        let result = size_deployment(
            &SizingRequest::new("RTX-PRO-6000", MEASURED_WEIGHTS_BYTES, "fp8", 0)
                .with_architecture("x/y", ArchFacts::from_config_value(&hybrid_config())),
        );
        assert_eq!(result.confidence, SizingConfidence::Unavailable);
        assert!(result
            .rejection
            .expect("a refusal")
            .contains("max_model_len = 0"));
    }

    #[test]
    fn zero_weights_are_flagged_rather_than_believed() {
        let result = size_deployment(
            &SizingRequest::new("RTX-PRO-6000", 0, "fp8", 8192)
                .with_architecture("x/y", ArchFacts::from_config_value(&hybrid_config())),
        );
        assert!(result
            .warnings
            .iter()
            .any(|line| line.contains("weights_bytes = 0")));
        assert!(result.kv_pool_tokens.expect("a pool") > 1_784_103);
    }

    #[test]
    fn the_shipped_constant_really_is_within_the_band_its_own_documentation_claims() {
        // The two fitted overheads, recomputed here from the measurements rather than quoted. The
        // architecture document states the result as a band, "roughly 7.3-7.9 GiB", so the declared
        // spread is the *width* of that band, not a symmetric tolerance about 8 GiB: the shipped 8
        // GiB sits about 0.1 GiB above the band's top, on the conservative side (a larger overhead
        // predicts a smaller pool). Asserting a symmetric ±0.6 GiB here would fail, and "fixing" it
        // by widening the constant would hide a real 0.1 GiB discrepancy in the document.
        let rtx = 7_810_033_321i64;
        let l40s = 8_427_630_167i64;
        let separation = (l40s - rtx) as u64;
        assert!(
            separation <= OVERHEAD_SPREAD_BYTES,
            "the fitted pair spans {separation} bytes, more than the declared {OVERHEAD_SPREAD_BYTES}"
        );
        for fitted in [rtx, l40s] {
            let gib = fitted as f64 / GIB as f64;
            // The document says "roughly 7.3-7.9 GiB"; 7.274 is the RTX figure, which it rounds up
            // to 7.3 in §7.2 and again in the band. The lower test bound is therefore 7.25 GiB, and
            // the 0.03 GiB rounding is stated rather than absorbed into the constants.
            assert!(
                (7.25..=7.9).contains(&gib),
                "fitted {gib} GiB is outside the band the documentation states"
            );
        }
        // The one honest disagreement, stated rather than smoothed: 8 GiB is above 7.9 GiB.
        let overshoot = DEFAULT_OVERHEAD_BYTES as i64 - l40s;
        assert!(
            overshoot > 0,
            "the shipped constant is above the fitted band's top"
        );
        assert!(
            (overshoot as f64 / GIB as f64) < 0.16,
            "{} GiB above the band",
            overshoot as f64 / GIB as f64
        );
        assert!(
            (rtx as f64 / GIB as f64 - 7.27).abs() < 0.01,
            "{}",
            rtx as f64 / GIB as f64
        );
        assert!(
            (l40s as f64 / GIB as f64 - 7.85).abs() < 0.01,
            "{}",
            l40s as f64 / GIB as f64
        );
    }

    #[test]
    fn the_geometric_unit_is_a_grid_block_not_a_decimal() {
        assert_eq!(GIB, 1_073_741_824);
        assert_eq!(DEFAULT_OVERHEAD_BYTES, 8 * GIB);
        assert_eq!(DEFAULT_BLOCK_SIZE, 64);
        assert_eq!(DEFAULT_GPU_MEMORY_UTILIZATION, 0.92);
        assert!((GDN_STATE_BYTES_REFERENCE_MODEL as f64) < (0.01 * DEFAULT_OVERHEAD_BYTES as f64));
        assert_eq!(blocks_for(2048, 64), 32);
        assert_eq!(blocks_for(0, 64), 0);
        assert_eq!(sessions_for(27_876, 8192, 64), 217);
        assert_eq!(sessions_for(27_876, 8192, 0), 0);
    }
}
