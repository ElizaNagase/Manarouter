# GPU lanes: driving Kaggle, molab, Saturn and Cerebrium

> **What this page is for.** [25-gpu-rentals.md](25-gpu-rentals.md) explains *what* a rented GPU is
> for and how Manarouter sizes one. This page documents the four *lane* surfaces: the HTTP routes
> behind `#/gpu-rentals`, what each platform can and cannot do, and which answer means what.

A **lane** is one external platform that can run a rented model. Four are wired: Kaggle, molab,
Saturn Cloud and Cerebrium. The fifth platform in the catalogue, Modal, does not need a lane in this
sense — it is the target of the `modal-vllm` dialect and the rental deployment path, documented in
[25-gpu-rentals.md](25-gpu-rentals.md) §4.

There is no shared abstraction over the four. They disagree about almost everything that matters
(batch versus interactive, CLI versus HTTP, one token versus two credentials, hours quota versus
per-second billing), and a lowest-common-denominator interface would have to lie about at least one
of them. What *is* shared is the shape of the answers: every route returns bare JSON on success and
an `{"error": {"code", "message", "details"}}` envelope on failure, produced by the single
`error_body` helper the rest of the API uses. A panel that can read one lane's failure can read all
four.

Everything below is on the **conductor** (`:20901`), not the relay. None of these routes touch a
request path.

---

## 1. The rule every lane follows

**A lane that cannot work says so, with a reason.** `ready: false` plus the reason is an answer; a
`501 Not Implemented` or a bare `404` is indistinguishable from a broken deployment, so neither is
used for "this platform is not configured". The same rule runs through the whole page:

| situation | what the route says |
|---|---|
| no credential attached | `ready: false`, the missing piece named |
| credential attached but the platform rejects it | `ready: false` and the platform's own text |
| platform unreachable | a `502`/`504` envelope naming the host, never a silent empty success |
| lane is usable | `ready: true` and the per-account detail |
| a route exists but this platform cannot do that thing | **there is no route** — see §2 |

The fourth row is the one worth stating twice: a lane reports what it learned, not what would be
convenient. A Saturn budget that nobody has recorded reads as `null` seconds, not as `0` used
(§5), because a zero would be a measurement the process never took.

**Authentication.** These routes are guarded exactly like the rest of the admin API: the conductor's
own source/origin/authentication middleware, in the order `source → origin → authentication`
(`api.rs`). They are not separately authenticated and they are not exempt. The conductor binds to
loopback for local use; exposing the panel publicly is a decision about the whole admin API, not
about these routes.

**Secrets.** No lane writes a credential to `config.json`, and no route returns one. Attach takes
the credential in the request body and keeps it **in this process's memory** for as long as the
registration lives; Saturn, molab and Cerebrium also write it to the encrypted vault under
`lane:<platform>:<account>` so the account survives a restart (§12). The `rental:` namespace stays
separate: it belongs to rental accounts (`rental:<account id>`), not to lanes. Two consequences an
operator will meet:

- a credential exists in plaintext inside the conductor process, which is why registrations are
  keyed by account *label* rather than by the token (a token used as a map key is one stray debug
  format away from disclosure);
- a restart restores the account but not its health, so the lane says
  `restored from the vault; not probed since this process started` until something dials it. That is
  a report, not a failure: a tunnel that only exists while the operator is present must not be
  marked dead at boot.

Persisting lane credentials under `rental:<account>` (and `rental:<account>:hf` for Cerebrium's
second credential) is the intended shape and is **not implemented**; treat this paragraph as the
record of that gap rather than as documentation of a feature.

---

## 2. What each platform can actually do

This table is the honest summary. "Batch only" means the platform cannot hold an inbound endpoint
open for a router to call; the work has to be submitted and collected.

