# Modal platform research

Fact-checking notes for the in-progress `modal-vllm` provider
(`docs/providers/modal-vllm.md`, `crates/mr-core/src/quota/modal.rs`).
All facts below were checked against Modal's official docs and pricing pages;
retrieval date: **2026-11-12**. Nothing was guessed; anything not found on an
official page is marked UNVERIFIED.

## Verified facts

**Q1 — GPU types (canonical SDK/CLI `gpu=` id strings)**
Source: https://modal.com/docs/guide/gpu.md — Confidence: high
Canonical values accepted by the `gpu` argument of `@app.function()` /
`@app.cls()` / `@app.server()`:
`T4`, `L4`, `A10`, `L40S`, `A100`, `A100-40GB`, `A100-80GB`, `RTX-PRO-6000`,
`H100`, `H200`, `B200`, `B300`.
Modifier suffixes documented on top of base ids: `H100!` (opt out of Modal's
automatic H100→H200 upgrade; cost unchanged), `B200+` (run on B200 or B300;
billed as B200). GPU count via `:n`, e.g. `H100:8`; up to 8 GPUs per
container for B300/B200/H200/H100/A100/L4/T4/L40S, up to 4 for A10.
No GPU type is documented as "preview"/limited; B300 requires CUDA 13.1+.
"A10G" appears in Modal's own blog (2025-08-18) and one pricing FAQ example,
but is **not** in the current canonical list — the current id is `A10`; formal
deprecation of "A10G" is not documented (see Unverified).

**Q2 — Per-second GPU/CPU/memory pricing (as published)**
Source: https://modal.com/pricing — Confidence: high (per-second rates as
published; per-hour column computed ×3600 and marked as such)

| GPU type (id) | USD / second | USD / hour (computed) |
|---|---|---|
| B300 | 0.001972 | 7.0992 |
| B200 | 0.001736 | 6.2496 |
| H200 SXM | 0.001261 | 4.5396 |
| H100 SXM5 | 0.001097 | 3.9492 |
| RTX PRO 6000 | 0.000842 | 3.0312 |
| A100, 80 GB | 0.000694 | 2.4984 |
| A100, 40 GB | 0.000583 | 2.0988 |
| L40S | 0.000542 | 1.9512 |
| A10 | 0.000306 | 1.1016 |
| L4 | 0.000222 | 0.7992 |
| T4 | 0.000164 | 0.5904 |

CPU: $0.0000131 per core/sec ($0.04716/core/hr computed); minimum 0.125
cores per container; 1 physical core = 2 vCPU.
Memory: $0.00000222 per GiB/sec ($0.007992/GiB/hr computed); minimum 128 MiB
per container.
Volumes: $0.09/GiB/month, first 1 TiB/month free. Network egress: $0.04/GiB.
Billing is per-second with no minimum usage-time increments
(https://modal.com/docs/guide/billing.md).

**Q3 — Free / Starter plan credits and GPU availability**
Source: https://modal.com/pricing (Starter plan row) — Confidence: high
Starter: $0/month + compute, **$30/month free compute credits**, 3 workspace
seats, 100 containers + 10 GPU concurrency, Scheduled and Web Functions
(limited), region selection, 1 TiB/month free egress. Team: $250/month +
$100/month free credits. Enterprise: custom.
Source: https://modal.com/docs/guide/billing.md — Confidence: high
"You must have a payment method on file in order to use Modal."
Source: https://modal.com/docs/guide/gpu.md — Confidence: high
"Using a GPU requires having a valid payment method on file."
GPU types NOT available on the free plan: **not documented**. The docs do not
draw a per-GPU-type free vs paid distinction; they state a platform-wide
payment-method requirement and list 10 GPU concurrency as included on Starter.
Any claim that specific GPU types are paywall-only is unsupported.

**Q4 — GPU regions/availability**
Source: https://modal.com/docs/guide/region-selection.md — Confidence: high
No per-GPU-region restriction is documented. `region=` is a per-function /
per-sandbox setting (broad regions 1.15x, narrow regions 1.75x multiplier on
base pricing). Routing region defaults to `us-east`. GPU availability "can
fluctuate, especially for tightly-constrained requests" (e.g. 8 co-located GPUs
in a specific region) — https://modal.com/docs/examples/gpu_fallbacks.md.

