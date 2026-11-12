# HuggingFace Hub HTTP discovery — established facts

Research basis: live calls to `https://huggingface.co` public API (anonymous, no token) against well-known public models, plus the official Hub API docs/OpenAPI. All JSON below is **observed and abbreviated**, not guessed. Nothing here uses Python/`huggingface_hub`.

Source model repos used:
- Qwen/Qwen2.5-7B-Instruct (public, 7B dense, BF16)
- meta-llama/Llama-3.1-8B-Instruct (public but **gated: "manual"**)
- mistralai/Mistral-7B-Instruct-v0.3 (public)
- Qwen/Qwen2.5-7B-Instruct-AWQ (public 4-bit AWQ)
- TheBloke/Llama-2-7B-GPTQ (public 4-bit GPTQ)
- bartowski/Qwen2.5-7B-Instruct-GGUF (public GGUF)
- google/gemma-2-9b-it (public metadata, **gated: "manual"**, files 401)
- deepseek-ai/DeepSeek-V2-Chat (public, 235B MoE/MLA)
- Qwen/Qwen3-32B (public, explicit head_dim)
- openai/gpt-oss-120b (public, MXFP4, MoE)

Official docs referenced:
- Hub API Endpoints: https://huggingface.co/docs/hub/api
- OpenAPI (machine-readable): https://huggingface.co/.well-known/openapi.md
- Gated models: https://huggingface.co/docs/hub/models-gated
- Rate limits: https://huggingface.co/docs/hub/main/rate-limits
- AWQ/GPTQ transformers docs: https://huggingface.co/docs/transformers/quantization/awq

---

## Endpoints

| # | Method | URL | Auth | What it returns |
|---|--------|-----|------|-----------------|
| 1 | GET | `/api/models/{repo_id}` | none (token optional) | Repo metadata. Observed top-level keys for Qwen: `_id, author, cardData, config, createdAt, disabled, downloads, gated, id, inference, lastModified, library_name, likes, model-index, modelId, pipeline_tag, private, safetensors, sha, siblings, spaces, tags, transformersInfo, usedStorage, widgetData`. |
| 2 | GET | `/api/models/{repo_id}/tree/{rev}` (+`?recursive=true`) | none | Flat **list** of `{type,size,path[,rfilename,oid,lfs{size,pointerSize},xetHash]}`. One entry per file/dir. No `Link` pagination header observed for small repos; OpenAPI says tree supports pagination. |
| 3 | GET | `/api/models/{repo_id}/resolve/main/{path}` | none for public; 401 `GatedRepo` for gated/private files | Raw file content (config.json etc.). Responds 307 redirect → `/api/resolve-cache/...` when followed with `-L`. |
| 4 | GET | `/api/models/{repo_id}/safetensors-metadata/{path}` | none | Parsed headers of all safetensors shards: `{"sharded":true,"index":{"metadata":{"total_size":15231233024}},"headers":{...shard→tensors{dtype,shape,data_offsets}}}`. Only `main` revision; ETag = commit sha; `If-None-Match` → 304. 400 if path isn't `.safetensors.index.json`; 422 if unparseable. |
| 5 | GET | `/api/models/{namespace}/{repo}/treesize/{rev}/{path}` | none | (listed in OpenAPI; not probed) |

### Endpoint 1 — `/api/models/{repo_id}`: real observed fields

Public Qwen (abbreviated):
```json
{
  "id":"Qwen/Qwen2.5-7B-Instruct","private":false,"gated":false,
  "pipeline_tag":"text-generation","library_name":"transformers",
  "tags":["transformers","safetensors","qwen2","text-generation","chat",...,"license:apache-2.0"],
  "downloads":6258699,"likes":2493,"modelId":"Qwen/Qwen2.5-7B-Instruct",
  "author":"Qwen","sha":"a09a35458c702b33eeacc393d103063234e8bc28",
  "lastModified":"2025-01-12T02:10:10.000Z","disabled":false,
  "safetensors":{"parameters":{"BF16":7615616512},"total":7615616512},
  "usedStorage":15255453428,
  "config":{"architectures":["Qwen2ForCausalLM"],"model_type":"qwen2","tokenizer_config":{...}},
  "cardData":{"license":"apache-2.0","language":["en"],"pipeline_tag":"text-generation","base_model":"Qwen/Qwen2.5-7B","tags":["chat"],"library_name":"transformers"}
}
```

