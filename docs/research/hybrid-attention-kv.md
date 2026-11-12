# Research: vLLM hybrid KV cache for Qwen3-Next / Qwen3.5-style models

**Question:** When vLLM serves a hybrid model (48 `linear_attention` + 16 `full_attention` layers) with `--kv-cache-dtype fp8`, does the KV cache grow only for full-attention layers while linear-attention layers hold a fixed-size recurrent state?

**Short answer: YES, with caveats.** vLLM allocates a growing token KV cache only for full-attention layers. Each `linear_attention` (Gated DeltaNet) layer holds a fixed-size recurrent state (conv + SSM/temporal) that does **not** grow with sequence length. That fixed state **is** charged against the same GPU KV-cache memory budget (it reduces the number of full-attention blocks that fit). `--kv-cache-dtype fp8` applies to full-attention KV only; the GDN state dtype is controlled separately by `--mamba-cache-dtype`/`--mamba-ssm-cache-dtype`.

---

## 1. Does vLLM allocate KV cache only for full-attention layers in hybrid models?

**VERIFIED — yes, at the spec/manager level.**

vLLM builds one `KVCacheSpec` per attention-like layer, then groups layers by spec into separate `KVCacheGroupSpec`s. Full-attention layers get `FullAttentionSpec` (growing token cache); `linear_attention` (GDN) layers get `MambaSpec` (fixed-size state).

### Source: layer → spec construction

`vllm/model_executor/layers/attention/attention.py` — `Attention.get_kv_cache_spec()` returns a `FullAttentionSpec` for decoder full-attention layers:

> ```python
> else:
>     return FullAttentionSpec(
>         block_size=block_size,
>         num_kv_heads=self.num_kv_heads,
>         head_size=self.head_size,
>         head_size_v=self.head_size_v,
>         dtype=self.kv_cache_torch_dtype,
>         kv_quant_mode=quant_mode,
>     )
> ```
> File: `vllm/model_executor/layers/attention/attention.py`, `get_kv_cache_spec` (returns `FullAttentionSpec` for full-attention decoder layers). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/model_executor/layers/attention/attention.py)

The same file explicitly documents hybrid handling:

> "In hybrid models (e.g. Qwen3.5 / ColQwen3.5: GatedDeltaNet linear_attention interleaved with full_attention) the runner iterates every attention module to build the KV-cache spec..."
> File: `vllm/model_executor/layers/attention/attention.py`, comment inside `Attention.get_kv_cache_spec`. [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/model_executor/layers/attention/attention.py)

`vllm/model_executor/layers/mamba/gdn/qwen_gdn_linear_attn.py` — `QwenGatedDeltaNetAttention.get_kv_cache_spec()` returns a `MambaSpec` for each `linear_attention` layer:

> ```python
> def get_kv_cache_spec(self, vllm_config: VllmConfig) -> KVCacheSpec | None:
>     spec = super().get_kv_cache_spec(vllm_config)
>     ...
>     return replace(
>         spec,
>         num_prefill_checkpoint_blocks=1,
>         prefill_checkpoint_alignment=get_mamba_prefill_checkpoint_grid(vllm_config),
>     )
> ```
> where `super().get_kv_cache_spec()` (in `MambaBase`) builds:
> ```python
> return MambaSpec(
>     shapes=tuple(self.get_state_shape()),
>     dtypes=self.get_state_dtype(),
>     block_size=mamba_block_size,
>     ...
> )
> ```
> File: `vllm/model_executor/layers/mamba/mamba_utils.py` (`MambaBase.get_kv_cache_spec`) and `vllm/model_executor/layers/mamba/gdn/qwen_gdn_linear_attn.py` (`QwenGatedDeltaNetAttention.get_kv_cache_spec`). [GitHub raw mamba_utils](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/model_executor/layers/mamba/mamba_utils.py) [GitHub raw qwen_gdn_linear_attn](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/model_executor/layers/mamba/gdn/qwen_gdn_linear_attn.py)

### Source: grouping into separate KV cache groups

`vllm/v1/core/kv_cache_planning.py` — `_get_kv_cache_groups_uniform_page_size()` groups all layers by `kv_cache_spec`; layers with the same spec form one group:

> "Group all layers by kv_cache_spec. E.g., 2 full attention layers and 3 sliding window attention layers, -> (full.0, full.1), (sw.0, sw.1, sw.2)."
> File: `vllm/v1/core/kv_cache_planning.py`, `_get_kv_cache_groups_uniform_page_size` (~line 462). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/v1/core/kv_cache_planning.py)

`vllm/v1/kv_cache_spec_registry.py` — `KVCacheSpecRegistry.register()` maps each spec class to a manager class. FullAttentionSpec → FullAttentionManager; MambaSpec → MambaManager:

> ```python
> KVCacheSpecRegistry.register(
>     FullAttentionSpec, FullAttentionManager, uniform_type_base_spec=FullAttentionSpec,
> )
> KVCacheSpecRegistry.register(
>     MambaSpec, MambaManager, uniform_type_base_spec=MambaSpec
> )
> ```
> File: `vllm/v1/core/single_type_kv_cache_manager.py`, `register_all_kvcache_specs` (~lines 1420-1440). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/v1/core/single_type_kv_cache_manager.py)

### Source: growth behavior — FullAttentionSpec grows with sequence length; MambaSpec is fixed

`FullAttentionSpec.max_memory_usage_bytes()`:

> ```python
> return cdiv(max_model_len, self.block_size) * self.page_size_bytes
> ```
> File: `vllm/v1/kv_cache_interface.py`, `FullAttentionSpec.max_memory_usage_bytes` (~line 658). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/v1/kv_cache_interface.py)

This grows linearly with `max_model_len` (sequence length).

`MambaSpec.max_memory_usage_bytes()`:

> ```python
> def max_memory_usage_bytes(self, vllm_config: VllmConfig) -> int:
>     if vllm_config.cache_config.mamba_cache_mode == "align":
>         return self.page_size_bytes * (2 + self.num_speculative_blocks + self.num_prefill_checkpoint_blocks)
>     else:
>         return self.page_size_bytes * (1 + self.num_speculative_blocks)
> ```
> File: `vllm/v1/kv_cache_interface.py`, `MambaSpec.max_memory_usage_bytes` (~line 1172). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/v1/kv_cache_interface.py)

This is **independent of `max_model_len` / sequence length** — it depends only on the fixed state size and speculative/checkpoint blocks.

`MambaSpec.page_size_bytes` = `state_content_size_bytes` (the fixed state size), not a per-token page:

> ```python
> @property
> def page_size_bytes(self) -> int:
>     page_size = self.state_content_size_bytes
>     ...
>     return page_size
> ```
> File: `vllm/v1/kv_cache_interface.py`, `MambaSpec.page_size_bytes` (~line 1161). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/v1/kv_cache_interface.py)

### Confirmation that Qwen3-Next full-attention layers use `Attention` (not a custom backend)

`vllm/model_executor/models/qwen3_next.py` — `Qwen3NextAttention.__init__` constructs a standard `vllm.model_executor.layers.attention.Attention` layer:

> ```python
> self.attn = Attention(
>     self.num_heads,
>     self.head_dim,
>     self.scaling,
>     num_kv_heads=self.num_kv_heads,
>     cache_config=cache_config,
>     quant_config=quant_config,
>     prefix=f"{prefix}.attn",
>     attn_type=attn_type,
> )
> ```
> File: `vllm/model_executor/models/qwen3_next.py`, `Qwen3NextAttention.__init__` (~line 357). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/model_executor/models/qwen3_next.py)

And `Qwen3NextDecoderLayer` branches on `layer_type`:

> ```python
> if self.layer_type == "linear_attention":
>     self.linear_attn = QwenGatedDeltaNetAttention(...)
> elif self.layer_type == "full_attention":
>     self.self_attn = Qwen3NextAttention(...)
> ```
> File: `vllm/model_executor/models/qwen3_next.py`, `Qwen3NextDecoderLayer.__init__` (~lines 515-523). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/model_executor/models/qwen3_next.py)

---

## 2. Is the linear-attention state fixed-size per sequence? What is its size?

