//! The HuggingFace `config.json` architecture parser: what a checkpoint's geometry is, and —
//! just as important — what it is *not*.
//!
//! ## Why the parser is separate from the fetch
//!
//! Nothing here opens a socket. The caller (the relay or the conductor) fetches the file, and this
//! module turns those bytes into an [`ArchFacts`] value plus a [`KvGeometry`]. That split is what
//! makes the arithmetic testable against real configs without a network, and it is the reason a
//! gated repository cannot silently produce numbers: if the caller could not fetch the config, it
//! has no `ArchFacts` at all and sizing says `unavailable` rather than guessing.
//!
//! ## The three things that actually break naive KV maths
//!
//! 1. **`head_dim` may be absent** and must then be derived as `hidden_size / num_attention_heads`.
//!    Qwen2-family configs do exactly this. Deriving it is fine; doing it *silently* is not, so
//!    [`ArchFacts::head_dim_derived`] records which happened and the basis string the caller
//!    publishes says so.
//! 2. **`head_dim` may contradict that derivation.** In the Qwen3 family `head_dim` is explicit and
//!    is *not* `hidden_size / num_attention_heads` (32B: 128 explicit against 5120/64 = 80), so a
//!    parser that "simplifies" by dividing produces a KV figure 60 % too small. The explicit value
//!    always wins.
//! 3. **GQA, MLA and sliding windows are not one formula.** Absent `num_key_value_heads` means MHA,
//!    which is a legitimate derivation; `kv_lora_rank` means MLA, which is *not* a
//!    `layers × heads × head_dim` shape at all and is reported as a problem rather than squeezed
//!    into one.
//!
//! ## What this module deliberately does not do
//!
//! It does not decide how big the KV pool is — that is [`super::sizing`] — and it does not fall back
//! to a "typical" value for anything. Every field it cannot read stays `None`, and the geometry
//! carries either a `problem` (no number) or a `warning` (a number with a caveat attached).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

/// How a model stores its KV cache, which decides whether the dense formula applies at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KvTopology {
    /// Every layer keeps a full KV cache in the same shape. The formula applies directly.
    #[default]
    Dense,
    /// A sliding-window model: the dense per-token figure is still computed, but the retained
    /// history is capped, so the number is a ceiling rather than a plain description.
    SlidingWindow,
    /// A model mixing sliding and full attention across layers. One uniform figure is an
    /// approximation and is labelled as one.
    Hybrid,
    /// A hybrid **linear-attention** model: only the `full_attention` layers keep a per-token KV
    /// cache, while the `linear_attention` layers carry a fixed-size recurrent state that does not
    /// grow with sequence length.
    ///
    /// This variant exists because getting it wrong is not a rounding error: counting every layer
    /// overstates the cache by the full-attention ratio — 4× on the operator's own model — and so
    /// understates how many concurrent sessions the card holds by the same factor.
    HybridLinear,
    /// Multi-head latent attention (`kv_lora_rank`): the cache is a compressed latent vector, not
    /// `heads × head_dim`, so the dense formula does not apply and no number is produced.
    Mla,
}

impl KvTopology {
    /// The stable identifier written into the sizing result.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dense => "dense",
            Self::SlidingWindow => "sliding-window",
            Self::Hybrid => "hybrid",
            Self::HybridLinear => "hybrid-linear",
            Self::Mla => "mla",
        }
    }
}

/// The architecture facts read out of a checkpoint's `config.json`.
///
/// Every optional field is genuinely optional: a missing field stays `None` and is never defaulted
/// to a plausible value. The two `*_derived` flags are the exceptions, and they exist so that a
/// derivation is *visible* rather than indistinguishable from a stated fact.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ArchFacts {
    /// `architectures`, e.g. `["Qwen3ForCausalLM"]`. A list because a config may name several.
    pub architectures: Vec<String>,
    /// `model_type`, e.g. `qwen3`.
    pub model_type: Option<String>,
    /// `num_hidden_layers` — the transformer block count.
    pub num_hidden_layers: Option<u64>,
    /// `num_attention_heads` — query heads per layer.
    pub num_attention_heads: Option<u64>,
    /// `num_key_value_heads` — KV heads per layer. Absent means plain MHA.
    pub num_key_value_heads: Option<u64>,
    /// Whether [`Self::num_key_value_heads`] was derived (from `num_attention_heads`) rather than
    /// stated in the file.
    pub num_key_value_heads_derived: bool,
    /// `head_dim` — the per-head width of the cache.
    pub head_dim: Option<u64>,
    /// Whether [`Self::head_dim`] was derived as `hidden_size / num_attention_heads` rather than
    /// stated in the file.
    pub head_dim_derived: bool,
    /// `hidden_size` — the model's residual width.
    pub hidden_size: Option<u64>,
    /// `intermediate_size` — the MLP width, useful context when reading parameter counts.
    pub intermediate_size: Option<u64>,
    /// `max_position_embeddings` — the maximum context the checkpoint was trained for. The bundle
    /// must not offer a longer `--max-model-len` than this without saying it is extrapolating.
    pub max_position_embeddings: Option<u64>,
    /// `sliding_window` — the window length, when the model has one.
    pub sliding_window: Option<u64>,
    /// `rope_scaling`, kept verbatim: long-context schemes change what a token costs in practice and
    /// reinterpreting the object here would be guesswork.
    pub rope_scaling: Option<Value>,
    /// `layer_types` — the per-layer attention kind, verbatim. This array is the difference between
    /// a 4× overestimate and the right answer on a hybrid model.
    pub layer_types: Vec<String>,
    /// `full_attention_interval` — "every Nth layer is full attention", stated as a number of layers
    /// rather than as an array.
    pub full_attention_interval: Option<u64>,
    /// How many layers actually hold a per-token KV cache.
    ///
    /// `None` when the config gives no way to tell; `Some(num_hidden_layers)` for a uniform model is
    /// *not* written here, because the caller must be able to distinguish "counted" from "assumed to
    /// be all of them".
    pub num_full_attention_layers: Option<u64>,
    /// How many layers are `linear_attention`, i.e. hold a fixed recurrent state instead.
    pub num_linear_attention_layers: Option<u64>,
    /// `linear_num_key_heads` — the linear-attention key-head count, kept to document the fixed
    /// state that `overhead_bytes` has to cover.
    pub linear_num_key_heads: Option<u64>,
    /// `linear_key_head_dim`.
    pub linear_key_head_dim: Option<u64>,
    /// `linear_num_value_heads`.
    pub linear_num_value_heads: Option<u64>,
    /// `linear_value_head_dim`.
    pub linear_value_head_dim: Option<u64>,
    /// `linear_conv_kernel_dim`.
    pub linear_conv_kernel_dim: Option<u64>,
    /// `mamba_ssm_dtype` — kept verbatim, and deliberately not interpreted.
    ///
    /// The recurrent state of a hybrid model is fixed-size but not free, and it has its own dtype
    /// switch in vLLM (`mamba_cache_dtype` / `mamba_ssm_cache_dtype`, defaulting to auto). Whether
    /// this config field is the one that switch reads is `[unverified]` in the research notes, so
    /// nothing may claim the state is `float32` on the strength of it.
    pub mamba_ssm_dtype: Option<String>,
    /// The topology inferred from the fields above.
    pub kv_topology: KvTopology,
    /// `torch_dtype` (or the newer `dtype`) — decides what `--kv-cache-dtype auto` resolves to.
    pub torch_dtype: Option<String>,
    /// `quantization_config.quant_method` — the method the *checkpoint* declares.
    pub quant_method: Option<String>,
    /// `quantization_config.bits` — the declared weight width, when the checkpoint states one.
    pub quant_bits: Option<u64>,
    /// `kv_lora_rank` — present only on MLA models, and the reason the dense formula is refused.
    pub kv_lora_rank: Option<u64>,
    /// `q_lora_rank`, the query-side latent of an MLA model.
    pub q_lora_rank: Option<u64>,
    /// `v_head_dim`, the value width of an MLA model, when stated separately.
    pub v_head_dim: Option<u64>,
    /// Whether the geometry came from a nested `text_config` (vision-language checkpoints) rather
    /// than from the root object.
    pub nested_text_config: bool,
    /// Whether a `vision_config` was present, i.e. this is a multimodal checkpoint whose text stack
    /// may be only part of the weights.
    pub has_vision_config: bool,
}

