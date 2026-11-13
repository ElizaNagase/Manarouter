# GPU lanes: driving Kaggle

> **What this page is for.** [25-gpu-rentals.md](25-gpu-rentals.md) explains *what* a rented GPU is
> for and how Manarouter sizes one. This page documents the one remaining *lane* surface: the HTTP
> routes behind `#/gpu-rentals`, what Kaggle can and cannot do, and which answer means what.

A **lane** is one external platform that can run a rented model. Kaggle is the only lane wired
today. Modal does not need a lane in this sense — it is the target of the `modal-vllm` dialect and
the rental deployment path, documented in [25-gpu-rentals.md](25-gpu-rentals.md) §4. Saturn Cloud,
molab and Cerebrium were wired as lanes earlier; all three, with their modules and routes, have been
removed (§9).

There is no shared abstraction over platforms. They disagree about almost everything that matters
(batch versus interactive, CLI versus HTTP, one token versus two credentials, hours quota versus
per-second billing), and a lowest-common-denominator interface would have to lie about at least one
of them. What *is* shared is a small vocabulary in `mr-core/src/lane/mod.rs` — `LaneKind`,
`LaneFacts`, `LaneShape`, `Inbound`, `Submission`, `Credential`, `plan_lane` — and the shape of the
answers: every route returns bare JSON on success and an
`{"error": {"code", "message", "details"}}` envelope on failure, produced by the single `error_body`
helper the rest of the API uses.

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

The last row is the one worth stating twice: a lane reports what it learned, not what would be
convenient.

**Authentication.** These routes are guarded exactly like the rest of the admin API: the conductor's
own source/origin/authentication middleware, in the order `source → origin → authentication`
(`api.rs`). They are not separately authenticated and they are not exempt. The conductor binds to
loopback for local use; exposing the panel publicly is a decision about the whole admin API, not
about these routes.

**Secrets.** No lane writes a credential to `config.json`, and no route returns one. Kaggle is the
only lane left, and it holds no credential of its own: it shells out to the `kaggle` CLI, which reads
the operator's key file under their home directory (§7). So there is no lane credential in this
process's memory and no `lane:` entry in the encrypted vault. The `rental:` namespace stays separate:
it belongs to rental accounts (`rental:<account id>`), not to lanes. The removed lanes did remember a
token so their accounts survived a restart; that machinery went with them (§9).

---

## 2. What Kaggle can actually do

Kaggle is batch-only: the platform cannot hold an inbound endpoint open for a router to call, so the
work has to be submitted and collected.

| property | Kaggle |
|---|---|
| wire | CLI (`kaggle`) |
| interactive endpoint | **no** (batch only) |
| lifetime cap | 12 h session cap, lane-enforced at 43 200 s (§3); TPU sessions ≈9 h — *published only* |
| quota | read at run time from `get_accelerator_quota`: 108 000 s (30 h) reported for **both** GPU and TPU (§3) |
| credential | the CLI's own `kaggle.json` key file; the `KGAT_` bearer token is a different credential (§7) |
| routes | 4 |
| on-demand creation | no |

Kaggle's numbers are the ones this page has had to sort into *verified* and *published*; §3 records
which is which and why.

One consequence that shapes the code: **Kaggle cannot be an upstream.** There is no socket to send
`/v1/chat/completions` to, so the lane submits kernels and reports their state; it never appears in a
combo. The module says "batch only" in its own doc comment for this reason, and `plan_lane` refuses
synchronous traffic against it rather than queueing a request nobody can answer.

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

- `deadline_secs` is optional and capped at **43 200** — the lane's own 12 h bound, safe whatever
  Kaggle's session figure turns out to be (§3). Over the cap is a `400`
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

### What we verified, what we could not

The lane's numbers are easy to state as fact, and not all of them were verified. So the split is
written down here rather than left to be re-derived from the code comments.

**Verified first-hand.**

