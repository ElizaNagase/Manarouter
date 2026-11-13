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

The template sets the dialect (`cursor`), the discovery origin (`https://api2.cursor.sh`), the auth
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
JSON file), using the id Cursor's own client sends. A request for an id the service does not serve
for your account comes back as an upstream error naming the model, not as a router fault.

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

- **Reasoning effort is not selectable.** The frame has no effort field, so the dialect offers only
  `none` and the service decides how much to think. Asking for a level is refused when the route is
  built, not silently ignored.
- **Every retry is a new conversation.** The router fingerprints a conversation per turn rather than
  per session, so a retry does not continue the previous exchange — and two callers who happen to
  open with the same words do not share one Cursor conversation and its memory.
- **The transport is reverse-engineered.** The endpoint, the discovery field numbers and the pinned
  client version come from the CLI's traffic. If Cursor changes them, requests fail; see
  [the implementation status](../23-implementation-status.md).

## Read next

- [The Cursor dialect](../24-provider-dialects.md) — framing, discovery, translation.
- [Cursor page login](../28-oauth-cursor.md) — the poll protocol, refresh, identity.