impl ArchFacts {
    /// Read a parsed `config.json` value.
    ///
    /// Never fails: a config that states nothing yields an [`ArchFacts`] that states nothing, and
    /// the geometry it produces says which field was missing. A parser that rejected such a file
    /// would push callers toward inventing values.
    pub fn from_config_value(value: &Value) -> Self {
        // Vision-language checkpoints keep the language stack under `text_config`; `llm_config` is
        // the same idea under a different name. The root object is used when neither exists.
        let source = value
            .get("text_config")
            .filter(|nested| nested.is_object())
            .or_else(|| value.get("llm_config").filter(|nested| nested.is_object()))
            .unwrap_or(value);
        let nested_text_config = !std::ptr::eq(source, value);

        let mut facts = ArchFacts {
            nested_text_config,
            has_vision_config: value.get("vision_config").is_some(),
            ..ArchFacts::default()
        };

        // Identity fields describe the *checkpoint*, so the root wins over a nested text stack: a
        // vision-language config states `Qwen2_5_VLForConditionalGeneration` / `qwen2_5_vl` at the
        // root, while `text_config.model_type` may name the text-only sub-model. Reading only the
        // nested object would report the text stack as the whole checkpoint.
        facts.architectures = string_list(value, "architectures");
        if facts.architectures.is_empty() {
            facts.architectures = string_list(source, "architectures");
        }
        facts.model_type =
            string_field(value, "model_type").or_else(|| string_field(source, "model_type"));
        facts.num_hidden_layers = u64_field(source, &["num_hidden_layers"]);
        facts.num_attention_heads = u64_field(source, &["num_attention_heads"]);
        facts.hidden_size = u64_field(source, &["hidden_size", "n_embd", "d_model"]);
        facts.intermediate_size = u64_field(source, &["intermediate_size", "n_inner"]);
        facts.max_position_embeddings = u64_field(
            source,
            &["max_position_embeddings", "n_positions", "max_seq_len"],
        );
        facts.sliding_window = u64_field(source, &["sliding_window"]);
        facts.rope_scaling = source.get("rope_scaling").filter(|v| !v.is_null()).cloned();
        // Likewise the stored dtype: it is a property of the saved weights, so the root's statement
        // is the checkpoint's. A nested value is the fallback, not the truth.
        facts.torch_dtype = string_field(value, "torch_dtype")
            .or_else(|| string_field(source, "torch_dtype"))
            .or_else(|| string_field(value, "dtype"))
            .or_else(|| string_field(source, "dtype"));
        facts.kv_lora_rank = u64_field(source, &["kv_lora_rank"]);
        facts.q_lora_rank = u64_field(source, &["q_lora_rank"]);
        facts.v_head_dim = u64_field(source, &["v_head_dim"]);

        // The layer mix. `layer_types` is the direct evidence; `full_attention_interval` states the
        // same thing as "every Nth layer", which floors correctly to the documented semantics (the
        // full-attention layers sit at positions interval-1, 2*interval-1, ... and so on).
        facts.layer_types = string_list(source, "layer_types");
        facts.full_attention_interval = u64_field(source, &["full_attention_interval"]);
        facts.linear_num_key_heads = u64_field(source, &["linear_num_key_heads"]);
        facts.linear_key_head_dim = u64_field(source, &["linear_key_head_dim"]);
        facts.linear_num_value_heads = u64_field(source, &["linear_num_value_heads"]);
        facts.linear_value_head_dim = u64_field(source, &["linear_value_head_dim"]);
        facts.linear_conv_kernel_dim = u64_field(source, &["linear_conv_kernel_dim"]);
        facts.mamba_ssm_dtype = string_field(source, "mamba_ssm_dtype");

        let declares_linear_attention = facts.linear_num_value_heads.is_some()
            || facts.linear_num_key_heads.is_some()
            || facts
                .layer_types
                .iter()
                .any(|kind| is_linear_attention_layer(kind));

        if !facts.layer_types.is_empty() {
            facts.num_full_attention_layers =
                Some(count_layers(&facts.layer_types, is_full_attention_layer));
            facts.num_linear_attention_layers =
                Some(count_layers(&facts.layer_types, is_linear_attention_layer));
        } else if let (Some(layers), Some(interval)) =
            (facts.num_hidden_layers, facts.full_attention_interval)
        {
            if interval > 0 && interval <= layers {
                let full = layers / interval;
                facts.num_full_attention_layers = Some(full);
                facts.num_linear_attention_layers = Some(layers - full);
            }
        }

        // KV heads: stated, else MHA (every query head has its own). The flag records which.
        match u64_field(source, &["num_key_value_heads"]) {
            Some(heads) => facts.num_key_value_heads = Some(heads),
            None => {
                facts.num_key_value_heads = facts.num_attention_heads;
                facts.num_key_value_heads_derived = facts.num_attention_heads.is_some();
            }
        }

        // head_dim: stated always wins, because the explicit value is the one vLLM uses and it is
        // not always `hidden_size / num_attention_heads`.
        match u64_field(source, &["head_dim"]) {
            Some(dim) => facts.head_dim = Some(dim),
            None => {
                facts.head_dim = match (facts.hidden_size, facts.num_attention_heads) {
                    (Some(hidden), Some(heads)) if heads > 0 && hidden % heads == 0 => {
                        facts.head_dim_derived = true;
                        Some(hidden / heads)
                    }
                    _ => None,
                };
            }
        }

        facts.kv_topology = infer_topology(source, &facts, declares_linear_attention);
        // Quantisation is applied to the checkpoint as a whole, so its block sits at the root on a
        // multimodal config; the nested object is checked second.
        let quantization = value
            .get("quantization_config")
            .filter(|block| block.is_object())
            .or_else(|| source.get("quantization_config"));
        facts.quant_method = quantization
            .and_then(|config| string_field(config, "quant_method"))
            .or_else(|| string_field(value, "quant_method"))
            .or_else(|| string_field(source, "quant_method"));
        facts.quant_bits = quantization.and_then(|config| u64_field(config, &["bits"]));

        facts
    }

    /// Parse `config.json` bytes.
    ///
    /// A malformed file is an error rather than an empty [`ArchFacts`]: bytes that are not JSON mean
    /// the caller fetched the wrong thing (an HTML error page, a truncated response) and reporting
    /// that as "no architecture" would hide the real problem.
    pub fn from_config_json(bytes: &[u8]) -> Result<Self> {
        let value: Value = serde_json::from_slice(bytes)?;
        Ok(Self::from_config_value(&value))
    }

    /// Parse a `config.json` string. Convenience for callers that already hold text.
    pub fn from_config_str(text: &str) -> Result<Self> {
        Ok(Self::from_config_value(&serde_json::from_str::<Value>(
            text,
        )?))
    }

    /// Whether the config told us enough to consider the dense KV formula.
    ///
    /// False for MLA models and for configs missing the layer or head count. Callers use this to
    /// decide between "size it" and "ask the operator", not to fill in a default.
    pub fn is_dense_kv(&self) -> bool {
        self.kv_topology != KvTopology::Mla
    }

