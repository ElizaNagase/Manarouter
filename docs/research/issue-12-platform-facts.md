# Saturn Cloud — platform facts for the Saturn lane (issue 12)

Retrieval date **2026-11-12**. Every value below was copied from the cited page in that session; no price,
hour count or limit is inferred. Pages were fetched as HTML with `curl` (no JS execution), so anything the
site renders client-side is marked UNVERIFIED. API facts are taken from the versioned OpenAPI document
published at `https://api.saturncloud.io` (indexed at `https://api.saturncloud.io/llms.txt`, version
`2026.6.1`), not from prose docs.

**Headline:** there are now *three* separate products. (a) **AI Studio** / "Hosted" — notebooks, jobs,
deployments; the self-service thing the spec means. (b) **Token Factory inference endpoints** — managed,
**OpenAI-compatible**, API-creatable. (c) **dedicated capacity** — sales-led, "priced per deal". The free
plan the spec quotes is no longer described anywhere on the live site (§2), and (b) is a much better fit
for Manarouter than either lane in the spec (see "What this means" below).

## 1. Product shape

| fact | value | source | status |
|---|---|---|---|
| Interactive workspaces | "Jupyter Servers" (and "R Servers"); the generic term is *resource*, not "workspace", in the UI | https://saturncloud.io/docs/user-guide/how-to/resources/jupyter-servers/ | verified |
| Scheduled/batch | "Jobs" — "runs automated tasks on a schedule or can be triggered manually" | https://saturncloud.io/docs/user-guide/how-to/resources/jobs/ | verified |
| Serverless GPU jobs | Job that "runs on a GPU when triggered and costs nothing when idle"; trigger = HTTP POST, cron or manual; "Each trigger produces a new pod on a new machine"; no warm pool | https://saturncloud.io/docs/user-guide/how-to/scale/serverless-gpu-jobs/ | verified |
| Always-on service | "Deployments" — "a model deployed as an API, a dashboard, or another application that runs continuously" | https://saturncloud.io/docs/user-guide/how-to/resources/deployments/ | verified |
| Managed OpenAI-compatible endpoints | "Token Factory" inference endpoints: "serves an open-weight model over an OpenAI-compatible HTTP API" | https://saturncloud.io/docs/inference/ | verified |
| Declarative resources | "recipes": YAML/JSON of a whole resource; schema at github.com/saturncloud/recipes | https://saturncloud.io/docs/user-guide/how-to/advanced/recipes/ | verified |
| Most /docs/ pages carry a banner | "This page documents AI Studio… Saturn Cloud also offers dedicated inference endpoints built with NVIDIA DSX" | any /docs/ page | verified |

## 2. Free plan

| fact | value | source | status |
|---|---|---|---|
| The only statement of the free quota ever found | "Get 150 hours a month of compute for free. Use up to 64GB of RAM, a GPU, and distributed computing clusters. No credit card required." | http://web.archive.org/web/20250512113721/https://saturncloud.io/try/ | **archived 2025-05-12 only** |
| Live `/try/` today | redirected to "Saturn Cloud \| Dedicated Inference for Open Models"; the 150-hours text is **gone** | https://saturncloud.io/try/ | verified absent |
| Live plans pages | never state hours, GPU allowance or credit-card requirement; only "Start with the Free Plan to gain free access and familiarize yourself with our platform, then upgrade to the Pro Plan to unlock resource usage capabilities" | https://saturncloud.io/plans/hosted/ | verified |
| Free tier on paid plan | "This is free for Hosted Free users, and paid for Hosted Pro… Under the paid plan, there is no free tier." | https://saturncloud.io/docs/user-guide/billing/pro/ | verified |
| Free-tier machine allowance | `GET /api/limits` returns `hours_per_day`, `hours_per_month`, `hours_forever`, `instance_sizes` (null = unlimited), `num_instances`, `auto_shutoff`, `storage_in_gb` — the quota is per-plan data, not prose | https://api.saturncloud.io/reference/limits_list-1.md | verified (shape) |
| Actual free-plan numbers | not published; must be read from `/api/limits` with a real free account token | — | **UNVERIFIED** |
| Sign-up | `https://app.community.saturnenterprise.io/auth/hosted-registration` (Nuxt SPA, 2.9 KB shell, no server-rendered form) | https://saturncloud.io/get-started/ | verified |

