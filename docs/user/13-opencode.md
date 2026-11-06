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
**New provider** form has two presets for it, **OpenCode Zen** (pay-as-you-go, and the free models
on the same host) and **OpenCode Go** (the subscription tier). Both are `open-ai` dialect with
bearer auth; add your OpenCode API key as an account secret after saving.

Those hosts also serve `/messages` and `/responses`, which a single-dialect provider cannot
express. If you need the Anthropic or Responses surface specifically, add a second provider on the
same host with the matching dialect.