**VERIFIED — yes, fixed-size; exact formula confirmed from source.**

The GDN state consists of two tensors: a conv state and a recurrent (SSM/temporal) state. Neither depends on sequence length.

### Source: state shape formula

`vllm/model_executor/layers/mamba/mamba_utils.py` — `MambaStateShapeCalculator.gated_delta_net_state_shape()`:

> ```python
> @classmethod
> def gated_delta_net_state_shape(
>     cls,
>     tp_world_size: int,
>     num_k_heads: int,
>     num_v_heads: int,
>     head_k_dim: int,
>     head_v_dim: int,
>     conv_kernel_size: int,
>     num_spec: int = 0,
> ):
>     conv_dim = head_k_dim * num_k_heads * 2 + head_v_dim * num_v_heads
>     conv_state_shape = cls._orient_conv_shape(
>         divide(conv_dim, tp_world_size),
>         conv_kernel_size - 1 + num_spec,
>     )
>     temporal_state_shape = (
>         divide(num_v_heads, tp_world_size),
>         head_v_dim,
>         head_k_dim,
>     )
>     return conv_state_shape, temporal_state_shape
> ```
> File: `vllm/model_executor/layers/mamba/mamba_utils.py`, `gated_delta_net_state_shape` (~line 280). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/model_executor/layers/mamba/mamba_utils.py)

`QwenGatedDeltaNetAttention.get_state_shape()` delegates to this (confirmed at ~line 684 of `qwen_gdn_linear_attn.py` on main).

### Source: state dtype formula

`vllm/model_executor/layers/mamba/mamba_utils.py` — `MambaStateDtypeCalculator.gated_delta_net_state_dtype()`:

> ```python
> @classmethod
> def gated_delta_net_state_dtype(cls, model_dtype, mamba_cache_dtype, mamba_ssm_cache_dtype="auto"):
>     return cls._mamba_state_dtype(model_dtype, mamba_cache_dtype, mamba_ssm_cache_dtype)
> ```
> where `_mamba_state_dtype` returns `(conv_state_dtype, temporal_state_dtype)`:
> - `conv_state_dtype = get_kv_cache_torch_dtype(mamba_cache_dtype, model_dtype)`
> - if `mamba_ssm_cache_dtype == "auto"`: `temporal_state_dtype = conv_state_dtype`
>   else: `temporal_state_dtype = STR_DTYPE_TO_TORCH_DTYPE[mamba_ssm_cache_dtype]`
> File: `vllm/model_executor/layers/mamba/mamba_utils.py`, `gated_delta_net_state_dtype` (~line 130) and `_mamba_state_dtype` (~line 106). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/model_executor/layers/mamba/mamba_utils.py)

### Numeric computation for the user's config

Config values:
- `linear_num_key_heads` = 16
- `linear_num_value_heads` = 48
- `linear_key_head_dim` = 128
- `linear_value_head_dim` = 128
- `linear_conv_kernel_dim` = 4
- `mamba_ssm_dtype` = "float32" (from config.json) — note: this is the HF config field; vLLM maps it through `mamba_cache_dtype`/`mamba_ssm_cache_dtype`

`conv_dim = head_k_dim × num_k_heads × 2 + head_v_dim × num_v_heads = 128 × 16 × 2 + 128 × 48 = 4,096 + 6,144 = 10,240`

**Conv state shape** (per TP rank, SD layout, num_spec=0):
- `(conv_dim ÷ tp, conv_kernel_size − 1) = (10,240 ÷ tp, 3)`
- Elements per layer: `10,240 × 3 ÷ tp = 30,720 ÷ tp`

**Recurrent (temporal) state shape** (per TP rank):
- `(num_v_heads ÷ tp, head_v_dim, head_k_dim) = (48 ÷ tp, 128, 128)`
- Elements per layer: `48 × 128 × 128 ÷ tp = 786,432 ÷ tp`

**Total elements per linear-attention layer (tp=1):** 30,720 + 786,432 = **817,152 elements**

