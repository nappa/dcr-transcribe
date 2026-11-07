import XCTest
@testable import DCRMonitor

final class AudioSettingsTests: XCTestCase {
    func testRateChoicesRequireBothDevicesAndIncludeRangeBoundaries() throws {
        let data = Data(#"""
        {"revision":"test","audio":{"device_id":"mic","output_device_id":"speaker","sample_rate":48000,"channels":3},
        "inputs":[{"name":"mic","is_default":true,"ranges":[{"min":24000,"max":48000,"channels":1}]}],
        "outputs":[{"name":"speaker","is_default":true,"ranges":[{"min":44100,"max":48000,"channels":2}]}]}
        """#.utf8)
        let settings = try JSONDecoder().decode(AudioSettings.self, from: data)
        XCTAssertEqual(settings.rates(input: "mic", output: "speaker"), [44100, 48000])
        XCTAssertEqual(settings.rates(input: "default", output: "default"), [44100, 48000])
        XCTAssertTrue(settings.rates(input: "disconnected", output: "speaker").isEmpty)
        XCTAssertTrue(settings.rates(input: "mic", output: "disconnected").isEmpty)
    }

    func testNativeSettingsCanBeReadWithoutStartingEngine() throws {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let library = root.appendingPathComponent("target/debug/libdcr_transcribe.dylib")
        let config = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + ".toml")
        defer { try? FileManager.default.removeItem(at: config) }
        try "[audio]\ndevice_id = 'test mic'\nsample_rate = 48000\nchannels = 3\n".write(to: config, atomically: true, encoding: .utf8)
        let result = try AudioSettingsAPI.request(config: config, libraryURL: library)
        XCTAssertEqual(result.audio.device_id, "test mic")
        XCTAssertEqual(result.audio.sample_rate, 48000)
        XCTAssertEqual(result.audio.channels, 3)
        XCTAssertFalse(result.revision.isEmpty)
    }
}
