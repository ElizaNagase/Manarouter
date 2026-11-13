# Issues #10–#13: what each lane can honestly be

> **Historical (2026-11-13).** The disposition reasoning below led to three lanes being built and
> then removed: Saturn Cloud, molab and Cerebrium are gone from the router, with their routes and
> vault entries. Kaggle is the lane that remains. Kept as the record of how each platform was judged
> and why; the live surface is [../26-gpu-lanes.md](../26-gpu-lanes.md).


Four issues ask for the same thing in four flavours: make another GPU host a first-class lane beside
the Modal one, with per-account quota switching. Each was researched against primary sources before
any code was written (`issue-10-platform-facts.md` … `issue-13-platform-facts.md`, next to this file).
Two of the four cannot be built as written, and the reason is not technical. This document records
the decision per issue so that the next person does not have to re-derive it, and so that nobody
mistakes a documented refusal for an unimplemented feature.

## The table

| Issue | Lane | What the platform offers | What will be built |
|---|---|---|---|
| #11 | Kaggle Notebooks | Headless submit-and-poll (`kaggle kernels push` → `status` → `output`), no inbound socket, 30 GPU-hours/week, **one account per person by Terms** | A single-account batch lane. No pooling. |
| #13 | molab | An interactive marimo notebook with a GPU. No API keys, no job API, no public endpoint — and tunnels are named as abuse | Nothing that carries traffic. Bookkeeping only. |
| #10 | Cerebrium | CLI-first, but a two-step REST deploy exists; a custom runtime passes paths through to `vllm serve`, so the inference URL is deterministic; GPU types are gated by plan and the gate is enforced at deploy time | A real serving lane. The deployment artefact is a generated `cerebrium.toml`; the endpoint is an ordinary `open-ai` provider row. |
| #12 | Saturn Cloud | The free plan the issue quotes is no longer on the site; deployments are auth-walled and port-8000-only, and SSH port-forwarding is undocumented. **Token Factory inference endpoints** are genuinely OpenAI-compatible and API-creatable | A catalogue row and a provider row. No tunnel, no image, no session cookie. |

## #11 Kaggle — the API is fine, the licence is not

The mechanism works. `kaggle kernels push -p DIR` creates or updates a kernel *and runs it*; `kaggle
kernels status` polls a small state enum; `kaggle kernels output` fetches the result. Once one API
token sits in the subprocess environment there is no browser and no interactive session. That is the
submit-and-poll shape the issue's batch design already assumed.

What does not work is the account model the issue asks for. Kaggle's Terms, §3:

> You also may not have, control, or operate under more than one active Kaggle account or Kaggle User
> ID. If we determine that you have, control, or are operating under more than one Kaggle account or
> Kaggle User ID, we may take action without notice, including banning your user account, revoking
> access to your Kaggle User ID…

The same section forbids letting several people share one identity, and limits use to "your own
internal, personal, non-commercial use, and not on behalf of or for the benefit of any third party".
The acceptable-use policy separately names "server farming" and "activity unrelated to ML data
science" as abuse. So "two pooled accounts rotated per request", the issue's own acceptance
criterion, is a ban with extra steps — and it would burn the accounts it was meant to multiply.

Consequences, which the implementation must encode rather than merely describe:

* **One account per lane.** The fresh `KAGGLE_CONFIG_DIR` per subprocess stays, because two processes
  with different config dirs is hygiene; it is not rotation.
* **Batch only, never marketed as an inference backend.** Minutes of floor per call (queue, image,
  weights, load), and the only return path is a file in `/kaggle/working`. A gateway that answers a
  chat request with a 504 after ten minutes is worse than one that says it cannot.
* **Quota is the binding constraint**, not session length: 30 GPU-hours per week against a 12-hour
  session cap, resets weekly, "sometimes higher depending on demand". `kaggle quota -v` prints it as
  CSV, so the lane can read remaining hours instead of scraping a web page, and should treat
  remaining ≈ 0 as lane-down.
* **A single operator routing their own traffic sits inside the licence.** A product serving other
  people's requests does not. That distinction belongs in the panel, not only in a code comment.

## #13 molab — the tunnel is the abuse

The issue proposes reaching a molab notebook over a reverse tunnel and treating it as an
OpenAI-compatible endpoint. molab's own restrictions page names that pattern four times over: abuse
includes "connecting to remote proxies or otherwise exposing molab compute to outside traffic" and
"reselling compute to third parties"; termination without warning is invited by "remote control such
as by SSH" and "using notebooks for non-interactive jobs". The stated remedy is that the account "may
be banned without notice".

