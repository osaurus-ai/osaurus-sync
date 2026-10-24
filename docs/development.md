# Development

How to build, test and release Osaurus Sync.

## Requirements

| Tool | Needed for | Notes |
| --- | --- | --- |
| Rust | Everything | Pinned in `rust-toolchain.toml`, with the Apple targets. `rustup` installs it on first use |
| Xcode 16+ | The Swift package | macOS only. Provides `xcodebuild` and the iOS Simulator |
| .NET 8 SDK | The C# package | `brew install --cask dotnet-sdk` on macOS |
| uniffi-bindgen-cs | Generating C# bindings | `cargo install uniffi-bindgen-cs --git https://github.com/NordSecurity/uniffi-bindgen-cs --tag v0.11.1+v0.31.0` |

Pinned versions that matter:

- **UniFFI is pinned to `=0.31.0`**, because uniffi-bindgen-cs `v0.11.1+v0.31.0` targets that
  release. Upgrade both together.
- **`libc` is held below 0.2.187** in `Cargo.lock`, for the current toolchain.

## Repository layout

```text
crates/
  osaurus-sync-core/    events, crypto, validation, storage, Loro, views, sync engine (no I/O)
  osaurus-sync-net/     Iroh endpoint, discovery, relays, streams, blobs, pairing transport
  osaurus-sync-ffi/     UniFFI surface; builds the native libraries
  osaurus-sync-sim/     deterministic simulation harness (tests only)
bindings/
  swift/                Swift package for macOS and iOS
  csharp/               .NET 8 package and xUnit tests
scripts/                build, binding generation, packaging, simulator helpers
tests/fixtures/         signed events from earlier builds (compatibility)
docs/                   architecture, client guide, this file
```

## Building and testing the core

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

Formatting uses `rustfmt.toml` (160 columns). Run `cargo fmt --all` before committing.

### Simulation

The invariant tests run three users (Alice with a Mac, a PC and a storing server; Bob; Carol)
through partitions, restarts and concurrent membership changes, and check the four invariants
from the [architecture](architecture.md#testing-and-verification) doc after every run.

```sh
cargo test -p osaurus-sync-sim --test invariants                          # seeds 0 to 11
SIM_SEEDS=500 cargo test -p osaurus-sync-sim --release --test invariants  # more seeds
SIM_SEED_START=37 SIM_SEEDS=1 cargo test -p osaurus-sync-sim --test invariants   # replay seed 37
```

Runs are fully deterministic. A failing assertion names its seed, and replaying that seed
reproduces the exact sequence of events. Other simulation tests cover sync engine scenarios
(`engine.rs`) and compaction (`compaction.rs`).

### Compatibility fixtures

`tests/fixtures` holds signed events, including unknown event kinds and a newer envelope version.
`cargo test -p osaurus-sync-sim --test fixtures` checks that every build still decodes, verifies
and applies them.

Only regenerate the fixtures for a deliberate format change, and treat it as a breaking change:

```sh
REGENERATE_FIXTURES=1 cargo test -p osaurus-sync-sim --test fixtures
```

### Benchmarks

```sh
cargo bench -p osaurus-sync-core --bench budgets    # local send, open conversation, cold start
cargo bench -p osaurus-sync-net --bench network     # catch-up of 1,000 events, live delivery
```

Each benchmark prints its median next to its budget. With `OSAURUS_BUDGETS=enforce`, a median over
budget fails the run, which is how CI uses them.

## Swift package

```sh
scripts/build-xcframework.sh                   # release; PROFILE=dev for a faster debug build
cd bindings/swift && swift test                # macOS
```

For the iOS Simulator, either:

```sh
cd bindings/swift
xcodebuild test -scheme OsaurusSync -destination "platform=iOS Simulator,id=$(../../scripts/ios-simulator.sh)"
```

or, on machines where `xcodebuild` can't target the installed runtimes:

```sh
scripts/test-ios-sim.sh
```

`scripts/ios-simulator.sh` prints the UDID of an iPhone simulator on the newest installed iOS
runtime, creating one named `osaurus-sync-ci` if none exists.

The Keychain test is skipped in environments without a usable Keychain, such as the simulator
under `xcodebuild` without a host app.

## C# package

```sh
scripts/gen-csharp.sh                          # builds the native library and generates bindings
dotnet test bindings/csharp/OsaurusSync.Tests
```

The tests run on macOS (osx-arm64) and Windows. The DPAPI test only runs on Windows.

## Changing the exported API

1. Change the Rust surface in `crates/osaurus-sync-ffi`.
2. If the change is incompatible, bump `API_VERSION` in `crates/osaurus-sync-ffi/src/lib.rs`.
   Old bindings will then fail to open with `IncompatibleVersion` instead of misbehaving.
3. Regenerate both bindings: `scripts/build-xcframework.sh` and `scripts/gen-csharp.sh`.
4. Update the helpers (`SyncCore+Swift.swift`, `SyncCoreExtensions.cs`), the binding tests, and
   the [client guide](client-guide.md).

Watch out for C# naming: a record field can't have the same name as its type (`CS0542`), so a
`Reaction` record can't have a field named `reaction`.

Error codes (`SyncError`) and rejection reason codes are stable. Only append new ones; never
renumber.

## Changing the wire format

Envelope layout, event kinds, payload encodings and protocol messages are shared by every
installed device, so changes need care:

- **Add, don't change.** New event kinds and new `Msg` variants go at the end. Unknown kinds are
  stored and forwarded by older builds, so new kinds are safe to introduce.
- **Bump the envelope version** for any change to the signed layout, and keep decoding the old
  one.
- **Bump `PROTOCOL_VERSION`** only for changes older peers can't tolerate. Peers on different
  versions won't sync.
- **Bump `VIEW_VERSION`** when a view table changes. The views are rebuilt from `events` on the
  next open.
- Add a fixture for the new format, and keep the old fixtures passing.

## CI

`.github/workflows/ci.yml` runs on pushes to `main`, on pull requests, and on manual dispatch:

| Job | Runner | What it does |
| --- | --- | --- |
| Rust | Ubuntu | fmt, clippy, workspace tests, 200 simulation seeds in release |
| Performance budgets | Ubuntu | Both benchmark suites with `OSAURUS_BUDGETS=enforce` |
| Swift | macOS 15 | Builds the XCFramework; Swift tests on macOS and the iOS Simulator |
| C# (win-x64) | Windows | Builds the DLL, generates bindings, runs xUnit tests |
| C# (win-arm64) | Windows | Cross-compiles the arm64 DLL (not tested in CI) |
| NuGet package | Ubuntu | Packs all native libraries and the bindings into a `.nupkg` |

Artifacts from each run: `xcframework` (the zipped XCFramework), `osx-arm64`, `win-x64` and
`win-arm64` (native libraries), `csharp-bindings`, and `nuget`.

## Releasing

1. Bump the version in `bindings/csharp/OsaurusSync/OsaurusSync.csproj` (or pass `VERSION` to
   `pack-nuget.sh`), and the crate versions if needed.
2. Push to `main` and wait for CI to pass.
3. Download the `nuget` artifact and publish the `.nupkg` to the package feed.
4. Download the `xcframework` artifact and attach it to a GitHub release, or ship it with the
   apps that use the local package.
