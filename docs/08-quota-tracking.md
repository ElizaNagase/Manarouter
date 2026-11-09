# Quota tracking

Quota is the reason most multi-provider setups break. A request fails not because a model is down but
because one credential has hit a limit, and the router must know that before it picks it again.

## Four things that get conflated

Other routers tend to use "quota" for all four of these. They behave differently, they are observed
differently, and conflating them produces a router that either gives up too early or retries into a
wall.

| Kind | Example | Observable by Manarouter? |
|---|---|---|
| **Rate limit** | 500 requests/minute, 200k tokens/minute | **Yes**, from response headers and `retry-after`. |
| **Subscription usage window** | A plan's rolling 5-hour or weekly allowance | **Partly.** Only when the provider exposes it; otherwise it is declared by the operator. |
| **Spend budget** | $20/month on an API key | **No.** A budget is a billing concept; the provider does not tell the router about it. |
| **Credit balance** | Prepaid account with $3.14 left | **Only** through an optional provider-specific probe. |

The honest position is that Manarouter tracks what it can observe and does not pretend to track what
it cannot. A budget is something you set in a provider's billing console; the router learns about it
the moment the provider starts returning 402 or 429, and not before.

## Live signals first, declared windows second

A request against a provider is itself a measurement. Every response carries evidence about how much
quota is left, and discarding it in favour of a locally declared limit is throwing away knowledge in
exchange for a guess.

So the rule is:

> **Read the upstream's own signals. Fall back to the operator's declared window only where the
> provider has said nothing.**

If the two disagree, the **live signal wins**, because it is a measurement and the declared window is
a configuration. This is ratified decision Q4, and the `live_signals_win` flag in `QuotaConfig` exists
so the operator can turn it off deliberately — for a provider whose headers are known to be wrong —
rather than because the router guessed.

### What is read

| Signal | Where | Notes |
|---|---|---|
| `retry-after` | Any 429 or 503 | Both forms: a number of seconds, and an **HTTP date**. The date form is the one implementations get wrong, and a naive parse of it yields a multi-decade cooldown. |
| `x-ratelimit-limit-requests`, `-remaining-requests`, `-reset-requests` | OpenAI-compatible | `-reset-requests` is a duration, not an instant. |
| `x-ratelimit-limit-tokens`, `-remaining-tokens`, `-reset-tokens` | OpenAI-compatible | Absent on many providers. Its absence is normal, not an error. |
| `anthropic-ratelimit-requests-*`, `-tokens-*`, `-input-tokens-*`, `-output-tokens-*` | Anthropic | Separate input and output token budgets, which most providers do not have. |
| `reset_at` / `reset` inside a 429 body | Various | **Not standardised.** The field name and its unit differ per provider, so each is parsed per provider family rather than guessed at generically. |

### When nothing is said

The configured `default_cooldown_secs` (default **60**) applies, and the account is marked
`rate-limited`.

**That is a guess, not knowledge**, and the panel shows it differently from a cooldown whose reset
instant was reported by the provider. An operator looking at a 60-second cooldown that keeps renewing
can see that the provider is not telling the router anything and that the configured default is what
is driving it.

## Configuration

```json
{
  "quota": {
    "enabled": true,
    "live_signals_win": true,
    "default_cooldown_secs": 60,
    "auth_failure_cooldown_secs": 900,
    "failures_before_dead": 3
  }
}
```

Per-account windows are declared on the account:

```json
{
  "provider": "openai",
  "label": "work",
  "windows": [
    { "name": "rpm", "kind": "rolling", "seconds": 60, "limit_requests": 500 },
    { "name": "daily", "kind": "fixed", "seconds": 86400, "limit_requests": 10000 },
    { "name": "weekly", "kind": "provider-declared", "seconds": 0, "limit_tokens": 0 }
  ]
}
```

### The three window kinds

| Kind | Starts | Resets | Use when |
|---|---|---|---|
| `rolling` | At the first request in the window | `seconds` after it started | The provider describes a sliding window, e.g. "500 requests per minute". |
| `fixed` | On a wall-clock boundary | On the next boundary | The provider's limit is aligned to the clock — hourly, or daily at midnight UTC. |
| `provider-declared` | Learned from the upstream's headers | When the upstream says | The provider tells you, and you do not want to guess its schedule. With `seconds: 0` and no limits, this window exists purely to record what the provider reports. |