- The `43 200` cap is the **lane's own** refusal, not a measurement of Kaggle: `MAX_DEADLINE_SECS =
  43_200` in `lane_kaggle.rs`, with a test that a 50 000 s deadline is refused before anything is
  spawned. It stays a safe bound whatever Kaggle's session length is today, because it is never
  larger than the published 12 h session.
- The account budget, read on 2026-11-12 through Kaggle's own MCP tool `get_accelerator_quota`
  ([27-kaggle-mcp.md](27-kaggle-mcp.md) §4) with a `KGAT_...` API token, returned
  `"total_time_allowed":"108000s"` — 30 h — for `gpu_quota` **and** for `tpu_quota`, with
  `"time_used":"0s"`, `"time_reserved":"0s"` and `"quota_refresh_time":"2026-11-14T00:00:00Z"`. That
  is one recorded call, not re-read for this note. It is an account-level "total allowed" figure that
  refreshes on a stated date; the reply does not say the period is a week.

**Published or secondary — not verified here.**

- 12 h CPU/GPU sessions, ~30 GPU h/week, ~9 h TPU sessions, ~20 TPU h/week. These are the figures the
  lane's own `FACTS` records against `https://www.kaggle.com/docs/efficient-gpu-usage`,
  `https://www.kaggle.com/docs/notebooks#technical-specifications` and
  `https://www.kaggle.com/docs/tpu` (`mr-core/src/lane/kaggle.rs`), and the ones the community
  notebook `ARahim3/kaggle-tpu-lab` restates (secondary, reviewed in
  [27-kaggle-mcp.md](27-kaggle-mcp.md) §9).
- `https://www.kaggle.com/docs/tpu` could not be quoted first-hand: it is client-rendered, and a
  fetch of it measured earlier returned an HTTP 200 shell of about 5.5 KB whose text contains no
  occurrence of "hour".
- So the API's 30 h for TPU and the published ~20 TPU h/week **disagree**, and this page does not
  resolve which is current.

**The rule that follows: never hardcode a ceiling.** Any route that reports a budget reads
`total_time_allowed` and `time_used` from `get_accelerator_quota` at submission time and reports the
refresh instant beside them — the plan, the accelerator family and the week's state all move the
budget, and a hardcoded number is wrong in the direction that burns the operator's quota. The lane
does not read it today (its status route reports no quota, §3); this is the rule a session-based route
would have to follow. The 12 h figure survives only where it is a *refusal bound* (`deadline_secs`),
not where it is a claim about what the platform allows.

---

## 4. The panel surface

`#/gpu-rentals` renders one lane card, for Kaggle, from `GET /api/gpu/lanes/kaggle/status`. The card
has its own refresh, so re-reading it does not re-read the rentals, and a failure on this route is
named with its path and its reason inside its own card rather than being folded into the page-level
error.

The card quotes the lane's own words: `detail` says what is missing, `notes` carries the platform
headline (batch-only, session caps, the one-account rule), and `ready` is rendered as a chip only
when the route actually sent a boolean. There is no attach form, because no lane has an attach route
now; the card says that Kaggle runs through the `kaggle` CLI on the host and has no attach route
instead. Deploy, run and down still exist only as routes; the cards name the route and fields rather
than pretending a button exists.

The **launch plan** preview (§9 of [25-gpu-rentals.md](25-gpu-rentals.md)) posts `{schedule}` to
`POST /api/gpu/launch-plan` and renders each wave with the deployments it holds — a list of indices
is not an answer. The copy states that a preview starts nothing.

Where a lane reports `warnings`, the panel prints them in amber and they block no button. Kaggle
carries its constraint in `notes` instead, because a one-account rule is a property of the platform
rather than an objection to the configuration (§5).

---

## 5. Policy is a warning, never a block

One platform states terms that a multi-account deployment strains: Kaggle's terms allow one active
account per person, and its own-use clause forbids serving third parties.

The operator's ruling is that Manarouter is a gateway deployed for several people, one account each,
and that these findings must be **visible and non-blocking**. A finding travels as a `Warning`
(`code`, `message`, and — where the platform states a clause — that `clause` with its `source` URL
next to it), so the person who owns the account decides. `Warning` and `Refusal` are separate lists
in `mr-core/src/lane/mod.rs`: a warning never becomes the reason a request is refused.

Nothing in the lane code turns a policy finding into a refusal. The refusals that exist are
mechanical: no account attached (`no_account`), synchronous traffic aimed at a batch-only lane
(`lane_is_batch_only`), and — in the shared vocabulary — traffic aimed at an owner-only lane
(`lane_is_owner_only`).

That split — `Warning` with a citation versus `Refusal` for an impossibility — is settled policy
across the project, documented in [25-gpu-rentals.md](25-gpu-rentals.md) and in the lane module's
own doc comments.

---

## 6. Testing a lane without a platform account

- The lane is tested at two layers: `mr-core/src/lane/kaggle.rs` carries 3 descriptor tests (shape,
  batch-only refusal, credential shape) and `crates/manarouter-conductor/src/lane_kaggle.rs` carries
  11 route tests. The route tests build a router from `AppState::for_test` and drive it with
  `tower::ServiceExt::oneshot`; none of them needs a network.
