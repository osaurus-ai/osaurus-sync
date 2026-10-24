# Client guide

How to integrate Osaurus Sync into an app. The API is the same on every platform; this guide shows
Swift and C# side by side. For package setup and platform-specific notes, see:

- [Swift package (macOS and iOS)](../bindings/swift/README.md)
- [C# package (Windows)](../bindings/csharp/README.md)

For how the library works inside, see the [architecture](architecture.md) doc. For the servers
the apps connect to, see [deployment](deployment.md).

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

## Known limitations

These hold for the current release. Design the UI around them rather than hiding them.

- **Devices only sync while another device that has the changes is online.** There's no
  always-on peer yet. With two devices, a message sent on the phone reaches the Mac only once
  both are online at the same time. Show `delivery = pending` on messages that no other device
  has acknowledged yet, and avoid copy that promises the message "synced". `syncStatus` reports
  `pendingOutbox` per scope.
- **History only grows.** Compaction waits for a storing peer, so nothing is compacted until a
  storage server exists. Show `storageUsage()` in settings, and offer `leaveConversation` for
  conversations the user wants gone from this device.
- **Some extra traffic.** A device pulling from several peers at once can fetch some events more
  than once. Duplicates are discarded and counted (`diagnostics().counters`); only bandwidth is
  wasted.
- **Views are stored in plaintext.** Events are encrypted on disk, but the views the app reads
  (titles, message text, the search index) aren't. The protection is the OS: FileVault, iOS data
  protection and BitLocker, plus per-user file permissions. Keep the data directory inside the
  app's own container or the user's profile.
- **iOS keeps recent history by default.** The last 90 days sync; older messages arrive when the
  user scrolls past `atSyncBoundary` and the app calls `requestHistory`. Desktop apps keep
  everything.
- **No servers are configured by default.** Without relays and a discovery server, devices sync
  only on the same network. See [Network configuration](#network-configuration) and the
  [deployment](deployment.md) doc.

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
| `network` | `defaultNetworkConfig()`: LAN only until the app sets relays and discovery | See [Network configuration](#network-configuration) |

A custom key store must return nil from `load` only when the item doesn't exist, and throw
`SyncError.keyStore` for anything else. If a read failure looked like a missing item, the core
would create a new identity. The built-in stores already do this: before the first unlock after
a reboot, `KeychainKeyStore` throws and open fails with `keyStore`, so open again after the
device is unlocked (for example on `protectedDataDidBecomeAvailable`).

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
| `identityChanged` | Pairing, recovery or rotation replaced the user identity; reload everything |
| `recoveryAvailable` | A restored backup is waiting for its recovery phrase |
| `selfRevoked` | This device was revoked; sync has stopped |
| `peerRevoked(device)` | Another device was revoked |
| `peerRejected(device)` | A device that is neither yours nor a member's tried to sync and was turned away |
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

Both sides prove they know the code, so if a typed code is combined with the wrong device id
(for example from a spoofed entry in a list of nearby devices), pairing fails with
`pairingFailed` rather than joining someone else's account. Prefer the QR payload, which
carries the device id.

Managing devices:

```swift
let devices = try core.devices()          // id, name, platform, lastSeenMs, revoked, isSelf
try core.revokeDevice(device: wiped.id)   // a lost or wiped device
let result = try core.revokeAndRotate(devices: [stolen.id])   // a stolen device
```

```csharp
var devices = core.Devices();
core.RevokeDevice(wiped.Id);
var result = core.RevokeAndRotate(new[] { stolen.Id });
```

Pick the call by what happened to the device:

- **`revokeDevice`** is for a device that was lost or wiped. It rotates scope keys so the device
  can't read new content, but the device keeps the user secret. A device that's still running
  could use that secret to read new content through the recovery grants, or to certify a new
  device key.
- **`revokeAndRotate`** is for a device that was stolen. It revokes the listed devices and
  replaces the user identity, so the stolen secret stops working. Your other devices take on
  the new identity when they next connect to a device that has it, and raise `identityChanged`.
  The recovery phrase changes: show `recoveryPhrase()` again and ask the user to write down the
  new one. The old phrase is refused.

`revokeAndRotate` returns the new user key and two lists of conversations. `rotated` holds the
ones this user owned, which the new identity now owns. `left` holds conversations owned by
someone else. The old identity left those, and their owners have to invite the new user key
before you can use them again. Messages your other devices send while the rotation is reaching
them may be marked not delivered.

### Recovery phrase

Show the 24-word phrase during setup so the user can write it down:

```swift
let words = try core.recoveryPhrase()
```

On a device with no other device available, `recoverFromPhrase` restores the identity. Local
conversations on that device are kept. Every scope key is also sealed to a recovery key derived
from the phrase, so the recovered device can read whatever history it can get. That might be a
restored backup, a member's device it syncs with, or a storing peer. It doesn't need another of
the user's devices to approve it. Recovery can't help if every device is gone and no backup or
peer still holds the events.

**Restored backups.** If the app's data directory comes back from a backup (Time Machine, iCloud
or Windows backup) without its keychain items, the core keeps the history and starts as a new
device. `recoveryAvailable()` returns true, and `recoveryAvailable` is delivered to the
listener. Until the user enters the phrase, the restored conversations aren't listed. If the user
secret was restored with the data (a synced keychain), the device finishes recovery by itself
when it opens.

```swift
if try core.recoveryAvailable() {
    try core.recoverFromPhrase(phrase: words.joined(separator: " "))
}
```

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

Anyone holding a `createInvite` link can redeem it. When the link travels over a channel others
can read, use `createInviteFor(scope:user:role:ttlMs:)` instead: only that user key can redeem it,
so a copied link admits nobody else. Either kind of invite is admitted only by the device that
created it, so that device has to come online for the redemption to finish.

`invite(scope:user:role:)` adds a user without a link, but it doesn't connect the two of you:
sync only dials devices already in a scope you share, and a device turns away strangers that
don't hold an invite. Send an invite link when the other user has never synced with you.

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

`agents(scope:)` lists a scope's agents. Each `AgentInfo` has the agent id, its `0x` address,
its role, the host device (`nil` if it was never assigned), `hostedHere`, and when the
assignment expires. If several devices assigned the same agent, the latest assignment wins.

### Reaching an agent by its address

[osaurus-relay](https://github.com/osaurus-ai/osaurus-relay) gives each agent a public URL,
`https://0x<address>.agent.osaurus.ai`, where the address comes from the agent's key. Sync
doesn't look agents up by address. Instead, the host app serves a signed **agent card** on that
URL, and a caller uses it to learn which device hosts the agent and which user owns it.
Membership still decides who syncs what.

```swift
// The host app, which keeps the osaurus-relay tunnel open for the agent:
let address = try agentAddress(agent: agentId)                   // "0x7e5f…", the tunnel's address
let card = try core.agentCard(agentSecret: secret, ttlMs: 86_400_000)
// Serve GET /.well-known/osaurus-sync as {"card": card}.

// The caller, given only the address:
let json = try await fetch("https://\(address).agent.osaurus.ai/.well-known/osaurus-sync")
let info = try verifyAgentCard(card: json.card, address: address)  // host device, owner's user key
let link = try core.createInviteFor(scope: workspace, user: info.user, role: .member, ttlMs: 86_400_000)
// POST the link to an endpoint the host app serves on the same URL.

// The host app, on receiving the link:
try core.redeemInvite(token: link)                               // dials the caller and joins
```

- The card is signed by the agent's key over the agent, the host device, the owner's user key,
  and an expiry. osaurus-relay terminates TLS and could change what it forwards, but it can't
  forge a card. The card doesn't include the device name.
- `verifyAgentCard` throws `invalidInput` for a malformed card, and `crypto` for a bad
  signature, an expired card, or an address that doesn't match. The address is compared
  case-insensitively. Pass `nil` only when you got the card some other way and will check
  `info.address` yourself.
- osaurus-relay passes any path to the host, so the well-known endpoint needs no relay change.
  The endpoint that receives invite links is up to the app; sync doesn't define it.
- Use a short TTL, such as a day, and sign a fresh card before the old one expires. A card for a
  host that no longer runs the agent keeps pointing at it until it expires.
- Keep the osaurus-relay tunnel on the device that `agents(scope:)` reports as the host, so the
  card and the tunnel agree.
- Send `createInviteFor` links over this channel, not `createInvite` links. osaurus-relay sees
  requests in plaintext, and a bound link is useless to anyone but the card's user.
- `publishAgentRecord` and `resolveAgent` are the other direction: given a device, which agent
  it hosts.

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

## Managing data

```swift
try core.leaveConversation(scope: scope)
let usage = try core.storageUsage()          // databaseBytes, attachmentBytes, attachmentCacheBytes
let summary = try await core.exportConversation(scope: scope, path: folder.path)
try await core.reset()                       // sign out: deletes this device's data and keys
```

- **`leaveConversation`** removes this user from the conversation and deletes the local
  copy. Other members keep theirs, and the remaining members get a new key, so this device
  can't read anything posted afterwards. The copy stays hidden on disk until another device
  has received the removal, so the removal isn't lost if the app quits first. On this user's
  other devices the conversation disappears from the list; each device deletes its own copy
  when it leaves. An invite brings the conversation back. The last owner of a conversation
  shared with others gets `PermissionDenied`; make someone else an owner first.
- **`storageUsage`** reports the database (including its write-ahead log), the encrypted
  attachments and the cache of decrypted attachments. The cache is trimmed automatically.
- **`exportConversation`** writes `conversation.json` and an `attachments/` folder into the
  given directory, creating it if needed. The JSON has `format: "osaurus-sync-conversation"`
  and `version: 1`, the title, members and roles, and each message with its author, device
  name, time, content, reactions and attachments (`file` is a path relative to the export,
  or `null` for attachments this device hasn't downloaded). `complete` is false when this
  device doesn't hold the whole history: older messages outside its history window, or
  history it compacted. Call `requestHistory` first to export more.
- **`reset`** is for signing out. It stops networking, deletes the data directory and the
  key store entries (`device-key`, `user-secret`, `user-secret-next`), and closes the core.
  Open a new core to start again as a new device. Other devices and members keep their
  data; revoke this device from another one to stop it being admitted with old keys.

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

`diag.counters` holds running totals since the core opened: sessions established and ended,
peers rejected, events received, applied, rejected, held, duplicate and invalid, limit hits,
pairings, and protocol bytes sent and received.

## Logging and support bundles

The core logs through a process-wide listener, so install it once at startup, before opening a
core. Records arrive on a dedicated thread, in order. If the app falls behind, records are
dropped rather than slowing sync down.

```swift
SyncLogging.useOSLog(level: .info)                     // os.Logger, one category per module
SyncLogging.setHandler(level: .debug) { record in … }  // or handle records yourself
```

```csharp
SyncLogging.UseLogger(loggerFactory, LogLevel.Info);   // ILogger, one category per module
SyncLogging.SetHandler(record => …, LogLevel.Debug);
```

Each `LogRecord` has a `level`, the Rust module as `target`, a `message`, structured `fields` and
`timeMs`. Debug and trace records come only from the sync crates; the networking stack adds
warnings and errors. Ids in fields appear as their first 12 hex digits. Message content,
titles and keys are never logged. With `os_log`, messages are public and fields are hashed
outside a debugger.

Typical messages: `session established` and `session ended` (per peer), `peer rejected` (with
`why`), `limit hit; disconnecting peer`, `pairing window opened`, `paired a new device`,
`pairing request refused`, `invite redemption`, and `networking started` and `networking
suspended`.

`supportBundle()` returns a JSON report to attach to a support request: versions, OS,
connection state per peer and scope, the counters, and the last 500 info-level records. Device
and scope ids become tokens (the same id gets the same token within one report), and addresses
and device names are left out. The recent records are kept even without a listener.

## Network configuration

The core doesn't default to anyone's servers. `defaultNetworkConfig()` has mDNS on and no
relays or discovery server, so devices sync on the same network only. For sync over the
internet, the app passes its own relay and discovery servers, for example from its remote
configuration:

```swift
var net = defaultNetworkConfig()
net.relays = ["https://relay-us.example.com", "https://relay-eu.example.com"]
net.discoveryServer = "https://dns.example.com/pkarr"   // set both or neither
net.discoveryOrigin = "dns.example.com"
net.publish = .relayOnly                                 // the default
net.mdns = false                                         // e.g. if the local network permission was declined
let core = try SyncCore.open(dataDirectory: dir, deviceName: name, network: net)
```

| Field | Meaning |
| --- | --- |
| `enabled` | Bring the network up at open. If false, call `resume()` to go online |
| `relays` | Relay server URLs (`iroh-relay`). Empty means no relays: direct connections only |
| `discoveryServer`, `discoveryOrigin` | A pkarr relay and the DNS origin it serves records under. Unset means no internet discovery |
| `publish` | What goes into this device's discovery record: `relayOnly` or `directAddresses` |
| `mdns` | LAN discovery |
| `bindPort` | 0 picks any port |

- **`n0PublicNetworkConfig()`** uses number0's public relays and discovery server. It's
  convenient for development and beta builds, but they're shared, rate limited and not run for
  Osaurus, so don't ship it. The [deployment](deployment.md) doc covers running your own
  servers and rolling them out.
- **`localNetworkConfig()`** also turns off mDNS. Use it for tests, and connect devices with
  `localAddress()` and `addPeerAddress(address:)`.
- Devices find each other by device id. Without a discovery server, a device that isn't on the
  same network can still be reached through a relay if both use the same relays and the dialer
  knows the address, for example from `localAddress()` in a QR code.
- `publishAgentRecord` and `resolveAgent` need a discovery server.

### What each setting discloses

All sync traffic is end-to-end encrypted and authenticated with the device keys, so no server
sees message content, titles, attachments or keys. What servers and peers can see:

| Who | Learns |
| --- | --- |
| Relay servers | Each device's id and public IP; which device ids talk to each other, when, and how much (for traffic that goes through the relay) |
| The discovery server | Each device's id and the IP it publishes from, and anyone's lookups of a device id |
| Anyone who knows a device id, with `relayOnly` | The device's home relay URL, and from the record's timestamp roughly when it was last online |
| Anyone who knows a device id, with `directAddresses` | The above, plus the device's LAN and public IP addresses |
| Devices on the same network, with `mdns` on | The device id and LAN addresses |
| A peer that connects | The device's public IP and LAN addresses, exchanged to set up a direct path, before the sync handshake decides whether to admit it |

Device ids appear in invites, pairing codes and QR payloads, so treat them as shareable, not
secret. Use `directAddresses` only for devices with a public address, such as a server, where
it saves the first round trip through a relay. Agent host records published with
`publishAgentRecord` go into the same discovery record and are readable the same way.

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
| 113 | `keyStore` | The key store failed, for example a Keychain read before the first unlock |
| 114 | `corrupt` | The database is damaged beyond repair; offer `reset` and sync again from other devices |
| 115 | `newerDatabase` | The data directory was written by a newer release; ask the user to update |
| 199 | `internal` | A bug; please report it |

```swift
do { try core.sendMessage(scope: scope, message: .text(text)) }
catch let e as SyncError where e.code == SyncErrorCode.permissionDenied { showReadOnlyBanner() }
```

```csharp
try { core.SendMessage(scope, OsaurusSync.Text(text)); }
catch (SyncException e) when (SyncErrorCode.CodeOf(e) == SyncErrorCode.PermissionDenied) { ShowReadOnlyBanner(); }
```

## Integration checklist

Before handing a build to testers, check that the app:

- [ ] **Opens one core per process** at startup, keeps it for the life of the process, and calls
      `close()` on exit. A second instance of the app gets `alreadyOpen`; handle it (for
      example, by bringing the first instance forward on Windows).
- [ ] **Uses the platform key store** (`KeychainKeyStore`, `DpapiKeyStore`), or a custom one
      that throws `keyStore` on read errors and returns nil only for a missing item.
- [ ] **Retries open after a `keyStore` error** on iOS once protected data becomes available,
      rather than treating it as fatal.
- [ ] **Follows the app lifecycle.** On iOS, start `SyncLifecycle`. On desktop, call `suspend()`
      on sleep and `resume()` on wake.
- [ ] **Calls `syncNow()`** when the network comes back and when the app becomes active.
- [ ] **Reads the network configuration from remote configuration**, with a bundled default.
      See the [deployment](deployment.md#client-configuration) doc.
- [ ] **Sets the iOS Info.plist keys** for local network discovery, and sets `mdns = false` if the
      user declines the permission.
- [ ] **Shows the recovery phrase** during setup, and again after `revokeAndRotate`.
- [ ] **Handles the core events:** reloads on `identityChanged`, stops sync UI on `selfRevoked`,
      prompts for the phrase on `recoveryAvailable`, suggests updating on `incompatiblePeer`,
      refreshes the device list on `devicesChanged` and `peerRevoked`, and reports
      `pairingFailed` to the user.
- [ ] **Reloads a scope on `reset`** changes, and hops to the UI thread in every callback.
- [ ] **Maps error codes** to user-facing messages, and logs `detail` only.
- [ ] **Shows connection state** from `diagnostics().peers` (connected, relayed) and pending
      messages from `delivery`.
- [ ] **Installs a log listener** at startup and offers `supportBundle()` from a settings or help
      screen.
- [ ] **Shows storage use** (`storageUsage()`) and offers sign-out with `reset()`.

## Troubleshooting

**Devices don't find each other.**

- Check that they're paired: each device should list the other in `devices()`.
- On the same network, discovery uses mDNS. On iOS, check the Info.plist keys and the local
  network permission (**Settings > Privacy & Security > Local Network**). On Windows, check that
  the firewall prompt was accepted for private networks. Some guest and corporate networks block
  mDNS; relays still work there.
- On different networks, both devices need relays and the same discovery server. Check
  `diagnostics().homeRelays`: at least one should be `connected`. If the list is empty, no relays
  are configured.

**Connections are always relayed.** `relayed` is normal on cellular and behind some NATs. On a
shared network, a relayed connection usually means a firewall is blocking UDP between the
devices. Relayed sync works, with more latency.

**Messages stay `pending`.** No other device has acknowledged them yet, usually because no other
device of the user's is online. Check `syncStatus(scope:).peers` and `diagnostics().peers`.

**Older history doesn't arrive.** Check `syncStatus(scope:).backfillComplete` and
`diagnostics().backfillPaused`. `BackfillPolicyMonitor` pauses backfill in Low Power Mode and on
cellular, hotspot and Low Data Mode connections. On iOS, older ranges also need
`requestHistory` (see [History and backfill](#history-and-backfill)).

**Open fails.**

| Error | Likely cause | What to do |
| --- | --- | --- |
| `keyStore` | iOS before the first unlock after a reboot, or a DPAPI file from another Windows user | Open again after unlock; for DPAPI, see the [C# README](../bindings/csharp/README.md#keys) |
| `alreadyOpen` | Another instance of the app, or an extension, has the data directory | Use one process per data directory |
| `incompatibleVersion` | The bindings and the native library come from different builds | Use the Swift package or `.nupkg` from one release, unmodified |
| `newerDatabase` | The data directory was written by a newer release | Ask the user to update |
| `corrupt` | The database is damaged | Offer `reset()`; the data syncs back from other devices |

**A device is turned away.** `peerRejected` means a device that isn't this user's, or a member's,
tried to sync. After a `reset()` on one device, it's a new device: pair it again, and revoke the
old entry from another device.

For anything else, collect `supportBundle()` from each device involved.

## Testing your integration

- Use `MemoryKeyStore` and a temporary data directory per test.
- Use `localNetworkConfig()` and exchange `localAddress()` values to sync two cores in one
  process. The binding tests (`BindingTests.swift`, `BindingTests.cs`) show complete examples,
  including pairing and streaming over loopback.
