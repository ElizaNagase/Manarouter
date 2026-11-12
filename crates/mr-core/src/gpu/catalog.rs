//! The verified GPU catalog: what each card is, what it costs, what it may execute, and which
//! platform facts the generated bundle must honour.
//!
//! ## Why this is a table in the crate and not a fetch at runtime
//!
//! Modal publishes no per-GPU VRAM endpoint and no machine-readable price feed, so there is nothing
//! to fetch. The alternative to a table is scraping `modal.com/pricing` at runtime, which would make
//! a price a parser's opinion and would let an HTML change silently alter what the operator is told
//! an hour of GPU costs. A table with a `checked_at` date, a `source_url` per row, and a test per
//! number is the honest shape: when the number is wrong, the failure is a visible date, not a
//! plausible-looking invoice.
//!
//! ## Provenance is per field, not per row
//!
//! Prices and canonical ids were re-verified against Modal's pricing page and `gpu.md`; **VRAM was
//! not.** Modal publishes no VRAM table at all, so a card's memory figure comes from either Modal's
//! own prose (`docs`), an NVIDIA datasheet (`datasheet`), or the operator's measurement inside a
//! running container (`observed`). Those are not equally authoritative and the catalog says so per
//! field via [`GpuSpec::vram_source`]. Datasheet capacity is also **not** usable capacity: the
//! operator measured 95 GiB usable on the RTX PRO 6000 (96 GB) and 46 GiB on the L40S (48 GB), so
//! both figures are carried and [`GpuSpec::usable_vram_bytes`] wins where it exists. The gap is then
//! visible in the sizing delta rather than hidden inside a fudge factor.
//!
//! ## The three questions callers actually have
//!
//! 1. [`gpu_by_id`] — "is this a card Modal will accept, and what does it cost?" The ids here are
//!    the exact `gpu=` values the generated `modal_app.py` emits, which is why `RTX-PRO-6000` is
//!    hyphenated while its label keeps the space.
//! 2. [`resolve_quant_for`] — "may this card execute this quantisation?" A verdict, not a boolean:
//!    `unsupported` is a documented impossibility, `unverified` means the sources do not settle it.
//!    Collapsing the two into one `false` would make the panel refuse a legal deployment, and
//!    collapsing them into `true` would let the operator rent a card that cannot boot the model.
//! 3. [`platform_by_id`] — "what will the platform let me do?" Concurrency ceilings, seats, and the
//!    CUDA floor that makes B300 unusable with a CUDA-12 image.

use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use crate::rental::{FactSource, FreePlan};

/// The date every row in this catalog was last checked against its `source_url`.
///
/// Exposed as a constant rather than copied into each row so a refresh changes one line and a stale
/// catalog is obvious from the API response alone.
pub const CATALOG_CHECKED_AT: &str = "2026-11-12";

/// Where the prices and canonical ids were read from.
pub const MODAL_PRICING_URL: &str = "https://modal.com/pricing";

/// Where the platform rules were read from: multi-GPU limits, the H100→H200 upgrade, CUDA floors.
pub const MODAL_GPU_DOCS_URL: &str = "https://modal.com/docs/guide/gpu.md";

/// The compute-capability table every `*_source: documented` capability below was taken from.
pub const CUDA_GPUS_URL: &str = "https://developer.nvidia.com/cuda/gpus";

/// vLLM's quantisation hardware matrix — the source of the `min_compute_capability` rows.
pub const VLLM_QUANT_MATRIX_URL: &str = "https://docs.vllm.ai/en/stable/features/quantization/";

/// vLLM's quantised-KV-cache page. Note this is a *storage* feature: distinct from FP8 GEMM.
pub const VLLM_KV_CACHE_URL: &str =
    "https://docs.vllm.ai/en/stable/features/quantization/quantized_kvcache/";

/// The operator's own measurements (issue #4), and the only source of `observed` figures here.
pub const ISSUE_4_URL: &str = "docs/research/issue-4-spec.md";

/// A CUDA compute capability, ordered so `>=` means "at least this capable".
///
/// It serialises as the string everyone else writes it as, `"8.9"`, because the ingress JSON is
/// read by people as well as parsers and `8.9` is what NVIDIA's tables say. Deserialisation accepts
/// either a string or a JSON number, so a hand-written config containing `8.9` is not rejected for
/// being an obvious typo-free float.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ComputeCapability {
    /// The major number, e.g. `8` in `8.9`.
    pub major: u8,
    /// The minor number, e.g. `9` in `8.9`.
    pub minor: u8,
}

impl ComputeCapability {
    /// Construct a capability. `const` so the catalog rows can be built at compile time.
    pub const fn new(major: u8, minor: u8) -> Self {
        Self { major, minor }
    }

    /// Read `"8.9"` (or `"9"`) — the form NVIDIA and vLLM both use.
    pub fn parse(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        let (major, minor) = match trimmed.split_once('.') {
            Some((major, minor)) => (major, minor),
            None => (trimmed, "0"),
        };
        let major = major.trim().parse::<u8>().ok()?;
        let minor = minor.trim().parse::<u8>().ok()?;
        Some(Self { major, minor })
    }

    /// Read a JSON number such as `8.9`. `9` and `9.0` both mean capability 9.0.
    ///
    /// A float capability is a lossy encoding of two integers, so the minor part is rounded rather
    /// than truncated: `8.9` in binary floating point is `8.899999...`, and truncation would turn
    /// the one capability that decides FP8 support into 8.8.
    pub fn from_f64(value: f64) -> Option<Self> {
        if !value.is_finite() || value < 0.0 || value >= 256.0 {
            return None;
        }
        let major = value.trunc();
        let minor = ((value - major) * 10.0).round();
        if !(0.0..=9.0).contains(&minor) {
            return None;
        }
        Some(Self {
            major: major as u8,
            minor: minor as u8,
        })
    }

    /// `"8.9"` — the canonical spelling, and what the wire format carries.
    pub fn as_string(&self) -> String {
        format!("{}.{}", self.major, self.minor)
    }
}

impl std::fmt::Display for ComputeCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_string())
    }
}

impl Serialize for ComputeCapability {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_string())
    }
}

impl<'de> Deserialize<'de> for ComputeCapability {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Self, D::Error> {
        struct CapabilityVisitor;

        impl<'de> serde::de::Visitor<'de> for CapabilityVisitor {
            type Value = ComputeCapability;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a compute capability such as \"8.9\", 8.9, or 9")
            }

            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> Result<ComputeCapability, E> {
                ComputeCapability::parse(value).ok_or_else(|| {
                    E::custom(format!(
                        "cannot read {value:?} as a compute capability; \
                         expected \"<major>.<minor>\", for example \"8.9\""
                    ))
                })
            }

            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<ComputeCapability, E> {
                ComputeCapability::from_f64(value).ok_or_else(|| {
                    E::custom(format!("cannot read {value} as a compute capability"))
                })
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<ComputeCapability, E> {
                if value > u64::from(u8::MAX) {
                    return Err(E::custom(format!(
                        "{value} is not a compute capability major number"
                    )));
                }
                Ok(ComputeCapability::new(value as u8, 0))
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<ComputeCapability, E> {
                if value < 0 || value > i64::from(u8::MAX) {
                    return Err(E::custom(format!(
                        "{value} is not a compute capability major number"
                    )));
                }
                Ok(ComputeCapability::new(value as u8, 0))
            }
        }

        deserializer.deserialize_any(CapabilityVisitor)
    }
}

/// Where a card's VRAM figure came from. Weaker evidence than the price, and labelled as such.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VramSource {
    /// Modal's own documentation states this figure in prose.
    Docs,
    /// An NVIDIA (or partner) datasheet figure. Nameplate capacity, not usable capacity.
    Datasheet,
    /// Measured inside a running container by the operator. The only figure that is usable capacity.
    Observed,
    /// A widely repeated number that was not confirmed from a primary source in the research pass.
    Unverified,
}

impl VramSource {
    /// The wire spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            VramSource::Docs => "docs",
            VramSource::Datasheet => "datasheet",
            VramSource::Observed => "observed",
            VramSource::Unverified => "unverified",
        }
    }
}

/// One card, as Modal offers it, with every fact's provenance attached.
///
/// `Serialize` only: this is a response shape. It is never read back from JSON, and saying so in the
/// type stops anyone treating the table as operator-editable configuration.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GpuSpec {
    /// The canonical `gpu=` value, exactly as the generated `modal_app.py` must emit it.
    pub id: &'static str,
    /// The human label. Differs from `id` on purpose: `RTX-PRO-6000` vs `RTX PRO 6000`.
    pub label: &'static str,
    /// Nameplate capacity as its source states it, read as GiB. `None` when nothing sources it.
    ///
    /// The unit is deliberate: sources mix GiB and decimal GB, and pretending to convert between
    /// them would invent precision. The figure is consumed as GiB and the resulting error shows up
    /// in the sizing delta, which is where a mixed-unit number belongs.
    pub vram_gib: Option<f64>,
    /// [`Self::vram_gib`] in bytes, for the sizing arithmetic. `None` when [`Self::vram_gib`] is.
    pub vram_bytes: Option<u64>,
    /// Where [`Self::vram_gib`] came from. `None` exactly when the two VRAM fields above are `None`.
    pub vram_source: Option<VramSource>,
    /// Capacity the operator actually saw inside a container, when it was measured.
    ///
    /// This is what sizing uses in preference to the nameplate figure, because a container that
    /// cannot allocate the last GiB will OOM mid-request rather than politely refusing.
    pub usable_vram_gib: Option<f64>,
    /// [`Self::usable_vram_gib`] in bytes.
    pub usable_vram_bytes: Option<u64>,
    /// Billed USD per GPU-second, as published. The unit Modal actually charges in.
    pub price_usd_per_second: f64,
    /// Billed USD per GPU-hour. Must equal `price_usd_per_second * 3600`; asserted by a test.
    pub price_usd_per_hour: f64,
    /// What the free plan allows for this card, as far as anything documents it.
    ///
    /// Modal documents **no** per-GPU gating: every card carries an unconditional price and the
    /// binding constraint is the payment-method requirement in [`PlatformSpec`]. So this is
    /// `unknown` for every card except the one the operator has an observation about.
    pub free_plan: FreePlan,
    /// Where [`Self::free_plan`] came from — deliberately separate from [`Self::source`], which is
    /// the price's provenance.
    ///
    /// Modal publishes no per-GPU free-tier gating, so `unknown` is a *documented* state on ten of
    /// the eleven cards: what the documentation says is that there is no per-GPU table. The eleventh
    /// is one operator's observation on a Starter workspace, and only that one may be `observed`.
    /// Keeping the two provenances apart is what stops an observation from masquerading as a
    /// published price fact, or a published price from borrowing an observation's authority.
    pub free_plan_source: FactSource,
    /// Provenance of the id and the price.
    pub source: FactSource,
    /// When the price and id were last checked.
    pub checked_at: &'static str,
    /// Where the price and id were read from.
    pub source_url: &'static str,
    /// The card's CUDA compute capability. `None` when no source states it — which makes every
    /// quantisation verdict for that card `unverified` rather than optimistically assumed.
    pub compute_capability: Option<ComputeCapability>,
    /// Provenance of [`Self::compute_capability`].
    pub compute_capability_source: Option<FactSource>,
    /// Modal's ceiling on this card's count in one container. Not a flat 8: A10 is 4.
    pub max_gpus_per_container: u32,
    /// The minimum CUDA toolkit version the container image must use, when Modal requires one.
    pub min_cuda: Option<&'static str>,
    /// Whether requesting [`Self::id`] pins the hardware.
    ///
    /// False for `H100`, which Modal may silently run on an H200. That matters because it changes
    /// the usable VRAM, so an H100 request cannot be budgeted as an 80 GiB card unless it is pinned.
    pub pins_hardware: bool,
    /// The spelling that *does* pin the hardware, when one exists (`H100!`).
    pub pinned_spelling: Option<&'static str>,
    /// Anything the generated bundle must know about this card.
    pub notes: &'static str,
}

