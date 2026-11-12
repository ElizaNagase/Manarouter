//! The GPU-rental *pure logic*: what a card is, what it costs, what it can execute, and how big a
//! KV-cache pool it can hold.
//!
//! ## What belongs here, and what does not
//!
//! This module tree contains no HTTP, no filesystem access and no clock. It answers questions from
//! constants and from caller-supplied facts, so every number it produces can be checked in a unit
//! test with no network and no GPU.
//!
//! | Question | Module |
//! |---|---|
//! | What cards exist, what do they cost, what may they execute? | [`catalog`] |
//! | What does a HuggingFace `config.json` say about cache topology? | [`arch`] |
//! | How many KV tokens fit, and how much is that number to be trusted? | [`sizing`] |
//! | The checkpoint is FP8 and 4-bit is wanted — what job, card, cost? | [`requantize`] |
//!
//! The three HTTP surfaces in `docs/research/ARCHITECTURE-gpu-rentals.md` §5 — `GET /api/gpu/catalog`,
//! `POST /api/gpu/probe` and `POST /api/gpu/apply` — live in the conductor and are thin adapters over
//! this tree plus the HuggingFace client. Keeping that split means a conductor restart is never
//! needed to test a sizing formula, and a wrong price is caught by a test rather than by an invoice.
//!
//! ## The rule the whole tree obeys
//!
//! **No invented facts.** Every constant carries a source, every unverifiable cell stays
//! `unverified` instead of defaulting to "probably fine", and every derived number ships with its
//! assumptions and its distance from the operator's own measurement. That is not politeness: the
//! operator is being asked to spend money on the strength of these numbers, and a confidently wrong
//! KV pool causes a mid-request OOM, which is worse than an admission decision that admits fewer
//! requests than the card could hold.

pub mod arch;
pub mod catalog;
pub mod cerebrium;
pub mod requantize;
pub mod sizing;

pub use arch::ArchFacts;
pub use catalog::{
    gpu_by_id, kv_cache_dtype_by_id, modal_scaledown_window, quant_method_by_id, resolve_quant_for,
    GpuSpec, QuantMethod, QuantSupport, QuantVerdict, CATALOG_CHECKED_AT,
};
pub use requantize::{
    plan_requantize, GpuHoursRow, QuantSource, QuantTarget, RecipeRequest, RequantizePath,
    RequantizePlan,
};
pub use sizing::{
    size_deployment, CalibrationPoint, SizingConfidence, SizingRequest, SizingResult,
    DEFAULT_BLOCK_SIZE, DEFAULT_GPU_MEMORY_UTILIZATION, DEFAULT_OVERHEAD_BYTES,
};
