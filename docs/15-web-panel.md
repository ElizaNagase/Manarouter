# The web panel

The panel is a small local web interface for the operator: providers, accounts, combos, keys,
aliases, quota, and the documentation. It runs on the **control plane**, port 20901 by default,
which is a different process and a different port from the relay on 20900.

That separation is the one architectural rule the panel must never break.

## Why it is a separate process

A request path and an admin page have incompatible cost profiles. The request path must do a
predictable, tiny amount of work per request; an admin page runs queries, renders lists, and is
allowed to be slow because a human is waiting for it.

Putting them in one process means they share an event loop, and a shared event loop means the slow
thing eventually blocks the fast thing. This is not hypothetical: the reference router measured in
[Latency benchmarks §1](19-latency-benchmarks.md#1-the-baseline-that-motivated-the-project) took
5.2–9.2 seconds to serve a purely local `/v1/models` request. An admin route mounted on the same
router was part of why.

So the panel cannot slow down a completion. The worst a broken panel can do is fail to render, and
the worst an admin query can do is fail to return — neither touches a token of anyone's inference.

The two processes share state through files, not through IPC: the conductor writes
`config.json` and the compiled snapshot, and the relay reads the snapshot. See
[Routing snapshot](04-routing-snapshot.md).

## Views

| View | Shows | Can change |
|---|---|---|
| **Overview** | Control-plane uptime, relay reachability, snapshot digest and age, configuration counts, and whether a panel password is set | Nothing |
| **Providers** | Every provider, its dialect, base URL, auth, key strategy, cooldown scope, account count, and health | Add, edit, enable/disable, remove; add, enable/disable and remove accounts |
| **Models** | Discovered and hand-entered models per provider | Hide/show, delete, and **test** a model (one small live request; the answer or the upstream error is shown as a toast). The test runs on the relay's loopback probe (`POST /control/probe`), so it goes through the ordinary dispatch path — routing table, dialect translation, account selection, cooldowns and fallbacks — and works for every dialect, not only the OpenAI shape. The route refuses non-loopback callers because it skips the client-key check |
| **Combos** | Every combo with its steps **in priority order**, each step's optional pinned reasoning level, and a **Live health** panel | Create, edit, reorder steps, enable/disable, remove, pin a step's reasoning level, and **check** the steps: one tiny completion per enabled step through the relay probe, in priority order, so the panel shows which step answers right now and which one failed and why |
| **Keys** | Client keys by label and prefix | Create (plaintext shown once), edit the policy, rotate the secret in place (new plaintext shown once, old secret invalidated after commit), enable/disable, delete |
| **Aliases** | Alias to target mappings | Add, remove |
| **Quota** | Allowances first — a strip of per-account window chips, then one card per account with one row per window the provider actually reported — then the recent request ledger underneath. A provider that quantifies a single window gets a single row, never padding for the families it did not mention; an account with nothing quantified (never fetched, or no reported fraction) is left off the page rather than shown as an empty shell. An account whose fetch *failed* keeps its card and the error | Refresh an account's windows from the provider (key *and* OAuth accounts: the conductor mints the same short-lived token the relay would) |
| **Settings** | Listen ports, client-key access, upstream proxies, the relay connection pool (including a low-resource profile), quota display, pricing data, the panel password, and the raw configuration | Edit; **ports apply on next restart** |
| **Documentation** | The top-level files in `docs/`, rendered from markdown | Nothing |

The **Models** view distinguishes discovered from hand-entered metadata explicitly, because a sync
must never overwrite what a human typed. If the two ever disagreed and the panel did not say which
was which, the operator would have no way to know why their edit vanished.

The **Quota** view is built to be read in two passes. The strip at the top lists each account's
windows as chips — name, percentage *left*, colour, `Resets in:` countdown — and clicking a chip
scrolls to that account's card, so a tired account is found without reading every card. Each card
takes the colour of its most exhausted window and draws every window the provider reported as a row:
name, a bar filled with what is **remaining**, the percentage left and used in words, and the reset
countdown on the right. Green is above 50 % left, amber at 50 % or less, red at 20 % or less, and
grey when a window is unlimited. The colour is always a second copy of a number the row states in
words — it never carries the meaning alone, and a window that arrives without a fraction is not
drawn at all rather than painted red or green. The page does not print the snapshot's `source` URL
or its per-window `details`; both stay in `/api/quota` for diagnosis, because beside the percentage
they buried the number the page exists to show.

## The live combo block

Under the Combos item in the sidebar there is one small block that answers a single question: what is
a combo doing right now?

It is fed by `GET /api/activity`, which the control plane proxies to the relay's loopback
`GET /control/activity`. The relay is the only process that can answer: it makes the routing decision
and it holds the account cooldowns, and neither fact is written to disk. The block polls every 1.5
seconds through the panel's shared scheduler, which stops entirely while the tab is hidden.

It has four states, and it says which one it is in words rather than by colour alone:

| State | Shows |
|---|---|
| **live** | The combo and the `provider/model` the relay committed to, with the word *live* and a pulsing dot. A request stays *live* for as long as the client is still receiving it |
| **recent** | The newest decision and its age (`12s ago`), because a combo that ran an hour ago is still the answer to "what did it do" |
| **no combo activity yet** | The relay is reachable and has served no combo since it started |
| **control plane unreachable** | The panel could not ask. A different sentence from the one above on purpose: "nothing is running" and "I cannot see" are not the same fact, and the panel must not blur them |

The block is a fixed 52 px tall on purpose. It fills and empties as requests come and go, and every
navigation item below it has to stay exactly where it was; the longest route is clipped with the full
text in the element's `title`. It is not an `aria-live` region: it is repainted on every poll, and a
polite region rewritten every 1.5 seconds reads the same sentence over and over. The announcement
lives in a separate `sr-only` status span that changes only when the state or the route changes.

An entry is finished when the client-facing response body is dropped, so a stream that the client
abandons is finished promptly rather than waiting for a timer to expire. Restarting the relay forgets
the whole ring, which is the honest scope of a fact about in-flight work.

## The design language

The whole panel follows one rule that is more useful than any palette:

> **Greys carry all the structure. A single accent is used for emphasis and interaction.
> The accent is never used for destructive actions or errors.**

So borders, headings, tables, dividers, disabled controls and secondary text are all greys. The
accent appears on the focused control, the selected tab, and the primary action — the things a
person is meant to look at or click. Nothing else.

Semantic colours handle meaning, and they are a separate vocabulary from the accent:

| Colour | Hex | Means |
|---|---|---|
| Green | `#4cc38a` | Healthy, ready, enabled |
| Amber | `#d9a441` | Degraded, cooling down, action needed |
| Red | `#ef6a6a` | Broken, refused, destructive |

Two consequences of taking that seriously:

**The accent is never red.** A destructive button does not become "the important one" by being
painted the emphasis colour; it is red, and it is still secondary to the primary action. Painting a
"Delete" button the same colour as "Save" is how interfaces get misclicked, and no amount of
confirmation dialog makes it acceptable.

**A semantic colour is never decorative.** Green in this panel always means a healthy state and never
"this looks nice". The moment green is used for styling, the operator can no longer scan for green
and find health.

### The case that catches people out

A provider whose accounts are all deliberately disabled is **`Unconfigured`, not `Dead`**.

This is a real distinction in the code — `ProviderHealth::from_counts(total, usable)` returns
`Unconfigured` when `total == 0` — and it has to survive to the screen. An operator who disabled
three accounts on purpose while changing providers should not see a red failure indicator for a
decision they made. Red is reserved for things that are wrong, and this is not wrong; it is parked.

Getting this wrong is not cosmetic. An operator who learns to ignore red because it is usually
crying wolf will also ignore the red that matters.

## Ports, restart, and the amber banner

The panel can change the relay and conductor ports, and a port change is **persisted but not
applied**. It is labelled "next restart", an amber banner appears, and the banner stays until the
restart happens.

The banner is not dismissible at all. It stays until the restart happens, because the cost of
forgetting that a change is pending is a confusing failure later ("I set the port, why is it still on
the old one?").

A **Restart** button applies it. The restart is never automatic, because rebinding a listening socket
means the old one closes, and everything connected to it — including an in-flight completion —
is dropped. An operator expects a settings change to take effect; they do not expect it to kill a
request that is mid-answer. Making the restart explicit turns a surprise into a decision.

## Authentication

- Bound to `127.0.0.1` by default, which requires no password.
- The listener can be bound to a non-loopback address (for example `0.0.0.0` so both `localhost`
  and the tailnet reach it), but the source address of every request is checked before anything
  else: only loopback (`127.0.0.0/8`, `::1`) and the Tailscale/CGNAT range
  `100.64.0.0/10` (which covers the tailnet address `100.104.1.69`) are trusted. Everything else
  — any host on the LAN, anything routed from the internet — is refused with `403` before the
  origin check ever runs, so an external probe learns nothing about the panel.
- Binding `0.0.0.0` without a password is therefore not the same as exposing the panel to the
  internet, but a password is still the second line of defence: set it under Settings → Panel
  password. The one way past the password requirement is the explicit
  `control.allow_remote_without_password` opt-out.
- Passwords are stored as **Argon2id** hashes. Session lookup is constant-time, so a timing
  difference cannot be used to probe a token.
- Every mutating request must carry an acceptable `Origin`, which is checked before the handler runs.
  A cross-origin form post from a page the operator is merely visiting is the realistic attack on
  a loopback admin interface, and browsers will send it. "Acceptable" means loopback, or exactly the
  host the request was addressed to (its `Host` header): the panel is usable at whatever address it
  is bound to — a tailnet name or IP included — but a page on any other origin still cannot drive it.
  A same host on a *different* port is a different origin and is refused.
- Sessions last 12 hours. Logging out revokes that session; changing the password revokes all of
  them. A `401` from the panel's own API means exactly one thing — that session is over — and the
  panel answers it by returning to the sign-in screen. A *provider* that refuses a credential is a
  different failure, so the model test reports it with the provider's own status and wording (a
  provider `401` is surfaced as `502` by `POST /api/models/test`) and leaves you signed in: entering
  the panel password again cannot fix an expired provider grant, and signing you out would hide the
  verdict you were reading.

### Reaching the panel from another device

Binding the panel to a non-loopback address is what makes it reachable from a phone or laptop on the
same tailnet, and it is the only way: the panel and the relay are ordinary listeners, so a tailnet
address or an SSH tunnel works, while a reverse proxy on another host does not change the bind.

Set the addresses in `config.json` (`control.host`, `relay.host`) — Settings → Raw configuration
edits and validates it in place, or edit the file and restart both services. `mr config` is a stub,
so there is no command for this yet.

Two things follow from that, and they are deliberate:

- Only loopback and tailnet sources are trusted by source address, so the panel is reachable from a
  phone or laptop on the same tailnet without exposing it to anything else. Prefer a password in
  addition: set it under Settings → Panel password.
- The relay must be reachable from the *browser's* point of view, not the server's. The Overview page
  probes the relay at the address the panel itself was opened from (falling back to the configured
  `relay.host`), because probing `127.0.0.1` from a remote browser tests the remote device's loopback
  and reports a healthy relay as unreachable.

The full threat model is in [Security](21-security.md). Cookie-based authentication against model
providers is **never** supported, for any provider, and the panel does not offer it.

## Resource discipline

The panel polls for health and status. It **stops polling while the tab is hidden**, and resumes on
focus.

This is a small thing that says something about the project. A background tab left open for a week
making a request every few seconds is thousands of pointless requests — to a loopback socket, on the
same machine that is serving inference. It is exactly the kind of waste the router exists to remove,
and it would be strange for the router's own panel to be the thing doing it.

The same reasoning drives the rest of the panel: it holds no more history than it displays, it does
no work while no one is looking, and it starts no timers it cannot stop.

## Implementation constraints

The panel is **plain HTML, CSS and ES modules**, served as static files by the conductor. There is no
framework, no build step, and no CDN.

Each of those is a choice with a cost:

- **No framework** means the views are small hand-written modules. It also means there is no
  dependency tree to audit, and no supply-chain risk in a component that handles provider
  credentials.
- **No build step** means editing the panel is edit-and-reload. It also means the panel cannot use
  JSX, TypeScript, or single-file components — a real limitation, accepted because the panel is
  small enough not to need them.
- **No CDN** means the panel works with no internet access, which is the normal state of a machine
  running a local router. A panel that fails to load its own stylesheet because the network is down
  would be an obvious own goal.

The panel is served from the `panel/` directory on disk, located once at startup by searching the
working directory, its parents, and the executable's directory, so there is no build step, no
`node_modules`, and no version skew against the API it talks to. A missing `panel/` directory degrades to a placeholder page rather than a crash.

## See also

- [Security](21-security.md) — the threat model, sessions, and the origin check.
- [Configuration](03-configuration.md) — what the panel is actually editing.
- [Combos](06-combos.md) — the view an operator uses most.
- [Latency benchmarks](19-latency-benchmarks.md) — why the panel is not on the request path.
- [Troubleshooting](20-troubleshooting.md) — what to check when the panel will not load.