impl GpuSpec {
    /// The VRAM figure sizing should use: what a container can allocate, else nameplate capacity.
    ///
    /// Returns `None` when no source states any figure for the card. Guessing a plausible capacity
    /// is how a KV budget becomes fiction, so the caller is made to handle the gap.
    pub fn vram_for_sizing(&self) -> Option<u64> {
        self.usable_vram_bytes.or(self.vram_bytes)
    }

    /// Whether the card can do native FP8 GEMM (compute), not merely store FP8 (memory).
    ///
    /// These are separate axes and confusing them is the classic FP8 mistake: a T4 will happily hold
    /// an fp8 KV cache and a Q4 checkpoint, and will not execute FP8 W8A8 matmuls at all.
    pub fn supports_native_fp8_gemm(&self) -> bool {
        match self.compute_capability {
            Some(cc) => cc >= ComputeCapability::new(8, 9),
            None => false,
        }
    }
}

/// The compute capability below which native FP8 GEMM is unavailable, from vLLM's matrix.
pub const NATIVE_FP8_GEMM_MIN: ComputeCapability = ComputeCapability::new(8, 9);

/// Modal's card table, ordered cheapest first so a panel can render it as-is.
///
/// Eleven cards: the eight full-price compute cards plus the three A100/B200 family overlaps. B300
/// is priced but has **no** sourced VRAM figure, so its VRAM fields are `None` and it cannot be
/// sized — an honest gap rather than a guessed 288 GB.
pub static GPUS: &[GpuSpec] = &[
    GpuSpec {
        id: "T4",
        label: "T4",
        vram_gib: Some(16.0),
        vram_bytes: Some(17_179_869_184),
        vram_source: Some(VramSource::Datasheet),
        usable_vram_gib: None,
        usable_vram_bytes: None,
        price_usd_per_second: 0.000164,
        price_usd_per_hour: 0.5904,
        free_plan: FreePlan::Unknown,
        free_plan_source: FactSource::Documented,
        source: FactSource::Documented,
        checked_at: CATALOG_CHECKED_AT,
        source_url: MODAL_PRICING_URL,
        compute_capability: Some(ComputeCapability::new(7, 5)),
        compute_capability_source: Some(FactSource::Documented),
        max_gpus_per_container: 8,
        min_cuda: None,
        pins_hardware: true,
        pinned_spelling: None,
        notes: "Turing, sm75. FP8 is memory-only here: `--kv-cache-dtype fp8` works, native FP8 GEMM \
                does not. The cheapest card in the catalog and the only one whose AWQ verdict the \
                sources disagree about (see `awq` in the quant matrix notes), so that verdict ships \
                unverified rather than resolved.",
    },
    GpuSpec {
        id: "L4",
        label: "L4",
        vram_gib: Some(24.0),
        vram_bytes: Some(25_769_803_776),
        vram_source: Some(VramSource::Datasheet),
        usable_vram_gib: None,
        usable_vram_bytes: None,
        price_usd_per_second: 0.000222,
        price_usd_per_hour: 0.7992,
        free_plan: FreePlan::Unknown,
        free_plan_source: FactSource::Documented,
        source: FactSource::Documented,
        checked_at: CATALOG_CHECKED_AT,
        source_url: MODAL_PRICING_URL,
        compute_capability: Some(ComputeCapability::new(8, 9)),
        compute_capability_source: Some(FactSource::Documented),
        max_gpus_per_container: 8,
        min_cuda: None,
        pins_hardware: true,
        pinned_spelling: None,
        notes: "Ada, sm89. The cheapest card here that can execute FP8 W8A8, because native FP8 GEMM \
                needs sm89 or better. For an FP8 checkpoint this card is the small option; for a \
                BF16 checkpoint it buys nothing over the A10.",
    },
    GpuSpec {
        id: "A10",
        label: "A10",
        vram_gib: Some(24.0),
        vram_bytes: Some(25_769_803_776),
        vram_source: Some(VramSource::Docs),
        usable_vram_gib: None,
        usable_vram_bytes: None,
        price_usd_per_second: 0.000306,
        price_usd_per_hour: 1.1016,
        free_plan: FreePlan::Unknown,
        free_plan_source: FactSource::Documented,
        source: FactSource::Documented,
        checked_at: CATALOG_CHECKED_AT,
        source_url: MODAL_PRICING_URL,
        compute_capability: Some(ComputeCapability::new(8, 6)),
        compute_capability_source: Some(FactSource::Documented),
        max_gpus_per_container: 4,
        min_cuda: None,
        pins_hardware: true,
        pinned_spelling: None,
        notes: "Ampere GA102, sm86. `A10` is the canonical `gpu=` value; `A10G` appears in Modal's \
                own blog and one pricing FAQ example and is not valid, so the bundle must never \
                emit it. Maximum four per container rather than the usual eight, and no native FP8 \
                GEMM: FP8 weights here need the Marlin weight-only path.",
    },
    GpuSpec {
        id: "L40S",
        label: "L40S",
        vram_gib: Some(48.0),
        vram_bytes: Some(51_539_607_552),
        vram_source: Some(VramSource::Docs),
        usable_vram_gib: Some(46.0),
        usable_vram_bytes: Some(49_392_123_904),
        price_usd_per_second: 0.000542,
        price_usd_per_hour: 1.9512,
        free_plan: FreePlan::Unknown,
        free_plan_source: FactSource::Documented,
        source: FactSource::Documented,
        checked_at: CATALOG_CHECKED_AT,
        source_url: MODAL_PRICING_URL,
        compute_capability: Some(ComputeCapability::new(8, 9)),
        compute_capability_source: Some(FactSource::Documented),
        max_gpus_per_container: 8,
        min_cuda: None,
        pins_hardware: true,
        pinned_spelling: None,
        notes: "Ada, sm89, native FP8 GEMM. Modal documents 48 GB; the operator measured 46 GiB \
                usable with the same model that was measured on the RTX PRO 6000 (issue #4). Sizing \
                uses the measured figure. Cheaper per hour than the A100-40GB.",
    },
    GpuSpec {
        id: "A100-40GB",
        label: "A100 40GB",
        vram_gib: Some(40.0),
        vram_bytes: Some(42_949_672_960),
        vram_source: Some(VramSource::Datasheet),
        usable_vram_gib: None,
        usable_vram_bytes: None,
        price_usd_per_second: 0.000583,
        price_usd_per_hour: 2.0988,
        free_plan: FreePlan::Unknown,
        free_plan_source: FactSource::Documented,
        source: FactSource::Documented,
        checked_at: CATALOG_CHECKED_AT,
        source_url: MODAL_PRICING_URL,
        compute_capability: Some(ComputeCapability::new(8, 0)),
        compute_capability_source: Some(FactSource::Documented),
        max_gpus_per_container: 8,
        min_cuda: None,
        pins_hardware: true,
        pinned_spelling: None,
        notes: "Ampere A100, sm80. FP8 storage works (CUDA 11.8+) and native FP8 GEMM does not \
                (sm80 < sm89), so an FP8 checkpoint runs here only through the weight-only path. \
                Note the L40S is both cheaper and faster than this card for FP8 work.",
    },
    GpuSpec {
        id: "A100-80GB",
        label: "A100 80GB",
        vram_gib: Some(80.0),
        vram_bytes: Some(85_899_345_920),
        vram_source: Some(VramSource::Datasheet),
        usable_vram_gib: None,
        usable_vram_bytes: None,
        price_usd_per_second: 0.000694,
        price_usd_per_hour: 2.4984,
        free_plan: FreePlan::Unknown,
        free_plan_source: FactSource::Documented,
        source: FactSource::Documented,
        checked_at: CATALOG_CHECKED_AT,
        source_url: MODAL_PRICING_URL,
        compute_capability: Some(ComputeCapability::new(8, 0)),
        compute_capability_source: Some(FactSource::Documented),
        max_gpus_per_container: 8,
        min_cuda: None,
        pins_hardware: true,
        pinned_spelling: None,
        notes: "Ampere A100, sm80, same FP8 caveat as the 40 GB variant. The card to reach for when \
                a BF16 checkpoint needs more than 48 GiB and neither Hopper price is justified.",
    },
    GpuSpec {
        id: "RTX-PRO-6000",
        label: "RTX PRO 6000",
        vram_gib: Some(96.0),
        vram_bytes: Some(103_079_215_104),
        vram_source: Some(VramSource::Datasheet),
        usable_vram_gib: Some(95.0),
        usable_vram_bytes: Some(102_005_473_280),
        price_usd_per_second: 0.000842,
        price_usd_per_hour: 3.0312,
        free_plan: FreePlan::Unknown,
        free_plan_source: FactSource::Documented,
        source: FactSource::Documented,
        checked_at: CATALOG_CHECKED_AT,
        source_url: MODAL_PRICING_URL,
        compute_capability: Some(ComputeCapability::new(12, 0)),
        compute_capability_source: Some(FactSource::Documented),
        max_gpus_per_container: 8,
        min_cuda: None,
        pins_hardware: true,
        pinned_spelling: None,
        notes: "Blackwell sm120. The `gpu=` value is hyphenated (`RTX-PRO-6000`) while the display \
                label keeps the space. Datasheet 96 GB against 95 GiB measured usable (issue #4), \
                which is the figure sizing uses. Native FP8 GEMM is expected on Blackwell but was \
                not confirmed from NVIDIA in the research pass, so its FP8 weight-quantisation \
                verdict is unverified even though its capability is sm120.",
    },
    GpuSpec {
        id: "H100",
        label: "H100",
        vram_gib: Some(80.0),
        vram_bytes: Some(85_899_345_920),
        vram_source: Some(VramSource::Docs),
        usable_vram_gib: None,
        usable_vram_bytes: None,
        price_usd_per_second: 0.001097,
        price_usd_per_hour: 3.9492,
        free_plan: FreePlan::Unavailable,
        free_plan_source: FactSource::Observed,
        source: FactSource::Documented,
        checked_at: CATALOG_CHECKED_AT,
        source_url: MODAL_PRICING_URL,
        compute_capability: Some(ComputeCapability::new(9, 0)),
        compute_capability_source: Some(FactSource::Documented),
        max_gpus_per_container: 8,
        min_cuda: None,
        pins_hardware: false,
        pinned_spelling: Some("H100!"),
        notes: "Hopper sm90, 80 GiB, always SXM on Modal. **Requesting `H100` does not pin the \
                hardware**: Modal may quietly run the request on an H200 at no extra cost, which \
                changes usable VRAM from 80 GiB to 141 GB. Emit `H100!` to pin it, or record the \
                request as *requested* rather than as observed hardware. The `unavailable` \
                free-plan flag on this row is the operator's observation on a Starter workspace, \
                not a documented per-GPU restriction; it is recorded as such in \
                `free_plan_source`, while the price keeps the documented provenance every other \
                row has.",
    },
    GpuSpec {
        id: "H200",
        label: "H200",
        vram_gib: Some(141.0),
        vram_bytes: Some(151_397_597_184),
        vram_source: Some(VramSource::Docs),
        usable_vram_gib: None,
        usable_vram_bytes: None,
        price_usd_per_second: 0.001261,
        price_usd_per_hour: 4.5396,
        free_plan: FreePlan::Unknown,
        free_plan_source: FactSource::Documented,
        source: FactSource::Documented,
        checked_at: CATALOG_CHECKED_AT,
        source_url: MODAL_PRICING_URL,
        compute_capability: Some(ComputeCapability::new(9, 0)),
        compute_capability_source: Some(FactSource::Documented),
        max_gpus_per_container: 8,
        min_cuda: None,
        pins_hardware: true,
        pinned_spelling: None,
        notes: "Hopper sm90, 141 GB HBM3e at 4.8 TB/s per Modal's docs. This is also the card an \
                unpinned `H100` request may land on, so it is what actually decides the KV budget \
                of an H100 deployment that was not pinned.",
    },
    GpuSpec {
        id: "B200",
        label: "B200",
        vram_gib: Some(188.0),
        vram_bytes: Some(201_863_462_912),
        vram_source: Some(VramSource::Unverified),
        usable_vram_gib: None,
        usable_vram_bytes: None,
        price_usd_per_second: 0.001736,
        price_usd_per_hour: 6.2496,
        free_plan: FreePlan::Unknown,
        free_plan_source: FactSource::Documented,
        source: FactSource::Documented,
        checked_at: CATALOG_CHECKED_AT,
        source_url: MODAL_PRICING_URL,
        compute_capability: Some(ComputeCapability::new(10, 0)),
        compute_capability_source: Some(FactSource::Documented),
        max_gpus_per_container: 8,
        min_cuda: None,
        pins_hardware: true,
        pinned_spelling: None,
        notes: "Blackwell sm100. VRAM 188 GB is the widely cited NVIDIA figure and was **not** \
                confirmed from a primary source in the research pass, so it is marked unverified \
                rather than presented as a datasheet fact — and sizing says so when it uses it. \
                `B200+` is the variant that may also run on B300.",
    },
    GpuSpec {
        id: "B300",
        label: "B300",
        vram_gib: None,
        vram_bytes: None,
        vram_source: None,
        usable_vram_gib: None,
        usable_vram_bytes: None,
        price_usd_per_second: 0.001972,
        price_usd_per_hour: 7.0992,
        free_plan: FreePlan::Unknown,
        free_plan_source: FactSource::Documented,
        source: FactSource::Documented,
        checked_at: CATALOG_CHECKED_AT,
        source_url: MODAL_PRICING_URL,
        compute_capability: None,
        compute_capability_source: None,
        max_gpus_per_container: 8,
        min_cuda: Some("13.1"),
        pins_hardware: true,
        pinned_spelling: None,
        notes: "Blackwell Ultra. Requires **CUDA 13.1+**, so a CUDA-12 image will fail: the bundle \
                generator must refuse to pair B300 with one. Modal publishes no per-card VRAM \
                figure and the research pass found none. The guide does bound it indirectly: eight \
                B300s reach the 2 304 GB per-container ceiling, which implies 288 GB each, but that \
                is container arithmetic from a list that also contains 180 GB B200s, not a nameplate \
                figure for this card — so `vram_gib` stays null rather than being fed to sizing, and \
                sizing reports `unavailable` instead of inventing a pool. Priced and requestable; \
                not sizeable.",
    },
];

