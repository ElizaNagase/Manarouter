# Cerebrium platform facts (issue #10)

> **Historical (2026-11-13).** This document specifies Cerebrium. The Cerebrium lane, its routes
> and its `cerebrium.toml` generator were removed from Manarouter on 2026-11-13, so nothing below
> is implemented any more. It is kept as the research and design record; the lane surface that is
> live today is [../26-gpu-lanes.md](../26-gpu-lanes.md).

Verified against primary sources on **2026-11-12**. `web_fetch` is unavailable in this
environment, so every source below was fetched with `curl -sS -m 20 -L` and returned HTTP 200.
Docs base is `https://cerebrium.ai/docs/`: a source cell reading `hardware/using-gpus` means
`https://cerebrium.ai/docs/hardware/using-gpus`. Docs pages were read from the official
whole-corpus file `https://cerebrium.ai/docs/llms-full.txt` (each page carries its own `Source:`
URL, which is the URL recorded here). Status: **V** = read a primary source; **U** = unverified.

The docs' own `/docs/api-reference/*` pages are 3-to-5-line stubs that only link the machine
spec at `https://s3.eu-west-1.amazonaws.com/www.cerebrium.ai/openapi_spec.json` (OpenAPI 3.0,
72 paths, server `https://rest.cerebrium.ai`). That spec plus the CLI's Go source
(`https://github.com/CerebriumAI/cerebrium`, branch `main`) are therefore the only real
authority for the control-plane REST surface.

## Q1 — How you deploy, and whether REST is usable from CI

| fact | value | source | st |
|---|---|---|---|
| Primary deploy mechanism | `cerebrium deploy` CLI (`pip install cerebrium`, thin wrapper over a Go binary), uploads a zip of the project | `deployments/deploying-apps`; `https://pypi.org/pypi/cerebrium/json` | V |
| REST deploy, step 1 | `POST https://rest.cerebrium.ai/v2/projects/{project_id}/apps`, `Authorization: Bearer <service-account-token>`, JSON body = app config; returns `{buildId, status, uploadUrl, keyName, internalEndpoint, dashboardUrl}` | spec path `/v2/projects/{project_id}/apps`; `cerebrium/main/internal/api/client.go` | V |
| REST deploy, step 2 | `PUT <uploadUrl>` with `Content-Type: application/zip` — the code upload is a second, pre-signed request | `internal/api/client.go` | V |
| REST maturity | the spec calls itself "mainly used by the Cerebrium CLI client"; there is **no** endpoint named `deploy`, and no public versioning/breaking-change promise | spec `info.description` | V |
| Official CI recipe | GitHub Actions + a dashboard-created Service Account key exported as `CEREBRIUM_SERVICE_ACCOUNT_TOKEN`; `cerebrium deploy -y --detach` is non-interactive, `-y` = `--disable-confirmation` | `deployments/ci-cd`; `internal/commands/deploy.go` | V |
| Other CI-relevant verbs | `apps list/get`, `apps scale APP_ID --min-replicas --max-replicas --cooldown --response-grace-period`, `logs`, `containers list`, `runs list`, `secrets add/list`, `projects set/current`, `region set`; **no** `builds` command | `cli/cli-reference`; `internal/commands/` | V |

## Q2 — Unit of deployment, URL shape, OpenAI compatibility

| fact | value | source | st |
|---|---|---|---|
| Unit | the **app** (project-scoped); one app = one build, one `cerebrium.toml`, one endpoint tree. `app_id` = `{project_id}-{app_name}` in CLI code (`fmt.Sprintf("%s-%s"…)`) | `internal/api/client.go`, `internal/commands/logs.go` | V |
| Cortex (default runtime) endpoint | `POST https://api.cerebrium.ai/v4/{PROJECT_ID}/{APP_NAME}/{FUNCTION_NAME}`; project id already starts with `p-` ("Do not add a second `p-`"); functions whose name starts with `_` are not exposed | `endpoints/inference-api` | V |
| Cortex response envelope | `{"run_id": "...", "run_time_ms": 0, "result": …}` | `endpoints/inference-api` | V |
| Custom runtime (`vllm serve`) | path **passthrough**: everything after `/{app}/` is proxied to the container port. Official vLLM example calls `https://api.cerebrium.ai/v4/p-xxxxxxxx/7-openai-gpt-oss/v1/chat/completions`; a Deepgram app is called at `…/deepgram/v1/listen?model=nova-3&smart_format=true` (query string preserved) | `v4/examples/gpt-oss`; `v4/examples/deepgram` | V |
| ⇒ vLLM base URL for Manarouter | `https://api.cerebrium.ai/v4/{project_id}/{app_name}/v1` — **not** `…/{app}/run` | derived from the two rows above | V |
| OpenAI compatibility | real: an actual `vllm serve` process answers `/v1/chat/completions` and streams SSE. The docs' claim that "all Cerebrium endpoints are OpenAI-compatible" is only true for Cortex apps if you hand-write a `run(messages, model, run_id, stream)` generator that yields `data: {...}` frames | `v4/examples/gpt-oss`; `endpoints/openai-compatible-endpoints` | V |
| Health checks | `healthcheck_endpoint` / `readycheck_endpoint` empty ⇒ plain TCP check on `port`; a non-200 ⇒ restart / no routing | `container-images/custom-web-servers` | V |
| Deploy-time sanity | `deployment_initialization_timeout` default 600 s, range 60–7200 — the app must be listening within it | spec create-app; `toml-reference/toml-reference` | V |

