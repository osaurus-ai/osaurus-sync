using System;
using System.Collections.Generic;

namespace Osaurus.Sync;

/// <summary>Stable numeric codes carried by every <see cref="SyncException"/> variant.</summary>
public static class SyncErrorCode
{
    public const uint Storage = 100;
    public const uint InvalidInput = 101;
    public const uint NotFound = 102;
    public const uint PermissionDenied = 103;
    public const uint Crypto = 104;
    public const uint Malformed = 105;
    public const uint AlreadyOpen = 106;
    public const uint DeviceRevoked = 107;
    public const uint NoKey = 108;
    public const uint Network = 109;
    public const uint IncompatibleVersion = 110;
    public const uint Pairing = 111;
    public const uint Closed = 112;
    public const uint KeyStore = 113;
    public const uint Corrupt = 114;
    public const uint NewerDatabase = 115;
    public const uint Internal = 199;

    /// <summary>The stable code of any sync exception.</summary>
    public static uint CodeOf(SyncException e) => e switch
    {
        SyncException.Storage x => x.code,
        SyncException.InvalidInput x => x.code,
        SyncException.NotFound x => x.code,
        SyncException.PermissionDenied x => x.code,
        SyncException.Crypto x => x.code,
        SyncException.Malformed x => x.code,
        SyncException.AlreadyOpen x => x.code,
        SyncException.DeviceRevoked x => x.code,
        SyncException.NoKey x => x.code,
        SyncException.Network x => x.code,
        SyncException.IncompatibleVersion x => x.code,
        SyncException.Pairing x => x.code,
        SyncException.Closed x => x.code,
        SyncException.KeyStore x => x.code,
        SyncException.Internal x => x.code,
        SyncException.Corrupt x => x.code,
        SyncException.NewerDatabase x => x.code,
        _ => Internal,
    };
}

public static class OsaurusSync
{
    /// <summary>
    /// Opens the core with the bindings' API version filled in. On Windows the default key store
    /// is <see cref="DpapiKeyStore"/>.
    /// </summary>
    public static SyncCore Open(string dataDirectory, string deviceName, KeyStore? keyStore = null, NetworkConfig? network = null, HistoryMode? history = null)
    {
        var platform = OperatingSystem.IsWindows() ? Platform.Windows : OperatingSystem.IsMacOS() ? Platform.MacOs : Platform.Other;
        if (keyStore is null)
        {
            if (!OperatingSystem.IsWindows())
                throw new ArgumentNullException(nameof(keyStore), "a key store is required on this platform");
            keyStore = new DpapiKeyStore();
        }
        var config = new CoreConfig(dataDirectory, deviceName, platform, history, network ?? OsaurusSyncFfiMethods.DefaultNetworkConfig(), OsaurusSyncFfiMethods.ApiVersion());
        return SyncCore.Open(config, keyStore);
    }

    public static OutgoingMessage Text(string content, MessageRole role = MessageRole.User) =>
        new(role, content, Array.Empty<AttachmentFile>(), null, null, null, null);

    public static Subscription Subscribe(this SyncCore core, string scope, Action<ScopeChange[]> onChanges) =>
        core.Subscribe(scope, new ScopeCallback(onChanges));

    public static Subscription SubscribeConversations(this SyncCore core, Action<ConversationChange[]> onChanges) =>
        core.SubscribeConversations(new ConversationCallback(onChanges));

    public static void SetListener(this SyncCore core, Action<CoreEvent> onEvent) => core.SetListener(new CoreCallback(onEvent));

    private sealed class ScopeCallback(Action<ScopeChange[]> f) : ScopeListener
    {
        public void OnChanges(ScopeChange[] changes) => f(changes);
    }

    private sealed class ConversationCallback(Action<ConversationChange[]> f) : ConversationListener
    {
        public void OnChanges(ConversationChange[] changes) => f(changes);
    }

    private sealed class CoreCallback(Action<CoreEvent> f) : CoreListener
    {
        public void OnEvent(CoreEvent e) => f(e);
    }
}