/// Look a card up by its canonical `gpu=` value. Unknown ids — including `A10G` — return `None`.
pub fn gpu_by_id(id: &str) -> Option<&'static GpuSpec> {
    let wanted = id.trim();
    GPUS.iter().find(|gpu| gpu.id == wanted)
}

/// Modal's Starter/Team limit table, as published on the pricing page.
///
/// This — not a per-GPU flag — is the real free-tier boundary. The Starter plan includes monthly
/// credits and caps GPU concurrency at 10; every card still carries an unconditional price, and
/// Modal documents no card as being unavailable on Starter. Any other reading would be an invention.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanLimit {
    /// The plan id used by [`PlatformSpec::plans`]: `free` or `paid`.
    pub id: &'static str,
    /// The plan's product name.
    pub label: &'static str,
    /// Fixed monthly fee in USD. `0` for Starter.
    pub usd_per_month: u32,
    /// Monthly included credits in USD.
    pub included_credits_usd: u32,
    /// Concurrent GPUs allowed on the plan. `None` when the source does not state one.
    pub gpu_concurrency: Option<u32>,
    /// Seats allowed, `None` when unlimited.
    pub seats: Option<u32>,
    /// Provenance. `documented` for both rows: this is on the pricing page, not inferred.
    pub source: FactSource,
    /// Where the row was read from.
    pub source_url: &'static str,
}

/// The GPU-concurrency ceiling the deployment validator reads, with its provenance.
///
/// Deliberately the same numbers as [`PlatformSpec::plan_limits`]; a test asserts they cannot drift,
/// so the account-validation path and the catalog response never disagree about what Starter allows.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PlanCeiling {
    /// Concurrent GPUs on the free/Starter plan.
    pub free: u32,
    /// Concurrent GPUs on the paid/Team plan.
    pub paid: u32,
    /// Provenance of both numbers.
    pub source: FactSource,
    /// Where both numbers were read from.
    pub source_url: &'static str,
}

/// What the platform actually lets a caller do — the part of the API that is not a GPU.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlatformDeployment {
    /// Whether a public REST deployment API exists. False: there is none, and saying so here stops
    /// the panel from implying the conductor can deploy over HTTP.
    pub programmatic: bool,
    /// Why not, in a sentence an operator can act on.
    pub explain: &'static str,
    /// The CLI invocation the bundle documents.
    pub cli: &'static str,
    /// The SDK entry points the bundle documents.
    pub sdk: &'static str,
    /// How to request more than one card in a container: `gpu="H100:8"`.
    pub multi_gpu_syntax: &'static str,
    /// `modal deploy` options worth exposing, including `--strategy [rolling|recreate]`.
    pub cli_options: &'static str,
    /// The deployment strategies the CLI accepts.
    pub strategies: &'static [&'static str],
    /// Whether `modal deploy` has `--detach`. It does not; a generated bundle must not claim it.
    pub has_detach_flag: bool,
}

/// One rental platform. Only Modal is seeded; the type is a table so a second platform is additive.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlatformSpec {
    /// Stable platform id, matching [`crate::rental::RentalPlatform::as_str`].
    pub id: &'static str,
    /// Display name.
    pub label: &'static str,
    /// Where the platform's documentation lives.
    pub docs_url: &'static str,
    /// The pricing page the numbers below were read from.
    pub source_url: &'static str,
    /// When those numbers were checked.
    pub checked_at: &'static str,
    /// The plan ids a deployment can be billed to.
    pub plans: &'static [PlanRef],
    /// How deployment works, and what it is not.
    pub deployment: PlatformDeployment,
    /// The GPU-concurrency ceiling per plan.
    pub max_concurrent_containers: PlanCeiling,
    /// The full plan table: price, credits, concurrency, seats.
    pub plan_limits: &'static [PlanLimit],
    /// Whether using any GPU requires a payment method on file. True, per Modal's own docs — which
    /// is why no card is marked free.
    pub payment_method_required: bool,
    /// The container ceiling on the Starter plan, independent of GPUs.
    pub max_containers_starter: u32,
    /// The `scaledown_window` bounds Modal accepts, in seconds.
    pub scaledown_window: ScaledownWindow,
    /// The cards this platform offers.
    pub gpus: &'static [GpuSpec],
}

/// A plan id and its label, as the panel's account form offers them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PlanRef {
    /// `free` or `paid`.
    pub id: &'static str,
    /// The display label.
    pub label: &'static str,
}

/// The `scaledown_window` range Modal accepts, and its default.
///
/// The conductor validates `shutdown_cooldown_secs` against this, because an out-of-range value is
/// rejected by `modal deploy` at deploy time — after the operator has already paid for the round
/// trip, and with an error that does not name the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ScaledownWindow {
    /// Minimum accepted value, in seconds.
    pub min_seconds: u32,
    /// The platform default, in seconds.
    pub default_seconds: u32,
    /// Maximum accepted value, in seconds.
    pub max_seconds: u32,
}

