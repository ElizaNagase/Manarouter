# Using OpenCode with Manarouter

OpenCode is a coding agent. It is a **client** of Manarouter, not an upstream provider: you point
it at the relay and it calls whatever combos and models your configuration already routes.

OpenCode has no built-in entry for a self-hosted gateway, so it needs a custom provider block in
`opencode.jsonc`. That block is the whole integration, and three of its fields are the difference
between "it works" and "it answers with no reasoning and no tools".

## The config

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "providers": {
    "manarouter": {
      "name": "Manarouter",
      "env": ["MANAROUTER_API_KEY"],
      "package": "@opencode/ai/providers/openai-compatible",
      "settings": { "baseURL": "http://127.0.0.1:20900/v1" },
      "compatibility": {
        "reasoningField": "reasoning_content",
        "maxTokensField": "max_tokens"
      },
      "models": {
        "deepseek": {
          "name": "DeepSeek V4.1 Flash",
          "modelID": "commandcodego/deepseek/deepseek-v4.1-flash",
          "capabilities": { "tools": true, "input": ["text", "image"], "output": ["text"] },
          "limit": { "context": 200000, "output": 32000 }
        }
      }
    }
  }
}
```

Then export the **client** key (not a provider key) and run:

```bash
export MANAROUTER_API_KEY=mr-3f9a...
opencode run --model manarouter/deepseek "Refactor parseToken"
```

## Why each field is there

| Field | Why it matters |
|---|---|
| `providers` (plural) | The current key. Older builds used `provider` with an `npm` field; that shape is not read. |
| `package` | `@opencode/ai/providers/openai-compatible` speaks Chat Completions, which is what the relay's `/v1` surface is. Do not use `.../openai` or `.../openai/responses`: those send the Responses API, a different translation path. |
| `settings.baseURL` | Must include `/v1`, because the package appends `/chat/completions` verbatim. |
| `env` | The name of the environment variable holding the client key. OpenCode reads it; the key never goes in the file. |
| `compatibility.reasoningField` | Manarouter streams reasoning in `choices[].delta.reasoning_content`. Without this, OpenCode shows nothing while the model thinks. |
| `compatibility.maxTokensField` | Manarouter accepts both spellings, but `max_tokens` is what its dialects forward upstream. |
| `modelID` | The id actually sent to the relay. Use it to keep the OpenCode-side name short while the real id stays `provider/model`. |
| `capabilities` / `limit` | OpenCode assumes tools, text+image input, and 200k/32k for a model it does not know. Set them when you know better. |

## Claude models through Manarouter

If a route ends at an Anthropic upstream, add the cache marker to that model:

```jsonc
"claude-sonnet": {
  "modelID": "anthropic/claude-sonnet-5-5",
  "compatibility": { "cacheFormat": "anthropic" }
}
```

Manarouter translates Chat Completions into Messages, and OpenCode cannot tell from an
OpenAI-shaped endpoint that Anthropic cache breakpoints are wanted. Without this the prompt is
re-sent in full on every turn.

## Filling the `models` map

`GET /v1/models` on the relay lists every combo and every non-hidden `provider/model` id. It is a
local read, so it answers immediately and costs nothing:

```bash
curl -sS http://127.0.0.1:20900/v1/models \
  -H "Authorization: Bearer $MANAROUTER_API_KEY"
```

Copy the ids you want into `models`, using `modelID` for the real id and the map key as the short
name you will type.

## Gotchas

- **Model ids contain slashes.** `manarouter/commandcodego/deepseek/deepseek-v4.1-flash` resolves,
  because OpenCode splits the reference on the first `/`. The `modelID` alias above is just easier
  to type.
- **A 401 is the client key.** A 404 is the base URL — check the `/v1` before anything else.
- **Reasoning is not `content`.** If the model thinks but OpenCode shows nothing, the
  `reasoningField` line is missing or misspelled.

## OpenCode as an upstream provider

The reverse direction also exists: Manarouter can route *to* OpenCode's own gateway. The panel's
**New provider** form has two presets for it, **OpenCode Zen** (pay-as-you-go) and **OpenCode Go**
(the subscription tier). Both are `open-ai` dialect with bearer auth; add your OpenCode API key as
an account secret after saving.

Those hosts also serve `/messages` and `/responses`, which a single-dialect provider cannot
express. If you need the Anthropic or Responses surface specifically, add a second provider on the
same host with the matching dialect.

### The Go host, which is the one that works

Go is a subscription, and it is served from a different base URL than pay-as-you-go. It also
needs a header that no other provider here needs, so a plain `open-ai` provider will not do:

```bash
mr provider add opencode-go --dialect open-ai --base-url https://opencode.ai/zen/go/v1 \
  --auth bearer --header "x-opencode-session: 00000000-0000-4000-8000-000000000000"
