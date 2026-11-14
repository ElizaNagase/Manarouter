# Overnight report — 14 November 2026

Written at 03:00 local (02:00 UTC). Everything below is either verified against the running instance
or marked as unverified. Twenty-eight commits are on `main` locally and **not pushed** (see
"Blocked").

## What is running, right now

- `manarouter-relay` on `127.0.0.1:20900`, `manarouter-conductor` on `:20901`, panel-lab on `:20902`,
  the DSH GUI on `:3080`. The relay was rebuilt from the current tree and restarted at 02:47; health
  and a real `cursor/auto` turn were re-checked afterwards (answer `"ok"`, `usage` present).
- `/v1/models` reports 23 056 models, of which **24** are `cursor/*`.

## Cursor, end to end

The provider works. A live call answers, streams, and reports usage:
`POST /v1/chat/completions` with `cursor/auto` returned `200` with an assistant message, reasoning
content, and token counts.

The model list was rebuilt from the **desktop picker** (the source of truth), not from guesswork:
ten rows, each mapped to the wire slug it implies, which is what produced
`gemini-4-carbon-{low,medium,high}`, `gemini-4-hydrogen-{low,medium,high}`,
`grok-4.7-{low,medium,high}`, `grok-4.7-high-fast`, `composer-2.5`(-fast),
`claude-{fable-5-2,opus-5-5,sonnet-5-5}-{high,medium}`, `gpt-5.6-sol-{high,medium}`, and `auto`.
Forty-nine stale ids were deleted. Effort is carried in the id, not in a request field — the frame has
no effort member — and that is now documented rather than implied
([`providers/cursor.md`](providers/cursor.md)).

**Free plan behaves as you asked.** A model the plan does not include comes back as a `429` with
`type: model_unavailable` and the server's own sentence ("Free includes Grok 4.7, Grok 4.6, and Auto
with fixed settings"), which parks *that model*, not the credential, and fails over. Nothing is
hidden from the model list: you pick, and a refusal fails the account over. `auto` and
`grok-4.7-medium` both answer.

Two rows remain unresolved and are documented as such rather than papered over:

- **Muse Wonder High** — every wire spelling tried returns `404 model_not_found`, so there is no slug
  to send. It stays out of the catalogue until a live probe succeeds.
- **Gemini 4 Hydrogen Extra High** — the picker row exists but no slug of that shape is accepted;
  `gemini-4-hydrogen-high` is the closest id that resolves, and only on a plan that includes it.

## The refusal fix, and the one case it does not cover

While probing `cursor/gemini-4-hydrogen-high` once, the relay answered `200` with
`{"error":{"message":"Error","type":"invalid_request_error"}}` — an error object under a success
status. The cause: the transport holds the response head so a refusal can *become* the response and
the account can be failed over, but that gate only opened when the dialect could extract a sentence
from the refusal. This shape has no sentence, so it fell through as an ordinary in-band error and the
head went out as `200`.

`mr_dialects::cursor::refusal` now writes the message itself when the server wrote none
("the server refused the turn without saying why"), the placeholder is never repeated as a cause, and
an unknown code keeps `502`. Two tests cover it — one in the dialect, one in the transport asserting
the response is a `502` and not a stream.

**The limit, stated plainly:** the hold is five seconds (`FIRST_ANSWER_GRACE`). A refusal that
arrives *later* still reaches a streaming caller as an SSE error chunk with `finish_reason: "error"`,
and a non-streaming caller as `200` with an error body, because the head is already spent. That is
recorded in [`24-provider-dialects.md`](24-provider-dialects.md) rather than fixed by lengthening the
hold, which would delay the first byte of every slow reasoning turn to buy correctness for a rare
case. If you would rather have the delay, it is a one-line change.

## Batch lane

Two things landed for issue #4's remaining asks:

- **A spending cap**, checked *before* dispatch against dollars already settled, never predicted.
  Undispatched items are retired exactly like a cancellation; in-flight items are never interrupted;
  an unpriced item contributes `0.0`, so the ledger is a floor and says so.
- **A worker-count knob**, `MANAROUTER_BATCH_WORKERS` (default 4, clamped 1–64, with a warning when it
  is clamped). This is the answer to "priority": there is deliberately no pre-emption, because
  cancelling a generation does not refund the GPU seconds it already spent — a batch delays an
  interactive request by at most one item, which is cheaper than killing a generation. The reasoning
  is in the batch module docs and in [`25-gpu-rentals.md`](25-gpu-rentals.md) §12.

A real batch also completed through the lane end to end (upload → create → poll → results), which is
how the upload route's multipart shape was found to be wrong and fixed. A batch has still **never**
run against a rented GPU: every cost figure in the docs is a computation over recorded measurements,
and that is stated where the figures live.

## Issues

- **#4 Modal + vLLM — substantial.** Budget cap and priority are answered above. What remains is not
  code I can finish tonight: Modal has no public REST deploy API, so bundle generation stays manual
  by design, and the 200 K-token end-to-end item needs a real GPU.
- **#11 Kaggle — partial by design.** The lane is admin-plane only: `status`, `run`, `run/{id}`,
  `runs`, with a 12-hour deadline cap refused as `deadline_exceeds_cap`. No inbound endpoint exists
  (the kernel is push-and-run), the `kaggle` CLI is not installed on this host, and Kaggle's terms
  forbid more than one active account, so pooled rotation is out of scope rather than unimplemented.

## Blocked, and needing you

1. **Rotate the Modal CLI credential.** It was exposed in a log earlier. I did not repeat it anywhere.
2. **Push.** Twenty-eight commits sit on local `main`. GitHub refuses writes from this session's
   identity (`403 … denied to ElizaNagase`), so PR 15 is merged *locally* only (`0e36fa2`) and
   nothing upstream has moved.
3. **One interpretation to confirm.** "Add a custom language for OpenRouter routing" was implemented
   as carrying OpenRouter's routing fields across the Anthropic translation (`e90f50f`). If you meant
   something else by "custom language" — a dialect, a panel surface, a docs page — say which and it
   gets redone; it is one commit on top of `e90f50f`.

## Deferred, not forgotten

`GetUsableModels`/`AvailableModels` pass-through; the KV blob responder; a `composer-2` → `composer-2.5`
alias; a relay-level HTTP test that drives a Cursor step end to end; wiring `carry_routing`'s
allow-list into the response side; and the `Intent`-side of the Anthropic tools path.

## Gates at the end of the night

`cargo fmt --all --check` clean, `cargo clippy --workspace --all-targets -- -D warnings` clean,
`cargo test --workspace` 1 891 passed / 0 failed, `node --test panel/test/*.test.mjs` 347 passed /
0 failed. Working tree clean; `main` at `a00920f`.
