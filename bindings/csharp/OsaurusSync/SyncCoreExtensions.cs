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
    public static SyncCore Open(string dataDirectory, string deviceName, KeyStore? keyStore = null, NetworkConfig? network = null, HistoryMode? history = null, uint? maxAppDataBytes = null)
    {
        var platform = OperatingSystem.IsWindows() ? Platform.Windows : OperatingSystem.IsMacOS() ? Platform.MacOs : Platform.Other;
        if (keyStore is null)
        {
            if (!OperatingSystem.IsWindows())
                throw new ArgumentNullException(nameof(keyStore), "a key store is required on this platform");
            keyStore = new DpapiKeyStore();
        }
        var config = new CoreConfig(dataDirectory, deviceName, platform, history, network ?? OsaurusSyncFfiMethods.DefaultNetworkConfig(), maxAppDataBytes, OsaurusSyncFfiMethods.ApiVersion());
        return SyncCore.Open(config, keyStore);
    }

    public static OutgoingMessage Text(string content, MessageRole role = MessageRole.User) =>
        new(role, content, Array.Empty<AttachmentFile>(), null, null, null, null, Array.Empty<AppDataWrite>());

    public static LegacyMessage LegacyText(string content, ulong createdMs, MessageRole role = MessageRole.User) =>
        new(role, content, createdMs, Array.Empty<AttachmentFile>(), null, null, Array.Empty<AppDataWrite>());

    public static LegacyConversation Legacy(string title, ulong createdMs, Replication replication = Replication.Synced) =>
        new(title, createdMs, replication, false, false, Array.Empty<Tag>(), Array.Empty<AppDataEntry>());

    /// <summary>
    /// A conversation filter. Use this rather than the generated record's defaults, which leave
    /// the list fields null.
    /// </summary>
    public static ConversationFilter Filter(
        IDictionary<string, string>? tags = null, string[]? hasTags = null, ScopeKind? kind = null, Replication? replication = null,
        bool? archived = null, bool? pinned = null, uint offset = 0, uint limit = 0) =>
        new(ToTags(tags), hasTags ?? Array.Empty<string>(), kind, replication, archived, pinned, offset, limit);

    public static string CreateConversation(this SyncCore core, string title) => core.CreateConversation(title, Replication.Synced);

    public static Conversation[] Conversations(this SyncCore core, IDictionary<string, string> tagged) => core.ConversationsFiltered(Filter(tagged));

    public static string Finish(this OutgoingStream stream, string? content = null) => stream.Finish(content, Array.Empty<AppDataWrite>());

    public static string? GetTag(this Conversation c, string key) => Array.Find(c.Tags, t => t.Key == key)?.Value;

    public static byte[]? GetAppData(this Conversation c, string ns) => Array.Find(c.AppData, e => e.Namespace == ns)?.Bytes;

    public static AppData? GetAppData(this Message m, string ns) => Array.Find(m.AppData, e => e.Namespace == ns);

    private static Tag[] ToTags(IDictionary<string, string>? tags)
    {
        if (tags is null) return Array.Empty<Tag>();
        var list = new List<Tag>();
        foreach (var (k, v) in tags) list.Add(new Tag(k, v));
        return list.ToArray();
    }

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
