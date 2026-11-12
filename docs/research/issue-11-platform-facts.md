# Issue 11 — Kaggle Notebooks as an inference lane: verified platform facts

Checked 2026-11-12. Method: `curl -sS -m 20` with HTTP status printed. `kaggle.com/docs/*`,
`/terms`, `/aup` return **200 but are client-rendered SPA shells** (~5 KB, no SSR payload), so their
text was read through a text-rendering proxy (`https://r.jina.ai/<url>`, HTTP 200) of the same page;
canonical URLs are cited. CLI/SDK facts come from the published source (`Kaggle/kaggle-cli`,
`Kaggle/kaggle-sdk-python`), which outranks docs prose when the two disagree.

## Facts

| Fact | Value | Source | Status |
|---|---|---|---|
| Auth, primary | API token created at `kaggle.com/settings/api`; sent as `Authorization: Bearer <KGAT_…>` | https://www.kaggle.com/docs/api (+ `kagglesdk/kaggle_http_client.py` `BearerAuth`) | verified |
| Token env / file | `KAGGLE_API_TOKEN`, or file `~/.kaggle/access_token` | https://github.com/Kaggle/kaggle-cli/blob/main/skills/references/auth.md | verified |
| Auth, interactive | `kaggle auth login` (OAuth 2.0 + PKCE, opens browser; `--no-launch-browser` prints URL) | https://github.com/Kaggle/kaggle-cli/blob/main/skills/references/auth.md | verified |
| Auth, legacy | `~/.kaggle/kaggle.json` = `{"username","key"}` ("Create Legacy API Key"), or `KAGGLE_USERNAME`+`KAGGLE_KEY`; sent as **HTTP Basic** (`requests` auth tuple) | `.../skills/references/auth.md`; `kaggle_http_client.py` `_try_fill_auth` | verified |
| Token vs key | `KGAT_`+32 hex is rejected if put in `KAGGLE_KEY`/`kaggle.json`; legacy key must be paired with username | `src/kaggle/api/kaggle_api_extended.py` (`_ACCESS_TOKEN_RE`, `_LEGACY_API_KEY_RE`) | verified |
| Config dir | `KAGGLE_CONFIG_DIR` else `~/.kaggle`, else `$XDG_CONFIG_HOME/kaggle`; read **at import** (class attribute); `os.makedirs` at import | `src/kaggle/api/kaggle_api_extended.py` | verified (code) |
| Credential scope | one credential per process: config dir fixed at import, one session-wide `session.auth` | same + `kaggle_http_client.py` | verified (code) |
| Rate limits | "dynamic rate limiting on both the public API and … the website"; HTTP 429; no published numbers | https://www.kaggle.com/docs/api | verified |
| API can create+run non-interactively | yes: `kaggle kernels push -p DIR` → "If the kernel … exists … it will be updated. Otherwise, a new kernel will be created. After uploading, Kaggle will attempt to run the kernel." | https://github.com/Kaggle/kaggle-cli/blob/main/docs/kernels.md | verified |
| Poll / fetch | `kernels status <owner>/<slug>`, `kernels logs`, `kernels output [-p DIR] [--file-pattern REGEX]` | docs/kernels.md | verified |
| Metadata, required | one of `id` **or** `id_no` ("If both are, `id_no` will be preferred"), `code_file`, `language: python`, `kernel_type: script\|notebook`, `title` ≥ 5 chars | https://github.com/Kaggle/kaggle-cli/blob/main/docs/kernels_metadata.md | verified |
| Metadata, optional | `is_private` (default `true`), `enable_gpu`, `enable_internet`, `machine_shape`, `dataset_sources`, `competition_sources`, `kernel_sources`, `model_sources`, `docker_image`, `keywords` | docs/kernels_metadata.md; `kernels_push` | verified |
| `accelerator` field | **does not exist**; metadata field is `machine_shape`, CLI flag is `--accelerator` (overrides metadata) | `kernels_push`, `docs/kernels_metadata.md` | verified |
| Accelerator ids (current) | `NvidiaTeslaT4` (T4×2, default GPU), `NvidiaTeslaA100`, `NvidiaL4`, `NvidiaL4X1`, `NvidiaH100`, `NvidiaRtxPro6000`, `TpuV5E8`, `TpuV6E8`; "some … only available to participants of specific competitions, and some … only to Kaggle admins" | docs/kernels.md ("as of Sep 2026") | verified |
| Retired ids | `NvidiaTeslaP100`→runs on T4; `TpuV38`,`Tpu1VmV38`→`TpuV5E8`; `TpuV232`,`TpuV2256`→CPU. Push still succeeds, server substitutes | docs/kernels.md; `_RETIRED_ACCELERATORS` in `kaggle_api_extended.py` | verified |
| Id validation | none client-side: "The allowed names are in an enum that is not currently included in kagglesdk"; arbitrary string is sent | `kernels_push` | verified (code) |
| Run status values | `QUEUED`, `RUNNING`, `COMPLETE`, `ERROR`, `CANCEL_REQUESTED`, `CANCEL_ACKNOWLEDGED` | `kagglesdk/kernels/types/kernels_enums.py` | verified |
| Per-run timeout | `kaggle kernels push -t <SECONDS>` → `session_timeout_seconds` | docs/kernels.md; `kernels_push` | verified |
| Session length | 12 h for CPU **and GPU** sessions, 9 h for TPU sessions | https://www.kaggle.com/docs/notebooks#technical-specifications | verified |
| GPU quota | "The quota resets weekly and is 30 hours or sometimes higher depending on demand and resources" | https://www.kaggle.com/docs/efficient-gpu-usage | verified |
| TPU quota | "up to 20 hours per week of TPUs and up to 9h at a time in a single session" | https://www.kaggle.com/docs/tpu | verified |
| Interactive idle timeout | **conflicting**: 20 min ("20 minutes of idle time for your interactive session") vs 60 min ("Interactive sessions remain active until they reach the 60 minute idle timeout limit") | docs/notebooks vs docs/efficient-gpu-usage | conflicting |
| Quota readout (programmatic) | `kaggle quota` / `-v` CSV `resource,used,remaining,total,refreshAt` (hours); remaining clamped at `0.00h`; `refreshAt` = `quota_refresh_time` | `.../skills/references/quota.md`; `quota_view` | verified |
| Behaviour at quota limit | not stated in any doc reached (assumed: run fails); no error string captured | — | unverified |
| Inbound HTTP endpoint | **no mechanism**: kernel API surface has create/cancel session, save, status, logs, list/download output — no port, ingress, webhook or public-URL call; the only per-file egress is `ApiKernelSessionOutputFile.url` (download) | `kagglesdk/kernels/types/kernels_api_service.py` | verified (absence) |
| Closest thing to a live endpoint | "Kaggle Jupyter Server" — a "Colab Compatible URL" lets the **owner** attach Colab/VS Code to their own running session; not a public API surface | https://www.kaggle.com/docs/notebooks | verified |
| Internet in kernel | per-session toggle (`enable_internet`); no allowlist/proxy restriction documented; on by default for new notebooks in the UI | docs/notebooks | verified |
| `enable_internet` default | metadata doc says default `false`; code sends `get_bool(meta, "enable_internet", True)` — i.e. **on** if omitted | docs/kernels_metadata.md vs `kernels_push` | conflicting |
| Internet prerequisite | account phone verification is needed to switch internet on — reported by several independent guides, **not** stated in Kaggle docs | https://stackoverflow.com/questions/68142524/cannot-access-internet-on-kaggle-notebook; https://comptools.climathematch.io/tutorials/TechnicalHelp/Tutorial_kaggle.html | unverified (3rd-party) |
| Working dir | "Up to 20 GBs of output … may be saved to disk in `/kaggle/working`", auto-saved per version, reusable as an input later | docs/notebooks | verified |
| Scratch space | "Additional scratchpad disk space (outside /kaggle/working) that will not be saved outside of the current session" | docs/notebooks#technical-specifications | verified |
| Dataset limits | 200 GB per dataset, 200 GB max private datasets, 50 top-level files | https://www.kaggle.com/docs/datasets#technical-specifications | verified |
| Weight caching | attach a Kaggle Dataset/Model via `dataset_sources`/`model_sources`; a notebook's output can be published as a dataset | docs/notebooks, docs/datasets | verified |
| Push exclusions | `DEFAULT_IGNORE_PATTERNS = [".git/", "*/.git/", ".cache/", ".huggingface/"]` — HF caches are not uploaded by `push` | `kaggle_api_extended.py` | verified (code) |
| Secrets | `kaggle_secrets.UserSecretsClient().get_secret("LABEL")` inside the kernel; created in the notebook editor, no CLI support | docs/kernels.md | verified |
| ToS — permitted use | "You will only use the Services for your own internal, personal, non-commercial use, and not on behalf of or for the benefit of any third party" | https://www.kaggle.com/terms §3 | verified |
| ToS — one account | "you may not have, control, or operate under more than one active Kaggle account or Kaggle User ID"; remedy = ban/revoke without notice | https://www.kaggle.com/terms §3 | verified |
| ToS — account sharing | "You are not allowed to allow multiple individuals to operate under one Kaggle User ID" | https://www.kaggle.com/terms §3 | verified |
| ToS — prohibited | cryptomining; crawling/scraping "any page, data, or portion … (through use of manual or automated means)" | https://www.kaggle.com/terms §4 | verified |
| AUP — prohibited | abuse of resources incl. "**server farming**", "activity unrelated to ML data science", DDOS, "excessive crawling" | https://www.kaggle.com/aup (effective 2025-06-22) | verified |
| Headless login | token creation and `auth login` both need a human in a browser **once**; afterwards the CLI is fully headless | docs/api; skills/references/auth.md | verified |
| 2FA / captcha | no 2FA documented anywhere; a CAPTCHA interstitial was hit on `kaggle.com/discussions/...` during this research | — | unverified |

