# How the Cursor agent CLI does inference

This page records what the Cursor agent CLI sends to Cursor's servers, recovered from the CLI's own
shipped bundle. It exists because the question "can Manarouter route to Cursor?" keeps arriving, and
the answer requires knowing the wire shape rather than guessing it.

**The short answer: Cursor is not a provider, it is an agent loop, and this page explains why that is
a different kind of thing.** The integration that does work is the other direction — Cursor as a
*client* of Manarouter — and that is documented in [Cursor](../providers/cursor.md).

## 1. What was read, and what was not

Everything below comes from static reading of the CLI's JavaScript. **No live Cursor account was
used, no inference request was ever completed against Cursor's servers, and no response payload was
observed.** Each claim below is therefore tagged:

| Tag | Meaning |
|---|---|
| **read** | Literally present in the bundle: a service descriptor, a message schema, a header, a URL. |
| **observed** | Seen on the wire against a local sink that impersonated the API endpoint. |
| **inferred** | Follows from the code but was not executed, or depends on a server we could not reach. |
| **unknown** | Could not be established. Named, not papered over. |

The CLI is an [SEA](https://nodejs.org/api/single-executable-applications.html) binary at
`~/.local/bin/agent` with its JavaScript unpacked, in plain text, under
`~/.local/share/cursor-agent/versions/2026.11.13-eacade1/` (`index.js`, 7.6 MB, plus
`8096.index.js`, `8891.index.js`). Nothing below required a disassembler: the protobuf descriptors
and every schema are readable source in those files.

`agent --api-key`, `CURSOR_API_KEY`, `-e/--endpoint` and `CURSOR_API_ENDPOINT` all exist, so a local
sink can be put in front of the CLI (`docs/providers/cursor.md` explains why that is not a route).

## 2. Transport

The CLI does not speak REST. It speaks the **Connect protocol** over HTTP with **binary protobuf**
payloads (`@connectrpc/connect` 1.6.1 and a patched `connect-node`, `@bufbuild/protobuf` 1.10.1):
**read**.

```
POST {endpoint}/{fully.qualified.Service}/{Method}
content-type: application/proto
connect-protocol-version: 1
user-agent: connect-es/1.6.1
```

Default endpoint `https://api2.cursor.sh`; every RPC is a `POST` to a path built from the protobuf
service name. The CLI's own headers on a normal call — **observed**, verbatim from a captured
request:

```
authorization: Bearer <access token>
connect-protocol-version: 1
content-type: application/proto
x-cursor-client-type: cli
x-cursor-client-version: extension-unknown
x-ghost-mode: true
x-request-id: <uuid>
```

There is **no `x-cursor-checksum`** on these calls — **observed**. The IDE's traffic carries one, and
the bundles still reference checksum, OS/arch, timezone, streaming-status, team-id and host-app
headers, but the CLI's unary calls do not send them. Copying IDE headers is therefore not necessary
and would be fingerprint mismatch, not fidelity.

## 3. Authentication

Two paths, and the interesting one is the API key, because it is not a bearer in the usual sense.

**API key → access token** — **observed** (`POST /auth/exchange_user_api_key`):

```http
POST https://api2.cursor.sh/auth/exchange_user_api_key
Authorization: Bearer <CURSOR_API_KEY>
Content-Type: application/json

{}
```

The response is JSON, not protobuf, and carries `accessToken` (a JWT) plus `refreshToken` — **read**,
confirmed against a sink that returned exactly that shape and was accepted. Everything after the
exchange uses `authorization: Bearer <accessToken>`; the API key itself is never sent again.

The CLI classifies the exchange response — **read**: `403` is a specific "invalid key" error, `5xx` is
retryable, every other non-2xx means "invalid". The JWT's `exp` is parsed **without verifying the
signature** and the token is refreshed when `exp - now < 300 s` — the same five-minute margin and the
same trust reasoning as [Codex](../11-oauth-codex.md): the token came from the issuer over TLS in this
process, so an unverified read of `exp` is a cache-freshness decision, not a security boundary.

Browser login (`agent login`) is a separate flow: `startLogin()` opens a URL, then polls
`POST /auth/poll` with `{uuid, verifier}` — **read**.

So a Cursor "credential" is two-layer: a long-lived API key, and a short-lived access token minted
from it. That is the same shape as [Copilot](../13-oauth-copilot.md) (GitHub token → Copilot token),
and it is why Cursor cannot be a plain `bearer` entry in the catalogue.

## 4. The service map

`agent.v1.AgentService` — **read**, from the service descriptor in `./src/client.ts`:

| Method | Input | Output | Kind |
|---|---|---|---|
| `Run` | `AgentClientMessage` | `AgentServerMessage` | bidi streaming |
| `RunSSE` | `BidiRequestId` | `AgentServerMessage` | server streaming |
| `RunPoll` | `BidiPollRequest` | `BidiPollResponse` | server streaming |
| `GetUsableModels` | `GetUsableModelsRequest` | `GetUsableModelsResponse` | unary |
| `GetDefaultModelForCli` | `GetDefaultModelForCliRequest` | `GetDefaultModelForCliResponse` | unary |
| `GetAllowedModelIntents` | — | — | unary |
| `NameAgent`, `UpdateConversationMetadata`, `CreateTranscriptOverview`, `GetPromptContextUsage`, `NotifyConversationClone`, `WarmUp`, `UploadConversationBlobs`, `UploadLocalAgentRunToPromptQuality`, `GetSignedUrlForAttachedMedia`, `ListLocalSubscriptionTools`, `CallLocalSubscriptionTool` | — | — | unary |
| `StreamLocalAgentMailbox` | — | — | server streaming |

The same bundle also exposes the older `aiserver.v1.*` surface — `DashboardService` (`GetMe`,
`GetUserPrivacyMode`, team settings), `ServerConfigService` (`GetServerConfig`), `AiService`
(`AvailableModels`, `GetUsableModels`, `GetDefaultModelForCli`) — **observed**: the CLI calls those
first at startup, in that order, before any inference.

**There is no `StreamUnifiedChat` in the CLI bundle.** The IDE's chat-completions-shaped
`aiserver.v1.AiService/StreamUnifiedChat*` schemas are not shipped here — **read** (absent). So there
is no plain completion surface in the CLI to imitate even if one wanted to.

## 5. The bidi channel is split across two RPCs

This is the structural fact that decides the whole question, and it is easy to get wrong because the
`Run` method looks like a normal streaming call.

The CLI rewrites `Run` into `RunSSE` on the client side — **read**:

```js
class q {
  map(t) {
    if (t.name === A.methods.run.name)
      return { name: "RunSSE", I: c.$r, O: t.O, kind: u.I.ServerStreaming };
    throw new Error(`Unknown method: ${t.name}`);
  }
}
```

Resolving `c.$r` inside `aiserver/v1/bidi_pb.js` gives `BidiRequestId`, whose schema is
`BidiRequestId|1 request_id 9` — a single string. So the POST body of `RunSSE` is **one request id**,
not the conversation.

The conversation travels out of band, through a second service — **read**:

```js
I = { typeName: "aiserver.v1.BidiService",
      methods: { bidiAppend: { name: "BidiAppend", I: c.Qd, O: c.yq, kind: u.I.Unary } } }
```

with, from `bidi_pb.js`:

```proto
message BidiAppendRequest {
  string data = 1;              // JSON form
  BidiRequestId request_id = 2;
  int64  append_seqno = 3;      // monotonic per channel
  bytes  data_binary = 4;       // protobuf form; this is the one the CLI uses
}
```

and a polling variant for links where a response stream is unavailable:

```proto
message BidiPollRequest  { BidiRequestId request_id = 1; bool start_request = 2; }
message BidiPollResponse { int64 seqno = 1; string data = 2; bool eof = 3; }
```

So one turn is not one HTTP request: it is a long-lived response stream plus one unary append per
client message, tied together by a request id and an `append_seqno` the client must keep monotonic —
**inferred** as a whole, **read** in every part. The bundled `src/client.ts` wraps each `AgentClientMessage` in a
`BidiAppendRequest` and pushes it through `bidiAppend` with a per-frame timeout computed as
`appendTimeoutMs + ceil(bytes / 131072 * 1000)`, capping in-flight appends at 16 — **read**.

Streaming responses use Connect's envelope, `[flags: 1 byte][length: 4 bytes big-endian][payload]`,
with `flags & 0x02` marking end-of-stream — **observed**, as the terminator only; no populated frame
was captured (§9).

## 6. What one turn sends

`AgentClientMessage` carries a oneof; the inference case is `run_request` — **read**:

```proto
message AgentClientMessage {
  oneof message {
    AgentRunRequest      run_request              = 1;
    ExecClientMessage    exec_client_message      = 2;   // tool results, back to the server
    KvClientMessage      kv_client_message        = 3;
    ConversationAction   conversation_action      = 4;
    ExecClientControlMessage exec_client_control_message = 5;
    InteractionResponse  interaction_response     = 6;
    ClientHeartbeat      client_heartbeat         = 7;
    PrewarmRequest       prewarm_request          = 8;
  }
}
```

`AgentRunRequest` itself — **read**, field numbers as they appear in the descriptor:

```proto
message AgentRunRequest {
  ConversationStateStructure conversation_state = 1;
  ConversationAction          action           = 2;
  ModelDetails                model_details    = 3;
  string                      requested_model  = 9;
  repeated McpTool            mcp_tools        = 4;
  string                      conversation_id  = 5;
  McpFileSystemOptions        mcp_file_system_options = 6;
  string                      custom_system_prompt = 8;
  string                      subagent_type_name   = 11;
  string                      harness              = 13;
  string                      dev_raw_model_slug   = 18;
  string                      run_id               = 25;
  string                      agent_session_id     = 26;
  ClientLlmGatewayCredential  client_llm_gateway_credential = 30;
  // …plus flags, subagent model selections and telemetry (fields 14-37)
}
```

and the action, for a user turn — **read**:

```proto
message ConversationAction {
  oneof action {
    UserMessageAction  user_message_action  = 1;
    ResumeAction       resume_action        = 2;
    CancelAction       cancel_action        = 3;
    SummarizeAction    summarize_action     = 4;
    ShellCommandAction shell_command_action = 5;
    StartPlanAction    start_plan_action    = 6;
    ExecutePlanAction  execute_plan_action  = 7;
    // …subagent, goal-continuation, context-injection and auth cases
  }
}

message UserMessageAction {
  ConversationMessage user_message                  = 1;
  RequestContext      request_context               = 2;
  bool                send_to_interaction_listener   = 3;
  repeated ConversationMessage prepend_user_messages = 4;
  ConversationHistory conversation_history           = 7;
  ConversationHistory trailing_conversation_history  = 8;
}
```

Note what is **not** here: there is no `messages` array, no `system`, no `max_tokens`, no
`temperature`, no `stop`, no `tools` in the OpenAI sense. Tools are MCP tools with the server's own
schema; sampling is a property of the model, chosen by the server; and the prompt is assembled
server-side from `conversation_state`.

That state is the blocker. `ConversationStateStructure` — **read** — carries
`root_prompt_messages_json`, a repeated **bytes** field:

```proto
message ConversationStateStructure {
  repeated bytes root_prompt_messages_json = 1;
  repeated ConversationTurn turns          = 8;   // agent turns and shell turns
  repeated string pending_tool_calls       = 4;
  map<string, FileState> file_states       = 12;
  string summary                           = 6;
  // …mode, plans, subagents, timers, voice-call state (35+ fields)
}
```

Each entry of `root_prompt_messages_json` is a JSON-encoded prompt message in a format whose
serializer is **not in the CLI bundle** — the bundle only ever passes the field through — **read**
(absent). Reconstructing that format from a chat-completions message list is not a matter of
translation effort; the information is not in the input. The CLI also does not send history itself:
the server owns the conversation and the client sends deltas against it.

One simplification is **read** in the bundle and worth knowing: for a brand-new conversation with
one user message the CLI takes the "simplest path" (`turns.length === 0` plus a
`userMessageAction`), which is why a single-shot probe can look like a normal request. It is still a
`run_request` against a state structure, not a completion.

## 7. What one turn receives

```proto
message AgentServerMessage {
  oneof message {
    InteractionUpdate interaction_update              = 1;
    ExecServerMessage exec_server_message             = 2;   // "please run this tool"
    ConversationCheckpointUpdate conversation_checkpoint_update = 3;
    KvServerMessage  kv_server_message                = 4;
    ExecServerControlMessage exec_server_control_message = 5;
    InteractionQuery interaction_query                 = 7;
    TtftBreakdown    ttft_breakdown                   = 8;
  }
}

message InteractionUpdate {
  oneof message {
    TextDeltaUpdate text_delta              = 1;
    ToolCallStartedUpdate tool_call_started = 2;
    ToolCallCompletedUpdate tool_call_completed = 3;
    ThinkingDeltaUpdate thinking_delta       = 4;
    ThinkingCompletedUpdate thinking_completed = 5;
    UserMessageAppendedUpdate user_message_appended = 6;
    PartialToolCallUpdate partial_tool_call  = 7;
    TokenDeltaUpdate token_delta             = 8;
    SummaryUpdate summary                    = 9;
    ShellOutputDelta shell_output_delta      = 12;
    HeartbeatUpdate  heartbeat               = 13;
    TurnEndedUpdate  turn_ended              = 14;
    ToolCallDeltaUpdate tool_call_delta      = 15;
    StepStartedUpdate step_started           = 16;
    RoutedModelUpdate routed_model           = 24;
    ModelCallCompletedUpdate model_call_completed = 31;
    // …roughly thirty more, including voice-call utterances and grok nudges
  }
}
```

Text arrives as `text_delta`; a stop reason is not a field — the turn ends with `turn_ended`, and the
server keeps going until it decides the loop is over. `exec_server_message` is the server asking the
**client** to run a tool on the user's machine, which is what makes this an agent protocol rather
than an inference API: the same connection carries the request, the tool calls, and the results.

## 8. Why this is not a Manarouter dialect

The repository's rule is that a new wire shape must be argued explicitly in
[Provider dialects](../24-provider-dialects.md), not that the set is closed at nine. Cursor is the
case where the argument comes out **against**, for four independent reasons — any one of them is
sufficient:

1. **There is no completion endpoint to translate to.** The unit of work is a *turn of an agent loop*
   whose state the server owns. A router sees `messages[]` and must produce a stateless answer; the
   upstream wants a conversation id, an action, and bytes of prompt state it, not the router, is
   authoritative for.
2. **The prompt cannot be reconstructed.** `root_prompt_messages_json` is written by a serializer that
   is not in the shipped bundle (§6). Without it a request can only be a fresh conversation with
   flattened history — a lossy approximation of a chat completion, dressed as a provider.
3. **The transport is a channel, not a call.** One turn is an SSE stream plus out-of-band appends
   with sequence numbers and their own retry semantics (§5). The relay's stream path is built around
   one request and one response stream; a bidi channel is a different data plane, not a new arm of
   the existing one.
4. **It is an undocumented internal API of a subscription product.** Cursor publishes agent-management
   endpoints, not inference; there is no versioning contract for `agent.v1`, and no published
   rate-limit or terms posture for third-party routing. Shipping it in the catalogue would promise operators a stability that does not
   exist, and the catalogue's `note` field is not the place to disclaim a whole protocol.

The cost of being wrong here is asymmetric: a `cursor` dialect that half-works would put a
plausible-looking provider in front of users, fail on multi-turn conversations, and be
indistinguishable from a Cursor outage.

### What would change the answer

- A published Cursor inference API with a documented request/response shape.
- Or: a stable, documented format for `root_prompt_messages_json`, which would make a faithful
  single-turn adapter possible — still not a chat dialect, but an honest `responses`-style adapter
  could be argued.
- Or: a documented "bring your own model" gateway on the server side, which would move the question
  from protocol to configuration.

Absent one of those, the supported integration is the reverse direction:
[Cursor as a client](../providers/cursor.md).

## 9. What was not established

- **A completed inference request.** The CLI gates models on `AiService/GetUsableModels`; a sink that
  returns an empty response yields "Cannot use this model: … Available models:" and the CLI stops
  before `RunSSE`. No Cursor account was available, and no live call was made.
- **A populated response frame.** Only the Connect end-of-stream envelope was observed (§5). The
  framing is inferred from the library version and the terminator; per-message padding and compression
  flags were not exercised.
- **Whether `RunSSE` requires a prior `BidiAppend`, and in what order.** The client appends after
  opening the stream — **read** — but the server's tolerance for the reverse order is unknown.
- **Server-side validation of the CLI fingerprint headers** (`x-cursor-client-*`, `x-ghost-mode`).
  They are sent; whether removing them changes anything is untested.
- **Model list contents and naming.** `GetUsableModelsResponse` is a repeated `ModelDetails`
  (`model_id`, `display_model_id`, `display_name`, aliases, thinking details, max-mode and credential
  fields), but the ids Cursor actually serves were never retrieved.

## 10. Reproducing the reading

The bundle is plain JavaScript, so the descriptors can be found without tooling. The version
directory is versioned; adjust the path.

```bash
V=~/.local/share/cursor-agent/versions/2026.11.13-eacade1
grep -o 'typeName:"agent.v1.AgentService"[^;]\{0,3000\}' "$V/index.js"
grep -o 'typeName:"aiserver.v1.BidiService"[^;]\{0,300\}' "$V/index.js"
```

Message schemas are attached to each generated class as `static $() { return ["Name|1 field 9|…"] }`,
where the leading digit is the protobuf wire type (`9` string, `8` bool, `5` int32, `3` int64,
`12` bytes, `13` uint32), `?` marks optional, `*` marks repeated, and `#N` refers to the trailing
array of message types. To attribute an export alias (`$r`, `Qd`) to a message, resolve the module's
`r.d(e, { … })` export map first — minified class names are reused across modules, so a global name
search is wrong.