Gated Llama (same call, anonymous, HTTP 200):
```json
{
  "id":"meta-llama/Llama-3.1-8B-Instruct","private":false,"gated":"manual",
  "config":{"architectures":["LlamaForCausalLM"],"model_type":"llama","tokenizer_config":{...}},
  "safetensors":{"parameters":{"BF16":8030261248},"total":8030261248},
  ...
}
```

Fields that matter and what is actually present:
- `id`, `modelId` — repo id.
- `gated` — `false` \| `"auto"` \| `"manual"` (OpenAPI enum; confirmed `false` and `"manual"` live).
- `private` — boolean (repo visibility; separate from gated).
- `tags` — array; includes `awq`, `gptq`, `gguf`, `4-bit`, `base_model:quantized:...`, `license:...`. **Unreliable as sole quant signal** (see quant section).
- `siblings` — present in base response but each entry is only `{rfilename}` (no `size`). Confirmed for Qwen (14 siblings) and AWQ (12 siblings). Size per file comes from `tree` (endpoint 2), not siblings.
- `config` — **minimal**: only `architectures`, `model_type`, `tokenizer_config` for dense public models (Qwen, Llama, Mistral, Gemma2, Qwen3). Does **not** include `num_hidden_layers`, `num_key_value_heads`, `head_dim`, `max_position_embeddings`, `torch_dtype`, `quantization_config` at top level. For quantized/MoE models a few extra keys appear (AWQ/GPTQ/MXFP4 add `quantization_config`; DeepSeek adds `num_experts_per_tok`; GPT-OSS adds `chat_template_jinja`,`quantization_config`). **Full sizing config must be read from endpoint 3 (`resolve/main/config.json`).**
- `cardData` — card metadata (`license`, `language`, `pipeline_tag`, `base_model`, `tags`); not a reliable quant source.
- `safetensors` — `{"parameters":{dtype→count},"total":N}` where `total` = sum = **parameter count** (observed: Qwen total 7,615,616,512 ≈ 7.6B; Llama 8,030,261,248 ≈ 8.0B; AWQ total 7,615,616,512; GPT-Q total 6,739,777,536). Present on base response for public models, no `?blobs=true` required.
- `usedStorage` — total repo on-disk bytes (Qwen: 15,255,453,428 ≈ 15.3 GB). Includes tokenizer etc.
- `downloads`, `likes`, `lastModified`, `sha`, `disabled`, `createdAt`, `author`, `pipeline_tag`, `library_name`, `inference`, `spaces`.

### Endpoint 2 — `tree/main?recursive=true`: real observed shape

HTTP 200, body is a JSON **list**. Qwen (14 entries, truncated):
```json
[
  {"type":"directory","size":0,"path":"original"},
  {"type":"file","size":1519,"path":".gitattributes"},
  {"type":"file","size":3945441440,"path":"model-00001-of-00004.safetensors"},
  {"type":"file","size":4976698672,"lfs":{"size":4976698672,"pointerSize":135},"path":"model-00001-of-00004.safetensors"},
  ...
]
```
Per-file `size` IS available here (bytes). LFS files also expose `lfs.size`. Gated repos still return the file list + sizes anonymously (Llama tree returned 200 with model shard sizes); only file **content** (`resolve`) is 401.
Pagination: no `Link` header observed for small repos. OpenAPI lists `Link`, `X-Total-Count`, `Content-Range` as exposed headers and describes tree as paginated; for very large repos page via `?recursive=true` + cursor/range and honor `Link` if present. **Unverified: exact pagination parameters.**

### Endpoint 3 — `resolve/main/config.json`: full config

