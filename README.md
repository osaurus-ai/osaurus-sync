# Osaurus Sync

Local-first, end-to-end encrypted sync of Osaurus conversations across a user's devices. One Rust
core runs inside the Mac, iOS and Windows apps, through generated Swift and C# bindings.

- **Peer to peer.** Devices sync directly over QUIC ([Iroh](https://iroh.computer)), finding each
  other on the LAN with mDNS or anywhere through DNS discovery. Relays only forward encrypted
  traffic when there's no direct path. The app chooses the relay and discovery servers, and
  they store nothing.
- **Local first.** Reads and writes hit local SQLite and return in well under a millisecond. The
  app works the same offline, and changes sync when devices reconnect.
- **Encrypted and signed.** Every change is a signed event. Content is encrypted with per-scope
  keys that only member devices hold.
- **Convergent.** Any devices with the same events show the same conversations, whatever order
  the events arrived in. Edits, deletions, reactions and metadata all merge without conflicts.
- **Built for real use.** Recent conversations sync first, older history backfills in the
  background (and pauses on battery or cellular), and agent responses stream live to other
  devices.

## Status

Phase 1: sync across one user's own devices. That covers pairing, recovery phrase, messages,
attachments, edits, deletions, reactions, conversation metadata, streaming, gradual sync,
compaction and point-in-time views.

The permission engine for sharing (roles, invites, removals, agents) is complete in the core and
exposed in the API. MLS group keys and the Osaurus Server binary are later phases.

## Platforms

| Platform | Package | Key storage |
| --- | --- | --- |
| macOS 14+ (Apple silicon) | Swift package, [`bindings/swift`](bindings/swift) | Keychain |
| iOS 17+ (device and simulator) | Swift package, [`bindings/swift`](bindings/swift) | Keychain (this device only) |
| Windows x64 and arm64 | NuGet `Osaurus.Sync`, [`bindings/csharp`](bindings/csharp) | DPAPI |

## Quick look

```swift
import OsaurusSync

let core = try SyncCore.open(dataDirectory: dir, deviceName: "Mac")
let scope = try core.createConversation(title: "Trip plans")
let sub = try core.subscribe(scope: scope) { changes in /* update the UI */ }
_ = try core.sendMessage(scope: scope, message: .text("Book the flights"))

// Pair a second device: show this as a QR code, then on the new device call pairWithOffer.
let offer = try core.pairingOffer(ttlMs: 300_000)
```

```csharp
using Osaurus.Sync;

using var core = OsaurusSync.Open(dir, Environment.MachineName);
var scope = core.CreateConversation("Trip plans");
using var sub = core.Subscribe(scope, changes => { /* update the UI */ });
core.SendMessage(scope, OsaurusSync.Text("Book the flights"));
```

## Documentation

- **[Client guide](docs/client-guide.md):** integrating the library into an app. Covers opening
  the core, reading and writing, subscriptions, streaming, pairing, history, errors and testing.
- **[Swift package](bindings/swift/README.md):** setup for macOS and iOS, Keychain, the app
  lifecycle, backfill policy and the Info.plist keys for local network discovery.
- **[C# package](bindings/csharp/README.md):** setup for Windows, the generated API's naming,
  threading, DPAPI and the firewall prompt.
- **[Architecture](docs/architecture.md):** the data model, envelope format, keys, membership
  rules, storage, merge rules, the sync protocol, networking, compaction and testing.
- **[Development](docs/development.md):** building, testing, the simulation harness,
  benchmarks, compatibility rules, CI and releasing.

## Repository layout

| Path | Contents |
| --- | --- |
| [`crates/osaurus-sync-core`](crates/osaurus-sync-core) | Events, crypto, validation, storage, Loro, views and the sync engine. No I/O |
| [`crates/osaurus-sync-net`](crates/osaurus-sync-net) | Iroh networking: discovery, relays, streams, blobs, pairing |
| [`crates/osaurus-sync-ffi`](crates/osaurus-sync-ffi) | The UniFFI surface the bindings are generated from |
| [`crates/osaurus-sync-sim`](crates/osaurus-sync-sim) | Deterministic simulation harness (tests only) |
| [`bindings/swift`](bindings/swift) | Swift package for macOS and iOS |
| [`bindings/csharp`](bindings/csharp) | .NET 8 package and tests |
| [`scripts`](scripts) | Building the XCFramework, generating bindings, packing NuGet |
| [`tests/fixtures`](tests/fixtures) | Signed events from earlier builds, for compatibility tests |

## Building

```sh
cargo test --workspace                            # core, networking, FFI and simulation tests
scripts/build-xcframework.sh                      # Swift: build the XCFramework and bindings
(cd bindings/swift && swift test)
scripts/gen-csharp.sh                             # C#: build the native library and bindings
dotnet test bindings/csharp/OsaurusSync.Tests
```

The Rust toolchain is pinned in `rust-toolchain.toml`. The C# bindings need
[uniffi-bindgen-cs](https://github.com/NordSecurity/uniffi-bindgen-cs). See
[Development](docs/development.md) for the requirements and the full workflow.
