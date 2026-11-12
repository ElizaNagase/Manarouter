# 25 — GPU rentals

Manarouter normally forwards to somebody else's endpoint. This document covers the other arrangement:
the operator **rents a GPU**, the model runs **there**, and Manarouter decides what to send it, how
many sessions it can hold, what it costs, and when it is cheaper to let it go cold.

The interface design is frozen in
[research/ARCHITECTURE-gpu-rentals.md](research/ARCHITECTURE-gpu-rentals.md), which carries the
platform facts and their sources. This document is the operational one: what the operator does, what
the panel shows, and what Manarouter will refuse to pretend.

## API routes

The conductor exposes seven GPU-rental endpoints:

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/api/gpu/catalog` | which GPUs each platform offers |
| `GET` | `/api/lanes` | what may be done on each host (refusals built in) |
| `GET` | `/api/gpu/probe` | measure latency / throughput on a GPU |
| `GET` | `/api/gpu/rentals` | current rentals and their state |
| `POST` | `/api/gpu/apply` | submit a rental application |
| `POST` | `/api/gpu/lanes/cerebrium/bundle` | render `cerebrium.toml` for deployment |
| `GET` | `/api/gpu/lanes/cerebrium/compute` | accepted Cerebrium compute types — pure read, 13 rows; Modal H100 not accepted |

> **Status.** Read [23-implementation-status.md](23-implementation-status.md) before relying on any
> behaviour described here. This document describes the intended contract; the status note there says
> which halves are built and verified.

---

## 1. The one thing to understand first

**Modal publishes no public REST API for deploying an app.** This was checked directly, not assumed:
`https://modal.com/docs/api` → 404, `https://modal.com/docs/rest` → 404, `https://modal.com/api` →
404, and `api.modal.com` answers 415. The only documented deployment surface is the CLI
(`modal deploy [OPTIONS] APP_REF`, with no `--detach`).

So Manarouter does the honest thing:

1. It **generates the deployment bundle** — a complete, runnable `modal_app.py` plus the exact
   `modal deploy …` command line, with the GPU, model, quantisation, context length, KV dtype,
   concurrency and cooldown already filled in from the panel's answers.
2. It **shows the operator what will be deployed and what it will cost**, and asks them to run it.
3. When the deployment answers at its `*.modal.run` URL, Manarouter registers it as a provider and
   routes to it.

**Modal flow: bundle → manual deploy → paste `base_url`.** The operator runs the `modal deploy`
command themselves (or on a host where the CLI is installed) and pastes the resulting `*.modal.run`
URL back so Manarouter can route to it.

**Manarouter never claims to have deployed anything it did not deploy.** There is no code path that
fabricates a successful `modal deploy`. If the `modal` CLI happens to be installed and the operator
opts in, the bundle can be executed locally — but that path is a convenience, not the mechanism, and
it degrades to "here is the command, run it yourself" when the CLI or the Python SDK is absent. On the
development host used for this work, **neither is installed**, so that path is *unverified by
construction* and must never become load-bearing.

Everything else in this document is built on that split: Manarouter computes and plans with high
confidence, and hands the irreversible platform action to the human.

## Other deployment platforms

| Platform | Flow |
|---|---|
| **Cerebrium** | `POST /api/gpu/lanes/cerebrium/bundle` returns the `cerebrium.toml` file, the exact `cerebrium deploy -y --detach` command, and a **deterministic base URL** at `https://api.cerebrium.ai/v4/{project}/{app}/v1` — the operator runs the command and Manarouter uses the URL directly. Before choosing, `GET /api/gpu/lanes/cerebrium/compute` lists the accepted compute types (pure read, 13 rows; Modal H100 not accepted). |
| **Saturn** | A provider row is created pointing at the deployed endpoint; token factory docs are pending (no provider doc yet). |
| **Kaggle / molab** | Serving is **refused by each platform's Terms of Service**. Manarouter marks both with `can_serve_requests: false` and will not present them as viable serving targets. |

---