The spec's "150 hours/month including a GPU, 64 GB, no credit card" is therefore **historically accurate
(2025-05) and currently unverifiable** — treat the quota as unknown-but-readable via `/api/limits`.

## 3. GPU catalogue, prices, instance ids

Self-service GPU rows, `/plans/hosted/`. Columns are the page's own; the "Saturn Cloud" component is
**$0.00 on every row**, i.e. compute is passed through at the hosting rate.

| instance id | vCPU | RAM GB | GPUs | $/h |
|---|---|---|---|---|
| `g4dn.xlarge` (T4-XLarge) | 4 | 16 | 1 | 0.53 |
| `g4dn.4xlarge` | 16 | 64 | 1 | 1.20 |
| `g4dn.8xlarge` | 32 | 128 | 1 | 2.18 |
| `g4dn.metal` | 96 | 384 | 8 | 7.82 |
| `p3.2xlarge` (V100) | 8 | 61 | 1 | 3.06 |
| `p3.8xlarge` | 32 | 244 | 4 | 12.24 |
| `p3.16xlarge` | 64 | 488 | 8 | 24.48 |
| `g5.xlarge` (A10G) | 4 | 16 | 1 | 1.00 |
| `g5.2xlarge` | 8 | 32 | 1 | 1.21 |
| `g5.4xlarge` | 16 | 64 | 1 | 1.62 |
| `g5.8xlarge` | 32 | 128 | 1 | 2.45 |
| `g5.12xlarge` | 48 | 192 | 4 | 5.67 |
| `g5.16xlarge` | 64 | 256 | 1 | 4.09 |
| `g5.24xlarge` | 96 | 384 | 4 | 8.14 |
| `g5.48xlarge` | 192 | 768 | 8 | 16.28 |
| Nebius `Large` | 4 | 16 | 0 | 0.09 |
| Nebius `2XLarge` | 16 | 64 | 0 | 0.40 |
| Nebius `4XLarge` | 64 | 256 | 0 | 1.59 |
| Nebius `1xH100` | 16 | 200 | 1 | 2.95 (1.99 on a 3-month term) |
| Nebius `8xH100` | 128 | 1600 | 8 | 23.60 |

Source for all rows: https://saturncloud.io/plans/hosted/ — verified. **Max GPUs per instance = 8**
(`g4dn.metal`, `p3.16xlarge`, `g5.48xlarge`, `8xH100`). Storage on that page: **$0.10/GB-month**.

| fact | value | source | status |
|---|---|---|---|
| Spec's prices | T4 `g4dn.xlarge` $0.53, A10G `g5.xlarge` $1.00, H100 $2.95 / $1.99 3-month — all three **confirmed exactly** | https://saturncloud.io/plans/hosted/ | verified |
| No A100, L4 or L40S in self-service | GPU filters on the plans pages offer only H100, H200, A10G, T4, V100 | https://saturncloud.io/plans/hosted/ | verified absent |
| Bigger cards | L40S 48 GB, RTX PRO 6000 96 GB, H200 141 GB, B200 180 GB, B300 288 GB, GB300 NVL72; page says "Priced per deal" | https://saturncloud.io/plans/saturn_cloud_plans/ | verified |
| GPU VRAM for self-service cards | **not published**: pricing tables give vCPUs / vGPUs / system RAM, never GPU memory. Only "Single A10G (24GB)" appears, in a card blurb | https://saturncloud.io/plans/hosted-orgs/ | verified absent |
| T4 16 GB / V100 16 GB | NVIDIA product specs, not stated by Saturn anywhere found | — | **UNVERIFIED (Saturn)** |
| `instance_size` naming | `InstanceSize.name` + `display_name`; documented recipe examples are `large`, `medium` and per-cloud slugs like `nebius-gpu-h200-sxm-1gpu-16vcpu-200gb`; sizes carry an optional `cloud/` prefix | https://api.saturncloud.io/reference/info_getserveroptions-1.md | verified (shape only) |
| Price conflict between two live pages | `/plans/hosted-orgs/` lists the same ids at very different rates — `g4dn.xlarge` **$0.15**, `g4dn.4xlarge` $0.57, `p3.2xlarge` $1.095, but `g5.xlarge` **$1.50**, `g5.48xlarge` $24.42, storage **$0.20**/GB-month — and its own embedded JSON dataset is the source of those numbers | https://saturncloud.io/plans/hosted-orgs/ | **conflict, unresolved** |