Returns 307 redirect to `/api/resolve-cache/models/{repo}/{sha}/config.json?...&etag=...`; follow redirects. Real Qwen `config.json` keys (observed):
```json
{
  "architectures":["Qwen2ForCausalLM"],"model_type":"qwen2",
  "hidden_size":3584,"num_hidden_layers":28,"num_attention_heads":28,
  "num_key_value_heads":4,"max_position_embeddings":32768,
  "torch_dtype":"bfloat16","vocab_size":152064,"intermediate_size":18944,
  "rms_norm_eps":1e-06,"rope_theta":1000000.0,"sliding_window":131072,
  "max_window_layers":16,"tie_word_embeddings":false, ...}
```
Note: Qwen2 has **no `head_dim` key** (derive `head_dim = hidden_size / num_attention_heads` = 128) and has `sliding_window`+`max_window_layers` (SWA).

---

## Gated detection flow

`gated` schema (OpenAPI): `{anyOf:[{type:"boolean",const:false},{type:"string",enum:["auto","manual"]}]}` → values `false` \| `"auto"` \| `"manual"`. Live confirmed: Qwen=`false`, Llama=`"manual"`, Gemma2=`"manual"`, and **`"auto"` on `orcarouter/Qwen3.8-27B-Uncensored-FP8`** (observed 2026-11-12). `"auto"` = automatic approval.

Meaning (per HF gated docs): by default a model is **not gated**. Enabling access requests defaults to **automatic approval** (`"auto"`: anyone who shares their info gets access). **Manual approval** (`"manual"`): owner approves each user.

Can gated detection be done **without a token**? **Yes — via `/api/models/{id}` only.** Anonymous GET returns HTTP 200 with `gated` set for both public and gated repos (Llama `gated:"manual"` returned 200 anonymously). So the UI can answer "is this gated?" before login.

Decision table (anonymous):

| Anonymous result | gated? | What UI should do |
|---|---|---|
| `GET /api/models/{id}` → 200, `gated:false` | No | Propose download; no login needed for public files. |
| `GET /api/models/{id}` → 200, `gated:"auto"` or `"manual"` | Yes (public-but-gated) | Tell user the repo is gated; they must log in & be accepted before fetching files. Do NOT promise download works. |
| `GET /api/models/{id}` → 401 `{"error":"Invalid username or password."}` | Unknown (repo may not exist / anti-enumeration) | Treat as "not accessible anonymously"; surface "repo not found or requires auth". |
| `GET .../resolve/main/config.json` (or any file) → 401 `x-error-code: GatedRepo` | Yes, and file content blocked | Confirms gated; prompts login. Metadata endpoint already revealed it. |

Important asymmetry: metadata (`/api/models/{id}`) is public for gated repos; file content (`resolve`, file bytes) is 401 `GatedRepo`. `tree` still leaks file names+sizes for gated repos.

Auth header: `Authorization: Bearer hf_...`. A malformed/unknown token on a **public** repo is ignored (observed HTTP 200); token validity only matters on gated/private file endpoints (401 `GatedRepo`).

### Getting the architecture of a gated repo without a token — use `cardData.base_model`

