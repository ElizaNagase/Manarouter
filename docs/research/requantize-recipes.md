# Requantisation recipes — what is actually possible when the operator asks for a quant the repo lacks

Research for `docs/research/ARCHITECTURE-gpu-rentals.md` §8, **Requirement 6**. Organised so that each
section maps to one field of the `requantize` JSON object: `tool`, `steps[]`, `estimated_minutes`,
`requires_vram_bytes`, `requires_extra_vram_bytes`, `feasible_on_gpu`, `note`.

Retrieval date: **2026-11-12**. Everything below is either quoted from an official doc/source file with
its URL, or explicitly marked **`[reasoned]`** (derived by arithmetic from a stated input) or
**`[unverified]`** (could not be established). No command in this document was executed; nothing here
is a measured claim about this machine.

Two room-clearing definitions, because the whole report depends on them:

- **Requantise** = produce a checkpoint in a *different* quantisation format from a checkpoint that is
  already quantised (here: FP8 → 4-bit integer).
- **Quantise** = produce a quantised checkpoint from a *dense* (BF16/FP16) checkpoint.

The operator's requirement — *"If the user asks for a quant that doesn't exist in the repo, an error
should show up, with the option to locally requantize from highest available quantization"* — is a
request for the first thing. The first thing is possible, but not in the form §8 assumes.

---

## 0. The four facts that decide the design

