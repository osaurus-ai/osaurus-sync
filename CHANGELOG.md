# Changelog

Each release lists what changed for the apps that use the library. Versions follow semantic
versioning; `API_VERSION` and `PROTOCOL_VERSION` are listed whenever they change. The release
workflow copies a version's section into its GitHub release notes.

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
