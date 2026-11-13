# Saturn Token Factory — provider doc (bare minimum)

> **Note (2026-11-13).** This is a *provider* page: a Saturn Token Factory endpoint is an
> OpenAI-compatible upstream, which the router still treats like any other. The Saturn Cloud **lane**
> — its routes, its SSH-tunnel budget and its vault entry — was removed; only this provider path
> remains.

Manarouter can route to a **Saturn Cloud Token Factory inference endpoint**:
a managed, OpenAI-compatible endpoint created and started entirely over the
platform REST API. There is no tunnel, no generator image, and no supervisor
inside a notebook container — one hand-minted Saturn API token authorises both
the control plane and inference.

## Goal

Call `https://<endpoint>/v1/chat/completions` through Manarouter's OpenAI
compatible relay, with `Authorization: Bearer <saturn-token>` and `model` set
to the endpoint's name, after creating and starting that endpoint over REST.

## 5-step setup

1. **Mint the token (attended, once).** In the Saturn Cloud UI, go to
   **User Profile → Access Keys → New Token**. It is shown once and never
   retrievable again; copy it somewhere the terminal can read (never paste it
   here). No token-free bootstrap is documented — no CLI login, no OAuth device
   flow.

2. **Create the endpoint.** `POST /api/orgs/{org_id}/token-factory/inference-endpoints`
   with required `name` and a GPU `instance_size`. `org_id` comes from your
   Saturn org; the endpoint's name becomes the `model` value later.

3. **Start the endpoint.** `POST` the start lifecycle call for the endpoint
   (list/get/start/stop/delete all exist in the API index). It is now a public
   HTTPS OpenAI-compatible API at `https://<endpoint>/v1`.

4. **Add the provider in Manarouter.** Dialect `open-ai`, base URL
   `https://<endpoint>/v1`, auth kind `bearer`. Capabilities: chat completions
   and model listing (`GET /v1/models` lists accepted names).

5. **Add the account and verify.** `printf '%s' "$SATURN_TOKEN" | mr account add
   saturn --secret-stdin`, then `mr models sync saturn` and
   `mr provider test saturn` (one minimal real request — it may incur provider
   charges).

## Curl example

```bash
curl https://<endpoint>/v1/chat/completions \
  -H 'Authorization: Bearer <saturn-token>' \
  -H 'Content-Type: application/json' \
  -d '{"model":"<endpoint-name>","messages":[{"role":"user","content":"Hello"}]}'
```

## Two unresolved facts

1. **Authorization spelling conflicts.** Saturn's deployment docs send
   `Authorization: token {TOKEN}`; the versioned OpenAPI declares `bearerAuth`
   (`http bearer`) for the same API; Token Factory docs say `Authorization:
   Bearer <token>`. Same token, two spellings — probe a discovery call on each
   surface rather than trusting either document to be the whole truth.

2. **No price and no verified free tier.** No per-endpoint price is published
   anywhere, and the two live plan pages disagree on per-hour rates for the same
   machine. Free-plan quota (hours, GPU entitlement, credit card) is stale on
   the live site — it is per-plan data readable only via `GET /api/limits` with
   a real token. Do **not** claim `(Paid)` or free: neither is verified.

## Sources

- `https://saturncloud.io/docs/inference/` — Token Factory product, OpenAI-compatible claim
- `https://api.saturncloud.io/reference/inferenceendpoints_create.md` — endpoint creation
- `https://api.saturncloud.io/llms.txt` — API index (start/stop/list/get/delete)
- `https://saturncloud.io/docs/user-guide/how-to/access/tokens/` — Access Keys → New Token
