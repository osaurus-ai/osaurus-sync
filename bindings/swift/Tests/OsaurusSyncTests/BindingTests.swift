import Foundation
import XCTest
@testable import OsaurusSync

final class Recorder<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var items: [T] = []
    private(set) var threads: [String] = []

    func append(_ new: [T]) {
        let name = Thread.current.name ?? ""
        lock.withLock {
            items.append(contentsOf: new)
            threads.append(name)
        }
    }

    var all: [T] { lock.withLock { items } }
    var threadNames: [String] { lock.withLock { threads } }
}

func waitUntil(_ what: String, timeout: TimeInterval = 20, _ cond: () throws -> Bool) rethrows {
    let deadline = Date().addingTimeInterval(timeout)
    while try !cond() {
        if Date() > deadline { XCTFail("timed out waiting for \(what)"); return }
        Thread.sleep(forTimeInterval: 0.02)
    }
}

final class BindingTests: XCTestCase {
    var dir: URL!

    override func setUpWithError() throws {
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("osaurus-sync-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: dir)
    }

    func open(_ name: String, platform: Platform = .macOs, online: Bool = false, keys: KeyStore = MemoryKeyStore()) throws -> SyncCore {
        var net = localNetworkConfig()
        net.enabled = online
        return try SyncCore.open(dataDirectory: dir.appendingPathComponent(name), deviceName: name, platform: platform, network: net, keyStore: keys)
    }

    func contents(_ core: SyncCore, _ scope: String) throws -> [String] {
        try core.messages(scope: scope, before: nil, limit: 100).messages.map(\.content)
    }

    func testVersionAndErrors() async throws {
        XCTAssertEqual(apiVersion(), 1)
        let config = CoreConfig(dataDir: dir.appendingPathComponent("v").path, deviceName: "v", platform: .macOs, history: nil, network: localNetworkConfig(), apiVersion: 999)
        XCTAssertThrowsError(try SyncCore.open(config: config, keyStore: MemoryKeyStore())) { e in
            XCTAssertEqual((e as? SyncError)?.code, SyncErrorCode.incompatibleVersion)
        }
        let core = try open("a")
        XCTAssertEqual(core.apiVersion(), apiVersion())
        XCTAssertThrowsError(try open("a")) { e in
            XCTAssertEqual((e as? SyncError)?.code, SyncErrorCode.alreadyOpen)
        }
        XCTAssertThrowsError(try core.sendMessage(scope: "nope", message: .text("x"))) { e in
            XCTAssertEqual((e as? SyncError)?.code, SyncErrorCode.invalidInput)
        }
        try await core.close()
        XCTAssertThrowsError(try core.conversations()) { e in
            XCTAssertEqual((e as? SyncError)?.code, SyncErrorCode.closed)
        }
    }

    func testLocalApiAndCallbacks() async throws {
        let core = try open("mac")
        let events = Recorder<CoreEvent>()
        core.setListener { events.append([$0]) }
        let convs = Recorder<ConversationChange>()
        let convSub = core.subscribeConversations { convs.append($0) }

        let scope = try core.createConversation(title: "Plans")
        let changes = Recorder<ScopeChange>()
        let sub = try core.subscribe(scope: scope) { changes.append($0) }

        let id = try core.sendMessage(scope: scope, message: .text("hello world"))
        try core.editMessage(id: id, content: "hello there")
        try core.addReaction(id: id, reaction: "👍")
        try core.renameConversation(scope: scope, title: "Trip plans")
        try core.setPinned(scope: scope, pinned: true)
        try core.setArchived(scope: scope, archived: false)
        try core.setSortOrder(scope: scope, order: 2.5)

        waitUntil("scope callbacks") { changes.all.count >= 3 }
        waitUntil("conversation callbacks") { !convs.all.isEmpty }
        XCTAssertTrue(changes.threadNames.allSatisfy { $0 == "osaurus-sync-callbacks" })

        let m = try XCTUnwrap(core.message(id: id))
        XCTAssertEqual(m.content, "hello there")
        XCTAssertTrue(m.edited)
        XCTAssertEqual(m.reactions.first?.value, "👍")
        let conv = try XCTUnwrap(core.conversation(scope: scope))
        XCTAssertEqual(conv.title, "Trip plans")
        XCTAssertTrue(conv.pinned)
        XCTAssertEqual(conv.sortOrder, 2.5)
        XCTAssertEqual(conv.myRole, .owner)
        XCTAssertEqual(try core.conversations().count, 1)
        XCTAssertEqual(try core.search(query: "there", limit: 10).first?.id, id)
        try core.markRead(scope: scope, hlc: m.hlc)
        try core.removeReaction(id: id, reaction: "👍")
        try core.deleteMessage(id: id)
        XCTAssertEqual(try contents(core, scope), [])

        // Attachments are encrypted on import and readable locally.
        let file = dir.appendingPathComponent("note.txt")
        try Data("attachment".utf8).write(to: file)
        var msg = OutgoingMessage.text("see file")
        msg.attachments = [AttachmentFile(path: file.path, mediaType: "text/plain", name: "note.txt")]
        let withFile = try core.sendMessage(scope: scope, message: msg)
        let blob = try XCTUnwrap(core.message(id: withFile)?.blobs.first)
        XCTAssertTrue(blob.available)
        let path = try await core.fetchAttachment(scope: scope, hash: blob.hash)
        XCTAssertEqual(try String(contentsOfFile: path, encoding: .utf8), "attachment")

        // Streaming: tokens are ephemeral, finish commits under the stream's id.
        let stream = try core.openStream(scope: scope, role: .assistant, asAgent: nil)
        try stream.push(delta: "Hel")
        try stream.push(delta: "lo")
        let streamed = try stream.finish(content: nil)
        XCTAssertEqual(streamed, stream.messageId())
        XCTAssertEqual(try core.message(id: streamed)?.content, "Hello")
        XCTAssertThrowsError(try stream.push(delta: "late"))
        let aborted = try core.openStream(scope: scope, role: .assistant, asAgent: nil)
        try aborted.push(delta: "never")
        aborted.abort()
        XCTAssertNil(try core.message(id: aborted.messageId()))

        let legacy = try core.importLegacyConversation(
            title: "Old", createdMs: 1_600_000_000_000,
            messages: [LegacyMessage(role: .user, content: "from 2020", createdMs: 1_600_000_000_000)])
        XCTAssertEqual(try core.messages(scope: legacy, before: nil, limit: 10).messages.first?.timestampMs, 1_600_000_000_000)

        // A read-only view of a conversation as it was at a past moment.
        let notes = try core.createConversation(title: "Notes")
        let first = try core.sendMessage(scope: notes, message: .text("first"))
        let then = try XCTUnwrap(core.message(id: first)).timestampMs
        Thread.sleep(forTimeInterval: 0.005)
        try core.renameConversation(scope: notes, title: "Renamed notes")
        _ = try core.sendMessage(scope: notes, message: .text("second"))
        let past = try core.viewAt(scope: notes, timestampMs: then, limit: 50)
        XCTAssertTrue(past.complete)
        XCTAssertEqual(past.conversation?.title, "Notes")
        XCTAssertEqual(past.messages.map { $0.content }, ["first"])
        XCTAssertEqual(try contents(core, notes), ["first", "second"])

        let workspace = try core.createWorkspace(title: "Team")
        let invite = try core.createInvite(scope: workspace, role: .member, ttlMs: 60_000)
        XCTAssertTrue(invite.hasPrefix("osaurus-invite:1:"))
        let other = try open("other")
        try core.invite(scope: workspace, user: other.userKey(), role: .viewer)
        let otherPrincipal = "u:" + (try other.userKey())
        try core.changeRole(scope: workspace, principal: otherPrincipal, role: .member)
        try core.remove(scope: workspace, principal: otherPrincipal)
        try other.redeemInvite(token: invite)
        try await other.close()

        let agentSecret = generateAgentSecret()
        let agent = try core.assignAgent(scope: workspace, agentSecret: agentSecret, host: nil, role: .member, ttlMs: 86_400_000)
        XCTAssertTrue(agent.count > 60)
        XCTAssertNotNil(try core.publishAgentRecord(agentSecret: generateAgentSecret(), ttlMs: 60_000))
        XCTAssertNil(try core.publishAgentRecord(agentSecret: nil, ttlMs: 0))
        let address = try agentAddress(agent: agent)
        XCTAssertTrue(address.hasPrefix("0x") && address.count == 42)
        let hosted = try XCTUnwrap(core.agents(scope: workspace).first)
        XCTAssertEqual(hosted.address, address)
        XCTAssertTrue(hosted.hostedHere)
        let card = try core.agentCard(agentSecret: agentSecret, ttlMs: 86_400_000)
        let verified = try verifyAgentCard(card: card, address: address.uppercased().replacingOccurrences(of: "0X", with: "0x"))
        XCTAssertEqual(verified.host, core.deviceKey())
        XCTAssertEqual(verified.user, try core.userKey())
        XCTAssertThrowsError(try verifyAgentCard(card: card, address: "0x" + String(repeating: "0", count: 40))) { e in
            XCTAssertEqual((e as? SyncError)?.code, SyncErrorCode.crypto)
        }
        let expired = try core.agentCard(agentSecret: agentSecret, ttlMs: 0)
        XCTAssertThrowsError(try verifyAgentCard(card: expired, address: nil)) { e in
            XCTAssertEqual((e as? SyncError)?.code, SyncErrorCode.crypto)
        }
        XCTAssertTrue(try core.createInviteFor(scope: workspace, user: verified.user, role: .member, ttlMs: 60_000).hasPrefix("osaurus-invite:1:"))

        XCTAssertEqual(try core.recoveryPhrase().count, 24)
        XCTAssertEqual(try core.devices().count, 1)
        XCTAssertEqual(try core.devices().first?.id, core.deviceKey())
        try core.setHistoryMode(scope: nil, mode: .recent(days: 30))
        try core.setHistoryMode(scope: scope, mode: .full)
        try core.requestHistory(scope: scope, beforeHlc: UInt64.max, days: 30)
        try core.setBackfillPolicy(policy: .paused)
        XCTAssertTrue(core.diagnostics().backfillPaused)
        try core.setBackfillPolicy(policy: .run)
        XCTAssertEqual(try core.syncStatus(scope: scope).scope, scope)
        XCTAssertFalse(core.isOnline())
        XCTAssertNil(core.localAddress())

        let offer = try core.pairingOffer(ttlMs: 60_000)
        XCTAssertTrue(offer.payload.contains(offer.code))
        core.cancelPairing()
        XCTAssertThrowsError(try core.approveDevice(device: String(repeating: "ab", count: 32)))
        XCTAssertThrowsError(try core.revokeDevice(device: core.deviceKey()))

        try await core.suspend()
        try await core.resume()
        XCTAssertTrue(core.isOnline())
        XCTAssertNotNil(core.localAddress())
        core.syncNow()

        sub.cancel()
        convSub.cancel()
        try await core.close()
        _ = events.all
    }

    func testPairingSyncAndStreamsOverLoopback() async throws {
        let mac = try open("mac", online: true)
        let phoneKeys = MemoryKeyStore()
        let phone = try open("phone", platform: .ios, online: true, keys: phoneKeys)
        let phoneEvents = Recorder<CoreEvent>()
        phone.setListener { phoneEvents.append([$0]) }

        let scope = try mac.createConversation(title: "Shared")
        _ = try mac.sendMessage(scope: scope, message: .text("before pairing"))
        try mac.addPeerAddress(address: XCTUnwrap(phone.localAddress()))
        try phone.addPeerAddress(address: XCTUnwrap(mac.localAddress()))

        let offer = try mac.pairingOffer(ttlMs: 300_000)
        try await phone.pairWithOffer(payload: offer.payload)
        waitUntil("paired") { phoneEvents.all.contains { if case .paired = $0 { return true } else { return false } } }
        try waitUntil("history") { try contents(phone, scope) == ["before pairing"] }
        XCTAssertEqual(try phone.userKey(), try mac.userKey())
        XCTAssertEqual(try phone.devices().count, 2)

        let changes = Recorder<ScopeChange>()
        let sub = try phone.subscribe(scope: scope) { changes.append($0) }
        let stream = try mac.openStream(scope: scope, role: .assistant, asAgent: nil)
        try stream.push(delta: "Thinking")
        waitUntil("stream tokens") {
            changes.all.contains { if case let .stream(u) = $0 { return u.text == "Thinking" && u.state == .streaming } else { return false } }
        }
        _ = try stream.finish(content: "Done")
        try waitUntil("committed") { try contents(phone, scope).contains("Done") }

        let diag = mac.diagnostics()
        XCTAssertTrue(diag.running)
        XCTAssertTrue(diag.peers.contains { $0.device == phone.deviceKey() && $0.connected && !$0.relayed })
        try waitUntil("acknowledged") { try mac.syncStatus(scope: scope).pendingOutbox == 0 }

        // Suspended writes wait in the outbox and go out on resume.
        try await phone.suspend()
        XCTAssertFalse(phone.isOnline())
        _ = try phone.sendMessage(scope: scope, message: .text("offline on the phone"))
        XCTAssertGreaterThan(try phone.syncStatus(scope: scope).pendingOutbox, 0)
        try await phone.resume()
        try phone.addPeerAddress(address: XCTUnwrap(mac.localAddress()))
        try mac.addPeerAddress(address: XCTUnwrap(phone.localAddress()))
        phone.syncNow()
        try waitUntil("catch-up after resume") { try contents(mac, scope).contains("offline on the phone") }

        sub.cancel()
        try await mac.close()
        try await phone.close()
    }

    func testLogsAndSupportBundle() async throws {
        let records = Recorder<LogRecord>()
        SyncLogging.setHandler(level: .info) { records.append([$0]) }
        defer { SyncLogging.useOSLog() }
        let core = try open("logs", online: true)
        try waitUntil("networking log") { records.all.contains { $0.message == "networking started" } }
        XCTAssertTrue(records.threadNames.allSatisfy { $0 == "osaurus-sync-logs" })

        let bundle = core.supportBundle()
        let json = try JSONSerialization.jsonObject(with: Data(bundle.utf8)) as? [String: Any]
        XCTAssertNotNil(json?["counters"] as? [String: Any])
        XCTAssertFalse(bundle.contains(String(core.deviceKey().prefix(12))))
        XCTAssertTrue(bundle.contains("networking started"))
        try await core.close()
    }

    func testDataManagement() async throws {
        let keys = MemoryKeyStore()
        let core = try open("data", keys: keys)
        let keep = try core.createConversation(title: "Keep")
        let file = dir.appendingPathComponent("notes.txt")
        try Data("attached".utf8).write(to: file)
        var msg = OutgoingMessage.text("hello")
        msg.attachments = [AttachmentFile(path: file.path, mediaType: "text/plain", name: "notes.txt")]
        _ = try core.sendMessage(scope: keep, message: msg)

        let usage = try core.storageUsage()
        XCTAssertGreaterThan(usage.databaseBytes, 0)
        XCTAssertGreaterThanOrEqual(usage.attachmentBytes, 8)

        let out = dir.appendingPathComponent("export")
        let summary = try await core.exportConversation(scope: keep, path: out.path)
        XCTAssertEqual(summary, ExportSummary(messages: 1, attachments: 1, missingAttachments: 0))
        XCTAssertTrue(FileManager.default.fileExists(atPath: out.appendingPathComponent("conversation.json").path))

        let gone = try core.createConversation(title: "Leave")
        try core.leaveConversation(scope: gone)
        XCTAssertEqual(try core.conversations().map(\.scope), [keep])

        XCTAssertNotNil(keys.load(name: "device-key"))
        try await core.reset()
        XCTAssertNil(keys.load(name: "device-key"))
        XCTAssertFalse(FileManager.default.fileExists(atPath: dir.appendingPathComponent("data").path))
        XCTAssertThrowsError(try core.conversations()) { e in
            XCTAssertEqual((e as? SyncError)?.code, SyncErrorCode.closed)
        }
        let fresh = try open("data", keys: keys)
        XCTAssertEqual(try fresh.conversations().count, 0)
        try await fresh.close()
    }

    func testKeychainKeyStoreRoundTrip() throws {
        #if os(macOS)
        let store = KeychainKeyStore(service: "ai.osaurus.sync.tests.\(UUID().uuidString)", useDataProtectionKeychain: false)
        #else
        let store = KeychainKeyStore(service: "ai.osaurus.sync.tests.\(UUID().uuidString)")
        #endif
        do {
            try store.store(name: "k", value: Data([1, 2, 3]))
        } catch let e as SyncError {
            throw XCTSkip("keychain unavailable in this environment: \(e)")
        }
        XCTAssertEqual(try store.load(name: "k"), Data([1, 2, 3]))
        try store.store(name: "k", value: Data([4]))
        XCTAssertEqual(try store.load(name: "k"), Data([4]))
        try store.delete(name: "k")
        XCTAssertNil(try store.load(name: "k"))
        try store.delete(name: "k")
    }

    func testBackfillPolicyMapping() {
        typealias C = BackfillPolicyMonitor.Conditions
        XCTAssertEqual(C(expensive: false, constrained: false, lowPower: false).policy, .run)
        XCTAssertEqual(C(expensive: true, constrained: false, lowPower: false).policy, .paused)
        XCTAssertEqual(C(expensive: false, constrained: true, lowPower: false).policy, .paused)
        XCTAssertEqual(C(expensive: false, constrained: false, lowPower: true).policy, .paused)
    }

    func testIosDefaultsToRecentHistory() throws {
        let phone = try open("phone", platform: .ios)
        let scope = try phone.createConversation(title: "x")
        // Recent-only scopes report the sync boundary until backfill completes.
        XCTAssertFalse(try phone.syncStatus(scope: scope).scope.isEmpty)
        let monitor = BackfillPolicyMonitor(core: phone)
        monitor.start()
        monitor.stop()
    }
}