## Q3 — Authentication and tokens

| fact | value | source | st |
|---|---|---|---|
| Auth default | `disable_auth` defaults to **true** in both the REST schema and the TOML reference ⇒ a deploy that omits it publishes a public endpoint. The docs attach an explicit WARNING callout | spec create-app; `toml-reference/toml-reference` | V |
| Inference bearer | project **API key**, sent as `Authorization: Bearer <key>`; deleting the key makes "inference requests using the key start failing within a few seconds" | `api-reference/api-keys/*` | V |
| Minting an inference key headlessly | `POST /v2/projects/{project_id}/api-keys?name=&expiresAt=YYYY-MM-DD` returns `{"apiKey": "<secret value>", "id", "expiresAt", …}` — the only documented way to get a key without the dashboard | spec `/v2/projects/{project_id}/api-keys` | V |
| Control-plane bearer | **service-account token** (`BearerAuth`, "Service Account Token authentication"), created in dashboard → API Keys → "Create Service Account", with an expiry; listed at `GET /v2/projects/{project_id}/service-accounts[/keys]` | spec `components.securitySchemes`; `api-reference/service-accounts/*` | V |
| Token kinds are not interchangeable | API keys are for inference; service-account keys are rejected by the API-key endpoints ("Service account keys are rejected here — delete the service account instead") | `api-reference/api-keys/delete-api-key` | V |
| Project inference errors | `401` missing/invalid key, `403` wrong project, `404` no such app/function, `5xx` container failure | `endpoints/inference-api` | V |

## Q4 — GPU catalogue: id, VRAM, price, cap, plan gate

Prices are the published per-second rates from `https://cerebrium.ai/pricing` (**V**). The
per-hour column is **derived** (×3600) — the pricing page only renders per-second values
server-side. `compute` in `cerebrium.toml` / the REST `compute` array is one of these 13 ids.

| `compute` id | GPU | VRAM GB | USD/s | USD/hr (derived) | Max `gpu_count` | Plan |
|---|---|---|---|---|---|---|
| `BLACKWELL_B200` | B200 | 180 | 0.00167 | 6.012 | 8 | Standard+ |
| `HOPPER_H200` | H200 | 141 | 0.001166 | 4.1976 | 8 | Standard+ |
| `HOPPER_H100` | H100 | 80 | 0.000944 | 3.3984 | 8 | Standard+ |
| `BLACKWELL_RTX6000` | RTX PRO 6000 | 96 | 0.000694 | 2.4984 | 8 | Standard+ |
| `AMPERE_A100_80GB` | A100 80 GB | 80 | 0.000583 | 2.0988 | 8 | Standard+ |
| `AMPERE_A100_40GB` | A100 40 GB | 40 | 0.000555 | 1.998 | 8 | Standard+ |
| `ADA_L40` | L40s | 48 | 0.000542 | 1.9512 | 8 | Hobby+ |
| `AMPERE_A10` | A10 | 24 | 0.000306 | 1.1016 | 8 | Hobby+ |
| `ADA_L4` | L4 | 24 | 0.000222 | 0.7992 | 8 | Hobby+ |
| `TURING_T4` | T4 | 16 | 0.000164 | 0.5904 | 8 | Hobby+ |
| `INF2` | Inferentia2 | 32 | not published | — | 8 | Hobby+ |
| `TRN1` | Trainium | 32 | not published | — | 8 | Enterprise |
| `CPU` | — | — | 0.00000655 /vCPU/s | 0.02358 /vCPU/hr | — | Hobby+ |