The distinction between `rolling` and `fixed` is not academic. With a 60-second rolling window, a
burst at the start of one minute and another burst 61 seconds later is legal. With a fixed
per-minute window, the same two bursts may land in the same minute and be rejected. A router that
assumes the wrong one will either under-use its quota or overrun it.

## Why quota is per account, not per provider

Two API keys on one provider have independent limits. Two OAuth accounts on one subscription may have
separate allowances, or may share one, depending on the provider. But the *consumption* is always
per credential: a rate limit is applied to the key that made the request.

So the tracked unit is the account. A provider's health is a **rollup** and is never a statement
about quota:

```text
ProviderHealth::from_counts(total, usable)
  total  == 0  ->  Unconfigured   (nothing is configured, which is not a failure)
  usable == 0  ->  Dead
  usable == total ->  Healthy
  otherwise    ->  Degraded
```

`Unconfigured` and `Dead` are deliberately different states. A provider the operator has not set up
yet, or whose accounts they have all disabled on purpose, is not broken — and the panel must not paint
it red, or operators learn to ignore red.

## What a refusal is recorded against: `cooldown_scope`

The *usage* is per credential, but the *limit* that refused a request is not always the credential's.
OpenAdapter's free plan counts Pro models apart from Lite ones and Antigravity publishes a quota per
model, so a 429 that means "this account has spent its Pro allowance" says nothing about that account's
Lite capacity. Recording it against the bare account throws the sibling models away with it.

Each provider therefore carries a `cooldown_scope`, and the key a refusal lands under follows it:

| `cooldown_scope` | Recorded against | Consulted for a request |
|---|---|---|
| `account` (default) | the account | the account |
| `model` | the account **and the model** | the account, then that model on the account |
| `provider` | **the provider and the model** | the account, then the model on every account of the provider |

A lookup consults the account-shaped key plus the one wider shape the scope adds, never a narrower
one, so a refusal recorded for one model cannot spread to its siblings. Two consequences of that rule
are worth stating outright:

- the reset instant for a route is the **latest** of the keys parking it — a route is free only when
  every fact that refused it has expired;
- `step_earliest_reset` and the last-resort pick take the **earliest** across accounts — the demoted
  step comes back as soon as *anything* can serve it.

Two facts are fixed rather than configurable, because they describe the credential instead of a limit:
`AuthFailed` is always recorded against the bare account (the credential is broken for every model),
and a malformed request records nothing at all.

