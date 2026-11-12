# ISSUE 11: Deploy Kaggle Notebooks as a batch inference lane with per-account quota switching
# Deploy Kaggle Notebooks as a batch inference lane with per-account quota switching

Supersedes #5, which treated Kaggle as an "asynchronous job backend". That framing would have been a
new surface with its own scheduler. Kaggle should instead be a **lane** in the sense established by
#10: it is deployed for inference, it serves real requests, and it participates in the account pool
like any other provider — with the hard constraint that Kaggle offers no inbound endpoint, so its
lane is batch-shaped rather than socket-shaped.

## What Kaggle actually gives you (verified, with sources)

* **A run is capped at 12 hours** (9 hours on TPU). Interactive sessions are also killed after an
  idle period measured in minutes.
  ([docs](https://www.kaggle.com/docs/notebooks), [GPU tips](https://www.kaggle.com/docs/efficient-gpu-usage))
* **Free accelerators:** a single P100, 2×T4, and TPU v3-8; the CLI's accelerator names are
  `NvidiaTeslaT4` (default GPU), `NvidiaL4`, `TpuV5E8`, `TpuV6E8`.
* **GPU quota is 30 hours/week by default**, with a floating "trial" allowance granted when demand
  permits; the live number is on `https://www.kaggle.com/me/account`.
  ([quota discussion](https://www.kaggle.com/discussions/product-feedback/517366))
* **The CLI is the API:** `kaggle kernels push -p <DIR>` uploads *and runs* a kernel (`--no-run` to
  only upload, `--accelerator` to pick the machine), then `kaggle kernels status|logs|output|pull`
  poll it. `kernel-metadata.json` requires `id` (`owner/slug`), `id_no`, `title`, `code_file`,
  `language: python`, `kernel_type: script|notebook`, and accepts `enable_gpu`, `enable_internet`,
  `machine_shape`, `dataset_sources`, `kernel_sources`, `model_sources`.
  ([kaggle-api kernels docs](https://github.com/Kaggle/kaggle-api/blob/main/docs/kernels.md))
* **Credentials are per account:** a `kaggle.json` (username + key), located via `KAGGLE_CONFIG_DIR`.
  Account-scoped secrets are available inside a kernel through `kaggle_secrets.UserSecretsClient`.
* **No inbound networking is offered.** There is no documented way to expose a port from a running
  kernel; the only outputs are the kernel's files and its log.
* **The terms are the binding constraint, not the technology:** the Terms of Service say you "will
  only use the Services for your own internal, personal, non-commercial use, and not on behalf of or
  for the benefit of any third party", and the acceptable-use policy names cryptomining.
  ([terms](https://www.kaggle.com/terms))

The last two points decide the design. A tunnel out of a kernel would technically work, and it is
exactly what the terms forbid: turning a personal notebook allowance into a serving endpoint for
third parties. So **this lane is batch-only, on purpose**, and that restriction will be written into
the docs, the lane's config, and its error messages.

## Design

A Kaggle lane is a `LaneBackend` (see #10) whose `endpoint()` is an adapter on loopback, not Kaggle.
The adapter accepts an OpenAI chat request, turns it into one kernel run, and holds the HTTP
connection until the run's output file comes back.

```
client → relay → adapter 127.0.0.1:20911/v1/chat/completions
                    │ 1. render kernel source (prompt + params inline)
                    │ 2. kaggle kernels push -p <run-dir> --accelerator NvidiaTeslaT4
                    │ 3. poll `kaggle kernels status`
                    │ 4. kaggle kernels output -p <run-dir> → response.json
                    └ 5. return the completion, or 504 past the deadline
```

* **The prompt is the input channel.** The generated kernel embeds the request body as a literal
  (it is a fresh kernel version per call, which is what `kernels push` means), so there is no
  per-request dataset upload and no `dataset_sources` churn. The kernel loads the model
  (`vllm`/`transformers`) from `model_sources` or a Hugging Face cache, generates, and writes
  `/kaggle/working/response.json`.
* **Latency is stated, not hidden.** A cold lane is minutes: image pull + kernel start + model load +
  generation. The adapter therefore has a configurable `request_deadline` (default 15 min), returns
  `504` with the run URL when it is exceeded, and `mr lane status` prints the observed start-to-first
  token time so the operator can decide whether the lane is usable for their traffic at all.
* **One kernel per account, versioned per call.** Kernel ids are `owner/slug`, so distinct accounts
  hold distinct kernels; `kernel_sources` pulls a shared base kernel so the generation logic is not
  duplicated per account.
* **Quota-aware account pool.** Each account carries its own `kaggle.json` (decrypted from the vault
  into a `0600` temp dir, exported as `KAGGLE_CONFIG_DIR`, unlinked after the call — never written
  into the repo or `config.json`). Weekly GPU-hours consumed are written to `provider_quota_state`
  as `(provider_id="lane-kaggle", account_id, fetched_at, source="kaggle-cli", payload={used_h,
  cap_h, window})`, which is the same cache the panel already renders. At 90 % of the weekly cap the
  account is marked unusable by the adapter (not by the relay), so selection moves on before Kaggle
  refuses.

### Account switching, concretely

* **Per-request (relay):** N Kaggle accounts are N accounts on `lane-kaggle`; `key_strategy =
  round-robin` keeps each account's weekly allowance draining evenly, `least-used` prefers the
  account with the most headroom, `priority` is for the operator who wants one account spent first.
  Because the adapter returns Kaggle's own failures (`401` bad key, `429` quota) the relay's
  cooldown and reroute logic evicts and retries exactly as it does for a paid provider.
* **Per-run (adapter):** two concurrent requests must not fight over one account's weekly hours, so
  the adapter holds a semaphore of `max_concurrent: 1` per account by default — a notebook run is
  a whole GPU, and Kaggle will not run two of your kernels on the same accelerator anyway.
* **Pool arithmetic is visible:** `mr lane status kaggle` prints, per account, hours used this week,
  the cap, and the last error, because the operator's real question ("do I have budget left?") is
  not answerable from a status code.

## Non-goals (deliberate, and cited above)

* **No public endpoint, no tunnel, no "serve from a notebook".** This is a terms restriction and the
  code will refuse it: the adapter binds loopback only and has no bind-address option.
* No attempt to farm multiple accounts to exceed 30 h/week. Multiple accounts are supported for
  *legitimate* multi-account use (a lab, a team, separate identities), and the docs say the quota is
  per account and that evading it is not a feature.
* No cryptomining, no model-training lane, no GPU resale.

## Work breakdown

1. `crates/mr-lane/src/backends/kaggle.rs`: run-directory renderer, metadata writer, CLI invocation
   (process spawn with an isolated `KAGGLE_CONFIG_DIR`), status polling with backoff, output fetch.
2. Adapter: batch-to-chat translation, deadline handling, per-account semaphore, usage accounting.
3. `mr lane up kaggle --account <label> --model <hf-id> --accelerator NvidiaTeslaT4` (verifies the
   account's quota and that the kernel id is free), `mr lane status kaggle`, `mr lane down kaggle`.
4. `deploy/kaggle/`: base kernel template, `kernel-metadata.json` template, a runner `.py`, and a
   README that leads with the 12 h cap, the weekly quota and the non-commercial clause.
5. Docs: `docs/24-gpu-lanes.md` section, `docs/LLM_INDEX.md`, and a
   `docs/23-implementation-status.md` row that says "batch lane, no inbound endpoint, by design".

## Testing

* Fake `kaggle` binary on `PATH` in tests (a script that writes the expected output file and status
  transitions), so no test needs credentials or quota.
* Deadline: a fake kernel that never finishes must produce `504` and must not leave a run dir.
* Quota: a fake usage payload at 95 % must make the adapter refuse before spawning.
* Secret hygiene: `KAGGLE_CONFIG_DIR` is a fresh temp dir per call and is removed even on panic;
  a test asserts no `kaggle.json` survives and that no key material appears in `config.json` or logs.

## Acceptance criteria

* A single `mr lane up kaggle` + relay request returns a completion from a Kaggle kernel, with the
  prompt observable in the kernel version and the response read back from `kaggle kernels output`.
* Two accounts are pooled and rotated; an exhausted account is skipped before it is called, and the
  panel shows per-account weekly headroom.
* The lane cannot be configured to expose a port, and the docs explain why in Kaggle's own words.
