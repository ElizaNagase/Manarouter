# ISSUE 12: Deploy Saturn Cloud as an inference lane: SSH-tunnelled Jupyter server on the free plan, managed Deployment on Hosted Pro
# Deploy Saturn Cloud as an inference lane: SSH-tunnelled Jupyter server on the free plan, managed Deployment on Hosted Pro

Supersedes #8, which treated Saturn as a "GPU jobs control plane" — recipes and jobs, no serving, no
account story. Saturn can serve inference, but *where* depends on the plan, so this issue specifies
both lanes and makes the plan a capability flag rather than an assumption.

## Research findings (provider docs, with the conflict stated)

* **The free plan is a real GPU allowance:** "Start for Free" gives **150 hours/month of compute
  including a GPU**, up to 64 GB RAM, Dask clusters, **no credit card required**
  ([try](https://saturncloud.io/try/)). Self-service accounts "have limited GPU availability".
* **Hosted Pro is pay-as-you-go per hour** with per-second billing on serverless jobs: T4
  `g4dn.xlarge` ≈ `$0.53/h`, A10G `$1.00/h`, H100 on-demand `$2.95/h` (`$1.99/h` on a 3-month term);
  storage `$0.10/GB-month`. ([hosted pricing](https://saturncloud.io/plans/hosted/))
* **The plan/feature matrix is documented inconsistently, and that matters here.** The quickstart
  marks *Job* and *Deployment* resources as "(Hosted Pro and Enterprise accounts only)", while the
  serverless-GPU-jobs how-to documents jobs generally without a plan gate.
  ([quickstart](https://saturncloud.io/docs/user-guide/getting-started/quickstart/),
  [serverless GPU jobs](https://saturncloud.io/docs/user-guide/how-to/scale/serverless-gpu-jobs/))
  Resolution for this issue: **do not assume.** Probe the account's capability at `lane up` and
  choose the lane from what the API actually accepts; say so in `mr lane status`.
* **What a free account does have is a Jupyter Server**, described as "interact via JupyterLab or by
  connecting via SSH from command line", so a free account can run a process and reach it over SSH.
  That is the basis of the free-tier inference lane below. (Verify on a live free account; if SSH
  turns out to be plan-gated too, this lane degrades to "batch over SSH" and the docs must say so.)
* **Deployments are the warm, always-on serving primitive** — "a continuously running activity like a
  hosted dashboard or API" — which is what an inference endpoint wants.
  ([deployments](https://saturncloud.io/docs/user-guide/how-to/resources/deployments/))
* **Jobs are serverless and billed per second from pod start to teardown**, no warm pool, cold start
  in minutes, outputs must land on networked storage (S3 / SaturnFS) rather than stdout, and spot
  instances are available (`use_spot_instance`). Recipes are declared in YAML
  (`schema_version: 2025.10.01`, `type: job`, `spec.name/image/instance_type/command/
  working_directory/git_repositories/scale`), applied with `sc apply recipe.yaml --start` or over
  HTTP (`PUT {SATURN_BASE_URL}/api/recipes`, then `POST {SATURN_BASE_URL}/api/jobs/{job_id}/start`),
  and polled with `sc pods job <name>` / `sc logs job <name> <pod>`.
* **Auth is a base URL plus a token**, and the token must be **unscoped** for this kind of
  automation: Saturn expects `Authorization: token {TOKEN}`, with `SATURN_BASE_URL` naming the host
  (community vs enterprise hosts differ). User tokens are created under *User Profile → Access
  Keys*; resource tokens are auto-injected as `SATURN_TOKEN` inside Saturn resources. A scoped token
  "grant[s] access to specific resources", which is the wrong shape for a lane that must create and
  destroy resources. ([tokens](https://saturncloud.io/docs/user-guide/how-to/access/tokens/))
* **Everything in the UI is available over the API**, so provisioning is scriptable — this is why
  Saturn is the best of the four for unattended account switching.
  ([api-guide](https://saturncloud.io/docs/api-guide/api/))

## Design: two lanes, one provider

Both lanes are `LaneBackend`s (see #10) and both register as ordinary OpenAI-compatible providers.

**`saturn-deployment` (Hosted Pro / Enterprise).** The lane creates a Deployment resource running an
OpenAI-compatible server (vLLM/SGLang behind FastAPI), waits for its URL to answer `/v1/models`, and
registers `base_url` = the deployment URL. Account switching is a plain pool: one Saturn account
(token + host) per lane account.

**`saturn-jupyter` (free plan).** The lane starts a **Jupyter Server** resource with a GPU, SSHes in,
launches the same server, and opens a local forward:

```
ssh -N -L 20912:127.0.0.1:8000 <user>@<jupyter-host>
```

The adapter dials `127.0.0.1:20912`, so the free-tier lane is a real, low-latency inference endpoint
from the relay's point of view. Requirements this lane must own, because nobody else will:

* **the 150 h/month budget** is the operator's whole free allowance: the lane tracks wall-clock
  hours per account, writes them to `provider_quota_state`, warns at 80 %, and **stops the server**
  at 100 % rather than letting it idle until Saturn does;
* **the idle reaper** — a Jupyter Server left running burns the allowance silently, so the lane
  stops it after `idle_ttl_s` with no request, and `mr lane status` shows remaining minutes;
* **tunnel supervision** — the forward is a child process; a dead forward must deregister the
  provider (write `config.json`, picked up by the relay's 2 s reload) and return `502` so the relay
  reroutes, rather than failing requests at the socket.

## Account switching, concretely

* **Multi-account is native here.** A Saturn account is `(SATURN_BASE_URL, unscoped token)`, and both
  are per-account: a community-host account and an enterprise-host account cannot share a config.
  The lane stores both in the vault (token) and `config.json` (host), never the token.
* **Relay layer:** N accounts on `lane-saturn`; `least-used` spends the 150 h allowances evenly,
  `priority` drains one account then the next, and `max_concurrent` per account is set to the GPUs
  that account can actually get (free GPUs are scarce, so the default is `1`).
* **Lane layer:** `401/403` → token revoked (mark dead, operator must re-add), `402/403` with a
  capacity body → account has no GPU capacity right now (mark *temporarily* unusable with backoff,
  not dead), `429` → concurrency. Distinguishing "no capacity" from "bad token" is the difference
  between a router that recovers and one that silently stops routing.
* **Probe, don't assume.** Because the plan matrix is contradictory, `lane up` first asks whether
  the account can create a Deployment. If it cannot, it falls back to `saturn-jupyter` and records
  the reason. `mr lane status` must print which lane the account is actually on and why.

## Non-goals

* No hidden use of spot instances for serving (an interrupted spot pod is not an inference endpoint);
  spot is allowed only for the batch path.
* No attempt to exceed the free 150 h by cycling accounts — the pool exists for legitimate
  multi-account ownership, and the quota is per account.
* No scraping of the Saturn web UI: the API is documented, so the API is what this uses.

## Work breakdown

1. `crates/mr-lane/src/backends/saturn/{deployment,jupyter}.rs`: resource create/start/stop over the
   documented API, SSH forward supervision, readiness probe on `/v1/models`.
2. Account pool: per-account host+token, capability probe, error classification
   (capacity vs auth vs concurrency), budget accounting into `provider_quota_state`.
3. `mr lane up saturn --account <label> --gpu <id> --lane auto|deployment|jupyter`,
   `mr lane down saturn`, `mr lane status saturn` (hours used/cap, lane kind, tunnel state).
4. `deploy/saturn/`: the OpenAI-compatible server, the recipe YAML for the batch/job variant, and a
   README stating the 150 h/mo cap, the Jobs/Deployments plan question, and the per-second job
   billing.
5. Docs: `docs/24-gpu-lanes.md`, `docs/LLM_INDEX.md`, `docs/23-implementation-status.md`.

## Testing

* Fake Saturn API in `tools/mock-upstream`: create → start → ready; stop; capacity-refusal body;
  revoked token. Assert the three error classes produce three different account states.
* Tunnel supervision: kill the forward and assert the provider disappears from `config.json` and the
  next request lands on another account.
* Budget: a fast-forwarded clock at 100 % must stop the resource and refuse the lane, with a test
  that no request is spent after the cap.
* No test may touch a real account; live verification is a documented manual step.

## Acceptance criteria

* A free Saturn account can be turned into a working OpenAI-compatible inference endpoint through
  one command, with a supervised tunnel and an enforced hourly budget.
* Two accounts pool and switch; capacity refusal marks an account temporarily unusable and routing
  recovers automatically when it comes back.
* `mr lane status` answers "which lane, how many hours left, is the tunnel up" without a dashboard.
* The docs state the free-plan limits and the Jobs/Deployments plan ambiguity honestly.