There is also nothing to automate. Clerk's `api_keys_settings.enabled` is `false`, so no API token
exists; there is no job or run endpoint; the share URLs (`/notebooks/<id>`, `/notebooks/<id>/app`)
serve marimo's UI, not a bound port; and the GPU (RTX Pro 6000, 96 GB) is selected by hand in a
settings dialog with no API behind it. GitHub OAuth is present but `authenticatable: false` — Google
and an email code are the only ways in. `single_session_mode` is on, so *n* accounts means *n* browser
profiles and *n* Google sign-ins.

So the honest shape is an **attended lane**: Manarouter can record that the operator has a molab
session, show the 12-hour cap and the 90-minute idle shutdown, and refuse to send traffic through it.
What it must not do is pretend a notebook is a server. If a genuinely programmatic serverless GPU is
wanted, that is Modal (already shipped) or a lane like #10 — not this one.

## What "attended" means in code

A lane descriptor says how traffic can reach it, and the router refuses the combinations the platform
forbids rather than failing at request time:

* `shape`: `serving` (an inbound HTTP endpoint exists), `batch` (submit a job, poll, fetch files), or
  `attended` (a human must act for each session).
* `inbound` / `submission` / `credentials`: how a request could arrive (`public-endpoint`,
  `owner-session-only`, `none`), how a job is handed over (`rest-api`, `cli`, `kernel-push`, `manual`),
  and what the platform authenticates with.
* `accounts`: `allowed`, or `prohibited` **with the clause that prohibits it and where it is written**.
* `use_scope`: absent, or the sentence that limits who may use the service.

`plan_lane(lane, traffic, accounts, for_third_parties)` returns a refusal — not a validation error a UI
can talk its way past — when `accounts > 1` on a lane that prohibits it, when a `batch` lane is asked
to answer a synchronous request, and when a lane restricted to its owner's own use is pointed at other
people's traffic. The refusals are served by `GET /api/lanes`, one verdict per lane and per traffic
kind, and the panel renders them with the platform's own clause attached instead of offering a "add
second account" button that would get someone banned.

## #10 Cerebrium — buildable, and closer to the issue than the issue expected

