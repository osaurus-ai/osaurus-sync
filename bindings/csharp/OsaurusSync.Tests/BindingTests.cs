using System.Collections.Concurrent;
using Osaurus.Sync;
using Xunit;

namespace Osaurus.Sync.Tests;

public sealed class BindingTests : IDisposable
{
    private readonly string _dir = Path.Combine(Path.GetTempPath(), "osaurus-sync-" + Guid.NewGuid());

    public BindingTests() => Directory.CreateDirectory(_dir);

    public void Dispose()
    {
        try { Directory.Delete(_dir, true); } catch (IOException) { }
    }

    private SyncCore Open(string name, Platform platform = Platform.Windows, bool online = false, KeyStore? keys = null)
    {
        var net = OsaurusSyncFfiMethods.LocalNetworkConfig() with { Enabled = online };
        var config = new CoreConfig(Path.Combine(_dir, name), name, platform, null, net, OsaurusSyncFfiMethods.ApiVersion());
        return SyncCore.Open(config, keys ?? new MemoryKeyStore());
    }

    private static string[] Contents(SyncCore c, string scope) =>
        c.Messages(scope, null, 100).Messages.Select(m => m.Content).ToArray();

    private static void WaitUntil(string what, Func<bool> cond, int timeoutMs = 20_000)
    {
        var deadline = DateTime.UtcNow.AddMilliseconds(timeoutMs);
        while (!cond())
        {
            if (DateTime.UtcNow > deadline) Assert.Fail($"timed out waiting for {what}");
            Thread.Sleep(20);
        }
    }

    [Fact]
    public async Task VersionAndErrors()
    {
        Assert.Equal(1u, OsaurusSyncFfiMethods.ApiVersion());
        var bad = new CoreConfig(Path.Combine(_dir, "v"), "v", Platform.Windows, null, OsaurusSyncFfiMethods.LocalNetworkConfig(), 999);
        var e = Assert.ThrowsAny<SyncException>(() => SyncCore.Open(bad, new MemoryKeyStore()));
        Assert.Equal(SyncErrorCode.IncompatibleVersion, SyncErrorCode.CodeOf(e));

        using var core = Open("a");
        e = Assert.ThrowsAny<SyncException>(() => Open("a"));
        Assert.IsType<SyncException.AlreadyOpen>(e);
        e = Assert.ThrowsAny<SyncException>(() => core.SendMessage("nope", OsaurusSync.Text("x")));
        Assert.Equal(SyncErrorCode.InvalidInput, SyncErrorCode.CodeOf(e));
        await core.Close();
        e = Assert.ThrowsAny<SyncException>(() => core.Conversations());
        Assert.Equal(SyncErrorCode.Closed, SyncErrorCode.CodeOf(e));
    }