The default is `account`, so an existing configuration behaves exactly as before. Changing the scope
does not migrate live entries — they keep the shape they were recorded under and expire on their own
within one cooldown window. `model` also multiplies the live entries by the number of refused models;
see [the entry count caveat](#the-entry-count-caveat).

## The cooldown lifecycle

```text
  request  ->  429 with retry-after: 47
                  |
                  v
       key = cooldown_key(cooldown_scope, ...)   (the account, account+model, or provider+model)
       key.state = RateLimited
       key.reset_at = now + 47s    (from the header: knowledge)
                  |
                  v
       every KeyStrategy skips the routes that key parks
       (priority moves on, round-robin skips it, least-used ignores it)
                  |
                  v
       the entry is left in place; each selection
       compares now against reset_at (`is_over(now)`)
                  |
                  v
       once now >= reset_at the key parks nothing
       a reset-aware combo promotes the step back to its authored position
```

Under the default `cooldown_scope` the key *is* the account, which is what the rest of this document
calls a cooldown. Read "the account" below as "the key that was refused" wherever a scoped key
applies.

There is no timer thread in either process: a cooldown is in-memory state in the relay, and "expired"
is computed lazily the next time a selection looks at the account. Nothing polls, and nothing wakes
up at `reset_at`.

Under `reset-aware-priority`, a step whose accounts are **all** cooling down is **demoted below every
healthy step, never removed** — so the operator's preferred model is the first thing tried again the
moment its quota returns. That is documented in
[Reset-aware priority](09-reset-aware-priority.md).

What a client sees during a cooldown: nothing different, provided another step is usable. The request
succeeds against a lower-priority step. If every step for every account is cooling down, the relay
returns a 429 **of its own** — distinguishable from a forwarded one, which
[the API reference](18-api-reference.md) describes.

## The entry count caveat

A cooldown entry disappears when its key next **succeeds**, or when a selection notices it has expired;
there is no pruning timer. That was invisible under the account-only default — one entry per refused
credential, a few dozen at most. A scoped key makes the count a function of the catalogue instead: one
entry per refused model under `model`, and under `provider` a single entry shared by every account
that serves it.

The entries are small — a key, an instant, a reason, a failure count — and a refused model is not
retried until its window passes, so the practical ceiling is "models actually refused in the last
window", not "models configured". That is still enough for a provider with hundreds of models and a
per-model quota to be worth a glance: `/control/activity` lists every parked shape, and a `model` key
shows up in composed form rather than as a bare account id.

## Why a 401 and a 429 are treated oppositely

| | 401 / 403 | 429 |
|---|---|---|
| Meaning | The credential is wrong | The credential is fine, it is being throttled |
| Will a retry help? | **No** | **Yes**, after the reset |
| Cooldown | `auth_failure_cooldown_secs` (**900**) | `retry-after`, or `default_cooldown_secs` (**60**) |
| State | `auth-failed` | `rate-limited` |
| Panel wording | "Log in again" / "Check the key" | "Cooling down until 14:32" |

A 900-second cooldown on a 401 is not a penalty; it is an acknowledgement that the operator has to do
something before a retry can possibly work. Retrying a bad key every 60 seconds produces logs that
hide the one line that matters, and a decade of experience says the operator will not notice.

## Failures before dead

`failures_before_dead` (default 3) is the number of consecutive failures — of any kind, after cooldown
handling — before an account is marked `Dead` rather than `Degraded`.

The reason this is a count and not a boolean: a single failure is almost always the network, the
provider, or a transient 5xx. Marking an account dead on one failure means a brief provider wobble
takes a perfectly good credential out of rotation. Three consecutive failures is a pattern.

The counter **resets on any success**. That is what makes it a measure of consecutive failure rather
than of lifetime unreliability, and it is why an account that fails twice a day but succeeds a
thousand times is never marked dead.

## Hedging costs quota

> **Status.** The race runs, so a hedge does spend quota today. The accounting below is the
> behaviour, not a proposal. One caveat the race adds: a loser is dropped when the winner is
> committed, and the tokens a loser had already produced are not accounted to the key — the key is
> charged for the winner's usage.

This is stated plainly because it is a real cost that is easy to forget when reading about the
latency benefit.

If a hedge sends a request to three participants and one wins, **the other two also consumed quota**.
For a non-streaming request, a loser may have been cancelled before its first byte — which is why
`cancel_losers` is on by default and why cancelling early matters — but a loser that had already
begun generating has spent tokens that were billed.

Two consequences:

- **Hedging trades quota for latency.** On a subscription with a tight five-hour window, a hedge that
  triples request volume will exhaust it three times faster. [Hedged race §4](07-hedged-race.md)
  discusses when that trade is worth making.
- **The spend is attributed to each account, not to the combo.** Usage records show the losing legs
  against their own accounts. A combo-level view would be tidier but would misattribute the cost, and
  the cost is attached to the credential that was charged.

## Credit balances

Some providers expose a balance endpoint. Where one is known, the intended design is for the conductor
to probe it **off the request path** on a long interval, with the panel showing the result — but no
balance probe exists in either service today, so a balance is currently always shown as unknown.

A balance that cannot be fetched is shown as **unknown**, never as zero. Displaying an unreadable
balance as zero would suggest an account is empty when it may be perfectly full, and would send an
operator hunting for a problem that does not exist.

## What is recorded per request

> **Status.** Nothing is recorded yet. `Store` has the `usage` table and `record_usage` API, and
> `/api/quota` can read it, but **no code in the relay or the conductor ever calls the writer** — the
> relay does not mention usage at all. In a real deployment the table stays empty, so `/api/quota`
> reports nothing regardless of traffic and no quota window is enforced from recorded usage. What does
> work today is the live-signal path: a `429`/`401` observed on the request path sets an in-memory
> cooldown (see `QuotaConfig`). The table below is the intended record.

| Field | Notes |
|---|---|
| Account and provider | Which credential was charged |
| Model | The model id as sent upstream, not the combo name |
| Prompt and completion tokens | **Optional.** A streamed response may report no usage at all. |
| Duration | Wall clock, end to end |
| Outcome | Success, upstream error, or router error |
| Hedge role | **Which participant won and which lost** |

Token counts are partial by nature. Many providers report no usage on a streamed response, and some
report it only on the final chunk. A usage figure that looks lower than reality usually means "the
provider did not tell us", not "the tokens were free", and the panel is meant to say which responses
lacked usage rather than silently summing what it has.

## Where the data lives

The home for usage and quota history is **`state.sqlite`**, never `config.json`: the
configuration is meant to be readable, diffable and version-controllable, and a file that changes on
every request is none of those. The relay records a `key_usage` row per request, and the schema also
carries `usage`, `account_health` and `provider_quota_state`; the conductor serves them through
`/api/usage` and `/api/quota`.

What is *not* there today is live cooldown state. The relay's cooldowns (from `429`/`401` signals and
`QuotaConfig`) are held in memory in the relay process and are never written to the database, so they
are lost when the relay restarts, and deleting `state.sqlite` does not clear them. **Back the file
up before deleting it:** it also holds the per-key usage ledger and the OAuth credential rows, and
neither can be reconstructed.