- The Kaggle tests rewrite `PATH`; they take a process-wide lock, because two tests mutating one
  process's environment is a race that produces a spurious `500`.
- A real kernel push is the only proof that Kaggle accepts one, and it costs quota and posts a
  notebook to the operator's account — which is why the sweep stops short of it (§8).
- A real attach is the only way to test a real platform. `scripts/test-relay.sh` exists for the
  *relay*, not for these routes; there is no scripted harness for a lane.

---

## 7. A real run: what the operator supplies

No lane has an attach route any more. Kaggle's credential is the `kaggle` CLI's own: the conductor is
pointed at a `kaggle.json` key file, and the CLI reads its config from `$KAGGLE_CONFIG_DIR` or
`~/.kaggle`. The `KGAT_…` bearer token is a different credential from the legacy username+key pair —
`mr-core/src/lane/kaggle.rs` records both and says not to treat them as spellings of one thing.

`GET …/kaggle/status` reports `kaggle_cli_present` and `credential_configured` without contacting
Kaggle, and `ready:false` names which of the two is missing. `ready:true` means a CLI and a file, not
that a model is up — the status route says so in its own `notes`. A run posts a kernel and returns
files; it cannot answer in-line (§3). Kaggle's one-active-account rule is policy, and the lane does
not enforce it (§5).

Keep the key file out of a shell history or a process argument: it is a file, and `chmod 600` is the
whole of the protection it has.

---

## 8. The end-to-end sweep

`scripts/gpu-lanes-e2e.py` asks every lane for its status and reads the GPU catalogue and the
deployment list. It exits non-zero unless every lane is ready, so it can be used as the acceptance
check for "the GPU lane works":

```bash
scripts/gpu-lanes-e2e.py                 # human-readable table
scripts/gpu-lanes-e2e.py --json          # machine-readable summary
scripts/gpu-lanes-e2e.py --base-url http://127.0.0.1:20998   # a test conductor
```

`MANAROUTER_ADMIN` sets the base URL when `--base-url` is absent. The sweep starts nothing and spends
nothing. There is no attach or per-lane probe route left, so it cannot make a lane ready; the ladder
is `GET /api/gpu/catalog` → `POST /api/gpu/probe` → `GET …/{lane}/status` → the real run, and a green
sweep stops after the status rung. It proves the lane's prerequisites are present, not that a GPU is
serving tokens.

**What has to exist before a sweep can be green.**

| lane | what to attach | where it comes from |
|---|---|---|
| Kaggle | the `kaggle` CLI on `PATH` plus the `kaggle.json` key file the conductor is pointed at | `kaggle.com` account settings; one active account only (§3) |

Install the CLI, point the conductor at the key file, then re-run `scripts/gpu-lanes-e2e.py`.

**The last recorded sweep** was run on 2026-11-12 against the production conductor, when four lanes
existed. Every route answered 200 and every lane was red for the one honest reason — no credential on
that host. Three of those rows (molab, saturn, cerebrium) are history now: the lanes were removed, so
a current sweep reports a single Kaggle row. Nothing in the router caused the failures then, and
nothing in the router can clear them without an account.

---

## 9. The lanes that were removed

Saturn Cloud, molab and Cerebrium were wired as lanes, each with its own conductor module, routes,
vault entry (`lane:<platform>:<account>`), panel card and attach form. They have been deleted, along
with the shared lane-vault helper and the Cerebrium GPU artefact module; `GET /api/lanes` and the GPU
catalogue now describe only Modal and Kaggle. `LaneKind` keeps `Kaggle = 2` rather than renumbering,
because the discriminants are documented as stable and `1` was Cerebrium's. This page no longer
documents those platforms; the research notes under `docs/research/` keep the record of why they were
tried, marked as history.

---

## See also

- [25-gpu-rentals.md](25-gpu-rentals.md) — deployments, sizing, the launch plan and the schedule.
- [research/ARCHITECTURE-gpu-rentals.md](research/ARCHITECTURE-gpu-rentals.md) — the frozen interface.
- [research/modal-platform.md](research/modal-platform.md) — verified platform facts, plans, limits.
- [18-api-reference.md](18-api-reference.md) — the rest of the admin API and its envelopes.
- [23-implementation-status.md](23-implementation-status.md) — what is implemented versus intended.
