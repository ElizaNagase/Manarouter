//! The offline re-quantisation job, described rather than run.
//!
//! The question this module answers is: *"this checkpoint is FP8 (or BF16) and I want 4-bit — what
//! exactly has to happen, on what card, for how long, at what cost, and what does the result
//! actually claim?"* Manarouter records the recipe and the commands; it does not run a multi-hour
//! quantisation job, and nothing here pretends otherwise.
//!
//! Two facts decide almost every field of [`RequantizePlan`], and both are easy to get wrong:
//!
//! **One — the produced artefact lies about its name.** llm-compressor's AWQ output does not carry
//! `quant_method: "awq"`: it writes `quant_method: "compressed-tensors"` with
//! `format: "pack-quantized"`. vLLM reads the method from the model config and, if a `--quantization`
//! flag disagrees with it, raises `ValueError` and refuses to start
//! (`vllm/config/model.py::_verify_quantization`). So nothing in this module may derive a
//! `--quantization` value from the *requested* target id — see [`default_target`] and
//! [`RequantizePlan::serve_command`]. Requesting `awq` and getting `compressed-tensors` is normal;
//! passing `--quantization awq` at serve time is a crash.
//!
//! **Two — a re-quantisation is not an upgrade.** FP8 → 4-bit has to go through a dense round trip:
//! the weights are dequantised to BF16, then quantised again. That costs the FP8 accuracy *and* adds
//! AWQ's own loss, which is why a local AWQ of an FP8 checkpoint is worse than the publisher's own
//! AWQ of the BF16 original, and why the recommended source is the ungated BF16 repository rather
//! than the FP8 one.
//!
//! The feasibility gate is deliberately crude and deliberately loud about it: VRAM first, then disk,
//! then host RAM, each against a stated constant, with [`RequantizePlan::vram_gate_evaluated`]
//! distinguishing "the gate said yes" from "the gate could not be evaluated because the card has no
//! published VRAM figure".

use serde::{Deserialize, Serialize};

use super::arch::GatedState;
use super::catalog;
use super::sizing::GIB;
use crate::rental::FactSource;

/// Where the recipe itself is documented. Every constant below is traceable to it.
pub const RECIPES_URL: &str = "docs/research/requantize-recipes.md";

/// The tool that performs the job. Named, because "quantise it" is not a recipe.
pub const TOOL: &str = "llm-compressor";
/// The version the recipe's pins were read from.
pub const TOOL_VERSION: &str = "0.15.0";

/// What vLLM reads back out of a `compressed-tensors` checkpoint, whatever was requested.
pub const PRODUCED_QUANT_METHOD: &str = "compressed-tensors";
/// The `format` string `compressed-tensors` writes for W4A16 weights.
pub const PRODUCED_FORMAT_W4A16: &str = "pack-quantized";

/// Bytes of VRAM the dense recipe needs at its peak, per the documentation.
///
/// Recomputed from the recipe's own arithmetic ([`recomputed_dense_vram_bytes`]) the figure comes out
/// 0.47 GiB higher; the documented value is what is shipped here, because it is the published number,
/// and the difference is recorded in [`RequantizePlan::notes`] rather than silently reconciled.
pub const DENSE_REQUIRES_VRAM_BYTES: u64 = 12_884_901_888;

/// Bytes of VRAM the model-free (RTN) recipe needs: no dense round trip, no calibration set.
pub const MODEL_FREE_REQUIRES_VRAM_BYTES: u64 = 8_589_934_592;

/// Extra VRAM the job needs *beyond the single rank that holds the layer being converted*: zero.
///
/// Not a guess: LLM Compressor does not currently support tensor parallelism for compression, so the
/// conversion runs per-rank over the model's own shards and needs no extra device.
pub const REQUIRES_EXTRA_VRAM_BYTES: u64 = 0;

/// Fraction of the card's VRAM the gate allows the job to occupy. The rest is the runtime's.
pub const VRAM_GATE_HEADROOM: f64 = 0.9;

/// Host RAM the dense job needs, from the recipe's own figure.
pub const DEFAULT_HOST_RAM_BYTES: u64 = 69_000_000_000;
/// Host RAM multiplier over the weight size (`k_ram`), as stated in the recipe.
pub const DEFAULT_K_RAM: f64 = 1.25;
/// Disk the dense job needs: the BF16 copy, the output, and scratch. Stated, not derived silently.
pub const DEFAULT_DISK_BYTES: u64 = 100_000_000_000;
/// Disk for the model-free path, which neither materialises a BF16 copy nor calibrates.
pub const MODEL_FREE_DISK_BYTES: u64 = 45_000_000_000;

/// The parallelism the recipe is written for. Data-parallel: each rank converts its own shards.
pub const DEFAULT_K_GPU: u32 = 4;

/// Wall-clock share of the ideal for a data-parallel run at each rank count, **measured**: the
/// recipe's own scaling test reports 2.0 at two ranks and 3.8 at four, i.e. 5 % off perfect at four.
pub const DDP_SPEEDUPS: [(u32, f64); 3] = [(1, 1.0), (2, 2.0), (4, 3.8)];

/// One step of the job, as a shell command or a plain instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeStep {
    /// What to run or do.
    pub command: String,
    /// What it is for, and what it is not.
    pub why: String,
}

/// A quantisation format as *requested*.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuantSource {
    /// Repository id.
    pub id: String,
    /// Human label.
    pub label: String,
    /// Bytes per parameter, from the checkpoint's file sizes. `None` when unknown.
    #[serde(default)]
    pub bytes_per_param: Option<f64>,
    /// Whether the hub gates it.
    pub gated: GatedState,
    /// SPDX-ish licence id as the hub reports it, or a note when it is not stated.
    #[serde(default)]
    pub license: Option<String>,
}

/// The format the job is asked to produce, and what it will really carry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuantTarget {
    /// What the operator asked for (`"awq"`, `"w4a16-rtn"`). **Not** a `--quantization` value.
    pub id: String,
    /// Human label.
    pub label: String,
    /// Bytes per parameter after conversion, used for the size estimate.
    #[serde(default)]
    pub bytes_per_param: Option<f64>,
    /// The `quant_method` vLLM will read out of the produced `config.json`.
    pub produced_quant_method: String,
    /// The `format` field that goes with it.
    pub produced_format: String,
}

