# vLLM KV-cache sizing and quantization research

Research for the in-progress `modal-vllm` provider (single-tenant vLLM on Modal) and its KV-token admission pool (`crates/manarouter-relay/src/kv_budget.rs`). All factual claims are traced to official vLLM docs/source or NVIDIA docs; nothing is guessed. Where a value is not documented, it is marked `?` or "unverified".

Primary sources used:
- vLLM Engine Arguments: https://docs.vllm.ai/en/stable/configuration/engine_args/
- vLLM Optimization and Tuning: https://docs.vllm.ai/en/latest/configuration/optimization/
- vLLM Conserving Memory: https://docs.vllm.ai/en/latest/configuration/conserving_memory/
- vLLM Quantized KV Cache: https://docs.vllm.ai/en/stable/features/quantization/quantized_kvcache/
- vLLM FP8 W8A8: https://docs.vllm.ai/en/stable/features/quantization/llm_compressor/fp8/
- vLLM Quantization overview + hardware matrix: https://docs.vllm.ai/en/stable/features/quantization/
- vLLM Hybrid KV Cache Manager: https://docs.vllm.ai/en/stable/design/hybrid_kv_cache_manager/
- vLLM source (current `main`): `vllm/config/cache.py`, `vllm/v1/kv_cache_interface.py`, `vllm/v1/core/kv_cache_planning.py`, `vllm/model_executor/layers/quantization/__init__.py`
- NVIDIA CUDA GPU Compute Capability: https://developer.nvidia.com/cuda/gpus
- NVIDIA RTX PRO 6000 Blackwell: https://www.nvidia.com/en-us/products/workstations/professional-desktop-gpus/rtx-pro-6000/
- NVIDIA/AWS A10G datasheet: https://d1.awsstatic.com/product-marketing/ec2/NVIDIA_AWS_A10G_DataSheet_FINAL_02_17_2022.pdf
- Startup log line examples (secondary, two independent sources): https://eurokommerz.at/blog/vllm-kv-cache-size-and-maximum-concurrency/ and https://github.com/vllm-project/vllm/issues/27017

---

## Corrections to what the Router code currently assumes

`crates/manarouter-relay/src/kv_budget.rs` models admission as a token pool: `pool_tokens` total, per-request `estimated_tokens`, plus `max_context` and `max_concurrent`. That shape is correct, but several constants the operator will plug in differ from current vLLM (V1) defaults:

| Assumption in doc/code | Current vLLM stable (V1) value | Source |
|---|---|---|
| `--gpu-memory-utilization` default 0.9 | **0.92** (`DEFAULT_GPU_MEMORY_UTILIZATION`) | `vllm/config/cache.py`; engine args |
| `--block-size` default 16 | **64 tokens** (`DEFAULT_BLOCK_SIZE = 64`) | `vllm/config/cache.py` |
| Per-token KV is uniform across layers | **Not guaranteed** — sliding-window / MLA / hybrid layers cache fewer tokens per token | `kv_cache_planning.py`, hybrid KV cache manager doc |
| `gpu_memory_utilization` is a pure KV-cache budget | It is the **executor memory budget** that must also fit weights, activations, CUDA graphs, tokenizer | Optimization + Conserving Memory docs |

---

## A. KV cache sizing

### A.1 Exact per-token KV formula

vLLM stores K and V paged in blocks. From `vllm/v1/kv_cache_interface.py` (`AttentionSpec`):

- `state_content_size_bytes = (head_size + head_size_v) * dtype_size` → K bytes + V bytes per head slot per token. `head_size_v` defaults to `head_size`.
- `unpadded_page_size_bytes = num_heads * num_states * state_content_size_bytes`; `num_states = tokens_per_state` (default 1 → one state per token).
- Per layer, per token: `num_kv_heads * head_dim * dtype_size * 2` (K and V), when `head_size_v == head_dim`.
- A layer's cache memory: `ceil(max_model_len / block_size) * block_size * num_kv_heads * head_dim * dtype_size * 2` (`max_memory_usage_bytes`), summed over all layers.
- Pool: `num_blocks = available_KV_bytes // bytes_per_block` where `bytes_per_block = block_size * bytes_per_token_total` (`kv_cache_planning.py`).