This is the single most useful trick found, and it is what makes requirement 1 ("GPU KV Cache Size
automatically calculated") achievable for the operator's own model.

When `config.json` of the served repo is 401 `GatedRepo`, the architecture is **still usually
obtainable anonymously**, because a quantised repo almost always names its unquantised base and the
base is public:

1. `GET /api/models/{repo_id}` → HTTP 200 (public even for gated repos) → read `cardData.base_model`
   (e.g. `"Qwen/Qwen3.8-27B"`) and confirm with `cardData.base_model_relation` (`"quantized"`).
2. `GET https://huggingface.co/{base_model}/resolve/main/config.json` → HTTP 200 if the base is public.
3. Take layer topology, `head_dim`, `num_key_value_heads`, `max_position_embeddings` from the base
   config; take weight size and quantisation from the quantised repo's own metadata
   (`safetensors.total`, `quantization_config`), which are **not** gated.

Observed end-to-end on `orcarouter/Qwen3.8-27B-Uncensored-FP8`: metadata 200 with
`cardData.base_model = "Qwen/Qwen3.8-27B"`, base `config.json` 200, giving the full 64-layer hybrid
topology. **Do not report `confidence:"unavailable"` for a gated repo until this path has been
tried.** It is only genuinely unavailable when the repo *and* its stated base are both gated/private
(and note `base_model` may be a list, may be absent, or may point at a private repo — handle all three).

Caveat worth stating in the UI: the base config describes the **unquantised** model. Quantisation does
not change layer count, KV-head count or `head_dim`, so it is valid for KV geometry — but it does mean
a quantised repo's own `config.json` is the more authoritative source when it is readable.

---

## Quant discovery algorithm (ordered, confidence per signal)

Check in this order; stop at first confident signal.

1. **`config.quantization_config.quant_method`** (from `resolve/main/config.json`) — **highest confidence**, authoritative.
   - AWQ: `{"bits":4,"quant_method":"awq"}` — observed on Qwen/Qwen2.5-7B-Instruct-AWQ. Tags also had `awq`,`4-bit`.
   - GPTQ: `{"bits":4,"quant_method":"gptq"}` — observed on TheBloke/Llama-2-7B-GPTQ.
   - GPT-OSS MXFP4: `{"quant_method":"mxfp4","modules_to_not_convert":[...]}` — observed on openai/gpt-oss-120b.
   - bitsandbytes / compressed-tensors / modelopt / NF4 / MXFP4 shapes not live-probed; expected `quant_method` values `bitsandbytes`/`compressed-tensors`/`modelopt`/`nf4`/`mxfp4` per transformers docs. **Unverified live.**

2. **Filename patterns** (from `tree/main?recursive=true` `path`) — **high confidence** when a weight file matches:
   - `*awq*` → AWQ; `*gptq*` → GPTQ; `*fp8*` → FP8; `*gguf*` → GGUF; `*bnb-4bit*` / `*int4*` / `*int8*` → bitsandbytes; `*compressed-tensors*`; `*modelopt*`; `*nvfp4*` / `*mxfp4*`.
   - GGUF repos (bartowski GGUF) have **no `config.json`** via API (`config keys: []`) and no `safetensors` — quant known only from filename/tags.

3. **`tags`** containing `awq`/`gptq`/`fp8`/`gguf`/`4-bit`/`8-bit`/`base_model:quantized:...` — **medium confidence, supportive only**. Observed reliable on AWQ/GPTQ/GGUF repos, but tags can be stale or absent.

4. **Repo-name suffix** `-AQW`/`-GPTQ`/`-FP8`/`-GGUF` — **lowest confidence**; convention, not enforced. Do not trust suffix alone.

Confidence summary: `config.quantization_config` > filename > tags > repo suffix. GGUF is the exception (config empty → filename/tags required).

---

## Param count and bytes-per-param

### Parameter count — reliable API field

`GET /api/models/{id}` → `safetensors.total` = total parameter count (observed on base response, no `?blobs=true` needed):

| Repo | safetensors | total ≈ |
|---|---|---|
| Qwen/Qwen2.5-7B-Instruct | `{"parameters":{"BF16":7615616512},"total":7615616512}` | 7.6B |
| meta-llama/Llama-3.1-8B-Instruct | `{"parameters":{"BF16":8030261248},"total":8030261248}` | 8.0B |
| Qwen/Qwen2.5-7B-Instruct-AWQ | `{"parameters":{"I32":6525288448,"F16":1090328064},"total":7615616512}` | 7.6B |
| TheBloke/Llama-2-7B-GPTQ | `{"parameters":{"F32":2048,"I32":6476005376,"F16":263770112},"total":6739777536}` | 6.7B |

`total` = sum of the `parameters` map. The dtype keys (`BF16`,`I32`,`F16`,`F32`) reflect **storage dtype**, so a 4-bit AWQ/GPTQ reports its packed weights under `I32`. For MoE (DeepSeek-V2 total 235,741,434,880 ≈ 236B; GPT-OSS 120B total 116,829,156,672 ≈ 117B) `safetensors.total` also works and reflects dense+expert params stored.

Name-based "size in the name" patterns (heuristic, cross-check only): `0.5B, 1.5B, 7B, 8B, 13B, 27B, 32B, 70B, 72B, 120B, 405B`; MoE like `30B-A3B` / `235B-A22B` (total params / active params). Not an API field; cross-check against `safetensors.total`.

### Model size on disk

- `safetensors-metadata/model.safetensors.index.json` → `index.metadata.total_size` (observed Qwen: 15,231,233,024 ≈ 15.2 GB = model shards only).
- `/api/models/{id}` → `usedStorage` (observed Qwen: 15,255,453,428 ≈ 15.3 GB = whole repo).
- Per-file `size` from `tree/main?recursive=true`.
All three agree within tokenizer/metadata overhead. Reliability: high.

### Bytes-per-parameter (heuristic — no authoritative HF doc cited)

| Quant | bytes/param | Basis |
|---|---|---|
| fp16 / bf16 | 2 | 16-bit |
| fp8 (e4m3/e5m2) | 1 | 8-bit |
| int8 | 1 | 8-bit |
| int4 / awq / gptq 4-bit | ~0.5 | 4-bit weights + per-group scales/zeros (~0.55–0.6 incl. scales) |

This bytes/param table is a **heuristic**; I found no authoritative HF page stating exact values. `torch_dtype` from `resolve/main/config.json` tells the base compute dtype (Qwen=`bfloat16`, Llama=`bfloat16`, Mistral=`bfloat16`, DeepSeek=`bfloat16`).

---

## Architecture KV config keys (real observed values)

Full config read from `resolve/main/config.json` (API `config` field alone is insufficient). Gotchas:

| Model | model_type | Observed sizing keys | Gotchas |
|---|---|---|---|
| Qwen2.5-7B | qwen2 | hidden_size, num_hidden_layers, num_attention_heads, num_key_value_heads=4, max_position_embeddings, torch_dtype=bfloat16, sliding_window, max_window_layers | **No `head_dim`** (derive hidden/heads=128); sliding-window attention. |
| Llama-3.1-8B | llama | hidden_size, num_hidden_layers, num_attention_heads, num_key_value_heads, max_position_embeddings, torch_dtype, rope_theta, rms_norm_eps | No explicit `head_dim` (derive). |
| Mistral-7B-v0.3 | mistral | hidden_size=4096, num_hidden_layers=32, num_attention_heads=32, num_key_value_heads=8, max_position_embeddings=32768, torch_dtype=bfloat16, sliding_window=null, rope_theta=1e6 | No `head_dim` (derive=128); sliding_window present but null. |
| Gemma2-9B | gemma2 | (files 401 unauth; gated=manual) | Gemma uses explicit `head_dim` ≠ hidden/heads; config not fetchable anonymously because gated. |
| Qwen3-32B | qwen3 | hidden_size=5120, num_hidden_layers=64, num_attention_heads=64, num_key_value_heads=8, **head_dim=128**, max_position_embeddings=40960, torch_dtype=bfloat16, rope_theta=1e6, use_sliding_window=false | `head_dim` **explicit** (128) and ≠ hidden/heads (5120/64=80). Qwen3-Next hybrid layers not probed. |
| DeepSeek-V2-Chat | deepseek_v2 | hidden_size=5120, num_hidden_layers=60, num_attention_heads=128, num_key_value_heads=128, kv_lora_rank=512, q_lora_rank=1536, qk_rope_head_dim=64, v_head_dim=128, num_experts_per_tok=6, torch_dtype=bfloat16 | **MLA**: KV compressed via `kv_lora_rank`/`q_lora_rank`; no single `head_dim` (use `qk_rope_head_dim`/`v_head_dim`). MoE: `num_experts_per_tok`. |
| GPT-OSS-120B | gpt_oss | quantization_config.mxfp4; num_experts_per_tok present | MoE + MXFP4; sizing keys not fully probed. |

KV-cache sizing formula (per layer): `2 * num_hidden_layers * batch * seq_len * num_key_value_heads * head_dim * bytes_per_element` (×2 for K and V). For MLA (DeepSeek) use compressed ranks instead of full `num_key_value_heads*head_dim`. **This formula is standard inference knowledge, not an HF doc citation.**

---

## Errors and status codes

Observed live (anonymous):
- `GET /api/models/{public}` → **200** + JSON metadata, even for gated repos (`gated:"manual"`).
- `GET /api/models/{missing}` → **401** `{"error":"Invalid username or password."}` (HF does not confirm existence of unknown repos anonymously — anti-enumeration).
- `GET .../resolve/main/{file}` on gated repo → **401** with headers `x-error-code: GatedRepo`, `x-error-message: Access to model ... is restricted. You must have access to it and be authenticated to access it. Please log in.`
- `Authorization: Bearer hf_faketoken` on public repo → **200** (invalid token ignored for public content). Token validity only enforced on gated/private file endpoints.
- Error-shape headers exposed: `X-Error-Code`, `X-Error-Message`; JSON body `{"error":"..."}`.

Interpretation:
- 401 + `x-error-code: GatedRepo` → repo is gated and caller lacks access (need valid HF token with accepted access).
- 401 + `Invalid username or password` (metadata) → repo unknown / not reachable anonymously.
- 404 → documented for `safetensors-metadata` when path isn't `.safetensors.index.json`; for missing repos HF tends to 401 anonymously.
- 403 → expected for a valid-but-unauthorized token on gated content (not live-probed).
- 429 → rate-limit exceeded (see below).

---

## Rate limits / etiquette (unauthenticated local panel)

Live response headers (anonymous):
- `ratelimit: "api";r=<remaining>;t=<seconds-to-reset>`
- `ratelimit-policy: "fixed window";"api";q=500;w=300`
→ **500 API requests per 300 s (5 min) per IP** for anonymous users.

Documented tiers (Hub rate limits page, September '25):
- Anonymous (per IP): API 500 · Resolvers 3,000 · Pages 100 per 5-min window
- Free user: API 1,000 · Resolvers 5,000 · Pages 200
- PRO: API 2,500 · Resolvers 12,000 · Pages 400

No documented `User-Agent` requirement observed; requests succeeded with curl's default UA. **Etiquette:** spread requests, cache `config.json`/tree/safetensors per repo (ETag/If-None-Match supported on resolve and safetensors-metadata), and prefer one `tree?recursive=true` call over many per-file calls.

---

## Unverified

- ~~Exact `gated:"auto"` live example~~ — **RESOLVED 2026-11-12**: `orcarouter/Qwen3.8-27B-Uncensored-FP8` returns `gated:"auto"` anonymously.
- `?blobs=true` / `?expand=` effect on `siblings` (base `siblings` entries confirmed to contain only `rfilename`, no `size`).
- Tree pagination parameters / `Link` header behavior on very large repos.
- bitsandbytes / compressed-tensors / modelopt / NF4 `quantization_config` shapes and `quant_method` enum values (expected from transformers docs, not live-probed).
- ~~Qwen3-Next hybrid-layer config keys~~ — **PARTLY RESOLVED 2026-11-12**: `Qwen/Qwen3.8-27B` (`model_type: "qwen3_5"`) confirmed to carry `layer_types` with `linear_attention` / `full_attention` strings, plus `linear_*` state keys and `mamba_ssm_dtype`. Live values recorded in `vllm-sizing-and-quant.md` A.5. Still unverified: other hybrid families (Gemma-2/3 window patterns, Qwen3-Next specifically).
- 403 status for valid-but-unauthorized token; exact `User-Agent` requirement.
- bytes-per-parameter exact values (heuristic; no authoritative HF citation found).
- Behavior with a valid token (authenticated flows).

---

Citations: https://huggingface.co/docs/hub/api ; https://huggingface.co/.well-known/openapi.md ; https://huggingface.co/docs/hub/models-gated ; https://huggingface.co/docs/hub/main/rate-limits ; https://huggingface.co/docs/transformers/quantization/awq
