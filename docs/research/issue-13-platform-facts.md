# molab platform facts (issue 13)

> **Historical (2026-11-13).** This document specifies molab. The molab lane, its routes and its
> vault entry were removed from Manarouter on 2026-11-13, so nothing below is implemented any
> more. It is kept as the research and design record; the lane surface that is live today is
> [../26-gpu-lanes.md](../26-gpu-lanes.md).

Verified against primary sources on **2026-11-12** with `curl`. Sources are molab's own
pages, `docs.marimo.io`, marimo's blog, and molab's live Clerk auth configuration
(`https://clerk.marimo.io/v1/environment`, the auth backend molab's frontend uses).
Anything not found on a primary page is marked UNVERIFIED, not guessed.

Method note: `web_fetch` is broken in this environment; all pages were fetched with
`curl -sSL -m 20`. molab is a Next.js app, so page *text* is partly client-rendered;
facts taken from HTML shells are marked as such. The Clerk config and the `llms.txt`
/ `.md` doc variants are machine-readable and were read verbatim.

## Facts

| # | Fact | Value (quoted where it matters) | Source | Status |
|---|------|----------------------------------|--------|--------|
| 1 | What molab is | "a free cloud-hosted marimo notebook environment"; a hosted service, not the open-source notebook | https://docs.marimo.io/guides/molab/ | verified |
| 2 | Who runs it | Marimo Inc.; since 2026-06-01 "run natively on CoreWeave Cloud" / "CoreWeave sandboxes" | https://marimo.io/blog/reintroducing-molab | verified |
| 3 | Corporate status | "Marimo is joining CoreWeave" (2025-10-30) | https://marimo.io/blog/joining-coreweave | verified |
| 4 | Access model | open sign-up, no invite/waitlist: "If this is your first-time using molab, you'll be prompted to create an account." | https://marimo.io/blog/announcing-molab | verified |
| 5 | Sign-up gate | open: `sign_up.mode = "public"`, `allowlist.enabled = false`, no captcha, `mfa.required = false`. A country `blocklist.enabled = true` (see #31). | https://clerk.marimo.io/v1/environment | verified |
| 6 | Cost | "Is molab free? Yes." — and "We're providing molab — GPUs and all — for free, as long as usage is reasonable." | https://docs.marimo.io/guides/molab/ , https://marimo.io/blog/reintroducing-molab | verified |
| 7 | Paid tiers | none exist: `billing.enabled=false`, `has_paid_user_plans=false`, `has_paid_org_plans=false` | https://clerk.marimo.io/v1/environment | verified (config, not prose) |
| 8 | GPU model | "NVIDIA RTX Pro 6000 Blackwell GPU — with 96 GB of VRAM and 125 TFLOPS — to any notebook" | https://docs.marimo.io/guides/molab/ | verified |
| 9 | How GPU is attached | manual UI toggle: "Toggle the GPU by clicking the notebook specs button in the app header." No API. | https://docs.marimo.io/guides/molab/ | verified |
| 10 | Default compute | "4 CPUs and 32 GB of RAM" per notebook | https://docs.marimo.io/guides/molab/ | verified |
| 11 | GPU-hour quota | none published; the only stated limit is "as long as usage is reasonable" | https://marimo.io/blog/reintroducing-molab | verified (absence) |
| 12 | Session cap | "Notebooks can run for as long as 12 hours before molab shuts them down" | https://docs.marimo.io/guides/molab/ | verified |
| 13 | Idle reclamation | "Notebooks that are idle for more than 90 minutes are automatically shut down." | https://docs.marimo.io/guides/molab/ | verified |
| 14 | What survives teardown | source at any depth, file-browser uploads, `.env`, `mo.persistent_cache`; "Everything else in the workspace is treated as scratch and is cleared" | https://marimo.io/pages/molab/storage | verified |
| 15 | Storage cap | "persist only your source files and files under 1GB"; >1 GB files "will not be persisted on notebook shutdown" | https://marimo.io/pages/molab/storage | verified |
| 16 | Login provider | Google OAuth (`oauth_google`: enabled, `authenticatable: true`) plus email code | https://clerk.marimo.io/v1/environment | verified |
| 17 | GitHub as login | **No.** `oauth_github` is `enabled: true` but `authenticatable: false`; GitHub is a notebook-sync integration only | https://clerk.marimo.io/v1/environment | verified |
| 18 | Auth host | Clerk at `accounts.marimo.io`; device-flow endpoint `accounts.marimo.io/device` exists | https://clerk.marimo.io/v1/environment | verified |
| 19 | Non-interactive API/token | **none**: `api_keys_settings.enabled=false`, user and org API keys disabled; no documented job/run API | https://clerk.marimo.io/v1/environment | verified |
| 20 | Runtime shape | Linux containers, not WASM: "molab notebooks are containers running marimo"; WASM is a separate preview/export path | https://docs.marimo.io/guides/molab/ | verified |
| 21 | Arbitrary Python / pip | yes: built-in package manager "will install additional packages as you import them, using `uv`", updating `pyproject.toml` | https://marimo.io/blog/announcing-molab | verified |
| 22 | Dependency persistence | 2025 post: "we plan to add support for syncing these dependencies"; not confirmed shipped | https://marimo.io/blog/announcing-molab | unverified |
| 23 | Bind a port / expose HTTP | no documented mechanism; explicitly abuse — "connecting to remote proxies or otherwise exposing molab compute to outside traffic" | https://marimo.io/pages/molab/restrictions | verified |
| 24 | Share-URL shape | `molab.marimo.io/notebooks/<id>` → 200 read-only preview; `/notebooks/<id>/app` → 200 app view; **`/p/<id>` → 404** | live probe of `nb_TWVGCgZZK4L8zj5ziUBNVL` | verified |
| 25 | What a share link gives | "Viewers will see a static preview of your notebook and the option to fork it into their own workspace." Not a live server. | https://marimo.io/blog/announcing-molab , https://docs.marimo.io/guides/molab/ | verified |
| 26 | SSH | prohibited: "remote control such as by SSH" → "terminated without warning" | https://marimo.io/pages/molab/restrictions | verified |
| 27 | Tunnel | prohibited (same clause as #23); also "reselling, proxying, or re-exposing molab's AI endpoints" | https://marimo.io/pages/molab/restrictions | verified |
| 28 | Non-interactive jobs | prohibited: "using notebooks for non-interactive jobs" → termination without warning | https://marimo.io/pages/molab/restrictions | verified |
| 29 | Third-party traffic | "reselling compute to third parties" is listed abuse; account "may be banned without notice" | https://marimo.io/pages/molab/restrictions | verified |
| 30 | Enforcement | "Your account may be banned without notice if you abuse molab"; 14-day appeal window | https://marimo.io/pages/molab/restrictions | verified |
| 31 | Geographic limits | accounts from a listed set of countries/regions "are not permitted" (incl. China, Hong Kong, Russia, Iran) | https://marimo.io/pages/molab/restrictions | verified |
| 32 | Notebook privacy | "notebooks are public but undiscoverable by default"; "like GitHub Gists" | https://docs.marimo.io/guides/molab/ , https://marimo.io/blog/announcing-molab | verified |
| 33 | GitHub sync | "synced" notebooks use GitHub as source of truth; preview statically, on an ephemeral server, or via WASM | https://docs.marimo.io/guides/molab/ | verified |
| 34 | Upload / download | sidebar file browser uploads; download button gets the notebook directory; `marimo edit <molab-url>` "brings just the notebook file down, and does not include your attached storage" | https://docs.marimo.io/guides/molab/ | verified |
| 35 | Remote storage | recommended beyond limits: own cloud storage over fsspec, or Google Drive | https://marimo.io/pages/molab/storage | verified |
| 36 | Agent access (marimo pair) | a local agent can drive a *running* notebook: "You can also pair your local agent with notebooks on molab" | https://docs.marimo.io/guides/generate_with_ai/marimo_pair/ | verified |
| 37 | anywidget / "Run as app" | sharing affordances only ("Run as app", slides, iframe embed); no ingress or API surface | https://docs.marimo.io/guides/molab/ | verified |
| 38 | Concurrent sessions | Clerk `single_session_mode: true` — one active session per user | https://clerk.marimo.io/v1/environment | verified (config) |
| 39 | More compute | "reach out to us" (Discord), not a purchasable tier | https://docs.marimo.io/guides/molab/ | verified |

## What this means for Manarouter

**Attended because there is no provisioning API and no ingress — and because the ingress the spec
proposes is explicitly forbidden.**

1. *Attended by design, not omission.* No API keys (#19), no job API, no session-start call: a human
   signs in with Google (#16), clicks the GPU toggle (#9), runs the cells. `mr` cannot change that.
2. *The tunnel is a ban risk, not merely an unattended-start gap.* The spec's `lane.py` (cloudflared /
   `ssh -R`, OpenAI server, relay traffic) hits four named abuses — exposing compute (#23), SSH (#26),
   non-interactive jobs (#28), third-party traffic (#29) — penalised by "banned without notice" (#30),
   per account, i.e. the exact pool the design depends on.
3. *`mr lane attach` stays implementable and useful.* Probe, 12 h expiry (#12), deregister and `status`
   are local behaviours that violate nothing. Keep them; drop the claim that this is an inference backend.
4. *Multi-account costs two manual steps each.* `single_session_mode` (#38) is one live session per
   account: N accounts = N browser profiles, N Google sign-ins, N notebooks started by hand.
5. *No way to reach a server inside molab, and not permitted.* No public URL for a bound port, no tunnel,
   SSH prohibited (#23, #26, #27). `/notebooks/<id>` and `/notebooks/<id>/app` (#24) serve marimo's own UI
   as a read-only preview (#25), not an OpenAI endpoint. `marimo pair` (#36) edits notebooks, not proxies.
6. *Persistence fits notebooks, not models.* 1 GB cap, scratch cleared on teardown (#14, #15); weights are
   re-fetched from HF/S3/Drive each session (#35) — another manual multi-minute step.
7. *Public-but-undiscoverable (#32) means a token in a cell is published.* `getpass` is the right
   instinct; the token still guards a server that should not be exposed at all.

## Could not verify

- **Any hourly GPU quota, concurrency cap, or fair-use number.** No primary page states one; #11
  is an absence of documentation, not a documented "unlimited".
- **Whether GPU sessions get the full 12 h.** Both pages state 12 h globally without a GPU caveat;
  no page confirms or denies a shorter GPU cap.
- **Whether dependency sync (#22) shipped.** The 2025 announcement says "plan to"; the 2026 guide
  is silent. Assume packages reinstall each session.
- **Whether `molab.marimo.io/p/<id>` ever existed.** It 404s today (#24); the brief's question is
  answered only for the current URL shape.
- **Exact wording of the molab ToS applicable to inference resale.** `marimo.io/pages/legal/terms`
  is dated 2022-09-22, generic, and mentions no molab/compute clauses; the operative document is
  the restrictions page (#23–#31). No molab-specific ToS page was found.
- **Whether a GPU notebook can be started without a browser session** (e.g. a direct API call the
  frontend uses). Not probed: doing so would cross from research into circumvention.
- **`marimo-team/molab` on GitHub** returns 404 — no public source repo for the platform.