| | Kaggle | molab | Saturn Cloud | Cerebrium |
|---|---|---|---|---|
| wire | CLI (`kaggle`) | HTTP to a URL you attach | HTTP + SSH tunnel | HTTP control plane |
| interactive endpoint | **no** (batch only) | yes, while the session lives | yes, through the tunnel | yes, `api.cerebrium.ai` |
| lifetime cap | 12 h per session, 30 h/week GPU | 12 h (`MAX_SESSION_SECS`), 90 min idle | 1 h auto-shutoff on the free plan | per-app cooldown + replica settings |
| quota | 30 h/week GPU | session-bounded | 150 h/month, self-reported | per-second billing, plan-dependent |
| credential | `kaggle.json` (API token) | Google OAuth, pasted token | one token (control plane + inference) | **two**: service-account token + app key |
| routes | 4 | 3 | 5 | 5 |
| on-demand creation | no | no | no (no create-route API) | yes (`POST /apps`) |

Two consequences that shape the code:

- **Kaggle cannot be an upstream.** There is no socket to send `/v1/chat/completions` to, so the lane
  submits kernels and reports their state; it never appears in a combo. The module says "batch only"
  in its own doc comment for this reason.
- **molab refuses to be a proxy.** Its terms forbid exposing compute or proxying it for others; the
  lane therefore attaches to a session the operator started themselves and uses it only while it is
  warm. That is a warning in the lane's output, not a block (§8).

---

## 3. Kaggle — four routes, batch only

`kaggle` is a CLI lane. The process shells out to `kaggle kernels push`, so the lane's health is
partly the health of that binary.

| method | path | what it does |
|---|---|---|
| `GET` | `/api/gpu/lanes/kaggle/status` | is the CLI present, is a credential configured |
| `POST` | `/api/gpu/lanes/kaggle/run` | submit one kernel and record it |
| `GET` | `/api/gpu/lanes/kaggle/run/{run_id}` | one run's state, results and stderr |
| `GET` | `/api/gpu/lanes/kaggle/runs` | every run this process knows about |

**Status** answers `{lane, ready, kaggle_cli_present, credential_configured, sources, notes}`.
`ready` is the conjunction of the two booleans, and both are reported separately so an operator can
tell "install the CLI" from "add the key". The `sources` block carries the documentation URLs the
lane's facts came from; `notes` is the headline fact (batch-only, session caps).

**Run** takes:

```json
{ "account": "kaggle-main", "model": "org/model", "items": ["prompt one"],
  "accelerator": "NvidiaTeslaT4", "deadline_secs": 3600 }
```

- `deadline_secs` is optional and capped at **43 200** (12 h). Over the cap is a `400`
  `deadline_exceeds_cap` with `details: [{"max_allowed": 43200}]` — the caller is told the limit
  rather than having it silently clamped.
- The lane writes the kernel's `script.py` and `kernel-metadata.json` into a per-run temporary
  directory, pushes it, and returns `{run_id, state: "submitted", …}`. Credentials never appear in
  either generated file; the config directory is created per run and removed on every exit path.
- A failed `kaggle kernels push` is a `502 kaggle_push_failed` carrying the CLI's `stderr` in
  `details`. Reporting `submitted` for a push that failed is the failure mode this guards.

**Run status** checks the deadline *before* asking Kaggle: a run that has outlived its window is
marked expired and answers `504 kaggle_deadline_exceeded`, because the kernel's own state can lag by
minutes and "still running" would be wrong in the direction that costs quota.

**Errors**: `503 kaggle_cli_absent`, `502 kaggle_api_error`, `502 kaggle_push_failed`,
`504 kaggle_deadline_exceeded`, `404 run_not_found`, `500 internal_error`.

`run_not_found` matters. A run id this process never registered is the caller's mistake, and
answering `502` for it told the operator Kaggle was broken. The registry is per-process: a conductor
restart forgets run ids, which is the correct behaviour for a run whose temporary directory is gone.

---

## 4. molab — three routes, attended sessions

molab has no non-interactive creation path (Google OAuth in a browser), so the lane does not start
sessions. The operator starts one, then attaches its URL and token.

| method | path | what it does |
|---|---|---|
| `GET` | `/api/gpu/lanes/molab/status` | which accounts are warm and how long is left |
| `POST` | `/api/gpu/lanes/molab/attach` | verify an endpoint and register it |
| `POST` | `/api/gpu/lanes/molab/down` | forget an account |

