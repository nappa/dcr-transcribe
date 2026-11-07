import XCTest
import AVFoundation
@testable import DCRMonitor

final class TimelineTests: XCTestCase {
    func testWindowCoordinatesClampingAndOverlap() {
        let start = Date(timeIntervalSince1970: 1000)
        let recording = Recording(url: URL(fileURLWithPath: "/tmp/a.wav"), channel: 0, start: start, duration: 30, isSegment: true, estimated: true)
        let window = TimelineWindow(end: start.addingTimeInterval(60), duration: 120)
        XCTAssertEqual(window.x(at: start, width: 600), 300, accuracy: 0.001)
        XCTAssertEqual(window.date(at: 300, width: 600), start)
        XCTAssertEqual(recording.offset(at: start.addingTimeInterval(-5)), 0)
        XCTAssertLessThan(recording.offset(at: start.addingTimeInterval(40)), 30)
        XCTAssertTrue(window.overlaps(recording))
        XCTAssertFalse(TimelineWindow(end: start, duration: 10).overlaps(recording))
    }

    func testMetadataLoadAndMalformedFileIsolation() async throws {
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: folder) }
        let audio = folder.appendingPathComponent("ch0_test.aac")
        try Data().write(to: audio)
        let metadata = SegmentMetadata(channel: 0, start_unix_ms: 1700000000123, duration_ms: 4500)
        try JSONEncoder().encode(metadata).write(to: audio.appendingPathExtension("json"))
        let broken = folder.appendingPathComponent("ch1_test.aac")
        try Data().write(to: broken)
        try Data("invalid".utf8).write(to: broken.appendingPathExtension("json"))
        let library = RecordingLibrary()
        let result = try await library.scan(folder)
        XCTAssertEqual(result.recordings.count, 1)
        XCTAssertEqual(result.skipped, 1)
        XCTAssertEqual(result.recordings[0].start.timeIntervalSince1970, 1700000000.123, accuracy: 0.001)
        XCTAssertEqual(result.recordings[0].duration, 4.5)
        try FileManager.default.removeItem(at: audio)
        let afterDelete = try await library.scan(folder)
        XCTAssertTrue(afterDelete.recordings.isEmpty)
    }

    @MainActor
    func testLatestRestoresFollowingAndChannelSelection() {
        let model = MonitorModel()
        defer { model.shutdown() }
        model.selectedChannel = 2
        model.following = false
        model.windowEnd = .distantPast
        model.recordings = [Recording(url: URL(fileURLWithPath: "/tmp/a.wav"), channel: 2, start: Date(timeIntervalSince1970: 1000), duration: 20, isSegment: true, estimated: true)]
        model.goLatest()
        XCTAssertTrue(model.following)
        XCTAssertEqual(model.selectedChannel, 2)
        XCTAssertNil(model.selected)
        XCTAssertFalse(model.playing)
        XCTAssertEqual(model.windowEnd.timeIntervalSince1970, 1020 + model.span * 0.03, accuracy: 0.001)
        model.scroll(100)
        XCTAssertFalse(model.following)
    }
    @MainActor
    func testPlaybackEndAutomaticallyReturnsToLatest() async throws {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + ".wav")
        defer { try? FileManager.default.removeItem(at: url) }
        let format = AVAudioFormat(standardFormatWithSampleRate: 16000, channels: 1)!
        let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: 1600)!
        buffer.frameLength = 1600
        buffer.floatChannelData![0].initialize(repeating: 0, count: 1600)
        do {
            let file = try AVAudioFile(forWriting: url, settings: format.settings)
            try file.write(from: buffer)
        }
        let model = MonitorModel()
        defer { model.shutdown() }
        let recording = Recording(url: url, channel: 0, start: Date(), duration: 0.1, isSegment: true, estimated: true)
        model.recordings = [recording]
        model.play(recording)
        XCTAssertFalse(model.following)
        for _ in 0..<100 {
            if model.following { break }
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertNil(model.error)
        XCTAssertTrue(model.following, "音声終了通知で最新表示へ戻る")
        XCTAssertNil(model.selected)
    }

    func testLegacySegmentAcrossMidnight() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let folder = root.appendingPathComponent("20260101_230000")
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let url = folder.appendingPathComponent("ch2_001234_500_1.wav")
        let format = AVAudioFormat(standardFormatWithSampleRate: 16000, channels: 1)!
        let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: 1600)!
        buffer.frameLength = 1600
        buffer.floatChannelData![0].initialize(repeating: 0, count: 1600)
        do { try AVAudioFile(forWriting: url, settings: format.settings).write(from: buffer) }
        try FileManager.default.setAttributes([.modificationDate: Date().addingTimeInterval(-60)], ofItemAtPath: url.path)
        let result = try await RecordingLibrary().scan(root)
        XCTAssertEqual(result.recordings.count, 1)
        let recording = try XCTUnwrap(result.recordings.first)
        XCTAssertTrue(recording.isSegment)
        XCTAssertEqual(recording.channel, 2)
        let components = Calendar.current.dateComponents([.year, .month, .day, .hour, .minute, .second], from: recording.start)
        XCTAssertEqual(components.day, 2)
        XCTAssertEqual(components.hour, 0)
        XCTAssertEqual(components.minute, 12)
        XCTAssertEqual(recording.duration, 0.1, accuracy: 0.001)
    }

}
