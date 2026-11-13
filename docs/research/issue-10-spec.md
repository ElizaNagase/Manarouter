# ISSUE 10: Deploy Cerebrium as a first-class serverless inference provider, with multi-account switching
# Deploy Cerebrium as a first-class serverless inference provider, with multi-account switching

> **Historical (2026-11-13).** This document specifies Cerebrium. The Cerebrium lane, its routes
> and its `cerebrium.toml` generator were removed from Manarouter on 2026-11-13, so nothing below
> is implemented any more. It is kept as the research and design record; the lane surface that is
> live today is [../26-gpu-lanes.md](../26-gpu-lanes.md).

Supersedes #6, which framed Cerebrium as "a provider entry". That is not enough: the endpoint does
not exist until an app is deployed, the account is a *project + service-account token* pair, and the
free plan caps how many apps and concurrent GPUs one account may hold. This issue covers deploying
the platform **for inference** and switching between several Cerebrium accounts without operator
intervention.

## Why this is the one to build first

Cerebrium is the only one of the four platforms in this batch that gives a real, always-addressable
HTTPS inference endpoint on a free account, so it is the natural place to build the shared **lane**
machinery that #11, #12 and #13 will reuse.

## Research findings (all provider-controlled; re-verify before implementing)

* **Free tier is a free *plan*, not free GPU hours.** The Hobby plan is `Free + compute / month`:
  3 user seats, up to **3 deployed apps**, 500 container concurrency and **5 concurrent GPUs**,
  1-day log retention. Standard is `$100 + compute / month` (unlimited seats/apps, 1000 containers,
  30 GPU concurrency, 30-day logs). [pricing](https://www.cerebrium.ai/pricing)
* **Billing is per second** on compute (e.g. `$0.000306/s` per GPU-second on the published rates;
  memory `$0.00000222 /GB/s`; storage `$0.05 /GB/mo`, first 100 GB free). Idle scale-to-zero is the
  default, so a *warm* endpoint costs money continuously and a *cold* one costs a cold start.
* **Every function is an HTTP endpoint, and auth is opt-out:** `POST
  https://api.cerebrium.ai/v4/p-xxxxxxxx/{app}/{function}`, `Authorization: Bearer <JWT>`,
  returning `{"run_id", "run_time_ms", "result"}`. Crucially, "Endpoints require this token only
  when `cerebrium.toml` sets `disable_auth = false`. **Authentication is disabled by default.**"
  ([REST API](https://cerebrium.ai/docs/endpoints/inference-api.md))
* **OpenAI-compatible surface:** a Deployable can expose `/chat/completions` and `/embedding`, with
  the base URL `https://api.cerebrium.ai/v4/p-xxxxxxxx/<APP>/run` used directly as the OpenAI client
  `base_url`, JWT as the API key.
  ([OpenAI-compatible endpoints](https://cerebrium.ai/docs/endpoints/openai-compatible-endpoints.md))
* **Async mode:** `?async=true` returns `202 Accepted` + `run_id` immediately, work may run up to
  **12 hours**, bounded by `response_grace_period` (default 15 min); results leave the function via
  `?webhookEndpoint=`.
  ([Async requests](https://cerebrium.ai/docs/endpoints/async.md))
* **Headless auth:** `cerebrium login` opens a browser and fails without a TTY; CI/headless use
  `CEREBRIUM_SERVICE_ACCOUNT_TOKEN` or `--service-account-token`. `cerebrium projects current` tells
  you which project the CLI is pointed at. A `cerebrium.toml` key left out is **reset to its
  default on deploy**, and deploys cost money.
  ([skill](https://cerebrium.ai/skill.md), [CLI reference](https://github.com/CerebriumAI/cerebrium-skills/blob/master/skills/cerebrium/references/cli.md))
* API keys are per-project and CRUD-able (create/list/delete) — the switching primitive is a
  *service-account token per project*, not a single global key.
  ([API keys](https://cerebrium.ai/docs/api-reference/api-keys/list-api-keys.md))

## Shared lane infrastructure (this issue builds it)

New crate `crates/mr-lane`, binary `mr-lane`, plus a `mr lane …` verb group in `crates/mr-cli`.

```
LaneBackend (trait)                 Lane definition (config.json)
  async fn provision(&Lane)         { id, kind, account, hardware,
  async fn await_ready(&Lane)         model, port, max_runtime_s,
  async fn endpoint(&Lane)            idle_ttl_s, start_script }
  async fn teardown(&Lane)
  async fn usage(&Lane) -> Usage
```

`mr-lane` runs an adapter HTTP listener on loopback that speaks the OpenAI surface
(`/v1/chat/completions`, `/v1/embeddings`, `/v1/models`) and translates to the backend's native call.
The lane is then registered as an **ordinary provider**:

```bash
mr provider add openai-compatible --id lane-cerebrium --base-url http://127.0.0.1:20910/v1
printf '%s' "$LANE_TOKEN" | mr account add lane-cerebrium --label acct-a --secret-stdin
```

This is the central design decision and it is deliberate:

* **No new dialect and no new code on the relay request path.** The relay keeps its
  compiled-snapshot, no-JS-on-the-request-path property; a lane is just another OpenAI-compatible
  upstream at `127.0.0.1`.
* **Account switching is inherited, not reinvented.** `KeyStrategy` (`priority`, `round-robin`,
  `random`, `least-used`), `max_concurrent`, `models_allow`/`models_deny`, sticky routing
  (`crates/manarouter-relay/src/sticky.rs`), cooldown scope and quota admission
  (`crates/manarouter-relay/src/quota.rs`) all work unchanged, because the lane exposes one
  *account per upstream account*.
* **Dynamic registration needs no restart.** The relay's config reloader fingerprints
  `~/.config/manarouter/config.json` every 2 s
  (`crates/manarouter-relay/src/startup.rs::spawn_config_reloader`), so `mr-lane` can write a
  provider in/out as lanes come up and die.

### Account switching, concretely

Three layers, in order:

1. **Lane accounts** — one vault credential per platform account (`--secret-stdin`, encrypted,
   never in `config.json`; `AGENTS.md` discipline). Secrets live in the vault, `config.json` holds
   only the reference.
2. **Relay selection** — the provider's `key_strategy` picks among *live* lane accounts per request:
   `priority` keeps a warm app hot, `least-used` spreads cold starts, `round-robin` shares spend.
3. **Lane-level rotation** — the adapter reports an account dead to the relay by returning the
   upstream's own status (401/403 → auth, 429 → concurrency/quota, 5xx → app), so the existing
   account cooldown and reroute logic evicts it; `mr-lane` then provisions that account's app out of
   band and re-adds it.

Per-account state is persisted where the panel already looks: `provider_quota_state`
(`crates/mr-core/src/store.rs`, keyed `(provider_id, account_id)`, payload cached verbatim) for
last-known remaining GPU-seconds / app slots, `fetched_at` + `source` recorded honestly.

## Cerebrium backend

* `deploy/cerebrium/` in this repo holds the deployable: `main.py` (vLLM or SGLang, model chosen by
  env), `cerebrium.toml` (`compute`, `gpu_count`, `region`, `min_replicas = 0`,
  `response_grace_period` sized to the longest completion, `disable_auth = false`), and a
  `README.md` that states the account/app/GPC caps in force.
* `mr lane up cerebrium --account <label> --model <hf-id> --gpu <id>` →
  `CEREBRIUM_SERVICE_ACCOUNT_TOKEN=<token> cerebrium deploy` from a per-account working directory
  (the CLI's own config directory is isolated per account, mirroring the `KAGGLE_CONFIG_DIR`
  approach in #11), then poll until `GET /v1/models` answers, then register the provider.
* `mr lane down cerebrium --account <label>` → `cerebrium apps delete`, provider removed, usage row
  written.
* Cap enforcement: never provision more apps per account than the plan allows (3 on Hobby), never
  exceed its GPU concurrency (5); queue or 429 at the adapter boundary rather than letting the
  platform refuse mid-generation.
* Cold-start honesty: a scale-to-zero app can take tens of seconds to answer the first request. The
  adapter does not hide it; it returns 503 with `Retry-After` when the request exceeds the
  configured grace, and `mr lane status` prints warm/cold per lane.

## Non-goals

* No automatic plan upgrade, no billing automation, no attempt to obtain more than the plan allows.
* No provider-specific dialect: everything above compiles down to `open-ai`.
* No secrets in `config.json`, in argv, or in logs (`--secret-stdin`/env only).

## Work breakdown

1. `crates/mr-lane`: `LaneBackend` trait, lane registry, account pool, config writer, adapter
   listener, idle reaper, usage accounting. Unit-tested against `tools/mock-upstream`.
2. `crates/mr-cli`: `mr lane up|down|status|attach|list`.
3. `deploy/cerebrium/` deployable + a documented one-command path.
4. Docs: `docs/24-gpu-lanes.md` (new), `docs/LLM_INDEX.md` task map entry,
   `docs/23-implementation-status.md` row stating exactly what runs and what is intended.
5. Panel: read-only lane list reusing `provider_quota_state` (no new write path).

## Testing

* Backend lifecycle against a fake Cerebrium API in `tools/mock-upstream`: deploy → ready → 200 on
  `/v1/chat/completions` → delete.
* Account rotation: two lane accounts, first returns 401, second serves, assert the relay evicted
  the first and no request failed.
* Cold start: adapter returns 503 + `Retry-After`; a relay reroute lands on a warm account.
* Secret hygiene: secret-scan test must fail if a token or JWT reaches `config.json` or a log line.
* No test may call the real API and spend money; live checks are opt-in and documented as such.

## Acceptance criteria

* `mr lane up cerebrium` produces a working OpenAI-compatible endpoint registered in `config.json`,
  visible to the relay within one 2 s reload, with the credential in the vault only.
* Two Cerebrium accounts can be pooled; killing one app mid-flight reroutes to the other with no
  client-visible failure other than a retry.
* `mr lane down` leaves no billable app behind and removes the provider.
* Docs state plainly which parts are implemented and which are intended
  (`docs/23-implementation-status.md` discipline).

