# Osaurus.Sync

.NET 8 bindings for the Osaurus sync core. The package ships native libraries for
`win-x64`, `win-arm64` and `osx-arm64` under `runtimes/<rid>/native`. NuGet picks the
right one at build time.

```csharp
using Osaurus.Sync;

var core = OsaurusSync.Open(
    dataDirectory: Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Osaurus", "Sync"),
    deviceName: Environment.MachineName,
    keyStore: new DpapiKeyStore());

var scope = core.CreateConversation("Plans");
using var sub = core.Subscribe(scope, changes => { /* runs on the core's callback thread */ });
core.SendMessage(scope, OsaurusSync.Text("hello"));
```

## Threading

Calls are synchronous and cheap (local SQLite), except the async methods that wait on
the network: `PairWithOffer`, `FetchAttachment`, `ResolveAgent`, `Suspend`, `Resume`
and `Close`. Listener callbacks run on one dedicated thread, in order. Marshal them to
the UI thread yourself, for example with `Dispatcher.InvokeAsync`.

## Keys

`DpapiKeyStore` keeps the device key, user key and scope keys under
`%LOCALAPPDATA%\Osaurus\Sync\keys`. Each key is encrypted with DPAPI (`CurrentUser`
scope), so only this Windows user on this machine can decrypt it. Use `MemoryKeyStore`
only in tests.

## Errors

Every failure throws a subclass of `SyncException`. `SyncErrorCode.CodeOf(e)` returns
the stable numeric code, which matches the Swift binding.

## Networking

Discovery uses mDNS on the LAN, DNS for known devices and relays as a fallback. The
first launch may show a Windows Firewall prompt for UDP on private networks. Accept it
so that devices on the same LAN can connect directly.