## 2. The two lanes (requirement 9)

An account can run a model in either of two modes, and they are meant to be used on **two different
cards at the same time**:

| Mode | What it is | Which card | Why |
|---|---|---|---|
| **Single** | One request is pinned to one chosen GPU. It is a request that must not queue. | the expensive card | latency matters more than throughput |
| **Batch** | *n* requests accumulate on **another** chosen GPU, and are executed together. | the cheap card | throughput matters more than latency |

The operator's own measurements on a 27 B-class model make the case: the more expensive card came out
**40 % cheaper per token** on decode-bound batches (derived $1.26/M vs $2.08/M at 2K context with 64
streams), while the cheaper card is the one that makes a small job worth starting at all. Capacity for
admission is measured in **KV tokens, not requests per minute** — the pool is a token pool, and a
512-token request and a 200 000-token request do not cost the same thing.

A single request that arrives when the pool is full is refused with a distinguishable reason
(`kv-budget-exceeded`), not silently queued behind a 200 K-token batch job.

---

## 3. Creating an account (requirement 8)

**An account is a platform credential. It is created on its own, and it holds many models.** This is
the one place the GPU-rental flow deliberately differs from the ordinary provider flow, where adding a
provider drops you straight into that provider's first-account form.

Creating a rental account asks for the platform and its credentials — for Modal, a **proxy token**
pair, `Modal-Key` (`wk-…`) and `Modal-Secret` (`ws-…`). The credential is written to the vault as a
bundle and referenced from `config.json` by id; **the secret itself never appears in `config.json`,
in a snapshot, in a log, or in this repository.** Entering it goes through the same
`--secret-stdin`-style flow the rest of the project uses, so it does not land in shell history.

An account carries no model. Models are attached to it afterwards, and one account can hold many.

---

## 4. Attaching a model to an account

This is the flow that satisfies requirements 1–7. The panel walks it as a sequence of steps —
platform, account, model, card, sizing, review — and each step is validated before the next is
offered.

### 4.1 Paste the model, then the quantisation opens up (requirements 4, 5)

The quantisation field is **blank and disabled until a model has been pasted**, because the available
quantisations depend on the model. Once the model is known, the panel determines its size:

1. **From the name first.** `…-27B-…` means 27 billion parameters. This is reported as
   `from_name`, and it is cheap and usually right, but it is a *name*, so it is not trusted as the
   final word.
2. **Then from the repository**, using the Hugging Face metadata API, which is authoritative about
   what actually exists.

The candidate list is then **intersected with what the chosen GPU can actually run**. Quantisation
methods are not GPU-neutral: an FP8 kernel needs a compute capability the card may not have, and
asking a T4 for an FP8 GEMM will fail at load time, not at request time. The panel shows what remains
after that intersection rather than offering a menu of things that will break.

### 4.2 A quantisation that does not exist is an error *and* a plan (requirement 6)

Asking for a quantisation the repository does not ship is not a dead end and it is not a silent
downgrade. The probe answers with an error **and** a `requantize` plan: the source quantisation
actually available, the target asked for, the tool that would do it, approximate time and VRAM, whether
it fits on the chosen card, and the ordered steps.

