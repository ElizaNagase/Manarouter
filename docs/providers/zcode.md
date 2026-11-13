# Z Code (Z.ai plan)

Z Code is Z.ai's **Agentic Development Environment (ADE)** — the desktop app, not a CLI. It has no
`zcode` binary and no local API server: what it has is a *plan* endpoint that serves Anthropic
Messages, authenticated with an account token that the app obtains through a login it brokers
itself. This provider is that endpoint, reached the way the app reaches it.

- Catalogue: `catalog/providers.toml`, `id = "zcode"`
- Dialect: [`zcode`](../24-provider-dialects.md#zcode--anthropic-messages-behind-the-apps-plan-surface)
- Translator: `crates/mr-dialects/src/zcode.rs`
- OAuth platform: `crates/mr-core/src/oauth/zcode.rs`
- Your own site: <https://zcode.z.ai/en>

## What the app actually sends

Recorded from the app's own agent core (`resources/glm/zcode.cjs`) and confirmed against its
configured endpoints. A model request is:

| | |
|---|---|
| Method and URL | `POST https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages` |
| Body | Anthropic Messages, built by the AI SDK's Anthropic client |
| `anthropic-version` | `2023-06-01` |
| Credential | `Authorization: Bearer …` when it holds a token, else `x-api-key` |
| Content type | `application/json` |
| Accept | `text/event-stream` when streaming |

Plus a correlation block the app attaches to every model request:

| Header | Value |
|---|---|
| `x-request-id` | a fresh id, one per request |
| `x-zcode-trace-id` | the same id |
| `x-zcode-session-type` | `main`, `other`, `side_chat` or `subagent` |
| `x-session-id` | the conversation marker, with `sess_` / `subagent_agent_` prefixes stripped |

The app sends **no** `User-Agent` override, no app version and no `X-Client-*` family on model
requests — that family rides its configuration and signing requests, not its inference. Manarouter
sends this block and nothing more, because a header the app would never send is exactly the
fingerprint this dialect exists to avoid.

`x-query-id`, `x-zcode-query-source` and `x-zcode-session-features` are *not* sent: they describe
provenance inside the app (which query, which feature turned it on) that a router cannot honestly
claim.

## Signing is not implemented

**This is the gap, and it is deliberate.** The app signs its plan requests. The signing layer
(`get_sign_key`, a `X-Client-Ts` / `X-Client-Version` / `X-Client-Sig` / `X-Client-Nonce` /
`X-Session-Id` / `X-App-Id` / `X-Client-Pow` header set, with a proof-of-work token) exists in the
app's bundle and is *not* reproduced here. Reproducing it would mean shipping a credential-shaped
algorithm and a live proof-of-work loop in a router; more importantly it is the app's own
authentication to its own vendor, and imitating it is not the same exercise as speaking its
protocol.

The app itself does not require signing to succeed. Its signer is skipped entirely in three modes
(`start-plan`, `off-peak`, `highspeed`), it falls back to unsigned when the origin does not match
or when a previous attempt was refused, and its last resort is a plain `sendUnsigned`. What this
provider does is send the unsigned request — the same one the app falls back to.

**What that costs.** `UNCERTAIN:` the plan endpoint may refuse an unsigned `zhipu-account` token
outright. The token is sent as `x-api-key`, which is the header the AI-SDK's `apiKey` path uses and
which the account-key surface (`https://api.z.ai/api/anthropic`) definitely accepts. If the plan
endpoint insists on a signed request, the failure appears as a 401/403 from the provider, and the
honest fix for that account is the [`zai`](#when-to-use-zai-instead) row instead. Expect this
provider to work when it is given a token the plan endpoint accepts unsigned, and to fail visibly
rather than silently when it is not.

## Signing in

There is no redirect URI to register and no user code to type. The app's desktop login is a
**brokered CLI flow**, and this is the same flow:

1. Manarouter generates a client id (32 random bytes, hex) and posts `{"provider":"zai"}` to
   `https://zcode.z.ai/api/v1/oauth/cli/init` with that id as a bearer token. The reply carries a
   `flow_id`, an `authorize_url` and a poll interval.
2. The panel shows that `authorize_url`, extended with the desktop login redirect the app uses
   (`/app/oauth/login?redirect=zcode://oauth/callback&app_version=…`). You sign in at zcode.z.ai in
   a browser.
3. Manarouter polls `https://zcode.z.ai/api/v1/oauth/cli/poll/<flow_id>` with the *same* client id
   until the browser side finishes. A ready reply carries the account token.
4. The token is exchanged once at `https://api.z.ai/api/auth/z/login` for the business access token
   that the inference surfaces accept, and that is the credential stored for the account.

Refreshing is the same trade again, which is why the JWT is kept as the refresh token: the endpoint
returns no refresh grant, and a JWT that has not expired can be traded repeatedly.

`flow` for a BigModel-hosted account differs: `provider` names `bigmodel`, and the reply carries a
real `refresh_token` for its own token endpoint. Both shapes are handled.

An access token's lifetime is not in the reply. `UNCERTAIN:` a one-hour lifetime is assumed; too
short means a 401 and one refresh, too long means a harmless extra trade.

## Operator notes

- Add an account with `mr account add zcode --secret-stdin`, then sign in from the panel — or start
  from the panel's **Z Code** preset and let the sign-in button drive the flow.
- The credential is sent as `x-api-key`; `zcode` has no bearer-auth surface of its own.
- `messages` is the only route. There is no embeddings or audio endpoint, no `models` listing on the
  plan surface, and `count_tokens` is answered locally by the relay rather than proxied (the app
  never calls it).
- Reasoning maps onto the Anthropic `thinking` block, exactly as `anthropic` does: a reasoning level
  becomes `{"thinking":{"type":"enabled","budget_tokens":…}}`.

## When to use `zai` instead

If what you hold is an **API key** — a `zhipu-coding-plan` key, a project access key, anything from
the Z.ai console — you do not need this provider. Use [`zai`](catalog.md), which points at
`https://api.z.ai/api/anthropic/v1`, authenticates with `x-api-key` and is the plain public
Anthropic-compatible surface. `zcode` exists for the *plan account* path: the login, the token
exchange, and the app's endpoint. The two are different credentials against different surfaces and
neither substitutes for the other.
