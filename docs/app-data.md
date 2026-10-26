# Osaurus app data

The sync core stores app data as opaque bytes. This document is the contract the Osaurus clients
(macOS, iOS, Windows) follow for what goes inside, so that a conversation written on one reads
the same on the others. The fixtures in [`tests/fixtures/app-data`](../tests/fixtures/app-data)
are the test vectors; the Swift and C# binding tests and the Osaurus mapper tests all read them.

## Where data lives

| Data | Where |
| --- | --- |
| Visible text | The message's native `content` |
| Who wrote it | The native role. Osaurus `tool` turns use `toolResult` with `toolCallId` and `toolName` |
| Files the user sees | Native message attachments (`Message.blobs`) |
| Title, pinned, archived, creation time | Native conversation fields |
| Last activity | Native `lastActivityHlc` |
| Lookup keys | Conversation tags (see below). Tags duplicate a few fields for indexed queries; they never replace app data |
| Everything else about a session | Conversation app data, namespace `ai.osaurus.session/1` |
| Everything else about a turn | Message app data, namespace `ai.osaurus.turn/1` |
| Device-only state (folder bookmarks, tool-call step logs) | Never in app data. It stays in the device's own database |

A client that understands none of the app data still shows a readable conversation: the text,
the roles, the tool results and the user's files are all native.

## Encoding

- Each namespace's bytes are one UTF-8 JSON object.
- Every object has `"v": 1`, the namespace's major version.
- Timestamps are integer milliseconds since the Unix epoch. Durations are seconds, as numbers.
- Binary values are base64 strings.
- Ids (session, turn, agent, project) are lowercase UUID strings.
- Absent and `null` mean the same thing. Writers omit absent fields.

### Unknown fields

Readers ignore fields they don't know. Writers that update an object read the current value,
change what they own, and write it back with every unknown field preserved. A field is never
reused with a different meaning.

### Versions

A breaking change creates a new namespace (`ai.osaurus.turn/2`). During the transition writers
write both versions. Data written by a different subsystem gets its own namespace
(for example `ai.osaurus.turn.billing/1`) rather than new fields in someone else's object.

### Blob references

In message app data, any value may be replaced by a blob reference:

```json
{ "$blob": 0, "size": 81234 }
```

`$blob` is an index into the attachments written with this namespace (`AppData.blobs`, in the
order they were written). The blob's content is the UTF-8 JSON of the replaced value; `size` is
its length in bytes. Indexes rather than hashes, because a blob's hash isn't known until it is
written. Conversation app data has no attachments, so it never contains blob references.

A writer that rewrites a namespace replaces its attachments too, so it writes every blob the new
object references again (fetching ones it didn't create with `readAttachment`). Readers fetch it with `fetchAttachment` (or `readAttachment`, which leaves no
plaintext file) when they need the value. App data blobs are internal: they never appear in
`Message.blobs` and are never shown as attachments.

### Size

One write is at most 64 KiB (`maxAppDataBytes`). Writers keep under it in this order:

1. In turn data, values over 16 KiB move to blobs: each tool result, `responsesOutputItems`,
   `reasoningEncrypted` and `sharedArtifacts`.
2. If a turn object is still too large, these fields are dropped, in order:
   `injectedContextPrefix`, `responsesOutputItems`, `reasoningEncrypted`, `thinking`.
3. If a session object is too large, `conversationSummary` is dropped (Osaurus regenerates it).
4. A writer that dropped anything sets `"truncated": true`.

## Conversation tags

Tags are small strings for indexed lookups (`conversations(filter:)`). Osaurus writes:

| Key | Value |
| --- | --- |
| `osaurus.session` | The session id |
| `source` | The session source (`chat`, `imported`, `http`, ...) |
| `agentId` | The agent id, when the session belongs to one |
| `projectId` | The project id, when set |
| `externalSessionKey` | The caller's grouping key, when set |
| `dispatchTaskId` | The dispatch task id, when set |
| `hostDevice` | The host's device id, when the session is hosted (see [Hosted runs](#hosted-runs)) |
| `servedBy` | The device that served a `workspace` session (see [Served chats](#served-chats)) |

## `ai.osaurus.session/1`

Conversation-level, written with `setConversationAppData`.

| Field | Type | Notes |
| --- | --- | --- |
| `v` | number | `1`. Required |
| `id` | string | The Osaurus session id. Required |
| `source` | string | `chat`, `plugin`, `http`, `channel`, `automation`, `schedule`, `watcher`, `webhook`, `email`, `self_schedule`, `imported`, `delegation`, `workspace`. Default `chat` |
| `automationKind` | string | The trigger kind when `source` is `automation` |
| `sourcePluginId` | string | The plugin when `source` is `plugin` |
| `externalSessionKey` | string | |
| `dispatchTaskId` | string | |
| `agentId` | string | Absent means the default agent |
| `projectId` | string | |
| `selectedModel` | string | |
| `capabilities` | string[] | `vision`, `voice`, `code`, `search` |
| `folderPath` | string | Display path only. The folder bookmark is device-only |
| `conversationSummary` | object | `{ id, summaryText, coveredTurnIds, createdAt, modelIdentifier, savedTokensEstimate }` |
| `workspace` | object | The shared-agent context (`workspaceId`, `agentAddress`, caller fields) |
| `parkedWorkflowRuns` | object[] | `{ run_id, call_id, tool_name }` |
| `hostDevice` | string | The device id that runs the session's agent. Absent means the device that sends a turn runs it. See [Hosted runs](#hosted-runs) |
| `updatedAt` | number | When the session last changed, if it differs from the last message |
| `truncated` | boolean | |

## `ai.osaurus.turn/1`

Message-level, written with the message (`OutgoingMessage.appData`, `finish(content:appData:)`)
or onto it later (`setMessageAppData`).

| Field | Type | Notes |
| --- | --- | --- |
| `v` | number | `1`. Required |
| `id` | string | The Osaurus turn id. Required; it maps turns to messages across saves |
| `role` | string | `user`, `assistant`, `system`, `tool`. Required |
| `thinking` | string | Reasoning text shown in the thinking block |
| `thinkingDuration` | number | |
| `toolCalls` | object[] | `{ id, type: "function", function: { name, arguments }, geminiThoughtSignature? }` |
| `toolResults` | object | Tool call id to result text |
| `toolCallDurations` | object | Tool call id to seconds |
| `attachments` | object[] | One per native attachment: `{ id, kind, index, filename?, fileSize?, format?, sha256?, structuredDocumentMetadata? }`. `index` is the attachment's position in `Message.blobs`; `kind` is `image`, `document`, `audio` or `video`; `sha256` is the lowercase hex SHA-256 of the file, which a client can use to find its own cached copy |
| `sharedArtifacts` | object[] | `{ id, contextId, contextType, filename, mimeType, fileSize, hostPath, ... }` |
| `createdAt`, `completedAt`, `lastOutputAt`, `requestedAt` | number | |
| `generationTokenCount`, `inputTokenCount`, `cachedInputTokenCount` | number | |
| `timeToFirstToken`, `generationTokensPerSecond`, `modelLoadSeconds` | number | |
| `terminalStopReason` | string | |
| `reasoningItemId`, `reasoningEncrypted` | string | OpenAI Responses reasoning continuity |
| `responsesOutputItems` | array | Provider-native Responses items, replayed as they are |
| `modelContextExcluded` | boolean | |
| `routerBilling` | object | Billing metadata, no text |
| `injectedContextPrefix` | string | The context block a user turn was sent with |
| `author` | object | Who wrote the turn in a shared conversation |
| `remoteParticipantMessageId` | string | |
| `hostRun` | object | On a user turn: `{ device, requestedMs }`, a request for `device` to run the agent on this turn. See [Hosted runs](#hosted-runs) |
| `truncated` | boolean | |

## Hosted runs

A session can run its agent on another of the user's devices: you chat on one Mac with an agent
that lives on another. The device that runs the agent is the *host*; the device you type on is
the *caller*. Only the host runs anything; the caller never runs the turn itself.

### Requesting a run

- The session's `hostDevice` names the host, and the conversation carries the same value as its
  `hostDevice` tag so the host can find the sessions it hosts.
- Each user turn the caller wants answered carries `hostRun: { device, requestedMs }`, with
  `device` equal to `hostDevice`. A turn without `hostRun` is never run: history, imports and
  edits don't start runs on their own.
- The host runs each `hostRun` turn addressed to it at most once. It records the turn as handled
  before it starts, so a crash or quit never runs it twice. A run that was interrupted is not
  retried; the host answers it with an assistant turn whose `terminalStopReason` is
  `interrupted`, and the user can send again.
- A host that doesn't run agents for other devices answers with an assistant turn whose
  `terminalStopReason` is `host_declined`.
- `agentId` is the agent's id on the host. The caller doesn't need that agent; it shows the
  name and avatar from the host's directory.

### Replies

The host assigns the agent to the conversation (`assignAgent`, with the agent's own secret, so
its `0x` address is the agent's address) and sends the reply `asAgent`, streamed with
`openStream(asAgent:)`. The reply turns are ordinary `ai.osaurus.turn/1` turns. A reader shows
the agent by matching the message's agent address to an entry in the host's directory.

### Agent directory

Each device that hosts agents keeps one conversation listing them, and writes only its own, so
two devices never write the same value.

| Tag | Value |
| --- | --- |
| `ai.osaurus.kind` | `agent-directory` |
| `device` | The device id |

The conversation has no messages. Its app data, namespace `ai.osaurus.directory/1`:

| Field | Type | Notes |
| --- | --- | --- |
| `v` | number | `1`. Required |
| `device` | string | The device id, the same as the tag. Required |
| `agents` | object[] | `{ id, address?, name, description?, avatar? }`. `id` is the agent's id on the device, `address` its lowercase `0x` sync agent address (absent before the agent has a key), `avatar` an emoji or short text. Empty when the device doesn't run agents for other devices |
| `updatedMs` | number | When the list last changed |

Readers take the union of the directories whose device is one of the user's non-revoked devices
and ignore the rest. Apps don't show directory conversations as chats.

### Stop and approvals

Caller and host each write their own namespace on the session's conversation, so neither's
write replaces the other's:

| Namespace | Written by | Fields |
| --- | --- | --- |
| `ai.osaurus.run/1` | Caller | `v`; `stop`: the id of the user turn whose run should stop; `stopMs` |
| `ai.osaurus.host-run/1` | Host | `v`; `turn`: the user turn the host is running or last ran; `state`: `running` or `done`; `updatedMs` |
| `ai.osaurus.approval/1` | Host | `v`; `pending`: the tool calls waiting for the user, `{ id, turnId, tool, description, arguments, surface?, offersRunLease, requestedMs }` |
| `ai.osaurus.approval-answer/1` | Caller | `v`; `answers`: approval id to `denied`, `allowOnce`, `allowForRun` or `alwaysAllow` |

- The host stops the run whose user turn is `stop`, and ignores a `stop` naming any other turn.
- The caller treats its latest `hostRun` turn as answered once `host-run/1` says that turn is
  `done`, or once an assistant turn follows it.
- `arguments` is the tool call's JSON arguments as a string. `surface` says where the call runs on
  the host: `sandboxVM`, `nativeHost` or `remoteServer`. `offersRunLease` says whether `allowForRun` ("Allow for This
  Task") is on offer.
- The host removes an approval from `pending` once it is answered, denied, or its run ended.
  It ignores answers for ids that aren't pending. The caller drops answers whose ids are no
  longer pending.
- `alwaysAllow` changes the agent's tool policy on the host, as if it had been chosen there.

## Served chats

A `workspace` session is a conversation a device ran for someone else with one of the user's
agents: a workspace teammate, an invite-link caller, or the user's own phone. The device that
ran it is the *server*. Only the server talks to the caller; the user's other devices act
through the conversation and the server carries the action out.

- The server writes `servedBy` (its device id) when it creates or promotes the conversation.
  Other writers keep the tag; it never changes. `hostDevice` is not used: these aren't hosted
  runs, and no `hostRun` turn is ever written into them.
- Readers list the session under the server's agent (`agentId` is the agent's id on the
  server), labelled with the caller from the session's `workspace` object.
- Turn `author` says who wrote each user turn: the caller (`teammate`, `invite_peer`, ...), the
  owner at the server (`owner`), or the owner on another device (`owner_device`, with
  `deviceLabel`).

### Owner turns from other devices

Another device writes the user's turn as an ordinary user message whose `author` is
`owner_device`. The server delivers it to the caller and to a live run once:

- It delivers only user messages written as the user (not `asAgent`) by one of the user's
  non-revoked devices other than itself, whose `author` is `owner` or `owner_device`. Anything
  else is history.
- It records each delivered turn before acting on it, so a restart never delivers twice, and it
  doesn't deliver turns older than seven days when it first sees them.
- Sessions from the user's phone take no owner turns.

### Owner approvals

| Namespace | Written by | Fields |
| --- | --- | --- |
| `ai.osaurus.served-delivery/1` | Server | `v`; `delivered`: the ids of the owner turns from other devices it handled, oldest first, at most 200 |
| `ai.osaurus.owner-approval/1` | Server | `v`; `pending`: the caller's tool calls waiting for the user, `{ id, tool, title, description, arguments?, caller?, agent?, offersConversationLease, requestedMs, expiresMs? }` |
| `ai.osaurus.owner-approval-answer/1/<device>` | That device | `v`; `answers`: approval id to `{ answer, answeredMs }`, `answer` one of `approved`, `denied`, `allowed_for_conversation` |

- Each answering device writes only the namespace ending in its own device id, so two devices
  answering never overwrite each other.
- The server applies the earliest answer from a non-revoked device and ignores answers for ids
  that are no longer pending; the first answer, on any device or at the server, wins.
- The server removes an approval from `pending` once it is answered, times out or its run ends;
  readers then withdraw it. Timeouts and cancellation happen on the server only.
- `allowed_for_conversation` is on offer only when `offersConversationLease` is true.
  `arguments` is the tool call's JSON arguments as a string; callers never receive it.
- Devices drop their answers for ids no longer pending.

## Fixtures

`hosted.json` is a hosted run with a pending approval and a stop request; `agent-directory.json`
is a device's directory; `served.json` is a served chat with a delivered owner turn and an
answered owner approval. Each fixture describes a conversation (native fields, tags and session app data) and its
messages (native fields, `attachments`, `app_data` and, per namespace, `app_data_blobs`).
Attachment and blob entries carry their content as `text`; a test writes each to a file, imports the conversation with
`importLegacyConversation`, and checks that every field reads back unchanged. Indexes in the JSON
refer to those lists in order.

## Replication

Osaurus syncs interactive sessions (`chat`, `imported`) and served chats (`workspace`), and keeps
headless ones (`plugin`, `http`, `channel`, automations, `self_schedule`, `delegation`)
device-local. The app data format is the same for both.