/// The source the recipe recommends: the ungated BF16 original.
///
/// It ships as bfloat16 with no quantisation applied, so the conversion has one lossy step (the
/// target's) rather than two, and it is public, so no token is needed and the derivative's licence
/// terms are the model's own.
pub fn default_source() -> QuantSource {
    QuantSource {
        id: "Qwen/Qwen3.8-27B".to_string(),
        label: "BF16".to_string(),
        bytes_per_param: Some(2.0),
        gated: GatedState::Open,
        license: Some("Apache-2.0".to_string()),
    }
}

/// The FP8 checkpoint the operator actually measured, offered as the fallback source.
///
/// It is gated, so a token with access is required before the job can read the weights, and its
/// weights are already lossy: converting them to 4-bit stacks a second loss on top of the first.
pub fn fp8_source() -> QuantSource {
    QuantSource {
        id: "orcarouter/Qwen3.8-27B-Uncensored-FP8".to_string(),
        label: "FP8 (block)".to_string(),
        bytes_per_param: Some(1.0),
        gated: GatedState::Manual,
        license: None,
    }
}

/// AWQ 4-bit, the recipe's default target — and the one whose name the artefact will not carry.
pub fn default_target() -> QuantTarget {
    QuantTarget {
        id: "awq".to_string(),
        label: "AWQ 4-bit".to_string(),
        bytes_per_param: Some(0.5),
        produced_quant_method: PRODUCED_QUANT_METHOD.to_string(),
        produced_format: PRODUCED_FORMAT_W4A16.to_string(),
    }
}

/// 4-bit round-to-nearest W4A16: the data-free one-pass alternative, with no calibration set.
pub fn rtn_target() -> QuantTarget {
    QuantTarget {
        id: "w4a16-rtn".to_string(),
        label: "4-bit RTN (W4A16)".to_string(),
        bytes_per_param: Some(0.5),
        produced_quant_method: PRODUCED_QUANT_METHOD.to_string(),
        produced_format: PRODUCED_FORMAT_W4A16.to_string(),
    }
}

/// How the weights get from the source precision to the target precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequantizePath {
    /// Dequantise to BF16 in memory, then quantise again with AWQ/GPTQ over a calibration set.
    /// Lossy twice, and the reference recipe.
    Dense,
    /// Quantise shard by shard straight from the source tensors, with no dense materialisation and
    /// no calibration data. One lossy step, much faster, weaker result.
    ModelFree,
}

impl RequantizePath {
    /// The wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dense => "dense",
            Self::ModelFree => "model-free",
        }
    }

    /// The VRAM the path needs at its peak.
    pub const fn requires_vram_bytes(self) -> u64 {
        match self {
            Self::Dense => DENSE_REQUIRES_VRAM_BYTES,
            Self::ModelFree => MODEL_FREE_REQUIRES_VRAM_BYTES,
        }
    }
}

/// The card the gate stopped on, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailedOn {
    /// The card, as a catalog id (or whatever the caller named, when it is not one).
    pub card: String,
    /// Which gate failed: `"vram"`, `"disk"` or `"host_ram"`.
    pub gate: String,
    /// The requirement that was not met.
    pub requires_bytes: u64,
    /// What was actually available.
    pub available_bytes: Option<u64>,
    /// The sentence a reader needs.
    pub reason: String,
}

/// One measured-fan-out row: what k ranks cost in wall clock, GPU-hours and dollars.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GpuHoursRow {
    /// Ranks used.
    pub k_gpus: u32,
    /// Measured speedup against one rank.
    pub speedup: f64,
    /// Hours of wall clock the job takes at this rank count.
    pub wall_clock_hours: f64,
    /// GPU-hours consumed: wall clock × ranks. This is what is billed.
    pub gpu_hours: f64,
    /// Dollars at the card's documented hourly price.
    pub cost_usd: f64,
    /// Whether the speedup is a measurement or an ideal.
    pub speedup_source: FactSource,
}

/// Everything the plan needs, with the canonical recipe as the default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecipeRequest {
    /// Source repository and its provenance.
    pub source: QuantSource,
    /// Requested target and what it really produces.
    pub target: QuantTarget,
    /// Which conversion path.
    pub path: RequantizePath,
    /// The card to run on. Defaults to the recipe's reference card.
    #[serde(default)]
    pub card: Option<String>,
    /// Disk to provision for the job. Defaults to the path's documented figure.
    #[serde(default)]
    pub provisioned_disk_bytes: Option<u64>,
    /// Host RAM **available** on the machine, for the RAM gate. The requirement is computed from
    /// the checkpoint, never taken from here. `None` means the operator did not say, and an
    /// unspecified host is not treated as a failing one: the gate then compares the requirement
    /// against itself and passes, which is a statement about the recipe rather than about the host.
    #[serde(default)]
    pub host_ram_bytes: Option<u64>,
    /// Whether the recipe offloads CPU-side tensors to disk (`device_map="auto_offload"`).
    pub offload: bool,
    /// Ranks the job runs on.
    pub k_gpu: u32,
    /// Host RAM multiplier over the weight size.
    pub k_ram: f64,
    /// Parameters in the checkpoint, for the size and RAM estimates.
    pub params: u64,
    /// Layers, for the per-layer working set.
    pub layers: u64,
    /// Wall-clock minutes at one rank before the measured speedup is applied.
    pub base_minutes: u64,
}

impl Default for RecipeRequest {
    fn default() -> Self {
        Self {
            source: default_source(),
            target: default_target(),
            path: RequantizePath::Dense,
            card: None,
            provisioned_disk_bytes: None,
            host_ram_bytes: None,
            offload: true,
            k_gpu: DEFAULT_K_GPU,
            k_ram: DEFAULT_K_RAM,
            // The §9 worked example: 27.8 B parameters in 64 layers, at 480 minutes per rank.
            params: 27_781_427_952,
            layers: 64,
            base_minutes: 480,
        }
    }
}

impl RecipeRequest {
    /// The model-free variant of the default request: no calibration set, one pass, disk-light.
    pub fn model_free() -> Self {
        Self {
            target: rtn_target(),
            path: RequantizePath::ModelFree,
            provisioned_disk_bytes: Some(MODEL_FREE_DISK_BYTES),
            base_minutes: 35,
            ..Self::default()
        }
    }