Do not reconcile the conflict silently: the `$0.15` T4 is a quarter of the T4 rate on `/plans/hosted/` and
below the underlying AWS on-demand rate, so that page looks unreliable; it is nevertheless a live page under
the same brand. Any cost model must cite which page it used.

## 4. Reaching a service inside a workspace from outside

| fact | value | source | status |
|---|---|---|---|
| SSH form | `ssh <resource URL>` — the string copied from the resource card is `<user>@<host>`; **port 22**; key-pair auth only, no password | https://saturncloud.io/docs/user-guide/how-to/access/ide_ssh/ | verified |
| Key type | ed25519 required ("incompatibilities with RSA on OS X") | same | verified |
| Key upload path | username → Manage → Access Keys → *Resource Connection SSH Keys* → New SSH Key; a running resource must be **restarted** afterwards | same | verified |
| Per-resource switch | "Allow SSH Connections" checkbox under *Additional features*, then Save | same | verified |
| SSH is API-toggleable | `WorkspaceCreate.start_ssh` (bool, "Enable SSH access on the deployment"); response carries `ssh_url` ("External SSH URL for the workspace") | https://api.saturncloud.io/reference/workspaces_create-1.md | verified |
| Keys are API-registrable | `POST /api/ssh_public_keys` with required `name`, `value` | https://api.saturncloud.io/reference/sshpublickeys_create-1.md | verified |
| Port forwarding | **not documented**; the SSH page covers an interactive shell and file copy only, no `-L` example. Tunnelling a port is standard OpenSSH but unstated by Saturn | https://saturncloud.io/docs/user-guide/how-to/access/ide_ssh/ | **UNVERIFIED** |
| Workspace HTTP routes | workspaces have ingress `Route`s: `subdomain`, `container_port`, `visibility`, `url`; `GET /api/workspaces/{id}/routes` lists them ("Expose app port" in the UI, port 8000) | https://api.saturncloud.io/reference/workspaces_listroutes-1.md | verified |
| No documented route-create | the API index lists *List* routes and *Create viewer* only; no create-route operation was found | https://api.saturncloud.io/llms.txt | verified absent |
| Deployment URL is **not public** | "Deployments are protected by an internal authentication layer. When accessed, the user's Saturn Cloud session is verified. If the user is not signed in, they will be prompted to sign in." Programmatic use needs `Authorization: token {USER_TOKEN}` | https://saturncloud.io/docs/user-guide/how-to/resources/deployments/ | verified |
| Making it public | per-deployment visibility *Just the owner / Entire organization / Entire internet* — the last "only if public deployments are enabled for your Saturn Cloud installation" — or "disable all authentication for a deployment" | same | verified |

## 5. Non-interactive control (headless)