| other GPU facts | value | source | st |
|---|---|---|---|
| `compute` is an array | `compute` accepts up to 5 ids "in priority order" (fallback across GPU types), not a single value | spec create-app `compute` | V |
| Tier | `computeTier` ∈ `interruptible` (spot, default) \| `protected` (on-demand); `protected` is "billed at 2x the interruptible rate" | `toml-reference/toml-reference` | V |
| `gpuCount` bounds | 0–16 in the schema, default **0**; per-hardware max CPU/memory also cap the pair (e.g. H100 24 vCPU / 256 GB, A10 48 / 192) | spec create-app; `hardware/cpu-and-memory` | V |
| `BLACKWELL_B300` | "appears in the published GPU table and is **not** in the accepted set. Do not use it." | `references/hardware.md` (cerebrium-skills) | V |
| Regions | `us-east-1` (N. Virginia), `us-central1` (Kansas City, nebius), `eu-north-1` (Stockholm), `eu-north1` (Finland, nebius — different id!), `global`; per-region accelerator matrix differs | `deployments/regions` | V |

## Q5 — Scaling knobs (the `scaledown_window` analogue and cold start)

| knob (TOML / REST) | default | range | note |
|---|---|---|---|
| `min_replicas` / `minReplicaCount` | 0 | 0–2000 | >0 kills cold starts, bills idle |
| `max_replicas` / `maxReplicaCount` | **1** | 1–2000 | must be raised or you can never burst |
| `cooldown` / `cooldownPeriodSeconds` | **10** | 0–3600 | the scale-down window: seconds at reduced concurrency before a container dies |
| `response_grace_period` / `responseGracePeriodSeconds` | **900** | 16–43200 | doubles as the per-request timeout; on shutdown → SIGTERM, wait, SIGKILL, `GatewayTimeout` in-flight |
| `replica_concurrency` | 100 CPU / **1 GPU** | — | hard per-container request cap; exceed it and the platform adds containers 1:1 |
| `scaling_metric` / `scalingMetric` | `concurrency_utilization` | also `requests_per_second`, `cpu_utilization`, `memory_utilization` | |
| `scaling_target` / `scalingTarget` | 100 | — | |
| `scaling_buffer` | — | — | only valid with `concurrency_utilization` or `requests_per_second` |
| `evaluationIntervalSeconds` | 30 | 6–300 | |
| `loadBalancingAlgorithm` | `round-robin` | — | docs recommend concurrency-based for GPU apps with `replica_concurrency <= 3` |
| Source for the table above | spec create-app property descriptions; `scaling/scaling-apps`; `toml-reference/toml-reference` — all **V** | | |

Cold start (`performance/faster-cold-starts`, `migrations/hugging-face`, **V**): it is two phases,
**queueing** then **initialization**. Published figures: "a standard Hugging Face load can take
40+ seconds even when reading from storage at ~2 GB/s"; Tensorizer cuts that "30–50%" for 20B+
models; the migration page claims apps "start from a cold state in an average of 2–5 seconds"
(platform average, not a vLLM measurement) and that a warm request costs "up to 50ms". No
vLLM-specific cold-start duration is published (`cerebrium_run_coldstart_time_ms` is a metric you
can measure yourself). Mitigations: module-scope init, weights in `/persistent-storage` (cached
per region) or `/global-persistent-storage`, memory checkpointing, `min_replicas`, `scaling_buffer`.

## Q6 — Plans, free tier, per-GPU gating

| plan | price | apps | concurrency | GPU gate |
|---|---|---|---|---|
| Hobby | Free + compute | 3 | "500 containers + 5 Concurrent GPUs" (card) / "10 concurrent GPUs is the limit on Cerebrium's Hobby plan" (example page) | 6 types: CPU, T4, A10, L4, L40s, INF2 |
| Standard | $100/mo + compute | unlimited | "1000 containers + 30 GPU concurrency" | 12 types: + A100-40/80, H100, H200, B200, RTX6000 |
| Enterprise | custom | unlimited | unlimited | all 13 (+ TRN1) |

Sources: `https://cerebrium.ai/pricing` (cards + comparison table), `hardware/using-gpus`
("Plan required" column), `v4/examples/wandb-sweep`. All **V**.