## Corrections to `issue-11-spec.md`

1. "a single P100, 2×T4, TPU v3-8" is stale: P100, TPU v3-8 (`TpuV38`) and `Tpu1VmV38` are **retired**
   and silently substituted (T4 / `TpuV5E8`); default GPU is `NvidiaTeslaT4` = 2×T4.
2. `id_no` is **not** required — "One of `id` or `id_no` must be specified"; `id_no` wins if both are present.
3. There is **no `accelerator` field** in `kernel-metadata.json`; it is `machine_shape`, and `--accelerator`
   is only the `push` flag.
4. `~/.kaggle/kaggle.json` is now the **legacy** path (HTTP Basic). Primary auth is a `KGAT_` API token
   (Bearer) or OAuth; `KGAT_` tokens are explicitly rejected inside `kaggle.json`.
5. "live number at kaggle.com/me/account" can be replaced by `kaggle quota` (machine-readable CSV).
6. The spec's own acceptance criterion "two pooled accounts rotated per request" is **prohibited by the
   Terms** (one active account per person), so it cannot be built as written.

## What this means for Manarouter (≤25 lines)

- An unattended batch lane **is technically possible**: `kaggle kernels push` (create-or-update-and-run),
  poll `kernels status` until `COMPLETE`/`ERROR`, then `kernels output` — all headless once one API token
  is placed in the subprocess environment. No browser, no interactive session, no inbound socket.
