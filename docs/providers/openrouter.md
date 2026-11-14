# OpenRouter

OpenRouter is a router in front of many providers. The catalog entry (`catalog/providers.toml`,
`openrouter`) points at `https://openrouter.ai/api/v1` with the `open-ai` dialect and a bearer API
key, so from this gateway's point of view it is an ordinary OpenAI-shaped provider whose model ids
happen to contain a vendor prefix: `openrouter/anthropic/claude-sonnet-4.5` is provider `openrouter`,
model `anthropic/claude-sonnet-4.5`. The name is split on the first `/` only, so the upstream id
keeps its own slash.

What makes it worth a page of its own is that OpenRouter accepts *routing* fields in the request
body. Those fields are the client's only control over which upstream serves the request, and this
gateway has to carry them across the translation instead of deciding they are not its business.

## What happens to a request on the way through

There are two client surfaces, and they treat the body differently.

**The OpenAI surface (`/v1/chat/completions`) forwards the body as written.** No translation happens
between two OpenAI-shaped dialects, so `models`, `provider` and `plugins` reach OpenRouter exactly as
the client wrote them. Two things can still touch the body, and both are surgical: the model is
retargeted when the local name carries a reasoning-level suffix that the upstream model id does not,
and a reasoning fragment is merged in when the model has one. Both work by parsing the body, merging
that one fragment and re-serialising it; every other field the client sent survives untouched, which
is the property the test `the_relay_fits_a_thinking_budget_under_the_requests_own_max_tokens` pins.

**The Anthropic surface (`/v1/messages`) is translated**, so routing fields used to be dropped with
the rest of the fields the translation did not know about. `carry_routing` in
`crates/mr-dialects/src/openai.rs` now carries the three that matter:

| Anthropic-surface field | What reaches the upstream |
| --- | --- |
| `provider` | copied verbatim |
| `plugins` | copied verbatim |
| `models` | copied verbatim — it is already the Chat Completions field |
| `fallbacks` | rewritten into `models`, led by the model the request resolved to |

`fallbacks` exists only on OpenRouter's Anthropic-shaped endpoint, where each entry may carry a
`model` and nothing else. Forwarding the field itself would be an unrecognised argument on a Chat
Completions upstream, so a request with `fallbacks: [{"model": "a/b"}, {"model": "c/d"}]` for local
model `m` becomes `models: ["m", "a/b", "c/d"]`. A client that sends `models` *and* `fallbacks` keeps
the `models` it wrote: the upstream refuses the pair, and inventing an order for them would be
guessing. A request that mentions no routing at all gains no routing fields.

## The routing fields, and where they act

Every one of these is OpenRouter's own language. This gateway forwards them and does not interpret
them; the values, defaults and behaviour below are OpenRouter's
([provider selection](https://openrouter.ai/docs/guides/routing/provider-selection.md),
[model fallbacks](https://openrouter.ai/docs/guides/routing/model-fallbacks.md),
[auto router](https://openrouter.ai/docs/guides/routing/routers/auto-router.md),
[model variants](https://openrouter.ai/docs/guides/routing/model-variants/overview.md)).

### `models` — the fallback array

An ordered array of model ids. When the first model fails — context-length errors, a moderation
refusal, a 429, an outage — OpenRouter tries the next, and the response's `model` says which one
answered. Absent by default, and without it a request has no failover at all.

### `provider` — provider selection

| Field | Type | Default | Effect |
| --- | --- | --- | --- |
| `order` | `string[]` | — | Try these providers first, in order |
| `only` / `ignore` | `string[]` | — | Allow-list / deny-list of providers |
| `allow_fallbacks` | `bool` | `true` | Whether other providers may serve the request |
| `sort` | `"price" \| "throughput" \| "latency"`, or `{by, partition}` | — | Bypass default routing and order providers by that attribute; `partition: "none"` sorts across the fallback models rather than within each |
| `require_parameters` | `bool` | `false` | Only providers that support every parameter in the request |
| `data_collection` | `"allow" \| "deny"` | `"allow"` | Exclude providers that may store data |
| `zdr` | `bool` | — | Zero-data-retention endpoints only |
| `enforce_distillable_text` | `bool` | — | Only models whose author allows text distillation |
| `quantizations` | `string[]` | — | Filter by endpoint quantization (`int4`, `fp8`, `bf16`, …) |
| `max_price` | `{prompt, completion, request?, image?}` | — | Hard price ceiling; unlike the preferences below, it *blocks* the request rather than reordering |
| `preferred_min_throughput` / `preferred_max_latency` | number or `{p50,p75,p90,p99}` | — | Deprioritise endpoints missing the threshold; never excludes them |

### `plugins` — router and compression settings

`plugins` is an array of objects with an `id`. The two that shape routing:

- `{"id": "auto-router", "allowed_models": ["anthropic/*"], "excluded_models": […], "cost_tier": "low"}` —
  settings for `model: "openrouter/auto"`, which picks a model and effort per request. Patterns are
  wildcards over model ids.
- `{"id": "context-compression"}` — drop or truncate middle messages to fit the context window;
  endpoints at 8k context or below do it by default and `"enabled": false` turns it off.

### Model-variant suffixes

A colon suffix on the model id: `:free` and `:batch` are catalog variants with their own entries,
`:nitro` (throughput), `:floor` (price) and `:exacto` (tool-calling quality) are routing variants.
Routing variants are *not* entries in `GET /api/v1/models`, which matters here: this gateway only
routes to model ids that are in the provider's synced list, so `openrouter/openai/gpt-5.2:nitro` is
answered with `model_not_found` until that exact id is added to the provider. The same routing is
available through `provider.sort` on a synced id, which needs no extra model row.

## What this gateway does not do

It does not interpret any of the fields above. Choosing a provider by live price, throughput or
data policy happens at OpenRouter, using data this gateway does not have; reproducing it locally is
not a matter of forwarding a field, and a local `provider.only` filter would be a different feature
with different failure modes. What the gateway does keep is its own account health: an upstream error
from OpenRouter counts against the account and trips local failover exactly as any other provider's
error does, and the local combo and cooldown machinery is still the thing that runs when a whole
OpenRouter account is unusable.

Two names that look like part of this language are not: there is no top-level `route` request field,
and there is no top-level `transforms` field — message transformation is the `context-compression`
plugin above. Sending either will be ignored by OpenRouter, so do not write them.

## Reproducing fallbacks with local combos

A local *combo* also tries steps in order and stops at the first answer, and it can mix providers,
models and accounts rather than only OpenRouter models. The differences are worth knowing before
choosing: a combo's ordering is the operator's (with optional reset-aware priority), its health is
this gateway's cooldowns, and it can route to a provider that is not OpenRouter at all. Use `models`
when the fallbacks are all OpenRouter models and their live availability should decide, and a combo
when the fallbacks are a policy the operator wants written down and observable in the panel.

## Operator notes

- The API key is an ordinary account secret; add it with `mr account add openrouter --secret-stdin`.
- Sync models before addressing one: the routing fields are carried, but the primary model id still
  has to be one the provider lists.
- `:free` models have their own rate limits, which arrive as upstream 429s and therefore look like
  any other rate limit to this gateway's account health.