**Verified formula** (bytes per token, all layers, K+V):

```
bytes_per_token = num_layers * num_kv_heads * head_dim * dtype_bytes * 2
```

- `num_layers`: number of attention (**per-token KV-caching**) layers. **This is NOT simply HF `num_hidden_layers`.** It must be derived from `layer_types` when that key is present — see A.5, which documents a real model in this project where the naive reading is wrong by exactly **4×**. Use `num_hidden_layers` only when `layer_types` is absent.
- `num_kv_heads`: number of KV heads (`num_key_value_heads`).
- `head_dim`: per-head dimension (`head_dim`).
- `dtype_bytes`: bytes per element of the KV cache dtype (fp16/bf16 = 2, fp8 = 1, int8 = 1).
- factor **2** = one tensor for K and one for V.

Units: bytes per token (across the whole model). Multiply by `max_model_len` tokens and `block_size` rounding to get bytes; divide block bytes into the KV pool to get `num_blocks` and resident token capacity.

**Where `num_key_value_heads` and `head_dim` come from (HF `config.json`):** standard Hugging Face `transformers` convention:
- `num_key_value_heads`: explicit KV-head count for GQA/MQA. **Fallback when absent:** equals `num_attention_heads` (i.e., plain MHA, no grouped-query reduction).
- `head_dim`: explicit per-head dim. **Fallback when absent:** `hidden_size / num_attention_heads` (must divide evenly).

Confidence: **high** for the formula (directly from vLLM source); **high** for HF fallbacks (standard `transformers` convention; verified indirectly via vLLM model configs deriving `head_size` from `hidden_size // num_attention_heads`).

### A.2 How the CLI flags affect the KV pool and concurrency

| Flag | Effect on KV pool / concurrency | Source |
|---|---|---|
| `--gpu-memory-utilization` (default **0.92**) | Fraction of GPU memory vLLM treats as the executor budget. The KV pool is carved from this budget (after weights/activations/CUDA graphs in V1 extensible mode). Raising it gives more KV cache space; it is a **per-instance** limit, not cluster-wide. | engine args |
| `--max-model-len` | Caps context tokens per request. KV memory per layer scales with `ceil(max_model_len / block_size)`, so this directly scales pool bytes needed and reduces concurrent long requests. Auto-reduced (auto-fit) if it won't fit in available KV memory. | engine args + planning |
| `--max-num-seqs` | Maximum sequences admitted to RUNNING per iteration; sizes the model runner / per-request buffers and CUDA graphs and is the default admission limit. Bounds concurrent request count independently of token pool. | engine args |
| `--max-num-batched-tokens` | Max tokens the scheduler may issue in one iteration (prefill + decode). Bounds batch size; chunked prefill splits oversized prefills. Indirectly bounds concurrency via the token budget per iteration. | engine args + optimization |
| `--kv-cache-dtype` | Sets bytes per element of stored K/V (`auto` = model dtype, `fp8` = 1 byte). Smaller dtype → more tokens per GB → more concurrency. | engine args + quantized KV cache |
| `--block-size` (default **64**) | Contiguous cache block size in tokens. Pool is allocated in blocks; `num_blocks = pool_bytes // bytes_per_block`. Larger blocks → coarser admission granularity and more internal fragmentation for short requests. | engine args + cache.py |

Concurrency ceiling is `min(KV_pool_tokens / tokens_per_request, max_num_seqs)` (plus `max_num_batched_tokens` limiting batch token throughput). Preemption (RECOMPUTE by default in V1) frees space when the pool is full rather than hard-rejecting, but hurts latency.

### A.3 Startup log lines reporting KV size and concurrency

vLLM tracks `kv_cache_size_tokens` and `kv_cache_max_concurrency` on `CacheConfig` (`vllm/config/cache.py`). Observed startup log format (two independent secondary sources agree on the shape):

```
Available KV cache memory: 67.27 GiB
GPU KV cache size : 1,500,832 tokens
Maximum concurrency for 131,072 tokens per request : 11.45
```

and

```
GPU KV cache size : 643,232 tokens
Maximum concurrency for 40,960 tokens per request: 15.70x
```