    [Fact]
    public async Task LocalApiAndCallbacks()
    {
        using var core = Open("pc");
        var events = new ConcurrentQueue<CoreEvent>();
        core.SetListener(events.Enqueue);
        var convs = new ConcurrentQueue<ConversationChange>();
        using var convSub = core.SubscribeConversations(cs => { foreach (var c in cs) convs.Enqueue(c); });

        var scope = core.CreateConversation("Plans");
        var changes = new ConcurrentQueue<ScopeChange>();
        var threads = new ConcurrentBag<int>();
        using var sub = core.Subscribe(scope, cs =>
        {
            threads.Add(Environment.CurrentManagedThreadId);
            foreach (var c in cs) changes.Enqueue(c);
        });

        var id = core.SendMessage(scope, OsaurusSync.Text("hello world"));
        core.EditMessage(id, "hello there");
        core.AddReaction(id, "👍");
        core.RenameConversation(scope, "Trip plans");
        core.SetPinned(scope, true);
        core.SetArchived(scope, false);
        core.SetSortOrder(scope, 2.5);
        WaitUntil("scope callbacks", () => changes.Count >= 3);
        WaitUntil("conversation callbacks", () => !convs.IsEmpty);
        Assert.Single(threads.Distinct());
        Assert.NotEqual(Environment.CurrentManagedThreadId, threads.First());

        var m = core.Message(id)!;
        Assert.Equal("hello there", m.Content);
        Assert.True(m.Edited);
        Assert.Equal("👍", m.Reactions[0].Value);
        var conv = core.Conversation(scope)!;
        Assert.Equal("Trip plans", conv.Title);
        Assert.True(conv.Pinned);
        Assert.Equal(2.5, conv.SortOrder);
        Assert.Equal(MemberRole.Owner, conv.MyRole);
        Assert.Single(core.Conversations());
        Assert.Equal(id, core.Search("there", 10)[0].Id);
        core.MarkRead(scope, m.Hlc);
        core.RemoveReaction(id, "👍");
        core.DeleteMessage(id);
        Assert.Empty(Contents(core, scope));

        var file = Path.Combine(_dir, "note.txt");
        File.WriteAllText(file, "attachment");
        var withFile = core.SendMessage(scope, OsaurusSync.Text("see file") with { Attachments = [new AttachmentFile(file, "text/plain", "note.txt")] });
        var blob = core.Message(withFile)!.Blobs[0];
        Assert.True(blob.Available);
        Assert.Equal("attachment", File.ReadAllText(await core.FetchAttachment(scope, blob.Hash)));

        using var stream = core.OpenStream(scope, MessageRole.Assistant, null);
        stream.Push("Hel");
        stream.Push("lo");
        var streamed = stream.Finish(null);
        Assert.Equal(stream.MessageId(), streamed);
        Assert.Equal("Hello", core.Message(streamed)!.Content);
        Assert.ThrowsAny<SyncException>(() => stream.Push("late"));
        using var aborted = core.OpenStream(scope, MessageRole.Assistant, null);
        aborted.Push("never");
        aborted.Abort();
        Assert.Null(core.Message(aborted.MessageId()));

        var legacy = core.ImportLegacyConversation("Old", 1_600_000_000_000, [new LegacyMessage(MessageRole.User, "from 2020", 1_600_000_000_000)]);
        Assert.Equal(1_600_000_000_000ul, core.Messages(legacy, null, 10).Messages[0].TimestampMs);

        var notes = core.CreateConversation("Notes");
        var first = core.SendMessage(notes, OsaurusSync.Text("first"));
        var then = core.Message(first)!.TimestampMs;
        Thread.Sleep(5);
        core.RenameConversation(notes, "Renamed notes");
        core.SendMessage(notes, OsaurusSync.Text("second"));
        var past = core.ViewAt(notes, then, 50);
        Assert.True(past.Complete);
        Assert.Equal("Notes", past.Conversation!.Title);
        Assert.Equal(["first"], past.Messages.Select(m => m.Content));
        Assert.Equal(["first", "second"], Contents(core, notes));

        var workspace = core.CreateWorkspace("Team");
        var invite = core.CreateInvite(workspace, MemberRole.Member, 60_000);
        Assert.StartsWith("osaurus-invite:1:", invite);
        using (var other = Open("other"))
        {
            core.Invite(workspace, other.UserKey(), MemberRole.Viewer);
            var principal = "u:" + other.UserKey();
            core.ChangeRole(workspace, principal, MemberRole.Member);
            core.Remove(workspace, principal);
            other.RedeemInvite(invite);
            await other.Close();
        }

        Assert.True(core.AssignAgent(workspace, OsaurusSyncFfiMethods.GenerateAgentSecret(), null, MemberRole.Member, 86_400_000).Length > 60);
        Assert.NotNull(core.PublishAgentRecord(OsaurusSyncFfiMethods.GenerateAgentSecret(), 60_000));
        Assert.Null(core.PublishAgentRecord(null, 0));

        Assert.Equal(24, core.RecoveryPhrase().Length);
        Assert.Equal(core.DeviceKey(), Assert.Single(core.Devices()).Id);
        core.SetHistoryMode(null, new HistoryMode.Recent(30));
        core.SetHistoryMode(scope, new HistoryMode.Full());
        core.RequestHistory(scope, ulong.MaxValue, 30);
        core.SetBackfillPolicy(BackfillPolicy.Paused);
        Assert.True(core.Diagnostics().BackfillPaused);
        core.SetBackfillPolicy(BackfillPolicy.Run);
        Assert.Equal(scope, core.SyncStatus(scope).Scope);
        Assert.False(core.IsOnline());
        Assert.Null(core.LocalAddress());

        var offer = core.PairingOffer(60_000);
        Assert.Contains(offer.Code, offer.Payload);
        core.CancelPairing();
        Assert.ThrowsAny<SyncException>(() => core.ApproveDevice(string.Concat(Enumerable.Repeat("ab", 32))));
        Assert.ThrowsAny<SyncException>(() => core.RevokeDevice(core.DeviceKey()));

        await core.Suspend();
        await core.Resume();
        Assert.True(core.IsOnline());
        Assert.NotNull(core.LocalAddress());
        core.SyncNow();
        await core.Close();
    }

