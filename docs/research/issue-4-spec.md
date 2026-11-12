## What I want

A provider class for **on-demand / batch inference servers**: endpoints that exist only while there is
work to do, are priced by wall-clock rather than by token, and are used to push a *batch* of requests
through a rented GPU - not to sit in an interactive combo as another always-on upstream.

So instead of 'a Modal URL that answers chat completions', the ask is:

1. a `modal_vllm` provider whose lifecycle the router controls (wake, use, drain, verify teardown),
2. a batch execution path on top of it - submit N requests, the router schedules them against the
   endpoint's real capacity, tracks GPU-seconds burned, and returns results in order,
3. cost accounting that answers 'what did this batch cost' in dollars, not in tokens.

Interactive use stays possible (same endpoint), but it is the secondary mode. Batch is the one that makes
a rented GPU worth it, and the one a normal OpenAI-compatible provider cannot express at all.

## Why batch is the right shape for this hardware

Measured on one 27B block-FP8 checkpoint (`orcarouter/Qwen3.8-27B-Uncensored-FP8`, rev `b32584cc`,
30.9 GB, 7 shards), vLLM 0.24.0, `--kv-cache-dtype fp8`, `--max-model-len 262144`, `ignore_eos`,
128 output tokens per request:

| | RTX PRO 6000 | L40S |
|---|---|---|
| price | $3.03/h | $1.95/h |
| VRAM | 95 GiB | 46 GiB |
| KV pool | **1,777,758 tok** | **281,729 tok** |
| cold start | 160-181 s | ~110 s |
| 64 streams, 2K prompts | **670 tok/s** | 261 tok/s |
| 32 streams, 8K prompts | 373 tok/s | 235 tok/s |
| 128 streams, 2K prompts | 613 tok/s (regression) | - |

Derived cost per million output tokens - the number a batch scheduler actually needs:

| workload | 6000 | L40S |
|---|---|---|
| 2K prompts, 64 streams | **$1.26 / M** | $2.08 / M |
| 8K prompts, 32 streams | $2.26 / M | $2.30 / M |

The more expensive card is **40% cheaper per token** on decode-bound batches, because throughput scales
faster than price. That inversion only shows up if the router measures throughput instead of assuming
price-per-hour is price-per-work - a good argument for the planner existing at all: picking the card from
the hourly rate alone gets this exactly backwards.

Two more facts that change batch economics:

- **Boot cost is a fixed fee per batch.** 180 s at $3.03/h = **$0.1516** before the first token on the
  6000 (110 s = $0.0596 on the L40S). A batch producing ~120K output tokens spends as much time booting
  as generating, so sub-100K-token batches mostly pay for CUDA graph capture. The scheduler should be able
  to say 'too small to be worth a cold start' - or hold it until it is.
- **Prefix caching is enormous and free.** Reused 200K-token prefixes collapsed TTFT from 87 s to
  1.9-4.1 s (~20x on prefill) via vLLM automatic prefix caching. Batches that share a system prompt or a
  document are far cheaper than their token count suggests; the API should encourage grouping by shared
  prefix rather than sharding blindly.

## What the batch path needs

**1. Submission and results, not just a proxy.** Accept a batch (JSONL or an array of prompt/max_tokens
pairs), return a job id, expose progress + per-item results + per-item errors. Follow the OpenAI
`/v1/batches` shape if possible so existing clients work; internally it is a queue, not a pass-through.
Requests must be individually retryable and results resumable - a batch that dies at 80% should not
restart from zero, and the endpoint container dying mid-batch is a normal event, not an exception.

**2. Capacity is KV tokens, not requests-per-minute.** The box holds 1,777,758 cache tokens: 64 concurrent
2K-prompt requests fit, 8 concurrent 200K ones do, mixing them means doing the arithmetic per admission.
Oversubscription is real and measured - 128 streams regressed throughput from 670 to 613 tok/s - so
'more concurrency' is not free, and the planner should cap at the measured knee (~64 streams for short
prompts) rather than at whatever the client asked for.

**3. Lifecycle control, including teardown verification.** Boot is 160-181 s (vLLM loading 30.9 GB from a
volume + CUDA graph capture); scale-to-zero via `scaledown_window` (Modal requires >= 2 s). The router
should wake on first batch, keep warm for the batch duration, drain, then confirm the container is gone.
That last step is not paranoia - see the `VLLM::EngineCore` pitfall below, where a leaked engine holds
88 GiB and every subsequent boot fails.

**4. Interactive requests must not starve behind batch work.** If one provider serves both, batch items
need a lower priority class and the ability to be preempted (they are retryable; a live chat turn is not).
Otherwise one 500-item batch turns the combo's latency into a coin flip.

**5. Cost in dollars.** Modal bills wall-clock per container per GPU-second. Track seconds-on-GPU per
batch and per provider, multiply by a declared `price_per_second`, report `$ / batch` and `$ / M output
tokens`. `docs/08-quota-tracking.md` currently assumes token windows; a self-hosted provider under that
model reads as free while it burns $3/h. A hard per-batch budget cap ('stop at $5') is the natural
control for this class of provider.

**6. Timeouts sized for cold starts and long prefills.** 300 s-class connect/read timeouts. A
260,117-token prefill took 87.2 s on the 6000 and 177.7 s on the L40S *after* the container was warm; a
503 during cold start is not a dead account and needs its own state with backoff.