Cerebrium is a paid serverless GPU host, and unlike #11 and #13 the answer here is "build it". That is
not because its terms bless the feature but because they simply do not address it: the Terms of Service
carry a generic Accounts section (accurate information, credential confidentiality, "We reserve the
right to refuse service, terminate accounts…") and a generic prohibited-use list aimed at scraping,
malware and denial of service — **no clause limits account multiplicity, resale, or proxying**, which
is exactly the clause Kaggle and molab do have. The one bullet worth reading twice is "use any robot,
spider, or another automatic device, process, or means to access the Service for any purpose", which in
context is about harvesting the website; a documented REST API and an official CLI are the intended way
in. So the descriptor records `accounts: allowed`, and that word means the absence of a prohibition,
not a licence to serve third parties — the platform has not been asked the question #11 and #13 answer.

The mechanism is better than #11's and worse than Modal's. Worse: deployment is CLI-first
(`cerebrium deploy -y --detach`), and the REST path underneath it is a two-step upload —
`POST https://rest.cerebrium.ai/v2/projects/{project_id}/apps` answers `{buildId, uploadUrl, …}`, then
a `PUT` of the project zip to that pre-signed URL. The OpenAPI document describes itself as "mainly
used by the Cerebrium CLI client" and names no `/deploy` path, so that flow is the CLI's own API
rather than a supported public contract. Better, and this is the part the issue got wrong: a **custom**
runtime (`vllm serve`) receives the path verbatim, so the endpoint is *deterministic* —

```text
https://api.cerebrium.ai/v4/{project_id}/{app_name}/v1
```

No URL to copy out of a build log, which is the one thing the Modal lane still cannot avoid. `…/{app}/run`,
which the issue's spec proposes, is the Cortex function form: it names a function and answers inside a
`{run_id, run_time_ms, result}` envelope. A custom runtime has no function name, so that URL 404s.

Two consequences for the design:

* **The endpoint is an ordinary provider row.** `open-ai` dialect, bearer key, deterministic base URL.
  No new dialect, no new relay code, no new transport — the same conclusion as #12 for a different
  reason. The lane's own contribution is the deployment artefact, not the request path.
* **Two credentials, not one.** A service-account token authenticates the control plane; a project
  **API key** authenticates inference. They are not interchangeable and both 401 the wrong host, so a
  setup flow keeps two slots. The key can be minted over REST
  (`POST /v2/projects/{id}/api-keys` → `{"apiKey": …}`), which matters here because Cerebrium's own
  documented `cerebrium secrets add KEY=VALUE` puts the secret in **argv** — the one thing this
  repository's guidance forbids.

The `cerebrium.toml` defaults are hostile to a rented vLLM and have to be overridden in every
generated file, which is the strongest argument for generating that file rather than documenting it:
`replica_concurrency` is **1** on GPU compute (one request per container, so a second caller starts a
second GPU), `cooldown` is **10 s**, `response_grace_period` is **900 s** and doubles as the request
timeout, `disable_auth` defaults to **true** so an app that omits it is public, `max_replicas` is 1, and
an omitted key is reset to its default rather than preserved. Real per-GPU plan gating is enforced at
deploy time on the Hobby plan (CPU, T4, A10, L4, L40s, INF2), so #10 is also the one lane where a
`(Paid)` marker on a card is a statement about the platform rather than about us.

What is not claimed: nothing was deployed. The cold start, the effective `cooldown` floor (only the
3600 s ceiling is published) and whether the two-step REST upload stays stable are all unverified here,
and the facts file says so at each row.

## #12 Saturn Cloud — the product the issue describes is not the product that exists

The issue assumes three things that are not true of Saturn Cloud in 2026, and one true thing that the
issue did not know about.

**The free plan is stale.** The "150 hours/month, 64 GB RAM, a GPU, no credit card" wording exists only
in a May 2025 archive of `/try/`; the live page is a sales page. The live site has a plan labelled
"Free", but **without a published quota** — which is why the lane's descriptor records no quota at all
rather than the numbers the issue quotes. What *is* machine-readable is `GET /api/limits`
(`hours_per_month`, `instance_sizes`, `auto_shutoff`, …), so remaining hours are readable at runtime
instead of assumed.

**Deployments are not public endpoints.** Saturn documents them as "protected by an internal
authentication layer": a caller needs `Authorization: token {USER_TOKEN}`, the container must listen on
**port 8000** and bind `0.0.0.0`, and there is no autoscaling and no scale-to-zero. A gateway pointed at
one would have to send a Saturn token as the *upstream* credential — possible, but it is a session-like
token, not a per-service key.

**The tunnel is our invention.** SSH is documented only as an interactive human shell. Port forwarding
(`ssh -L 8000:…`) appears nowhere in Saturn's documentation, and while workspaces do have ingress
routes (`GET /api/workspaces/{id}/routes`) there is **no create-route endpoint**. So the issue's
"tunnel the notebook's :8000" design is unverified engineering, and it also needs a token that must
first be minted in a browser. **The tunnel is not built here.** The Token Factory lane described below
is the implementation we ship instead.

**What the issue missed: Token Factory.** Saturn's managed inference endpoints *are* an
OpenAI-compatible provider — `base_url https://<endpoint>/v1`, `Authorization: Bearer`, model = endpoint
name, `GET /v1/models` — and the whole lifecycle is API-creatable
(`POST /api/orgs/{org_id}/token-factory/inference-endpoints` with `name` + GPU `instance_size`, then
start/stop/list/get/delete). That is a lane Manarouter can already route to: one provider row and one
token, no tunnel, no image build, no port-8000 constraint, no session cookie.

So the honest deliverable for #12 is a catalogue row plus documentation, and explicitly **not** a
Saturn-specific deployment generator. Two facts stay unresolved and are recorded as such: whether a free
plan may create an inference endpoint, and the per-endpoint price (the hosted plans publish GPU rates
with $0.00 Saturn markup, but `/plans/hosted/` and `/plans/hosted-orgs/` disagree — `g4dn.xlarge` at
$0.53 vs $0.15 — and neither page is about endpoints).

## What this means for the lane catalogue

The four issues were not four versions of one feature. Two needed a refusal driven by platform
Terms of Service — Kaggle's §3 forbids multiple accounts and third-party use (**#11**), and
molab's abuse policy bans tunneling and remote automation (**#13**) — so a lane descriptor with
`shape: attended` and account policy `prohibited` is the correct model. One needed a deployment
artefact and a **side-effect-free route** (`POST /api/gpu/lanes/cerebrium/bundle` returns a ready
`cerebrium.toml` without writing config, snapshots, or credentials — **#10**). One needed a
catalogue row and a provider row only; Saturn's Token Factory inference endpoints are an existing
OpenAI-compatible surface, so this is provider row + documentation, not a generator (**#12**).
That is why the shipped surface is a **lane descriptor** — shape, inbound, submission,
credential kinds, account policy, quota rules, each with the platform's own words attached — rather
than a per-platform code path. A lane that only needs a provider row costs one data entry; a lane
that needs an artefact costs a generator; a lane whose licence forbids the traffic costs a refusal
that a UI cannot talk its way past.