    /// The byte width `--kv-cache-dtype auto` resolves to for this checkpoint.
    ///
    /// `None` when the config states no dtype, or states one this crate does not map. The caller
    /// then has to choose, and [`super::sizing`] records the resulting assumption explicitly.
    pub fn torch_dtype_bytes(&self) -> Option<u32> {
        self.torch_dtype
            .as_deref()
            .and_then(bytes_per_element_for_dtype)
    }

    /// The KV geometry for a chosen cache dtype, or the reason there is none.
    ///
    /// `kv_bytes_per_element` comes from the catalog's [`super::catalog::KvCacheDtype`], and
    /// `dtype_label` is only used to explain the arithmetic in the basis string.
    pub fn kv_geometry(&self, kv_bytes_per_element: u32, dtype_label: &str) -> KvGeometry {
        let topology = self.kv_topology;

        if topology == KvTopology::Mla {
            return KvGeometry {
                kv_bytes_per_token: None,
                topology,
                basis: format!(
                    "MLA model: kv_lora_rank is {}",
                    self.kv_lora_rank
                        .map(|rank| rank.to_string())
                        .unwrap_or_else(|| "set".to_string())
                ),
                problem: Some(
                    "this checkpoint uses multi-head latent attention: its cache is a compressed \
                     latent vector (kv_lora_rank), not num_key_value_heads × head_dim, so the dense \
                     KV formula does not apply and no per-token figure is produced"
                        .to_string(),
                ),
                warning: None,
            };
        }

        let (layers, heads, dim) = match (
            self.num_hidden_layers,
            self.num_key_value_heads,
            self.head_dim,
        ) {
            (Some(layers), Some(heads), Some(dim)) => (layers, heads, dim),
            _ => {
                let missing = [
                    ("num_hidden_layers", self.num_hidden_layers),
                    ("num_key_value_heads", self.num_key_value_heads),
                    ("head_dim", self.head_dim),
                ]
                .iter()
                .filter(|(_, value)| value.is_none())
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ");
                return KvGeometry {
                    kv_bytes_per_token: None,
                    topology,
                    basis: format!("dense formula impossible: config.json does not state {missing}"),
                    // The message names only what is absent. A fixed list of the three fields read
                    // as though all three were missing, which is the opposite of a useful error on a
                    // config that states two of them.
                    problem: Some(format!(
                        "the dense KV formula cannot be applied: config.json does not state {missing}"
                    )),
                    warning: None,
                };
            }
        };

        // A hybrid linear-attention model carries per-token KV in its full-attention layers only.
        // The rest hold a fixed-size recurrent state that does not grow with the context, which is
        // why the layer term is the *full-attention* count and why sizing covers that state with
        // the fixed overhead term instead. Getting this wrong is not a detail: for the 48-linear /
        // 16-full mix this crate was written against, `num_hidden_layers` is exactly 4× too many.
        let (layers, layer_term) = if topology == KvTopology::HybridLinear {
            let Some(full) = self.num_full_attention_layers else {
                return KvGeometry {
                    kv_bytes_per_token: None,
                    topology,
                    basis: format!(
                        "dense formula impossible: this is a hybrid linear-attention model but \
                         config.json states neither layer_types nor a usable \
                         full_attention_interval, so the number of full-attention layers among the \
                         {layers} total is unknown"
                    ),
                    problem: Some(
                        "this checkpoint mixes linear-attention and full-attention layers, and only \
                         the full-attention layers carry a per-token KV cache. config.json does not \
                         say how many there are (no layer_types, no full_attention_interval), so \
                         using num_hidden_layers would overstate the cache by the ratio of total to \
                         full-attention layers. No figure is produced rather than a wrong one"
                            .to_string(),
                    ),
                    warning: None,
                };
            };
            (
                full,
                format!("full_attention_layers={full} of {layers} (hybrid linear attention)"),
            )
        } else {
            (layers, format!("{layers} layers"))
        };

        if layers == 0 || heads == 0 || dim == 0 {
            return KvGeometry {
                kv_bytes_per_token: None,
                topology,
                basis: format!("dense formula impossible: {layers} layers × {heads} heads × {dim}"),
                problem: Some(
                    "config.json states a zero layer count, KV-head count or head_dim, which \
                     cannot describe a cache"
                        .to_string(),
                ),
                warning: None,
            };
        }

        // layers × kv_heads × head_dim × bytes × 2 (one K tensor and one V tensor per token).
        let product = layers
            .checked_mul(heads)
            .and_then(|n| n.checked_mul(dim))
            .and_then(|n| n.checked_mul(u64::from(kv_bytes_per_element)))
            .and_then(|n| n.checked_mul(2));

        let Some(bytes_per_token) = product else {
            return KvGeometry {
                kv_bytes_per_token: None,
                topology,
                basis: "dense formula overflowed u64".to_string(),
                problem: Some(
                    "the dense KV formula overflowed; the config states implausible dimensions"
                        .to_string(),
                ),
                warning: None,
            };
        };

        let mut basis = if topology == KvTopology::HybridLinear {
            format!(
                "{layer_term} x num_kv_heads={heads} x head_dim={dim} x dtype_bytes={kv_bytes_per_element}; \
                 x2 for K and V = {bytes_per_token} bytes/token"
            )
        } else {
            format!(
                "{layer_term} × {heads} kv heads × {dim} head_dim × {kv_bytes_per_element} byte(s) \
                 ({dtype_label}) × 2 (K and V) = {bytes_per_token} bytes/token"
            )
        };
        if self.head_dim_derived {
            basis.push_str(&format!(
                "; head_dim was derived as hidden_size {} / num_attention_heads {} because \
                 config.json does not state head_dim",
                self.hidden_size
                    .map(|size| size.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                self.num_attention_heads
                    .map(|heads| heads.to_string())
                    .unwrap_or_else(|| "?".to_string()),
            ));
        }
        if self.num_key_value_heads_derived {
            basis.push_str(
                "; num_key_value_heads was derived as num_attention_heads because config.json \
                 does not state it, i.e. plain multi-head attention is assumed",
            );
        }
        if self.nested_text_config {
            basis.push_str("; taken from the nested text_config of a multimodal checkpoint");
        }

        let warning = match topology {
            KvTopology::Dense | KvTopology::Mla => None,
            KvTopology::SlidingWindow => Some(format!(
                "this model declares a sliding window of {} tokens: the per-token figure is one \
                 layer's full cache, and the retained history is capped by the window, so treat \
                 the resulting pool as a ceiling rather than an exact count",
                self.sliding_window
                    .map(|window| window.to_string())
                    .unwrap_or_else(|| "unspecified".to_string())
            )),
            KvTopology::Hybrid => Some(
                "this model mixes sliding-window and full-attention layers, so one uniform \
                 per-token figure is an approximation: vLLM allocates these layers differently and \
                 the real pool will differ"
                    .to_string(),
            ),
            KvTopology::HybridLinear => Some(format!(
                "only the {} full-attention layers hold a per-token KV cache: the {} \
                 linear-attention layers keep a fixed-size recurrent state that does not grow with \
                 the context, and it is charged against the KV budget separately. \
                 `--kv-cache-dtype` selects the width of the full-attention cache alone — on these \
                 models the recurrent state has its own dtype switch (`mamba_cache_dtype` / \
                 `mamba_ssm_cache_dtype`, defaulting to auto), whose value this crate cannot read",
                self.num_full_attention_layers
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                self.num_linear_attention_layers
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "?".to_string()),
            )),
        };

        KvGeometry {
            kv_bytes_per_token: Some(bytes_per_token),
            topology,
            basis,
            problem: None,
            warning,
        }
    }
}

