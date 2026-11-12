# Modal-hosted vLLM

`modal-vllm` is for a **single-tenant vLLM server running on Modal**, not Modal's hosted model API. The server must expose the OpenAI-compatible chat-completions endpoint, usually `https://<deployment>--<function>.modal.run/v1/chat/completions`.

## Auth: Modal Proxy Tokens

Modal's Proxy Token has two parts: Token ID and Token Secret. Store them as the provider account's secret in the documented combined form `wk-…ws-…` separated by one period:

```text
TOKEN_ID.TOKEN_SECRET
```

The relay sends these as separate `Modal-Key` and `Modal-Secret` headers. Never put either token in the provider URL or static headers. See [Modal's Proxy Token guide](https://modal.com/docs/guide/webhook-proxy-auth.md).

## Endpoint settings

The provider editor exposes these self-hosted settings:

| Field | Meaning |
|---|---|
| `max_context` | Context length to put on the compiled model listing and use as the per-request context limit, overriding `/v1/models`. For per-model differences, enter `context_length` on each model; that value takes precedence. |
| `kv_pool_tokens` | Total prompt-token reservation capacity across in-flight requests. A request that does not fit is rejected locally before it is sent to vLLM. |
| `max_concurrent` | Maximum in-flight requests for this provider/model endpoint. |
| `cold_start_ttfb_timeout_ms` | Optional first-byte timeout override for cold start and long prefill. A value such as `300000` allows a five-minute boot/prefill window. |
| `price_per_second` | Optional GPU-container price in USD per second. Successful requests record elapsed wall-clock GPU seconds and an estimated dollar amount in the usage ledger. |

KV reservations are process-local and are keyed by provider/model. They are released when the response body completes or is dropped. Configure capacity against the actual deployed pool and avoid hedging to the same endpoint: each parallel attempt needs its own reservation.

The GPU accounting is an estimate: vLLM does not report Modal's billable container lifetime for an individual request. Manarouter records request wall time for successfully started attempts, and applies `price_per_second`; it does not infer idle-container time or startup billed outside a request.

## Cold starts and fallback

Modal functions may scale to zero. **If** a `modal-vllm` step answers `503`, that response is classified as `cold-start` and receives the normal bounded exponential cooldown with any `Retry-After` honored, so the combo can fall through to a hosted provider; the relay does not retry a failed cold-start request against the same endpoint in a tight loop. That classification is implemented and covered by tests.

What Modal itself returns during a cold start is **not verified here**. Modal's documentation describes requests being queued while a container starts rather than refused with a 503, and this project has only measured the startup time (110–181 s for the operator's 27B deployment), not the response code a queued request receives. Both behaviours are therefore possible depending on the deployment's configuration, and this document does not claim which one an operator will see:

- If the deployment **holds the connection** while the container boots, the request succeeds but slowly, and the relevant setting is `cold_start_ttfb_timeout_ms` — a value such as `300000` gives a five-minute boot and prefill window. This is the path the operator's own measurements exercised.
- If something in front of the deployment **refuses** the request, it is a 503 and the cooldown path above applies.

Do not tune `cold_start_ttfb_timeout_ms` expecting it to suppress a 503; it cannot, because the connection is already closed by then. Determine which behaviour your own deployment exhibits before relying on either.

## Operator validation

1. Confirm the Modal deployment URL directly serves `POST /v1/chat/completions` and that its Token ID and Token Secret authenticate as a Proxy Token. Add a `modal-vllm` provider with the deployment's public HTTPS base URL, then add an account secret in the exact `TOKEN_ID.TOKEN_SECRET` form. Never pass the credential on a command line or in chat.
2. Sync models to discover the endpoint's model IDs, then verify a short chat request with a valid Manarouter client key. A first call can wake a scaled-to-zero container and incur GPU charges; obtain authorization before making it.
3. Check model metadata against vLLM's actual launch flags, especially `--max-model-len`; `/v1/models` may not describe context, vision, tools or reasoning accurately. Set model `context_length` for a per-model limit, or provider `max_context` as the default override. The model value takes precedence over the provider value and the values advertised by `/v1/models`.
4. Set `kv_pool_tokens` no higher than the real aggregate KV capacity available to requests, and account for the router's coarse prompt-token estimate (request-body bytes / 4), not the vLLM tokenizer. Try a request whose estimate is over the configured limit to confirm it is rejected locally; check concurrent requests respect `max_concurrent`. Reservations are process-local and keyed by provider/model, so multiple relay processes do not share one pool ledger.
5. If testing scale-to-zero, establish first which behaviour your deployment shows: a request that hangs and then succeeds after a cold boot, or a returned `503`. Only the second produces a `cold-start` cooldown and lets a later combo step run; the first consumes the full boot inside `cold_start_ttfb_timeout_ms`. Review the cooldown/`Retry-After` and fallback behavior in relay logs/response; do not expect an immediate same-endpoint retry.
6. If `price_per_second` is configured, inspect the usage ledger after a successful completed response. The recorded GPU seconds are request wall-clock elapsed time, not Modal's actual billable container lifetime; an aborted/failed attempt does not create a settled usage row. Confirm estimated cost is seconds multiplied by the configured USD rate.

## Model metadata and verification

vLLM's model list does not reliably describe operator-selected `--max-model-len`, vision, tools or reasoning. Sync models to discover names, then set metadata manually. A provider-level `max_context` is the default context override; individual model metadata wins for per-model limits.

A first request can spend GPU time and may incur Modal charges. For a long context configuration, check the endpoint's observed KV pool and set `kv_pool_tokens` conservatively; the router estimates prompt tokens from request-body size, not the vLLM tokenizer.