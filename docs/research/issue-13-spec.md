# ISSUE 13: Deploy molab as an attended inference lane with multi-account endpoint switching
# Deploy molab as an attended inference lane with multi-account endpoint switching

Supersedes #9 ("Do not automate molab; document and add a manual GitHub handoff instead"). That issue
was right that molab cannot be *provisioned* by software, and wrong to stop there: provisioning is
attended, but **attachment, health checking, failover and account switching are all automatable**, and
those are what make molab usable as an inference backend at all.

## What molab is, precisely (verified)

* **Free cloud marimo notebooks**, default **4 vCPU / 32 GB RAM**, with an **NVIDIA RTX Pro 6000
  Blackwell (96 GB VRAM, ~125 TFLOPS)** attachable from the notebook's specs button.
  ([molab guide](https://docs.marimo.io/guides/molab.md))
* **Sessions are capped at 12 hours**, and a session idle for more than **90 minutes is shut down**.
* **Notebooks are public but undiscoverable** — a URL is an unguessable link, not a private document.
  This is a security constraint, not a footnote: anything written into the notebook source is
  effectively published.
* **GitHub-synced notebooks** make GitHub the source of truth, so the lane's notebook can live in
  this repo (`deploy/molab/`) and be pulled into molab rather than pasted.
* `marimo edit https://molab.marimo.io/notebooks/nb_…` downloads a notebook locally; WebAssembly
  embeds exist for output only.
* **There is no documented job API and no inbound serving endpoint.** No "run this notebook" call, no
  port exposure, no webhook. Everything a lane needs must therefore come from *inside* the notebook
  plus a tunnel the operator controls.
* Persistent storage is limited (sidebar uploads, `mo.persistent_cache`); real datasets belong on
  remote storage (Drive/HF/S3).
* Compute beyond the default is a support conversation (marimo Discord), not an API call.

## Design: the lane is the notebook, the automation is around it

`deploy/molab/lane.py` is a marimo notebook that, when run by the operator in molab:

1. installs the server dependencies and loads the model from remote storage / Hugging Face;
2. starts an OpenAI-compatible server on `127.0.0.1:8000` with a **bearer token obtained from
   `getpass`** — never a cell literal, because the notebook is public;
3. opens a tunnel (cloudflared quick tunnel, or `ssh -R` to a host the operator controls) and prints
   the URL **and** the exact `mr lane attach` command to run locally;
4. heartbeats: writes a timestamp to a small object in the operator's storage on a timer, so a stale
   session is detectable from outside.

`mr lane attach molab --url <tunnel-url> --secret-stdin` is the automated half: it verifies
`GET /v1/models` with the token, registers `lane-molab` as an `openai-compatible` provider in
`config.json` (picked up by the relay's 2 s reload), and starts a **supervisor** that:

* probes `/v1/models` every 30 s;
* on N consecutive failures (default 3) deregisters that account's endpoint and returns `502` to any
  in-flight request so the relay reroutes per its existing semantics;
* expires the attachment at the session's 12 h mark even if probes still succeed, because the
  notebook will die then and a predictable removal beats a racing one;
* `mr lane down molab --account <label>` removes it early.

### Account switching, concretely

This platform makes the "seamless" requirement literal, because the endpoints are long-lived and
manually started:

* **One lane account per molab account** — `lane-molab` with accounts `molab-a`, `molab-b`, … each
  identified by `(url, token)`. Tokens go to the vault via `--secret-stdin`; `config.json` holds the
  URL and a credential reference only.
* **Warm-pool semantics.** Each account is attached by its own attended notebook. When account A's
  session hits 12 h, or idles out, the supervisor marks A unusable and the relay's `key_strategy`
  moves subsequent requests to account B — no config edit by the operator, which is the whole point.
* **The operator's remaining job is honest and small:** start a notebook per account, and attach it.
  `mr lane status molab` reports, per account, warm/cold, minutes of session left, last probe error,
  and — critically — "0 warm lanes, start one", because a router whose backend pool is empty should
  say so rather than 502 quietly.
* **`priority` over `least-used`:** these lanes are the same hardware and the same model, so spending
  one session fully before starting the next is the cheapest operator behaviour.

## Non-goals

* **No pretending provisioning is automated.** The notebook start is attended; the docs, the CLI help
  and `mr lane status` all say so. Any code path that claims to start a molab session does not exist
  and must not be written.
* No credentials in notebook source (public notebooks), no token in a URL query parameter, no
  unauthenticated tunnel.
* No attempts to bypass the 12 h / 90 min limits or to obtain more compute than the default without
  asking marimo.

## Work breakdown

1. `crates/mr-lane/src/backends/molab.rs`: attachment model (`url`, token ref, expiry), probe
   supervisor, deregistration on failure/expiry, `provider_quota_state` row carrying probe status and
   session deadline.
2. `mr lane attach|down|status molab` in `crates/mr-cli`.
3. `deploy/molab/lane.py`: the notebook (model load, OpenAI-compatible server, `getpass` token,
   tunnel, printed attach command, heartbeat), plus a README that states session limits, the public
   notebook exposure, and the exact steps.
4. Docs: `docs/24-gpu-lanes.md` ("attended lanes" subsection), `docs/LLM_INDEX.md`,
   `docs/23-implementation-status.md` row: *attachment and failover implemented; provisioning is
   manual by platform design*.

## Testing

* Attachment lifecycle against `tools/mock-upstream`: attach → healthy → probe failures →
  deregistered; attach with a bad token → rejected, nothing written.
* Expiry: a lane with 1 second of session left is removed even while probes pass.
* Two accounts: kill A, assert the next request is served by B with no operator action and no
  client-visible failure beyond the relay's existing retry boundary.
* The notebook template is tested with a stub tunnel (a local process that spawns a server) so its
  printed attach command and heartbeat are exercised without molab.

## Acceptance criteria

* An operator can turn a running molab notebook into a relay-served inference provider in one command,
  with the token in the vault and the notebook free of secrets.
* A dead or expired session stops receiving traffic automatically, and traffic continues on another
  attached account.
* `mr lane status molab` tells the operator exactly what is warm, what has expired, and what to do
  next — and never implies that the tool can start a session for them.
* Docs state the 12 h cap, the 90 min idle shutdown, the 96 GB Blackwell GPU, and the public-notebook
  exposure of anything in the source.