/// The `scaledown_window` bounds and default, from Modal's documentation.
pub const fn modal_scaledown_window() -> ScaledownWindow {
    ScaledownWindow {
        min_seconds: 2,
        default_seconds: 60,
        max_seconds: 1200,
    }
}

/// Modal, as this catalog describes it.
const MODAL: PlatformSpec = PlatformSpec {
    id: "modal",
    label: "Modal",
    docs_url: MODAL_GPU_DOCS_URL,
    source_url: MODAL_PRICING_URL,
    checked_at: CATALOG_CHECKED_AT,
    plans: &[
        PlanRef {
            id: "free",
            label: "Free credits",
        },
        PlanRef {
            id: "paid",
            label: "Paid",
        },
    ],
    deployment: PlatformDeployment {
        programmatic: false,
        explain: "Modal has no public REST endpoint that deploys or updates an app; deployment goes \
                  through the `modal` CLI or `modal.App.deploy()` from a Python process. The \
                  conductor therefore generates the app and the operator deploys it — it never \
                  claims to have deployed anything itself.",
        cli: "modal deploy modal_app.py",
        sdk: "modal.App / app.deploy()",
        multi_gpu_syntax: "gpu=\"H100:8\"",
        cli_options: "--name, -e/--env, --stream-logs, --tag, -m, --timestamps, \
                      --strategy [rolling|recreate]",
        strategies: &["rolling", "recreate"],
        has_detach_flag: false,
    },
    max_concurrent_containers: PlanCeiling {
        free: 10,
        paid: 50,
        source: FactSource::Documented,
        source_url: MODAL_PRICING_URL,
    },
    plan_limits: &[
        PlanLimit {
            id: "free",
            label: "Starter",
            usd_per_month: 0,
            included_credits_usd: 30,
            gpu_concurrency: Some(10),
            seats: Some(3),
            source: FactSource::Documented,
            source_url: MODAL_PRICING_URL,
        },
        PlanLimit {
            id: "paid",
            label: "Team",
            usd_per_month: 250,
            included_credits_usd: 100,
            gpu_concurrency: Some(50),
            seats: None,
            source: FactSource::Documented,
            source_url: MODAL_PRICING_URL,
        },
    ],
    payment_method_required: true,
    max_containers_starter: 100,
    scaledown_window: modal_scaledown_window(),
    gpus: GPUS,
};

/// Every platform in the catalog. Ordered as the API returns them.
///
/// Sliced from a `const` copy rather than from the `static` above, because a `static` slice
/// initialiser may not read another `static`.
static PLATFORMS: [PlatformSpec; 1] = [MODAL];

/// All platforms.
pub fn platforms() -> &'static [PlatformSpec] {
    &PLATFORMS
}

/// Look a platform up by id.
pub fn platform_by_id(id: &str) -> Option<&'static PlatformSpec> {
    let wanted = id.trim();
    PLATFORMS.iter().find(|platform| platform.id == wanted)
}

/// Modal, the only platform seeded today.
pub fn modal() -> &'static PlatformSpec {
    &PLATFORMS[0]
}

/// How far a quantisation method is supported on a given card.
///
/// Three states rather than a boolean, because "the docs say no" and "the docs do not say" lead to
/// different operator decisions: the first is a hard stop, the second is a decision the operator
/// makes with the vendor's own log as the arbiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuantSupport {
    /// A documented capability on this card.
    Supported,
    /// A documented impossibility on this card. Renting it will not boot the model.
    Unsupported,
    /// The sources do not settle it. Surfaced as a warning, never as a silent success.
    Unverified,
}

impl QuantSupport {
    /// The wire spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            QuantSupport::Supported => "supported",
            QuantSupport::Unsupported => "unsupported",
            QuantSupport::Unverified => "unverified",
        }
    }

    /// Whether to let the deployment proceed: everything except a documented impossibility.
    pub fn runnable(&self) -> bool {
        !matches!(self, QuantSupport::Unsupported)
    }
}

/// One `[card_id, verdict]` cell of the quantisation matrix. Serialises as a two-element array.
pub type QuantCell = (&'static str, QuantSupport);

/// A quantisation method as `--quantization` accepts it, with the card verdicts that are known.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuantMethod {
    /// The `--quantization` value.
    pub id: &'static str,
    /// The display label.
    pub label: &'static str,
    /// Weight bytes per parameter this method implies, when it is fixed. `None` when it depends on
    /// the checkpoint (GGUF, and therefore anything that must be read from the file itself).
    pub bytes_per_param: Option<f64>,
    /// The compute capability vLLM's matrix gives this method, `None` when it gives none.
    pub min_compute_capability: Option<ComputeCapability>,
    /// Provenance of the matrix row.
    pub source: FactSource,
    /// Where the row was read from.
    pub source_url: &'static str,
    /// The card verdicts that depart from a plain `capability >= min` comparison: documented
    /// impossibilities and cells the sources leave open. An empty slice means the comparison is all
    /// there is; `None` means no per-card research was done and every card is unverified.
    pub cells: Option<&'static [QuantCell]>,
    /// Why this row looks the way it does, including any disagreement between sources.
    pub notes: &'static str,
}

impl QuantMethod {
    /// The verdict for one card, without the reason text.
    fn support_on(&self, gpu: &GpuSpec) -> QuantSupport {
        if let Some(cells) = self.cells {
            if let Some((_, support)) = cells.iter().find(|(id, _)| *id == gpu.id) {
                return *support;
            }
        } else {
            // No per-card research for this method: nothing about it is verified for any card.
            return QuantSupport::Unverified;
        }
        match (gpu.compute_capability, self.min_compute_capability) {
            (Some(card), Some(required)) => {
                if card >= required {
                    QuantSupport::Supported
                } else {
                    QuantSupport::Unsupported
                }
            }
            // Unknown card capability (B300 today): an unknown cannot clear a known floor.
            _ => QuantSupport::Unverified,
        }
    }
}

/// What a card can do with a quantisation method, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuantVerdict {
    /// The method's `--quantization` id.
    pub method: &'static str,
    /// The card's canonical id.
    pub gpu: &'static str,
    /// The verdict.
    pub support: QuantSupport,
    /// How well-sourced the verdict is.
    pub confidence: FactSource,
    /// The sentence the panel shows: the numeric rule where there is one, and the reason where the
    /// sources disagree or a documented exclusion applies.
    pub reason: String,
}

impl QuantVerdict {
    /// Whether the deployment may proceed: everything except a documented impossibility.
    pub fn runnable(&self) -> bool {
        self.support.runnable()
    }
}

/// One `--kv-cache-dtype` value.
///
/// This is the **storage** axis. FP8 KV storage needs CUDA 11.8+ and works on every card here,
/// including the T4, which cannot execute an FP8 matmul at all. Keeping the two axes apart is what
/// stops the catalog from refusing a legal T4 deployment or permitting an illegal FP8 GEMM.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KvCacheDtype {
    /// The `--kv-cache-dtype` value.
    pub id: &'static str,
    /// The display label.
    pub label: &'static str,
    /// Bytes per cached element. `None` for `auto`, which means "the model's dtype".
    pub bytes_per_element: Option<u32>,
    /// Whether the value defers to the checkpoint's dtype instead of naming a width.
    pub follows_model_dtype: bool,
    /// The minimum CUDA toolkit for the storage path, when the source gives one.
    pub min_cuda: Option<&'static str>,
    /// Provenance.
    pub source: FactSource,
    /// Where the row was read from.
    pub source_url: &'static str,
    /// What it means in practice.
    pub notes: &'static str,
}

/// The `--kv-cache-dtype` values the catalog accepts, with the sizes sizing uses.
pub static KV_CACHE_DTYPES: &[KvCacheDtype] = &[
    KvCacheDtype {
        id: "auto",
        label: "Auto (checkpoint dtype)",
        bytes_per_element: None,
        follows_model_dtype: true,
        min_cuda: None,
        source: FactSource::Documented,
        source_url: VLLM_KV_CACHE_URL,
        notes: "vLLM's default: the cache uses the model's own dtype, so a BF16 checkpoint caches \
                in 2 bytes per element. Sizing reads the checkpoint's `torch_dtype` for this case \
                and falls back to 2 with an assumption line when the config does not state one.",
    },
    KvCacheDtype {
        id: "bfloat16",
        label: "BF16",
        bytes_per_element: Some(2),
        follows_model_dtype: false,
        min_cuda: None,
        source: FactSource::Documented,
        source_url: VLLM_KV_CACHE_URL,
        notes: "Two bytes per element, no precision loss against a BF16 checkpoint. The expensive \
                choice, and the one to compare an fp8 deployment against.",
    },
    KvCacheDtype {
        id: "float16",
        label: "FP16",
        bytes_per_element: Some(2),
        follows_model_dtype: false,
        min_cuda: None,
        source: FactSource::Documented,
        source_url: VLLM_KV_CACHE_URL,
        notes: "Two bytes per element. Legal on every card here; rarely worth choosing over BF16.",
    },
    KvCacheDtype {
        id: "fp8",
        label: "FP8",
        bytes_per_element: Some(1),
        follows_model_dtype: false,
        min_cuda: Some("11.8"),
        source: FactSource::Documented,
        source_url: VLLM_KV_CACHE_URL,
        notes: "One byte per element, halving the cache against BF16 — the single biggest lever on \
                how many tokens fit. Storage-only: it needs CUDA 11.8+ and **not** sm89, so it is \
                legal on the T4 and the A100 even though FP8 GEMM is not. This is the dtype the \
                operator measured both reference pools with.",
    },
    KvCacheDtype {
        id: "fp8_e4m3",
        label: "FP8 E4M3",
        bytes_per_element: Some(1),
        follows_model_dtype: false,
        min_cuda: Some("11.8"),
        source: FactSource::Documented,
        source_url: VLLM_KV_CACHE_URL,
        notes: "The explicit spelling of the 4-bit-exponent FP8 format: one byte per element, wider \
                range than E5M2 at the cost of mantissa precision. Same hardware rules as `fp8`.",
    },
    KvCacheDtype {
        id: "fp8_e5m2",
        label: "FP8 E5M2",
        bytes_per_element: Some(1),
        follows_model_dtype: false,
        min_cuda: Some("11.8"),
        source: FactSource::Documented,
        source_url: VLLM_KV_CACHE_URL,
        notes: "The 5-bit-exponent variant: one byte per element, more range and less precision \
                than E4M3. Same hardware rules as `fp8`.",
    },
];

