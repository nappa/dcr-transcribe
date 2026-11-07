import Foundation
import Darwin

/// Rust cdylib を同じプロセスへロードする。HTTP・子プロセスは使用しない。
final class EngineBridge: @unchecked Sendable {
    private typealias Start = @convention(c) (UnsafePointer<CChar>?) -> UnsafeMutableRawPointer?
    private typealias Snapshot = @convention(c) (UnsafeRawPointer?) -> UnsafeMutablePointer<CChar>?
    private typealias Stop = @convention(c) (UnsafeRawPointer?) -> Void
    private typealias Destroy = @convention(c) (UnsafeMutableRawPointer?) -> Void
    private typealias Free = @convention(c) (UnsafeMutablePointer<CChar>?) -> Void
    private typealias Select = @convention(c) (UnsafeRawPointer?, Int32) -> Void
    private typealias OutputEnabled = @convention(c) (UnsafeRawPointer?, Int32, Bool) -> Void
    private typealias Mute = @convention(c) (UnsafeRawPointer?, Bool) -> Void
    private typealias Version = @convention(c) () -> UInt32
    private let snapshotFunction: Snapshot
    private let stopFunction: Stop
    private let destroyFunction: Destroy
    private let freeFunction: Free
    private let selectFunction: Select
    private let muteFunction: Mute
    private let outputEnabledFunction: OutputEnabled
    private var handle: UnsafeMutableRawPointer?
    private let lock = NSLock()

    init(config: URL, libraryURL: URL? = nil) throws {
        let url = libraryURL ?? Bundle.main.bundleURL.appendingPathComponent("Contents/Frameworks/libdcr_transcribe.dylib")
        guard let library = dlopen(url.path, RTLD_NOW | RTLD_LOCAL) else {
            throw BridgeError.message("収録エンジンを読み込めません。アプリを再ビルドしてください。")
        }
        // Rust の TLS・ランタイムが参照するため、ライブラリはプロセス終了まで保持する。
        func symbol<T>(_ name: String, as type: T.Type) throws -> T {
            guard let address = dlsym(library, name) else { throw BridgeError.message("収録エンジンのAPIが一致しません: \(name)") }
            return unsafeBitCast(address, to: type)
        }
        let version = try symbol("dcr_engine_abi_version", as: Version.self)
        guard version() == 1 else { throw BridgeError.message("収録エンジンのバージョンが一致しません") }
        let start = try symbol("dcr_engine_start", as: Start.self)
        snapshotFunction = try symbol("dcr_engine_snapshot", as: Snapshot.self)
        stopFunction = try symbol("dcr_engine_stop", as: Stop.self)
        destroyFunction = try symbol("dcr_engine_destroy", as: Destroy.self)
        freeFunction = try symbol("dcr_string_free", as: Free.self)
        selectFunction = try symbol("dcr_engine_select_output", as: Select.self)
        muteFunction = try symbol("dcr_engine_mute_output", as: Mute.self)
        outputEnabledFunction = try symbol("dcr_engine_output_enabled", as: OutputEnabled.self)
        handle = config.path.withCString { start($0) }
        guard handle != nil else { throw BridgeError.message("収録エンジンを開始できません。停止処理中の可能性があります。") }
    }

    func snapshot() throws -> EngineSnapshot {
        lock.lock(); defer { lock.unlock() }
        guard let handle, let text = snapshotFunction(handle) else { throw BridgeError.message("収録状態を取得できません") }
        defer { freeFunction(text) }
        return try JSONDecoder().decode(EngineSnapshot.self, from: Data(String(cString: text).utf8))
    }
    func stop() { lock.lock(); defer { lock.unlock() }; if let handle { stopFunction(handle) } }
    func selectOutput(_ channel: Int?) {
        lock.lock(); defer { lock.unlock() }
        if let handle { selectFunction(handle, channel.flatMap(Int32.init(exactly:)) ?? -1) }
    }
    func setOutput(_ channel: Int, enabled: Bool) {
        lock.lock(); defer { lock.unlock() }
        if let handle, let channel = Int32(exactly: channel) { outputEnabledFunction(handle, channel, enabled) }
    }
    func muteOutput(_ muted: Bool) { lock.lock(); defer { lock.unlock() }; if let handle { muteFunction(handle, muted) } }

    /// 呼び出し側でポーリングを停止した後、バックグラウンドで呼ぶ。
    func close() {
        lock.lock()
        let detachedHandle = handle
        handle = nil
        lock.unlock()
        if let detachedHandle { destroyFunction(detachedHandle) }
    }
    deinit { close() }
}

enum BridgeError: LocalizedError {
    case message(String)
    var errorDescription: String? { switch self { case .message(let text): return text } }
}

struct EngineSnapshot: Decodable {
    struct Entry: Decodable { let time: String; let seconds: Double; let text: String; let partial: Bool }
    struct Channel: Decodable {
        let id: Int
        let name: String
        let level_db: Double
        let threshold_db: Double
        let voice: Bool
        let transcribe: Int
        let quality_warning: String?
        let recording_error: String?
        let transcripts: [Entry]
    }
    let version: Int
    let pid: Int
    let phase: String
    let error: String?
    let recordings_dir: String
    let segments_dir: String
    let input_problem: String?
    let output_problem: String?
    let mobile_url: String?
    let selected_outputs: [Int]?
    let selected_output: Int?
    let channels: [Channel]
}