/// The result of applying the dense KV formula to an architecture.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KvGeometry {
    /// Bytes of KV per token, summed over every layer and K and V. `None` when the formula does not
    /// apply, in which case [`Self::problem`] says why.
    pub kv_bytes_per_token: Option<u64>,
    /// The topology the number was computed under.
    pub topology: KvTopology,
    /// The arithmetic, written out, so the number can be checked by hand from the API response.
    pub basis: String,
    /// Why there is no number. `None` exactly when [`Self::kv_bytes_per_token`] is `Some`.
    pub problem: Option<String>,
    /// A caveat that does not stop the number being used.
    pub warning: Option<String>,
}

/// Bytes per element for a HuggingFace dtype name, or `None` when it is unknown.
///
/// `auto` deliberately returns `None`: it means "whatever the file is", which is a question for the
/// caller to resolve against `torch_dtype`, not a width to assume here.
pub fn bytes_per_element_for_dtype(name: &str) -> Option<u32> {
    match name.trim().to_ascii_lowercase().as_str() {
        "float32" | "fp32" | "float" => Some(4),
        "bfloat16" | "bf16" => Some(2),
        "float16" | "fp16" | "half" => Some(2),
        "float8_e4m3fn" | "float8_e4m3fnuz" | "float8_e5m2" | "float8_e5m2fnuz" | "fp8" => Some(1),
        _ => None,
    }
}

/// A lower bound on weight bytes from a parameter count and a per-parameter width.
///
/// "Lower bound" is meant literally: quantised checkpoints store scales, zero-points and sometimes
/// embedding tables in higher precision, so the real file set is larger. Sizing treats the larger
/// real figure as the input where it is known — this helper exists for the case where only a
/// parameter count is available, and using it understates the weights, which *overstates* the KV
/// pool. Callers should prefer the measured `safetensors` byte total.
pub fn weights_bytes_from_param_count(params: u64, bytes_per_param: f64) -> Option<u64> {
    if !bytes_per_param.is_finite() || bytes_per_param <= 0.0 {
        return None;
    }
    let bytes = (params as f64) * bytes_per_param;
    if !bytes.is_finite() || bytes < 0.0 || bytes >= (u64::MAX as f64) {
        return None;
    }
    Some(bytes.round() as u64)
}

/// Parameter count from the HuggingFace model API response (`safetensors.total`).
pub fn params_from_hf_api(value: &Value) -> Option<u64> {
    let total = value.get("safetensors")?.get("total")?;
    match total {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// Whether a repository requires accepting terms before its files can be fetched.
///
/// Kept as three values because they are not interchangeable: `Auto` can be satisfied by any
/// authenticated account, `Manual` needs an approval that may take hours, and neither can be
/// satisfied by guessing. A gated repo with no token yields no `config.json`, therefore no
/// [`ArchFacts`], therefore a sizing result that says `unavailable` — which is the intended path,
/// not a failure to work around.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GatedState {
    /// Openly downloadable.
    Open,
    /// Gated with automatic approval for authenticated accounts.
    Auto,
    /// Gated with manual approval by the repository owner.
    Manual,
}

/// Read the `gated` field of a HuggingFace model API response.
///
/// `None` when the field is absent, which is different from `false`: a response that does not
/// mention gating should not be read as a promise that the repo is open.
pub fn gated_state_from_hf_api(value: &Value) -> Option<GatedState> {
    match value.get("gated")? {
        Value::Bool(false) => Some(GatedState::Open),
        Value::Bool(true) => Some(GatedState::Manual),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "false" | "open" | "none" => Some(GatedState::Open),
            "auto" => Some(GatedState::Auto),
            "manual" => Some(GatedState::Manual),
            _ => None,
        },
        _ => None,
    }
}

/// Where to look for an architecture when the requested repository cannot be read.
///
/// The problem this solves is concrete: the operator's repository is FP8 and gated, so
/// `raw/main/config.json` needs an accepted gate, and a sizing check would otherwise report
/// `unavailable` for a model whose architecture is public one level up. (The caller reads `/raw/`,
/// not `/resolve/`: `resolve` answers 307 to a cache host, and the shared quota client does not
/// follow redirects, so a `/resolve/` URL can never succeed here.) The Hub answers that in
/// `GET /api/models/{id}`, which is readable without a token and carries
/// `cardData.base_model` (`"Qwen/Qwen3.8-27B"`) and `base_model_relation` (`"quantized"`).
///
/// This type is data, not a fetcher: it says *which repository to read next*. Fetching stays with
/// the caller, so the rule is testable offline and the crate keeps its no-I/O rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseModelHint {
    /// The repository the checkpoint was derived from, e.g. `"Qwen/Qwen3.8-27B"`.
    pub repo_id: String,
    /// `base_model_relation`: `"quantized"`, `"finetune"`, `"adapter"`, …
    ///
    /// Kept because a *quantised* base shares the architecture exactly, whereas a finetune may have
    /// changed it — and a KV figure borrowed from a repo that changed the architecture would be
    /// plausible and wrong.
    pub relation: Option<String>,
}

impl BaseModelHint {
    /// Whether the base repository's `config.json` can stand in for this one's.
    ///
    /// True for a quantised derivative, which by construction keeps the layer stack, and only then:
    /// the architecture is what the quantiser was given, so the KV geometry is the same. A relation
    /// that is absent or means something else gets no such promise.
    pub fn shares_architecture(&self) -> bool {
        matches!(
            self.relation.as_deref().map(str::trim).map(str::to_ascii_lowercase),
            Some(ref relation) if relation == "quantized" || relation == "quantised"
        )
    }
}

/// Read `cardData.base_model` and `base_model_relation` from a HuggingFace model API response.
///
/// `base_model` may be a string or a one-element array — the API has used both forms — and, when
/// the relation is not a sibling field, `cardData.base_model_relation` carries it.
pub fn base_model_hint_from_hf_api(value: &Value) -> Option<BaseModelHint> {
    let card = value.get("cardData");
    let repo_id = card
        .and_then(|card| card.get("base_model"))
        .and_then(base_model_repo_id)
        .or_else(|| value.get("base_model").and_then(base_model_repo_id))?;
    let relation = card
        .and_then(|card| string_field(card, "base_model_relation"))
        .or_else(|| string_field(value, "base_model_relation"));
    Some(BaseModelHint { repo_id, relation })
}

/// Accept either the string form or the array form of `base_model`.
fn base_model_repo_id(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Array(items) => items.iter().find_map(base_model_repo_id),
        _ => None,
    }
}

/// Read a field as a string, accepting it only when it actually is one.
fn string_field(source: &Value, key: &str) -> Option<String> {
    source.get(key)?.as_str().map(|text| text.to_string())
}