    /// The FP8 checkpoint as the source, when the BF16 original is not the one being converted.
    pub fn from_fp8(mut self) -> Self {
        self.source = fp8_source();
        self
    }
}

/// The plan: what to run, where, how long, for how much, and what it will not promise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequantizePlan {
    /// The requested-and-produced target pair, the trap included.
    pub target: QuantTarget,
    /// The source checkpoint.
    pub source: QuantSource,
    /// Which conversion path.
    pub path: RequantizePath,
    /// Why the source repository cannot simply be served at the target precision.
    pub unavailable_reason: String,
    /// The card the plan is written for.
    pub card: Option<String>,
    /// VRAM the job needs at its peak.
    pub requires_vram_bytes: u64,
    /// VRAM needed beyond the rank doing the conversion: zero, because compression is not
    /// tensor-parallel.
    pub requires_extra_vram_bytes: u64,
    /// Host RAM the job needs.
    pub requires_host_ram_bytes: u64,
    /// Disk the job needs.
    pub requires_disk_bytes: u64,
    /// Minutes of wall clock at one rank.
    pub estimated_minutes: u64,
    /// The range the estimate sits in.
    pub estimated_minutes_range: [u64; 2],
    /// Dollars, as a low–high range across the cards that clear the gate.
    pub estimated_cost_usd: [f64; 2],
    /// The tool and version that does the work.
    pub tool: String,
    /// The version.
    pub tool_version: String,
    /// The steps, in order. No `llmcompressor` CLI exists; the recipe writes a Python file.
    pub steps: Vec<RecipeStep>,
    /// The serve command for the *output* directory — with no `--quantization` flag, deliberately.
    pub serve_command: String,
    /// Whether the schedule is GPU work the operator must actually run.
    pub runs_remotely: bool,
    /// Whether the VRAM gate could be evaluated at all. `false` means "no VRAM figure for this card",
    /// which is not the same as "it fits".
    pub vram_gate_evaluated: bool,
    /// Whether the job fits on the chosen card. `None` when the gate could not be evaluated.
    pub feasible_on_gpu: Option<bool>,
    /// The card VRAM the gate compared against.
    pub card_vram_bytes: Option<u64>,
    /// The gate that stopped it, when one did.
    pub failed_on: Option<FailedOn>,
    /// Other catalogue cards that clear the gate, cheapest first.
    pub alternative_cards: Vec<String>,
    /// Fan-out rows at one, two and four ranks.
    pub gpu_hours_at_k: Vec<GpuHoursRow>,
    /// The assumption behind the fan-out.
    pub k_gpu: u32,
    /// The host RAM multiplier, stated as an assumption.
    pub k_ram: f64,
    /// Whether the result may be published, as far as this crate can tell. Always false: it can
    /// check the licence field and nothing else.
    pub publishable: bool,
    /// Caveats a reader must not have to infer.
    pub notes: Vec<String>,
}

