# Development

How to build, test and release Osaurus Sync.

## Requirements

| Tool | Needed for | Notes |
| --- | --- | --- |
| Rust | Everything | Pinned in `rust-toolchain.toml`, with the Apple targets. `rustup` installs it on first use |
| Xcode 16+ | The Swift package | macOS only. Provides `xcodebuild` and the iOS Simulator |
| .NET 8 SDK | The C# package | `brew install --cask dotnet-sdk` on macOS |
| uniffi-bindgen-cs | Generating C# bindings | `cargo install uniffi-bindgen-cs --git https://github.com/NordSecurity/uniffi-bindgen-cs --tag v0.11.1+v0.31.0` |
| cargo-deny, cargo-about, cargo-cyclonedx | Dependency checks, notices, SBOMs | Optional locally; see [Dependencies and licenses](#dependencies-and-licenses) |
| Nightly Rust and cargo-fuzz | Fuzzing | Optional locally; see [Fuzzing](#fuzzing) |

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
fuzz/                   cargo-fuzz targets (separate crate, nightly)
scripts/                build, binding generation, packaging, notices, SBOM, fuzzing, simulator helpers
tests/fixtures/         signed events from earlier builds (compatibility)
docs/                   architecture, client guide, threat model, this file
deny.toml, about.toml   dependency policy and the notices configuration
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

`crates/osaurus-sync-core/tests/fixtures/store-v1` is a data directory written by the last
commit at store version 1, with its key store entries. `tests/upgrade.rs` opens it with the
current code and checks that conversations, edits, reactions and attachments survive the
migrations. When you add a migration (see `store.rs`), first freeze the outgoing version: check
out the last commit at that version and run

```sh
OSAURUS_FIXTURE_OUT=/tmp/store-vN cargo test -p osaurus-sync-core --test upgrade write_fixture -- --ignored
```

then copy `/tmp/store-vN` (without `data/.lock`) to `tests/fixtures/store-vN` and add a test for
it next to `opens_a_version_1_database`.

### Scale tests

```sh
cargo test -p osaurus-sync-sim --release --test scale -- --ignored --nocapture --test-threads=1
OSAURUS_SCALE=0.05 cargo test -p osaurus-sync-sim --release --test scale -- --ignored --nocapture
```

`scale.rs` syncs 100,000 messages in 1,000 conversations to 10 devices and stores 1 GB of
attachments, printing write and sync times, memory, time to first view after a restart, search
time and disk use. The assertions are loose (first view under 5 seconds, the attachment cache
under its cap); the numbers are for watching trends in the nightly job summary. The full run
takes about 10 minutes; `OSAURUS_SCALE` shrinks every count.

### Benchmarks

```sh
cargo bench -p osaurus-sync-core --bench budgets    # local send, open conversation, cold start
cargo bench -p osaurus-sync-net --bench network     # catch-up, live delivery, send during catch-up
```

Each benchmark prints its median next to its budget. With `OSAURUS_BUDGETS=enforce`, a median over
budget fails the run. `OSAURUS_BUDGET_SLACK` multiplies every budget: push CI uses 3, since shared
runners are noisy, and the nightly workflow enforces the budgets themselves.

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

Until the first tagged release, `API_VERSION` and `PROTOCOL_VERSION` stay at 1 and
incompatible changes don't bump them, since no installed build needs to interoperate. From the
first release on, the rules above apply.

## Fuzzing

`fuzz/` is a separate cargo-fuzz crate (it needs nightly) with a target for each decoder of
untrusted input:

| Target | Input |
| --- | --- |
| `envelope` | Event envelopes: decode, re-encode, verify, purge |
| `msg` | Sync protocol messages, and slicing of `Events` |
| `stream_frame` | Ephemeral stream frames |
| `payload` | Control and content payloads of every kind, certificates and agent bindings |
| `loro_update` | Loro updates imported into a document with local state |
| `recon` | `recon::respond` against hostile ranges |
| `invite` | Invite links and pairing codes |

```sh
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz
scripts/fuzz.sh 60                 # every target for 60 seconds
scripts/fuzz.sh 600 loro_update    # one target for longer
cargo +nightly fuzz run envelope fuzz/artifacts/envelope/crash-...   # reproduce a crash
```

A prebuilt `cargo-fuzz` on Linux targets musl by default, which the sanitizers don't support.
Pass `--target x86_64-unknown-linux-gnu` when running `cargo fuzz` by hand there; the script
already passes the host triple.

The envelope corpus is seeded with `tests/fixtures`. Add a fuzz target with any new decoder of
data from peers, and turn every crash into a regular unit test when fixing it. The targets use
`osaurus-sync-core`'s `fuzzing` feature for internals such as Loro import; it isn't part of the
API.

## Dependencies and licenses

`deny.toml` configures `cargo deny check`, which runs in CI on every push and checks:

- **Licenses.** Only the licenses in `deny.toml` are allowed: permissive ones plus MPL-2.0
  (uniffi, and `im` through loro), whose file-level copyleft is met by shipping the notices
  unmodified. A new dependency under any other license fails CI; don't add it to the allow
  list without checking with whoever owns licensing.
- **Advisories.** RustSec advisories against `Cargo.lock`. The nightly workflow also runs this,
  since advisories land without a push. To ignore one, add its id to `[advisories] ignore` with
  a comment saying why it doesn't apply, and remove it once a fixed version is out.
- **Bans and sources.** No `openssl-sys` or `native-tls` (everything uses rustls), no wildcard
  versions, and crates only from crates.io.

The graph is limited to the shipped targets, so Linux-only and wasm dependencies don't count.

Releases also ship:

- **`THIRD_PARTY_NOTICES.md`**, from `scripts/gen-notices.sh` (cargo-about with `about.toml`).
  It covers the crates compiled into the native library and is included in the NuGet package,
  the Swift package and the GitHub release. It isn't checked in; run the script to see it.
- **A CycloneDX 1.5 SBOM per target**, from `scripts/gen-sbom.sh` (cargo-cyclonedx), attached
  to the GitHub release as `osaurus-sync-ffi_<target>.cdx.json`.

Install the tools with `cargo install cargo-deny cargo-about cargo-cyclonedx` (or Homebrew).

The repository's own code is proprietary for now (`LICENSE`, and `license = "UNLICENSED"` in
`Cargo.toml`). If that changes, update both, and the NuGet package picks up the new `LICENSE`.

## Device validation

CI covers the logic on simulators and hosted runners, but not hardware, permissions, power
management or real networks. Before each release, run these checks on physical devices with a
build of the release candidate and record the results (device, OS version, build, pass or fail,
notes) in the release's pull request. Attach a `supportBundle()` from any failing step.

Use two devices signed in to the same identity, plus a Mac on a different network for the relay
checks. The app under test should use the production relay and discovery configuration, not
`n0PublicNetworkConfig()`.

### iOS

Run on the oldest supported iPhone and a current one.

| Check | Steps | Expected |
| --- | --- | --- |
| Keychain after reboot | Pair the phone, reboot it, and launch the app before unlocking (for example from a notification or a background launch), then again after unlocking | Before the first unlock, open fails with `KeyStore`; the app retries after unlock. After the first unlock, the same identity opens and `deviceKey()` is unchanged |
| Keychain is device only | Restore the phone from an encrypted backup onto a second phone | The second phone has no sync identity and must pair as a new device |
| Suspend and resume | Start a long sync, press Home, wait 30 seconds, return | `suspend` finishes within the background task; no crash report or watchdog kill. On return the sync resumes and completes |
| Writes while suspended | Background the app, send a message from the Mac, foreground the phone | The message arrives within a few seconds of `resume` |
| Long suspension | Leave the app in the background overnight | It resumes, reconnects and catches up without a fresh pairing |
| Local network prompt | Fresh install, open the app | The local network prompt appears once. With Allow, LAN devices connect directly (`diagnostics()` shows `relayed == false`) |
| Local network denied | Fresh install, choose Don't Allow | The app sets `mdns = false`; sync still works through relays; no repeated prompts. Re-enabling in Settings restores direct LAN connections after relaunch |
| Cellular | Turn off Wi-Fi | Connections are relayed and sync completes. Switching between Wi-Fi and cellular mid-sync reconnects without user action |
| Low Data Mode | Turn on Low Data Mode for the network | `BackfillPolicyMonitor` pauses backfill; new messages still sync; backfill resumes when it's turned off |
| Low Power Mode | Turn on Low Power Mode | Backfill pauses as above; foreground sync keeps working |
| Pairing by QR | Pair a new phone from the Mac's QR code, then deny camera access and pair with the typed code | Both paths succeed; a wrong code fails with `Pairing` and can be retried |
| Storage pressure | Fill the phone to under 1 GB free and sync attachments | The attachment cache shrinks to its limit; no crash; `storageUsage()` reflects it |
| Reset | Sign out from settings | `reset()` removes the data directory and Keychain items; the app returns to onboarding |

### Windows arm64

Run on a Windows 11 arm64 device (for example a Snapdragon laptop), with the NuGet package
from the release candidate, not a locally built DLL.

| Check | Steps | Expected |
| --- | --- | --- |
| Native load | Install the package in a clean arm64 app and call `ApiVersion()` | The `win-arm64` DLL loads with no x64 emulation (Task Manager shows the process as Arm64) |
| Signature | Check the DLL's properties, or run `Get-AuthenticodeSignature` | Signed by the Osaurus certificate, valid |
| Firewall prompt | First launch on a private network | One Windows Firewall prompt for UDP. With Allow, LAN peers connect directly |
| Firewall denied | Fresh install, cancel the prompt | Sync works through relays; diagnostics show the connections as relayed |
| DPAPI | Pair, sign out of Windows and back in, then reboot | The identity reopens. Copying `%LOCALAPPDATA%\Osaurus\Sync` to another account or machine doesn't: open fails with `KeyStore` |
| Sleep and resume | Sleep the laptop for 10 minutes during a sync | After waking, the core reconnects and completes without a restart |
| Network change | Switch between Wi-Fi and a phone hotspot | Reconnects and keeps syncing |
| Battery saver | Turn on battery saver | Sync continues; no excess CPU use while idle (under 1% in Task Manager) |
| Pairing with the Mac | Pair the Windows device with a Mac and an iPhone | All three converge on the same conversations, including attachments |
| Reset | Sign out from the app | `Reset()` removes the data directory and the DPAPI key files |

Repeat the Windows table on x64 when the release changes native code, networking or the key
store.

## CI

`.github/workflows/ci.yml` runs on pushes to `main`, on pull requests, and on manual dispatch:

| Job | Runner | What it does |
| --- | --- | --- |
| Rust | Ubuntu | fmt, clippy, workspace tests, 200 simulation seeds in release, and a check that the fuzz targets compile |
| Dependencies | Ubuntu | `cargo deny check`: licenses, advisories, bans and sources |
| Performance budgets | Ubuntu | Both benchmark suites, enforced at three times the budgets |
| Swift | macOS 15 | Builds the XCFramework; Swift tests on macOS and the iOS Simulator |
| C# (win-x64) | Windows x64 | Builds the DLL, generates bindings, runs xUnit tests |
| C# (win-arm64) | Windows 11 arm64 | The same, natively on arm64 |
| NuGet package | Ubuntu | Packs all native libraries and the bindings into a `.nupkg` |

`.github/workflows/nightly.yml` runs every night and on manual dispatch:

| Job | What it does |
| --- | --- |
| Performance budgets (strict) | Both benchmark suites against the budgets themselves, with longer measurement |
| Simulation | 2,000 simulation seeds in release |
| Scale | The scale tests at full size; metrics go to the job summary |
| Security advisories | `cargo deny check advisories` against the latest RustSec database |
| Fuzzing | Every fuzz target for two minutes, with the corpus kept between nights; crashes are uploaded as `fuzz-crashes` |

A strict budget failure on a shared runner is worth one re-run before treating it as a regression.

Artifacts from each run: `xcframework` (the zipped XCFramework), `osx-arm64`, `win-x64` and
`win-arm64` (native libraries), `csharp-bindings`, and `nuget`.

## Releasing

Releases are cut by pushing a tag. `.github/workflows/release.yml` then:

1. Builds the XCFramework in release, runs the Swift tests, zips it and computes its SwiftPM
   checksum.
2. Writes the distributable Swift package (`scripts/make-swift-package.sh`): the sources with
   the generated bindings, and a `binaryTarget(url:checksum:)` pointing at the release asset.
3. Builds the `win-x64` and `win-arm64` DLLs on native runners, runs the xUnit tests on each,
   and signs both with Authenticode (`signtool`, SHA-256, RFC 3161 timestamp), verifying the
   signatures.
4. Packs `Osaurus.Sync` with the tag's version and both signed DLLs plus `osx-arm64`.
5. Creates the GitHub release with the XCFramework zip, its checksum, the Swift package zip and
   the `.nupkg`; pushes the package to NuGet; and, if configured, commits the Swift package to
   the distribution repository and tags it with the bare version (SwiftPM ignores a `v`
   prefix).

To cut a release:

1. Check that CI and the latest nightly passed on the commit, and run the
   [device validation](#device-validation) checklists on its build.
2. Update the crate versions in `Cargo.toml` if they changed, and note breaking API or wire
   changes (`API_VERSION`, protocol version) in the release notes.
3. Tag and push: `git tag v0.2.0 && git push origin v0.2.0`. Use a suffix such as
   `v0.2.0-beta.1` for a prerelease; the GitHub release is marked as one.

Run the workflow by hand (**Actions > Release > Run workflow**) for a dry run: it builds,
tests and packs everything as workflow artifacts, signs only if the certificate is configured,
and publishes nothing.

The publishing jobs run in the `release` environment, so it can require approval. They need:

| Name | Kind | Purpose |
| --- | --- | --- |
| `WINDOWS_SIGNING_CERT` | Secret | Base64 of the code-signing `.pfx`. Required for a tagged release |
| `WINDOWS_SIGNING_PASSWORD` | Secret | The `.pfx` password |
| `NUGET_API_KEY` | Secret | Push key for `Osaurus.Sync` |
| `NUGET_SOURCE` | Variable | Optional feed URL; defaults to nuget.org |
| `SWIFT_DIST_REPO` | Variable | Optional `owner/name` of the Swift distribution repository, such as `osaurus-ai/osaurus-sync-swift` |
| `SWIFT_DIST_TOKEN` | Secret | A token that can push to that repository |

If the signing key lives in an HSM or a cloud signing service rather than a `.pfx`, replace the
Authenticode step with that service's signing action; the rest of the pipeline is unchanged.