**7. Model metadata declared in config.** vLLM's `/v1/models` gives the served name and `max_model_len`
but not vision/tool support, and the 262,144 context is a deployment flag. `max_context`,
`kv_pool_tokens`, `max_concurrent`, `price_per_second` belong in provider config and must override what
the endpoint claims.

## Reference deployment (what the router would be driving)

```
@app.function(gpu='RTX-PRO-6000', volumes={'/model': vol}, timeout=3600,
              scaledown_window=120, max_containers=1)
@modal.concurrent(max_inputs=64)
@modal.web_server(port=8000, startup_timeout=600)
def serve():
    subprocess.Popen(['vllm', 'serve', '/model/Qwen3.8-27B-Uncensored-FP8',
        '--served-model-name', 'qwen', '--language-model-only',
        '--kv-cache-dtype', 'fp8', '--gpu-memory-utilization', '0.92',
        '--max-model-len', '262144', '--max-num-seqs', '64',
        '--max-num-batched-tokens', '8192'])
```

Environment: `VLLM_USE_DEEP_GEMM=0` (SM120 crash workaround),
`PYTORCH_CUDA_ALLOC_CONF=expandable_segments:True`.

Modal's managed *dedicated endpoints* would be the cleaner primitive, but they are plan-gated on Starter
(H100 GPUs are not available on the Starter plan) and refuse a custom checkpoint unless the base repo is
already FP8. Hence `web_server` + our own volume - which is precisely why the router needs to understand
this provider class rather than treat it as a generic OpenAI-compatible URL.

## Pitfalls we already paid for (troubleshooting fodder)

- **`pkill -f vllm` does not kill vLLM.** The engine renames itself `VLLM::EngineCore`; the match is
  case-sensitive and misses it. The orphan keeps ~88 GiB resident and the next boot dies with
  `Free memory on device cuda:0 (7.47/95.01 GiB) ... less than desired GPU memory utilization`. Use
  `pkill -9 -i -f EngineCore` and poll `nvidia-smi` until the device is actually free.
- **`--quantization fp8` must not be passed.** The checkpoint is already block-FP8 and declares its own
  scheme with 882 `modules_to_not_convert`; forcing fp8 corrupts the dequant boundaries.
- **DeepGEMM crashes on SM120** (RTX PRO 6000) in vLLM 0.24.0 (`Unknown recipe` in `layout.hpp:76`).
  `VLLM_USE_DEEP_GEMM=0` falls back to CUTLASS block-scaled GEMM, which works.
- **`--max-num-batched-tokens 32768` returns HTTP 500 on the L40S** for a 32K prefill - the chunk does not
  fit beside 29 GB of weights in 46 GB. 8192 is the safe ceiling there.
- **MTP / speculative decoding is a 3-6x loss on SM120**: 63% acceptance, 26.9 vs 42.4 tok/s per stream,
  124 vs 373 tok/s aggregate at 32 streams. Do not enable by default.
- **`--async-scheduling` is neutral** (373.5 vs 373.2 tok/s) and costs KV headroom.
- **`ignore_eos` is mandatory for batch throughput measurements** - without it, short-stop responses
  report decode rates divided by an early-terminated stream. Three of our runs were invalid before this
  was caught.

## Acceptance criteria

- [ ] `modal_vllm` provider dialect: `Modal-Key`/`Modal-Secret` auth, streaming and non-streaming
      `/v1/chat/completions`, `finish_reason` preserved,
- [ ] batch submission (JSONL/array) -> job id -> progress -> per-item results and errors, resumable
      across a mid-batch container death,
- [ ] admission control on KV tokens (`kv_pool_tokens`, `max_concurrent`), honouring the measured
      concurrency knee instead of client-requested concurrency,
- [ ] lifecycle: wake on demand, keep warm during a batch, drain, verify teardown (0 containers),
- [ ] cold start is a state with backoff, not an account death; 300 s-class timeouts,
- [ ] GPU-second accounting with `price_per_second`, `$ / batch`, `$ / M tokens`, and a per-batch budget
      cap,
- [ ] batch items are lower priority than interactive requests and retryable if preempted,
- [ ] a 200K-token item survives end-to-end (measured: 87.2 s TTFT on the 6000),
- [ ] combo fallback when the endpoint is scaled to zero falls through to a hosted provider instead of
      hanging.

## Open questions

- Tool/function calling through this stack was never exercised - unknown, not unsupported.
- Outputs beyond 128 tokens were not benchmarked; long generations should improve decode-bound
  throughput, but the numbers above stop at 128.
- Warm pool (`min_containers=1`) during business hours vs pure scale-to-zero: $3.03/h idle is $73/day, so
  the default should be zero and the planner should batch aggressively to amortise the 180 s boot.

## Alternatives considered

- **Generic OpenAI-compatible provider.** Loses lifecycle control, KV admission and cost accounting -
  the three things that actually differ. A 90-second cold start inside a combo is worse than a 429.
- **Modal dedicated endpoints.** Cleaner primitive, plan-gated today with a custom FP8 checkpoint.
- **A bigger card to dodge the KV ceiling** (H100 $3.95/h, H200 $4.54/h): more expensive with *less*
  VRAM, so the 200K-session count does not improve. B200 ($6.25/h) buys FlashInfer GDN prefill for ~2x
  the price; not worth it for batch.

## Appendix: measurement method

Single-container A/B harness, vLLM booted repeatedly in one container with the GPU verified released
between boots; 128 output tokens with `ignore_eos`; aggregate = wall-clock over N concurrent identical
requests; decode = per-stream steady-state. Raw logs available on request.