/// Plan the job. Pure arithmetic and string assembly: nothing here touches a GPU, a hub or a clock.
pub fn plan_requantize(request: &RecipeRequest) -> RequantizePlan {
    let mut notes: Vec<String> = Vec::new();

    // The names do not match, and that mismatch is the single most expensive mistake available.
    if request.target.produced_quant_method != request.target.id {
        notes.push(format!(
            "the target is requested as `{}` but llm-compressor writes \
             quant_method=\"{}\" with format=\"{}\" — vLLM reads that from config.json and raises \
             ValueError if a `--quantization` flag disagrees with it, so the flag is never emitted",
            request.target.id, request.target.produced_quant_method, request.target.produced_format
        ));
    }

    let requires_vram_bytes = request.path.requires_vram_bytes();
    let requires_disk_bytes = request
        .provisioned_disk_bytes
        .unwrap_or(match request.path {
            RequantizePath::Dense => DEFAULT_DISK_BYTES,
            RequantizePath::ModelFree => MODEL_FREE_DISK_BYTES,
        });
    let requires_host_ram_bytes = host_ram_requirement(request.params, request.k_ram);
    // No stated host is not a failing host.
    let host_ram_available = request.host_ram_bytes.unwrap_or(requires_host_ram_bytes);

    if request.offload {
        notes.push(
            "the recipe offloads CPU-side tensors to disk (`device_map=\"auto_offload\"`), so host \
             RAM is a working-set floor rather than a hard ceiling — but it does mean the offload \
             directory needs real disk, which is what requires_disk_bytes covers"
                .to_string(),
        );
    }

    if let Some(recomputed) = recomputed_dense_vram_bytes(request.params, request.layers) {
        if request.path == RequantizePath::Dense && recomputed != DENSE_REQUIRES_VRAM_BYTES {
            notes.push(format!(
                "recomputing the dense peak from the recipe's own arithmetic gives \
                 {recomputed} bytes ({:.2} GiB) against the documented {DENSE_REQUIRES_VRAM_BYTES} \
                 ({:.2} GiB); the documented figure is shipped, so the gate carries up to {:.2} GiB \
                 of optimism. Both catalogue cards clear either figure by a wide margin",
                recomputed as f64 / GIB as f64,
                DENSE_REQUIRES_VRAM_BYTES as f64 / GIB as f64,
                (recomputed.saturating_sub(DENSE_REQUIRES_VRAM_BYTES)) as f64 / GIB as f64
            ));
        }
    }

    // Choose the card: the caller's, else the recipe's reference card while it clears the gate.
    let card_id = request
        .card
        .clone()
        .unwrap_or_else(|| REFERENCE_CARD.to_string());
    let card = catalog::gpu_by_id(&card_id);
    let card_vram_bytes = card.and_then(|card| card.vram_for_sizing());

    let mut failed_on: Option<FailedOn> = None;
    let mut vram_gate_evaluated = false;
    let mut feasible_on_gpu: Option<bool> = None;

    // Gate order is fixed and stated: VRAM, then disk, then host RAM.
    match card_vram_bytes {
        None => {
            notes.push(format!(
                "the VRAM gate could not be evaluated for `{card_id}`: the catalogue holds no VRAM \
                 figure for it, and `feasible_on_gpu` is therefore null rather than true"
            ));
        }
        Some(vram) => {
            vram_gate_evaluated = true;
            let ceiling = (vram as f64 * VRAM_GATE_HEADROOM).floor();
            if (requires_vram_bytes as f64) > ceiling {
                feasible_on_gpu = Some(false);
                failed_on = Some(FailedOn {
                    card: card_id.clone(),
                    gate: "vram".to_string(),
                    requires_bytes: requires_vram_bytes,
                    available_bytes: Some(ceiling as u64),
                    reason: format!(
                        "the {} recipe needs {} bytes of VRAM and `{card_id}` offers {} bytes at \
                         {} % utilisation",
                        request.path.as_str(),
                        requires_vram_bytes,
                        ceiling as u64,
                        VRAM_GATE_HEADROOM * 100.0
                    ),
                });
            }
        }
    }

    if feasible_on_gpu.is_none() && vram_gate_evaluated {
        if requires_disk_bytes == 0 {
            feasible_on_gpu = Some(false);
            failed_on = Some(FailedOn {
                card: card_id.clone(),
                gate: "disk".to_string(),
                requires_bytes: requires_disk_bytes,
                available_bytes: None,
                reason: "the job needs scratch space and none was provisioned".to_string(),
            });
        } else if !request.offload && requires_host_ram_bytes > host_ram_available {
            feasible_on_gpu = Some(false);
            failed_on = Some(FailedOn {
                card: card_id.clone(),
                gate: "host_ram".to_string(),
                requires_bytes: requires_host_ram_bytes,
                available_bytes: request.host_ram_bytes,
                reason: format!(
                    "the job needs {} bytes of host RAM, {} bytes are available, and offload is \
                     off; either raise the RAM or enable offload",
                    requires_host_ram_bytes,
                    request
                        .host_ram_bytes
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "no figure was given for the".to_string())
                ),
            });
        } else {
            feasible_on_gpu = Some(true);
        }
    }

    if feasible_on_gpu == Some(false) {
        if let Some(failed) = failed_on.as_ref() {
            notes.push(failed.reason.clone());
        }
    }

    // Every other catalogued card that clears the same gate, cheapest first.
    let mut alternative_cards: Vec<String> = Vec::new();
    for candidate in catalog::GPUS {
        if candidate.id.eq_ignore_ascii_case(&card_id) {
            continue;
        }
        let Some(vram) = candidate.vram_for_sizing() else {
            continue;
        };
        let ceiling = (vram as f64 * VRAM_GATE_HEADROOM).floor();
        if (requires_vram_bytes as f64) <= ceiling {
            alternative_cards.push(candidate.id.to_string());
        }
    }
    if !alternative_cards.is_empty() {
        notes.push(format!(
            "{} clears the documented 90 % VRAM gate too; the gate says the job fits, not that the \
             run will be quick: a card with little bandwidth will spend the time the recipe's own \
             estimate does not show",
            alternative_cards.join(", ")
        ));
    }

    // Fan-out. Wall clock is the single-rank estimate divided by the measured speedup, so the
    // GPU-hours *rise* with rank count — which is the point of showing them.
    let mut gpu_hours_at_k: Vec<GpuHoursRow> = Vec::new();
    let price = card.map(|card| card.price_usd_per_hour).unwrap_or(0.0);
    for (k_gpus, speedup) in DDP_SPEEDUPS {
        let wall_clock_hours = (request.base_minutes as f64 / 60.0) / speedup;
        let gpu_hours = wall_clock_hours * f64::from(k_gpus);
        gpu_hours_at_k.push(GpuHoursRow {
            k_gpus,
            speedup,
            wall_clock_hours: round2(wall_clock_hours),
            gpu_hours: round2(gpu_hours),
            cost_usd: round2(gpu_hours * price),
            speedup_source: FactSource::Observed,
        });
    }

    // Cost range across the cards that clear the gate: cheapest card at the fast end of the minute
    // range, dearest card at the slow end. Rounding to whole dollars, because the estimate has no
    // business pretending to cents.
    let low_minutes = request.estimated_minutes_min();
    let high_minutes = request.estimated_minutes_max();
    let mut low = f64::INFINITY;
    let mut high: f64 = 0.0;
    let mut excluded_from_cost: Vec<&str> = Vec::new();
    for candidate in catalog::GPUS {
        let Some(vram) = candidate.vram_for_sizing() else {
            continue;
        };
        if (requires_vram_bytes as f64) > (vram as f64 * VRAM_GATE_HEADROOM).floor() {
            continue;
        }
        // Only cards whose capacity someone measured. The gate above will accept a nameplate
        // figure, but a price built on one would quote a card no one has run this recipe on: on the
        // shipped catalog that is the T4, which clears the 90 % peak on paper and could not hold the
        // 27.8 GB result afterwards. Both ends of the range therefore come from cards with a
        // measured `usable_vram_bytes`, and the exclusion is stated rather than silent.
        let Some(measured) = candidate.usable_vram_bytes else {
            excluded_from_cost.push(candidate.id);
            continue;
        };
        if (requires_vram_bytes as f64) > (measured as f64 * VRAM_GATE_HEADROOM).floor() {
            continue;
        }
        let low_here = (low_minutes as f64 / 60.0) * candidate.price_usd_per_hour;
        let high_here = (high_minutes as f64 / 60.0) * candidate.price_usd_per_hour;
        low = low.min(low_here);
        high = high.max(high_here);
    }
    if !low.is_finite() {
        low = 0.0;
    }
    if !excluded_from_cost.is_empty() {
        notes.push(format!(
            "the cost range covers only cards with a measured VRAM figure, because a price for an unmeasured card would quote a run nobody has done; {} clear the 90 % peak on a nameplate figure and are excluded from the price",
            excluded_from_cost.join(", ")
        ));
    }
    let estimated_cost_usd = [low.round(), high.round()];

    notes.push(
        "this is a real offline job on a real GPU. Manarouter records the recipe and the command; \
         it does not run it for you"
            .to_string(),
    );
    match request.source.gated {
        GatedState::Open => {}
        GatedState::Auto | GatedState::Manual => notes.push(
            "quantisation creates a derivative of the source model. Check the source repo's licence \
             before publishing the result; the source repo is gated, so an HF token with access is \
             required before the job can read the weights"
                .to_string(),
        ),
    }
    notes.push(
        "a re-quantisation of an already-quantised checkpoint loses precision twice: the FP8 \
         weights must be dequantised to BF16 before they can be quantised again, so the result is \
         worse than a quantisation run from the BF16 original. Prefer the BF16 source when it exists"
            .to_string(),
    );

    let mut steps = vec![
        RecipeStep {
            command: format!("pip install \"llm-compressor=={TOOL_VERSION}\""),
            why: "the recipe needs the pinned version: its torch, transformers and \
                  compressed-tensors pins are what the scripts were written against"
                .to_string(),
        },
        RecipeStep {
            command: format!("write requantize.py (see {RECIPES_URL} §2.2)"),
            why: "llm-compressor registers no CLI entry point, so there is no `llmcompressor` \
                  command to run; the job is a Python file"
                .to_string(),
        },
        RecipeStep {
            command: "python requantize.py".to_string(),
            why: "runs the conversion over the calibration set and writes a new checkpoint \
                  directory; hours, not minutes"
                .to_string(),
        },
        RecipeStep {
            command:
                "upload the produced directory to the operator's own HF repo or object store"
                    .to_string(),
            why: "the deployment has to point somewhere durable; a Modal container's disk is not"
                .to_string(),
        },
        RecipeStep {
            command: "vllm serve <your-repo>   # no --quantization flag: it is read from config.json"
                .to_string(),
            why: "passing `--quantization` derived from the requested target raises ValueError, \
                  because the produced config says compressed-tensors rather than the name that was \
                  asked for"
                .to_string(),
        },
    ];

    if request.source.gated != GatedState::Open {
        steps.insert(
            1,
            RecipeStep {
                command: "hf auth login  # the source repo is gated: accept its terms first"
                    .to_string(),
                why: "the weights cannot be read without an account that has accepted the terms"
                    .to_string(),
            },
        );
    }

    RequantizePlan {
        target: request.target.clone(),
        source: request.source.clone(),
        path: request.path,
        unavailable_reason: "the repository ships FP8 only; no AWQ sibling and no AWQ tensors"
            .to_string(),
        card: Some(card_id),
        requires_vram_bytes,
        requires_extra_vram_bytes: REQUIRES_EXTRA_VRAM_BYTES,
        requires_host_ram_bytes,
        requires_disk_bytes,
        estimated_minutes: request.base_minutes,
        estimated_minutes_range: [
            request.estimated_minutes_min(),
            request.estimated_minutes_max(),
        ],
        estimated_cost_usd,
        tool: TOOL.to_string(),
        tool_version: TOOL_VERSION.to_string(),
        steps,
        serve_command: "vllm serve <your-repo> --max-model-len 262144 --kv-cache-dtype fp8"
            .to_string(),
        runs_remotely: true,
        vram_gate_evaluated,
        feasible_on_gpu,
        card_vram_bytes,
        failed_on,
        alternative_cards,
        gpu_hours_at_k,
        k_gpu: request.k_gpu,
        k_ram: request.k_ram,
        publishable: false,
        notes,
    }
}