**0.1 You cannot requantise FP8 → 4-bit in one step without passing through dense weights.**
This is the single most important fact in this report. `llm-compressor`'s own converter
documentation describes the compressed-tensors → different-scheme case as *"A compressed-tensors
checkpoint that needs to be re-quantized under a different scheme, **requiring a round-trip through
dense weights first**"*
([converter guide](https://docs.vllm.ai/projects/llm-compressor/en/latest/guides/entrypoints/convert/)).
There is no 1-bit-per-param → 0.5-bit-per-param arithmetic shortcut: 4-bit group scales have to be
computed from real numbers, and FP8 has already thrown away most of the mantissa. A second
quantisation applied to an already-quantised tensor measures the *quantisation error of the first
step* as if it were the signal.

**0.2 That round-trip is lossy, so a locally requantised AWQ is worse than the repo's own AWQ would
have been.** E4M3 has 3 explicit mantissa bits. Upcasting FP8 → BF16 restores the storage type but
not the information; the second quantiser is then fitting 4-bit groups to data that is already
grid-snapped. `[reasoned]` The practical consequence for the feature's honesty: the option the
operator is offering is **not** "get the AWQ the publisher did not upload" — it is "get a *worse*
AWQ, computed locally, hours later, at the operator's cost". The `note` field must say so.

**0.3 There is an officially demonstrated one-pass path — and it produces RTN, not AWQ.**
`model_free_ptq` accepts a `converter` argument, which lets one `convert_checkpoint` call dequantise
and requantise shard-by-shard *"as part of the quantization run without writing an intermediate
checkpoint to disk"*, *"without ever loading the full model into memory as a torch.nn.Module"*.
The official example is a from-FP8→W4A16 recipe:
[`examples/model_free_ptq/minimax_m27_w4a16.py`](https://github.com/vllm-project/llm-compressor/blob/main/examples/model_free_ptq/minimax_m27_w4a16.py)

```python
from compressed_tensors.entrypoints.convert import FP8BlockDequantizer
from compressed_tensors.quantization.quant_scheme import W4A16

model_free_ptq(
    model_stub="MiniMaxAI/MiniMax-M2.7",
    save_directory="MiniMax-M2.7-W4A16",
    scheme=QuantizationScheme(**W4A16, targets=[...]),
    converter=FP8BlockDequantizer(targets=[...]),
)
```

This is the only path that is both fast and cheap. It is **data-free** (RTN / round-to-nearest), and
`llm-compressor` refuses to let it calibrate: `_validate_scheme` in
[`entrypoints/model_free/validate.py`](https://github.com/vllm-project/llm-compressor/blob/main/src/llmcompressor/entrypoints/model_free/validate.py)
raises *"Model Free PTQ cannot calibrate activations. Please use `oneshot` instead."* So it cannot
produce an AWQ-quality or GPTQ-quality 4-bit model — only an RTN one. AWQ and GPTQ accuracy need
calibration data and a real forward pass, which means `oneshot`, which means the round-trip.

**0.4 VRAM is not the constraint, and §8's numbers are wrong in both directions.**
Under llm-compressor's default pipeline only **one decoder layer** is on the GPU at a time; the model
lives in host RAM. Published model: GPU ≈ `mem(1 layer)` for RTN and `mem(1 layer) × 2` for GPTQ
([memory guide](https://docs.vllm.ai/projects/llm-compressor/en/latest/guides/memory/)). For this
27B model that is roughly **2 GB, not the 42 GB §8's sample JSON states** — so `feasible_on_gpu`
comes out `true` on both catalog cards, and the answer that "matters" (per §8's own prose) is the
opposite of what §8 expects. Meanwhile §8's `estimated_minutes: 90` is also wrong, by ~4–5×: the only
published measurement of this workload puts a GPTQ pass on a 30B-class model at **3.9 hours**, and
the AWQ+GPTQ recipe the official 27B example uses needs two such passes. §5 has the arithmetic.

---

## 1. `tool` — what does weight quantisation per target, and what is still alive

| Tool | Status (checked 2026-11-12) | Covers | Verdict for this feature |
|---|---|---|---|
| **`llm-compressor`** (`vllm-project/llm-compressor`, v0.15.0) | Active, the official one. *"If you want to quantize a model for vLLM, use LLM Compressor."* | AWQ, GPTQ, RTN, SmoothQuant, FP8 W8A8, NVFP4/MXFP4/MXFP8, W4A16, sparse | **The recommendation.** Only tool that both (a) is what vLLM supports and (b) documents a from-FP8 path at all. |
| **AutoAWQ** | **Deprecated.** README: *"The vLLM project has fully adopted AutoAWQ … AutoAWQ is officially deprecated and will no longer be maintained."* Last tested Torch 2.6.0 / Transformers 4.51.3. | AWQ | **Do not recommend.** vLLM's own page: *"The AutoAWQ library is deprecated. This functionality has been adopted by the vLLM project in llm-compressor."* |
| **GPTQModel** (`ModelCloud/GPTQModel`) | Active; broadest coverage (GPTQ, AWQ, ParoQuant, QQQ, GGUF, FP8, EXL3, GPTAQ). Own vLLM plugin. | GPTQ + variants | Viable alternative if the operator already uses it; not needed here. Its AWQ backends (`AWQ_TORCH_INT8`, `TORCH_INT8_AWQ`) are retired. |
| **NVIDIA TensorRT Model Optimizer** (`modelopt`) | Active, well-funded. Quantise/distil/prune/NAS; exports for vLLM, SGLang, TensorRT-LLM. | FP8, NVFP4, INT4 AWQ/GPTQ | Reachable from vLLM as `--quantization modelopt*`. NVIDIA-centric; unnecessary for an L40S-or-6000 choice. |
| **TorchAO** | Winding down: *"We are minimizing new feature development in torchao, and consolidating quantization development in other repositories."* | int4/int8, float8 weight-only | **Do not recommend** for producing a checkpoint. |
| **AMD Quark** | Active. AWQ/GPTQ/Qronos (PyTorch) + AdaQuant/AdaRound/GPTQ/Bias Correction (ONNX). Backs `--quantization quark`. | INT/FP8/MXFP4 | AMD-only relevance; the catalog has no AMD cards. |

The one sentence the JSON `tool` field should carry: **`"llm-compressor"`**, with the version pinned.

```json
"tool": "llm-compressor",
"tool_version": "0.15.0",
"tool_note": "AutoAWQ is deprecated and must not be suggested. llm-compressor is vLLM's supported path."
```

Environment pinning matters and cannot be guessed by the code — it must come from the recipe, because
`llm-compressor` 0.15.0 declares hard ranges in
[`setup.py`](https://github.com/vllm-project/llm-compressor/blob/main/setup.py): `torch>=2.10.0,<=2.14.1`,
`transformers>=5.15.0,<=5.21.0`, `compressed-tensors==0.20.1`, `accelerate>=1.15.0`, `datasets>=4.8.4`,
`auto-round>=0.15.1`, `python_requires>=3.11`. **There is no `llmcompressor` CLI** — `setup.py`
registers only `llmcompressor.trace` and a deprecated `reindex_fused_weights`. Every recipe is a
Python file you write and run. That is why the `steps[]` field cannot be a one-liner.

---

## 2. `steps[]` — the literal invocations

`steps[]` must be strings a developer (or a shell) can execute in order. Nothing below is invented:
each block is assembled from an official example plus the official docs for its arguments.

### 2.1 Path A — FP8 → W4A16 RTN in one pass (cheapest, lowest quality)

Precedent: `examples/model_free_ptq/minimax_m27_w4a16.py`. No calibration data, no dense checkpoint
on disk, streams shard by shard.

```bash
pip install "llm-compressor==0.15.0"
hf auth login          # required: the source repo is gated, see §7
```

`requantize_a.py`:

```python
from compressed_tensors.entrypoints.convert import FP8BlockDequantizer
from compressed_tensors.quantization import QuantizationScheme
from compressed_tensors.quantization.quant_scheme import W4A16
from llmcompressor.entrypoints.model_free import model_free_ptq

SRC = "orcarouter/Qwen3.8-27B-Uncensored-FP8"
DST = "Qwen3.8-27B-Uncensored-W4A16-RTN"

TARGETS = [
    r"re:.*self_attn\.(q|k|v|o)_proj$",
    r"re:.*linear_attn\.(in_proj_qkv|in_proj_z|out_proj)$",
    r"re:.*mlp\.(gate|up|down)_proj$",
]

model_free_ptq(
    model_stub=SRC,
    save_directory=DST,
    scheme=QuantizationScheme(**W4A16, targets=TARGETS),
    converter=FP8BlockDequantizer(targets=TARGETS, weight_block_size=(128, 128)),
    ignore=[
        "lm_head",
        r"re:.*embed_tokens$",
        r"re:.*linear_attn\.in_proj_a$",
        r"re:.*linear_attn\.in_proj_b$",
        r"re:.*visual.*",
    ],
    device="cuda:0",
)
```

Notes that are load-bearing, not cosmetic:

- **`weight_block_size=(128, 128)` must match the source's `quantization_config.weight_block_size`.**
  The published DeepSeek-V3.2 config is `{"activation_scheme":"dynamic","fmt":"e4m3",
  "quant_method":"fp8","scale_fmt":"ue8m0","weight_block_size":[128,128]}`
  ([config.json](https://huggingface.co/deepseek-ai/DeepSeek-V3.2/raw/main/config.json)), and that is
  the exact repo the official DeepSeek example dequantises. Get the value from the source repo; do not
  hardcode it.
- **The ignore list is model-specific and comes from the source checkpoint's own recipe**, not from
  this document. The official 27B 4-bit example ignores
  `["re:visual.*", "re:model.visual.*", r"re:.*lm_head", "re:.*embed_tokens$",
  r"re:.*linear_attn\.in_proj_a$", r"re:.*linear_attn\.in_proj_b$"]`
  ([27B 4-bit example](https://docs.vllm.ai/projects/llm-compressor/en/latest/key-models/qwen3.8/27b/4bit-example/)).
- `model_free_ptq` takes `(model_stub, save_directory, scheme=None, config=None, ignore=(), max_workers="auto", device=None, converter=None)`
  (source: [`entrypoints/model_free/__init__.py`](https://github.com/vllm-project/llm-compressor/blob/main/src/llmcompressor/entrypoints/model_free/__init__.py)).
  `max_workers` defaults to `"auto"`, derived from shard count/size and free device memory — **host
  memory is not taken into account**, so on a small-RAM box set it explicitly to `1` or `2`.

### 2.2 Path B — FP8 → dense BF16 → W4A16 via AWQ+GPTQ (best quality, the round-trip)

Precedent: the official Qwen3.8-27B 4-bit example, plus the canonical two-stage disk-offload script.

**Step 1 — dequantise FP8 to a dense BF16 directory on disk.**

```bash
python -c "
from compressed_tensors.entrypoints.convert import convert_checkpoint, FP8BlockDequantizer
convert_checkpoint(
    model_stub='orcarouter/Qwen3.8-27B-Uncensored-FP8',
    save_directory='./qwen38-27b-bf16',
    converter=FP8BlockDequantizer(targets=TARGETS, weight_block_size=(128,128)),
)
"
```

**Step 2 — AWQ + GPTQ from the dense directory.**

```python
from llmcompressor import oneshot
from llmcompressor.modifiers.awq import AWQModifier
from llmcompressor.modifiers.quantization import GPTQModifier
from llmcompressor.utils import load_context
from transformers import AutoProcessor, Qwen3_5ForConditionalGeneration

MODEL = "./qwen38-27b-bf16"
with load_context(Qwen3_5ForConditionalGeneration):
    model = Qwen3_5ForConditionalGeneration.from_pretrained(
        MODEL, dtype="auto", device_map="auto_offload",
        offload_folder="./offload_folder", max_memory={"cpu": 500e9},
    )
processor = AutoProcessor.from_pretrained(MODEL)

recipe = [
    AWQModifier(duo_scaling="both"),
    GPTQModifier(
        targets="Linear", scheme="W4A16",
        ignore=["re:visual.*", "re:model.visual.*", r"re:.*lm_head",
                r"re:.*embed_tokens$", r"re:.*linear_attn\.in_proj_a$",
                r"re:.*linear_attn\.in_proj_b$"],
    ),
]
oneshot(
    model=model, processor=processor, recipe=recipe,
    dataset="perfectblend", splits="train[:512]",
    max_seq_length=4096, num_calibration_samples=512,
)
model.save_pretrained("Qwen3.8-27B-Uncensored-W4A16-GPTQ-AWQ",
                      save_compressed=True, processor=processor)
```

This is the official example with three deliberate changes: the model directory is the local dense
copy instead of `"Qwen/Qwen3.8-27B"`, `device_map="auto_offload"` + `offload_folder` + `max_memory`
are added, and the recipe is unchanged. `device_map="auto_offload"` is not optional on a machine with
less than ~64 GB RAM; it is what the official
[`examples/disk_offloading/deepseek_v32_example.py`](https://github.com/vllm-project/llm-compressor/blob/main/examples/disk_offloading/deepseek_v32_example.py)
does for a 671B model, and that example also warns that `transformers==5.14` breaks saving for
disk-offloaded models and `>=5.15` is needed — consistent with the `>=5.15.0` pin in `setup.py`.

One trap carried verbatim from the official example, which comments on it directly: *"Qwen3.8-27B is a
vision-language model, so it must be loaded with its `Qwen3_5ForConditionalGeneration` class to keep
the vision tower."* Loading it through a generic `AutoModel*` class silently drops the tower, and
quantising a model whose tower has been dropped produces a checkpoint that no longer matches the
source's architecture.

### 2.3 Path C — in-place layerwise requantisation (no dense directory)

If disk is the binding constraint rather than time, `oneshot` can accept the already-offloaded FP8
model and decompress layer-by-layer:

```python
oneshot(
    model=model, processor=processor, recipe=recipe,
    dataset="perfectblend", splits="train[:512]", num_calibration_samples=512,
    pipeline="sequential",
    sequential_targets=["Qwen3_5DecoderLayer"],   # must be the real class/target name
    layerwise_decompression=True,
    layerwise_compression=True,
    stage_weights_in_pinned_memory=True,
)
```

Release note for v0.15.0: *"The sequential pipeline can decompress and compress one layer at a time,
avoiding upfront whole-model conversion for large checkpoints."* Precedents:
`examples/layerwise_decompression/kimi_k3_layerwise_example.py` and `glm_5_3_layerwise_example.py`.

**`[unverified]`** — I could not confirm that `Qwen3_5DecoderLayer` (or whatever
`Qwen3_5ForConditionalGeneration`'s decoder-layer class is actually named in
`transformers>=5.15`) works as a `sequential_target` with the vision tower present. Path C should be
offered as the expert route with that caveat, not as the default.

### 2.4 What does **not** work: vLLM online quantisation

vLLM can quantise at load time — `vllm serve <model> --quantization fp8_per_block` — and the docs
describe it as taking *"a BF16/FP16 model and quantiz[ing] its Linear and MoE weights to lower
precision … at load time, without needing a pre-quantized checkpoint or calibration data"*
([online quantisation](https://docs.vllm.ai/en/latest/features/quantization/online.html)). Accepted
values include `fp8_per_tensor`, `fp8_per_block`, `fp8_per_channel`, `mxfp8`, `nvfp4_per_token`,
`int8_per_channel_weight_only`, `online`.

**It takes a dense model as input.** Handing it an already-quantised FP8 checkpoint does not
requantise it to 4-bit. Any recipe that suggests `--quantization` as the requantisation mechanism is
wrong, and this is worth an explicit test in the Rust code, because it is exactly the shortcut a
reviewer would reach for.

### 2.5 `steps[]` as the JSON array

```json
"steps": [
  "pip install \"llm-compressor==0.15.0\"",
  "hf auth login  # the source repo is gated: accept its terms first",
  "write requantize.py (see docs/research/requantize-recipes.md §2.2)",
  "python requantize.py",
  "upload the produced directory to the operator's own HF repo or object store",
  "point the rental deployment at the new repo id and set --max-model-len to fit the W4A16 KV budget"
]
```

The last two steps are not optional garnish: the job produces a checkpoint **the operator must host**,
and vLLM cannot serve a directory that exists only inside the quantisation container.

---

## 3. Calibration data

Needed by AWQ, GPTQ, SmoothQuant, AutoRound; not needed by RTN/`model_free_ptq`; not needed by FP8
dynamic. From
[choosing a dataset](https://docs.vllm.ai/projects/llm-compressor/en/latest/steps/choosing-dataset/):

- *"Typical range: 128-512 samples is sufficient for most models"*; *"Start with 128-256 samples"*.
- The 27B 4-bit example uses `dataset="perfectblend", splits="train[:512]", num_calibration_samples=512, max_seq_length=4096`.
- The AWQ Llama example uses `splits="train[:512]", num_calibration_samples=256, max_seq_length=512`.
- The W4A16 GPTQ Llama example uses `num_calibration_samples=512, max_seq_length=2048`.
- FP8 `QuantizationModifier` recipes pass **no dataset at all** — plain `oneshot(model, recipe=recipe)`.

`perfectblend` resolves to **`mlabonne/open-perfectblend`**
([`transformers/data/perfectblend.py`](https://github.com/vllm-project/llm-compressor/blob/main/src/llmcompressor/transformers/data/perfectblend.py)
registers `name="perfectblend"`, `text_column="conversations"`,
`ROLE_MAP={"human": "user", "gpt": "assistant"}`). HF API reports `"gated": false`,
`license: apache-2.0`, ~7 342 downloads ⇒ **ungated and redistributable**, so an unattended job will
not stall on a dataset licence click. `ultrachat-200k`, `open-platypus`, `wikitext-2-raw-v1`, `c4` and
`razorcal` (MoE) are the other documented choices.

Practical default for the recipe: **512 samples, `max_seq_length=4096`, `perfectblend`.** That is the
official 27B choice and it is reproducible.

---

## 4. `estimated_minutes`

There is no published measurement for a 27B dense W4A16 AWQ+GPTQ run. There is one published
measurement for the nearest workload and a published runtime table for a much smaller model; the
estimate below is those two plus arithmetic, and must be labelled as such.

**Measured (Red Hat, *LLM Compressor v0.10*, 2026-03-18, GPTQ on Qwen3-30B-A3B — scheme not stated
in the article, assume W4A16-class):**
[single GPU **3.9 h**; 2 GPUs 1.95 h (~2.0×); 4 GPUs 1 h (~3.8×)](https://developers.redhat.com/articles/2026/03/18/llm-compressor-010-faster-compression-distributed-gptq)

**Published runtime table (Meta-Llama-3-8B-Instruct, A100):** RTN weights-only ~1 min; RTN
weights+activations ~20 min; GPTQ weights-only ~30 min; AWQ weights-only ~30 min
([runtime guide](https://docs.vllm.ai/projects/llm-compressor/en/latest/guides/runtime/)).

**`[reasoned]` for `orcarouter/Qwen3.8-27B-Uncensored-FP8` → W4A16 AWQ+GPTQ (Path B):**

| stage | basis | minutes |
|---|---|---|
| dequantise FP8 → BF16 on disk (27.8 GB read, 55.6 GB write) | I/O-bound; no published figure | 10–25 |
| GPTQ pass | measured 234 min on a 30B model, scaled to 27.8B ≈ ×0.93 | 215–235 |
| AWQ pass | docs give AWQ ≈ GPTQ cost for 8B (30 vs 30 min) | 215–235 |
| save + upload | ~15 GB write | 10–20 |
| **total, 1 GPU** | | **450–515 min (7.5–8.6 h)** |

**Path A (`model_free_ptq` RTN, one pass):** RTN weights-only is *"~1 min"* for 8B in the runtime
table, but that row does not include the FP8 dequantisation or the 28 GB read / 15 GB write. Reasoned
estimate **20–45 min** on one GPU. **`[unverified]`** — no measurement exists.

**Path A (multi-GPU):** llm-compressor *"does not currently support tensor parallelism for
compression"*. Speed-ups come from DDP (`torchrun --nproc_per_node=N`) or distributed GPTQ (v0.10+,
experimental), measured at ~2.0× on 2 GPUs and ~3.8× on 4.

So the JSON should say:

```json
"estimated_minutes": 480,
"estimated_minutes_range": [450, 515],
"estimated_minutes_note": "Reasoned from a measured 3.9 h GPTQ pass on a 30B model (Red Hat, LLM Compressor v0.10) plus an AWQ pass of comparable cost. Not measured on this model. Path A (RTN, one pass) is 20-45 min."
```

Cost follows directly, at §9's rates: 7.5–8.6 h on an L40S at $1.95/h is **$15–17**; on the RTX PRO
6000 at $3.03/h it is **$23–26**. That is real money for a job that produces a *worse* AWQ than the
publisher would have shipped, and the panel should say so before it is started.

---

## 5. `requires_vram_bytes`, `requires_extra_vram_bytes`, `feasible_on_gpu`

### 5.1 The published memory model

From the [memory guide](https://docs.vllm.ai/projects/llm-compressor/en/latest/guides/memory/):

- `mem(model) ≈ 2 GB per 1B parameters`, and **the CPU holds the whole model**; sequential onloading
  puts one layer on the GPU at a time with the rest on CPU/disk.
- **GPU = `mem(1 layer)` for RTN; GPU = `mem(1 layer) × 2` for GPTQ** (the Hessian is
  weight-sized).
- Published rows: Llama-3-8B — CPU ~16 Gb, GPU RTN ~0.5 Gb, GPTQ ~1 Gb. DeepSeek-R1-0528-BF16 — CPU
  ~1368 Gb, RTN ~22.4 Gb, GPTQ ~44.8 Gb. Qwen2.5-VL-7B — CPU ~14 Gb, RTN 1.3 Gb, GPTQ 2.6 Gb.
- **Vision towers load onto the GPU all at once and are not quantised.** This applies: the target is
  `Qwen3_5ForConditionalGeneration`, a `model_type: "qwen3_5"` image-text-to-text model.

Note what the Qwen2.5-VL row proves: for a VLM the GPU term is
`max(mem(1 text layer), mem(vision tower))` — the 7B row's 1.3 Gb is the vision tower, ~3× a text
layer. The published model contains **no margin** for the calibration batch's activations, for
pre-trace buffers, or for allocator fragmentation.

### 5.2 The rule, with `k` stated and derived

Two separate requirements, because they bind in different places and it is dishonest to fold them
into one number:

```text
# peak GPU
requires_vram_bytes ≈ ceil(params / n_layers) * b_dequant * (1 + gptq) * k_gpu
                      + vision_tower_bytes + fixed_gpu_bytes

  b_dequant  = 2        bytes/param when one layer is upcast to BF16
  gptq       = 1 if the recipe chains a GPTQ modifier, 0 for RTN/AWQ-free paths
  k_gpu      = 4        CHOSEN margin (published value is 1 for RTN, 2 for GPTQ)
  fixed_gpu_bytes ≈ 2 GiB   CUDA context + kernels + fragmentation  [reasoned]

# host RAM and scratch disk — the constraints that actually bite
requires_host_ram_bytes ≈ params * 2 * k_ram          k_ram = 1.25
requires_disk_bytes     ≈ params * bpp_source          (source checkout)
                        + params * 2                  (dense intermediate, Path B only)
                        + params * bpp_target * 1.2    (output; 1.2 absorbs unquantised residue)
```

**Where `k_gpu = 4` comes from, honestly:** the published model is exactly 1.0 (RTN) or 2.0 (GPTQ)
with nothing left over for a batch of `max_seq_length × batch_size` activations, the tracer's captured
intermediates, the quantised copy of the layer being written, or the whole-model vision-tower onload.
Doubling the published GPTQ term is a margin I chose, not a published constant, and the document must
say so — it is an operator-visible assumption in the same sense §7 already treats `overhead_bytes`.
The alternative — pinning the *whole* model in VRAM with `pipeline="basic"` — is the regime §8's
sample numbers came from, and §5.3 shows it fails.

### 5.3 The numbers for this model

`params = 27 781 427 952`, `n_layers = 64` ⇒ **434 M params/layer ⇒ 0.87 GB/layer in BF16.**

| term | Path B (AWQ+GPTQ) | Path A (RTN, one pass) |
|---|---|---|
| per-layer BF16 weight | 0.87 GB | 0.87 GB |
| ×(1+gptq)×k_gpu=4 | 6.9 GB | 3.5 GB |
| vision tower | 1–4 GB **`[unverified]`** | 1–4 GB |
| fixed GPU (context, kernels) | ~2 GB | ~2 GB |
| **`requires_vram_bytes`** | **~10–13 GiB** | **~7–10 GiB** |
| host RAM (`params×2×1.25`) | ~69 GB | ~69 GB (or trade for disk via `auto_offload`) |
| disk: source FP8 (`×1.0`) | 27.8 GB | 27.8 GB |
| disk: dense BF16 intermediate | 55.6 GB | **0** |
| disk: W4A16 output (`×0.5×1.2×params`) | 16.7 GB | 16.7 GB |
| **`requires_disk_bytes`** | **~100 GB → provision 120 GB** | **~45 GB → provision 60 GB** |
| `estimated_minutes` | 450–515 | 20–45 |
| quality | AWQ+GPTQ, the official recipe | RTN, data-free, lowest |

**The §8 sample JSON is contradicted on three fields at once.** It states
`requires_vram_bytes: 42 000 000 000` and `requires_extra_vram_bytes: 14 000 000 000` (totalling
~56 GB ≈ `params × 2`, i.e. it models the upcast as resident in VRAM) and `estimated_minutes: 90`.
Under the tool's actual default behaviour the upcast lives in host RAM and one layer at a time on the
GPU, so:

- `requires_vram_bytes` should be **~10–13 GiB**, not 42 GB — smaller than the card either way;
- `requires_extra_vram_bytes` should be **0** — see below;
- `estimated_minutes` should be **~480**, not 90.

And in the other direction, §8's prose — *"requantising a 27B model to AWQ needs the FP16 upcast plus
the calibration pass, so it does not fit on the card you were trying to squeeze it into"* — is **not
true for either catalog card** under the default recipe: ~10–13 GiB fits on a 46 GiB L40S with
room to spare. The prose only becomes true if you write the recipe badly (`pipeline="basic"`, whole model
pinned in VRAM: `params × 2 × 2` = **111 GB**, which does not fit on the 95 GiB RTX PRO 6000 either).

### 5.4 `requires_extra_vram_bytes` = 0, and why

§8 does not define this field. The only coherent reading is "VRAM required beyond the card the
deployment already occupies". Two honest answers, both 0:

- Requantisation is a **separate offline job** that wants the card to itself; the correct instruction
  is to stop the vLLM deployment (or use a scratch instance), not to co-schedule.
- llm-compressor *"does not currently support tensor parallelism for compression"*, so a second card
  adds **speed** (DDP / distributed GPTQ, measured ~2.0× on 2 GPUs) and never **memory headroom**.
  Emitting a non-zero extra-VRAM requirement would imply the tool can be given more VRAM to succeed,
  which it cannot.

```json
"requires_extra_vram_bytes": 0,
"requires_extra_vram_note": "Compression does not support tensor parallelism; additional GPUs only shorten the job (measured 2.0x on 2 GPUs, 3.8x on 4). It is a single-card job that wants exclusive use of that card."
```

### 5.5 The predicate the code should emit

```text
feasible_on_gpu =
      requires_vram_bytes      <= vram_bytes(chosen_card) * safety(0.9)
  AND requires_disk_bytes      <= provisioned_disk_bytes
  AND requires_host_ram_bytes  <= host_ram_bytes  OR  the recipe sets device_map="auto_offload"

failed_on = first failing term, named: "vram" | "disk" | "host_ram"
```

`failed_on` is what makes `feasible_on_gpu: false` useful rather than just discouraging. For the
operator's actual case — `orcarouter/Qwen3.8-27B-Uncensored-FP8`, currently usable quants
`["fp8"]` — the answer on **both** catalog cards is `feasible_on_gpu: true`, `failed_on: null`,
**with the caveats carried in `note`**: ~7.5–8.6 h, ~100 GB of scratch disk, ~69 GB RAM, a gated
source repo, and a quality regression relative to a publisher-native AWQ.

---

## 6. Output shape, and how vLLM loads it

### 6.1 Directory layout

Path A and Path B both produce a **plain HuggingFace-style model directory**, not a single file:

```text
Qwen3.8-27B-Uncensored-W4A16/
  config.json               # + quantization_config
  generation_config.json
  tokenizer.json, tokenizer_config.json, vocab.json, merges.txt
  preprocessor_config.json, chat_template.jinja   # VLM: come from the processor
  model-00001-of-00003.safetensors
  model-00002-of-00003.safetensors
  model-00003-of-00003.safetensors
  model.safetensors.index.json
```

Each quantised `nn.Linear` stores **`weight_packed` + `weight_scale` + `weight_shape`** instead of
`weight` (verified in
[`compressors/pack_quantized/base.py`](https://github.com/vllm-project/compressed-tensors/blob/main/src/compressed_tensors/compressors/pack_quantized/base.py):
*"packs it into int32 as `weight_packed`, stores the original shape as `weight_shape`, and removes
`weight`"*). `weight_zero_point` appears only for asymmetric schemes — W4A16 is symmetric, so it is
absent. Any tool that expects to find `weight` will fail.

For a VLM, `save_pretrained(..., processor=processor)` is what writes the tokenizer and
`preprocessor_config.json` sidecars. **`[unverified]`** — I could not confirm the exact minimal
sidecar set for `Qwen3_5ForConditionalGeneration`; if the sidecars are missing, vLLM will load the
weights and then fail on the processor.

### 6.2 `config.json` — the keys that matter

Shape of a real published `pack-quantized` W4A16 repo
(`RedHatAI/Qwen3-30B-A3B-quantized.w4a16`):

```json
"quantization_config": {
  "quant_method": "compressed-tensors",
  "format": "pack-quantized",
  "quantization_status": "compressed",
  "kv_cache_scheme": null,
  "config_groups": {
    "group_0": {
      "targets": ["Linear"],
      "input_activations": null,
      "output_activations": null,
      "weights": {
        "num_bits": 4, "type": "int", "strategy": "group",
        "group_size": 128, "symmetric": true, "dynamic": false,
        "observer": "minmax", "actorder": "weight"
      }
    }
  },
  "ignore": ["lm_head", "model.layers.0.mlp.gate", "..."]
}
```

`W4A16` in compressed-tensors is exactly `_int_wnam(4)`:
`{num_bits: 4, type: INT, strategy: GROUP, group_size: 128, symmetric: True, dynamic: False}` with
**no `input_activations` entry**
([`quantization/quant_scheme.py`](https://github.com/vllm-project/compressed-tensors/blob/main/src/compressed_tensors/quantization/quant_scheme.py)).
Optional fields (`actorder`, `observer`, `scale_dtype`, `zp_dtype`, `block_structure`, `version`) vary
by compressed-tensors version — **do not generate a config; read the one the tool wrote.**

For comparison, the source repo's own FP8 block config (vLLM-native form) is
`{"quant_method": "fp8", "activation_scheme": "dynamic", "fmt": "e4m3", "scale_fmt": "ue8m0",
"weight_block_size": [128, 128]}` — note **`weight_block_size` and `weight_scale_inv`**, not
`weight_scale`. That naming difference is why the dequantiser must be `FP8BlockDequantizer` and not
the generic `CompressedTensorsDequantizer`.

### 6.3 `quant_method` strings

| string in `quantization_config.quant_method` | produced by | `format` / extra keys | loadable by vLLM |
|---|---|---|---|
| `"compressed-tensors"` | llm-compressor, **including its AWQ output** | `format: "pack-quantized"` (W4A16), `"float-quantized"` / `"naive-quantized"` (FP8) | yes, auto-detected |
| `"awq"` | AutoAWQ legacy | `bits`, `group_size`, `zero_point`, `version` | yes, via the `auto_awq` override |
| `"gptq"` | AutoGPTQ / GPTQModel | `bits`, `group_size`, `desc_act`, `sym`, `checkpoint_format`, `damp_percent` | yes |
| `"fp8"` | vLLM-native FP8 | `weight_block_size`, `activation_scheme`, `fmt` | yes |
| `"modelopt"`, `"modelopt_fp4"`, `"nvfp4"`, `"mxfp4"` | NVIDIA ModelOpt / llm-compressor | — | yes |

**This is a naming trap that will reach the UI.** When the user asks for `"awq"` and the recipe runs
llm-compressor, the checkpoint that comes out has `quant_method: "compressed-tensors"` and
`format: "pack-quantized"`. It is not an AutoAWQ-format repository, it will not open with
AutoAWQ tooling, and its `config.json` does not contain `bits` or `group_size` at the top level. The
JSON's `target.id: "awq"` is therefore a **request label, not a description of the artefact**, and the
`note` must say which. Suggestion: keep `target.id: "awq"` (it is what the operator asked for and what
the panel shows) and add `target.produced_quant_method: "compressed-tensors"`.

### 6.4 Never synthesise `--quantization` from the request

vLLM's resolution order, from `_verify_quantization` in
[`vllm/config/model.py`](https://github.com/vllm-project/vllm/blob/main/vllm/config/model.py):

1. it reads `quant_method` out of the checkpoint's `config.json`;
2. it probes an `overrides` list in preference order —
   `auto_gptq, gptq, gptq_marlin, auto_awq, awq, awq_marlin, inc, moe_wna16, modelopt, modelopt_fp4,
   modelopt_mxfp8, modelopt_int8, mxfp8, modelopt_mixed, mxfp4, gpt_oss_mxfp4, deepseek_v4_fp8,
   humming` — and adopts the matching method;
3. if `--quantization` was **not** given, it adopts the detected method;
4. if `--quantization` **was** given and disagrees, it raises
   `ValueError("Quantization method specified in the model config (...) does not match the quantization method specified in the `quantization` argument (...)")`;
5. an unrecognised `quant_method` raises `ValueError("Unknown quantization method: ...")`.

Therefore: **`vllm serve <new-dir>` with no `--quantization` flag at all.** The official deployment
guide shows exactly that — `vllm serve "Qwen3-30B-A3B-FP8-BLOCK"`, no flag
([deploying compressed models](https://docs.vllm.ai/projects/llm-compressor/en/latest/guides/deploy/)).
If the generated command says `--quantization awq` against a `compressed-tensors` artefact, the
deployment **fails to start** and the operator pays for a boot that never serves a token. This is the
single most likely way to break the feature, and it deserves a test that asserts the generated command
carries no `--quantization` value derived from `target.id`.

---

## 7. Licensing and redistribution

The job produces a **new derivative checkpoint that the operator must host**. Three separate questions,
only the first of which is cleanly answerable:

**(a) May the operator redistribute a quantised copy of this model?**
For the operator's actual model, yes. HF API metadata for `Qwen/Qwen3.8-27B` reports
`license: apache-2.0`, `gated: false`; `orcarouter/Qwen3.8-27B-Uncensored-FP8` carries
`cardData.license: apache-2.0` with `cardData.base_model = "Qwen/Qwen3.8-27B"` (relation
`quantized`). Apache-2.0 permits derivative works and redistribution, requiring the licence, the
NOTICE file, and *"prominent notices stating that You changed the files"* — so a requantised repo must
say it is a quantisation of the named base model.

**(b) Practically: the source repo is gated, which is an operational blocker, not a legal one.**
`GET /api/models/orcarouter/Qwen3.8-27B-Uncensored-FP8` returns **`"gated": "auto"`**. An unattended
requantisation job therefore needs an authenticated `HF_TOKEN` belonging to an account that has
accepted the gate, or it fails on the first `from_pretrained`. Note the inversion, which is worth
recording: the **BF16 base** `Qwen/Qwen3.8-27B` is ungated, so re-downloading the base and quantising
from *it* is technically easier than dequantising the gated FP8 — and gives better quality. The only
reason to start from FP8 is that it is half the download.

**(c) Other model families are not so simple, and the code must not generalise from Qwen.**
`meta-llama/Llama-3.3-70B-Instruct` reports `license: llama3.3` with `"gated": "manual"`. The Llama
licence binds distribution of derivative works to the same agreement, carries an acceptable-use policy
that propagates to derivatives, and requires derivative model names to keep the `Llama` prefix. A
requantised Llama is a derivative work: redistributing it is not the same act as redistributing an
Apache-2.0 derivative. **`[unverified]`** whether serving such a derivative to third parties over an
API constitutes distribution under that licence — that is a legal question, not a documentation one,
and this report should not pretend to settle it.

**What the code must do**: put `license` and `gated` into the requantise plan, and refuse to promise
that the output can be published. The honest `note`: *"Quantisation creates a derivative of the source
model. Check the source repo's licence before publishing the result; the source repo is gated, so an
HF token with access is required before the job can read the weights."*

---

## 8. Honest caveats (the `note` field)

Every one of these is a reason the operator's phrasing — *"the option to locally requantize"* — sounds
cheaper and better than it is:

1. **It is an offline batch job**, not a request. ~7.5–8.6 h single-GPU for AWQ+GPTQ; 20–45 min for
   RTN. Hours, not seconds, and it needs a GPU with exclusive use.
2. **It costs money now.** $15–26 at §9's catalog rates, paid before the first token is served.
3. **It needs ~100 GB of scratch disk and ~69 GB of host RAM** for the AWQ+GPTQ path.
4. **The result is worse than a publisher-native AWQ**, because it is a second quantisation of a
   lossy first one (§0.2). If quality matters, quantise the **BF16 base** instead — it is ungated, and
   the round-trip disappears — at the cost of downloading ~56 GB.
5. **It produces a checkpoint the operator must host.** vLLM cannot serve a directory that only
   existed inside the quantisation container. Hosting it is a new artefact with its own licence
   obligations (§7).
6. **Requantisation degrades the model non-freely.** §0.2 again: 4-bit is not a format conversion.
7. **The tool is Python-only and version-fragile** — no CLI, hard `torch`/`transformers`/
   `compressed-tensors` ranges (§1). The recipe must pin, and it may not be installable in whatever
   image the deployment uses.
8. **The source repo is gated**, so the job needs a human-approved HF token (§7b).
9. **The output's `quant_method` is not the one that was requested** (§6.3).

The sentence §8 already has is good and should survive verbatim: *"This is a real offline job on a
real GPU. Manarouter records the recipe and the command; it does not run it for you."*

---

## 9. Proposed filled `requantize` object

For the operator's actual request, `requested: "awq"` against a repo that ships FP8 only. Every value
carries its provenance.

```json
{
  "requested": "awq",
  "unavailable_reason": "the repository ships FP8 only; no AWQ sibling and no AWQ tensors",
  "source": {"id": "fp8", "label": "FP8 (block)", "bytes_per_param": 1.0},
  "target": {
    "id": "awq",
    "label": "AWQ 4-bit",
    "bytes_per_param": 0.5,
    "produced_quant_method": "compressed-tensors",
    "produced_format": "pack-quantized"
  },
  "feasible_on_gpu": true,
  "failed_on": null,
  "requires_vram_bytes": 12884901888,
  "requires_extra_vram_bytes": 0,
  "requires_host_ram_bytes": 69000000000,
  "requires_disk_bytes": 100000000000,
  "estimated_minutes": 480,
  "estimated_minutes_range": [450, 515],
  "estimated_cost_usd": [15, 26],
  "tool": "llm-compressor",
  "tool_version": "0.15.0",
  "steps": [
    "pip install \"llm-compressor==0.15.0\"",
    "hf auth login  # the source repo is gated; accept its terms first",
    "write requantize.py (docs/research/requantize-recipes.md §2.2)",
    "python requantize.py",
    "upload the produced directory to a repository you control",
    "vllm serve <your-repo>   # no --quantization flag: it is read from config.json"
  ],
  "note": "Real offline job: ~7.5-8.6 h on one GPU, ~100 GB scratch disk, ~69 GB RAM, ~$15-26. Peak VRAM is small (~10 GiB) because llm-compressor keeps one layer on the card at a time - the card is not the constraint, time and disk are. Starting from FP8 means a round-trip through dense weights, so the result is worse than an AWQ quantised from the BF16 base; the BF16 base is an alternative and is not gated. Produces a compressed-tensors checkpoint, not an AutoAWQ one, and does not accept --quantization awq. Manarouter records the recipe and the command; it does not run it for you."
}
```

An RTN variant, when the operator prefers speed over quality, differs in `target.label`
(`"4-bit RTN (W4A16)"`), `estimated_minutes: 35`, `requires_disk_bytes: 45000000000`,
`requires_vram_bytes: 8589934592`, and a `note` that states plainly that RTN is data-free and the least
accurate option.

---

## 10. What I could not verify

1. **Whether `model_free_ptq` + `FP8BlockDequantizer` is validated for `Qwen3_5ForConditionalGeneration`
   specifically.** The precedent example is a different architecture (MiniMax-M2.7). The vision tower
   and the `linear_attn` layers must be excluded from the scheme targets, and I could not confirm from
   source or docs that the fused/linear-attention modules survive the round-trip. **Code should treat
   Path A as "attempt and report failure", never as the guaranteed path.**
2. **The exact wall-clock for a 27B dense W4A16 AWQ+GPTQ run.** No published measurement exists; my
   450–515 min comes from a 30B measurement plus a reasoned AWQ pass. The two official sources
   disagree by ~2× on scaling (8B→30B is 30 min→234 min, i.e. ~2× superlinear), so the range could be
   wrong in either direction.
3. **The size of `Qwen3.8-27B`'s vision tower.** The memory model says it loads whole onto the GPU and
   is not quantised; I could not read the gated checkpoint's tensor index to weigh it. The 1–4 GB
   figure is `[reasoned]` from the Qwen2.5-VL-7B row (1.3 GB).
4. **Whether `Qwen3_5DecoderLayer` works as a `sequential_target`** for Path C with the vision tower
   present, or what the real class name is under `transformers>=5.15`.
5. **The exact tokenizer/preprocessor sidecar files** a `Qwen3_5ForConditionalGeneration` checkpoint
   needs to be re-served after `save_pretrained`, and whether `save_compressed=True` with
   `processor=processor` writes all of them.
6. **The source repo's actual `weight_block_size`.** The arch doc records
   `quant_method=fp8` and label "FP8 (block)" but the repo is gated and I could not read its full
   `quantization_config`. `(128,128)` is the overwhelmingly common default and matches DeepSeek-V3.2,
   but the recipe must read it rather than assume it.
7. **Whether a non-zero `requires_extra_vram_bytes` is ever the right answer.** §8 does not define the
   field; `0` is my reading of what "extra VRAM beyond the serving card" must mean given that
   compression cannot use tensor parallelism.
8. **Any legal conclusion about redistributing quantised derivatives of Llama-licensed models.** §7
   reports what the licences say; whether that makes a hosted API a distribution is not something
   documentation can settle.

---

## Sources

- llm-compressor: [converter guide](https://docs.vllm.ai/projects/llm-compressor/en/latest/guides/entrypoints/convert/), [memory guide](https://docs.vllm.ai/projects/llm-compressor/en/latest/guides/memory/), [runtime guide](https://docs.vllm.ai/projects/llm-compressor/en/latest/guides/runtime/), [choosing a dataset](https://docs.vllm.ai/projects/llm-compressor/en/latest/steps/choosing-dataset/), [choosing a scheme](https://docs.vllm.ai/projects/llm-compressor/en/latest/steps/choosing-scheme/), [choosing an algorithm](https://docs.vllm.ai/projects/llm-compressor/en/latest/steps/choosing-algo/), [deploying compressed models](https://docs.vllm.ai/projects/llm-compressor/en/latest/guides/deploy/), [Qwen3.8-27B 4-bit example](https://docs.vllm.ai/projects/llm-compressor/en/latest/key-models/qwen3.8/27b/4bit-example/)
- llm-compressor source: [`setup.py`](https://github.com/vllm-project/llm-compressor/blob/main/setup.py), [`src/llmcompressor/entrypoints/model_free/__init__.py`](https://github.com/vllm-project/llm-compressor/blob/main/src/llmcompressor/entrypoints/model_free/__init__.py), [`.../model_free/validate.py`](https://github.com/vllm-project/llm-compressor/blob/main/src/llmcompressor/entrypoints/model_free/validate.py), [`src/llmcompressor/pipelines/registry.py`](https://github.com/vllm-project/llm-compressor/blob/main/src/llmcompressor/pipelines/registry.py)
- llm-compressor examples: [`model_free_ptq/minimax_m27_w4a16.py`](https://github.com/vllm-project/llm-compressor/blob/main/examples/model_free_ptq/minimax_m27_w4a16.py), [`disk_offloading/deepseek_v32_example.py`](https://github.com/vllm-project/llm-compressor/blob/main/examples/disk_offloading/deepseek_v32_example.py), [`awq/llama_example.py`](https://github.com/vllm-project/llm-compressor/blob/main/examples/awq/llama_example.py), [`quantization_w4a16/llama3_example.py`](https://github.com/vllm-project/llm-compressor/blob/main/examples/quantization_w4a16/llama3_example.py), [`layerwise_decompression/kimi_k3_layerwise_example.py`](https://github.com/vllm-project/llm-compressor/blob/main/examples/layerwise_decompression/kimi_k3_layerwise_example.py)
- vLLM: [quantisation overview + hardware matrix](https://docs.vllm.ai/en/latest/features/quantization/index.html), [AutoAWQ (deprecated)](https://docs.vllm.ai/en/latest/features/quantization/auto_awq/), [GPTQModel](https://docs.vllm.ai/en/latest/features/quantization/gptqmodel.html), [online quantisation](https://docs.vllm.ai/en/latest/features/quantization/online.html), [`vllm/config/model.py`](https://github.com/vllm-project/vllm/blob/main/vllm/config/model.py), [`vllm/model_executor/layers/quantization/__init__.py`](https://github.com/vllm-project/vllm/blob/main/vllm/model_executor/layers/quantization/__init__.py)
- compressed-tensors: [`quantization/quant_scheme.py`](https://github.com/vllm-project/compressed-tensors/blob/main/src/compressed_tensors/quantization/quant_scheme.py), [`compressors/pack_quantized/base.py`](https://github.com/vllm-project/compressed-tensors/blob/main/src/compressed_tensors/compressors/pack_quantized/base.py), [`entrypoints/convert`](https://github.com/vllm-project/compressed-tensors/tree/main/src/compressed_tensors/entrypoints/convert)
- Other tools: [AutoAWQ (deprecation notice)](https://github.com/casper-hansen/AutoAWQ), [GPTQModel](https://github.com/ModelCloud/GPTQModel), [NVIDIA TensorRT Model Optimizer](https://github.com/NVIDIA/TensorRT-Model-Optimizer), [TorchAO](https://github.com/pytorch/ao), [AMD Quark](https://github.com/amd/Quark)
- Measurement: [Red Hat, "LLM Compressor v0.10" (2026-03-18)](https://developers.redhat.com/articles/2026/03/18/llm-compressor-010-faster-compression-distributed-gptq)
- HF API metadata: [`Qwen/Qwen3.8-27B`](https://huggingface.co/api/models/Qwen/Qwen3.8-27B), [`orcarouter/Qwen3.8-27B-Uncensored-FP8`](https://huggingface.co/api/models/orcarouter/Qwen3.8-27B-Uncensored-FP8), [`meta-llama/Llama-3.3-70B-Instruct`](https://huggingface.co/api/models/meta-llama/Llama-3.3-70B-Instruct), [`deepseek-ai/DeepSeek-V3.2` config](https://huggingface.co/deepseek-ai/DeepSeek-V3.2/raw/main/config.json)