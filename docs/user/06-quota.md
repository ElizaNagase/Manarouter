# Quota and usage

Use this guide to distinguish configured provider/account quota windows, transient cooldowns, and the request usage that Manarouter records.

## What the panel shows today

The panel's **Declared windows** section reads the limits present in provider and account configuration. It can show a window kind and duration, request or token limits, and reset-only declarations. An account's own declared windows override its provider's windows when present; otherwise the provider's windows are the effective defaults. These are operator-entered settings, not a live reading of your provider's billing or remaining allowance.

The **Observed usage** section reads cached provider/account quota snapshots from `GET /api/quota`, including source, fetch time, windows, details, and per-account errors. Use the account refresh control or **Refresh all** to ask supported provider integrations to update their snapshot; OAuth accounts currently report refresh as unsupported. A configured provider/account window is only a declaration, not a measured allowance. The API returns `{ "accounts": [...] }` and preserves cached data alongside refresh errors.

### Which providers report a live allowance

A provider appears in **Observed usage** only when a reader exists for it and can prove it has an
allowance to read. These are the readers in this build:

| Reader | Recognised by |
|---|---|
| Antigravity, Claude, Codex, Command Code, Cursor, DeepSeek, GitHub, GLM, Grok, Kimi, MiniMax, Moonshot, OpenRouter, Qwen | the provider id, or the host its base URL points at |
| Modal | provider id `modal` and account-specific `modalQuotaEndpoint` metadata |
| OpenCode Go | the provider id `opencode-go` only — `/usage` exists on the Go host and 404s on the others |

Modal has no assumed vendor endpoint: configure `modalQuotaEndpoint` in that account's provider-specific metadata. It accepts an HTTPS URL string or an object such as `{ "url": "https://your-controlled-endpoint.example/modal-quota", "auth": "bearer" }`; `auth` may be `bearer` (default), `x-api-key`, or `none`. The endpoint returns JSON with numeric non-negative `spend` and `remaining` at the root or under `data`; optional `limit` (or `total`) supplies the window total, otherwise it is derived as `spend + remaining`. Optional `currency`, `display_name`, and `reset_at` (or `resetAt`) are carried into the quota snapshot. This is an operator-owned endpoint contract, not a claimed Modal API shape. Store any credential only in the account's normal secret field; never embed it in the URL or metadata.

Everything else falls to the generic OpenAI-compatible reader, which reads a live allowance in two
ways: from an endpoint the account declares with `quotaEndpoint`, or from the candidate list below.
An account with no reader, no declaration and no list to walk is left off the list rather than shown
as empty: a row of zeros would claim a measurement nobody made.

The candidate list (`quotaProbe`) is a configurable list of quota endpoints for exactly that
undeclared case — it walks `/v1/balance`, `/api/balance`, `/api/v1/key`, `/v1/key`, `/v1/user/info`,
`/v1/dashboard/billing/subscription`, `/v1/credits`, `/v1/quota`, `/v1/usage`, and finally a
one-token `POST /v1/chat/completions` read for its rate-limit or `x-balance-*` / `x-credit-*` headers,
stopping at the first candidate that resolves a quota. Each candidate is a path on that account's own
base URL and is requested with that account's credential, so a refresh spends a little of the
account's own rate limit; a `404` or `405` means "not served here" and the walk goes on. Edit the
list, the one-token `POST` and the model it names under **Settings → Quota probing** (the same values
live in the `quotaProbe` section of the configuration file, where an individual account may override
them). When a walk produces a reading, the account's card on the Quota page names the candidate that
answered. See [candidate quota endpoints](../08-quota-tracking.md#candidate-quota-endpoints) for the
rules and the reasoning behind each one.

A plan that meters several windows at once (a five-hour, a weekly and a monthly allowance, for
example) is reported as one row: the window closest to exhaustion is the row's own fraction, and
every window is listed underneath with its duration in the name.

This provider quota view is separate from request usage accounting. The **client-key** caps are a separate mechanism: the relay enforces the per-key rolling token and request caps on the request path and refuses a request that would exceed them with HTTP 429, before it contacts a provider. What the panel's provider view does *not* show is client spend: the relay writes the settled per-key usage rows into the store, but the panel's quota view reads the *provider* snapshots, not that ledger. Do not treat an empty request-usage history as zero provider usage.

## The token count of a request

`GET /api/usage` serves the per-request usage ledger the relay writes: one row per request, newest first, with the request's and the response's byte counts.

The token count is reported **either by the provider or by the router**:

- **By the provider** — the upstream sent a usage block. Those numbers are in `provider_prompt_tokens` and `provider_completion_tokens`, and `provider_reported` is `true`.
- **By the router** — the provider reported nothing, and the token count was estimated from the byte counts the relay measured, using its four-bytes-per-token rule. Such a row has `estimated: true`, so an estimate is never mistaken for a provider's count.

The estimate is computed in the panel process, not on the relay's request path: the relay only counts bytes as they stream past. A request with neither a provider count nor measured bytes keeps `null` numbers — unknown stays unknown and never becomes zero.

`?limit=` (default 50, maximum 500) and `?key=<client-key-id>` narrow the answer.
## Cooldowns are not usage totals

The relay does temporarily cool down an account after certain upstream failures. A recognized numeric `Retry-After` on a rate-limit response can determine the cooldown; otherwise the relay uses its built-in default cooldown. A provider can be set to cool down per model instead of per account (the `cooldown_scope` field, useful when the upstream meters each model separately); the default cools down the whole account. The relay may then try a different step in a combo. This operational fallback is not a usage ledger, a billing-window counter, or a guarantee that the account's provider quota has reset. For diagnosing an upstream 429, see [troubleshooting](10-troubleshooting.md); for combo routing choices, see [combos](05-combos.md).

A quota-window row in configuration describes declared boundaries and request/token limits. A `reset_only` row marks reset timing without a limit. In this build, those declarations are shown in the panel but do not themselves count requests, enforce limits, or demote a combo step. Manarouter is not polling the provider or calculating remaining allowance from your upstream account.

## Interpretations to avoid

- **An empty observed-usage list is not zero provider usage.** The provider-quota view reads cached provider snapshots, not your own traffic; use `GET /api/usage` for the requests your keys actually made.
- **An estimated token count is not a provider's count.** `estimated: true` means the router worked it out from byte counts. It is close for text and coarse for small or schema-heavy payloads.
- **`N/A` is not 0%.** In a remaining-allowance display, it means no remaining fraction was reported.
- **An unknown reset is not an expired reset.** Without a recognized reset instant, the panel cannot show a reliable countdown.
- **Configured limits are not verified upstream limits.** Check your provider's own account or billing dashboard for current consumption and reset details.

For safe credential handling and for supported CLI operations, see [security and credentials](11-security.md) and [the CLI reference](12-cli-reference.md). Do not paste provider credentials or client keys into commands shown in shared logs or conversations.