impl RecipeRequest {
    /// The fast end of the estimate: 450 minutes for the dense path.
    pub fn estimated_minutes_min(&self) -> u64 {
        match self.path {
            RequantizePath::Dense => 450,
            RequantizePath::ModelFree => 30,
        }
    }

    /// The slow end: 515 minutes for the dense path.
    pub fn estimated_minutes_max(&self) -> u64 {
        match self.path {
            RequantizePath::Dense => 515,
            RequantizePath::ModelFree => 45,
        }
    }
}

/// The card the recipe's own estimate and GPU-hours row are stated for.
pub const REFERENCE_CARD: &str = "L40S";

/// The order vLLM probes quantisation methods in, as read from its source.
///
/// Useful for answering "does vLLM even know this name?" — and for seeing that `compressed-tensors`
/// is detected by content, while `awq` is the legacy AutoAWQ spelling.
pub fn probe_order() -> &'static [&'static str] {
    &[
        "auto_gptq",
        "gptq",
        "gptq_marlin",
        "auto_awq",
        "awq",
        "awq_marlin",
        "inc",
        "moe_wna16",
        "modelopt",
        "modelopt_fp4",
        "modelopt_mxfp8",
        "modelopt_int8",
        "mxfp8",
        "modelopt_mixed",
        "mxfp4",
        "gpt_oss_mxfp4",
        "deepseek_v4_fp8",
        "humming",
    ]
}

/// Whether a string would be accepted as a `--quantization` value by the vLLM version this crate
/// was written against.
pub fn is_known_quantization_flag(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    probe_order().contains(&value) || catalog::quant_method_by_id(value).is_some()
}

/// Host RAM the recipe needs: the checkpoint's weights at two bytes each, scaled by the documented
/// `k_ram` multiplier for the dequantised working copy.
///
/// This is computed from the checkpoint rather than read off the request, so lowering the figure the
/// host is *said* to have can never quietly lower what the job is said to need.
pub fn host_ram_requirement(params: u64, k_ram: f64) -> u64 {
    ((params as f64) * 2.0 * k_ram).round() as u64
}

/// Recompute the dense path's peak VRAM from the recipe's own arithmetic.
///
/// Per-layer slice at two bytes, held twice (the BF16 copy and GPTQ's own accumulation), multiplied
/// by the four ranks the recipe spreads over, plus the fixed vision tower and runtime allowances.
/// Returns `None` when `layers` is zero, because then the estimate is not defined.
pub fn recomputed_dense_vram_bytes(params: u64, layers: u64) -> Option<u64> {
    if layers == 0 {
        return None;
    }
    let slice = params.div_ceil(layers);
    let bf16 = slice.checked_mul(2)?;
    let held_twice = bf16.checked_mul(2)?;
    let across_ranks = held_twice.checked_mul(4)?;
    across_ranks
        .checked_add(VISION_TOWER_BYTES)?
        .checked_add(RUNTIME_BYTES)
}

/// The vision tower's weights, charged once to the converting rank. An upper bound, not a reading.
pub const VISION_TOWER_BYTES: u64 = 4_294_967_296;
/// Runtime and scratch on the converting rank.
pub const RUNTIME_BYTES: u64 = 2_147_483_648;

