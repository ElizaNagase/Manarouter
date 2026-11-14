# Cursor

Cursor's agent service, reached with a Cursor account instead of an API key. The router speaks the
same Connect-RPC stream the Cursor CLI speaks, so a model is whatever Cursor's agent serves to that
account — there is no published model list to sync.

## What it is, and what it is not

It is **not** an OpenAI-compatible endpoint, and it is not Cursor's cloud agents API. It is the
single-turn agent stream the editor and the CLI use: one prompt in, one answer streamed back, with
the account's own entitlements deciding which models exist and how much thinking happens.

## Before you start

- A Cursor account that the agent service will answer for. The router cannot tell you whether your
  plan covers the model you ask for; the service answers or refuses.
- The router running locally, with the panel at `http://127.0.0.1:20901`.
- Nothing else. No key to paste: the login happens in Cursor's own page.

## Add it

```bash
mr provider add cursor --from-catalog cursor
```

The template sets the dialect (`cursor`), the backend origin (`https://api2.cursor.sh`), the auth
kind (`oauth`, platform `cursor`), and the `chat` capability. Or use **Providers → Cursor** in the
panel, which opens the same template.

## Sign in

In the panel, open the provider and press **Connect**. A Cursor login page opens; sign in there. The
sign-in finishes in that page and the panel learns of it by polling, so nothing is ever pasted back
into the router. The grant is stored in the vault and the panel afterwards shows only an account
label and a fingerprint.

If the account's password or 2FA is yours to type, type it in the browser window — the router never
sees it. If the login page reports that the request expired, start again: the session has a
ten-minute life.

## Name the models you want

Nothing is synced, because Cursor publishes no model catalogue over this surface. Add the ids you
intend to call as provider models (panel: **Providers → Cursor → Models**, or `mr models` against a
JSON file), using the id Cursor's own client sends. `auto` is the one name the router translates: it is sent as
`default`, which is what Cursor calls automatic model selection. A request for an id the service
does not know comes back as a `404` naming it — the request is at fault, so no credential is parked
for it.

**The id is the whole name.** Cursor does not take an effort field, so the effort lives in the id:
`<base>[-<effort>][-fast]`, with `low`, `medium`, `high` as the efforts and `-fast` only on some
families. The desktop app's model picker is the source of truth — it is what Cursor itself offers —
and its titles map to ids by lowercasing and hyphenating them. Two families keep their dots
(`grok-4.7-high-fast`, `gpt-5.6-sol-medium`); the Claude ids spell the version with dashes
(`claude-opus-5-5-high`, `claude-sonnet-5-5-medium`, `claude-fable-5-2-high`). The verified set at
the time of writing is `auto`, `composer-2.5`, `composer-2.5-fast`,
`grok-4.7-{low,medium,high}`, `grok-4.7-high-fast`, `grok-4.6-{low,medium,high}`,
`gemini-4-carbon-{low,medium,high}`, `gemini-4-hydrogen-{low,medium,high}`,
`gpt-5.6-sol-{medium,high}`, `claude-opus-5-5-{medium,high}`, `claude-sonnet-5-5-{medium,high}` and
`claude-fable-5-2-{medium,high}`. A `-low` variant of those three Claude families and of
`gpt-5.6-sol` does not exist, and effort suffixes beyond the ones listed (`-thinking`, `-max`,
`-minimal`, `-extended`) are `404` too, so an id is proved by a request rather than by pattern.

**Plans.** Whether a model exists for you is the service's answer, not the router's. On Cursor's
Free plan, for example, `auto` answers while a premium id is refused with "Free includes Grok 4.7,
Grok 4.6, and Auto with fixed settings". The router passes that sentence through and parks **that
model only** for a while, so the models your plan does include keep working and another provider can
serve the turn.

Then call it like any other provider:

```bash
curl http://127.0.0.1:20900/v1/chat/completions \
  -H 'Authorization: Bearer YOUR_CLIENT_KEY' \
  -H 'Content-Type: application/json' \
  -d '{"model":"cursor/your-model-id","messages":[{"role":"user","content":"Hello"}]}'
```

## What the router refuses, and why

- **Client tools.** A Cursor turn may ask this client to execute a tool. That protocol is not
  implemented, so a request carrying tools is refused with an error naming the field rather than
  being sent and stalling mid-turn.
- **Images and other non-text content.** The frame this router builds carries text.
- **A proxy on the provider.** The stream is HTTP/2 bidirectional; a proxied one is not
  implemented, so the turn is refused instead of quietly going direct around your egress policy.
- **A credential that is not a bearer token.** The agent endpoint reads `Authorization: Bearer`.

## Limits worth knowing

- **Reasoning effort is a different model id, not a setting.** The frame has no effort field, so the
  dialect's own `reasoning` parameter stays `none`; the effort you get is the one baked into the id
  you asked for (`grok-4.7-medium` versus `grok-4.7-high`). Setting a reasoning level on the route
  cannot change it.
- **Images never reach Cursor.** A turn that carries an image part is rerouted by the relay's
  `routing.defaults.image` step before this dialect is reached; the answer, and the `model` it
  reports, then belong to that other route.
- **Every retry is a new conversation.** The router fingerprints a conversation per turn rather than
  per session, so a retry does not continue the previous exchange — and two callers who happen to
  open with the same words do not share one Cursor conversation and its memory.
- **The transport is reverse-engineered.** The endpoints, the frame and message shapes, and the
  pinned client version come from the CLI's own traffic. It has been exercised live against a Free
  plan account (a real streamed answer), but a system prompt delivered as a server-side blob, tools,
  images, and continuity across turns are still unproven. If Cursor changes any of it, requests fail;
  see [the implementation status](../23-implementation-status.md).

## Read next

- [The Cursor dialect](../24-provider-dialects.md) — framing, discovery, translation.
- [Cursor page login](../28-oauth-cursor.md) — the poll protocol, refresh, identity.