**Attach** takes `{account, url, token}`, calls `GET /v1/models` on the URL, and registers the
account only if that answers with a model list. A `3xx` is a failure, not a redirect to follow: the
HTTP helper does not follow redirects, and a redirect here means the URL is not the inference
endpoint the operator thought it was. Errors: `400 endpoint_rejected` (the URL answered, but not
with a model list), `502 host_unreachable`, `400 redirect_not_followed`, `503 http_client_unavailable`.

The registration is keyed by **account label**, not by `url:token`. A bearer token used as a map key
is one stray debug log away from disclosure, and the label is already unique per lane.

**Status** answers `{lane, warm_lanes, accounts: [{account, warm, minutes_left, last_probe_error}]}`.
An empty lane answers `{warm_lanes: 0, detail: "0 warm lanes, start one"}` — a sentence, because a
zero with no accounts and a zero with every account expired are different situations.

**Lifetime.** `MAX_SESSION_SECS = 14 400` (4 h) is the lane's own ceiling and molab's 12 h hard cap
is upstream of it; a supervisor re-checks warmth every 30 s and drops a registration that has aged
out. `minutes_left` therefore counts down to the lane's ceiling, not to molab's.

---

## 5. Saturn Cloud — five routes, one self-reported budget

Saturn is the only lane whose quota the process cannot observe. The free plan includes **150 GPU
hours a month**; only the operator can see how much of it is spent.

| method | path | what it does |
|---|---|---|
| `GET` | `/api/gpu/lanes/saturn/status` | accounts, tunnel state, budget block |
| `POST` | `/api/gpu/lanes/saturn/attach` | register a host and token |
| `POST` | `/api/gpu/lanes/saturn/budget` | record the operator-reported monthly usage |
| `POST` | `/api/gpu/lanes/saturn/down` | forget an account |
| `POST` | `/api/gpu/lanes/saturn/probe` | check one account against its endpoint |

**The budget block** is one shape, produced by one function, embedded in both the status answer and
the `/budget` answer:

```json
{ "used_secs": null, "remaining_secs": null, "total_secs": 540000,
  "measured": false, "cap_exceeded": false }
```

`used_secs` is `null` until somebody records it. `measured: false` is what tells a reader the
difference between "nothing spent" and "nobody looked". `cap_exceeded` is only ever true for a
recorded figure at or over the cap; an unrecorded ledger is *not* treated as exhausted, because the
lane cannot prove exhaustion, but it is also not presented as clean.

`POST /budget` takes `{"used_secs": 123456}` or `{"used_secs": null}` to clear the record, and
answers the same block plus a sentence saying which of the three things happened (recorded, recorded
as fully consumed, cleared). Recording usage at or over the cap makes `/probe` answer `402` until the
record is cleared — the cap is checked before the probe spends anything.

**Attach** takes `{account, host, token}`. Saturn's Token Factory issues one token that works for
both the control plane and the OpenAI-compatible inference endpoint, so there is one credential where
Cerebrium needs two.

**The tunnel is yours, not the lane's.** Saturn exposes the Jupyter server through SSH, and the lane
starts no process of any kind — no `ssh` child, no supervisor, nothing to kill. Open the tunnel in
your own terminal:

```text
ssh -N -L 127.0.0.1:8000:localhost:8000 <user>@<workspace>.saturncloud.io
```

