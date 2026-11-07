import XCTest
@testable import DCRMonitor

final class HistoryTests: XCTestCase {
    private func location() throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("history.jsonl")
    }
    private func line(_ id: Int) -> TranscriptLine {
        TranscriptLine(id: String(id), channel: 0, date: Date(timeIntervalSince1970: Double(1000 + id)), text: "交信\(id)", partial: false)
    }
    func testRestartRestoresAllHistoryAndClearDoesNotReappear() async throws {
        let url = try location()
        defer { try? FileManager.default.removeItem(at: url.deletingLastPathComponent()) }
        let store = HistoryStore(url: url)
        let entries = (0..<200).map(line)
        let recording = Recording(url: url.deletingLastPathComponent().appendingPathComponent("audio.wav"), channel: 0, start: entries[0].date, duration: 10, isSegment: true, estimated: true)
        try Data([1, 2, 3]).write(to: recording.url)
        try await store.append(.transcripts(entries, [0: "本部"]))
        try await store.append(.root(recording.url.deletingLastPathComponent()))
        try await store.append(.recordings([recording]))
        let restored = try await HistoryStore(url: url).restore()
        XCTAssertEqual(restored.lines.count, 200)
        XCTAssertEqual(restored.recordings, [recording])
        XCTAssertEqual(restored.channels[0], "本部")
        XCTAssertEqual(restored.roots.count, 1)
        try await store.append(.clearTranscripts(Date(timeIntervalSince1970: 1200)))
        try await store.append(.clearPlayback(Date(timeIntervalSince1970: 1200)))
        try await store.append(.transcripts(entries + [line(201)], [:]))
        try await store.append(.recordings([recording]))
        let cleared = try await HistoryStore(url: url).restore()
        XCTAssertEqual(cleared.lines, [line(201)])
        XCTAssertTrue(cleared.recordings.isEmpty)
        XCTAssertEqual(try Data(contentsOf: recording.url), Data([1, 2, 3]))
    }
    func testTruncatedLastWriteIsRecoveredBeforeAppending() async throws {
        let url = try location()
        defer { try? FileManager.default.removeItem(at: url.deletingLastPathComponent()) }
        let entry = line(0)
        var data = try JSONEncoder().encode(HistoryEvent.transcripts([entry], [:]))
        data.append(Data("\n{unfinished".utf8))
        try data.write(to: url)
        let store = HistoryStore(url: url)
        let saved = try await store.restore()
        XCTAssertEqual(saved.lines, [entry])
        try await store.append(.transcripts([line(1)], [:]))
        let restored = try await HistoryStore(url: url).restore()
        XCTAssertEqual(restored.lines.count, 2)
    }
    func testLegacyLogImportIsDeduplicatedAndRespectsClear() async throws {
        let url = try location()
        defer { try? FileManager.default.removeItem(at: url.deletingLastPathComponent()) }
        let log = url.deletingLastPathComponent().appendingPathComponent("legacy.log")
        try #"[INFO engine] {"channel":0,"timestamp":"2026-10-18T10:00:00.000Z","timestamp_seconds":1.0,"text":"過去の交信","is_partial":false}"#.appending("\n").write(to: log, atomically: true, encoding: .utf8)
        let store = HistoryStore(url: url)
        try await store.importLog(log)
        try await store.importLog(log)
        let saved = try await store.restore()
        XCTAssertEqual(saved.lines.map(\.text), ["過去の交信"])
        try await store.append(.clearTranscripts(.distantFuture))
        let restarted = HistoryStore(url: url)
        try await restarted.importLog(log)
        let cleared = try await restarted.restore()
        XCTAssertTrue(cleared.lines.isEmpty)
    }
    @MainActor
    func testModelRestoresTranscriptAndPlaybackIndex() async throws {
        let url = try location()
        defer { try? FileManager.default.removeItem(at: url.deletingLastPathComponent()) }
        let store = HistoryStore(url: url)
        let entry = line(0)
        let recording = Recording(url: url.appendingPathExtension("wav"), channel: 0, start: entry.date, duration: 10, isSegment: true, estimated: true)
        try await store.append(.transcripts([entry], [0: "本部"]))
        try await store.append(.recordings([recording]))
        let model = MonitorModel(historyURL: url)
        model.feed.configURL = nil
        defer { model.shutdown() }
        await model.restoreHistory()
        XCTAssertEqual(model.feed.lines, [entry])
        XCTAssertEqual(model.recording(for: entry), recording)
        await model.clearHistory(.transcripts)
        XCTAssertTrue(model.feed.lines.isEmpty)
        XCTAssertEqual(model.recordings, [recording])
        await model.clearHistory(.playback)
        XCTAssertTrue(model.recordings.isEmpty)
    }
}