mr account add opencode-go --secret-stdin
mr models sync opencode-go
```

Without `x-opencode-session` every request is a 400:

```json
{"type":"error","error":{"type":"MissingSessionID","message":"Request is missing x-opencode-session and cannot be routed efficiently."}}
```

OpenCode sends a stable id per conversation so the host can route and cache. A gateway has no
conversation to key on, so a constant id is the only option. It costs prompt-cache locality across
conversations, which is the trade the header exists to avoid — but it is better than a 400 on
every request.

### Only the models your plan lists

This is the trap. The Console catalogue is large; your Go plan is not. `GET /zen/go/v1/models`
with your key returns exactly what you are entitled to, and it is usually a short list. A model
that exists in the catalogue but not in that list answers:

```json
{"error":{"type":"permission_error","message":"Model access is disabled"}}
```

That is not a broken key and not a broken provider. Sync models from the Go host and use what
comes back:

```bash
curl -sS https://opencode.ai/zen/go/v1/models -H "Authorization: Bearer $OPENCODE_API_KEY"
```

### The other three refusals

**Free models on the pay-as-you-go host are refused.** OpenCode's docs are explicit: free-tier
chat models are only available from inside the OpenCode client, and other clients get HTTP 403
even with a valid key. `big-pickle` and friends answer `429 FreeUsageLimitError` ("Free models
work in OpenCode"). The same model on the *Go* host works, because there it is part of the
subscription rather than the free tier.

**A paid model on an unfunded account is a 402.** `{"error":{"type":"invalid_request_error",
"message":"Insufficient account funds"}}` means the key is fine and the balance is zero. Top up
in the Console; nothing on this side changes it. The relay parks the account on that response, so
the next request reports `no_usable_accounts` with a cooldown instead of the 402 — that is the
cooldown working, not a second fault.

**Some models are region-locked or need an opt-in.** A Go model hosted outside the EU and US
answers `400` with a link to your organisation's privacy settings; select **Global** there. A
model that trains on request data answers `400` until you enable *Allow paid endpoints that train
on request data* on the same page. Both are Console settings, not relay settings.

### Which base URL

`https://opencode.ai/zen/v1` and `https://opencode.ai/inference/openai/v1` are the same
pay-as-you-go surface; the presets use the first because the provider catalogue does. The Console
docs write the second. Go is `https://opencode.ai/zen/go/v1`, or
`https://opencode.ai/inference/go/openai/v1` — again the same host, two spellings.

### The Go allowance in the panel

The Go plan reports its own windows, and Manarouter reads them. `GET /zen/go/v1/usage` with your
key answers three windows — `rolling` (five hours), `weekly` and `monthly` — each with a `percent`
*spent* and its own `resetsAt`:

```bash
curl -sS https://opencode.ai/zen/go/v1/usage -H "Authorization: Bearer $OPENCODE_API_KEY"
```

Nothing else is needed: the same key works, and `x-opencode-session` is not required for this call.
Once a provider is registered under the id `opencode-go`, its account appears in the panel's
**Observed usage** list after **Refresh all** (or the account's own refresh control). Manarouter
reports the window you are closest to exhausting and lists all three underneath it, so a nearly
spent monthly allowance is visible even when the rolling window is still full.

Two limits worth knowing. The reader matches the provider id `opencode-go` and does not claim the
host, because `/usage` is a Go endpoint: the pay-as-you-go and free hosts answer it with 404, so a
host-wide match would report a working key as broken. And the metered Zen tier's spending is
tracked as a *budget* in the Console, which has no public endpoint — do not expect it here.