/// The byte width of a KV-cache dtype. `None` when the id is unknown; `auto` falls back to 2.
///
/// `auto` is a *default*, not a fact about the cache, so it is resolved by the caller that knows the
/// checkpoint's dtype and by the assumption line that records the fallback.
pub fn kv_cache_dtype_by_id(id: &str) -> Option<&'static KvCacheDtype> {
    let wanted = id.trim();
    KV_CACHE_DTYPES.iter().find(|dtype| dtype.id == wanted)
}

/// vLLM's accepted quantisation methods that this catalog does **not** carry a verified row for.
///
/// They are listed so the API can explain the gap instead of reporting these ids as unknown: vLLM
/// accepts them, and this research pass did not establish their per-card support.
pub const UNRESOLVED_QUANT_IDS: &[&str] = &[
    "modelopt",
    "modelopt_fp4",
    "modelopt_mxfp8",
    "modelopt_mixed",
    "torchao",
    "quark",
    "humming",
    "deepseek_v4_fp8",
    "online",
];

/// The quantisation matrix. Rows come from vLLM's hardware matrix; unverified cells stay unverified.
pub static QUANT_METHODS: &[QuantMethod] = &[
    QuantMethod {
        id: "none",
        label: "Unquantised (BF16/FP16 weights)",
        bytes_per_param: Some(2.0),
        min_compute_capability: Some(ComputeCapability::new(7, 0)),
        source: FactSource::Documented,
        source_url: VLLM_QUANT_MATRIX_URL,
        cells: Some(&[]),
        notes: "The checkpoint's own dtype with no quantisation. Every card in the catalog is \
                documented as BF16-capable, including the T4, so the minimum here is Volta and \
                nothing in the table overrides the comparison.",
    },
    QuantMethod {
        id: "fp8",
        label: "FP8 W8A8 (native GEMM)",
        bytes_per_param: Some(1.0),
        min_compute_capability: Some(NATIVE_FP8_GEMM_MIN),
        source: FactSource::Documented,
        source_url: VLLM_QUANT_MATRIX_URL,
        cells: Some(&[
            ("T4", QuantSupport::Unsupported),
            ("A10", QuantSupport::Unsupported),
            ("A100-40GB", QuantSupport::Unsupported),
            ("A100-80GB", QuantSupport::Unsupported),
            ("RTX-PRO-6000", QuantSupport::Unverified),
        ]),
        notes: "Native FP8 GEMM needs compute capability 8.9 or better, which is why the T4, the \
                A10 and both A100 variants are documented impossibilities while the L4, L40S, H100, \
                H200 and B200 clear the floor. The RTX PRO 6000 is sm120 and therefore expected to \
                work, but NVIDIA support was not confirmed directly, so it is unverified rather \
                than assumed. The online FP8 shorthands (`fp8_per_tensor`, `fp8_per_block`, \
                `fp8_per_channel`) follow this row: same compute requirement.",
    },
    QuantMethod {
        id: "fp8_weight_only",
        label: "FP8 weights, higher-precision activations (Marlin)",
        bytes_per_param: Some(1.0),
        min_compute_capability: Some(ComputeCapability::new(7, 5)),
        source: FactSource::Documented,
        source_url: VLLM_QUANT_MATRIX_URL,
        cells: Some(&[
            ("T4", QuantSupport::Unverified),
            ("B200", QuantSupport::Unverified),
            ("RTX-PRO-6000", QuantSupport::Unverified),
        ]),
        notes: "The path that lets Ampere cards serve an FP8 checkpoint: weights stay 8-bit and \
                the matmuls run through Marlin kernels. It is the reason `fp8` weights on an A100 \
                is not a dead end even though native FP8 GEMM is. T4 is left unverified because the \
                weight-only row and the Marlin row disagree about Turing.",
    },
    QuantMethod {
        id: "awq",
        label: "AWQ 4-bit",
        bytes_per_param: Some(0.5),
        min_compute_capability: Some(ComputeCapability::new(7, 5)),
        source: FactSource::Documented,
        source_url: VLLM_QUANT_MATRIX_URL,
        cells: Some(&[
            ("T4", QuantSupport::Unverified),
            ("B200", QuantSupport::Unverified),
            ("RTX-PRO-6000", QuantSupport::Unverified),
        ]),
        notes: "**Sources disagree about the T4.** vLLM's hardware matrix row gives AWQ a minimum \
                of 7.5 and prints Turing as supported with a `?` for the T4; the same document's \
                combination table says AWQ is impossible on the T4 as pre-Turing hardware. This \
                catalog does not pick a winner: the T4 stays unverified so the operator is told to \
                check the vendor's own startup log. B200 and RTX PRO 6000 are `?` in the matrix \
                too, so they stay unverified rather than being upgraded by their capability.",
    },
    QuantMethod {
        id: "gptq",
        label: "GPTQ 4-bit",
        bytes_per_param: Some(0.5),
        min_compute_capability: Some(ComputeCapability::new(7, 0)),
        source: FactSource::Documented,
        source_url: VLLM_QUANT_MATRIX_URL,
        cells: Some(&[
            ("B200", QuantSupport::Unverified),
            ("RTX-PRO-6000", QuantSupport::Unverified),
        ]),
        notes: "The best-supported 4-bit path here: documented on every NVIDIA card including the \
                T4. The two Blackwell cards are `?` in the matrix, so they are unverified rather \
                than assumed.",
    },
    QuantMethod {
        id: "awq_marlin",
        label: "AWQ 4-bit via Marlin kernels",
        bytes_per_param: Some(0.5),
        min_compute_capability: Some(ComputeCapability::new(7, 5)),
        source: FactSource::Documented,
        source_url: VLLM_QUANT_MATRIX_URL,
        cells: Some(&[
            ("T4", QuantSupport::Unsupported),
            ("B200", QuantSupport::Unverified),
            ("RTX-PRO-6000", QuantSupport::Unverified),
        ]),
        notes: "Faster than plain AWQ on the cards that have Marlin kernels, and documented as \
                impossible on the T4 (no Marlin on pre-Turing). Where AWQ itself is only \
                *unverified* on the T4, this alias is a documented no, which is exactly the kind of \
                distinction a boolean would lose.",
    },
    QuantMethod {
        id: "gptq_marlin",
        label: "GPTQ 4-bit via Marlin kernels",
        bytes_per_param: Some(0.5),
        min_compute_capability: Some(ComputeCapability::new(7, 5)),
        source: FactSource::Documented,
        source_url: VLLM_QUANT_MATRIX_URL,
        cells: Some(&[
            ("T4", QuantSupport::Unsupported),
            ("B200", QuantSupport::Unverified),
            ("RTX-PRO-6000", QuantSupport::Unverified),
        ]),
        notes: "Same Marlin row as `awq_marlin`: documented impossible on the T4, `?` on the two \
                Blackwell cards.",
    },
    QuantMethod {
        id: "compressed-tensors",
        label: "compressed-tensors checkpoints",
        bytes_per_param: None,
        min_compute_capability: None,
        source: FactSource::Unverified,
        source_url: VLLM_QUANT_MATRIX_URL,
        cells: None,
        notes: "vLLM accepts this method, but the research pass established no per-card support \
                figures for it — and its byte width depends on the checkpoint's own recipe. Every \
                card therefore comes back unverified: not a refusal, a statement that nothing here \
                is known.",
    },
    QuantMethod {
        id: "int8_w8a8",
        label: "INT8 W8A8 (llm-compressor)",
        bytes_per_param: Some(1.0),
        min_compute_capability: Some(ComputeCapability::new(7, 5)),
        source: FactSource::Documented,
        source_url: VLLM_QUANT_MATRIX_URL,
        cells: Some(&[
            ("T4", QuantSupport::Unverified),
            ("B200", QuantSupport::Unsupported),
            ("RTX-PRO-6000", QuantSupport::Unsupported),
        ]),
        notes: "Turing and up in vLLM's matrix, though the T4 cell is `?` and stays unverified. \
                Both Blackwell cards are a documented no, which is worth knowing before treating \
                INT8 as the safe middle ground.",
    },
    QuantMethod {
        id: "int8_w4a8",
        label: "INT8 W4A8",
        bytes_per_param: Some(0.5),
        min_compute_capability: None,
        source: FactSource::Documented,
        source_url: VLLM_QUANT_MATRIX_URL,
        cells: Some(&[
            ("T4", QuantSupport::Unsupported),
            ("L4", QuantSupport::Unsupported),
            ("A10", QuantSupport::Unsupported),
            ("L40S", QuantSupport::Unsupported),
            ("A100-40GB", QuantSupport::Unsupported),
            ("A100-80GB", QuantSupport::Unsupported),
            ("RTX-PRO-6000", QuantSupport::Unsupported),
            ("H100", QuantSupport::Unsupported),
            ("H200", QuantSupport::Unsupported),
            ("B200", QuantSupport::Unsupported),
            ("B300", QuantSupport::Unsupported),
        ]),
        notes: "Every card is listed explicitly because the matrix marks this method `?` in the \
                GPU columns while its own footnote says x86-CPU-only. On Modal there is no x86 CPU \
                option, so for this catalog it is a documented no on all eleven cards — the one \
                row where an explicit cell list beats a capability comparison.",
    },
    QuantMethod {
        id: "bitsandbytes",
        label: "bitsandbytes (NF4/INT8)",
        bytes_per_param: Some(0.5),
        min_compute_capability: Some(ComputeCapability::new(7, 0)),
        source: FactSource::Documented,
        source_url: VLLM_QUANT_MATRIX_URL,
        cells: Some(&[
            ("B200", QuantSupport::Unverified),
            ("RTX-PRO-6000", QuantSupport::Unverified),
        ]),
        notes: "Documented as working on every NVIDIA card here, Volta and up. Two caveats the \
                operator should hear: the 0.5 bytes per parameter is the 4-bit payload and not the \
                whole file, since scales and zeros are stored beside it; and whether this is \
                accepted as an explicit `--quantization` value or only auto-detected from the \
                checkpoint's `quantization_config` was not established, so the bundle prefers \
                auto-detection.",
    },
    QuantMethod {
        id: "gguf",
        label: "GGUF (llama.cpp format)",
        bytes_per_param: None,
        min_compute_capability: Some(ComputeCapability::new(7, 0)),
        source: FactSource::Documented,
        source_url: VLLM_QUANT_MATRIX_URL,
        cells: Some(&[("T4", QuantSupport::Unverified)]),
        notes: "Treated separately on purpose: GGUF is a llama.cpp file format served through \
                vLLM's GGUF path, not a native CUDA quantisation, so it does not behave like the \
                others and its byte width depends on the file's own quantisation level (`Q4_K_M` \
                and friends). The T4 cell is `?` in the matrix and stays unverified.",
    },
    QuantMethod {
        id: "nvfp4",
        label: "NVFP4 / MXFP4",
        bytes_per_param: Some(0.5),
        min_compute_capability: Some(ComputeCapability::new(10, 0)),
        source: FactSource::Documented,
        source_url: VLLM_QUANT_MATRIX_URL,
        cells: Some(&[
            ("B200", QuantSupport::Unverified),
            ("RTX-PRO-6000", QuantSupport::Unverified),
        ]),
        notes: "Blackwell only (sm100+), so everything older fails the capability comparison and \
                is reported as unsupported. Both capable cards are `?` in the matrix and stay \
                unverified. The online shorthands `nvfp4_per_token` and `mxfp4` follow this row.",
    },
];

