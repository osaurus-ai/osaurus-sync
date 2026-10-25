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
| `conversationSummary` | object | `{ id, summaryText, coveredTurnIds, createdAt, modelIdentifier }` |
| `workspace` | object | The shared-agent context (`workspaceId`, `agentAddress`, caller fields) |
| `parkedWorkflowRuns` | object[] | `{ run_id, call_id, tool_name }` |
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
| `attachments` | object[] | One per native attachment: `{ id, kind, index, filename?, fileSize?, format?, structuredDocumentMetadata? }`. `index` is the attachment's position in `Message.blobs`; `kind` is `image`, `document` or `audio` |
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
| `truncated` | boolean | |

## Fixtures

Each fixture describes a conversation (native fields, tags and session app data) and its
messages (native fields, `attachments`, `app_data` and, per namespace, `app_data_blobs`).
Attachment and blob entries carry their content as `text`; a test writes each to a file, imports the conversation with
`importLegacyConversation`, and checks that every field reads back unchanged. Indexes in the JSON
refer to those lists in order.

## Replication

Osaurus syncs interactive sessions (`chat`, `imported`) and keeps headless ones (`plugin`,
`http`, `channel`, automations, `self_schedule`, `delegation`, `workspace`) device-local. The app
data format is the same for both.
