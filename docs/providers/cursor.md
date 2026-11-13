# Cursor

**Cursor is a client of Manarouter, not a provider behind it.** There is no `cursor` entry in the
catalogue and no `cursor` dialect, and this page explains how to connect the two products in the
direction that works.

If you arrived looking for "route my Cursor subscription through Manarouter", the answer is that it
cannot be done faithfully, and the reason is a protocol rather than a missing adapter:
[how the Cursor agent CLI does inference](../research/cursor-cli-inference.md).

## Pointing Cursor at Manarouter

Cursor's IDE has a **bring-your-own-key** path for OpenAI-shaped endpoints, which is exactly what
Manarouter serves. The mechanism is an API key plus an **override base URL** — Settings → Models, add
an OpenAI API key, and set the base URL to your relay. The labels move between Cursor releases, so
look for "Override OpenAI Base URL" rather than for a fixed menu path.

1. Create a **client key** (not a provider credential) and keep it out of shell history:

   ```bash
   mr key create --label cursor --print
   ```

2. Set Cursor's OpenAI API key to that client key, and its base URL to the relay's OpenAI surface:

   ```
   http://127.0.0.1:20900/v1
   ```

   The trailing `/v1` matters: Cursor appends `/chat/completions` to whatever you give it.

3. Add the models you want to use as **custom model names**. Cursor sends the name it is given
   verbatim as `model`, so it must be a name your configuration actually provides — a
   `provider/model`, or a combo. `GET /v1/models` on the relay lists them; a combo is usually the
   right answer here, because it keeps the model choice on the Manarouter side where it can be
   changed without editing every Cursor client.

4. Check one request in the relay log before trusting the setup end to end. Cursor sends the usual
   OpenAI shape, so an operator who sees nothing in the log has a base-URL or key problem, not a
   translation problem.

### What the override does and does not cover

Two limits are worth knowing before you rely on it:

- **Only the OpenAI-shaped path is overridden.** Cursor's own hosted models, and features that talk
  to Cursor's servers directly, keep going to Cursor. This is a bring-your-own-key lane, not a
  replacement for Cursor's backend, and the split is Cursor's to change.
- **Cursor sets its own sampling and tool wiring.** Whatever the IDE decides per feature is what the
  relay receives; Manarouter forwards it. Don't expect to pin `temperature` or a tool list from the
  Manarouter side.

Both are properties of Cursor, reported by its users rather than documented as a contract: see
[Cursor's forum thread on custom OpenAI-compatible endpoints](https://forum.cursor.com/t/how-to-use-a-custom-openai-compatible-api-in-cursor-setup-limits-and-tips/171497).
Treat the mechanism as stable enough to try and as unversioned.

## The CLI cannot be pointed at Manarouter

`agent --endpoint` looks like it should do the same thing, and it does not. It sets **Cursor's own
Connect API base** — the host that serves `agent.v1.AgentService` — not an OpenAI-compatible base
URL (`agent --help`, "Target API endpoint URL"). Pointing it at the relay produces Connect requests
the relay does not serve.

The CLI's programmatic surfaces are `agent -p --output-format stream-json` and
[ACP](https://cursor.com/docs/cli/acp) over stdio. Both are ways for *another agent* to drive Cursor;
neither is an inference API, and neither is something a router can stand behind.

## Why there is no provider entry

Four reasons, any one of which is enough, argued in full in
[the CLI protocol record](../research/cursor-cli-inference.md#8-why-this-is-not-a-manarouter-dialect):

- the unit of work is a turn of a server-owned agent loop, not a completion;
- the prompt is assembled server-side from a state blob whose serializer is not in the shipped
  client, so a router cannot reconstruct it from `messages[]`;
- one turn is a bidi channel — an SSE stream plus out-of-band sequence-numbered appends — not one
  request and one response stream;
- it is an undocumented internal API of a subscription product, with no versioning or terms posture
  for third-party routing.

A catalogue entry is a promise that a route works. For Cursor that promise cannot be kept today, and
the `note` field is not the place to disclaim a whole protocol.

## What would make this page different

A published inference API, a documented `root_prompt_messages_json` format, or a server-side
bring-your-own-model gateway. Any of those turns the question from protocol archaeology into
configuration, and the record in
[the CLI protocol record](../research/cursor-cli-inference.md) is what a future adapter would start
from.

## See also

- [Provider dialects](../24-provider-dialects.md) — the rule that a new wire shape is argued, not
  assumed
- [Cursor's own documentation](https://cursor.com/docs) — the vendor's account, which is
  authoritative for what the IDE does
- [Adding a provider](README.md) — for providers that *do* have a reachable inference endpoint