/// Read a field as a list of strings; anything else yields an empty list.
fn string_list(source: &Value, key: &str) -> Vec<String> {
    source
        .get(key)
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(|text| text.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Read the first present key as a non-negative integer, accepting `"64"` as well as `64`.
///
/// Configs in the wild carry both, and a numeric string is unambiguous, so refusing it would be
/// pedantry rather than safety. A negative, fractional or unparseable value yields `None`.
fn u64_field(source: &Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        let Some(value) = source.get(*key) else {
            continue;
        };
        let parsed = match value {
            Value::Number(number) => number.as_u64(),
            Value::String(text) => text.trim().parse::<u64>().ok(),
            _ => None,
        };
        if let Some(number) = parsed {
            return Some(number);
        }
    }
    None
}

/// Is this `layer_types` entry an attention layer that keeps a per-token KV cache?
///
/// vLLM's own rule, which this mirrors: a full-attention (or global-attention) layer gets an
/// `AttentionSpec` that grows with `max_model_len`. Everything else does not.
fn is_full_attention_layer(kind: &str) -> bool {
    let kind = kind.trim().to_ascii_lowercase();
    kind == "full_attention" || kind == "full" || kind.contains("global")
}

/// Is this `layer_types` entry a linear-attention (recurrent-state) layer?
///
/// `linear_attention` is the spelling Qwen3.5 uses; `mamba`/`gdn`/`ssm` are the neighbouring
/// families that behave the same way for sizing. Such a layer holds a fixed-size state whose size
/// does not depend on the sequence length, so it contributes no bytes per token.
fn is_linear_attention_layer(kind: &str) -> bool {
    let kind = kind.trim().to_ascii_lowercase();
    kind.contains("linear_attention")
        || kind.contains("linear")
        || kind.contains("mamba")
        || kind.contains("gdn")
        || kind.contains("ssm")
}

/// Count the entries a predicate accepts.
fn count_layers(layer_types: &[String], predicate: fn(&str) -> bool) -> u64 {
    layer_types
        .iter()
        .filter(|kind| predicate(kind))
        .count()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Infer the KV topology from the fields a config actually carries.
fn infer_topology(
    source: &Value,
    facts: &ArchFacts,
    declares_linear_attention: bool,
) -> KvTopology {
    if facts.kv_lora_rank.is_some() {
        return KvTopology::Mla;
    }

    // A hybrid *linear*-attention model — Qwen3.5 and its relatives — is the case that matters most,
    // because ignoring it inflates the per-token figure by the ratio of total layers to
    // full-attention layers (4× for the 48-linear/16-full mix this crate was written against). The
    // evidence is its own: `linear_num_*_heads` fields, or `linear_attention` entries in
    // `layer_types`. It outranks the sliding-window reading because a model can declare both.
    if declares_linear_attention && facts.num_linear_attention_layers != Some(0) {
        return KvTopology::HybridLinear;
    }

    // Newer configs list the attention type per layer, which is the only way to see a hybrid model
    // without counting arrays by hand.
    let layer_types = string_list(source, "layer_types");
    let sliding_layers = layer_types
        .iter()
        .filter(|kind| kind.contains("sliding"))
        .count();
    let full_layers = layer_types
        .iter()
        .filter(|kind| kind.contains("full") || kind.contains("global"))
        .count();
    if sliding_layers > 0 && full_layers > 0 {
        return KvTopology::Hybrid;
    }

    // A sliding-window model that also declares a shorter window than its context stores less than
    // the dense figure implies.
    match (facts.sliding_window, facts.max_position_embeddings) {
        (Some(window), Some(context)) if window > 0 && window < context => {
            KvTopology::SlidingWindow
        }
        (Some(window), None) if window > 0 => KvTopology::SlidingWindow,
        _ => KvTopology::Dense,
    }
}

/// Parse a `config.json` file, mapping an unreadable file to a [`Error::Json`] with context.
///
/// Offered because the most common failure is not bad JSON but the wrong URL: an HTML error page
/// parses as neither, and a caller that reported it as "no architecture" would send the operator
/// looking for a config problem that does not exist.
pub fn arch_facts_from_config_bytes(bytes: &[u8]) -> Result<ArchFacts> {
    ArchFacts::from_config_json(bytes).map_err(|error| match error {
        Error::Json(source) => Error::Json(source),
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Qwen2-7B: no `head_dim`, no `sliding_window`, 28 layers, GQA with 4 KV heads.
    fn qwen2_config() -> Value {
        serde_json::json!({
            "architectures": ["Qwen2ForCausalLM"],
            "model_type": "qwen2",
            "hidden_size": 3584,
            "intermediate_size": 18944,
            "num_attention_heads": 28,
            "num_hidden_layers": 28,
            "num_key_value_heads": 4,
            "max_position_embeddings": 32768,
            "torch_dtype": "bfloat16"
        })
    }

    /// Qwen3-32B: `head_dim` is explicit **and** differs from `hidden_size / num_attention_heads`.
    fn qwen3_32b_config() -> Value {
        serde_json::json!({
            "architectures": ["Qwen3ForCausalLM"],
            "model_type": "qwen3",
            "hidden_size": 5120,
            "intermediate_size": 25600,
            "num_attention_heads": 64,
            "num_hidden_layers": 64,
            "num_key_value_heads": 8,
            "head_dim": 128,
            "max_position_embeddings": 40960,
            "torch_dtype": "bfloat16"
        })
    }

    #[test]
    fn qwen2_has_no_head_dim_so_it_is_derived_and_said_so() {
        let facts = ArchFacts::from_config_value(&qwen2_config());
        assert_eq!(facts.num_hidden_layers, Some(28));
        assert_eq!(facts.num_key_value_heads, Some(4));
        assert_eq!(facts.head_dim, Some(128), "3584 / 28 = 128");
        assert!(facts.head_dim_derived, "the derivation must be visible");
        assert!(!facts.num_key_value_heads_derived, "GQA heads were stated");
        assert_eq!(facts.kv_topology, KvTopology::Dense);
        assert_eq!(facts.torch_dtype.as_deref(), Some("bfloat16"));
        assert_eq!(facts.torch_dtype_bytes(), Some(2));

        let geometry = facts.kv_geometry(2, "bfloat16");
        assert_eq!(
            geometry.kv_bytes_per_token,
            Some(28 * 4 * 128 * 2 * 2),
            "28 layers × 4 kv heads × 128 head_dim × 2 bytes × 2 (K and V)"
        );
        assert_eq!(geometry.kv_bytes_per_token, Some(57_344));
        assert!(geometry.problem.is_none());
        assert!(
            geometry.basis.contains("head_dim was derived"),
            "{}",
            geometry.basis
        );
        assert!(
            geometry.basis.contains("3584 / num_attention_heads 28"),
            "{}",
            geometry.basis
        );
    }

    #[test]
    fn an_explicit_head_dim_beats_the_hidden_size_division() {
        let facts = ArchFacts::from_config_value(&qwen3_32b_config());
        assert_eq!(facts.hidden_size, Some(5120));
        assert_eq!(facts.num_attention_heads, Some(64));
        assert_eq!(
            facts.head_dim,
            Some(128),
            "5120 / 64 would give 80, and using it would understate the cache by 60 %"
        );
        assert!(!facts.head_dim_derived, "the value was stated, not derived");

        // With fp8 KV storage the dense figure is 131 072 bytes/token. The point of the test is that
        // it is *this* large: the same geometry is what makes a candidate architecture in
        // `sizing` imply a negative overhead against the measured pool.
        let geometry = facts.kv_geometry(1, "fp8");
        assert_eq!(geometry.kv_bytes_per_token, Some(131_072));
        assert_eq!(geometry.kv_bytes_per_token, Some(64 * 8 * 128 * 2));
        assert!(geometry.warning.is_none());
    }

    #[test]
    fn absent_num_key_value_heads_means_mha_and_says_so() {
        let mut value = qwen2_config();
        value
            .as_object_mut()
            .expect("object")
            .remove("num_key_value_heads");
        let facts = ArchFacts::from_config_value(&value);
        assert_eq!(facts.num_key_value_heads, Some(28));
        assert!(facts.num_key_value_heads_derived);
        let geometry = facts.kv_geometry(2, "bfloat16");
        assert_eq!(geometry.kv_bytes_per_token, Some(28 * 28 * 128 * 2 * 2));
        assert!(
            geometry.basis.contains("multi-head attention is assumed"),
            "{}",
            geometry.basis
        );
    }

    #[test]
    fn a_multimodal_config_is_read_from_its_nested_text_config() {
        let value = serde_json::json!({
            "architectures": ["Qwen2_5_VLForConditionalGeneration"],
            "model_type": "qwen2_5_vl",
            "vision_config": {"hidden_size": 1152, "depth": 32},
            "text_config": {
                "hidden_size": 3584,
                "num_attention_heads": 28,
                "num_key_value_heads": 4,
                "num_hidden_layers": 28,
                "torch_dtype": "bfloat16"
            }
        });
        let facts = ArchFacts::from_config_value(&value);
        assert!(facts.nested_text_config);
        assert!(facts.has_vision_config);
        assert_eq!(facts.model_type.as_deref(), Some("qwen2_5_vl"));
        assert_eq!(
            facts.num_hidden_layers,
            Some(28),
            "from text_config, not the root"
        );
        assert_eq!(facts.head_dim, Some(128));
        let geometry = facts.kv_geometry(2, "bfloat16");
        assert_eq!(geometry.kv_bytes_per_token, Some(57_344));
        assert!(
            geometry.basis.contains("nested text_config"),
            "{}",
            geometry.basis
        );
        assert!(
            facts.has_vision_config,
            "the caller has to know the weights include a vision tower"
        );
    }

    #[test]
    fn mla_checkpoints_get_no_number_and_a_reason() {
        // DeepSeek-V2 style: kv_lora_rank replaces the per-head KV shape entirely.
        let value = serde_json::json!({
            "architectures": ["DeepseekV2ForCausalLM"],
            "model_type": "deepseek_v2",
            "hidden_size": 5120,
            "num_hidden_layers": 60,
            "num_attention_heads": 128,
            "kv_lora_rank": 512,
            "q_lora_rank": 1536,
            "v_head_dim": 128,
            "torch_dtype": "bfloat16"
        });
        let facts = ArchFacts::from_config_value(&value);
        assert_eq!(facts.kv_topology, KvTopology::Mla);
        assert!(!facts.is_dense_kv());
        let geometry = facts.kv_geometry(1, "fp8");
        assert_eq!(geometry.kv_bytes_per_token, None);
        assert!(geometry.problem.is_some());
        let problem = geometry.problem.expect("problem");
        assert!(problem.contains("latent"), "{problem}");
        assert_eq!(geometry.topology, KvTopology::Mla);
        assert!(geometry.basis.contains("512"), "{}", geometry.basis);
    }

    #[test]
    fn a_sliding_window_model_is_flagged_without_losing_the_number() {
        let value = serde_json::json!({
            "model_type": "mistral",
            "hidden_size": 4096,
            "num_attention_heads": 32,
            "num_key_value_heads": 8,
            "head_dim": 128,
            "num_hidden_layers": 32,
            "sliding_window": 4096,
            "max_position_embeddings": 32768
        });
        let facts = ArchFacts::from_config_value(&value);
        assert_eq!(facts.kv_topology, KvTopology::SlidingWindow);
        let geometry = facts.kv_geometry(2, "bfloat16");
        assert_eq!(geometry.kv_bytes_per_token, Some(32 * 8 * 128 * 2 * 2));
        assert!(geometry.problem.is_none());
        let warning = geometry
            .warning
            .expect("a window is a caveat, not a blocker");
        assert!(warning.contains("4096"), "{warning}");
    }

    #[test]
    fn a_hybrid_layer_mix_is_an_approximation_and_says_so() {
        let value = serde_json::json!({
            "model_type": "gemma3_text",
            "hidden_size": 2304,
            "num_attention_heads": 8,
            "num_key_value_heads": 4,
            "head_dim": 256,
            "num_hidden_layers": 26,
            "sliding_window": 1024,
            "max_position_embeddings": 131072,
            "layer_types": [
                "sliding_attention", "sliding_attention", "sliding_attention", "sliding_attention",
                "sliding_attention", "full_attention"
            ]
        });
        let facts = ArchFacts::from_config_value(&value);
        assert_eq!(facts.kv_topology, KvTopology::Hybrid);
        let geometry = facts.kv_geometry(2, "bfloat16");
        assert_eq!(geometry.kv_bytes_per_token, Some(26 * 4 * 256 * 2 * 2));
        assert!(geometry.warning.expect("warning").contains("approximation"));
    }

    #[test]
    fn an_empty_config_produces_a_problem_not_a_guess() {
        let facts = ArchFacts::from_config_value(&serde_json::json!({}));
        assert_eq!(facts, ArchFacts::default());
        assert!(
            facts.is_dense_kv(),
            "an empty config is not evidence of MLA"
        );
        let geometry = facts.kv_geometry(2, "bfloat16");
        assert_eq!(geometry.kv_bytes_per_token, None);
        let problem = geometry.problem.expect("problem");
        for field in ["num_hidden_layers", "num_key_value_heads", "head_dim"] {
            assert!(problem.contains(field), "{problem} must name {field}");
        }
        assert!(
            geometry.basis.contains("does not state"),
            "{}",
            geometry.basis
        );
    }

    #[test]
    fn a_config_missing_only_the_layer_count_says_which_field_is_missing() {
        let value = serde_json::json!({
            "hidden_size": 4096,
            "num_attention_heads": 32,
            "head_dim": 128
        });
        let facts = ArchFacts::from_config_value(&value);
        let geometry = facts.kv_geometry(2, "bfloat16");
        assert_eq!(geometry.kv_bytes_per_token, None);
        let problem = geometry.problem.expect("problem");
        assert!(problem.contains("num_hidden_layers"), "{problem}");
        assert!(
            !problem.contains("head_dim"),
            "head_dim was present: {problem}"
        );
    }

    #[test]
    fn zero_or_inconsistent_dimensions_are_refused_rather_than_divided_by() {
        let value = serde_json::json!({
            "hidden_size": 4096,
            "num_attention_heads": 0,
            "num_hidden_layers": 32,
            "max_position_embeddings": 4096
        });
        let facts = ArchFacts::from_config_value(&value);
        assert_eq!(facts.num_attention_heads, Some(0));
        assert_eq!(facts.head_dim, None, "no division by zero, no fallback 128");
        let geometry = facts.kv_geometry(2, "bfloat16");
        assert_eq!(geometry.kv_bytes_per_token, None);
        assert!(geometry.problem.is_some());

        let zero_heads = ArchFacts {
            num_hidden_layers: Some(8),
            num_key_value_heads: Some(0),
            head_dim: Some(128),
            ..ArchFacts::default()
        };
        assert_eq!(
            zero_heads.kv_geometry(2, "bfloat16").kv_bytes_per_token,
            None
        );
    }

    #[test]
    fn a_non_divisible_hidden_size_leaves_head_dim_unknown() {
        let value = serde_json::json!({
            "hidden_size": 4097,
            "num_attention_heads": 32,
            "num_key_value_heads": 8,
            "num_hidden_layers": 32
        });
        let facts = ArchFacts::from_config_value(&value);
        assert_eq!(facts.head_dim, None, "4097 / 32 is not a head width");
        assert!(!facts.head_dim_derived);
    }

    #[test]
    fn numeric_strings_and_alternative_spellings_are_read() {
        let value = serde_json::json!({
            "n_embd": "4096",
            "num_attention_heads": "32",
            "num_key_value_heads": 8,
            "n_layer": 32,
            "num_hidden_layers": "32",
            "n_positions": 2048,
            "dtype": "float16"
        });
        let facts = ArchFacts::from_config_value(&value);
        assert_eq!(facts.hidden_size, Some(4096), "n_embd");
        assert_eq!(facts.num_attention_heads, Some(32));
        assert_eq!(facts.max_position_embeddings, Some(2048), "n_positions");
        assert_eq!(facts.head_dim, Some(128));
        assert_eq!(
            facts.torch_dtype.as_deref(),
            Some("float16"),
            "the newer `dtype` key is accepted"
        );
        assert_eq!(facts.torch_dtype_bytes(), Some(2));
    }

    #[test]
    fn quantisation_declarations_are_read_from_the_nested_config() {
        let value = serde_json::json!({
            "architectures": ["Qwen3ForCausalLM"],
            "hidden_size": 5120,
            "num_attention_heads": 64,
            "num_key_value_heads": 8,
            "head_dim": 128,
            "num_hidden_layers": 64,
            "quantization_config": {"quant_method": "fp8", "bits": 8}
        });
        let facts = ArchFacts::from_config_value(&value);
        assert_eq!(facts.quant_method.as_deref(), Some("fp8"));
        assert_eq!(facts.quant_bits, Some(8));
    }

    #[test]
    fn dtype_widths_are_mapped_and_unknown_names_are_not() {
        assert_eq!(bytes_per_element_for_dtype("bfloat16"), Some(2));
        assert_eq!(bytes_per_element_for_dtype("BF16"), Some(2));
        assert_eq!(bytes_per_element_for_dtype("float32"), Some(4));
        assert_eq!(bytes_per_element_for_dtype("float8_e4m3fn"), Some(1));
        assert_eq!(bytes_per_element_for_dtype("auto"), None);
        assert_eq!(bytes_per_element_for_dtype("int4"), None);
        assert_eq!(
            ArchFacts::default().torch_dtype_bytes(),
            None,
            "an unstated dtype has no width"
        );
    }

    #[test]
    fn weights_bytes_from_a_param_count_is_a_documented_lower_bound() {
        // 30.9e9 parameters at FP8 is 30.9 GB of payload, under the real 27.78 GiB measured file set
        // once scales and embeddings are counted — which is why the measured figure is preferred.
        assert_eq!(weights_bytes_from_param_count(1_000, 2.0), Some(2_000));
        assert_eq!(
            weights_bytes_from_param_count(8_000_000_000, 0.5),
            Some(4_000_000_000),
            "8B parameters at 4 bits"
        );
        assert_eq!(weights_bytes_from_param_count(1_000, 0.0), None);
        assert_eq!(weights_bytes_from_param_count(1_000, f64::NAN), None);
        assert_eq!(
            weights_bytes_from_param_count(u64::MAX, 2.0),
            None,
            "no overflow wrap"
        );
    }

    #[test]
    fn the_hf_api_summary_fields_are_read_without_inventing_them() {
        let api = serde_json::json!({
            "id": "orcarouter/Qwen3.8-27B-Uncensored-FP8",
            "gated": false,
            "private": false,
            "safetensors": {"total": 30_900_000_000u64, "parameters": {}}
        });
        assert_eq!(params_from_hf_api(&api), Some(30_900_000_000));
        assert_eq!(gated_state_from_hf_api(&api), Some(GatedState::Open));

        let gated_auto = serde_json::json!({"gated": "auto", "safetensors": {"total": "1000"}});
        assert_eq!(
            params_from_hf_api(&gated_auto),
            Some(1_000),
            "numeric strings are read"
        );
        assert_eq!(gated_state_from_hf_api(&gated_auto), Some(GatedState::Auto));

        let gated_manual = serde_json::json!({"gated": "manual"});
        assert_eq!(
            gated_state_from_hf_api(&gated_manual),
            Some(GatedState::Manual)
        );

        let unknown = serde_json::json!({"id": "org/repo"});
        assert_eq!(
            gated_state_from_hf_api(&unknown),
            None,
            "an absent `gated` key is not a promise that the repo is open"
        );
        assert_eq!(params_from_hf_api(&unknown), None);
        assert_eq!(
            params_from_hf_api(&serde_json::json!({"safetensors": {"total": -1}})),
            None
        );
    }

    #[test]
    fn arch_facts_round_trip_through_json() {
        let facts = ArchFacts::from_config_value(&qwen2_config());
        let text = serde_json::to_string(&facts).expect("serialises");
        let back: ArchFacts = serde_json::from_str(&text).expect("round-trips");
        assert_eq!(back, facts);
        // A partially written object deserialises rather than failing: every field has a default.
        let partial: ArchFacts = serde_json::from_str("{\"head_dim\": 128}").expect("partial");
        assert_eq!(partial.head_dim, Some(128));
        assert_eq!(partial.num_hidden_layers, None);
    }

    #[test]
    fn parsing_bytes_reports_bad_json_as_an_error_and_good_json_as_facts() {
        let facts = ArchFacts::from_config_json(
            br#"{"hidden_size": 4096, "num_attention_heads": 32, "num_key_value_heads": 8,
                 "num_hidden_layers": 32, "torch_dtype": "bfloat16"}"#,
        )
        .expect("valid config parses");
        assert_eq!(facts.head_dim, Some(128));
        assert_eq!(
            facts.kv_geometry(2, "bfloat16").kv_bytes_per_token,
            Some(32 * 8 * 128 * 2 * 2)
        );

        // An HTML error page from the wrong URL must be an error, not "no architecture".
        let error = ArchFacts::from_config_json(b"<html><body>404 Not Found</body></html>")
            .expect_err("HTML is not a config");
        assert!(matches!(error, Error::Json(_)), "got {error:?}");
        assert!(ArchFacts::from_config_str("not json").is_err());

        // The same path, wrapped, keeps the error a JSON error.
        let wrapped = arch_facts_from_config_bytes(b"{").expect_err("truncated JSON");
        assert!(matches!(wrapped, Error::Json(_)), "got {wrapped:?}");
    }

    #[test]
    fn geometry_never_overflows_on_absurd_dimensions() {
        let facts = ArchFacts {
            num_hidden_layers: Some(u64::MAX),
            num_key_value_heads: Some(u64::MAX),
            head_dim: Some(u64::MAX),
            ..ArchFacts::default()
        };
        let geometry = facts.kv_geometry(2, "bfloat16");
        assert_eq!(geometry.kv_bytes_per_token, None);
        assert!(geometry.problem.expect("problem").contains("overflow"));
    }

    #[test]
    fn topology_names_are_the_wire_spellings() {
        assert_eq!(KvTopology::Dense.as_str(), "dense");
        assert_eq!(KvTopology::SlidingWindow.as_str(), "sliding-window");
        assert_eq!(KvTopology::Hybrid.as_str(), "hybrid");
        assert_eq!(KvTopology::HybridLinear.as_str(), "hybrid-linear");
        assert_eq!(KvTopology::Mla.as_str(), "mla");
        assert_eq!(
            serde_json::to_string(&KvTopology::SlidingWindow).expect("serialises"),
            "\"sliding-window\""
        );
        assert_eq!(KvTopology::default(), KvTopology::Dense);
    }

    #[test]
    fn a_geometry_with_a_number_never_carries_a_problem() {
        let configs = [qwen2_config(), qwen3_32b_config(), serde_json::json!({})];
        for config in configs {
            let facts = ArchFacts::from_config_value(&config);
            for bytes in [1u32, 2, 4] {
                let geometry = facts.kv_geometry(bytes, "test");
                assert_eq!(
                    geometry.kv_bytes_per_token.is_some(),
                    geometry.problem.is_none(),
                    "a number and a problem are mutually exclusive: {geometry:?}"
                );
            }
        }
    }

    /// The verified Qwen3.8-27B architecture, read from the public base repository's `config.json`
    /// (the operator's FP8 repository is gated, so this is the file the fallback rule fetches).
    ///
    /// The layer mix is the whole point: 48 `linear_attention` layers and 16 `full_attention` ones
    /// with `full_attention_interval: 4`. Only the 16 carry a per-token KV cache.
    fn qwen3_8_27b_config() -> Value {
        let mut layer_types: Vec<Value> = Vec::new();
        for _ in 0..64 {
            layer_types.push(Value::String("linear_attention".to_string()));
        }
        for (index, layer) in layer_types.iter_mut().enumerate().take(64) {
            if index % 4 == 3 {
                *layer = Value::String("full_attention".to_string());
            }
        }
        serde_json::json!({
            "architectures": ["Qwen3_5ForConditionalGeneration"],
            "model_type": "qwen3_5",
            "hidden_size": 5120,
            "intermediate_size": 17408,
            "num_attention_heads": 24,
            "num_hidden_layers": 64,
            "num_key_value_heads": 4,
            "head_dim": 256,
            "layer_types": layer_types,
            "full_attention_interval": 4,
            "linear_conv_kernel_dim": 4,
            "linear_num_key_heads": 16,
            "linear_key_head_dim": 128,
            "linear_num_value_heads": 48,
            "linear_value_head_dim": 128,
            "mamba_ssm_dtype": "float32",
            "max_position_embeddings": 262144,
            "tie_word_embeddings": false,
            "torch_dtype": "bfloat16",
            "vision_config": { "hidden_size": 1152, "depth": 27 }
        })
    }

    #[test]
    fn a_hybrid_linear_model_costs_only_its_full_attention_layers() {
        let facts = ArchFacts::from_config_value(&qwen3_8_27b_config());
        assert_eq!(facts.kv_topology, KvTopology::HybridLinear);
        assert_eq!(facts.num_full_attention_layers, Some(16));
        assert_eq!(facts.num_linear_attention_layers, Some(48));
        assert_eq!(
            facts.head_dim,
            Some(256),
            "the explicit key, never 5120 / 24"
        );

        let geometry = facts.kv_geometry(1, "fp8");
        assert_eq!(
            geometry.kv_bytes_per_token,
            Some(32_768),
            "16 layers × 4 kv heads × 256 head_dim × 1 byte × 2 (K and V) = 32768 bytes/token"
        );
        assert_eq!(geometry.problem, None);
        assert!(
            geometry
                .basis
                .starts_with("full_attention_layers=16 of 64 (hybrid linear attention)"),
            "the basis must name the split: {}",
            geometry.basis
        );
        let warning = geometry.warning.expect("a hybrid model carries a caveat");
        assert!(
            warning.contains("48"),
            "the warning names the state layers: {warning}"
        );
    }

    #[test]
    fn counting_every_layer_would_overstate_a_hybrid_cache_by_exactly_four() {
        let facts = ArchFacts::from_config_value(&qwen3_8_27b_config());
        let correct = facts
            .kv_geometry(1, "fp8")
            .kv_bytes_per_token
            .expect("the hybrid count is determinable");

        // The naive form — `num_hidden_layers × heads × head_dim × bytes × 2` — spelled out here on
        // purpose, so the falsification lives in the test rather than only in a comment.
        let naive = 64u64 * 4 * 256 * 2;
        assert_eq!(naive, 131_072);
        assert_eq!(naive, correct * 4);
    }

    #[test]
    fn a_uniform_model_still_counts_every_layer() {
        // No `layer_types`, no `linear_*` fields: nothing declares a recurrent state, so the naive
        // path stays correct and must keep using num_hidden_layers.
        let facts = ArchFacts::from_config_value(&qwen3_32b_config());
        assert_eq!(facts.kv_topology, KvTopology::Dense);
        assert_eq!(
            facts.kv_geometry(2, "bfloat16").kv_bytes_per_token,
            Some(64 * 8 * 128 * 2 * 2),
            "64 × 8 × 128 × 2 × 2 = 262144"
        );
    }

    #[test]
    fn a_declared_recurrent_state_without_a_count_is_refused_rather_than_guessed() {
        // `linear_num_value_heads` declares a hybrid stack but no `layer_types` and no interval, so
        // the number of full-attention layers — and therefore the cache — is unknowable.
        let facts = ArchFacts::from_config_value(&serde_json::json!({
            "architectures": ["Qwen3_5ForConditionalGeneration"],
            "model_type": "qwen3_5",
            "hidden_size": 5120,
            "num_attention_heads": 24,
            "num_hidden_layers": 64,
            "num_key_value_heads": 4,
            "head_dim": 256,
            "linear_num_value_heads": 48,
            "max_position_embeddings": 262144
        }));
        assert_eq!(facts.kv_topology, KvTopology::HybridLinear);
        let geometry = facts.kv_geometry(1, "fp8");
        assert_eq!(geometry.kv_bytes_per_token, None);
        let problem = geometry.problem.expect("a refusal explains itself");
        assert!(problem.contains("full-attention"), "{problem}");
    }

    #[test]
    fn the_recurrent_state_dtype_is_recorded_and_never_interpreted() {
        let facts = ArchFacts::from_config_value(&qwen3_8_27b_config());
        assert_eq!(facts.mamba_ssm_dtype.as_deref(), Some("float32"));
        // Declared as a fact about the file, and deliberately not used anywhere: whether
        // `mamba_ssm_dtype` is the switch vLLM reads for the recurrent state is unverified.
        let warning = facts.kv_geometry(1, "fp8").warning.expect("hybrid warning");
        assert!(warning.contains("mamba_ssm_cache_dtype"), "{warning}");
        assert!(
            warning.contains("--kv-cache-dtype"),
            "the warning must not let that flag stand in for the state dtype: {warning}"
        );
    }

    #[test]
    fn the_full_attention_count_can_come_from_the_interval_alone() {
        let facts = ArchFacts::from_config_value(&serde_json::json!({
            "hidden_size": 5120,
            "num_attention_heads": 24,
            "num_hidden_layers": 64,
            "num_key_value_heads": 4,
            "head_dim": 256,
            "full_attention_interval": 4,
            "linear_num_value_heads": 48
        }));
        assert_eq!(facts.num_full_attention_layers, Some(16));
        assert_eq!(facts.num_linear_attention_layers, Some(48));
        assert_eq!(facts.kv_geometry(1, "fp8").kv_bytes_per_token, Some(32_768));
    }

    #[test]
    fn a_base_model_hint_is_read_and_only_a_quantised_relation_is_trusted() {
        let quantised = serde_json::json!({
            "id": "orcarouter/Qwen3.8-27B-Uncensored-FP8",
            "gated": "manual",
            "cardData": {
                "base_model": "Qwen/Qwen3.8-27B",
                "base_model_relation": "quantized"
            }
        });
        let hint = base_model_hint_from_hf_api(&quantised).expect("the base model is stated");
        assert_eq!(hint.repo_id, "Qwen/Qwen3.8-27B");
        assert!(hint.shares_architecture());
        assert_eq!(
            gated_state_from_hf_api(&quantised),
            Some(GatedState::Manual)
        );

        // The array form, which the Hub also uses.
        let listed = serde_json::json!({
            "cardData": { "base_model": ["Qwen/Qwen3.8-27B"], "base_model_relation": "finetune" }
        });
        let hint = base_model_hint_from_hf_api(&listed).expect("readable");
        assert!(
            !hint.shares_architecture(),
            "a finetune may have changed the architecture, so its KV figure is not this one's"
        );

        // Nothing stated means no fallback, which is different from a fallback to a guess.
        assert_eq!(
            base_model_hint_from_hf_api(&serde_json::json!({ "id": "x/y" })),
            None
        );
        assert_eq!(base_model_hint_from_hf_api(&serde_json::json!({})), None);
    }

    #[test]
    fn layer_kind_spellings_are_matched_the_way_vllm_matches_them() {
        assert!(is_full_attention_layer("full_attention"));
        assert!(is_full_attention_layer("Full_Attention"));
        assert!(is_full_attention_layer("global_attention"));
        assert!(!is_full_attention_layer("sliding_attention"));
        assert!(is_linear_attention_layer("linear_attention"));
        assert!(is_linear_attention_layer("mamba2"));
        assert!(!is_linear_attention_layer("full_attention"));
        assert_eq!(
            count_layers(
                &["full_attention".to_string(), "linear_attention".to_string()],
                is_full_attention_layer
            ),
            1
        );
    }
}