/// Every quantisation method the catalog knows, with its card verdicts.
pub fn quant_methods() -> &'static [QuantMethod] {
    QUANT_METHODS
}

/// Look a quantisation method up by its `--quantization` value.
pub fn quant_method_by_id(id: &str) -> Option<&'static QuantMethod> {
    let wanted = id.trim();
    QUANT_METHODS.iter().find(|method| method.id == wanted)
}

/// Resolve whether a card may run a quantisation method, or `None` when the method is unknown.
///
/// `None` is reserved for ids that are not in the catalog at all. An id vLLM accepts but this
/// research pass did not cover comes back as an `unverified` verdict with a reason, because "not
/// researched" and "not a real method" are different answers and the panel shows them differently.
pub fn resolve_quant_for(gpu: &GpuSpec, quant_id: &str) -> Option<QuantVerdict> {
    let wanted = quant_id.trim();
    if let Some(method) = quant_method_by_id(wanted) {
        let support = method.support_on(gpu);
        return Some(QuantVerdict {
            method: method.id,
            gpu: gpu.id,
            support,
            confidence: if support == QuantSupport::Unverified {
                FactSource::Unverified
            } else {
                method.source
            },
            reason: quant_reason(method, gpu, support),
        });
    }
    if UNRESOLVED_QUANT_IDS.contains(&wanted) {
        return Some(QuantVerdict {
            method: "unknown-to-catalog",
            gpu: gpu.id,
            support: QuantSupport::Unverified,
            confidence: FactSource::Unverified,
            reason: format!(
                "vLLM accepts {wanted:?}, but this catalog has no verified per-card support \
                 figures for it; check the vendor's startup log before relying on it"
            ),
        });
    }
    None
}

/// The sentence shown with a verdict: the numeric rule when the comparison decided it, the sources'
/// own wording when a cell or a gap did.
fn quant_reason(method: &QuantMethod, gpu: &GpuSpec, support: QuantSupport) -> String {
    let card = match gpu.compute_capability {
        Some(cc) => format!("compute capability {}", cc.as_string()),
        None => "an unknown compute capability".to_string(),
    };
    match support {
        QuantSupport::Supported => match method.min_compute_capability {
            Some(required) => format!(
                "documented: {} needs compute capability {} or better and {} has {}",
                method.id,
                required.as_string(),
                gpu.label,
                card
            ),
            None => format!(
                "documented as available on {} in vLLM's hardware matrix",
                gpu.label
            ),
        },
        QuantSupport::Unsupported => match method.min_compute_capability {
            Some(required) => format!(
                "{} needs compute capability {} or better; {} has {}. {}",
                method.id,
                required.as_string(),
                gpu.label,
                card,
                method.notes
            ),
            None => format!("documented as impossible on {}: {}", gpu.label, method.notes),
        },
        QuantSupport::Unverified => {
            if method.cells.is_none() {
                format!(
                    "no verified per-card support figure exists for {} on {}",
                    method.id, gpu.label
                )
            } else if gpu.compute_capability.is_none() {
                format!(
                    "{} has no documented compute capability in this catalog, so its {} support \
                     cannot be resolved",
                    gpu.label, method.id
                )
            } else {
                format!(
                    "the sources do not settle {} on {}; check the vendor's startup log",
                    method.id, gpu.label
                )
            }
        }
    }
}

