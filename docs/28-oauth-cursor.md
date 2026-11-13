# OAuth: Cursor

Cursor has no loopback redirect and no device code. Its sign-in page lives on `cursor.com`, and the
CLI that already shipped for it never receives a callback at all: it sends the operator to that page
with a PKCE challenge and a UUID, then polls an endpoint until the page says the login is done. This
is the **page login** in `docs/10-oauth-overview.md` §2 — the third flow shape, and the reason the
`OAuthPlatform` trait grew `begins_with_screen_login()`.

Everything below is reverse-engineered from the CLI's own traffic, so reverse-engineered facts carry
`UNCERTAIN` in the code. Nothing here is documented by Cursor.

---

## 1. The login

### Step 1: open the page

```
GET https://cursor.com/loginDeepControl
      ?challenge=<base64url(sha256(verifier))>
      &uuid=<uuid>
      &mode=login
      &redirectTarget=cli
```

`verifier` is 32 random bytes, base64url without padding; `challenge` is its SHA-256 in the same
encoding. The PKCE pair is generated per attempt and kept only in memory, in the pending session.

The page completes the sign-in itself. There is no redirect back to this machine and no code for the
operator to copy, which is why `validates_redirect_port()` is `false` and `redirect_ports()` is inert:
those hooks exist for the loopback flow and Cursor never uses them.

### Step 2: poll until the page reports completion

```
GET https://api2.cursor.sh/auth/poll?uuid=<uuid>&verifier=<verifier>
```

No `Authorization` header: the verifier is the credential. The status codes are the whole protocol:

| Response | Meaning | What the CLI does |
|---|---|---|
| `404` | the page has not finished yet | keep polling |
| `2xx` with `accessToken` + `refreshToken` | the login is done | store both, stop |
| anything else | a real failure | report it and stop |

Both tokens are required. A `2xx` carrying only one of them is a protocol error, not a half-login:
the access token is useless without the refresh token, and storing it would create an account that
dies within the hour.

### Step 3: the polling cadence

One second, growing by a fifth each attempt, capped at ten seconds (1.0 s, 1.2 s, 1.44 s, … 10 s).
The page is valid for fifteen minutes; the session expires after ten, which is deliberate — the panel
must give up before the page does, so an operator who returns to a stale tab is told to start again
rather than shown a spinner that will never resolve.

Because the cadence and the ceiling live in `crates/mr-core/src/oauth/session.rs`, the platform does
not own them. `Cursor::screen_login_poll` answers one question — *has this page finished?* — and the
session machinery decides when to ask again.

---

## 2. Refresh

```
POST https://api2.cursor.sh/auth/exchange_user_api_key
Authorization: Bearer <refreshToken>
Content-Type: application/json

{}
```

The body is a literal empty object; the refresh token is the bearer credential, not a body field.

* 15 s timeout per attempt, three attempts.
* Retried on `429`, `500`, `502`, `503`, `504` only, with `300 ms · 2^(n-1)` and ±20 % jitter.
* `401` and `403` are unrecoverable: the refresh token is dead and the account needs a new login.
* A response that omits `refreshToken` **keeps the stored one**. Cursor does not always rotate it,
  and treating a missing field as "no refresh token" logs the account out on the next refresh.

### The refresh window

300 s before expiry (`Cursor::refresh_lead`). The observed token lifetime is an hour, and the window
leaves room for one full retry cycle — three attempts of 15 s each plus backoff fit inside it, so a
transient failure during the pre-expiry refresh still lands before the token dies.

Expiry is read from the access token's own JWT, `exp * 1000 − 5 minutes`, when the payload carries a
numeric `exp`. Otherwise — Cursor does not guarantee the claim — the stored credential gets an hour,
which is the observed lifetime. Guessing short would refresh too often; guessing long would send
expired tokens upstream. The fallback is the observed number, and it is marked `UNCERTAIN`.

---

## 3. Identity

`account_identity` reads the access token's JWT payload: `sub` is the account id and `email` is the
account's label, lowercased. Neither is trusted for authorisation — they only decide which stored
account a credential belongs to, so two logins of the same account share one entry and refresh once.

---

## 4. Why the panel polls instead of waiting

The sign-in happens in a browser tab the router does not control, and the only completion signal is
the provider's own poll endpoint. So the panel cannot block on a callback the way the loopback flow
does; it starts a session, opens the tab, and polls the *conductor*, which polls Cursor. Two hops, no
shared state, and a session id is the only thing the browser and the panel need to agree on.

The conductor's poll answer is one of three things, and the HTTP status says which:

| Status | Body | Meaning |
|---|---|---|
| `200` | `{"status":"pending"}` | the page has not finished; poll again after `retry_after_seconds` |
| `200` | `{"status":"complete", …}` | the tokens were exchanged; the account is stored |
| `400` | `oauth_session_mismatch` | the session belongs to another provider or platform |
| `409` | `oauth_session_busy` | a poll for this session is already in flight; try again |
| `404` | `not_found` | no such session, or it expired — final |

The panel ends the flow on any 4xx except `409` and `429`, so a session that the conductor has already
forgotten stops the loop instead of being polled until the panel's own deadline.

---

## See also

- [OAuth overview](10-oauth-overview.md) — the three flow shapes and the shared machinery.
- [Provider dialects](24-provider-dialects.md) — the `cursor` dialect and its frame stream.
- [Adding a platform](14-oauth-adding-a-platform.md) — the worked example this one followed.