and attach `http://127.0.0.1:8000` (or the workspace's Token Factory endpoint). This is deliberate:
a conductor that opens an SSH connection by itself would hold a connection nobody asked for, with a
credential nobody is watching. `down` removes the registration, its token from memory and its vault entry; it does
not close your tunnel, because it never opened it.

**Errors**: `400 account_not_registered`, `502 host_unreachable`, `503 http_client_unavailable`,
`402 quota_exhausted`, `502 credential_rejected`.

---

## 6. Cerebrium — five routes, two credentials, one required default

Cerebrium is the only lane that can create a deployment over an API, and therefore the only one whose
`deploy` needs the most care.

| method | path | what it does |
|---|---|---|
| `GET` | `/api/gpu/lanes/cerebrium/status` | registrations, plan, app count |
| `POST` | `/api/gpu/lanes/cerebrium/attach` | verify a project token and register the app |
| `POST` | `/api/gpu/lanes/cerebrium/deploy` | create or replace the app record |
| `POST` | `/api/gpu/lanes/cerebrium/down` | delete the app |
| `POST` | `/api/gpu/lanes/cerebrium/probe` | read the app's state from the control plane |

**Two credentials.** A service-account token (`BearerAuth`) authenticates the control plane
(`https://rest.cerebrium.ai`); the app's own API key authenticates inference
(`https://api.cerebrium.ai/v4/{project_id}/{app_name}/v1`). Attach stores both under
`rental:<account>` and `rental:<account>:hf` respectively. Conflating them produces a `401` that
looks like a bad token but is a wrong *kind* of token.

**`disableAuth` is required and defaults to `true`.** The control plane's `POST /apps` defaults
`disableAuth` to true; the lane sends it explicitly on every deploy so the value is a decision in the
request rather than a default in someone else's schema. An app whose auth is disabled is only
reachable by someone who already has the endpoint; inference auth is a separate layer and the lane
does not weaken it.

**Deploy takes** `{account, cerebrium_toml, compute, gpu_count?, disable_auth?}`:

- `compute` must be one of the platform's ids (`BLACKWELL_B300` is deliberately not offered: the
  catalogue entry is unverified for this lane). An unknown id is a `400` listing the valid ones,
  rather than a deploy that fails later at the platform.
- `cerebrium_toml` is rendered from the deployment's tuning by `gpu::cerebrium::artefact` — the one
  renderer, tested in `mr-core`. The lane does not re-render a bundle of its own.
- `POST /apps` answers with `{buildId, dashboardUrl, internalEndpoint, keyName, status, uploadUrl}`
  and **no app id**. The lane therefore lists the project's apps and matches by name to learn the id
  before it can ever delete anything. The app name comes from the registration, never from the
  deploy body.
- The response names the pre-signed upload URL as step 2 of the deploy. A deploy that returns 200 and
  stops there would leave an empty app, so the answer says plainly what remains.

**Free plan**: `FREE_PLAN_MAX_APPS = 3`. The lane reports the count; it refuses nothing, because the
plan is a property of the account, not of the request.

**Errors**: `400 invalid_request` (unknown compute id, unsafe app name), `404 project_not_found`,
`404 app_not_found`, `502 credential_rejected`, `402 quota_exhausted`, `502 host_unreachable`,
`502 unavailable`, `400 account_not_registered`, `503 http_client_unavailable`.

---

## 7. The panel surface

`#/gpu-rentals` renders one status card per lane from the four `GET …/status` routes. The four
requests are issued together and settle independently: one lane's failure cannot hide another lane's
answer, and a failed route is named with its path and its reason inside its own card. Each card has
its own refresh, so an operator waiting for a session to warm does not re-read the other three.

A card for a lane with an attach route (molab, saturn, cerebrium) also carries **Attach account**,
which opens a form holding exactly the fields that route takes (§10). The submit posts them to
`POST /api/gpu/lanes/{platform}/attach`; the token input is a password field, is never prefilled,
and is cleared only after the attach succeeds, so a failed attach does not make the operator retype
a secret. The reply is quoted: when the lane reports `remembered: false` the form stays open and
says the credential is for this process only, because a repaint would discard the one sentence the
operator must not miss (§12). A successful attach closes the form and re-reads the card, and its
confirmation is held on the view rather than in the form, so the repaint cannot erase the sentence
that says the credential was kept. Kaggle has no form because it has no attach route — it runs
through the `kaggle` CLI on the host — and the card says that instead.

Each account row says `warm` or `cold` in words (the lane sends a boolean, and printing
`String(false)` as a status word would be a panel invention, not a report) and quotes the lane's own
`last_probe_error`, including the restored-from-the-vault sentence. So an account that exists but has
never been dialled reads `registered · cold · restored from the vault; not probed since this process
started`, which is the whole truth and looks nothing like a healthy one. Deploy, run and down still exist only
as routes; their cards name the route and fields rather than pretending a button exists.