| further plan facts | value | source | st |
|---|---|---|---|
| Gating is enforced | "Deploying with a compute type outside the project's plan is rejected at deploy time"; per-project exceptions via sales@ | `hardware/using-gpus` | V |
| Free GPU hours | **none found.** Hobby is a free *plan*; GPU time is billed at the per-second rates above from the first second | `https://cerebrium.ai/pricing` | V |
| Programmatic plan change | `PATCH /v2/projects/{project_id}/subscription` body `{"planCode":"hobby"\|"standard"\|"enterprise"}` | spec | V |
| Logs / audit by plan | logs 7 d (Hobby) / 30 d (Standard); audit log **not included** on Hobby, 7 d Standard, 30 d Enterprise | `pricing`; `other-topics/audit-log` | V |

## Q7 — Secrets and non-interactive supply

| fact | value | source | st |
|---|---|---|---|
| CLI | `cerebrium secrets add KEY=VALUE [--app APP_ID]`, `secrets list [--show-values]` — value is in **argv** (visible in `ps`/history); no stdin form is documented | `cli/cli-reference` | V |
| REST | `GET`/`PATCH /v2/projects/{project_id}/secrets` and `GET`/`PATCH /v2/projects/{project_id}/apps/{app_id}/secrets`; the spec documents **no requestBody schema** for the PATCHes | spec | V |
| Storage/delivery | encrypted at rest (256-bit AES), injected as environment variables; app secrets override project secrets; loaded at container start, so a running container must be restarted to pick up a change | `other-topics/using-secrets` | V |
| Platform-injected env | `APP_NAME`, `PROJECT_ID`, `BUILD_ID`, `HF_HOME=/persistent-storage/.cache/huggingface` | `other-topics/environment-variables` | V |

## Q8 — Headless-deploy blockers

| fact | value | source | st |
|---|---|---|---|
| Interactive login is dead in CI | "`login` fails without a TTY by design"; CI path is the service-account token | `deployments/ci-cd`; `cli/cli-reference` | V |
| Token precedence | `--service-account-token` flag > `CEREBRIUM_SERVICE_ACCOUNT_TOKEN` env > stored token | `internal/commands/root.go` | V |
| Project resolution | the SA token is a JWT carrying a `project_id` claim; the CLI reads the project from the config file (`~/.cerebrium/config.yaml`, key `project`), else parses the claim. **`CEREBRIUM_PROJECT_ID` is not read anywhere in the CLI source** — the docs' CI example is wrong | `pkg/config/config.go` | V |
| ⇒ truly minimal CI | only `CEREBRIUM_SERVICE_ACCOUNT_TOKEN` is needed; no `projects set`, no config file | derived | V |
| Other prompts | `-y`/`--yes` skips the confirmation prompt; `--detach` returns before the build finishes; UI degrades on non-TTY (`isatty` gate on the spinner), so no TTY is required | `internal/commands/deploy.go`, `internal/ui/displayconf.go` | V |
| Signup itself | whether signup needs email verification / TOTP / a card before the first deploy | — | **U** |

## Q9 — Terms of service: what is not prohibited

`https://cerebrium.ai/terms-of-service` (HTTP 200, 300 258 B) is a generic SaaS template. The clauses
that matter for this lane:

| Clause | Text that bears on it | What it means here |
|---|---|---|
| Accounts | "You guarantee that you are above the age of 18, and that the information you provide us is accurate…"; "You are responsible for maintaining the confidentiality of your account and password" | Nothing about how many accounts one person may hold, unlike Kaggle's Terms §3 |
| Prohibited Uses (automation) | "Use any robot, spider, or another automatic device, process, or means to access the Service for any purpose, including monitoring or copying any of the material on the Service" | Read with the bullet's own object ("monitoring or copying any of the material") and its neighbour ("any manual process to monitor or copy"), this targets harvesting the website. A published REST API and an official CLI are the documented way in, so programmatic deploy is not what this bullet reaches |
| Prohibited Uses (load) | "In any manner that could disable, overburden, damage, or impair the Service, or interfere with any other party's use" | A capacity-planning concern, not an account-policy one; batching inside one replica is the opposite of overburdening |
| Reservation | "We reserve the right to refuse service, terminate accounts, remove or edit content, or cancel orders at our sole discretion" | The usual escape hatch; present on both this and the Kaggle terms, and not a policy statement |

**Absent, and conspicuously so:** no clause prohibits multiple accounts, reselling compute, proxying
traffic, or exposing an app to third parties — the four things Kaggle's Terms §3, Kaggle's AUP and
molab's restrictions page name explicitly. So [`crate::lane::cerebrium`] records `accounts: allowed`,
and the honest gloss is "no document prohibits it", not "the platform permits it". Cerebrium has not
been asked the question #11 and #13 answer.