- It is **not a request/response lane**. Queueing, image pull, weight download and model load put a floor
  of minutes per call; the file-only return path (`response.json` in `/kaggle/working`, ≤20 GB) means the
  adapter must be a submit-and-poll job, with a hard deadline and a 504 past it — exactly the shape the
  spec already chose. Keep the deadline; do not pretend it is chat.
- The binding constraint is **quota, not session length**: 30 h GPU/week (sometimes more) vs 12 h/run.
  At realistic per-request cost that is hundreds of requests per week per account, not thousands. Read
  headroom from `kaggle quota` into `provider_quota_state`; treat `remaining` near 0 as lane-down.
- **Licensing is the real blocker, not the API.** The Terms limit use to "your own internal, personal,
  non-commercial use, and not on behalf of or for the benefit of any third party". One operator routing
  their own traffic is inside that sentence; a gateway serving other people's requests, or a paid product,
  is outside it. The AUP separately bans "server farming" and "activity unrelated to ML data science".
- **Multi-account pooling must be dropped.** The Terms forbid operating more than one active account, with
  banning as the remedy. Ship **one account per deployment**; keep the fresh `KAGGLE_CONFIG_DIR` per spawn
  for hygiene, not rotation. So: no `--accounts 2`, no identity failover, no retry storm (429s are dynamic
  and unpublished).
- Verdict: an opt-in, single-account, batch-only lane, documented as ToS-sensitive and quota-bounded —
  never marketed as an inference backend.

## Could not verify

- Behaviour at quota exhaustion (error text, whether the run is queued or rejected) — nothing in docs or source.
- Any Kaggle-hosted statement that phone verification is required for internet/GPU (only third-party guides).
- Whether 2FA exists on Kaggle accounts, and whether account creation needs a phone number.
- The real idle timeout (20 vs 60 min) — the two Kaggle docs contradict each other.
- Actual per-request latency on T4×2 for a 7B-class model — no measurement was run (read-only task).
- Whether `NvidiaL4`/`A100`/`H100` are reachable for a normal free account (docs say competition/admin-gated only).
- A Kaggle-published retention period for notebook outputs (how long `kernels output` keeps working).
- Numeric API rate limits — docs describe dynamic limiting without figures.
