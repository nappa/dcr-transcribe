import Foundation
import Darwin
import Combine

struct AudioSettings: Decodable {
    struct Audio: Decodable {
        let device_id: String
        let output_device_id: String
        let sample_rate: UInt32
        let channels: Int
    }
    struct Device: Decodable, Identifiable {
        struct Range: Decodable { let min: UInt32; let max: UInt32; let channels: Int }
        let name: String
        let is_default: Bool
        let ranges: [Range]
        var id: String { name }
        func supports(_ rate: UInt32) -> Bool { ranges.contains { $0.min <= rate && rate <= $0.max } }
    }
    let audio: Audio
    let revision: String
    let inputs: [Device]
    let outputs: [Device]

    func device(_ name: String, input: Bool) -> Device? {
        (input ? inputs : outputs).first { name == "default" ? $0.is_default : $0.name == name }
    }
    func rates(input: String, output: String) -> [UInt32] {
        guard let mic = device(input, input: true), let speaker = device(output, input: false) else { return [] }
        let candidates: [UInt32] = [8000, 16000, 22050, 24000, 32000, 44100, 48000, 88200, 96000, 176400, 192000, audio.sample_rate]
        let endpoints = (mic.ranges + speaker.ranges).flatMap { [$0.min, $0.max] }
        return Set(candidates + endpoints).filter { $0 > 0 && mic.supports($0) && speaker.supports($0) }.sorted()
    }
}

struct AudioSettingsUpdate: Encodable {
    let revision: String
    let device_id: String
    let output_device_id: String
    let sample_rate: UInt32
}

enum AudioSettingsAPI {
    private typealias Request = @convention(c) (UnsafePointer<CChar>?, UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>?
    private typealias Free = @convention(c) (UnsafeMutablePointer<CChar>?) -> Void
    private static let library = dlopen(Bundle.main.bundleURL.appendingPathComponent("Contents/Frameworks/libdcr_transcribe.dylib").path, RTLD_NOW | RTLD_LOCAL)

    static func request(config: URL, update: AudioSettingsUpdate? = nil, libraryURL: URL? = nil) throws -> AudioSettings {
        let handle = libraryURL.map { dlopen($0.path, RTLD_NOW | RTLD_LOCAL) } ?? library
        guard let library = handle,
              let requestSymbol = dlsym(library, "dcr_audio_settings"),
              let freeSymbol = dlsym(library, "dcr_string_free") else {
            throw BridgeError.message("音声設定を読み込めません。アプリを再ビルドしてください。")
        }
        let request = unsafeBitCast(requestSymbol, to: Request.self)
        let free = unsafeBitCast(freeSymbol, to: Free.self)
        let updateText = try update.map { String(decoding: try JSONEncoder().encode($0), as: UTF8.self) }
        let reply = config.path.withCString { path in
            if let updateText { return updateText.withCString { request(path, $0) } }
            return request(path, nil)
        }
        guard let reply else { throw BridgeError.message("音声設定の処理に失敗しました") }
        defer { free(reply) }
        let data = Data(String(cString: reply).utf8)
        struct Failure: Decodable { let error: String }
        if let failure = try? JSONDecoder().decode(Failure.self, from: data) { throw BridgeError.message(failure.error) }
        return try JSONDecoder().decode(AudioSettings.self, from: data)
    }
}

@MainActor
final class AudioSettingsModel: ObservableObject {
    @Published private(set) var settings: AudioSettings?
    @Published var input = "default"
    @Published var output = "default"
    @Published var rate: UInt32 = 48000
    @Published private(set) var working = false
    @Published private(set) var error: String?
    @Published private(set) var saved = false
    private var loadedURL: URL?
    var rates: [UInt32] { settings?.rates(input: input, output: output) ?? [] }
    var canSave: Bool { settings != nil && rates.contains(rate) && !working }

    func load(_ url: URL?) async {
        guard !working else { return }
        guard let url else { settings = nil; loadedURL = nil; return }
        working = true; error = nil; saved = false
        defer { working = false }
        do {
            let result = try await Task.detached { try AudioSettingsAPI.request(config: url) }.value
            settings = result; loadedURL = url
            input = result.audio.device_id; output = result.audio.output_device_id; rate = result.audio.sample_rate
        } catch { settings = nil; loadedURL = nil; self.error = error.localizedDescription }
    }
    func save() async {
        guard canSave, let settings, let url = loadedURL else { return }
        working = true; error = nil; saved = false
        defer { working = false }
        let update = AudioSettingsUpdate(revision: settings.revision, device_id: input, output_device_id: output, sample_rate: rate)
        do {
            self.settings = try await Task.detached { try AudioSettingsAPI.request(config: url, update: update) }.value
            saved = true
        } catch { self.error = error.localizedDescription }
    }
}