**Q5 — Proxy Tokens**
Source: https://modal.com/docs/guide/webhook-proxy-auth.md — Confidence: high
Header names: `Modal-Key` (token id) + `Modal-Secret` (token secret), or a
single `Authorization: Bearer <token_id>.<token_secret>`. Proxy-token id/secret
prefixes: `wk-` / `ws-` (docs example: `wk-1234abcd` / `ws-1234abcd`).
Tokens created in Dashboard settings or `modal workspace proxy-tokens create`.
Endpoint protection: Endpoints and Servers require authentication by default
(`--unauthenticated` / `unauthenticated=True` to allow public traffic); Web
Functions are public by default and are protected with
`requires_proxy_auth=True` on `fastapi_endpoint`/`asgi_app`/`wsgi_app`/
`web_server`. Workspace managers can block unauthenticated URLs entirely.
Correlation with project doc: `docs/providers/modal-vllm.md` §Auth matches
this (combined `TOKEN_ID.TOKEN_SECRET` form, `Modal-Key`/`Modal-Secret`
headers); the linked URL https://modal.com/docs/guide/webhook-proxy-auth.md is
valid and consistent.
API-token prefixes (separate from proxy tokens): `~/.modal.toml` example uses
`ak-` / `as-` for `token_id`/`token_secret` set via `modal token set`.

**Q6 — Current (2025/2026) deployment API and renames**
Source: https://modal.com/docs/sdk/py/latest/App — Confidence: high
- `modal.App` — current constructor; `App("name")`, `App.lookup(name,
  create_if_missing=True)`.
- `@app.function(image, secrets, gpu, timeout=300, startup_timeout,
  scaledown_window, min_containers, max_containers, buffer_containers, region,
  cloud, nonpreemptible, single_use_containers, ...)` — current decorator.
- `app.deploy(name, environment_name, tag, strategy="rolling"|"recreate")` —
  documented as "a programmatic alternative to the `modal deploy` CLI command".
  No top-level `modal.deploy()` function exists; `modal deploy` is the CLI.
- `@app.server(port=8000, unauthenticated=False, startup_timeout=30, ...)` —
  current primitive for long-lived HTTP servers (replaces ad-hoc web-serving
  patterns); `web_server` decorator (`@app.function()` + `@modal.web_server(8000)`)
  also exists for non-ASGI servers.
- `secrets=[modal.Secret.from_name(...)]` — current `secrets` parameter on
  `@app.function` / `@app.cls` / `@app.server`.