| fact | value | source | status |
|---|---|---|---|
| REST base | `https://app.community.saturnenterprise.io/api/...` (OpenAPI server), OpenAPI version `2026.6.1` | https://api.saturncloud.io/llms.txt | verified |
| Auth header | prose docs, every code sample, and the client all use `Authorization: token {TOKEN}`; the OpenAPI security scheme declares `bearerAuth` (`http bearer`). Same token, two spellings | docs + https://api.saturncloud.io/reference/tokens-1.md | **conflict, low risk (use `token`)** |
| Python client + CLI | `saturn-client` **2026.2.1** (2026-03-18) on PyPI, ships console script `sc`; env `SATURN_BASE_URL` (**no default**), `SATURN_TOKEN`; `sc list/get/pods/logs/apply/start/stop/delete TYPE NAME`, `TYPE ∈ workspace\|deployment\|job` | https://github.com/saturncloud/saturn-client | verified |
| Apply a whole resource | `PUT /api/recipes` (create-or-update); `sc apply recipe.yaml --start`; `sc pods job <name>`, `sc logs job <name> <pod>` — all three spec commands are real | https://saturncloud.io/docs/user-guide/how-to/scale/dispatching-jobs/ | verified |
| Lifecycle endpoints | `POST /api/workspaces/{id}/start`, `/stop`; `POST /api/deployments/{id}/start`; `POST /api/jobs/{id}/start` ("returns once the pod is scheduled, does not wait for the command to finish") | https://api.saturncloud.io/llms.txt | verified |
| Batch trigger without CLI | `PUT {BASE}/api/recipes` then `POST {BASE}/api/jobs/{id}/start`, both with `Authorization: token …` | https://saturncloud.io/docs/user-guide/how-to/scale/serverless-gpu-jobs/ | verified |
| Inference endpoints are API-creatable | `POST /api/orgs/{org_id}/token-factory/inference-endpoints`, required `name` + GPU `instance_size`; plus list/get/start/stop/delete and even a `chat_completions` convenience call | https://api.saturncloud.io/reference/inferenceendpoints_create.md | verified |
| Getting the token itself | User Profile → Access Keys → New Token, shown once; no token-free bootstrap documented | https://saturncloud.io/docs/user-guide/how-to/access/tokens/ | verified |

## 6. Deployments (paid path)

| fact | value | source | status |
|---|---|---|---|
| Definition | UI form: name, **command**, hardware, size, Docker image, env vars, start script, working dir; or recipe/API with `image_uri` + `command` | https://saturncloud.io/docs/user-guide/how-to/resources/deployments/ | verified |
| Port rule | "must listen to port 8000 for requests, **no other ports will be open**"; must bind `0.0.0.0`, not `127.0.0.1` (else 502) | same | verified |
| Stable HTTPS URL | yes — `https://<name>-deploy.community.saturnenterprise.io`, per-port subdomains — but behind Saturn auth (§4) | same | verified |
| Autoscaling | **none documented**. The create schema has only a flat `scale` ("Number of pod replicas"); no min/max, no scale-to-zero | https://api.saturncloud.io/reference/deployments_create-1.md | verified absent |
| Scale-to-zero | no. "Deployments are constantly running… always using resources and incurring expenses" | https://saturncloud.io/docs/user-guide/how-to/resources/deployments/ | verified |
| Cost model | hourly instance rate, `$0.00` Saturn component; billed when accumulated charges exceed $10 or after 30 days; disk $0.10/GB-month, shared folders $0.30/GB-month | https://saturncloud.io/docs/user-guide/billing/pro/ | verified |
| Healthcheck | `healthcheck` = "Healthcheck path on the deployment's primary port"; `start_ssh` also available | https://api.saturncloud.io/reference/deployments_create-1.md | verified |
| Serving vLLM | Saturn's own recipe for an LLM deployment: command `vllm serve <model> --dtype half --quantization bitsandbytes --load-format bitsandbytes`, GPU size, image `saturn-python-llm:2024.08.01` | https://saturncloud.io/docs/user-guide/how-to/llms/vllm/ | verified |

## 7. Idle / shutdown

| fact | value | source | status |
|---|---|---|---|
| Default auto-shutoff | **1 hour** of inactivity — both the docs and the API default (`"1 hour"`) | https://saturncloud.io/docs/user-guide/how-to/access/autoshutoff/ | verified |
| Options | 1 hour, 6 hours, 24 hours, 3 days, 7 days, Never (API enum is `1 hour\|6 hours\|24 hours\|3 days\|7 days\|Never`) | https://api.saturncloud.io/reference/workspaces_create-1.md | verified |
| What counts as activity | a JupyterLab browser window open **or** an SSH connection open; a sleeping laptop drops both and restarts the timer | https://saturncloud.io/docs/user-guide/how-to/access/autoshutoff/ | verified |
| Plan gating | the UI labels the control "(Hosted Pro and Enterprise accounts only)" | same | verified |
| Deployments | not covered by auto-shutoff — they run until stopped | https://saturncloud.io/docs/user-guide/how-to/resources/deployments/ | verified |

## 8. Anything blocking headless automation