**State size per linear-attention layer:**
- At 2 bytes/element (bfloat16, if `mamba_cache_dtype="auto"` → model dtype = bf16): 817,152 × 2 = **1,634,304 bytes ≈ 1.63 MB**
- At 4 bytes/element (float32): 817,152 × 4 = **3,268,608 bytes ≈ 3.27 MB**

**For all 48 linear-attention layers (tp=1, bf16):** 48 × 1,634,304 = **78,446,592 bytes ≈ 78.4 MB** (fixed, regardless of sequence length)

Note: `num_spec` (speculative tokens) adds `num_spec` to the conv state length (`conv_kernel_size - 1 + num_spec`), increasing the conv state slightly when speculative decoding is enabled.

### The user's guess was partially correct

The user guessed `linear_num_value_heads × linear_value_head_dim × linear_key_head_dim × mamba_ssm_dtype`. That matches the **recurrent/temporal state** shape `(48, 128, 128)` but **omits the conv state** `(10,240, 3)` from `linear_conv_kernel_dim` = 4 (kernel − 1 = 3 time steps × conv_dim 10,240). The conv state is ~4% of the recurrent state but is real and allocated.

---

## 3. Does the fixed linear-attention state consume GPU memory that reduces the KV budget?

**VERIFIED — yes.** The mamba/GDN state is charged against the same GPU KV-cache memory budget.

### Source: bytes_per_block includes mamba groups

`vllm/v1/core/kv_cache_planning.py` — `_get_kv_cache_bytes_per_block()` computes the per-block bytes as the max over groups of the sum of each layer's `page_size_bytes`:

> ```python
> bytes_per_block = max(
>     sum(
>         _get_per_layer_spec(group, layer_name).page_size_bytes
>         for layer_name in group.layer_names
>     )
>     for group in kv_cache_groups
> )
> ```
> Then `num_blocks = available_memory // bytes_per_block`.
> File: `vllm/v1/core/kv_cache_planning.py`, `_get_kv_cache_bytes_per_block` (~line 620) and `get_kv_cache_config_from_groups` (~line 724). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/v1/core/kv_cache_planning.py)

Since `MambaSpec.page_size_bytes = state_content_size_bytes` (the full fixed state), each mamba group's layers contribute their full fixed state size to `bytes_per_block`. This directly reduces `num_blocks`, which reduces the number of full-attention token blocks that can be allocated. The GDN state therefore **reduces the KV budget** available for full-attention layers.

### Source: per-request block accounting is group-aware

`vllm/v1/core/kv_cache_utils.py` — `get_max_concurrency_for_kv_cache_config()` sums `max_memory_usage_bytes / page_size_bytes` over **all** groups (full-attention and mamba):

> ```python
> for group in kv_cache_config.kv_cache_groups:
>     required = cdiv(
>         group.kv_cache_spec.max_memory_usage_bytes(vllm_config),
>         group.kv_cache_spec.page_size_bytes,
>     )
>     ...
>     num_blocks_per_request += required
> ```
> File: `vllm/v1/core/kv_cache_utils.py`, `get_max_concurrency_for_kv_cache_config` (~line 1082). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/v1/core/kv_cache_utils.py)

For mamba groups (non-align mode), `max_memory_usage_bytes / page_size_bytes = 1 + num_speculative_blocks` — a fixed per-request cost, not growing with sequence length, but still consuming a block-table slot from the shared pool.

### Source: KV cache zeroing flag includes mamba layers

`KVCacheConfig.has_mamba_layers` detects MambaSpec groups, and `__post_init__` sets `needs_kv_cache_zeroing` accordingly:

> ```python
> self.needs_kv_cache_zeroing = (
>     self.has_mamba_layers or self.has_mixed_precision_kv_cache
> )
> ```
> File: `vllm/v1/kv_cache_interface.py`, `KVCacheConfig.__post_init__` (~line 1606) and `has_mamba_layers` (~line 1680). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/v1/kv_cache_interface.py)

---

## 4. What does vLLM report on startup and in `CacheConfig`?

**VERIFIED — the startup log reports group-aware token capacity; `kv_cache_size_tokens` is group-aware.**

### Source: startup log line

`vllm/v1/core/kv_cache_utils.py` — `update_kv_cache_capacity()`:

> ```python
> logger.info_once(
>     "%s KV cache size: %s tokens, "
>     "Maximum concurrency for %s tokens per request: %.2fx",
>     pools,
>     f"{num_tokens:,}",
>     f"{max_model_len:,}",
>     max_concurrency,
> )
> ```
> where `num_tokens = int(max_concurrency * max_model_len)` from `get_kv_cache_capacity()` → `get_max_concurrency_for_kv_cache_config()`.
> File: `vllm/v1/core/kv_cache_utils.py`, `update_kv_cache_capacity` (~line 1157). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/v1/core/kv_cache_utils.py)

### Source: `kv_cache_size_tokens` field and readiness response

`vllm/config/cache.py` — `kv_cache_size_tokens` is `init=False`, set after profiling:

> "Per-DP-engine KV cache capacity in tokens (group-aware). Uses group-aware capacity since num_gpu_blocks * block_size can be wrong for hybrid models where requests occupy multiple KV cache groups."
> File: `vllm/config/cache.py`, `kv_cache_size_tokens` field doc (~line 347). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/config/cache.py)

`vllm/v1/engine/core.py` — `_make_ready_response()` reports it to the frontend:

> ```python
> kv_cache_size_tokens=self.vllm_config.cache_config.kv_cache_size_tokens,
> num_gpu_blocks=self.vllm_config.cache_config.num_gpu_blocks or 0,
> ```
> File: `vllm/v1/engine/core.py`, `_make_ready_response` (~line 2120). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/v1/engine/core.py)

**Does the token count count only full-attention layers?** The `num_tokens` figure is `max_concurrency × max_model_len`, where `max_concurrency` is `num_blocks / num_blocks_per_request` summed over **all** groups. Because mamba groups contribute a fixed per-request block cost (1 + speculative blocks), the effective token capacity is reduced proportionally. So the reported "GPU KV cache size: N tokens" is a **group-aware capacity that accounts for both full-attention blocks and mamba state blocks** — it is not purely full-attention, but the mamba contribution is fixed per request rather than per token.

---

## 5. Is `--kv-cache-dtype fp8` supported for full-attention layers of this hybrid architecture?

**VERIFIED — yes for full-attention; GDN state dtype is controlled separately.**

### Source: `kv_cache_dtype` applies to `AttentionSpec` (full-attention) only

`vllm/config/cache.py` — `CacheConfig.cache_dtype`:

> "Data type for kv cache storage. If 'auto', will use model data type. ... CUDA 11.8+ supports fp8 (=fp8_e4m3) and fp8_e5m2."
> File: `vllm/config/cache.py`, `cache_dtype` field (~line 238). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/config/cache.py)

`Attention.__init__` resolves `self.kv_cache_torch_dtype` from `cache_config.cache_dtype` (via `kv_cache_dtype_str_to_dtype`), and `Attention.get_kv_cache_spec()` passes `dtype=self.kv_cache_torch_dtype` into `FullAttentionSpec`. File: `vllm/model_executor/layers/attention/attention.py`. [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/model_executor/layers/attention/attention.py)

### Source: mamba/GDN state dtype is separate (`mamba_cache_dtype`, not `kv_cache_dtype`)

`vllm/config/cache.py` — `mamba_cache_dtype` and `mamba_ssm_cache_dtype` are distinct fields:

> "The data type to use for the Mamba cache (both the conv as well as the ssm state). If set to 'auto', the data type will be inferred from the model config." (`mamba_cache_dtype`)
> "The data type to use for the Mamba cache (ssm state only, conv state will still be controlled by mamba_cache_dtype)." (`mamba_ssm_cache_dtype`)
> File: `vllm/config/cache.py`, `mamba_cache_dtype` (~line 299) and `mamba_ssm_cache_dtype` (~line 303). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/config/cache.py)

`MambaStateDtypeCalculator.gated_delta_net_state_dtype()` uses `mamba_cache_dtype` and `mamba_ssm_cache_dtype` — **not** `kv_cache_dtype`. File: `vllm/model_executor/layers/mamba/mamba_utils.py` (~line 130). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/model_executor/layers/mamba/mamba_utils.py)

