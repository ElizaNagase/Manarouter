# 23 — Implementation status

This file exists because a specification that reads as finished while the code is not is worse than
no specification. Everything below is checked against the tree, and the command that checks it is
given so you can re-run it.

> **Status note (2026-10-20):** this document retains earlier implementation-history sections below;
> they are not a comprehensive audit of every Round 2 item. The checks for this close-out passed:
> workspace tests (1 172 passed, 1 ignored), the release-profile test run, `cargo fmt --all -- --check`,
> strict workspace Clippy, strict Clippy and tests in `tools/mock-upstream` and `tools/bench`, the
> panel Node tests, the secret scan, the isolated installer test, and the isolated
> relay/conductor/mock-upstream smoke. The store schema is at **v4**: `key_usage` and
> `provider_quota_state` (v2), `usage_tokens` (v3) and `key_usage_bytes` (v4) are present and
> tested. All four are now **written on the request path**, not merely present: the relay records
> the provider's cache-read/cache-write split and the generation window into `usage_tokens` in the
> same transaction as the ledger row, so tokens per second is a stored fact rather than a number
> derived after the fact. The v4 migration is additive — a side table for the byte counts the relay measures — and
> is idempotent, so a crash-and-retry of the migration cannot fail.
> **D2 is now implemented**: the relay admits a request against the key's caps before any upstream
> side effect and settles it afterwards, in `crates/manarouter-relay/src/quota.rs` — a refused request
> is HTTP 429 and writes nothing, and a token cap can be overshot by the request that crosses it
> because the completion size is not knowable in advance.
> **D4 is now implemented**: proxy egress is on the request path (`crates/manarouter-relay/src/egress.rs`),
> each proxy has its own connection pool so a proxied request can never be served on a direct
> connection, and a tunnel that fails ends the request with HTTP 502 `proxy_error` — no direct
> fallback, no reroute to another provider.
> **D3 is implemented**: a request whose modality the selected route cannot carry is retried against
> the configured `routing.defaults` entry, as a complete reroute against the same pinned snapshot,
> with a budget of one fallback (a target that also lacks the modality, or names no known model, fails
> with HTTP 501).
> **The hedged race runner is implemented**: a combo with `hedge.enabled` dispatches every
> participant at once, `hedge_decide` is called from the request path, and the
> highest-priority participant that answers inside the eligibility window is committed while the
> losers are dropped. **The winner's recovery is implemented too**: the relay wraps the committed
> winner's body (`crates/manarouter-relay/src/recovery.rs`), so a death after the commit is
> recovered from a participant that passed the eligibility gate — silently before any byte reached
> the client, with the `manarouter.retry` boundary after, and never after a tool-call delta. All
> four `WinnerFailMode`s are implemented: `pick-highest-passed` is the default and re-dispatches
> nothing, `pick-highest-passed-different-provider` reuses the same passed participants filtered to
> another provider, and `rerun-excluding` and `stable-fallback` issue new upstream work only when the
> winner actually fails, bounded by `max_rounds` (or one round for `stable-fallback`).
> `tools/bench` measures the relay's own added latency; the hedge's own overhead — one extra
> upstream request per participant — is not measured separately.
> Re-run before release and inspect `git status`.


## 1. What runs

