# Client guide

How to integrate Osaurus Sync into an app. The API is the same on every platform; this guide shows
Swift and C# side by side. For package setup and platform-specific notes, see:

- [Swift package (macOS and iOS)](../bindings/swift/README.md)
- [C# package (Windows)](../bindings/csharp/README.md)

For how the library works inside, see the [architecture](architecture.md) doc.

## Concepts

- **Scope.** A conversation or a workspace. Everything (messages, members, keys) belongs to a
  scope, and you pass its id to most calls.
- **Identifiers are hex strings.** Scope ids, message ids, device keys and user keys are
  lowercase hex. Treat them as opaque.
- **Principals** name who did something: `u:<hex>` for a user and `a:<hex>` for an agent.
- **HLC.** Each message has an `hlc` (a hybrid logical clock value, used for ordering and
  cursors) and a `timestampMs` derived from it for display.
- **Local first.** Reads and writes go to the local database and return right away. Sync happens
  in the background, and changes from other devices arrive through subscriptions.

## Opening the core

Open one `SyncCore` per data directory, when the app starts, and keep it for the life of the
process. A second open of the same directory fails with `AlreadyOpen`.

```swift
import OsaurusSync

let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
    .appendingPathComponent("Sync")
let core = try SyncCore.open(
    dataDirectory: dir,
    deviceName: Host.current().localizedName ?? "Mac",   // UIDevice.current.name on iOS
    keyStore: KeychainKeyStore(accessGroup: "TEAMID.ai.osaurus.shared")
)
```

```csharp
using Osaurus.Sync;

var dir = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Osaurus", "Sync");
using var core = OsaurusSync.Open(dir, Environment.MachineName);   // DpapiKeyStore by default
```

These helpers fill in the platform, the default network configuration and the API version. The
optional parameters are:

| Parameter | Default | Notes |
| --- | --- | --- |
| `keyStore` | Keychain (Swift), DPAPI (C# on Windows) | Use `MemoryKeyStore` only in tests |
| `history` | Platform default | iOS: last 90 days. macOS and Windows: full history |
| `network` | `defaultNetworkConfig()` | See [Network configuration](#network-configuration) |

On first open the core creates a device key and a user identity. Opening also brings the network
up, unless `network.enabled` is false.

Before exit, call `close()`. It stops networking, flushes state and releases the data directory.
Any call after `close` fails with `Closed`.

## Reading

All reads are synchronous and local.

```swift
let list = try core.conversations()                        // [Conversation]
let page = try core.messages(scope: scope, before: nil, limit: 50)
for m in page.messages { print(m.role, m.content) }        // oldest first
if let cursor = page.nextCursor {
    let older = try core.messages(scope: scope, before: cursor, limit: 50)
}
let hits = try core.search(query: "flight", limit: 20)     // [SearchResult]
```

```csharp
var list = core.Conversations();
var page = core.Messages(scope, null, 50);
if (page.NextCursor is { } cursor)
{
    var older = core.Messages(scope, cursor, 50);
}
var hits = core.Search("flight", 20);
```

- **`messages`** returns one page, oldest first. Pass `nextCursor` as `before` to page back.
- **`atSyncBoundary`** is true when older history may exist on other devices but isn't synced
  here (recent-only mode). Offer a "load older messages" action that calls `requestHistory`.
- **`conversation(scope:)`** and **`message(id:)`** return one item, or nil.
- A **`Conversation`** has its title, pinned, archived and sort order, a preview of the last
  message, the unread count, your role and the member list.
- A **`Message`** has its role, content, author, attachments (`blobs`), reply and tool-call
  fields, whether it was `edited`, its reactions and its `delivery` state.

**Delivery** is `delivered`, `pending` (written here, not yet acknowledged by another device), or
`rejected` with a stable `reasonCode`:

| Code | Reason |
| --- | --- |
| 1 | Not a member |
| 2 | Role too low |
| 3 | Removed concurrently |
| 4 | Device revoked |
| 6 | Scope has no genesis |
| 7 | Invalid payload |
| 8 | Agent not hosted by this device |
| 9 | Not the author |
| 10 | Bad key epoch |

## Writing

```swift
let scope = try core.createConversation(title: "Trip plans")
let id = try core.sendMessage(scope: scope, message: .text("Book the flights"))
try core.editMessage(id: id, content: "Book the flights and hotel")
try core.addReaction(id: id, reaction: "👍")
try core.deleteMessage(id: id)

try core.renameConversation(scope: scope, title: "Japan trip")
try core.setPinned(scope: scope, pinned: true)
try core.setArchived(scope: scope, archived: false)
try core.setSortOrder(scope: scope, order: 1.5)    // fractional, for manual reordering
try core.markRead(scope: scope, hlc: lastMessage.hlc)
```

```csharp
var scope = core.CreateConversation("Trip plans");
var id = core.SendMessage(scope, OsaurusSync.Text("Book the flights"));
core.EditMessage(id, "Book the flights and hotel");
core.AddReaction(id, "👍");
core.DeleteMessage(id);
core.RenameConversation(scope, "Japan trip");
core.SetPinned(scope, true);
```

Each call is one local transaction. It returns once the change is durable and visible in the
views, and before it syncs.

- **Only the author can edit** a message. **The author or an Admin can delete** one. Deleting
  purges the content on every device.
- **`createWorkspace`** creates a scope for a team or project instead of a single conversation.
- **Messages with more fields.** `OutgoingMessage` also has `attachments`, `replyTo`, `asAgent`
  (to post as an agent this device hosts), and `toolCallId` and `toolName` for tool messages.

### Attachments

Pass file paths. The core encrypts each file into its blob store when you send.

```swift
var msg = OutgoingMessage.text("Here's the itinerary")
msg.attachments = [AttachmentFile(path: url.path, mediaType: "application/pdf", name: "itinerary.pdf")]
let id = try core.sendMessage(scope: scope, message: msg)

// Later, on any device:
let blob = message.blobs[0]                 // hash, size, mediaType, name, available
let path = try await core.fetchAttachment(scope: scope, hash: blob.hash)
```

`fetchAttachment` returns a path to the decrypted file, downloading it from a peer first if
`available` is false.

### Importing existing conversations

On first run, move existing local history in with `importLegacyConversation`. The original
timestamps are kept, so imported messages sort correctly among new ones.

```swift
let scope = try core.importLegacyConversation(
    title: old.title, createdMs: old.createdMs,
    messages: old.messages.map { LegacyMessage(role: .user, content: $0.text, createdMs: $0.createdMs) })
```

## Live updates

Subscribe to the scopes on screen and to the conversation list. Each subscription stays active
until you cancel it or release the object.

```swift
let sub = try core.subscribe(scope: scope) { changes in
    Task { @MainActor in
        for change in changes {
            switch change {
            case let .messageAdded(message): model.insert(message)
            case let .messageChanged(message): model.update(message)
            case let .messageRemoved(id): model.remove(id)
            case let .stream(update): model.showStreaming(update)
            case .syncStatusChanged: model.refreshStatus()
            case .reset: model.reload()
            }
        }
    }
}

let listSub = core.subscribeConversations { changes in /* .changed(conversation), .removed(scope) */ }

core.setListener { event in /* devices, pairing, identity; see below */ }
```

```csharp
using var sub = core.Subscribe(scope, changes =>
{
    foreach (var change in changes)
    {
        switch (change)
        {
            case ScopeChange.MessageAdded a: Ui(() => model.Insert(a.Message)); break;
            case ScopeChange.MessageChanged c: Ui(() => model.Update(c.Message)); break;
            case ScopeChange.MessageRemoved r: Ui(() => model.Remove(r.Id)); break;
            case ScopeChange.Stream s: Ui(() => model.ShowStreaming(s.Update)); break;
            case ScopeChange.Reset: Ui(model.Reload); break;
        }
    }
});
```

**Threading.** All callbacks run on one core thread named `osaurus-sync-callbacks`, in commit
order. Hop to the UI thread before touching UI. Callbacks may call back into the core.

**`reset`** means a membership change invalidated events you've already shown (for example, a
concurrent removal). Reload the scope from `messages`.

**Core events** (from `setListener`):

| Event | Meaning |
| --- | --- |
| `devicesChanged` | The device list changed; refresh `devices()` |
| `paired(device)` | Pairing finished |
| `pairingFailed(reason)` | Pairing failed |
| `identityChanged` | Pairing or recovery replaced the user identity; reload everything |
| `selfRevoked` | This device was revoked; sync has stopped |
| `peerRevoked(device)` | Another device was revoked |
| `inviteResult(scope, error)` | The result of `redeemInvite` |
| `incompatiblePeer(device, version)` | A peer runs an incompatible protocol version; suggest updating |

## Streaming responses

While an agent generates a reply, stream its tokens to the user's other devices. Tokens are never
stored. `finish` commits the final message.

```swift
let stream = try core.openStream(scope: scope, role: .assistant, asAgent: nil)
for try await token in model.generate(prompt) {
    try stream.push(delta: token)
}
let id = try stream.finish(content: nil)     // commits the streamed text; pass content to replace it
```

```csharp
using var stream = core.OpenStream(scope, MessageRole.Assistant, null);
await foreach (var token in model.Generate(prompt)) stream.Push(token);
var id = stream.Finish(null);
```

Other devices receive `stream` changes with the full text so far and a state of `streaming`,
`finished` or `interrupted`. The committed message arrives as `messageAdded` with the same id as
`stream.messageId()`, so it can replace the streaming bubble in place. Call `abort()` to cancel.
Releasing a stream without finishing it sends `interrupted`.

## Devices and pairing

Pairing adds a new device to the user's account. Start on a device that's already set up, then
join from the new one:

```swift
// Existing device
let offer = try core.pairingOffer(ttlMs: 5 * 60_000)
showQRCode(offer.payload)     // or show offer.code for typing

// New device
try await core.pairWithOffer(payload: scannedPayload)
// From a typed code: "osaurus-pair:1:\(offer.device):\(typedCode)"
// The result arrives as CoreEvent .paired or .pairingFailed.
```

The code expires after `ttlMs` and allows 5 attempts. Once paired, the new device has the same
user identity and syncs every conversation. `cancelPairing()` withdraws an open offer.

Managing devices:

```swift
let devices = try core.devices()          // id, name, platform, lastSeenMs, revoked, isSelf
try core.revokeDevice(device: lost.id)    // rotates scope keys; the device can't read new content
```

### Recovery phrase

Show the 24-word phrase during setup so the user can write it down:

```swift
let words = try core.recoveryPhrase()
```

On a device with no other device available, `recoverFromPhrase` restores the identity. Local
conversations on that device are kept. The recovered device can't read existing conversations
until another of the user's devices sees it and calls `approveDevice(device:)`, which returns the
scopes it was added to.

## Sharing and membership

Phase 1 apps sync one user's devices, but the membership API is complete:

```swift
let link = try core.createInvite(scope: scope, role: .member, ttlMs: 7 * 86_400_000)  // "osaurus-invite:1:..."
try otherCore.redeemInvite(token: link)            // result arrives as CoreEvent .inviteResult

try core.invite(scope: scope, user: userKey, role: .viewer)
try core.changeRole(scope: scope, principal: "u:\(userKey)", role: .member)
try core.remove(scope: scope, principal: "u:\(userKey)")
```

Roles are `viewer`, `member`, `admin` and `owner`. Viewers can only read. Members can post.
Admins manage Viewers and Members. Owners manage everyone. A disallowed call throws
`PermissionDenied`.

## Agents

An agent has its own identity (a secp256k1 secret), is hosted by one device, and can post in the
scopes it's assigned to.

```swift
let secret = generateAgentSecret()                 // store it in the key store yourself
let agentId = try core.assignAgent(scope: scope, agentSecret: secret, host: nil, role: .member, ttlMs: 30 * 86_400_000)
try core.sendMessage(scope: scope, message: OutgoingMessage(
    role: .assistant, content: "Done", attachments: [], replyTo: nil, asAgent: agentId, toolCallId: nil, toolName: nil))

_ = try core.publishAgentRecord(agentSecret: secret, ttlMs: 86_400_000)   // advertise this device as the host
let hosted = try await core.resolveAgent(host: deviceKey)                 // which agent a device hosts
```

`host: nil` makes this device the host. Sending `asAgent` from a device that doesn't host the
agent fails with `PermissionDenied`.

## History and backfill

```swift
try core.setHistoryMode(scope: nil, mode: .recent(days: 30))   // default for new scopes
try core.setHistoryMode(scope: scope, mode: .full)              // one scope
try core.requestHistory(scope: scope, beforeHlc: oldest.hlc, days: 30)
try core.setBackfillPolicy(policy: .paused)
```

- Recent conversations always sync first. Older history (more than 7 days) is **backfill**,
  which can be paused.
- **`requestHistory`** fetches older messages at high priority, for example when the user
  scrolls past `atSyncBoundary`. The messages arrive as `messageAdded` changes.
- On Apple platforms, `BackfillPolicyMonitor` pauses backfill automatically in Low Power Mode
  and on expensive or constrained networks. See the [Swift README](../bindings/swift/README.md).

## Point-in-time view

`viewAt` shows a conversation as it was at a past moment, for example in a history browser.

```swift
let past = try core.viewAt(scope: scope, timestampMs: then, limit: 50)
past.conversation?.title    // the title at that time
past.messages               // the newest 50 messages as of then, oldest first
past.complete               // false if this device compacted or never synced part of that history
```

The view is read-only. To restore old content, write new messages.

## App lifecycle

On iOS, call `suspend()` when the app goes to the background and `resume()` when it returns.
`SyncLifecycle` in the Swift package does this for you. Desktop apps can call them on sleep and
wake.

- **`suspend`** flushes state and closes the network endpoint. Local reads and writes still
  work; new messages wait in the outbox.
- **`resume`** reconnects and catches up.
- **`syncNow`** dials known devices immediately instead of waiting for the retry timer. Call it
  when the network comes back or the app becomes active.

## Status and diagnostics

```swift
let status = try core.syncStatus(scope: scope)
status.pendingOutbox        // local events not yet acknowledged by another device
status.lastSyncMs
status.backfillComplete
status.peers                // device, name, connected, relayed, lastContactMs

let diag = core.diagnostics()
diag.running
diag.homeRelays             // url, connected
diag.peers                  // connected, relayed, rttMs, syncing, storageRole
```

`relayed` is true when traffic goes through a relay rather than a direct path. On cellular this
is normal. Use it for a connection indicator or a support screen.

## Network configuration

```swift
var net = defaultNetworkConfig()   // Osaurus relays, default discovery, mDNS on
net.mdns = false                   // e.g. if the iOS local network permission was declined
net.relays = ["https://relay.example.com"]
net.dnsServer = "https://dns.example.com"; net.dnsOrigin = "example.com"   // set both or neither
let core = try SyncCore.open(dataDirectory: dir, deviceName: name, network: net)
```

| Field | Meaning |
| --- | --- |
| `enabled` | Bring the network up at open. If false, call `resume()` to go online |
| `relays` | Relay URLs. Empty means the Osaurus defaults |
| `relaysDisabled` | No relays: direct connections only |
| `dnsServer`, `dnsOrigin` | A self-hosted discovery server |
| `dnsDisabled` | No DNS discovery |
| `mdns` | LAN discovery |
| `bindPort` | 0 picks any port |

`localNetworkConfig()` turns off relays, DNS and mDNS. Use it for tests, and connect devices with
`localAddress()` and `addPeerAddress(address:)`.

## Errors

Every failing call throws `SyncError` in Swift or a `SyncException` subclass in C#. Each carries
a stable numeric code. Map codes to user-facing text, and use `detail` only for logs.

| Code | Name | Typical cause |
| --- | --- | --- |
| 100 | `storage` | Disk full or database I/O failure |
| 101 | `invalidInput` | A malformed id, phrase or argument |
| 102 | `notFound` | The message, scope or device doesn't exist |
| 103 | `permissionDenied` | The role doesn't allow it, or it's not your message |
| 104 | `crypto` | A key or signature check failed |
| 105 | `malformed` | Corrupt event data |
| 106 | `alreadyOpen` | Another process has this data directory open |
| 107 | `deviceRevoked` | This device was revoked |
| 108 | `noKey` | No scope key yet (for example, before approval) |
| 109 | `network` | A network operation failed |
| 110 | `incompatibleVersion` | The bindings and native library don't match |
| 111 | `pairing` | Pairing failed |
| 112 | `closed` | The core was closed |
| 113 | `keyStore` | The key store callback failed |
| 199 | `internal` | A bug; please report it |

```swift
do { try core.sendMessage(scope: scope, message: .text(text)) }
catch let e as SyncError where e.code == SyncErrorCode.permissionDenied { showReadOnlyBanner() }
```

```csharp
try { core.SendMessage(scope, OsaurusSync.Text(text)); }
catch (SyncException e) when (SyncErrorCode.CodeOf(e) == SyncErrorCode.PermissionDenied) { ShowReadOnlyBanner(); }
```

## Testing your integration

- Use `MemoryKeyStore` and a temporary data directory per test.
- Use `localNetworkConfig()` and exchange `localAddress()` values to sync two cores in one
  process. The binding tests (`BindingTests.swift`, `BindingTests.cs`) show complete examples,
  including pairing and streaming over loopback.