`Qwen3NextForCausalLM.get_mamba_state_dtype_from_config()`:

> ```python
> return MambaStateDtypeCalculator.gated_delta_net_state_dtype(
>     vllm_config.model_config.dtype,
>     vllm_config.cache_config.mamba_cache_dtype,
>     vllm_config.cache_config.mamba_ssm_cache_dtype,
> )
> ```
> File: `vllm/model_executor/models/qwen3_next.py` (~line 897). [GitHub raw](https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/model_executor/models/qwen3_next.py)

**Conclusion:** `--kv-cache-dtype fp8` sets the full-attention KV cache to fp8. The GDN linear-attention state uses `--mamba-cache-dtype` (default "auto" → model dtype, typically bfloat16) and `--mamba-ssm-cache-dtype` independently. vLLM does **not** force fp8 on the GDN state, nor does it force the GDN state to a different dtype because of `--kv-cache-dtype fp8`.

There is also `kv_cache_dtype_skip_layers` (layer patterns to skip KV cache quantization), but that is user-controlled and not automatic for hybrid models.

---

## Exact formulas

### KV bytes per token (full-attention layers only)

```
bytes_per_token = num_full_layers × num_kv_heads × head_dim × fp8_bytes × 2
                = 16 × 4 × 256 × 1 × 2
                = 32,768 bytes/token
```

The naive all-64-layers figure (131,072 bytes/token) is **wrong** for this hybrid model because the 48 linear-attention layers do not have a per-token KV cache.

### GDN state size per linear-attention layer

```
conv_dim = head_k_dim × num_k_heads × 2 + head_v_dim × num_v_heads
         = 128 × 16 × 2 + 128 × 48
         = 10,240

conv_state_elements     = (conv_dim ÷ tp) × (conv_kernel_size − 1 + num_spec)
temporal_state_elements = (num_v_heads ÷ tp) × head_v_dim × head_k_dim

total_elements_per_layer = conv_state_elements + temporal_state_elements
                          = (10,240 ÷ tp) × 3 + (48 ÷ tp) × 128 × 128   (num_spec=0)
                          = (30,720 + 786,432) ÷ tp
                          = 817,152 ÷ tp

bytes_per_layer = total_elements_per_layer × dtype_size
```

With tp=1 and bfloat16 (2 bytes): **1,634,304 bytes ≈ 1.63 MB per linear-attention layer**
With tp=1 and float32 (4 bytes): **3,268,608 bytes ≈ 3.27 MB per linear-attention layer**

For all 48 linear-attention layers (tp=1, bf16): **≈ 78.4 MB fixed** (sequence-length independent).

---

## Summary of verification status

| Claim | Status | Basis |
|-------|--------|-------|
| KV cache allocated only for full-attention layers; GDN layers get fixed MambaSpec | **VERIFIED** | `attention.py` `get_kv_cache_spec` → `FullAttentionSpec`; `mamba_utils.py`/`qwen_gdn_linear_attn.py` → `MambaSpec`; `kv_cache_planning.py` groups by spec |
| Full-attention `max_memory_usage_bytes` grows with `max_model_len` | **VERIFIED** | `kv_cache_interface.py` `FullAttentionSpec.max_memory_usage_bytes` |
| GDN `max_memory_usage_bytes` independent of sequence length | **VERIFIED** | `kv_cache_interface.py` `MambaSpec.max_memory_usage_bytes` |
| GDN state shape formula | **VERIFIED** | `mamba_utils.py` `gated_delta_net_state_shape` |
| GDN state dtype formula | **VERIFIED** | `mamba_utils.py` `gated_delta_net_state_dtype` |
| GDN state charged against KV budget (reduces num_blocks) | **VERIFIED** | `kv_cache_planning.py` `_get_kv_cache_bytes_per_block`; `kv_cache_utils.py` `get_max_concurrency_for_kv_cache_config` |
| Startup log reports group-aware token capacity | **VERIFIED** | `kv_cache_utils.py` `update_kv_cache_capacity`; `cache.py` `kv_cache_size_tokens` doc |
| `--kv-cache-dtype fp8` applies to full-attention only | **VERIFIED** | `cache.py` `cache_dtype`; `attention.py` `kv_cache_torch_dtype` → `FullAttentionSpec.dtype` |
| GDN state dtype controlled by `mamba_cache_dtype`/`mamba_ssm_cache_dtype`, not `kv_cache_dtype` | **VERIFIED** | `cache.py` `mamba_cache_dtype`/`mamba_ssm_cache_dtype`; `mamba_utils.py` `gated_delta_net_state_dtype`; `qwen3_next.py` `get_mamba_state_dtype_from_config` |
| Exact numeric GDN state size for this config | **COMPUTED from verified formula** | 1.63 MB/layer (bf16, tp=1); ~78.4 MB for 48 layers |
| Whether Qwen3-Next's HF `mamba_ssm_dtype="float32"` maps to vLLM `mamba_ssm_cache_dtype` | **NOT CONFIRMED** | The HF config field `mamba_ssm_dtype` was seen in config.json but I did not trace how vLLM's `VerifyAndUpdateConfig` reads it into `mamba_ssm_cache_dtype` for Qwen3-Next specifically |