Saturn's budget is printed only from what the lane reported. An unrecorded ledger shows as
unrecorded; the panel never formats `null` as `0`.

The **launch plan** preview (§9 of [25-gpu-rentals.md](25-gpu-rentals.md)) posts `{schedule}` to
`POST /api/gpu/launch-plan` and renders each wave with the deployments it holds — a list of indices
is not an answer. The copy states that a preview starts nothing.

Where a lane reports `warnings` — Saturn under `plan.warnings`, Cerebrium in its top-level
`warnings` array — the panel prints them in amber and they block no button. Kaggle and molab carry
their constraints in `notes` and `facts` instead, because their constraints are properties of the
platform rather than objections to the configuration. See §8.

---

## 8. Policy is a warning, never a block

Three of the four platforms state terms that a multi-account deployment strains:

- Kaggle's terms allow one active account per person;
- molab's terms forbid exposing compute or proxying it for others;
- Saturn's free tier is per-account.

The operator's ruling is that Manarouter is a gateway deployed for several people, one account each,
and that these findings must be **visible and non-blocking**. A finding travels as a `Warning`
(`code`, `message`, and — where the platform states a clause — that `clause` with its `source` URL
next to it), so the person who owns the account decides. `Warning` and `Refusal` are separate lists
in `mr-core/src/lane/mod.rs`: a warning never becomes the reason a request is refused.

Nothing in the lane code turns a policy finding into a refusal. The only refusals are physical: a
missing credential, an unreachable host, an exhausted recorded budget, a deadline over the platform's
own cap.

That split — `Warning` with a citation versus `Refusal` for an impossibility — is settled policy
across the project, documented in [25-gpu-rentals.md](25-gpu-rentals.md) and in the lane module's
own doc comments.

---

## 9. Testing a lane without a platform account

- The lane modules carry their own tests (11 Kaggle, 8 molab, 13 Saturn, 19 Cerebrium, 6 for the
  vault round trip) which build a router from `AppState::for_test` and drive it with
  `tower::ServiceExt::oneshot`. None of them needs a network.
- Which of those have met a real platform: Saturn's attach and its vault restore were driven through
  the panel against a second conductor on `127.0.0.1:20998` with a fake endpoint (attach → restart
  unprobed → probe → down → restart gone). molab's attach and restore were exercised the same way
  over curl, not through the panel. Cerebrium's attach and restore are unit-tested and
  compile-checked only — this host holds no Cerebrium credential, so the first real attach is still
  the test. It is written to the same shape as the other two, but "written" is not "proven".
- HTTP lanes take an injected `QuotaHttp`, so a `FakeHttp` answers with a canned status and body and
  a `DeadHttp` simulates a refused connection. That is how `host_unreachable` and
  `credential_rejected` are tested without a socket.
- The Kaggle tests rewrite `PATH`; they take a process-wide lock, because two tests mutating one
  process's environment is a race that produces a spurious `500`.
- A real attach is the only way to test a real platform. `scripts/test-relay.sh` exists for the
  *relay*, not for these routes; there is no scripted harness for a lane.

---

## 10. Attaching a lane account for a real run

The panel's lane card can attach an account for molab, saturn and cerebrium (§7), which is the
shortest path when the operator is already looking at the card. The curl bodies below are the same
request for scripts, and they remain the only path for Kaggle, deploy, run and down. The credential
a lane is given is remembered in the encrypted vault, so a conductor restart re-registers the
account instead of emptying the lane — §12 describes exactly what is and is not restored.

Keep a credential out of `ps` and out of shell history: write the body to a `0600` file, post the
file, delete it.

```bash
umask 077
read -rsp 'lane token: ' TOKEN; echo
python3 - "$TOKEN" > /tmp/lane-attach.json <<'PY'
import json, sys
print(json.dumps({"account": "molab-1", "url": "https://<server>", "token": sys.argv[1]}))
PY
curl -sS -X POST http://127.0.0.1:20901/api/gpu/lanes/molab/attach \
  -H 'content-type: application/json' --data-binary @/tmp/lane-attach.json
rm -f /tmp/lane-attach.json
```