Interpretation: "GPU KV cache size" = total resident tokens across all requests = KV pool bytes ÷ bytes_per_token_total. "Maximum concurrency for N tokens per request" = KV cache size ÷ N (e.g., 643,232 / 40,960 = 15.70×). Full-attention-only models match this division exactly; hybrid/sliding-window models do not.

Confidence: **medium** — format corroborated by two sources and the `CacheConfig` fields exist in source, but I did not capture the exact `logger.info` call site in primary source.

### A.4 `--kv-cache-dtype` accepted values and FP8

Accepted values (engine args, current stable): `auto` (default), `bfloat16`, `float16`, `fp8`, `fp8_ds_mla`, `fp8_e4m3`, `fp8_e5m2`, `fp8_inc`, `fp8_per_token_head`, `int4_per_token_head`, `int8_per_token_head`, `nvfp4`, `nvfp4_4over6`, `nvfp4_ds_mla`, `octave_k3v3`, `octave_k3v3_compact`, `octave_k3v4`, `turboquant_3bit_nc`, `turboquant_4bit_nc`, `turboquant_k3v4_nc`, `turboquant_k8v4`, `ultraquant_4bit`. Custom dtypes registerable via `register_kv_cache_dtype`.

**Real/common:** `auto`, `bfloat16`, `float16`, `fp8` (alias for `fp8_e4m3` on CUDA), `fp8_e4m3`, `fp8_e5m2`. `fp8_inc` is Intel Gaudi (HPU), not CUDA. The `octave_*`/`turboquant_*` are ROCm/experimental platform backends, not mainstream CUDA.

**What changes when fp8 is chosen:** bytes per element drops from 2 (fp16/bf16) to **1** for stored K/V (quantized KV cache), roughly halving KV memory and doubling resident token capacity; may cause accuracy drop without proper scaling factors. CUDA 11.8+ supports fp8 (`fp8_e4m3` and `fp8_e5m2`); ROCm supports fp8_e4m3; Intel Gaudi supports fp8 via `fp8_inc`. Source: engine args `kv_cache_dtype` help.

Note: FP8 *KV-cache storage* (CUDA 11.8+, incl. Ampere) is different from FP8 *GEMM/compute*, which needs native FP8 tensor cores (compute capability ≥ 8.9: Ada/Hopper/Blackwell); see matrix below.

### A.5 Sliding-window / hybrid / MLA layers — is per-token KV uniform?

**No.** The simple per-token formula is an approximation because KV accounting is layer-dependent:

- Full-attention layers reserve slots for all tokens up to `max_model_len` (uniform, matches the formula).
- Sliding-window attention layers (e.g., Mistral, Gemma-2's alternating pattern `1 sw : 1 full`) only cache the last `sliding_window` tokens per layer, so they hold far fewer tokens per token than full layers.
- MLA (DeepSeek-V2/V3/V4) compresses K/V into a shared latent vector, so its KV per token is much smaller than `num_kv_heads * head_dim * dtype * 2`.
- vLLM splits layers into KV cache groups by spec; uniform-spec models get one group; hybrid models (Gemma-2: 1 sw : 1 full; Gemma-3; Qwen3-Next; Mistral) get multiple groups with different per-token byte costs, and block sizes are aligned/padded to the common page (`kv_cache_planning.py` `get_kv_cache_groups`, `_ensure_min_page_size`).

Sources: Hybrid KV Cache Manager doc ("Full attention layers: reserve slots for all tokens"; Gemma-2 example), `kv_cache_planning.py` grouping logic, `vllm/v1/kv_cache_interface.py` `AttentionSpec` / `MLAAttentionSpec`.

Confidence: **high** (code + design doc).

**Correction — the "overestimates small layers" framing is too soft for a real case measured in this
project.** A hybrid model whose non-caching layers are the *majority* is not slightly overestimated; the
whole-model figure is wrong by the ratio of total layers to caching layers. On 2026-11-12 this was
confirmed against a real model: `Qwen/Qwen3.8-27B` (`model_type: "qwen3_5"`, verified by fetching
`https://huggingface.co/Qwen/Qwen3.8-27B/resolve/main/config.json` unauthenticated, HTTP 200) has:

```json
"num_hidden_layers": 64,
"layer_types": [ 48 x "linear_attention", 16 x "full_attention" ],   // every 4th layer full
"num_attention_heads": 24, "num_key_value_heads": 4,
"head_dim": 256, "hidden_size": 5120, "max_position_embeddings": 262144
```

Only the **16** `full_attention` layers cache per token. The 48 `linear_attention` layers (Gated
DeltaNet-style linear attention) hold a **fixed-size recurrent state that does not grow with sequence
length**, so they contribute nothing to `bytes_per_token`:

```
naive (num_hidden_layers): 64 * 4 * 256 * 1 * 2 = 131 072 bytes/token   <- 4x too high
correct (full_attention):  16 * 4 * 256 * 1 * 2 =  32 768 bytes/token
```

**Confirmed against vLLM source, not merely inferred from the config** (full trace:
`docs/research/hybrid-attention-kv.md`). `Attention.get_kv_cache_spec()` returns a `FullAttentionSpec`
(whose `max_memory_usage_bytes()` *grows* with `max_model_len`) for full-attention layers and a
`MambaSpec` (whose `max_memory_usage_bytes()` is **independent of sequence length**) for each GDN
`linear_attention` layer; `kv_cache_planning.py` sorts them into separate cache groups. `Attention.py`
names the Qwen3.5 hybrid case in a comment.

**"Contributes nothing" is true per token, but the state is not free.** The GDN state *is* charged
against the KV budget, so it does reduce the full-attention block count. From vLLM's
`gated_delta_net_state_shape()`: conv state `(head_k_dim*num_k_heads*2 + head_v_dim*num_v_heads,
conv_kernel_size-1)` = `(10 240, 3)` = 30 720 elements, plus recurrent state
`(num_v_heads, head_v_dim, head_k_dim)` = `(48, 128, 128)` = 786 432 elements → **817 152 elements per
layer** (~1.63 MB bf16, ~3.27 MB fp32), i.e. **~78.4 MB for all 48 layers** at `tp=1`. Against an ~8 GiB
overhead that is under 1 %: it is absorbed by the calibrated `overhead_bytes` constant rather than
modelled explicitly. Two corollaries: `--kv-cache-dtype fp8` covers the **full-attention KV only** —
the GDN state dtype is a separate switch (`mamba_cache_dtype` / `mamba_ssm_cache_dtype`, default `auto`
→ model dtype); and the HF field `mamba_ssm_dtype: "float32"` is **not confirmed** to map onto
`mamba_ssm_cache_dtype`, so nothing may claim to know the state dtype.

The token figure was then checked against two independent operator measurements on that model (RTX PRO
6000 and L40S, `--kv-cache-dtype fp8`): the architecture-derived 32 768 predicts both measured KV pools
to within **1.3 %** and **1.8 %** respectively. See `ARCHITECTURE-gpu-rentals.md` §7.1–7.2 for the full
table. **Rule: count `layer_types` entries equal to `full_attention` when the key exists.**

Two further traps in that same config, both real:
- **`head_dim` must be read explicitly.** The A.1 fallback `hidden_size / num_attention_heads` gives
  `5120 / 24 = 213.3` against a declared `head_dim` of 256. Prefer the explicit key; derive only when absent.
- **`max_position_embeddings` matched the operator's `--max-model-len 262144` exactly**, which is a
  useful cross-check that the config belongs to the model actually being served.

### A.6 VRAM overhead beyond weights

vLLM does **not** publish a fixed overhead percentage. `gpu_memory_utilization` is the single budget knob: vLLM pre-allocates the GPU cache from that fraction of memory, and it must also cover model weights, activation peak, CUDA-graph capture memory, tokenizer/CPU-side buffers, and (V1 extensible) what remains after warmup. The optimization and conserving-memory docs list CUDA graphs, tensor/pipeline parallelism (shards weights), and preemption as the levers; none gives a blanket % to add.

Guidance for the Router pool: size `kv_pool_tokens` from `gpu_memory_utilization × total_VRAM − estimated_weights_and_activations`, then apply a conservative safety margin. Risk: if overhead is underestimated the pool over-allocates and vLLM OOMs at allocation or preempts at runtime; `gpu_memory_utilization` is the budget, not a guarantee the KV cache alone may use all of it.

Confidence: **high** that no official % exists and `gpu_memory_utilization` is the budget knob; **low** for any specific overhead number (do not invent one).

---

## B. Quantization

### B.7 `--quantization` accepted values (current stable)

From `vllm/model_executor/layers/quantization/__init__.py` (`QuantizationMethods` Literal):

```
awq, auto_awq, fp8, w4afp8, fbgemm_fp8 [DEPRECATED], modelopt, modelopt_fp4,
modelopt_mxfp8, modelopt_int8, modelopt_mixed, auto_gptq, gptq, gptq_marlin,
awq_marlin, humming, compressed-tensors, experts_int8, quark, moe_wna16,
torchao, inc, mxfp4, gpt_oss_mxfp4, deepseek_v4_fp8, online
```

Online shorthands (also accepted, see `vllm.config.quantization` `_ONLINE_SHORTHANDS`):
`fp8_per_tensor, fp8_per_block, fp8_per_channel, int8_per_channel_weight_only,
nvfp4_per_token, mxfp8, lut_b_moe, mxfp4_hadamard32`

Deprecated / aliases: `fbgemm_fp8` is explicitly in `DEPRECATED_QUANTIZATION_METHODS`. `awq_marlin`/`gptq_marlin` map to `AutoAWQConfig`/`AutoGPTQConfig` with Marlin backend (aliases of awq/gptq using Marlin kernels). `--quantization` without a value falls back to the model config's `quantization_config` attribute, else unquantized. `--allow-deprecated-quantization` defaults to `False`.

Confidence: **high** (source Literal + deprecated list).

### B.8 Hardware support matrix (quantization method → GPU)

From vLLM's official quantization hardware table. Note vLLM's arch shorthand: Volta = SM 7.0, Turing = SM 7.5, Ampere = SM 8.0/8.6, Ada = SM 8.9, Hopper = SM 9.0. Blackwell is SM 10.x (not in vLLM's table shorthand; inferred from CUDA capability page).

| Method | Min CC | CUDA arch names | T4 (75) | L4 (89) | A10G (86) | L40S (89) | A100 (80) | H100 (90) | H200 (90) | B200 (100) | RTX PRO 6000 (120) | Notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| AWQ | 7.5 | Turing | ? | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ? | ? | vLLM table: Turing/Ampere/Ada/Hopper ✅ |
| GPTQ | 7.0 | Volta | ? | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ? | ? | vLLM table: all ✅ |
| Marlin (GPTQ/AWQ/FP8/FP4) | 7.5* | Turing* | ❌* | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ? | ? | *Turing no Marlin MXFP4 |
| llm-compressor INT8 W8A8 | 7.5 | Turing | ? | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ❌ | ❌ | |
| llm-compressor INT8 W4A8 | ? | ? | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | x86 CPU only per table |
| llm-compressor FP8 W8A8 | 8.9 | Ada | ❌ | ✅ | ❌ | ✅ | ❌ | ✅ | ✅ | ✅ | ? | |
| bitsandbytes | 7.0 | Volta | ? | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ? | ? | vLLM table ✅ all NVIDIA |
| GGUF | 7.0 | Volta | ? | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | via vLLM GGUF path |
| FP8 KV-cache storage (`--kv-cache-dtype fp8`) | 11.8 CUDA | CUDA 11.8+ | ✅ (store only) | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | storage, not GEMM |
| FP8 W8A8 compute | 8.9 | Ada | ❌ | ✅ | ❌ | ✅ | ❌ | ✅ | ✅ | ✅ | ? | native FP8 GEMM |

Legend: ✅ = vLLM table says supported, ❌ = not supported, `?` = unverified for that exact card, `*` = caveat. FP8 W8A8 on Turing/Ampere is weight-only W8A16 via FP8 Marlin (PR #5975 merged); native FP8 compute needs ≥ 8.9.

Sources: vLLM quantization hardware matrix; FP8 W8A8 doc ("FP8 computation is supported on NVIDIA GPUs with compute capability >= 8.9"; "Turing/Ampere GPUs are supported for W8A16 (weight-only FP8) utilizing FP8 Marlin"); PR #5975.

### B.9 GPU facts table (Modal card offerings)

| Card | VRAM | Compute capability | BF16 | FP8 tensor (native GEMM) | Source |
|---|---|---|---|---|---|
| T4 | 16 GB | sm75 (7.5) | ✅ | ❌ (FP8 storage only via CUDA 11.8; no native FP8 GEMM) | NVIDIA CUDA GPUs |
| L4 | 24 GB | sm89 (8.9) | ✅ | ✅ | NVIDIA CUDA GPUs |
| A10G | 24 GB | sm86 (8.6) | ✅ | ❌ native GEMM (FP8 Marlin weight-only) | NVIDIA CUDA GPUs (A10/sm86) + AWS datasheet (GA102, 24 GB) |
| L40S | 48 GB | sm89 (8.9) | ✅ | ✅ | NVIDIA CUDA GPUs |
| A100-40GB | 40 GB | sm80 (8.0) | ✅ | ❌ native GEMM (FP8 storage only) | NVIDIA CUDA GPUs |
| A100-80GB | 80 GB | sm80 (8.0) | ✅ | ❌ native GEMM (FP8 storage only) | NVIDIA CUDA GPUs |
| H100 | 80 GB | sm90 (9.0) | ✅ | ✅ | NVIDIA CUDA GPUs |
| H200 | 141 GB | sm90 (9.0) | ✅ | ✅ | NVIDIA CUDA GPUs |
| B200 | 188 GB (often rounded ~180) | sm100 (10.0) | ✅ | ✅ | NVIDIA CUDA GPUs (sm100); VRAM unverified from NVIDIA in this session |
| RTX PRO 6000 Blackwell | 96 GB | sm120 (12.0) | ✅ | ✅ (expect) | NVIDIA CUDA GPUs (sm120); NVIDIA product page (96 GB GDDR7) |

Marked `?`/unverified where I did not confirm the exact figure from NVIDIA in this session: B200 VRAM (188 GB is the widely cited NVIDIA figure), RTX PRO 6000 FP8 tensor support (expected on Blackwell, not directly confirmed), A10G compute capability (GA102 = sm86 per CUDA table listing A10 at 8.6; A10G is the data-center GA102 variant).

### B.10 Which quant methods are (a) supported by vLLM on each GPU and (b) present as pre-quantized HF weights

Combining B.8 with HF reality:

| Method | vLLM on T4/L4/A10G/L40S/A100/H100/H200/B200/RTX6000 | Pre-quantized HF weights commonly available? | Impossible / problematic combos |
|---|---|---|---|
| AWQ | ✅ on L4/A10G/L40S/A100/H100/H200; ❌ T4 (pre-Turing); ? B200/RTX6000 | ✅ very common (AutoAWQ) | AWQ on T4/pre-Ampere (no) |
| GPTQ | ✅ all NVIDIA (incl. T4) | ✅ common | none major |
| GPTQ/AWQ Marlin | ✅ L4/A10G/L40S/A100/H100/H200; ❌ T4 (no Marlin on pre-Turing); *Turing no MXFP4 | ✅ common | Marlin on T4 |
| FP8 W8A8 (compute) | ❌ T4/A10G/A100; ✅ L4/L40S/H100/H200/B200; ? RTX6000 | ✅ growing (e.g., Meta FP8 checkpoints) | FP8 weights on T4 (no native FP8) |
| FP8 weight-only / storage | ✅ all via CUDA 11.8 storage; compute only ≥ sm89 | ✅ | none |
| bitsandbytes (e.g., NF4/INT8) | ✅ all NVIDIA per table | ✅ common (NF4) | none |
| GGUF | ✅ all via vLLM GGUF path | ✅ (llama.cpp ecosystem) | GGUF is a llama.cpp-format path in vLLM, not a native CUDA quant; treat separately |
| int8_per_channel_weight_only / nvfp4_per_token / mxfp4 | online shorthands; nvfp4/mxfp4 need Blackwell (sm100+) | limited | nvfp4/mxfp4 on T4/L4/A100 (no Blackwell) |

Confidence: **medium** — matrix is official vLLM; HF "common availability" is my assessment, not a per-model HF crawl.

### B.11 On-the-fly / local re-quantization: reality check

vLLM **does not** offer a "load BF16 and one-click re-quantize to FP8/AWQ" path at serve time. Real options:

- **llm-compressor** (recommended by vLLM): offline recipe-driven quantization (FP8, INT8, INT4, W8A8, W4A16, W4A8). Needs calibration data, GPU (for calibration/inference), and time; produces a vLLM-ready checkpoint. Not click-a-button.
- **AutoAWQ / GPTQModel**: offline AWQ/GPTQ quantization. Needs calibration data + GPU, minutes to hours per model. Produces `--quantization awq`/`gptq` checkpoints.
- **vLLM online FP8** (`--quantization fp8_per_tensor/per_block/per_channel` etc.): applies quantization config at load time for supported layers; dynamic/static FP8. Constrained to layers vLLM can quantize online; not a full model re-quant of arbitrary BF16 to AWQ/GPTQ.
- **NVIDIA Model Optimizer (modelopt)**, **TorchAO**, **Quark**, **compressed-tensors**: offline tools producing vLLM-compatible checkpoints.

Verdict: re-quantization is an offline, data- and GPU-intensive operation with accuracy tradeoffs; it is not a vLLM runtime flag you flip on a BF16 deployment.

Confidence: **high** (vLLM docs point to LLM Compressor for "getting started with quantization"; no online re-quant feature documented).

---

## Open questions / unverified

- Exact `logger.info` call site and exact format string for the "GPU KV cache size / Maximum concurrency" startup log in current vLLM main (only secondary sources captured). Medium confidence on exact punctuation.
- Default `max_num_seqs` / `max_num_batched_tokens` numeric defaults (docs say "mainly for convenience... set in EngineArgs.create_engine_config"; not printed as fixed defaults).
- B200 VRAM exact official figure (188 GB widely cited, not confirmed from NVIDIA page in this session); RTX PRO 6000 FP8 tensor support (expected, not confirmed).
- Whether `--quantization bitsandbytes` / `gguf` are accepted on the CLI or only auto-detected from model `quantization_config` (they appear in vLLM's hardware matrix but not in the `QuantizationMethods` CLI Literal).
- Per-model HF pre-quantized weight availability (not crawled; marked as assessment).

---

## Implications for `modal-vllm` / `kv_budget.rs`

The Rust admission pool's token-based model is sound. To set `kv_pool_tokens` correctly on Modal, the operator should compute:

```
pool_bytes = gpu_memory_utilization * card_VRAM − weights − activations − CUDA_graph_headroom
caching_layers = count(layer_types == "full_attention") if layer_types present else num_hidden_layers
kv_bytes_per_token = caching_layers * num_kv_heads * head_dim * dtype_bytes * 2
kv_pool_tokens = floor((pool_bytes) / kv_bytes_per_token)   # then round down to block_size (64) multiple
```

and set `max_concurrent` ≤ `kv_pool_tokens / max_model_len` (per-request worst case), lower for
sliding-window/MLA models. Defaults to plug in: `gpu_memory_utilization = 0.92`, `block_size = 64`.

**Always count caching layers from `layer_types`.** Do not use `num_hidden_layers` directly: on the
hybrid model measured above it inflates `kv_bytes_per_token` by exactly 4× (131 072 vs 32 768) and so
under-reports capacity by 4×. `head_dim` must likewise be read explicitly rather than derived from
`hidden_size / num_attention_heads` (213.3 vs the true 256 on that model).

**Calibrated default for `overhead_bytes`:** 8 GiB, where `overhead_bytes` covers activations,
CUDA-graph capture, block padding and any fixed recurrent state, and excludes weights. Two independent
operator measurements on the model above imply 7.27 GiB and 7.85 GiB respectively, so treat the result
as accurate to roughly **±2 %**, not exactly. vLLM's own startup line (`GPU KV cache size: N tokens`)
remains authoritative.

**Obtaining the architecture for a gated model.** If `config.json` of the served repo is gated or
private, do not give up: read `cardData.base_model` from `GET /api/models/{repo_id}` (which is public
and requires no token, even for gated repos — see `huggingface-discovery.md`) and fetch `config.json`
from that base repo. That is how the gated `orcarouter/Qwen3.8-27B-Uncensored-FP8` was resolved to the
public `Qwen/Qwen3.8-27B`.
