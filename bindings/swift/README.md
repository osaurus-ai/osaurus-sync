# OsaurusSync for macOS and iOS

Swift package wrapping the Osaurus sync core (`OsaurusSyncFFI.xcframework`). Targets macOS 14+
(arm64) and iOS 17+ (device and arm64 simulator).

## Building

```sh
scripts/build-xcframework.sh        # builds the three slices and regenerates Sources/OsaurusSync/Generated
cd bindings/swift && swift test     # macOS
xcodebuild test -scheme OsaurusSync -destination 'platform=iOS Simulator,name=iPhone 16'
scripts/test-ios-sim.sh             # iOS Simulator without xcodebuild (older runtimes only)
```

## Opening the core

```swift
import OsaurusSync

let core = try SyncCore.open(
    dataDirectory: appSupport.appendingPathComponent("sync"),
    deviceName: UIDevice.current.name,
    keyStore: KeychainKeyStore(accessGroup: "TEAMID.ai.osaurus.shared")
)
core.setListener { event in /* devices, pairing, identity changes */ }
let sub = try core.subscribe(scope: conversationId) { changes in /* message diffs, streams */ }
```

Callbacks arrive on the core's `osaurus-sync-callbacks` thread in commit order; hop to the main
actor before touching UI. Keep `Subscription` objects alive; dropping one cancels it.

Errors are `SyncError` values with a stable `code` (`SyncErrorCode`); map codes, not `detail`
strings, to user-facing copy.

## Key storage

`KeychainKeyStore` stores the device key and user secret as generic passwords with
`kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`. They are never backed up or migrated, so a
restored or new device always gets a fresh device key and must be paired (or recovered from the
24-word phrase). On macOS, signed apps should use the data protection keychain (the default);
unsigned tools pass `useDataProtectionKeychain: false`.

## iOS integration

### Lifecycle

iOS closes the sockets of suspended apps. Wire the core to the app lifecycle:

```swift
let lifecycle = SyncLifecycle(core: core)   // keep a strong reference
lifecycle.start()
```

On entering the background it calls `suspend()` inside a background task: Loro snapshots are
flushed, the SQLite WAL is checkpointed and the endpoint is closed. Messages written while
suspended wait in the outbox. On entering the foreground it calls `resume()`, which reconnects
and catches up. The core does no networking in the background beyond that task; background
refresh and push wakeups are out of scope.

### History

iOS defaults to recent-only history: the last 90 days sync, and older ranges are fetched on
demand when the user scrolls past `MessagePage.atSyncBoundary` (call `requestHistory`). Override
with `history: .full` or per scope with `setHistoryMode`.

### Backfill on battery and metered networks

```swift
let backfill = BackfillPolicyMonitor(core: core)
backfill.start()
```

Background backfill pauses in Low Power Mode and on expensive (cellular, hotspot) or constrained
(Low Data Mode) paths. Live sync, pairing and on-demand history are unaffected. The same monitor
works on macOS for hotspot and Low Data Mode paths.

### Local network discovery (Info.plist)

Devices on the same network find each other with mDNS. On iOS this requires the local network
permission, so add to the app's Info.plist:

```xml
<key>NSLocalNetworkUsageDescription</key>
<string>Osaurus finds your other devices on this network to sync conversations directly.</string>
<key>NSBonjourServices</key>
<array>
    <string>_osaurus-sync._udp</string>
</array>
```

Without these keys, or if the user declines, set `network.mdns = false`; devices still connect
through discovery and relays. On cellular, connections are usually relayed rather than direct;
that is expected, and `diagnostics()` reports it per peer.