| Component | State | Evidence / limitation |
|---|---|---|
| `mr-core` — config, snapshot compiler, vault, usage store, OAuth | **Substantial, not complete** | Codex, Copilot, and Antigravity OAuth authorization/token exchange/refresh/Google userinfo identity code is wired; Antigravity IDE client details remain reverse-engineered and unverified. |
| `mr-dialects` — OpenAI, Anthropic, Responses, Google, Command Code translation | **Reachable, test coverage uneven** | Relay dispatches request and response translation per selected step. The Responses implementation now covers the arms that are only reachable in combination — reasoning replay, a mid-conversation system turn keeping its place, the encrypted-reasoning include, a dropped output cap, and a truncated stream closed with an error — and the relay drives it through a per-stream `responses::Translator`; the other dialects are better covered than it is. `docs/24-provider-dialects.md` documents the nine dialects, including `google-ai-studio`, whose
thinking fragment is level-only because the public API refuses a level and a budget together. |
| `manarouter-relay` — HTTP API, auth, snapshot, account selection, translation | **Runs; the hedged race runs and the winner's recovery works** | Startup, flag/config precedence, listener bind selection and the 2 s `config.json` reload poller live in `crates/manarouter-relay/src/startup.rs` (declared at `crates/manarouter-relay/src/lib.rs:54`; 17 tests). The step loop and the race share one attempt implementation (`attempt_step` in `crates/manarouter-relay/src/server.rs`), so the sequential and concurrent paths cannot drift; `hedge_decide` decides the race and the losers are dropped when a winner is committed; the participants that passed the race are kept for the recovery to use. Winner recovery is implemented in `crates/manarouter-relay/src/recovery.rs` (`pick-highest-passed`, the default): the winner's body is watched, and a failure is recovered from the highest-priority participant that passed — silently before any byte reached the client, with the `manarouter.retry` boundary after, refused after a tool-call delta. All four `WinnerFailMode`s are implemented: `pick-highest-passed` and `pick-highest-passed-different-provider` reuse the participants that passed the race, while `rerun-excluding` and `stable-fallback` issue new upstream work only when the winner actually fails, bounded by `max_rounds` (one round for `stable-fallback`). Priority combos with `hedge.enabled` now remember the successful provider/model for a configurable inactivity window (default 1800s), combo-wide across callers; the preference is process-local and rechecked against current eligibility on each request. Client-key scan exits at first match (a deliberate latency tradeoff that reveals configured match position to a local timing observer). |
| `manarouter-conductor` — auth, API, state, serving, provider quota refresh | **Partial** | `/api/quota` returns cached provider/account quota snapshots and declares refresh routes. Provider-specific OAuth refresh is reported unsupported. `GET /api/usage` serves the per-request usage ledger with the provider's prompt/completion counts, its cache-read and cache-write split, the generation window and a derived tokens-per-second figure (an em dash, never a zero, when the provider reported no timing); where the provider reported no counts the router's estimate is used and the row says so, and that estimate is computed in the panel process, never on the relay's request path. Every authenticated request is recorded, not only those made with a capped key. The older per-account `usage` table is still never written. No live/provider refresh was exercised in this audit. |
| `mr-cli` | **Partial** | Account/provider/combo/key/model families dispatch; `mr key rotate` is supported; `mr doctor` performs read-only config, vault metadata, snapshot freshness, and listener exposure checks; it fails if no enabled client key is present while relay bind is not confirmed loopback-only. `mr migrate omniroute` imports OAuth grants (by one refresh) and API keys (copied to the vault), model lists, effort-variant aliases and reference-defined OpenAI-compatible nodes — dry run by default, `--apply` to write, reading a copy of the reference database. `mr serve` runs both service binaries as child processes for a foreground session (`crates/mr-cli/src/serve.rs`, 6 tests, committed as `65f543d`; one Ctrl-C stops both, either child exiting stops the other, and the failure message names the child and its status — `mr serve` itself exits 1, it does not forward the child's code); `config` and `vault` remain unimplemented (`crates/mr-cli/src/config.rs:46`, `crates/mr-cli/src/vault.rs:24`). |
| `panel/` | **Implemented; critical browser workflow verified** | Firefox E2E covers panel login, client-key creation, and rotation against an isolated mock API; other routes remain unverified. The Combos nav entry also carries a live view fed by `GET /api/activity`, which the control plane proxies to the relay's loopback `GET /control/activity` (a 24-entry in-memory ring, newest first, ending when the client-facing body drops); `panel/combo-live.js` is a pure decision over that payload and is covered by Node tests, not by the browser E2E. |
| OAuth | **Partial; flows wired, paste fallback verified live** | Conductor OAuth routes cover Codex (browser **and** device-code flows), Copilot, and Antigravity authorization code flows. Config changes written by the CLI are adopted without a restart by both the conductor (on the next authenticated `GET`) and the relay (2 s poll of `config.json`); tokens are vault-backed, metadata lives in SQLite, and relay resolves/refreshes credentials. Antigravity's interactive loopback OAuth exchange/refresh, v2 userinfo identity, free-tier Cloud Code onboarding with bounded LRO polling, encrypted token context persistence, and account-bound relay project injection are implemented. Paid-tier onboarding remains fail-closed without explicit project input. `mr migrate omniroute` transfers Codex and Antigravity grants by one refresh each, using the client identities in `mr-core/src/published.rs`; this was exercised against a live installation (10 grants imported, all re-refreshed with the reference stopped, and a real completion served through the relay). Copilot tokens issued as `ghu_` GitHub App tokens are refused by GitHub's mint endpoint and are reported as needing a fresh login. T2 client metadata and PKCE remain reverse-engineered/unverified. Cline remains unavailable. **Paste fallback (codex + antigravity):** the panel reveals a "paste the URL you were redirected to" field the moment the browser tab opens, and `POST /api/oauth/sessions/{id}/redirect` hands that URL (full URL, bare query, or `code#state`) to the sign-in that is already waiting; the exchange still happens in the single waiting `complete` call, so the pasted and automatic paths cannot diverge. The pasted `state` is compared against the session's own, a bare code is refused because it carries nothing to compare, and a second paste for one sign-in is refused. Because a waiting `complete` takes its session out of the registry, pasted redirects are parked in a separate slot keyed by session id and raced against the loopback listener — a paste that arrives before the wait starts is still found. Verified end to end against an isolated conductor: wrong state 400, correct state 202, duplicate 400, and a retry now succeeds because a new attempt releases the redirect port the abandoned one held (Antigravity has one registered port and no fallback, so retrying otherwise failed for the whole ten-minute session TTL). |
| `tools/mock-upstream` | **Fault control API wired; behaviors partial** | Detached crate tests pass; `POST /__control` and request-local `x-mock-fault` merge patches are reachable. Not every field in `Fault` is yet applied to response generation. |
| `tools/bench` | **Present; measures the total added overhead** | A detached crate that measures the relay's added latency against the budget in `docs/16-latency-engineering.md` §6, using a local fake upstream and no network or credentials. On an idle machine it measures 14.6 µs at p50 and 110.5 µs at p99 — inside the budget. It is report-or-threshold and deliberately not a CI gate: a microsecond measurement on a shared runner is dominated by the runner's own load. The six per-stage rows of the budget table are still unmeasured. |

Verification run during this session (all local tests used mock/isolated data):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo test --workspace --release
bash scripts/check-secrets.sh
bash scripts/test-installer-systemd.sh
PATH="$HOME/.cargo/bin:$PATH" python3 tests/smoke_e2e.py
node --test panel/test/*.test.mjs
(cd tools/mock-upstream && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test)
# Browser workflow (local opt-in): Playwright and the browser binaries are not project dependencies.
BROWSER=firefox PLAYWRIGHT_MODULE=/path/to/playwright FIREFOX_EXECUTABLE=/path/to/firefox node panel/test/browser.e2e.mjs
```
Every line above was re-run for the revision described here and passed: **1 146 workspace tests**
(1 ignored), a clean release-profile run, clean strict Clippy in the workspace *and* in
`tools/mock-upstream` (22 tests there), `check-secrets: OK` over 140 files, the isolated installer
simulation, the isolated mock-upstream smoke, and 42/42 panel Node tests. The **smoke test, the
installer test and the panel Node tests are wired into CI**; the browser test remains a local opt-in
because Playwright and the browser binaries are not project dependencies.


The OmniRoute migration was verified against a live installation as well, which is a network operation
and so is not part of the list above:

```sh
# Cheap: proves every imported credential is present, readable and usable. Returns the stored
# token without a network call when it is not yet due, so it is safe to re-run.
cargo test -p mr-core --test imported_grants_are_independent -- --ignored --nocapture

# Strong: forces a real rotation from every provider, proving the grants are independently
# refreshable. Spends one single-use refresh token per account. Do not run it in a loop.
MANAROUTER_FORCE_REFRESH=1 \
  cargo test -p mr-core --test imported_grants_are_independent -- --ignored --nocapture
```

Both read this installation's vault and store only — the reference database is never opened — and both
were run with the reference fully stopped. On the live installation the cheap mode reports **10 of 10**
grants usable and the forced mode reports **9 of 10** refreshing: one Codex account was killed by an
earlier, destructive version of this test, which called `refresh_with_context` directly. That method
returns a rotated token set and persists nothing, so on a single-use platform the replacement was
discarded and the account became unusable, in the reference as well as here. The test now goes through
`Refresher::ensure_fresh`, the production path, which persists rotation; the dead account needs an
interactive re-login and nothing in the repository can repair it. See
[the Codex "move, not a copy" warning](17-migrating-from-omniroute.md#warning-for-codex-this-is-a-move-not-a-copy).

## 2. What running it found

Reading the code did not find these; starting the binary did.

**The relay could not start on a fresh install.** `RoutingSnapshot::load_from` wrapped every read
failure in one variant, so the "compile me a fresh one" arm in `RelayState::load` was unreachable and
the process exited with `cannot read snapshot ... No such file or directory`. Fixed twice over: a
missing file is now a distinct error, and — more to the point — the snapshot is treated as a *derived
cache*, so any problem reading it recompiles rather than refusing to start. A cache that can be
rebuilt must never be able to stop the program.

**A keyless relay served everyone.** With no client keys configured, `authenticate` answers `Open`.
That is right on loopback, where refusing everything would make a fresh install unusable, and wrong
anywhere else: binding a keyless relay to a reachable interface hands this machine's provider
credentials to anyone who can route to it. The bind is now refused with exit code 2 when no keys are
configured and the address is not loopback.

Verified by running, not reading: boots on loopback, `/healthz` in 0.5 ms, `/v1/models` served from the
snapshot without contacting a provider, `--host 0.0.0.0` refused, `--host ::1` bound.

## 3. The honest gap in the headline feature

**The hedged race runs, and so does the recovery of a dead winner.**

`select::hedge_decide` implements and tests the entire *decision* policy — eligibility window,
priority winner, immediate resolution when the top participant replies in window. The relay calls
it: a combo with `hedge.enabled` dispatches every participant at once, the highest-priority
participant that answers inside the window is committed, and the losers are dropped. The step loop
and the race share one attempt implementation, so the two paths cannot drift.

The *recovery* half runs too. The relay wraps the committed winner's body
(`crates/manarouter-relay/src/recovery.rs`), so a death after the commit is visible to it, and the
participants that passed the race are kept for it to fall back on. The replacement is the
highest-priority participant that passed the eligibility gate; the switch is invisible before any
byte reached the client, and after that it follows the combo's `midstream_policy` — the
`manarouter.retry` boundary for `restart`, an error for `terminate`. A tool-call delta on the wire
stops the splice under both policies.

The *other* splice — two concurrent answers contaminating each other's tool calls — is fixed. It
was never about recovery: every dialect adapter accumulated per-stream state in a `thread_local!`,
and a thread is not a stream. The relay polls a response body on whichever runtime worker is free,
so two answers in flight on one worker shared a cell, and one stream's tool-argument fragments were
appended to the other stream's call. The client received a second tool call with an empty id and an
empty name, which cannot be executed or matched, and that is what failed the client's own format
validation (`requires toolCallId matching its tool source`). Command Code now owns its state per
stream (`mr_dialects::commandcode::Translator`, held by the relay for exactly one response body),
and the regression is pinned at both layers by an interleaved two-stream test plus a replay of a
verbatim capture from the live endpoint. The remaining dialects still use the per-thread cell and
are reset at a stream boundary; that is weaker, and is the next target.

What is still missing is narrower. `winner_metric` does not yet distinguish `first_response_byte` from
`first_complete_token`: both resolve at the response arrival. No `WinnerFailMode` has been exercised
against a real provider outage — the recovery paths are covered by unit tests only.

A loser's cancellation is local. Dropping the loser closes this relay's connection to the provider,
but a provider may keep generating and keep billing, so a hedge can cost a full completion on every
participant it dispatched.

## 4. Verified implementation properties and limits

* **The relay avoids full JSON interpretation for model lookup.** It scans the buffered body for the
  model and forwards original bytes on passthrough paths. This is *not* zero-copy: the request body is
  buffered. Cross-dialect routes do parse and translate bodies, as required.
* **No conductor IPC on the request path.** The relay reads the compiled snapshot and uses ArcSwap;
  account health is read as one immutable view per request.
* **No credential in a URL.** `AuthKind::QueryParam` is recognized and deliberately not applied.
* **`/v1/models` contacts no provider.** It is served from the compiled snapshot.
* **Dialect translation is reachable.** Request translation is selected per step, with response
  translation, translator path overrides, and forced-stream metadata applied in the relay.
* **Quota APIs are distinct.** `/api/quota` returns cached provider/account quota snapshots and
  supports refresh orchestration for supported integrations; OAuth refresh is reported unsupported.
  This is not the request-usage ledger. The relay now enforces the per-key rolling request/token
  caps in dispatch and writes the settled request-usage rows; the conductor's provider-quota view is
  a separate thing and still does not reflect client-request spend.
* **A modality the route cannot carry is rerouted, not misrouted.** When the selected step's model
  does not accept the request's modality, the relay retries the whole request against the
  `routing.defaults` entry for that modality, resolved against the same pinned snapshot, so the
  target's own provider, account, credential, URL and dialect are used. The budget is one fallback:
  a target that also lacks the modality, or names no known model, fails with HTTP 501
  `modality_fallback_unavailable` rather than looping. Covered by five tests in
  `crates/manarouter-relay/src/server.rs`.

### 4.1 GPU rentals (issue #4) — implemented and exercised end to end, with two named limits

The feature is split the way the rest of the project is: **pure logic in `mr-core`**, the HTTP surface
in the conductor, presentation in the panel, and the request path in the relay.

* `crates/mr-core/src/gpu/catalog.rs` — cards, prices, plans, quant support, all with a source and a
  `verified`/`unverified` marker per figure.
* `crates/mr-core/src/gpu/arch.rs` — a HuggingFace `config.json` (plus a gated repo's `base_model`
  fallback) → cache topology and bytes per token.
* `crates/mr-core/src/gpu/sizing.rs` — the KV-pool arithmetic and the `SizingResult` wire type.
* `crates/mr-core/src/gpu/requantize.rs` — the "this quant does not exist, here is how to make it"
  planner.
* `crates/mr-core/src/rental.rs` — the persisted account and deployment shapes.
* `crates/mr-core/src/gpu/cerebrium.rs` — a `cerebrium.toml` generator (issue #10): a **pure,
  side-effect-free** function from a request to the file text, the deploy command and the base URL —
  it writes no config, no vault, and no filesystem artefact. A test posts the body through the real
  router and asserts the config bytes and snapshot bytes are **unchanged** (20 tests).
* `crates/mr-core/src/lane/` — one descriptor per host, with the platform's own words for what the
  host will not let you do; no HTTP, no clock, no verdicts beyond the ones quoted.
* `crates/manarouter-conductor/src/gpu_api.rs` — **six routes**: catalog, probe, rentals, apply,
  lanes, and cerebrium/bundle (full paths: `GET /api/gpu/catalog`, `POST /api/gpu/probe`,
  `GET /api/gpu/rentals`, `POST /api/gpu/apply`, `GET /api/lanes`,
  `POST /api/gpu/lanes/cerebrium/bundle`).
* `panel/gpu-setup-model.js` (pure) and `panel/view-gpu-rentals.js` (the two-mode view).
* The panel's Cerebrium bundle button is **pending** — `grep -n cerebrium panel/view-gpu-rentals.js`
  returns zero matches; no UI control for Cerebrium bundles exists on the rentals page.
* `crates/manarouter-relay/src/batch.rs` — the durable batch lane that mode B dispatches through.

`crates/mr-core/src/gpu/mod.rs` states the split: that tree contains **no HTTP, no filesystem access
and no clock**, so every number is checkable in a unit test with no network and no GPU.

**What has actually been exercised.** `cargo test -p mr-core gpu::cerebrium` is green — 20 passed.
`node --test panel/test/gpu-setup-model.test.mjs` is green — 113 passed. The whole workspace suite
is green — 1 685 passed, 0 failed, 4 ignored (`cargo test --workspace`) — the panel suite is green
(303 tests, `node --test panel/test/*.test.mjs`), and — the part that matters most — a second
conductor built from this tree was driven over a socket on `127.0.0.1:20999` with its own config and
state directories, through the real sequence: catalog → probe → create account → create deployment →
read the rental back. Live captures of four of those responses are committed under
`docs/research/fixtures/` so that the next change is checked against the real wire shape rather than a
remembered one.

**Four defects that only a socket could find.** Each of these was invisible to a green unit suite on
both sides, because each was a disagreement *between* two correct-in-isolation halves:

1. The generated Modal bundle wrote `""fp8""` — a pre-quoted, pre-escaped `py_str(..)` interpolated
   inside another quote pair. The Python was never executed, so nothing caught it; the response is now
   `ast.parse`d during verification, and a Rust test asserts each argument is quoted exactly once.
2. The probe read `{repo}/resolve/main/config.json`, which answers **307** to a cache host. The shared
   quota client does not follow redirects, so the gated-repo `base_model` fallback could never run.
   `/raw/{rev}/config.json` answers 200 directly; the fix made `arch.source: "base_model"` reachable
   for the first time. `curl -L` would have hidden this — it proves the server is fine, not that the
   product's client works.
3. The panel sent a session ladder shaped like a display table (`{context, sessions}`); the conductor
   re-parses `sizing` into `SizingResult`, whose `SessionsAtContext` has non-`Option` `context_len`,
   `blocks_per_session` and `bytes_per_session`. HTTP 400. Rows are now emitted in the server's shape.
4. `GET /api/gpu/rentals` reported `fit: {known: true, kv_pool_tokens: null, sessions_at_max_len:
   null}`: the panel computed the pool but never forwarded the derived fields, and `fit_of` reads them.
   Fixed on **both** halves — the panel now emits `kv_pool_tokens_aligned`, `kv_blocks`,
   `kv_bytes_per_session_at_max_len` and `simultaneous_sessions_at_max_len`, and the conductor falls
   back to dividing the pool when a client forwards only the raw figure. Verified in all three states
   (stale payload, regenerated payload, stored record).

The general lesson, worth stating because it will recur: **a `bool`-returning usability predicate that
reads one field will certify an object missing everything else**, and `known: true` beside nulls is a
degraded summary rather than a healthy one. Both halves of a JSON contract need a test that posts the
client's real payload at the real server.

**Limit 1 — no Modal deployment has ever been created by this code.** The `modal` CLI and the Python
`modal` package are absent from this machine, and Modal publishes no public deployment REST API
(`/docs/api`, `/docs/rest`, `/docs/guide/deploying-apps` and `api.modal.com` all fail; the CLI is the
only documented path and has no `--detach`). Manarouter therefore generates the bundle, `ast.parse`s
it, reports its SHA-256, and prints the exact `modal deploy` command plus the proxy-token headers a
curl call needs. A human runs it. The endpoint then becomes an ordinary `modal-vllm` provider whose
`base_url` the operator pastes at the review step. That boundary is deliberate and documented, not a
gap to be closed by pretending.

**Limit 2 — requirement 10 cannot be implemented as literally written.** Modal documents no per-GPU
free/paid gating: all eleven cards carry an unconditional per-second price, and "using a GPU requires
having a valid payment method on file" applies to the account as a whole. A `(Paid)` badge on cards
that were never refused would be fabricated. The catalogue records `free_plan: "unknown"` for every
card except H100, which was **observed** refused on the Starter plan, with `free_plan_source` carrying
that provenance separately from the price's (`documented`); `(Paid)` renders only for `"unavailable"`.
The plan limits are quoted with their source (Starter: $30/month plus compute, 10 GPU concurrency;
Team: $250/month plus compute, 50).

**The batch lane, and the question the issue actually asks.** `batch.rs` is a durable queue — an
internal queue rather than a pass-through, per-item isolation, retry only for transience (three
attempts, never on a 4xx), resume from disk, cancellation, token admission through `kv_budget.rs`, and
cost in dollars with the boot fee on its own line. What the issue asks beyond that is the economic
question — *is this batch too small to be worth a cold start* — and the answer is now reported from
measured dollars rather than a predicted token count: the usage block carries `boot_share_pct` and
`cold_start_verdict` (`boot_dominates` above half the bill, `work_dominates` below, `unknown` when
there is no price to divide). It also carries the figure the issue's own table compares cards on,
`usd_per_million_output_tokens` (the whole bill, boot included, over the output tokens the provider
counted) and `serving_usd_per_million_output_tokens` (the same figure with the boot fee removed, which
is what a card-to-card comparison needs). Both are divisions of two measured numbers and both are
`null` when either is missing — a batch that produced no output tokens has no denominator, and a price
of zero matches no measured boot time, so neither figure is invented to fill the gap. A pre-flight hold would need a throughput figure this build does not
have for any card; inventing one is the guess the module refuses to make.

Still not verified: no batch has been submitted to a real GPU and observed to complete, so every
figure in the lane is a computation over a recorded measurement rather than a fresh observation.

**The other four hosts (issues #10–#13), and why only one of them grew code.** The catalogue now
answers two different questions separately, because they have different answers per host: *what
hardware exists* (`GET /api/gpu/catalog`) and *what may I do with it* (`GET /api/lanes`). The second
route reports, per host, the shape of the lane (serving / batch / attended), whether a request can be
answered while it is open, how a job is submitted, which credential kind is needed, session and idle
limits, the published quota rules, and — the part that decides two of the four — a **refusal** with the
platform's own clause and URL when the platform itself forbids the feature that was requested. The
refusals are derived at request time from the same `LaneFacts` the route reports, so a row can never
claim "usable" while a refusal exists for that traffic type.

* **#10 Cerebrium is implemented**, additively: `POST /api/gpu/lanes/cerebrium/bundle` renders the
  `cerebrium.toml` and the deterministic base URL
  `https://api.cerebrium.ai/v4/{project}/{app}/v1` — **side-effect-free**: no config write, no vault
  write, no filesystem artefact. That route is deliberately separate from `POST /api/gpu/apply`
  rather than a generic "any platform" path: apply's inputs (a saved `RentalDeployment`, a Modal
  `gpu_by_id`, a vault credential, a *pasted* `base_url`) are all wrong for a host whose URL is
  computable and whose artefact needs no token. A test posts the body through the real router and
  then asserts the config bytes and the snapshot bytes are **unchanged**, proving nothing was
  persisted.
* **#11 Kaggle and #13 molab are documented refusals, not missing work.** Kaggle's Terms §3 allows one
  active account and restricts the service to "your own internal, personal, non-commercial use"; the
  issue asks for two pooled accounts rotated per request. molab's restrictions page prohibits "remote
  control such as by SSH", "exposing molab compute to outside traffic" and "using notebooks for
  non-interactive jobs", which is precisely the tunnelled OpenAI-compatible server the issue specifies.
  The batch half of Kaggle's lane is real and reported as usable; the multi-account half is refused with
  the sentence quoted.
* **#12 Saturn Cloud is buildable for its own reason**: its **Token Factory inference endpoints are
  already an ordinary OpenAI-compatible provider** (base URL, bearer token, `GET /v1/models`), and the
  whole lifecycle is API-creatable. So the lane needs a catalogue row and documentation, not a tunnel.
  Its spec'd `ssh -L` port-forward design is unverified and is not claimed.

Two honesty constraints carried into the UI: a `(Paid)` per-card badge is only shown where the platform
really does gate card types by plan (Cerebrium, enforced at deploy time), not on Modal, which documents
no such gating; and the panel invents no facts — it renders the route's rows, including the refusal
clauses, verbatim.

The panel Cerebrium button is **pending, not implemented**: `grep -n cerebrium panel/view-gpu-rentals.js` returns
nothing, so no Cerebrium UI is claimed here. The conductor route exists; the panel does not yet render it.

Test totals: `cargo test -p mr-core gpu::cerebrium` → **12 passed; 0 failed; 0 ignored**.
`node --test panel/test/gpu-setup-model.test.mjs` → **tests 113, pass 113, fail 0**.

## 5. Claims that remain unverified or false

* That every hedge failover mode has been exercised under a real provider outage. All four are
  dispatched (§3), but the recovery paths are covered by unit tests rather than by a live outage
  drill, and `stable-fallback` honours its nominated step only when that step is still routable.
* That Antigravity works end to end. Authorization, token exchange, refresh, and Google userinfo
  identity are implemented, but Cloud Code onboarding and project injection/context are incomplete.
  The registered T2 OAuth client/PKCE behavior is reverse-engineered and unverified; do not rely on
  production access until a user explicitly opts into a live login and validates the upstream API.
  Codex and Copilot also still need real-provider opt-in smoke verification.
* That the provider catalogue was executed against every entry. `docs/providers/catalog.md` marks
  uncertain entries and the catalogue is not a live reachability guarantee.
* That every panel route works in a real browser. Firefox E2E verifies login and client-key create/rotate against a mock API; the remaining routes are still unverified. The boot failure path is visible.
* That all mock fault fields are effective: control and request-local patch endpoints are wired, but
  a subset of fault knobs still needs implementation/coverage.
* That the latency budget is enforced. `tools/bench/` has a harness that fails over budget, but it is deliberately not a CI gate.
* That the panel's hedge editor round-trips through the backend **in a browser**. The wire form is
  now correct and pinned twice: `panel/format.js` (`parseWinnerFail`/`buildWinnerFail`) emits and
  accepts the internally tagged shape (`{"mode":"rerun-excluding","max_rounds":2}`) that
  `WinnerFailMode` deserializes, with a Node test on the round trip and
  `on_winner_fail_uses_the_internally_tagged_wire_form` in `crates/mr-core/src/dialect.rs` pinning the
  Rust side. The legacy externally tagged form is still *read*, so a config written before the fix
  renders correctly. What is not verified is the full editor flow in a real browser: the Firefox E2E
  covers login and client-key create/rotate only.

## 6. Preserved partial attempts

Three pieces of real work exist only as gitignored drafts, because each was left in a state that does
not compile and reverting the tracked files was the only way to keep the tree green. They are
resumable and they are worth reading before starting over:

| Path | What it is |
|---|---|
| `.research/incomplete-cli-attempt/` | `mr-cli`: 805-line `main.rs`, 544-line `output.rs`, its `Cargo.toml`. Declares eleven modules and writes two of them, so it fails to build. The *layout* is the useful part. |
| `.research/store-incomplete-attempt.rs` | `mr-core/src/store.rs`: 867 lines, truncated mid-`impl`. References `struct Store` and `struct UsageWriter`, neither of which was ever written. |
| `.research/incomplete-mock-faults-attempt.rs` | `tools/mock-upstream`: 1360 lines attempting the `faults.rs` contract. Superseded — the committed `lib.rs` compiles and its 13 tests pass, so start from that, not this. |

These are gitignored deliberately. Shipping a file that does not compile, to record that someone once
tried, trades a working tree for a note — and the note can live outside the build.

## 7. Why this file is in the repository

A reader deciding whether to use this project needs to know which of its sentences are backed by
running code. Putting that question in one place, with a command beside each answer, is cheaper for
them than discovering it one 404 at a time.