    [Fact]
    public async Task PairingSyncAndStreamsOverLoopback()
    {
        using var pc = Open("pc", online: true);
        using var laptop = Open("laptop", online: true);
        var events = new ConcurrentQueue<CoreEvent>();
        laptop.SetListener(events.Enqueue);

        var scope = pc.CreateConversation("Shared");
        pc.SendMessage(scope, OsaurusSync.Text("before pairing"));
        pc.AddPeerAddress(laptop.LocalAddress()!);
        laptop.AddPeerAddress(pc.LocalAddress()!);

        var offer = pc.PairingOffer(300_000);
        await laptop.PairWithOffer(offer.Payload);
        WaitUntil("paired", () => events.Any(e => e is CoreEvent.Paired));
        WaitUntil("history", () => Contents(laptop, scope).SequenceEqual(["before pairing"]));
        Assert.Equal(pc.UserKey(), laptop.UserKey());
        Assert.Equal(2, laptop.Devices().Length);

        var changes = new ConcurrentQueue<ScopeChange>();
        using var sub = laptop.Subscribe(scope, cs => { foreach (var c in cs) changes.Enqueue(c); });
        using var stream = pc.OpenStream(scope, MessageRole.Assistant, null);
        stream.Push("Thinking");
        WaitUntil("stream tokens", () => changes.Any(c => c is ScopeChange.Stream s && s.Update.Text == "Thinking" && s.Update.State == StreamState.Streaming));
        stream.Finish("Done");
        WaitUntil("committed", () => Contents(laptop, scope).Contains("Done"));

        var diag = pc.Diagnostics();
        Assert.True(diag.Running);
        Assert.Contains(diag.Peers, p => p.Device == laptop.DeviceKey() && p.Connected && !p.Relayed);
        WaitUntil("acknowledged", () => pc.SyncStatus(scope).PendingOutbox == 0);

        await laptop.Suspend();
        laptop.SendMessage(scope, OsaurusSync.Text("while suspended"));
        Assert.True(laptop.SyncStatus(scope).PendingOutbox > 0);
        await laptop.Resume();
        laptop.AddPeerAddress(pc.LocalAddress()!);
        pc.AddPeerAddress(laptop.LocalAddress()!);
        laptop.SyncNow();
        WaitUntil("catch-up after resume", () => Contents(pc, scope).Contains("while suspended"));

        await pc.Close();
        await laptop.Close();
    }

    [Fact]
    public async Task LogsAndSupportBundle()
    {
        var records = new ConcurrentQueue<(LogRecord, int)>();
        var me = Environment.CurrentManagedThreadId;
        SyncLogging.SetHandler(r => records.Enqueue((r, Environment.CurrentManagedThreadId)), LogLevel.Info);
        try
        {
            using var core = Open("logs", online: true);
            WaitUntil("networking log", () => records.Any(r => r.Item1.Message == "networking started"));
            Assert.All(records, r => Assert.NotEqual(me, r.Item2));

            var bundle = core.SupportBundle();
            using var json = System.Text.Json.JsonDocument.Parse(bundle);
            Assert.True(json.RootElement.GetProperty("counters").GetProperty("sessions_established").GetUInt64() >= 0);
            Assert.DoesNotContain(core.DeviceKey()[..12], bundle);
            Assert.Contains("networking started", bundle);
            await core.Close();
        }
        finally
        {
            SyncLogging.Disable();
        }
    }

    [Fact]
    public async Task DataManagement()
    {
        var keys = new MemoryKeyStore();
        var core = Open("data", keys: keys);
        var keep = core.CreateConversation("Keep");
        var file = Path.Combine(_dir, "notes.txt");
        File.WriteAllText(file, "attached");
        core.SendMessage(keep, OsaurusSync.Text("hello") with { Attachments = [new AttachmentFile(file, "text/plain", "notes.txt")] });

        var usage = core.StorageUsage();
        Assert.True(usage.DatabaseBytes > 0);
        Assert.True(usage.AttachmentBytes >= 8);

        var outDir = Path.Combine(_dir, "export");
        var summary = await core.ExportConversation(keep, outDir);
        Assert.Equal(new ExportSummary(1, 1, 0), summary);
        Assert.True(File.Exists(Path.Combine(outDir, "conversation.json")));

        var gone = core.CreateConversation("Leave");
        core.LeaveConversation(gone);
        Assert.Equal([keep], core.Conversations().Select(c => c.Scope));

        Assert.NotNull(keys.Load("device-key"));
        await core.Reset();
        Assert.Null(keys.Load("device-key"));
        Assert.False(Directory.Exists(Path.Combine(_dir, "data")));
        var e = Assert.ThrowsAny<SyncException>(() => core.Conversations());
        Assert.Equal(SyncErrorCode.Closed, SyncErrorCode.CodeOf(e));
        core.Dispose();

        using var fresh = Open("data", keys: keys);
        Assert.Empty(fresh.Conversations());
        await fresh.Close();
    }

    [Fact]
    public void DpapiKeyStoreRoundTrip()
    {
        if (!OperatingSystem.IsWindows()) return;
        var store = new DpapiKeyStore(Path.Combine(_dir, "keys"));
        Assert.Null(store.Load("k"));
        store.Store("k", [1, 2, 3]);
        Assert.Equal(new byte[] { 1, 2, 3 }, store.Load("k"));
        store.Store("k", [4]);
        Assert.Equal(new byte[] { 4 }, store.Load("k"));
        store.Delete("k");
        Assert.Null(store.Load("k"));

        using var core = SyncCore.Open(
            new CoreConfig(Path.Combine(_dir, "dpapi"), "dpapi", Platform.Windows, null, OsaurusSyncFfiMethods.LocalNetworkConfig() with { Enabled = false }, OsaurusSyncFfiMethods.ApiVersion()),
            store);
        Assert.NotNull(store.Load("device-key"));
    }
}