| item | finding | source | status |
|---|---|---|---|
| Sign-up | web SPA only (`/auth/hosted-registration`); no CLI or API sign-up | https://saturncloud.io/get-started/ | verified |
| Login | same SPA serves `/auth/login`; the shell references Google/SSO providers, so email-or-SSO web login is the only documented route | https://app.community.saturnenterprise.io/auth/login | verified (partial) |
| Captcha | none observed; the auth pages are an SPA, so a captcha could exist behind JS | — | **UNVERIFIED** |
| Org approval | not documented for self-service accounts; enterprise orgs are admin-provisioned | https://saturncloud.io/docs/admin-guide/administration/active-resources/ | **UNVERIFIED** |
| Real blocker | everything after login is automatable (API + `sc` + SSH keys + recipes); only *obtaining the token* needs a human in a browser once | §5 | verified |

## What this means for Manarouter

1. **The free-plan premise is stale.** Nothing live states 150 h/month or a free GPU; `/try/` is now a
   dedicated-inference sales page. The real quota is data: `GET /api/limits` → `hours_per_month`,
   `instance_sizes`.
2. **Free + SSH tunnel** is *technically* automatable: keys via API, `start_ssh` on create, `ssh_url` in the
   response, `auto_shutoff` to cap spend, `POST …/start|stop` for lifecycle. But *zero* documentation
   supports it — SSH is described as an interactive shell for humans, port forwarding is never shown, so
   `ssh -L 8000:localhost:8000` is our invention, untested against their gateway. It also needs a
   supervisor inside a notebook container, and the idle timer stops the workspace, killing the upstream.
3. **Paid Deployment** is documented and reproducible (`image_uri` + `command` + port 8000 + `scale`), and is
   what Saturn's own vLLM guide prescribes. But its URL sits behind Saturn's auth layer: the client must send
   `Authorization: token <saturn-token>`, which collides with the `Bearer` key the relay owns. It never
   scales to zero and bills every hour.
4. **The finding that changes the plan:** Token Factory **inference endpoints** are exactly an
   OpenAI-compatible provider — `base_url = https://<endpoint>/v1`, `Authorization: Bearer <token>`,
   `model` = endpoint name, `GET /v1/models` lists them — and the lifecycle is API-creatable/startable:
   a provider row with a token, no tunnel, no image, no port, no supervisor.
5. **Recommendation:** model the Saturn lane as an OpenAI-compatible provider keyed by a Saturn API token and
   target inference endpoints first; keep the SSH tunnel and Deployment as documented fallbacks, not the
   design. First price one endpoint (none published) and confirm a free plan can create one — "Anyone can
   sign up and create an inference endpoint to try the platform" is the only free-GPU offer still standing.

## Could not verify

- Free-plan quota in hours, its GPU entitlement, and whether a credit card is required **today**. Only the
  2025-05-12 Wayback snapshot states 150 h/month + a GPU + no card; every live page is silent.
- Whether the free plan may attach a GPU size at all (`instance_sizes` in `/api/limits` is the only answer,
  and it needs a token).
- GPU VRAM for T4 / V100 / A10G / H100 *as Saturn publishes it* — the tables give vCPUs, vGPUs and system
  RAM only. A10G 24 GB appears once, in prose.
- The exact `instance_size` string for AWS sizes (`g4dn.xlarge` vs `T4-XLarge` vs `t4-xlarge`); the
  authoritative list is `GET /api/info/servers`, which needs a token.
- Whether SSH port forwarding works through Saturn's SSH gateway (`sshpiper` is in their GitHub org, which
  suggests a proxy, not a direct host).
- Inference-endpoint price, and whether "Min workers / Max workers" autoscaling is exposed in
  `InferenceEndpointCreate` (the schema has no such fields).
- Whether the Saturn auth layer strips the `Authorization` header before it reaches the model in a
  Deployment — matters if vLLM is started with `--api-key`.
- The `/plans/hosted/` vs `/plans/hosted-orgs/` price conflict (§3); no page says which one checkout uses.
- Billing granularity (per-second vs per-hour). The serverless jobs page says "costs nothing when idle" but
  no page states a per-second meter; the spec's "per-second billing" claim is unsourced.
- Whether the docs' CI/CD story exists: `/docs/user-guide/how-to/advanced/continuous-integration/` is a
  stub that ends at "This section assumes that you already have a CI/CD pipeline setup".