**molab** takes `{account, url, token}` and probes the URL before registering; a redirect or a
non-JSON answer is refused rather than stored. `POST …/molab/down {account}` forgets it. The URL
must be the notebook's own serving endpoint — molab's terms forbid proxying its compute elsewhere,
so the lane does not build a tunnel for you.

**Saturn** takes `{account, host, token}` and expects the operator's SSH tunnel to be up on
`127.0.0.1:8000`; the lane never opens one. `host` may be a bare hostname or include a scheme:
`workspace-abc.saturncloud.io` is probed as `https://…`, while a loopback or `localhost` host is
probed as `http://…` because that is what a tunnel speaks. An explicit `https://` or `http://`
always wins. `POST …/saturn/budget {"used_secs": N}` records the
month's consumption against the 150-hour free allowance, `{"used_secs": null}` clears it, and
`POST …/saturn/probe {account}` asks the platform for the endpoint's view. `down` tears down the
deployment, not your tunnel.

**Cerebrium** takes two different credentials and refuses to conflate them:
`POST …/cerebrium/attach {account, project_id, app_name, token}` where `token` is the
**service-account** token for `rest.cerebrium.ai`. A deploy then needs the complete
`cerebrium.toml` (a deploy replaces the app record, so omitted keys take their defaults), a
`compute` id from the catalogue, and an explicit `disable_auth` — `true` additionally requires
`acknowledge_public_endpoint`, because Cerebrium otherwise publishes an unauthenticated endpoint.
Generate the TOML with `POST …/cerebrium/bundle`, then pass it verbatim:

```bash
curl -sS -X POST http://127.0.0.1:20901/api/gpu/lanes/cerebrium/deploy \
  -H 'content-type: application/json' --data-binary @/tmp/cerebrium-deploy.json
```

`POST …/cerebrium/probe {account}` is the platform's view; a real completion is the only proof that
a model is serving, and it is also the only thing that clears a cold start.

**Kaggle** has no attach route by design: it runs the `kaggle` CLI on this host under the
operator's own login. `GET …/kaggle/status` reports `kaggle_cli_present` and
`credential_configured` without contacting Kaggle, and `ready:false` means one of those two is
missing. A run posts a kernel and returns files; it cannot answer in-line. §3's one-active-account
finding is policy, and the lane does not enforce it.

`ready:true` on any lane means a credential is registered, not that a model is up. The four status
routes say so in their own words (`detail`, `note`, `notes`), and the panel now prints those words
instead of reducing every lane to a chip.

## 11. The end-to-end sweep

`scripts/gpu-lanes-e2e.py` asks every lane for its status, reads the account list each lane holds,
probes Saturn and Cerebrium per account when a probe route exists, and reads the GPU catalogue and
the deployment list. It exits non-zero unless every lane is ready and every probe that ran
succeeded, so it can be used as the acceptance check for "all GPU platforms work":

```bash
scripts/gpu-lanes-e2e.py                 # human-readable table
scripts/gpu-lanes-e2e.py --json          # machine-readable summary
scripts/gpu-lanes-e2e.py --no-probe      # status only, no platform calls
scripts/gpu-lanes-e2e.py --base-url http://127.0.0.1:20998   # a test conductor
```

Saturn counts as ready only when at least one registered account is healthy: `attach` probes once
and records the failure, so a registration alone is not a working endpoint.

`MANAROUTER_ADMIN` sets the base URL when `--base-url` is absent. The sweep starts nothing and spends
nothing.

**What has to exist before a sweep can be green.** Each lane is ready for a different reason, so the
operator supplies a different thing:

| lane | what to attach | where it comes from |
|---|---|---|
| Kaggle | the `kaggle` CLI on `PATH` plus a credential file (`~/.kaggle/kaggle.json`, or `KAGGLE_USERNAME`/`KAGGLE_KEY`) | `kaggle.com` account settings; one active account only (§3) |
| molab | `{account, url, token}` from the panel card or `POST …/molab/attach` | the notebook's own serving endpoint, reachable from this host; molab's terms forbid proxying it elsewhere |
| Saturn | `{account, host, token}` plus an SSH tunnel the operator runs | `ssh -N -L 8000:<jupyter-host>:8000 <user>@<jupyter-host>`, then attach `host: 127.0.0.1:8000` |
| Cerebrium | `{account, project_id, app_name, token}` | `rest.cerebrium.ai` service-account token, **not** an inference key |

The ladder is the same for every lane: `GET /api/gpu/catalog` → `POST /api/gpu/probe` →
`POST …/{lane}/attach` → `GET …/{lane}/status` (the account is present and, where a probe route
exists, healthy) → `POST /api/gpu/launch-plan` (the deployment is placed) → the real run. A green
sweep stops after the status and probe rungs: it proves the platform answered, not that a GPU is
serving tokens, and a lane that has never run inference must not be reported as if it had. Kaggle is the exception it cannot cover: it is batch-only, so proving it means submitting a
kernel, which consumes the operator's weekly GPU allowance and posts a notebook to their account.
That stays a deliberate manual step. A green Saturn or Cerebrium probe means the control plane
answered; only a real completion proves inference, which is what `POST …/probe` says in its own note.

---

## 12. A lane account outlives the process

A registration is a credential plus an endpoint. The endpoint is not a secret; the token is. Both
are needed again after a restart, and a conductor restart happens on every deploy — so a lane that
forgot its accounts would have to be re-attached by hand each time, which is exactly the kind of
operational chore that makes a feature look broken.

Saturn, molab and Cerebrium therefore store what they were given in the encrypted vault:

| lane | vault id | remembered fields |
| --- | --- | --- |
| Saturn | `lane:saturn:<account>` | `account`, `host`, `token` |
| molab | `lane:molab:<account>` | `account`, `url`, `token` |
| Cerebrium | `lane:cerebrium:<account>` | `account`, `project_id`, `app_name`, `token` |

Nothing else is remembered. Probe timestamps, health, failure counts, a Cerebrium `app_id`, and the
Saturn budget are measurements or derived identifiers; restoring them would be inventing a fact
about a process that has not made one yet. Cerebrium's app id is learned again by the first probe or
deploy, and a redeploy would have made a remembered id stale anyway.

**A restored account is not a healthy account.** At startup the conductor re-registers each
remembered account without dialling it — a Saturn host is often a tunnel that only exists while the
operator's session does, and a conference of failed probes at boot would mark good accounts bad.
The status route reports the truth instead:
`restored from the vault; not probed since this process started`. The first `POST …/probe`, the
end-to-end sweep, or any real request replaces that sentence with a measurement.

Attach responses carry `remembered: true` or `remembered: false` with a `remember_error`. `false`
means the attach worked for this process and will not survive the next restart; the usual cause is
a vault that could not be written. `POST …/down {account}` deletes the remembered entry, so a
deliberately removed account does not come back on the next boot. Rotating a token means attaching
again: the write replaces the entry.

The token is only ever stored encrypted, never logged, and never returned in a response. The vault
key is the operator's (`vault.key`); backing up the vault without it is pointless, and sharing it is
sharing every provider credential on the host.

Kaggle has no entry here. Its credential is the `kaggle` CLI's own login under the operator's home
directory, not a string this lane holds, and its readiness is reported from that login.

---

## See also

- [25-gpu-rentals.md](25-gpu-rentals.md) — deployments, sizing, the launch plan and the schedule.
- [research/ARCHITECTURE-gpu-rentals.md](research/ARCHITECTURE-gpu-rentals.md) — the frozen interface.
- [research/modal-platform.md](research/modal-platform.md) — verified platform facts, plans, limits.
- [18-api-reference.md](18-api-reference.md) — the rest of the admin API and its envelopes.
- [23-implementation-status.md](23-implementation-status.md) — what is implemented versus intended.