/// Whether a card may store its KV cache in a given dtype.
///
/// Separate from [`resolve_quant_for`] because it is a different question with a different answer
/// on the same card: FP8 *storage* is fine on a T4 while FP8 *compute* is not.
pub fn resolve_kv_dtype_for(gpu: &GpuSpec, dtype_id: &str) -> Option<QuantVerdict> {
    let dtype = kv_cache_dtype_by_id(dtype_id)?;
    let support = match gpu.compute_capability {
        // The storage path is a CUDA-toolkit requirement, not a capability requirement, so a card
        // with a known capability can take every dtype here.
        Some(_) => QuantSupport::Supported,
        None => QuantSupport::Unverified,
    };
    let reason = match support {
        QuantSupport::Supported => match dtype.min_cuda {
            Some(cuda) => format!(
                "{} is a storage-only dtype: it needs CUDA {} or later and no particular compute \
                 capability, so {} can cache in it",
                dtype.id, cuda, gpu.label
            ),
            None => format!("documented as available on {} (no special hardware needed)", gpu.label),
        },
        _ => format!(
            "{} has no documented compute capability in this catalog, so this dtype is unverified \
             on it",
            gpu.label
        ),
    };
    Some(QuantVerdict {
        method: dtype.id,
        gpu: gpu.id,
        support,
        confidence: FactSource::Documented,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The eleven cards, in price order, exactly as documented.
    const DOCUMENTED_SECOND_PRICES: &[(&str, f64)] = &[
        ("T4", 0.000164),
        ("L4", 0.000222),
        ("A10", 0.000306),
        ("L40S", 0.000542),
        ("A100-40GB", 0.000583),
        ("A100-80GB", 0.000694),
        ("RTX-PRO-6000", 0.000842),
        ("H100", 0.001097),
        ("H200", 0.001261),
        ("B200", 0.001736),
        ("B300", 0.001972),
    ];

    #[test]
    fn catalog_holds_exactly_the_eleven_documented_cards() {
        assert_eq!(GPUS.len(), 11);
        assert_eq!(GPUS.len(), DOCUMENTED_SECOND_PRICES.len());
        for (gpu, (id, price)) in GPUS.iter().zip(DOCUMENTED_SECOND_PRICES) {
            assert_eq!(gpu.id, *id, "catalog order is price order");
            assert_eq!(
                gpu.price_usd_per_second, *price,
                "{id} per-second price must be the verified one"
            );
        }
    }

    #[test]
    fn hourly_price_is_the_per_second_price_times_3600() {
        for gpu in GPUS {
            let computed = gpu.price_usd_per_second * 3600.0;
            assert!(
                (gpu.price_usd_per_hour - computed).abs() < 1e-9,
                "{}: stated {:.6}/h but {:.6}/s * 3600 = {:.6}/h",
                gpu.id,
                gpu.price_usd_per_hour,
                gpu.price_usd_per_second,
                computed
            );
        }
    }

    #[test]
    fn documented_price_anchors_match_the_research() {
        let anchors = [
            ("RTX-PRO-6000", 3.0312),
            ("L40S", 1.9512),
            ("H100", 3.9492),
            ("H200", 4.5396),
            ("B200", 6.2496),
        ];
        for (id, hourly) in anchors {
            let gpu = gpu_by_id(id).expect("anchor card is in the catalog");
            assert!((gpu.price_usd_per_hour - hourly).abs() < 1e-9, "{id} hourly");
        }
    }

    #[test]
    fn every_card_has_a_documented_price_provenance() {
        for gpu in GPUS {
            assert_eq!(gpu.source, FactSource::Documented, "{} price source", gpu.id);
            assert_eq!(gpu.source_url, MODAL_PRICING_URL, "{} price url", gpu.id);
            assert_eq!(gpu.checked_at, CATALOG_CHECKED_AT, "{} checked_at", gpu.id);
        }
    }

    #[test]
    fn exactly_one_card_is_observed_unavailable_on_the_free_plan() {
        let unavailable: Vec<&str> = GPUS
            .iter()
            .filter(|gpu| gpu.free_plan == FreePlan::Unavailable)
            .map(|gpu| gpu.id)
            .collect();
        assert_eq!(
            unavailable,
            vec!["H100"],
            "Modal documents no per-GPU gating, so only the operator's own observation may be \
             anything other than unknown"
        );
        for gpu in GPUS {
            // The price is documented on every row, including the one whose free-plan flag was
            // observed: the two provenances never share a field.
            assert_eq!(gpu.source, FactSource::Documented, "{} price source", gpu.id);
            if gpu.id == "H100" {
                assert_eq!(
                    gpu.free_plan_source,
                    FactSource::Observed,
                    "the only free-plan observation in the catalog must say so"
                );
            } else {
                assert_eq!(
                    gpu.free_plan,
                    FreePlan::Unknown,
                    "{} must not be given an invented free-plan flag",
                    gpu.id
                );
                assert_eq!(
                    gpu.free_plan_source,
                    FactSource::Documented,
                    "{}: absent per-GPU gating is a documented state, not an observed one",
                    gpu.id
                );
            }
        }
    }

    #[test]
    fn canonical_ids_are_the_values_modal_accepts() {
        assert!(gpu_by_id("RTX-PRO-6000").is_some(), "hyphenated gpu= value");
        assert_eq!(
            gpu_by_id("RTX-PRO-6000").expect("present").label,
            "RTX PRO 6000",
            "the label keeps the space, the id does not"
        );
        assert!(gpu_by_id("A10").is_some());
        assert!(
            gpu_by_id("A10G").is_none(),
            "A10G is not a valid gpu= value, so it must not resolve"
        );
        assert!(gpu_by_id("A100").is_none(), "A100 alone is ambiguous: use a variant id");
        for id in ["A100-40GB", "A100-80GB", "H100", "H200", "B200", "B300", "T4", "L4", "L40S"] {
            assert!(gpu_by_id(id).is_some(), "{id} must resolve");
        }
    }

    #[test]
    fn lookup_trims_whitespace_but_stays_case_sensitive() {
        assert!(gpu_by_id("  H100  ").is_some());
        assert!(gpu_by_id("h100").is_none(), "gpu values are case-sensitive");
    }

    #[test]
    fn multi_gpu_limit_is_four_on_the_a10_and_eight_everywhere_else() {
        for gpu in GPUS {
            let expected = if gpu.id == "A10" { 4 } else { 8 };
            assert_eq!(
                gpu.max_gpus_per_container, expected,
                "{} per-container GPU ceiling",
                gpu.id
            );
        }
    }

    #[test]
    fn b300_carries_a_cuda_floor_and_an_honest_vram_gap() {
        let b300 = gpu_by_id("B300").expect("present");
        assert_eq!(b300.min_cuda, Some("13.1"));
        assert!(b300.vram_gib.is_none(), "no source states B300 VRAM");
        assert!(b300.vram_bytes.is_none());
        assert!(b300.vram_source.is_none());
        assert!(b300.vram_for_sizing().is_none());
        assert!(b300.compute_capability.is_none());
    }

    #[test]
    fn unpinned_jobs_say_so() {
        let h100 = gpu_by_id("H100").expect("present");
        assert!(!h100.pins_hardware);
        assert_eq!(h100.pinned_spelling, Some("H100!"));
        assert!(
            h100.notes.contains("H200"),
            "the H100 row must warn that an unpinned request can land on an H200"
        );
        for gpu in GPUS.iter().filter(|gpu| gpu.id != "H100") {
            assert!(gpu.pins_hardware, "{} is pinned by its own id", gpu.id);
        }
    }

    #[test]
    fn vram_figures_carry_their_own_provenance_or_none_at_all() {
        for gpu in GPUS {
            match (gpu.vram_gib, gpu.vram_bytes, gpu.vram_source) {
                (Some(gib), Some(bytes), Some(_)) => {
                    let expected = (gib * 1024.0 * 1024.0 * 1024.0) as u64;
                    assert_eq!(bytes, expected, "{} vram_bytes must follow vram_gib", gpu.id);
                }
                (None, None, None) => {}
                other => panic!("{} has an inconsistent VRAM triple: {other:?}", gpu.id),
            }
        }
    }

    #[test]
    fn measured_usable_vram_is_recorded_where_the_operator_measured_it() {
        let rtx = gpu_by_id("RTX-PRO-6000").expect("present");
        assert_eq!(rtx.usable_vram_gib, Some(95.0));
        assert_eq!(rtx.usable_vram_bytes, Some(102_005_473_280));
        assert_eq!(
            rtx.vram_for_sizing(),
            rtx.usable_vram_bytes,
            "sizing must prefer usable capacity over nameplate capacity"
        );
        let l40s = gpu_by_id("L40S").expect("present");
        assert_eq!(l40s.usable_vram_gib, Some(46.0));
        assert_eq!(l40s.vram_source, Some(VramSource::Docs));
        // Nameplate figures stay visible so the delta against usable capacity is inspectable.
        assert_eq!(rtx.vram_gib, Some(96.0));
        assert_eq!(l40s.vram_gib, Some(48.0));
    }

    #[test]
    fn docs_observed_and_unverified_vram_sources_all_appear() {
        let sources: Vec<VramSource> = GPUS.iter().filter_map(|gpu| gpu.vram_source).collect();
        assert!(sources.contains(&VramSource::Docs));
        assert!(sources.contains(&VramSource::Datasheet));
        assert!(
            sources.contains(&VramSource::Unverified),
            "B200's widely-cited VRAM figure must not be dressed up as a datasheet fact"
        );
        assert_eq!(
            gpu_by_id("B200").expect("present").vram_source,
            Some(VramSource::Unverified)
        );
    }

    #[test]
    fn compute_capabilities_match_the_verified_table() {
        let expected = [
            ("T4", 7, 5),
            ("L4", 8, 9),
            ("A10", 8, 6),
            ("L40S", 8, 9),
            ("A100-40GB", 8, 0),
            ("A100-80GB", 8, 0),
            ("RTX-PRO-6000", 12, 0),
            ("H100", 9, 0),
            ("H200", 9, 0),
            ("B200", 10, 0),
        ];
        for (id, major, minor) in expected {
            let gpu = gpu_by_id(id).expect("present");
            assert_eq!(
                gpu.compute_capability,
                Some(ComputeCapability::new(major, minor)),
                "{id}"
            );
            assert_eq!(gpu.compute_capability_source, Some(FactSource::Documented));
        }
    }

    #[test]
    fn native_fp8_gemm_requires_capability_eight_nine() {
        assert_eq!(NATIVE_FP8_GEMM_MIN, ComputeCapability::new(8, 9));
        assert!(gpu_by_id("L4").expect("present").supports_native_fp8_gemm());
        assert!(gpu_by_id("L40S").expect("present").supports_native_fp8_gemm());
        assert!(gpu_by_id("RTX-PRO-6000").expect("present").supports_native_fp8_gemm());
        assert!(!gpu_by_id("T4").expect("present").supports_native_fp8_gemm());
        assert!(!gpu_by_id("A10").expect("present").supports_native_fp8_gemm());
        assert!(!gpu_by_id("A100-80GB").expect("present").supports_native_fp8_gemm());
        // Unknown capability is not a pass: the answer is "cannot say", which is false here.
        assert!(!gpu_by_id("B300").expect("present").supports_native_fp8_gemm());
    }

    #[test]
    fn fp8_compute_is_impossible_exactly_where_the_matrix_says_so() {
        let verdict = |id: &str| {
            resolve_quant_for(gpu_by_id(id).expect("card exists"), "fp8").expect("method exists")
        };
        assert_eq!(verdict("L40S").support, QuantSupport::Supported);
        assert_eq!(verdict("H100").support, QuantSupport::Supported);
        assert_eq!(verdict("T4").support, QuantSupport::Unsupported);
        assert_eq!(verdict("A10").support, QuantSupport::Unsupported);
        assert_eq!(verdict("A100-40GB").support, QuantSupport::Unsupported);
        assert_eq!(verdict("A100-80GB").support, QuantSupport::Unsupported);
        assert_eq!(
            verdict("RTX-PRO-6000").support,
            QuantSupport::Unverified,
            "sm120 is expected to work but was not confirmed, so it must not read as supported"
        );
        assert!(verdict("T4").reason.contains("8.9"));
        assert!(!verdict("T4").runnable());
        assert!(verdict("L40S").runnable());
        assert!(
            verdict("RTX-PRO-6000").runnable(),
            "unverified is not a refusal: the operator decides"
        );
        assert_eq!(verdict("RTX-PRO-6000").confidence, FactSource::Unverified);
    }

    #[test]
    fn fp8_kv_storage_is_legal_where_fp8_compute_is_not() {
        let t4 = gpu_by_id("T4").expect("present");
        assert!(!t4.supports_native_fp8_gemm());
        let storage = resolve_kv_dtype_for(t4, "fp8").expect("dtype exists");
        assert_eq!(storage.support, QuantSupport::Supported);
        assert!(storage.reason.contains("storage-only"));
        assert_eq!(
            resolve_quant_for(t4, "fp8").expect("exists").support,
            QuantSupport::Unsupported
        );
    }

    #[test]
    fn kv_dtype_widths_are_what_sizing_uses() {
        let width = |id: &str| kv_cache_dtype_by_id(id).and_then(|d| d.bytes_per_element);
        assert_eq!(width("fp8"), Some(1));
        assert_eq!(width("fp8_e4m3"), Some(1));
        assert_eq!(width("fp8_e5m2"), Some(1));
        assert_eq!(width("bfloat16"), Some(2));
        assert_eq!(width("float16"), Some(2));
        assert_eq!(width("auto"), None);
        assert!(kv_cache_dtype_by_id("auto").expect("present").follows_model_dtype);
        assert!(kv_cache_dtype_by_id("int4").is_none(), "not a vLLM dtype");
        for dtype in KV_CACHE_DTYPES {
            assert_eq!(dtype.source_url, VLLM_KV_CACHE_URL);
            assert_eq!(dtype.source, FactSource::Documented);
        }
    }

    #[test]
    fn kv_storage_is_unverified_on_a_card_with_no_known_capability() {
        let b300 = gpu_by_id("B300").expect("present");
        let verdict = resolve_kv_dtype_for(b300, "fp8").expect("dtype exists");
        assert_eq!(verdict.support, QuantSupport::Unverified);
    }

    #[test]
    fn every_quant_method_id_is_unique_and_every_cell_names_a_real_card() {
        for (index, method) in QUANT_METHODS.iter().enumerate() {
            assert!(
                quant_method_by_id(method.id).is_some(),
                "{} must be findable",
                method.id
            );
            assert_eq!(
                QUANT_METHODS
                    .iter()
                    .filter(|other| other.id == method.id)
                    .count(),
                1,
                "{} is duplicated at index {index}",
                method.id
            );
            if let Some(cells) = method.cells {
                for (gpu_id, _) in cells {
                    assert!(
                        gpu_by_id(gpu_id).is_some(),
                        "{} has a cell for unknown card {gpu_id}",
                        method.id
                    );
                }
                for gpu in GPUS {
                    assert!(
                        cells.iter().filter(|(id, _)| *id == gpu.id).count() <= 1,
                        "{} has a duplicate cell for {}",
                        method.id,
                        gpu.id
                    );
                }
            }
        }
    }

    #[test]
    fn unresolved_ids_are_reported_as_unverified_not_as_unknown() {
        let l40s = gpu_by_id("L40S").expect("present");
        let verdict = resolve_quant_for(l40s, "torchao").expect("a known-to-vLLM id resolves");
        assert_eq!(verdict.support, QuantSupport::Unverified);
        assert!(verdict.runnable());
        assert!(verdict.reason.contains("torchao"));
        assert!(
            resolve_quant_for(l40s, "definitely-not-a-quant").is_none(),
            "an id nobody accepts stays unknown"
        );
        assert!(quant_method_by_id("definitely-not-a-quant").is_none());
        assert!(
            quant_method_by_id("torchao").is_none(),
            "unresolved ids are not catalog rows"
        );
    }

    #[test]
    fn int8_w4a8_is_impossible_on_every_modal_card() {
        let method = quant_method_by_id("int8_w4a8").expect("present");
        let cells = method.cells.expect("explicit cells");
        assert_eq!(cells.len(), GPUS.len(), "the row is explicit for every card");
        for gpu in GPUS {
            let verdict = resolve_quant_for(gpu, "int8_w4a8").expect("exists");
            assert_eq!(
                verdict.support,
                QuantSupport::Unsupported,
                "{} must not be offered int8_w4a8",
                gpu.id
            );
            assert!(!verdict.runnable());
        }
    }

    #[test]
    fn awq_on_the_t4_stays_unverified_because_the_sources_conflict() {
        let t4 = gpu_by_id("T4").expect("present");
        let verdict = resolve_quant_for(t4, "awq").expect("exists");
        assert_eq!(verdict.support, QuantSupport::Unverified);
        assert!(verdict.runnable(), "unverified is not a documented impossibility");
        let method = quant_method_by_id("awq").expect("present");
        assert!(
            method.notes.contains("disagree"),
            "the conflict has to be recorded on the row, not just in a comment"
        );
        // The Marlin alias, by contrast, is a documented no on the same card.
        assert_eq!(
            resolve_quant_for(t4, "awq_marlin").expect("exists").support,
            QuantSupport::Unsupported
        );
    }

    #[test]
    fn a_theoretical_capability_never_clears_an_unknown_card() {
        let b300 = gpu_by_id("B300").expect("present");
        assert!(b300.compute_capability.is_none(), "the premise: no capability");
        for method in QUANT_METHODS {
            let verdict = resolve_quant_for(b300, method.id).expect("exists");
            // Nothing can be *cleared* for a card whose capability is unknown: a missing input
            // never produces a positive answer.
            assert_ne!(
                verdict.support,
                QuantSupport::Supported,
                "{} on B300 must not read as supported on a capability nobody verified",
                method.id
            );
            // Where no cell names B300, the comparison must decline to answer rather than fail
            // the card: `Unverified`, not `Unsupported`.
            let documented_for_b300 = method
                .cells
                .is_some_and(|cells| cells.iter().any(|(id, _)| *id == "B300"));
            if documented_for_b300 {
                continue;
            }
            assert_eq!(
                verdict.support,
                QuantSupport::Unverified,
                "{} on B300 cannot be resolved without a capability",
                method.id
            );
            assert!(
                verdict.runnable(),
                "{}: unverified is not a refusal, the operator decides",
                method.id
            );
        }

        // The documented exception, and the reason it is not a capability rule: `int8_w4a8` is
        // x86-CPU-only, and Modal offers no x86 CPU, so it is impossible on B300 for a host reason
        // that an unknown compute capability neither creates nor excuses.
        let w4a8 = resolve_quant_for(b300, "int8_w4a8").expect("exists");
        assert_eq!(w4a8.support, QuantSupport::Unsupported);
        assert!(!w4a8.runnable());
        assert!(w4a8.reason.contains("x86"), "{}", w4a8.reason);
    }

    #[test]
    fn platform_plan_limits_are_the_documented_starter_and_team_rows() {
        let modal = modal();
        assert_eq!(modal.plan_limits.len(), 2);
        let starter = &modal.plan_limits[0];
        assert_eq!(starter.id, "free");
        assert_eq!(starter.label, "Starter");
        assert_eq!(starter.usd_per_month, 0);
        assert_eq!(starter.included_credits_usd, 30);
        assert_eq!(starter.gpu_concurrency, Some(10));
        assert_eq!(starter.seats, Some(3));
        let team = &modal.plan_limits[1];
        assert_eq!(team.id, "paid");
        assert_eq!(team.label, "Team");
        assert_eq!(team.usd_per_month, 250);
        assert_eq!(team.included_credits_usd, 100);
        assert_eq!(team.gpu_concurrency, Some(50));
        assert_eq!(team.seats, None, "Team seats are unlimited, not zero");
        for limit in modal.plan_limits {
            assert_eq!(limit.source, FactSource::Documented);
            assert_eq!(limit.source_url, MODAL_PRICING_URL);
        }
    }

    #[test]
    fn concurrency_ceiling_agrees_with_the_plan_table() {
        let modal = modal();
        let starter = modal
            .plan_limits
            .iter()
            .find(|limit| limit.id == "free")
            .expect("starter row");
        let team = modal
            .plan_limits
            .iter()
            .find(|limit| limit.id == "paid")
            .expect("team row");
        assert_eq!(modal.max_concurrent_containers.free, starter.gpu_concurrency.expect("stated"));
        assert_eq!(modal.max_concurrent_containers.paid, team.gpu_concurrency.expect("stated"));
        assert_eq!(modal.max_concurrent_containers.source, FactSource::Documented);
        assert_eq!(modal.max_concurrent_containers.source_url, MODAL_PRICING_URL);
    }

    #[test]
    fn plan_ids_line_up_with_the_plan_limit_rows() {
        let modal = modal();
        assert_eq!(modal.plans.len(), modal.plan_limits.len());
        for plan in modal.plans {
            assert!(
                modal.plan_limits.iter().any(|limit| limit.id == plan.id),
                "plan {} has no limits row",
                plan.id
            );
        }
    }

    #[test]
    fn platform_flags_the_things_the_bundle_cannot_forget() {
        let modal = modal();
        assert!(!modal.deployment.programmatic, "Modal has no REST deploy API");
        assert!(!modal.deployment.has_detach_flag, "`modal deploy` has no --detach");
        assert!(modal.deployment.strategies.contains(&"rolling"));
        assert!(modal.deployment.strategies.contains(&"recreate"));
        assert_eq!(modal.deployment.multi_gpu_syntax, "gpu=\"H100:8\"");
        assert!(
            modal.payment_method_required,
            "Modal requires a payment method on file before any GPU can be used"
        );
        assert_eq!(modal.max_containers_starter, 100);
        assert_eq!(modal.id, "modal");
        assert!(platform_by_id("modal").is_some());
        assert!(platform_by_id("runpod").is_none());
        assert_eq!(platforms().len(), 1);
    }

    #[test]
    fn scaledown_window_bounds_are_the_documented_ones() {
        let window = modal_scaledown_window();
        assert_eq!(window.min_seconds, 2);
        assert_eq!(window.default_seconds, 60);
        assert_eq!(window.max_seconds, 1200);
        assert_eq!(modal().scaledown_window, window);
    }

    #[test]
    fn compute_capability_parses_strings_numbers_and_display_forms() {
        assert_eq!(ComputeCapability::parse("8.9"), Some(ComputeCapability::new(8, 9)));
        assert_eq!(ComputeCapability::parse(" 12.0 "), Some(ComputeCapability::new(12, 0)));
        assert_eq!(ComputeCapability::parse("9"), Some(ComputeCapability::new(9, 0)));
        assert_eq!(ComputeCapability::parse("eight"), None);
        assert_eq!(
            ComputeCapability::from_f64(8.9),
            Some(ComputeCapability::new(8, 9)),
            "the float 8.9 must not truncate to 8.8"
        );
        assert_eq!(ComputeCapability::from_f64(9.0), Some(ComputeCapability::new(9, 0)));
        assert_eq!(ComputeCapability::from_f64(-1.0), None);
        assert_eq!(ComputeCapability::new(8, 9).as_string(), "8.9");
        assert!(ComputeCapability::new(8, 9) > ComputeCapability::new(8, 6));
        assert!(ComputeCapability::new(12, 0) > ComputeCapability::new(10, 0));
    }

    #[test]
    fn compute_capability_round_trips_through_json_in_both_forms() {
        let cc = ComputeCapability::new(12, 0);
        let text = serde_json::to_string(&cc).expect("serialises");
        assert_eq!(text, "\"12.0\"");
        let back: ComputeCapability = serde_json::from_str(&text).expect("round-trips");
        assert_eq!(back, cc);
        let numeric: ComputeCapability = serde_json::from_str("8.9").expect("accepts a number");
        assert_eq!(numeric, ComputeCapability::new(8, 9));
        let wrong: Result<ComputeCapability, _> = serde_json::from_str("\"sm89\"");
        assert!(wrong.is_err(), "an unparseable capability is an error, not a default");
    }

    #[test]
    fn the_catalog_response_carries_the_fields_the_panel_reads() {
        let value = serde_json::to_value(modal()).expect("platform serialises");
        let gpus = value["gpus"].as_array().expect("gpus array");
        assert_eq!(gpus.len(), 11);
        let rtx = gpus
            .iter()
            .find(|gpu| gpu["id"] == "RTX-PRO-6000")
            .expect("rtx row");
        assert_eq!(rtx["label"], "RTX PRO 6000");
        assert_eq!(rtx["vram_gib"], 96.0);
        assert_eq!(rtx["vram_bytes"], 103_079_215_104u64);
        assert_eq!(rtx["vram_source"], "datasheet");
        assert_eq!(rtx["usable_vram_gib"], 95.0);
        assert_eq!(rtx["free_plan"], "unknown");
        assert_eq!(rtx["source"], "documented");
        assert_eq!(rtx["checked_at"], CATALOG_CHECKED_AT);
        assert_eq!(rtx["source_url"], MODAL_PRICING_URL);
        assert_eq!(rtx["compute_capability"], "12.0");
        assert_eq!(rtx["max_gpus_per_container"], 8);
        let b300 = gpus.iter().find(|gpu| gpu["id"] == "B300").expect("b300 row");
        assert!(b300["vram_gib"].is_null());
        assert!(b300["vram_bytes"].is_null());
        assert_eq!(b300["min_cuda"], "13.1");
        assert_eq!(value["plan_limits"][0]["label"], "Starter");
        assert_eq!(value["plan_limits"][1]["gpu_concurrency"], 50);
        assert!(value["plan_limits"][1]["seats"].is_null());
        assert_eq!(value["max_concurrent_containers"]["free"], 10);
        assert_eq!(value["max_concurrent_containers"]["free"], 10);
        assert_eq!(value["payment_method_required"], true);
    }

    #[test]
    fn quant_rows_serialise_their_cells_and_provenance() {
        let value = serde_json::to_value(quant_methods()).expect("methods serialise");
        let rows = value.as_array().expect("array");
        let fp8 = rows
            .iter()
            .find(|row| row["id"] == "fp8")
            .expect("fp8 row");
        assert_eq!(fp8["min_compute_capability"], "8.9");
        assert_eq!(fp8["bytes_per_param"], 1.0);
        assert_eq!(fp8["source"], "documented");
        let cells = fp8["cells"].as_array().expect("cells array");
        assert!(
            cells.iter().any(|cell| cell[0] == "T4" && cell[1] == "unsupported"),
            "the T4 exclusion must survive serialisation"
        );
        let gptq = rows.iter().find(|row| row["id"] == "gptq").expect("gptq row");
        assert_eq!(gptq["bytes_per_param"], 0.5);
        let gguf = rows.iter().find(|row| row["id"] == "gguf").expect("gguf row");
        assert!(
            gguf["bytes_per_param"].is_null(),
            "GGUF's width depends on the file, so it must not be given a number"
        );
    }
}