/// Rounds to two decimal places, so a published figure does not pretend to more precision.
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> RequantizePlan {
        plan_requantize(&RecipeRequest::default())
    }

    #[test]
    fn the_target_name_is_not_the_produced_name_and_the_plan_says_so() {
        let plan = plan();
        assert_eq!(plan.target.id, "awq");
        assert_eq!(plan.target.produced_quant_method, "compressed-tensors");
        assert_eq!(plan.target.produced_format, "pack-quantized");
        assert_ne!(plan.target.id, plan.target.produced_quant_method);
        assert!(plan
            .notes
            .iter()
            .any(|note| note.contains("raises ValueError") && note.contains("never emitted")));
    }

    /// What a recipe step would actually execute: a `#` starts a comment, and a comment is not
    /// argv. Several steps *talk about* `--quantization` in exactly those words, so asserting on
    /// raw text would both fail on honest comments and pass on a real flag hidden in one.
    fn argv_of(command: &str) -> Vec<&str> {
        command
            .split('#')
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .collect()
    }

    /// Whether any token is `--quantization`, in either the space-separated or `=` spelling.
    fn passes_quantization_flag(command: &str) -> bool {
        argv_of(command)
            .iter()
            .any(|token| *token == "--quantization" || token.starts_with("--quantization="))
    }

    #[test]
    fn no_quantization_flag_is_ever_derived_from_the_requested_target() {
        // The trap: `--quantization awq` against a compressed-tensors artefact makes vLLM refuse to
        // start. Nothing here may emit it.
        //
        // The positive control comes first, because a detector that has quietly stopped detecting
        // anything would make every assertion below vacuous: this exact command line is what a
        // regression would look like, and the helper must still see the flag in it.
        assert!(
            passes_quantization_flag("vllm serve <your-repo> --quantization awq"),
            "the detector must still see a flag that is genuinely there"
        );
        assert!(passes_quantization_flag(
            "vllm serve m --quantization=compressed-tensors"
        ));
        assert!(!passes_quantization_flag(
            "vllm serve m  # no --quantization flag: it is read from config.json"
        ));

        let plan = plan();
        let commands: Vec<&str> = std::iter::once(plan.serve_command.as_str())
            .chain(plan.steps.iter().map(|step| step.command.as_str()))
            .collect();
        for command in &commands {
            assert!(
                !passes_quantization_flag(command),
                "a quantisation flag must never be synthesised from the request: {command}"
            );
            assert!(
                !argv_of(command).iter().any(|token| *token == "--awq"
                    || token.starts_with("--awq=")
                    || token.eq_ignore_ascii_case("awq")),
                "the target's name must not leak into a command line: {command}"
            );
        }

        // And the same for the model-free path, and for the FP8 source.
        for variant in [
            plan_requantize(&RecipeRequest::model_free()),
            plan_requantize(&RecipeRequest::default().from_fp8()),
        ] {
            for command in std::iter::once(&variant.serve_command)
                .chain(variant.steps.iter().map(|step| &step.command))
            {
                assert!(!passes_quantization_flag(command), "{command}");
            }
        }
    }

    #[test]
    fn the_serve_command_is_plain_and_reads_the_method_from_the_config() {
        let plan = plan();
        assert!(plan.serve_command.starts_with("vllm serve "));
        assert!(plan.serve_command.contains("--max-model-len 262144"));
        assert!(plan.serve_command.contains("--kv-cache-dtype fp8"));
        assert!(!plan.serve_command.contains("--quantization"));
    }

    #[test]
    fn awq_is_not_the_name_vllm_will_read_back() {
        // Both names are things vLLM knows, but they are not interchangeable: one is the legacy
        // AutoAWQ spelling, the other is what llm-compressor writes.
        assert!(is_known_quantization_flag("awq"));
        assert!(is_known_quantization_flag("compressed-tensors"));
        assert!(!is_known_quantization_flag("w4a16-rtn"));
        assert!(!is_known_quantization_flag(""));
        assert_eq!(probe_order()[0], "auto_gptq");
        assert!(probe_order().contains(&"awq"));
    }

    #[test]
    fn the_recipe_writes_a_python_file_because_there_is_no_cli() {
        let plan = plan();
        assert_eq!(plan.tool, "llm-compressor");
        assert_eq!(plan.tool_version, "0.15.0");
        let commands: Vec<&str> = plan
            .steps
            .iter()
            .map(|step| step.command.as_str())
            .collect();
        assert!(commands
            .iter()
            .any(|command| command.starts_with("pip install \"llm-compressor==0.15.0\"")));
        let python_file = format!("write requantize.py (see {RECIPES_URL} §2.2)");
        assert!(commands.iter().any(|command| *command == python_file));
        assert!(commands.contains(&"python requantize.py"));
        assert!(
            !commands
                .iter()
                .any(|command| command.starts_with("llmcompressor")),
            "there is no llmcompressor CLI entry point to call: {commands:?}"
        );
        assert!(commands
            .iter()
            .any(|command| command.contains("upload the produced directory")));
    }

    #[test]
    fn a_gated_source_adds_the_login_step_and_the_licence_note() {
        let open = plan();
        assert!(!open
            .steps
            .iter()
            .any(|step| step.command.contains("hf auth login")));
        assert!(!open.notes.iter().any(|note| note.contains("licence")));

        let gated = plan_requantize(&RecipeRequest::default().from_fp8());
        assert!(gated
            .steps
            .iter()
            .any(|step| step.command.contains("hf auth login")));
        assert!(gated
            .notes
            .iter()
            .any(|note| note.contains("Check the source repo's licence")));
    }

    #[test]
    fn nothing_promises_the_result_may_be_published() {
        let plan = plan();
        assert!(!plan.publishable);
        assert!(plan
            .notes
            .iter()
            .any(|note| note.contains("does not run it for you")));
        assert!(plan.runs_remotely);
    }

    #[test]
    fn the_gate_passes_on_the_two_catalogue_cards_with_the_headroom_to_run_it() {
        for card in ["L40S", "RTX-PRO-6000"] {
            let plan = plan_requantize(&RecipeRequest {
                card: Some(card.to_string()),
                ..RecipeRequest::default()
            });
            assert!(plan.vram_gate_evaluated, "{card}");
            assert_eq!(plan.feasible_on_gpu, Some(true), "{card}");
            assert_eq!(plan.failed_on, None, "{card}");
            assert!(
                plan.card_vram_bytes.expect("vram") > plan.requires_vram_bytes,
                "{card}"
            );
        }
        assert_eq!(DENSE_REQUIRES_VRAM_BYTES, 12_884_901_888);
        assert_eq!(REQUIRES_EXTRA_VRAM_BYTES, 0);
    }

    #[test]
    fn a_card_with_no_published_vram_leaves_the_gate_unevaluated_rather_than_passing() {
        let plan = plan_requantize(&RecipeRequest {
            card: Some("B300".to_string()),
            ..RecipeRequest::default()
        });
        assert!(!plan.vram_gate_evaluated);
        assert_eq!(plan.feasible_on_gpu, None);
        assert_eq!(plan.failed_on, None);
        assert!(plan
            .notes
            .iter()
            .any(|note| note.contains("null rather than true")));
    }

    #[test]
    fn an_unknown_card_is_refused_by_the_gate_not_assumed_to_fit() {
        let plan = plan_requantize(&RecipeRequest {
            card: Some("not-a-card".to_string()),
            ..RecipeRequest::default()
        });
        assert!(!plan.vram_gate_evaluated);
        assert_eq!(plan.feasible_on_gpu, None);
        assert_eq!(plan.card_vram_bytes, None);
    }

    #[test]
    fn the_gate_order_is_vram_then_disk_then_host_ram() {
        // No card in the shipped catalog fails the dense VRAM gate — the smallest of them, the T4,
        // clears it — so the order is pinned with the two gates that can fail on a real request: a
        // passing VRAM check leaves disk to stop the job, and a provisioned disk leaves host RAM.
        // The smallest card's clearance is asserted rather than assumed, so this test would notice
        // if a future catalog row made the VRAM gate reachable.
        let plan = plan_requantize(&RecipeRequest {
            card: Some("T4".to_string()),
            path: RequantizePath::Dense,
            offload: false,
            provisioned_disk_bytes: Some(0),
            host_ram_bytes: Some(1),
            ..RecipeRequest::default()
        });
        assert_eq!(plan.card_vram_bytes, Some(17_179_869_184));
        assert!(
            DENSE_REQUIRES_VRAM_BYTES as f64 <= (17_179_869_184f64 * VRAM_GATE_HEADROOM).floor(),
            "the premise: the T4 passes VRAM, so disk is the first gate that can stop it"
        );
        let failed = plan.failed_on.expect("the plan must name what stopped it");
        assert_eq!(failed.gate, "disk");
        assert_eq!(failed.requires_bytes, 0);
        assert_eq!(failed.available_bytes, None);

        // With the VRAM gate satisfied, the host RAM gate is the one that reports.
        let ram = plan_requantize(&RecipeRequest {
            card: Some("L40S".to_string()),
            offload: false,
            host_ram_bytes: Some(1_000_000),
            ..RecipeRequest::default()
        });
        let failed = ram.failed_on.expect("the plan must name what stopped it");
        assert_eq!(failed.gate, "host_ram");
        assert_eq!(ram.feasible_on_gpu, Some(false));
        assert!(failed.reason.contains("enable offload"));
        // The requirement is the recipe's, computed from the checkpoint; the figure the operator
        // supplied is only what the host has, and is never allowed to lower the requirement.
        assert_eq!(failed.requires_bytes, 69_453_569_880);
        assert_eq!(failed.available_bytes, Some(1_000_000));

        // With offload on, the same host RAM is acceptable and the plan says why.
        let offloaded = plan_requantize(&RecipeRequest {
            card: Some("L40S".to_string()),
            offload: true,
            host_ram_bytes: Some(1_000_000),
            ..RecipeRequest::default()
        });
        assert_eq!(offloaded.feasible_on_gpu, Some(true));
        assert!(offloaded
            .notes
            .iter()
            .any(|note| note.contains("auto_offload")));
    }

    #[test]
    fn the_vram_gate_boundary_is_exact_on_the_smallest_card() {
        // T4 usable VRAM is 16 GiB (17 179 869 184 bytes) → ceiling floor(17 179 869 184 × 0.9) =
        // 15 461 882 265, above the dense requirement, so the T4 passes. A 10 GiB card would not,
        // which is the boundary this pins: the arithmetic, not a card name.
        let ceiling = (17_179_869_184f64 * VRAM_GATE_HEADROOM).floor();
        assert_eq!(ceiling, 15_461_882_265.0);
        assert!(DENSE_REQUIRES_VRAM_BYTES as f64 <= ceiling);
        let ten_gib = (10u64 * GIB) as f64 * VRAM_GATE_HEADROOM;
        assert!(
            (DENSE_REQUIRES_VRAM_BYTES as f64) > ten_gib.floor(),
            "a 10 GiB card is where the dense peak stops fitting"
        );
        let plan = plan_requantize(&RecipeRequest {
            card: Some("T4".to_string()),
            ..RecipeRequest::default()
        });
        assert_eq!(plan.feasible_on_gpu, Some(true));
        assert!(plan.alternative_cards.iter().any(|card| card == "L40S"));
    }

    #[test]
    fn the_fan_out_rows_show_that_gpu_hours_rise_with_the_rank_count() {
        let plan = plan();
        assert_eq!(plan.k_gpu, DEFAULT_K_GPU);
        let rows = &plan.gpu_hours_at_k;
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].k_gpus, 1);
        assert_eq!(rows[0].wall_clock_hours, 8.0);
        assert_eq!(rows[0].gpu_hours, 8.0);
        // 8 hours at the L40S's documented 1.9512/h.
        assert_eq!(rows[0].cost_usd, 15.61);

        assert_eq!(rows[1].k_gpus, 2);
        assert_eq!(rows[1].wall_clock_hours, 4.0);
        assert_eq!(rows[1].gpu_hours, 8.0);

        assert_eq!(rows[2].k_gpus, 4);
        assert_eq!(rows[2].speedup, 3.8);
        assert_eq!(rows[2].wall_clock_hours, 2.11);
        assert_eq!(rows[2].gpu_hours, 8.42);
        assert!(
            rows[2].cost_usd > rows[0].cost_usd,
            "5 % off perfect at four ranks costs money"
        );
        assert!(rows
            .iter()
            .all(|row| row.speedup_source == FactSource::Observed));

        // The measured speedups themselves are the recipe's, not a guess.
        assert_eq!(DDP_SPEEDUPS, [(1, 1.0), (2, 2.0), (4, 3.8)]);
    }

    #[test]
    fn the_cost_range_spans_the_cheapest_card_fast_and_the_dearest_card_slow() {
        let plan = plan();
        assert_eq!(plan.estimated_cost_usd, [15.0, 26.0]);
        assert_eq!(plan.estimated_minutes, 480);
        assert_eq!(plan.estimated_minutes_range, [450, 515]);
        // The extremes are reproducible from the prices the catalogue documents.
        let low: f64 = 450.0 / 60.0 * 1.9512;
        let high: f64 = 515.0 / 60.0 * 3.0312;
        assert_eq!(low.round(), plan.estimated_cost_usd[0]);
        assert_eq!(high.round(), plan.estimated_cost_usd[1]);
    }

    #[test]
    fn the_model_free_path_is_the_cheap_one_pass_alternative() {
        let plan = plan_requantize(&RecipeRequest::model_free());
        assert_eq!(plan.path, RequantizePath::ModelFree);
        assert_eq!(plan.path.as_str(), "model-free");
        assert_eq!(plan.target.label, "4-bit RTN (W4A16)");
        assert_eq!(plan.target.produced_quant_method, PRODUCED_QUANT_METHOD);
        assert_eq!(plan.requires_vram_bytes, 8_589_934_592);
        assert_eq!(plan.requires_disk_bytes, MODEL_FREE_DISK_BYTES);
        assert_eq!(plan.estimated_minutes, 35);
        assert_eq!(plan.estimated_minutes_range, [30, 45]);
        assert_eq!(plan.feasible_on_gpu, Some(true));
        // Its requested name is not a vLLM quantisation method at all, which is exactly why the
        // flag must never be derived from it.
        assert!(!is_known_quantization_flag(&plan.target.id));
    }

    #[test]
    fn the_documented_vram_figure_and_the_recomputed_one_are_both_visible() {
        let recomputed = recomputed_dense_vram_bytes(27_781_427_952, 64).expect("a figure");
        assert_eq!(recomputed, 13_387_807_936);
        assert!(recomputed > DENSE_REQUIRES_VRAM_BYTES);
        assert_eq!(recomputed - DENSE_REQUIRES_VRAM_BYTES, 502_906_048);

        let plan = plan();
        assert_eq!(plan.requires_vram_bytes, DENSE_REQUIRES_VRAM_BYTES);
        let note = plan
            .notes
            .iter()
            .find(|note| note.contains("recomputing the dense peak"))
            .expect("the difference is stated, not hidden");
        assert!(note.contains("13387807936"), "{note}");
        assert!(note.contains("12.47 GiB"), "{note}");
        assert!(note.contains("0.47 GiB"), "{note}");

        assert_eq!(recomputed_dense_vram_bytes(1, 0), None);
    }

    #[test]
    fn host_ram_and_disk_are_stated_with_their_multiplier() {
        let plan = plan();
        assert_eq!(plan.k_ram, 1.25);
        assert_eq!(plan.requires_host_ram_bytes, 69_453_569_880);
        assert_eq!(DEFAULT_HOST_RAM_BYTES, 69_000_000_000);
        assert!(plan.requires_host_ram_bytes > DEFAULT_HOST_RAM_BYTES);
        assert_eq!(plan.requires_disk_bytes, DEFAULT_DISK_BYTES);
        assert!(plan
            .notes
            .iter()
            .any(|note| note.contains("device_map=\"auto_offload\"")));
    }

    #[test]
    fn the_extra_vram_is_zero_because_compression_is_not_tensor_parallel() {
        let plan = plan();
        assert_eq!(plan.requires_extra_vram_bytes, 0);
        assert_eq!(plan.requires_extra_vram_bytes, REQUIRES_EXTRA_VRAM_BYTES);
    }

    #[test]
    fn the_plan_lists_the_alternative_cards_that_clear_the_same_gate() {
        let plan = plan();
        assert!(plan
            .alternative_cards
            .iter()
            .any(|card| card == "RTX-PRO-6000"));
        assert!(plan.alternative_cards.iter().any(|card| card == "T4"));
        assert!(
            !plan
                .alternative_cards
                .iter()
                .any(|card| card == REFERENCE_CARD),
            "the chosen card is not its own alternative"
        );
        assert!(plan
            .notes
            .iter()
            .any(|note| note.contains("the run will be quick")));
    }

    #[test]
    fn the_default_source_is_the_ungated_bf16_original_and_the_fallback_is_not() {
        let open = plan();
        assert_eq!(open.source.id, "Qwen/Qwen3.8-27B");
        assert_eq!(open.source.bytes_per_param, Some(2.0));
        assert_eq!(open.source.gated, GatedState::Open);
        assert_eq!(open.source.license.as_deref(), Some("Apache-2.0"));

        let fallback = plan_requantize(&RecipeRequest::default().from_fp8());
        assert_eq!(fallback.source.id, "orcarouter/Qwen3.8-27B-Uncensored-FP8");
        assert_eq!(fallback.source.gated, GatedState::Manual);
        assert_eq!(fallback.source.license, None);
        assert!(fallback
            .notes
            .iter()
            .any(|note| note.contains("loses precision twice")));
    }

    #[test]
    fn the_unavailable_reason_is_the_one_the_documentation_gives() {
        assert_eq!(
            plan().unavailable_reason,
            "the repository ships FP8 only; no AWQ sibling and no AWQ tensors"
        );
    }

    #[test]
    fn the_plan_round_trips_through_json_with_every_honesty_field() {
        let plan = plan();
        let value = serde_json::to_value(&plan).expect("serialises");
        for field in [
            "requires_vram_bytes",
            "requires_extra_vram_bytes",
            "requires_host_ram_bytes",
            "requires_disk_bytes",
            "estimated_minutes",
            "estimated_minutes_range",
            "estimated_cost_usd",
            "tool",
            "tool_version",
            "steps",
            "serve_command",
            "vram_gate_evaluated",
            "feasible_on_gpu",
            "failed_on",
            "gpu_hours_at_k",
            "k_gpu",
            "k_ram",
            "publishable",
            "notes",
        ] {
            assert!(
                value.get(field).is_some(),
                "the wire shape must carry `{field}`"
            );
        }
        assert_eq!(value["path"], serde_json::json!("dense"));
        assert_eq!(
            value["target"]["produced_quant_method"],
            serde_json::json!("compressed-tensors")
        );
        assert_eq!(value["feasible_on_gpu"], serde_json::json!(true));
        assert_eq!(value["failed_on"], serde_json::Value::Null);

        let again: RequantizePlan = serde_json::from_value(value).expect("deserialises");
        assert_eq!(again, plan);
    }
}