The other side of the ledger: the product is sold as *hosting an app other people call*, so the traffic
shape this lane serves is the one the pricing page is about. What the terms do not settle is resale —
taking money from third parties for the endpoint. That stays outside what this lane claims.

## Source conflicts (recorded, not resolved)

1. Max GPUs: docs table says **8 for every type**; `references/hardware.md` says 4 (T4/L4/A10/L40),
   8 (A100/RTX6000/H100/H200/B200), 12 (INF2), 16 (TRN1); the schema allows 0–16.
2. Hobby log retention: card "1 day" vs comparison table "7 day" vs audit page "7 day".
3. Standard seats: card "Unlimited" vs table "10". Standard apps: "Unlimited" card vs `100` in one ref.
4. Hobby concurrency: "5 Concurrent GPUs" (pricing card) vs "10 concurrent GPUs" (docs example).
5. `python_version` default: `3.11` (REST schema) vs `3.12` (TOML reference).
6. `app_id` join character: `-` (CLI Go source) vs `_` (secrets doc).
7. `rollOutDurationSeconds` max: 21600 (schema) vs 600 (TOML reference).
8. `deployment_initialization_timeout`: 60–7200 (schema) vs "60 to 830" (older config ref).

## What this means for Manarouter

- **Every GPU worth a vLLM lane is paid.** A100/H100/H200/B200/RTX6000 are "Standard+"; Hobby stops
  at A10/L4/L40s/T4 (24–48 GB), so the free path is an 8B in FP8 on L40s — issue #10's "(Paid)" marker is honest.
- **The endpoint is the custom-runtime one, not `/run`.** For `vllm serve`, base URL =
  `https://api.cerebrium.ai/v4/{project}/{app}/v1`; paths after `/{app}/` reach `port` verbatim.
  `…/{APP}/run` is the Cortex convention and would not serve vLLM.
- **`disable_auth` defaults to true**, so an omitted key ships a public endpoint (both official
  vLLM examples do). Emit `disable_auth = false` and send the bearer key.
- **Two credentials:** service-account token for deploy/scale/delete, project API key for inference —
  and `POST /v2/projects/{p}/api-keys` returns that key in its response body, so
  `mr lane up` needs no dashboard and no argv exposure.
- **Deploy by shelling out** to `cerebrium deploy -y --detach` with `CEREBRIUM_SERVICE_ACCOUNT_TOKEN`
  (the documented CI path); the two-step REST deploy (`POST …/apps` → `PUT uploadUrl`) is a fallback.
- **Pooling is cheap:** one project per account; the project id comes from the token's `project_id` claim, so `--account b` is just another token.
- **Override the hostile defaults:** `replica_concurrency` 1 on GPU, `cooldown` 10 s,
  `max_replicas` 1, `min_replicas` 0.
- **No published vLLM cold-start number.** Requests queue during a cold start instead of failing, so
  "503 + Retry-After" may never fire; expect a wait bounded by `response_grace_period` (900 s).
- **`mr lane down` needs `DELETE …/apps/{app}`**; `/persistent-storage` weights survive and bill at
  $0.05/GB/mo past 100 GB free, while compute stops at zero replicas.
- **Traps:** a missing key is *reset to default* (whole record, not a diff); `secrets add KEY=VALUE` puts the value in argv — use `PATCH …/secrets`.

## Could not verify

- Any published **vLLM** cold-start or tokens/s figure per GPU (only "40+ s" generic HF load,
  Tensorizer "30–50%", and a marketing "2–5 s average" for unspecified apps).
- The request body schema for `PATCH /v2/projects/{id}/secrets` and `…/apps/{id}/secrets`
  (the spec shows no body, and the CLI's JSON encoding was not traced).
- Whether a **service-account token is also accepted as an inference bearer** on
  `api.cerebrium.ai` (docs keep the two key types separate; not proven either way).
- Whether `disable_auth = false` makes the key **mandatory** or merely accepted, and what
  `401` body/headers come back (no auth-failure sample found).
- Signup friction before the first deploy: email verification, TOTP, or a card on file.
- INF2 / TRN1 per-second prices (not on the pricing page).
- Actual `max gpu_count` per type — three mutually inconsistent sources (conflict 1 above).
- Cold-start behaviour for a **custom runtime** specifically (the "waits for infrastructure"
  claim is on a migration page aimed at Cortex apps).
- Whether the `?async=true` 202 flow (12 h ceiling) applies to custom-runtime apps or only
  Cortex functions — the async page documents the Cortex shape.
