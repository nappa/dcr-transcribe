import XCTest
import Darwin
@testable import DCRMonitor

final class TranscriptTests: XCTestCase {
    private func snapshot(_ entries: String, phase: String = "running") throws -> EngineSnapshot {
        try JSONDecoder().decode(EngineSnapshot.self, from: Data("""
        {"version":1,"pid":123,"phase":"\(phase)","recordings_dir":"/tmp/recordings","segments_dir":"/tmp/segments",
         "mobile_url":"https://example.com/?k=test","selected_output":0,"input_problem":"入力が停止しました",
         "channels":[{"id":0,"name":"本部","level_db":-24.5,"threshold_db":-40,"voice":true,"transcribe":1,
         "quality_warning":"ハムの疑い","recording_error":"保存失敗","transcripts":\(entries)}]}
        """.utf8))
    }

    @MainActor
    func testSnapshotsReplacePartialWithoutDuplicatingFinals() throws {
        let feed = TranscriptFeed()
        let partial = try snapshot(#"[{"time":"2026-10-18T12:00:00.000Z","seconds":1,"text":"途中","partial":true}]"#)
        feed.apply(partial)
        XCTAssertEqual(feed.lines.count, 1)
        XCTAssertTrue(feed.lines[0].partial)
        let final = try snapshot(#"[{"time":"2026-10-18T12:00:00Z","seconds":1,"text":"確定","partial":false}]"#)
        feed.apply(final)
        let originalID = feed.lines[0].id
        feed.apply(final)
        XCTAssertEqual(feed.lines.count, 1)
        XCTAssertEqual(feed.lines[0].id, originalID)
        XCTAssertFalse(feed.lines[0].partial)
        XCTAssertEqual(feed.lines[0].text, "確定")
        feed.apply(try snapshot("[]", phase: "starting"))
        XCTAssertEqual(feed.lines.map(\.text), ["確定"])
        XCTAssertFalse(feed.connected)
        XCTAssertNil(feed.mobileURL)
    }

    @MainActor
    func testFeedPersistsFinalsBeyondEngineWindowAndDoesNotRestoreClearedRows() async throws {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString).appendingPathComponent("history.jsonl")
        defer { try? FileManager.default.removeItem(at: url.deletingLastPathComponent()) }
        let store = HistoryStore(url: url)
        let feed = TranscriptFeed(history: store)
        let final = try snapshot(#"[{"time":"2026-10-18T12:00:00Z","seconds":1,"text":"保存する交信","partial":false}]"#)
        feed.apply(final)
        feed.apply(try snapshot("[]"))
        await feed.flushHistory()
        let restarted = TranscriptFeed(history: store)
        restarted.restore(try await store.restore())
        XCTAssertEqual(restarted.lines.map(\.text), ["保存する交信"])
        try await restarted.clearHistory(at: .distantFuture)
        restarted.apply(final)
        XCTAssertTrue(restarted.lines.isEmpty)
    }

    @MainActor
    func testExactEngineStatusAndWarningsAreDisplayed() throws {
        let feed = TranscriptFeed()
        feed.apply(try snapshot("[]"))
        XCTAssertTrue(feed.connected)
        XCTAssertEqual(feed.status, "収録中")
        XCTAssertEqual(feed.mobileURL, "https://example.com/?k=test")
        XCTAssertEqual(feed.channels[0], "本部")
        XCTAssertEqual(feed.selectedOutput, 0)
        XCTAssertEqual(feed.readings[0]?.levelDB, -24)
        XCTAssertEqual(feed.readings[0]?.thresholdDB, -40)
        XCTAssertEqual(feed.readings[0]?.receiving, true)
        XCTAssertEqual(feed.readings[0]?.transcribe, 1)
        XCTAssertEqual(feed.readings[0]?.warning, "ハムの疑い")
        XCTAssertEqual(feed.readings[0]?.recordingError, "保存失敗")
        XCTAssertEqual(feed.deviceProblem, "入力が停止しました")
        XCTAssertEqual(feed.recordingsDirectory?.path, "/tmp/recordings")
        XCTAssertEqual(feed.segmentsDirectory?.path, "/tmp/segments")
    }

    @MainActor
    func testOutputToggleRespondsImmediatelyWithoutChangingPlaybackChannel() throws {
        let model = MonitorModel()
        defer { model.shutdown() }
        model.selectedChannel = 2
        model.feed.apply(try snapshot("[]"))
        XCTAssertEqual(model.feed.selectedOutput, 0)
        model.toggleOutput(0)
        XCTAssertNil(model.feed.selectedOutput)
        model.toggleOutput(0)
        XCTAssertEqual(model.feed.selectedOutput, 0)
        model.toggleOutput(0)
        XCTAssertNil(model.feed.selectedOutput)
        XCTAssertEqual(model.selectedChannel, 2)
        model.toggleOutput(8)
        XCTAssertNil(model.feed.selectedOutput)
    }

    @MainActor
    func testMultipleOutputsToggleIndependently() throws {
        let feed = TranscriptFeed()
        let data = Data(#"""
        {"version":1,"pid":1,"phase":"running","recordings_dir":"","segments_dir":"","selected_outputs":[],
        "channels":[{"id":0,"name":"a","level_db":-40,"threshold_db":-30,"voice":false,"transcribe":1,"transcripts":[]},
                    {"id":1,"name":"b","level_db":-40,"threshold_db":-30,"voice":false,"transcribe":1,"transcripts":[]}]}
        """#.utf8)
        feed.apply(try JSONDecoder().decode(EngineSnapshot.self, from: data))
        feed.toggleOutput(0); feed.toggleOutput(1)
        XCTAssertEqual(feed.selectedOutputs, [0, 1])
        feed.toggleOutput(0)
        XCTAssertEqual(feed.selectedOutputs, [1])
        feed.toggleOutput(1)
        XCTAssertTrue(feed.selectedOutputs.isEmpty)
    }

    @MainActor
    func testTranscriptPlaybackMatchesChannelAndTimePreferringSegments() {
        let model = MonitorModel()
        defer { model.shutdown() }
        let start = Date(timeIntervalSince1970: 1000)
        let full = Recording(url: URL(fileURLWithPath: "/tmp/full.wav"), channel: 0, start: start, duration: 60, isSegment: false, estimated: true)
        let segment = Recording(url: URL(fileURLWithPath: "/tmp/segment.wav"), channel: 0, start: start.addingTimeInterval(10), duration: 10, isSegment: true, estimated: true)
        model.recordings = [full, segment]
        let line = TranscriptLine(id: "test", channel: 0, date: start.addingTimeInterval(12), text: "交信", partial: false)
        XCTAssertEqual(model.recording(for: line), segment)
        model.playTranscript(line)
        XCTAssertEqual(model.selected, segment)
        XCTAssertEqual(model.position, 2, accuracy: 0.001)
        XCTAssertFalse(model.following)
        XCTAssertNil(model.recording(for: TranscriptLine(id: "other", channel: 1, date: line.date, text: "", partial: false)))
        model.recordings = [full]
        XCTAssertEqual(model.recording(for: line), full)
    }

    @MainActor
    func testReachingTranscriptBottomResumesFollowing() {
        let model = MonitorModel()
        defer { model.shutdown() }
        model.updateTranscriptFollowing(atBottom: false)
        XCTAssertFalse(model.following)
        model.windowEnd = .distantPast
        model.updateTranscriptFollowing(atBottom: true)
        XCTAssertTrue(model.following)
        XCTAssertGreaterThan(model.windowEnd, Date().addingTimeInterval(-1))
    }

    func testNativeEngineRunsInThisProcessAndCanRestartAfterFailure() async throws {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let library = root.appendingPathComponent("target/debug/libdcr_transcribe.dylib")
        // 存在しない設定で初期化に失敗させ、マイク・AWSを使わず実際のFFIを検証する。
        let missing = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString).appendingPathComponent("missing.toml")
        for _ in 0..<2 {
            let engine = try EngineBridge(config: missing, libraryURL: library)
            defer { engine.close() }
            var state = try engine.snapshot()
            for _ in 0..<100 where state.phase != "failed" {
                try await Task.sleep(for: .milliseconds(20))
                state = try engine.snapshot()
            }
            XCTAssertEqual(state.pid, Int(getpid()))
            XCTAssertEqual(state.phase, "failed")
            XCTAssertNotNil(state.error)
            engine.stop()
            await Task.detached { engine.close() }.value
            engine.stop()
            XCTAssertThrowsError(try engine.snapshot())
        }
    }
}