Retention is the operator's decision. There is no default pruning, because silently deleting an
operator's usage history is worse than an unpruned database, and the panel shows the file's size so
the decision can be made with information.

## How the panel paints a window

The Quota view answers one question — *how much is left, and when does it come back* — so it shows
the number in words, the bar, the reset instant and the colour together, and every one of those is
derived from the same snapshot.

- **Bars fill with what is REMAINING**, not what is spent. An empty bar means "nothing left", which
  is the thing an operator acts on; a full bar means "fresh window".
- **Colour is a second copy of the number, never the only copy.** Green above 50 % left, amber at
  50 % or less, red at 20 % or less. The thresholds live in one exported constant pair in
  `panel/format.js` and every chip, bar and card border asks it, so the same window cannot be green
  in one place and amber in another.
- **No measurement is grey, not red.** A provider that reports no fraction is `unknown`; painting it
  red would claim exhaustion the data does not support, and painting it green would claim headroom.
- **The worst window decides the account.** A card takes the colour of its most exhausted window,
  because an account with one dead 5 h window is an account to look at whatever its weekly window says.
- **The at-a-glance strip is a signpost, not a summary.** One line per account lists each window as a
  chip (name, percentage left, colour, reset countdown); clicking a chip scrolls to that account's
  card. It exists so the operator can find the tired account without reading every card.

Nothing here invents data: a window with no fraction, no reset time, or no fetch is shown as such. The
page also says how old the snapshot is, because a quota figure without an age is a claim about now
that nobody made.

## Limitations

Stated so they are not discovered later. Most of these describe the intended accounting; only what
the provider actually reports reaches the ledger:

- **Usage a provider does not report is not counted.** Streamed responses frequently omit it.
- **Requests that fail before reaching the provider are not counted against quota**, correctly — they
  did not consume any — but they do appear in the error rate.
- **Race spend is attributed per account**, not per combo, so a combo's total cost is not a single
  figure.
- **A dropped usage record is counted, not retried** (by design). The intended usage channel is bounded
  so a slow disk cannot stall a request, and when it is full the drop is recorded and surfaced. Losing
  a usage row is preferable to adding latency to a completion, and pretending the row exists is worse
  than both.
- **Spend budgets and credit balances are not enforced.** The router can tell you a provider is
  returning 402; it cannot predict it.

## See also

- [Reset-aware priority](09-reset-aware-priority.md) — demotion when a step has no usable account.
- [Key selection](05-key-selection.md) — how accounts are chosen and how unusable ones are skipped.
- [Configuration](03-configuration.md) — `QuotaConfig` and account windows in full.
- [Hedged race](07-hedged-race.md) — the quota cost of racing.
- [Web panel](15-web-panel.md) — how cooldowns and health are displayed.
- [Migration](17-migrating-from-omniroute.md) — what happens to the reference router's quota data.
