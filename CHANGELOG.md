# Changelog

Each release lists what changed for the apps that use the library. Versions follow semantic
versioning; `API_VERSION` and `PROTOCOL_VERSION` are listed whenever they change. The release
workflow copies a version's section into its GitHub release notes.

## Unreleased

**App data**

- [docs/app-data.md](docs/app-data.md) describes hosted runs, where one of the user's devices runs
  the agent for a chat on another: the session's `hostDevice` (also a tag), the user turn's
  `hostRun` request, each device's agent directory (`ai.osaurus.directory/1`), the host's run
  state (`ai.osaurus.host-run/1`), and the stop and approval namespaces (`ai.osaurus.run/1`,
  `ai.osaurus.approval/1`, `ai.osaurus.approval-answer/1`). New fixtures:
  `hosted.json` and `agent-directory.json`. The library itself is unchanged.

## 0.1.0-beta.3 - 2026-10-24

`API_VERSION` 2: apps built against beta.2 need the signature changes below. `PROTOCOL_VERSION`
is unchanged, so beta.3 devices keep syncing with beta.1 and beta.2 ones; app data and tags
written by a beta.3 device are kept, but not shown, by older ones.

**App data**

- Apps can attach their own structured data to conversations (`setConversationAppData`) and
  messages (`OutgoingMessage.appData`, `setMessageAppData`, `OutgoingStream.finish(content:appData:)`),
  under a namespace, with encrypted blob attachments for large values. The last write wins per
  namespace; superseded values are dropped at compaction.
- `CoreConfig.maxAppDataBytes` sets the largest value this device writes (64 KiB by default).
- `readAttachment(scope:hash:)` returns an attachment's bytes without leaving a decrypted file.
- [docs/app-data.md](docs/app-data.md) is Osaurus's contract for its app data, with fixtures in
  `tests/fixtures/app-data` that the Rust, Swift and C# tests import.

**Device-local conversations**

- `createConversation(title:replication:)` with `.deviceLocal` creates a conversation that is
  never sent to other devices and compacts on its own. `promoteToSynced` syncs it later;
  `deleteLocalConversation` deletes one.
- `Conversation.replication`, and `StorageUsage.syncedEventBytes` and `localEventBytes`.

**Tags and queries**

- `setConversationTags`, `Conversation.tags`, `createdMs` and `messageCount`.
- `conversationsFiltered(filter:)` and `searchFiltered(query:filter:limit:)` filter by tags,
  kind, replication, archived and pinned, with paging.

**Encrypted at rest**

- The whole database, views and search index included, is now encrypted with SQLCipher. Existing
  databases are encrypted in place on first open. The key is kept in the key store
  (`view-key`) and sealed to the recovery key, so restoring a backup without the key store
  moves the database aside until the phrase is entered.
- The decrypted attachment cache is emptied when the core opens and closes.

**Signing out**

- `signOut(keepConversations:)`: with true, the device leaves the identity but keeps the
  conversations it owned as device-local ones.
- Removing a member, or rotating the identity, keeps what that member's revoked devices wrote
  before. It used to read as written after the removal, so a device that had joined an
  identity couldn't sign out.

**Other changes**

- `importLegacyConversation(conversation:messages:)` takes a `LegacyConversation` (metadata,
  tags and app data), and `LegacyMessage` gains attachments, tool fields and app data.
- `OutgoingStream.finish` takes app data. The Swift and C# helpers keep `finish(content:)`,
  `createConversation(title:)` and `LegacyMessage.text` working as before.
- The data directory moves to store version 4. Earlier releases can't open it afterwards.
- Support bundles now also redact ids whose 12-digit prefix happens to be all decimal digits.

## 0.1.0-beta.2 - 2026-10-24

`API_VERSION` and `PROTOCOL_VERSION` are unchanged; every addition is a new call.

**Reaching an agent by its address**

- `agentAddress(agent:)` returns an agent's `0x` address, the one osaurus-relay names it by.
- `agents(scope:)` lists a scope's agents with their address, role, host device and expiry.
- `agentCard(agentSecret:ttlMs:)` signs a card naming this device and user as the agent's host,
  for the app to serve at `/.well-known/osaurus-sync` on the agent's osaurus-relay URL.
- `verifyAgentCard(card:address:)` checks a card's signature, expiry and address, and returns
  the host device and the owner's user key.
- `createInviteFor(scope:user:role:ttlMs:)` creates an invite that only that user key can
  redeem, for sending over channels others can read.

**Changes**

- Invites are now admitted only by the device that created them. Redeeming already sends the
  request to that device, so working flows are unaffected.
- The data directory moves to store version 3, which adds a column for bound invites. Earlier
  releases can't open it afterwards.

## 0.1.0-beta.1 - 2026-10-24

The first prerelease, for building the Mac, iOS and Windows clients. `API_VERSION` 1,
`PROTOCOL_VERSION` 1. Compatibility rules apply from this release on: later builds keep
syncing with it and keep opening its data directories.

**Sync across one user's devices**

- Pairing with a QR payload or a typed code, the 24-word recovery phrase, recovery of restored
  backups, device revocation, and identity rotation for stolen devices.
- Messages, attachments, edits, deletions, reactions and conversation metadata (title, pinned,
  archived, sort order, read state), converging on every device in any delivery order.
- Live streaming of agent responses to other devices.
- Gradual sync: recent conversations first, older history as backfill that pauses on battery and
  metered networks, and on-demand history on iOS (90 days by default).
- Point-in-time views, conversation export, storage usage, leaving a conversation, and reset.
- Importing existing conversations with their original timestamps.

**In the API, not yet product-tested:** sharing with roles, invites and removals, and agents with
their own identities.

**Platforms and packages**

- Swift package for macOS 14+ and iOS 17+ (Apple silicon, device and simulator), with
  `KeychainKeyStore`, `SyncLifecycle`, `BackfillPolicyMonitor` and `os_log` integration.
- .NET 8 package for Windows x64 and arm64 (plus `osx-arm64` for development), with
  `DpapiKeyStore` and `ILogger` integration.
- Logging through a process-wide listener, diagnostics, and redacted support bundles.

**About this prerelease**

- The Windows DLLs are not code-signed.
- The packages are only attached to the GitHub release. Use
  `OsaurusSync-swift-local-0.1.0-beta.1.zip` as a local Swift package, and the `.nupkg` from a
  local NuGet source.
- Use `n0PublicNetworkConfig()` for sync over the internet during the beta.

**Known limitations**

- Devices only sync while another device that has the changes is online; there's no storage
  server yet.
- Nothing is compacted until a storage server exists, so history only grows.
- Pulling from several peers at once can fetch some events more than once.
- The local views are stored in plaintext; rely on OS disk encryption.
- The external cryptography review hasn't been done yet.