- `allow_concurrent_inputs` — **removed/replaced**: superseded by
  `@modal.concurrent(max_inputs=, target_inputs=)` (added in modal v0.73.148;
  https://modal.com/docs/sdk/py/latest/concurrent).
- `max_inputs` — **deprecated**, replaced by `single_use_containers`.
- `container_idle_timeout` — **not found** in current docs; the documented
  idle-scale-down parameter is `scaledown_window`.
- `--detach`: exists on `modal run -d/--detach` ("Don't stop the app if the
  local process dies or disconnects") and on `App.run(detach=...)`; it is **not**
  a flag of `modal deploy` (deploy flags: `--name`, `-e/--env`, `--stream-logs`,
  `--tag`, `-m`, `--timestamps`, `--strategy`).
- Autoscaling can be updated without redeploy via
  `Function.autoscaler.update(min_containers/max_containers/buffer_containers)`
  (reverts to decorator config on next deploy)
  https://modal.com/docs/guide/scale.md.

**Q7 — Public REST API for app deployment**
Source: https://modal.com/docs/sdk/py/latest/App (`app.deploy` description) —
Confidence: high
The documented ways to create/update a deployment are the `modal deploy` CLI
and the `App.deploy()` Python method ("a programmatic alternative to the
modal deploy CLI command"). No official public REST API for creating/updating
app deployments is documented. Third-party API mirrors exist
(e.g. modal.rest, an api-evangelist OpenAPI mirror on GitHub) but are not
Modal's official documentation. Token needed for CLI/SDK: Modal API tokens
(`MODAL_TOKEN_ID` / `MODAL_TOKEN_SECRET`); endpoint calls use Proxy Tokens.

**Q8 — `modal deploy` authentication**
Source: https://modal.com/docs/reference/modal.config — Confidence: high
Env vars: `MODAL_TOKEN_ID`, `MODAL_TOKEN_SECRET`. File: `~/.modal.toml`
(overridable via `MODAL_CONFIG_PATH`), with profile sections, e.g.
`[ default ] token_id = "..." token_secret = "..."`. CLI: `modal token set
--token-id <id> --token-secret <secret>` (value `-` reads from stdin).
Profiles: `MODAL_PROFILE` env var (default `"default"`), `modal profile`
command, `config_profiles()` / `config_set_active_profile()` in Python.
OAuth integrations inject `MODAL_OAUTH_REFRESH_TOKEN`,
`MODAL_OAUTH_CLIENT_ID`, `MODAL_OAUTH_CLIENT_SECRET` or `MODAL_OAUTH_JWT_KEY`.

**Q9 — Cold start / scale-to-zero semantics for web endpoints**
Source: https://modal.com/docs/guide/cold-start.md — Confidence: high
Functions scale to zero by default; when no warm container exists, new inputs
**queue** for a container to boot; a container is not considered warm until all
`modal.enter` hooks complete, and no inputs are routed to a not-yet-ready
container (https://modal.com/docs/guide/scale.md; LLM guide
https://modal.com/docs/guide/high-performance-llm-inference.md).
Modal docs **do not document an explicit 503** on cold start for web
endpoints/functions; they describe queueing. Whether the Modal proxy returns
503 or holds the HTTP connection during scale-up is not stated in the docs
(see Unverified).

**Q10 — `scaledown_window` default and limits**
Source: https://modal.com/docs/guide/cold-start.md — Confidence: high
Default: **60 seconds** (maximum idle time before a container shuts down).
Allowed range: **2 seconds to 20 minutes (1200 seconds)**. Meaning: max time
a container may remain idle while scaling down (per `@app.function` /
`@app.cls` / `@app.server`). The autoscaler may scale down sooner when the
function is substantially over-provisioned.

## Corrections to existing project assumptions

1. `docs/providers/modal-vllm.md` §"Cold starts and fallback" assumes a **503**
   from a scaled-to-zero Modal deployment, with the alternative being a
   deployment that "holds the connection open". Modal's docs document **queueing
   during cold start**, not an explicit 503; neither behavior (503 vs held
   connection) is confirmed by official docs. Treat the project's 503 handling
   as an assumption to verify against the real deployment, not as documented
   Modal behavior.
2. `docs/providers/modal-vllm.md` §Auth is **consistent** with current docs:
   combined `TOKEN_ID.TOKEN_SECRET`, headers `Modal-Key`/`Modal-Secret`, linked
   proxy-auth guide URL valid. Proxy-token prefixes are `wk-`/`ws-`; note that
   API tokens (`.modal.toml`, `modal token set`) use `ak-`/`as-` prefixes in
   Modal's own example — do not conflate the two.
3. GPU id `A10G` (used in the task's example list and possibly in hardcoded
   project values) is **not** a current canonical Modal id; the current id is
   `A10`. "A10G" survives in Modal's blog/FAQ examples; formal deprecation is
   undocumented (Unverified).
4. `crates/mr-core/src/quota/modal.rs`: contains no factual claims about Modal
   (it deliberately uses an operator-declared quota endpoint and fabricates no
   Modal URL). No correction needed.

## Unverified / open

- Whether any specific GPU type is unavailable on the Starter/free plan. Docs
  state a platform-wide payment-method requirement and list "10 GPU concurrency"
  on Starter, but no per-GPU-type free/paid distinction is documented.
- Whether Modal's proxy/web endpoints return HTTP 503 or hold the connection
  during a cold start; docs only describe queueing.
- `container_idle_timeout`: not present in current docs; possibly an older name
  superseded by `scaledown_window` — not confirmed.
- Formal deprecation status of the legacy GPU id `A10G`.
- Any GPU type explicitly marked "preview" or "limited": none found in docs.
- A public REST API for app deployment: none documented; absence is not proven
  (only the CLI and `App.deploy()` are documented).

## Copy-paste constants (retrieved 2026-11-12)

Sources: https://modal.com/docs/guide/gpu.md (ids), https://modal.com/pricing
(per-second rates). Per-hour column is computed (×3600), not published.

```json
{
  "retrieval_date": "2026-11-12",
  "sources": [
    "https://modal.com/docs/guide/gpu.md",
    "https://modal.com/pricing",
    "https://modal.com/docs/guide/webhook-proxy-auth.md",
    "https://modal.com/docs/reference/modal.config",
    "https://modal.com/docs/sdk/py/latest/App",
    "https://modal.com/docs/sdk/py/latest/concurrent",
    "https://modal.com/docs/guide/cold-start.md",
    "https://modal.com/docs/guide/scale.md",
    "https://modal.com/docs/guide/billing.md",
    "https://modal.com/docs/guide/region-selection.md"
  ],
  "gpu_ids": ["T4","L4","A10","L40S","A100","A100-40GB","A100-80GB","RTX-PRO-6000","H100","H200","B200","B300"],
  "gpu_id_modifiers": {"H100!": "opt out of automatic H100->H200 upgrade", "B200+": "B200 or B300, billed as B200"},
  "max_gpus_per_container": {"A10": 4, "default_others": 8},
  "price_usd_per_second": {
    "B300": 0.001972, "B200": 0.001736, "H200": 0.001261, "H100": 0.001097,
    "RTX_PRO_6000": 0.000842, "A100_80GB": 0.000694, "A100_40GB": 0.000583,
    "L40S": 0.000542, "A10": 0.000306, "L4": 0.000222, "T4": 0.000164
  },
  "cpu_usd_per_core_second": 0.0000131,
  "memory_usd_per_gib_second": 0.00000222,
  "starter_free_credits_usd_per_month": 30,
  "scaledown_window": {"default_seconds": 60, "min_seconds": 2, "max_seconds": 1200},
  "proxy_token_headers": ["Modal-Key", "Modal-Secret"],
  "proxy_token_prefixes": {"id": "wk-", "secret": "ws-"},
  "auth_env": ["MODAL_TOKEN_ID", "MODAL_TOKEN_SECRET"],
  "config_file": "~/.modal.toml",
  "config_profile_env": "MODAL_PROFILE"
}
```

| GPU id | USD / sec | USD / hr (computed) |
|---|---|---|
| B300 | 0.001972 | 7.0992 |
| B200 | 0.001736 | 6.2496 |
| H200 | 0.001261 | 4.5396 |
| H100 | 0.001097 | 3.9492 |
| RTX PRO 6000 | 0.000842 | 3.0312 |
| A100-80GB | 0.000694 | 2.4984 |
| A100-40GB | 0.000583 | 2.0988 |
| L40S | 0.000542 | 1.9512 |
| A10 | 0.000306 | 1.1016 |
| L4 | 0.000222 | 0.7992 |
| T4 | 0.000164 | 0.5904 |
| CPU (per core) | 0.0000131 | 0.04716 |
| Memory (per GiB) | 0.00000222 | 0.007992 |