**Manarouter does not run the quantisation.** It prints the exact command for the operator to run, in
the same spirit as §1: the tool is named, the parameters are computed, and the human executes it.
Re-quantisation is an offline job (llm-compressor, AutoAWQ, GPTQModel, or vLLM's own online FP8 path),
not something a router should start behind your back.

### 4.3 The Hugging Face token appears only when it is needed (requirement 7)

A token field that is always visible trains people to paste secrets into forms. The panel asks for an
HF token **only once the repository is known to be gated** — and it can know that **before any login
prompt**, because the metadata endpoint answers anonymously (HTTP 200) and reports `gated` as
`false`, `"auto"` or `"manual"` even for repositories whose *files* are 401.

So: public repository → no token field at all. Gated repository → the field appears, with the reason.

**And a gated repository is not the end of the sizing story.** A quantised repository usually names
its unquantised base (`cardData.base_model`), and the base is usually public. Fetching the base's
`config.json` anonymously gives the real layer and head topology, which is what the KV calculation
needs. Manarouter follows that path automatically. `confidence: "unavailable"` is reported only when
the repository *and* its stated base are both inaccessible.

### 4.4 Which GPUs are listed, and the honest `(Paid)` marker (requirement 10)

The catalogue lists every GPU the platform offers. Requirement 10 asks for a `(Paid)` marker on the
GPUs that do not work on the free plan, **if the operator is on the free plan**.

**That marker cannot be implemented as literally stated, and the reason matters.** Modal publishes no
per-GPU free/paid gating at all: all eleven GPUs carry unconditional per-second prices, and the
platform's own words are that *using a GPU requires having a valid payment method on file* — which is
a property of the whole account, not a discriminator between cards. A `(Paid)` badge on eleven GPUs
would therefore be **fabricated**.

What the panel does instead, and why each part is defensible:

| What is shown | Basis | Provenance |
|---|---|---|
| Plan limits per plan — Starter 10 concurrent GPUs / $30 monthly credits; Team 50 / $100 | published plan documentation | `source: "documented"` |
| `(Paid)` on a GPU **refused on the Starter plan** | observed directly | `source: "observed"` |
| nothing on a GPU whose plan availability is unknown, with the provenance stated | absence of evidence | `source: "unknown"` |

So exactly **one** card carries `(Paid)` — the H100, which was observed refused on Starter — and the
rest honestly say `"unknown"`. A test asserts that split, so the honesty cannot silently erode.

`(Paid)` renders **only** for an observed refusal. Unknown is not paid, and the panel does not let a
guess look like a fact.

### 4.5 Two sizing numbers the operator should not have to guess

| Operator's words | What it is | What it does |
|---|---|---|
| **"Simultaneous Sessions"** | vLLM's `--max-num-seqs`, derived from the KV pool | how many requests the container will run at once |
| **"Shutdown Cooldown (seconds)"** | Modal's `scaledown_window` (default 60 s, range 2–1200 s) | how long an idle container stays warm before it is released |

They are shown together because they trade against each other: a longer cooldown costs money while
idle and saves a cold start later, and a cold start on this class of model is **~110 s on the cheaper
card and 160–181 s on the more expensive one**. Cold-start cost is a fixed per-batch cost, not a
per-request one, which is why the batch lane should say "too small to be worth a cold start" rather
than quietly starting one.

---

## 5. The KV cache number, and exactly how much to trust it (requirement 1)

The requirement is that the KV cache size is calculated **automatically**. It is, and the calculation
is worth explaining because a wrong answer here is not visible — it just means the container refuses
work it could have done, or accepts work it cannot hold.

vLLM's pool is, per token:

```
bytes_per_token = caching_layers × num_kv_heads × head_dim × KV_dtype_bytes × 2      (K and V)
kv_pool_tokens  = (gpu_memory_utilization × VRAM − weights − overhead) / bytes_per_token
```

with `gpu_memory_utilization = 0.92` and `block_size = 64` (vLLM's own defaults), and
`overhead_bytes` covering activations, CUDA-graph capture, block padding and any fixed recurrent
state — everything except the weights.

Three things are easy to get wrong, and this project got all three wrong at first:

1. **`caching_layers` is not `num_hidden_layers`.** A hybrid model can be mostly *linear* attention,
   where those layers hold a fixed-size state that does **not** grow with sequence length and
   therefore has no per-token KV cost. On the operator's own model — 64 layers, of which **48 are
   `linear_attention` and only 16 are `full_attention`** — the naive formula is **exactly 4× too
   high** (131 072 vs 32 768 bytes/token), which would under-report capacity four-fold. The layer
   split comes from the model's `config.json` `layer_types`.
2. **`head_dim` must be read, not derived.** The common fallback `hidden_size / num_attention_heads`
   gives `5120 / 24 = 213.3` on that model, against a declared `head_dim` of **256**. Prefer the
   explicit key.
3. **`overhead_bytes` is an assumption, and it is labelled as one.** vLLM documents no separate
   figure, so Manarouter uses **8 GiB** and says so. Two independent measurements of the same model
   imply 7.27 GiB and 7.85 GiB respectively, so the honest accuracy claim is **about ±2 %**, and the
   result is reported with `delta_vs_measurement_pct` next to the operator's measured value rather
   than being presented as exact.

**Independent confirmation is not claimed.** When a measurement exists, the panel shows the analytic
estimate and the measured number side by side and lets the operator pick. Two cautions are stated in
the output itself:

- A fit that was solved *from* two measurements cannot then be described as validated *by* them.
- **vLLM's own startup line is authoritative** — it prints `GPU KV cache size : N tokens` and
  `Maximum concurrency for L tokens per request : Cx` — and it is not available until the container
  has booted. The calculated number exists to fill the gap before that, and to be replaced by it.

Where the architecture genuinely cannot be read, `confidence` is `"unavailable"`, `kv_pool_tokens` is
`null`, and the reason is in `warnings` — the panel does not invent a number to fill the box.

---

## 6. What the operator ends up with

On success a rental deployment is a normal Manarouter provider of dialect `modal-vllm`: it appears in
the provider list, its models appear in `/v1/models`, and it can be used in combos like anything else.
The difference is that its capacity is tracked in **KV tokens** and its spend is tracked in
**dollars**.

Cost accounting is per-GPU-second, from the published per-second price for the card, because a
scale-to-zero deployment is billed for the time it exists rather than for a monthly average. A
`$X/month` figure that assumes 730 hours of uptime is wrong for a deployment that spends most of its
life scaled to zero, and the panel does not show one.

---

## 7. Operationally

- The **relay** (port 20900) is the request path and is the only process that talks to the rented
  GPU. It learns about rental changes through the compiled snapshot digest, exactly like every other
  provider — there is no IPC between the two processes.
- The **conductor** (port 20901) owns the admin surface: the catalogue, the probe, the sizing, the
  bundle generation, and the panel.
- Restarting is a live-traffic event. The units bind `0.0.0.0`, and the restart procedure is in
  [23-implementation-status.md](23-implementation-status.md) and the README; do not restart to test a
  change that a test can prove.

---

## 8. What Manarouter will not do

- **Deploy for you** and tell you it succeeded. See §1.
- **Run your quantisation.** See §4.2.
- **Show a `(Paid)` badge it cannot evidence.** See §4.4.
- **Claim an exact KV number.** See §5.
- **Present a fitted constant as a physical one.** `overhead_bytes` is named an assumption in the
  output, every time.
- **Serve through Kaggle or molab.** Both platforms refuse this in their Terms of Service. The
  lanes API marks them with `can_serve_requests: false`; the panel will not let them be selected
  as a serving target.

The last five are the same rule stated five ways: where the platform does not publish a fact,
Manarouter reports *what it does know* and *how it knows it*, and refuses to manufacture the rest.

---

## See also

- [research/ARCHITECTURE-gpu-rentals.md](research/ARCHITECTURE-gpu-rentals.md) — the frozen interface:
  object model, config schema, conductor API, catalogues, and the platform facts with sources.
- [providers/modal-vllm.md](providers/modal-vllm.md) — the `modal-vllm` dialect on the wire.
- [research/modal-platform.md](research/modal-platform.md) — the verified Modal facts: GPU ids,
  prices, plans, proxy tokens, `scaledown_window`.
- [research/vllm-sizing-and-quant.md](research/vllm-sizing-and-quant.md) — the KV formula, the hybrid
  correction, and the quantisation support matrix.
- [research/huggingface-discovery.md](research/huggingface-discovery.md) — anonymous metadata,
  gated detection, and the `base_model` workaround.
- [research/requantize-recipes.md](research/requantize-recipes.md) — the commands behind §4.2.