---

## Primary sources fetched

All fetched via `curl -sL` from `raw.githubusercontent.com/vllm-project/vllm/main/`:

1. `vllm/model_executor/layers/attention/attention.py` — `Attention.get_kv_cache_spec` (FullAttentionSpec construction; hybrid-model comment)
2. `vllm/model_executor/layers/mamba/mamba_utils.py` — `MambaBase.get_kv_cache_spec`, `MambaStateShapeCalculator.gated_delta_net_state_shape`, `MambaStateDtypeCalculator.gated_delta_net_state_dtype`
3. `vllm/model_executor/layers/mamba/gdn/qwen_gdn_linear_attn.py` — `QwenGatedDeltaNetAttention.get_kv_cache_spec`, `get_state_shape`
4. `vllm/model_executor/models/qwen3_next.py` — `Qwen3NextDecoderLayer` layer-type branching, `Qwen3NextAttention` construction, `Qwen3NextForCausalLM.get_mamba_state_dtype_from_config`/`get_mamba_state_shape_from_config`
5. `vllm/v1/kv_cache_interface.py` — `FullAttentionSpec`, `MambaSpec`, `KVCacheConfig` (`has_mamba_layers`, `needs_kv_cache_zeroing`), `AttentionSpec.page_size_bytes`
6. `vllm/v1/kv_cache_spec_registry.py` — `KVCacheSpecRegistry.register`/`get_manager_class`
7. `vllm/v1/core/single_type_kv_cache_manager.py` — `FullAttentionManager`, `MambaManager`, spec registrations, `MambaManager.get_num_blocks_to_allocate`
8. `vllm/v1/core/kv_cache_planning.py` — `_get_kv_cache_groups_uniform_page_size`, `_get_kv_cache_bytes_per_block`, `get_kv_cache_config_from_groups`
9. `vllm/v1/core/kv_cache_utils.py` — `update_kv_cache_capacity`, `get_max_concurrency_for_kv_cache_config`
10. `vllm/config/cache.py` — `CacheConfig.cache_dtype`, `mamba_cache_dtype`, `mamba_ssm_cache_dtype`, `kv_cache_size_tokens`
11. `vllm/v1/engine/core.py` — `_make_ready_response` (reports `num_gpu_blocks`, `kv_cache_size_tokens`)
12. `vllm/model_executor/layers/attention_layer_base.py` — `AttentionLayerBase` (abstract `get_kv_cache_spec`)
13. `vllm/utils/torch_utils.py` — `get_kv_cache_torch_dtype` ("auto" → model dtype)
14. `vllm/v1/worker/gpu/model_states/mamba_hybrid.py` — `MambaHybridModelState` (hybrid model state)
15. `vllm/v1/worker/gpu/model_states/__init__.py` — `init_model_state` selects `MambaHybridModelState` when `vllm_config.model_config.is_hybrid`

**File created:** `docs/research/hybrid-attention-kv.md` (this document). No other files were modified.

**No files were modified in the repository.** This is read-only research; findings are reported only.