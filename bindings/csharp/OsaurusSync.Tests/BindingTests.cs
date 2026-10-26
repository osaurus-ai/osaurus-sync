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
        var config = new CoreConfig(Path.Combine(_dir, name), name, platform, null, net, null, OsaurusSyncFfiMethods.ApiVersion());
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
        Assert.Equal(2u, OsaurusSyncFfiMethods.ApiVersion());
        var bad = new CoreConfig(Path.Combine(_dir, "v"), "v", Platform.Windows, null, OsaurusSyncFfiMethods.LocalNetworkConfig(), null, 999);
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

        var legacy = core.ImportLegacyConversation(OsaurusSync.Legacy("Old", 1_600_000_000_000), [OsaurusSync.LegacyText("from 2020", 1_600_000_000_000)]);
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

        var agentSecret = OsaurusSyncFfiMethods.GenerateAgentSecret();
        var agent = core.AssignAgent(workspace, agentSecret, null, MemberRole.Member, 86_400_000);
        Assert.True(agent.Length > 60);
        Assert.NotNull(core.PublishAgentRecord(OsaurusSyncFfiMethods.GenerateAgentSecret(), 60_000));
        Assert.Null(core.PublishAgentRecord(null, 0));
        var address = OsaurusSyncFfiMethods.AgentAddress(agent);
        Assert.Matches("^0x[0-9a-f]{40}$", address);
        var hosted = Assert.Single(core.Agents(workspace));
        Assert.Equal(address, hosted.Address);
        Assert.True(hosted.HostedHere);
        var card = core.AgentCard(agentSecret, 86_400_000);
        var verified = OsaurusSyncFfiMethods.VerifyAgentCard(card, "0x" + address[2..].ToUpperInvariant());
        Assert.Equal(core.DeviceKey(), verified.Host);
        Assert.Equal(core.UserKey(), verified.User);
        var wrongAddress = Assert.ThrowsAny<SyncException>(() => OsaurusSyncFfiMethods.VerifyAgentCard(card, "0x" + new string('0', 40)));
        Assert.Equal(SyncErrorCode.Crypto, SyncErrorCode.CodeOf(wrongAddress));
        var expired = core.AgentCard(agentSecret, 0);
        var expiredError = Assert.ThrowsAny<SyncException>(() => OsaurusSyncFfiMethods.VerifyAgentCard(expired, null));
        Assert.Equal(SyncErrorCode.Crypto, SyncErrorCode.CodeOf(expiredError));
        Assert.StartsWith("osaurus-invite:1:", core.CreateInviteFor(workspace, verified.User, MemberRole.Member, 60_000));

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

    private static byte[] Utf8(string s) => System.Text.Encoding.UTF8.GetBytes(s);

    [Fact]
    public async Task AppDataTagsAndLocalConversations()
    {
        using var core = Open("app");
        var scope = core.CreateConversation("Chat");
        core.SetConversationTags(scope, [new Tag("source", "chat")]);
        core.SetConversationAppData(scope, "ai.osaurus.session/1", Utf8("{\"v\":1}"));

        var file = Path.Combine(_dir, "result.json");
        File.WriteAllText(file, "\"big\"");
        var id = core.SendMessage(scope, OsaurusSync.Text("hi") with
        {
            AppData = [new AppDataWrite("ai.osaurus.turn/1", Utf8("{\"v\":1,\"id\":\"a\"}"), [new AttachmentFile(file, "application/json", "toolResults.c1")])],
        });
        var turn = core.Message(id)!.GetAppData("ai.osaurus.turn/1")!;
        Assert.Equal(Utf8("{\"v\":1,\"id\":\"a\"}"), turn.Bytes);
        Assert.Equal(Utf8("\"big\""), await core.ReadAttachment(scope, turn.Blobs[0].Hash));
        core.SetMessageAppData(id, [new AppDataWrite("ai.osaurus.turn/1", Utf8("{\"v\":1,\"id\":\"b\"}"), [])]);
        Assert.Empty(core.Message(id)!.GetAppData("ai.osaurus.turn/1")!.Blobs);

        using var stream = core.OpenStream(scope, MessageRole.Assistant, null);
        stream.Push("answer");
        var streamed = stream.Finish(null, [new AppDataWrite("ai.osaurus.turn/1", Utf8("{\"v\":1}"), [])]);
        Assert.NotNull(core.Message(streamed)!.GetAppData("ai.osaurus.turn/1"));

        var conv = core.Conversation(scope)!;
        Assert.Equal("chat", conv.GetTag("source"));
        Assert.Equal(Utf8("{\"v\":1}"), conv.GetAppData("ai.osaurus.session/1"));
        Assert.Equal(2u, conv.MessageCount);
        Assert.Equal(Replication.Synced, conv.Replication);

        var local = core.CreateConversation("Plugin run", Replication.DeviceLocal);
        core.SetConversationTags(local, [new Tag("source", "plugin")]);
        Assert.Equal([local], core.Conversations(new Dictionary<string, string> { ["source"] = "plugin" }).Select(c => c.Scope));
        Assert.Equal([local], core.ConversationsFiltered(OsaurusSync.Filter(replication: Replication.DeviceLocal)).Select(c => c.Scope));
        Assert.Single(core.ConversationsFiltered(OsaurusSync.Filter(hasTags: ["source"], limit: 1)));
        core.SendMessage(local, OsaurusSync.Text("headless"));
        Assert.Empty(core.SearchFiltered("headless", OsaurusSync.Filter(replication: Replication.Synced), 10));
        core.PromoteToSynced(local);
        Assert.Equal(Replication.Synced, core.Conversation(local)!.Replication);
        var gone = core.CreateConversation("Scratch", Replication.DeviceLocal);
        core.DeleteLocalConversation(gone);
        Assert.Null(core.Conversation(gone));

        await core.SignOut(true);
        var e = Assert.ThrowsAny<SyncException>(() => core.Conversations());
        Assert.Equal(SyncErrorCode.Closed, SyncErrorCode.CodeOf(e));
    }

    /// <summary>Imports the shared fixtures in tests/fixtures/app-data, as the Rust and Swift tests do.</summary>
    [Fact]
    public async Task AppDataFixtures()
    {
        var root = FixtureDir();
        var names = Directory.GetFiles(root, "*.json").OrderBy(n => n, StringComparer.Ordinal).ToArray();
        Assert.Equal(6, names.Length);
        using var core = Open("fixtures");
        var n = 0;
        AttachmentFile[] Files(System.Text.Json.Nodes.JsonNode? list) =>
            list is null ? [] : list.AsArray().Select(a =>
            {
                var name = (string)a!["name"]!;
                var path = Path.Combine(_dir, $"{n++}-{name}");
                File.WriteAllText(path, (string)a["text"]!);
                return new AttachmentFile(path, (string)a["media_type"]!, name);
            }).ToArray();
        static byte[] Json(System.Text.Json.Nodes.JsonNode? v) => Utf8(v!.ToJsonString());
        static bool Same(byte[] a, System.Text.Json.Nodes.JsonNode? b) =>
            System.Text.Json.Nodes.JsonNode.DeepEquals(System.Text.Json.Nodes.JsonNode.Parse(a), b);
        static MessageRole Role(string s) => s switch
        {
            "user" => MessageRole.User,
            "assistant" => MessageRole.Assistant,
            "system" => MessageRole.System,
            "tool_call" => MessageRole.ToolCall,
            "tool_result" => MessageRole.ToolResult,
            _ => throw new ArgumentException(s),
        };
        foreach (var name in names)
        {
            var f = System.Text.Json.Nodes.JsonNode.Parse(File.ReadAllText(name))!;
            var c = f["conversation"]!;
            var tags = c["tags"]!.AsObject().Select(t => new Tag(t.Key, (string)t.Value!)).OrderBy(t => t.Key, StringComparer.Ordinal).ToArray();
            var convData = c["app_data"]!.AsObject().Select(e => new AppDataEntry(e.Key, Json(e.Value))).ToArray();
            var msgs = f["messages"]!.AsArray();
            var legacy = msgs.Select(m => new LegacyMessage(
                Role((string)m!["role"]!), (string)m["content"]!, (ulong)m["created_ms"]!,
                Files(m["attachments"]), (string?)m["tool_call_id"], (string?)m["tool_name"],
                m["app_data"]!.AsObject().Select(e => new AppDataWrite(e.Key, Json(e.Value), Files(m["app_data_blobs"]?[e.Key]))).ToArray())).ToArray();
            var scope = core.ImportLegacyConversation(
                new LegacyConversation((string)c["title"]!, (ulong)c["created_ms"]!, Replication.Synced, (bool)c["pinned"]!, (bool)c["archived"]!, tags, convData),
                legacy);

            var got = core.Conversation(scope)!;
            Assert.Equal((string)c["title"]!, got.Title);
            Assert.Equal(tags, got.Tags);
            foreach (var (ns, v) in c["app_data"]!.AsObject()) Assert.True(Same(got.GetAppData(ns)!, v), $"{name} {ns}");
            var page = core.Messages(scope, null, 100).Messages;
            Assert.Equal(msgs.Count, page.Length);
            for (var i = 0; i < page.Length; i++)
            {
                var (m, want) = (page[i], msgs[i]!);
                Assert.Equal((string)want["content"]!, m.Content);
                Assert.Equal((string?)want["tool_call_id"], m.ToolCallId);
                var atts = want["attachments"]?.AsArray() ?? [];
                for (var j = 0; j < atts.Count; j++)
                    Assert.Equal(Utf8((string)atts[j]!["text"]!), await core.ReadAttachment(scope, m.Blobs[j].Hash));
                foreach (var (ns, v) in want["app_data"]!.AsObject())
                {
                    var d = m.GetAppData(ns)!;
                    Assert.True(Same(d.Bytes, v), $"{name} {ns}");
                    var blobs = want["app_data_blobs"]?[ns]?.AsArray() ?? [];
                    Assert.Equal(blobs.Count, d.Blobs.Length);
                    for (var j = 0; j < blobs.Count; j++)
                        Assert.Equal(Utf8((string)blobs[j]!["text"]!), await core.ReadAttachment(scope, d.Blobs[j].Hash));
                }
            }
        }
        await core.Close();
    }

    private static string FixtureDir([System.Runtime.CompilerServices.CallerFilePath] string here = "") =>
        Path.GetFullPath(Path.Combine(Path.GetDirectoryName(here)!, "../../../tests/fixtures/app-data"));

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

        File.WriteAllBytes(Path.Combine(_dir, "keys", "bad.dpapi"), [1, 2, 3]);
        Assert.Throws<SyncException.KeyStore>(() => store.Load("bad"));

        using var core = SyncCore.Open(
            new CoreConfig(Path.Combine(_dir, "dpapi"), "dpapi", Platform.Windows, null, OsaurusSyncFfiMethods.LocalNetworkConfig() with { Enabled = false }, null, OsaurusSyncFfiMethods.ApiVersion()),
            store);
        Assert.NotNull(store.Load("device-key"));
    }
}
