import Foundation
import Combine
import AppKit
import AVFoundation

struct TranscriptLine: Identifiable, Equatable, Codable, Sendable {
    let id: String
    let channel: Int
    let date: Date
    let text: String
    let partial: Bool
}

struct ChannelReading: Equatable {
    let levelDB: Int
    let receiving: Bool
    let transcribe: Int
    let thresholdDB: Double
    let warning: String?
    let recordingError: String?
}

/// TUIと同じ状態を同一プロセスのRustから直接取得する。
@MainActor
final class TranscriptFeed: ObservableObject {
    @Published private(set) var lines: [TranscriptLine] = []
    @Published private(set) var channels: [Int: String] = [:]
    @Published private(set) var readings: [Int: ChannelReading] = [:]
    @Published private(set) var status = "停止中"
    @Published private(set) var connected = false
    @Published private(set) var phase = "stopped"
    @Published private(set) var mobileURL: String?
    @Published private(set) var selectedOutputs: Set<Int> = []
    var selectedOutput: Int? { selectedOutputs.sorted().first }
    @Published private(set) var recordingsDirectory: URL?
    @Published private(set) var segmentsDirectory: URL?
    @Published private(set) var deviceProblem: String?
    @Published var error: String?
    @Published var configURL: URL? = UserDefaults.standard.url(forKey: "engineConfigURL")
    private var task: Task<Void, Never>?
    private var bridge: EngineBridge?
    private let history: HistoryStore?
    private var persistedLines: [String: TranscriptLine] = [:]
    private var transcriptCutoff = Date.distantPast
    private var livePartials: [TranscriptLine] = []
    private var persistence: Task<Void, Never>?

    init(history: HistoryStore? = nil) { self.history = history }

    func restore(_ saved: HistorySnapshot) {
        transcriptCutoff = saved.transcriptCutoff
        persistedLines.merge(Dictionary(saved.lines.map { ($0.id, $0) }, uniquingKeysWith: { _, new in new }), uniquingKeysWith: { old, _ in old })
        channels.merge(saved.channels, uniquingKeysWith: { old, _ in old })
        lines = persistedLines.values.sorted { $0.date < $1.date }
    }

    private func persist(_ event: HistoryEvent) {
        guard let history else { return }
        let previous = persistence
        persistence = Task {
            await previous?.value
            do { try await history.append(event) }
            catch { self.error = "履歴を保存できません: \(error.localizedDescription)" }
        }
    }

    func flushHistory() async { await persistence?.value }

    func clearHistory(at date: Date) async throws {
        await flushHistory()
        try await history?.append(.clearTranscripts(date))
        transcriptCutoff = date
        persistedLines = persistedLines.filter { $0.value.date > date }
        lines = lines.filter { $0.date > date }
        livePartials = livePartials.filter { $0.date > date }
    }
    var busy: Bool { bridge != nil || phase == "starting" }

    func chooseConfig() {
        guard !busy else { return }
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.message = "収録に使う config.toml を選択"
        if panel.runModal() == .OK, let url = panel.url {
            configURL = url
            UserDefaults.standard.set(url, forKey: "engineConfigURL")
        }
    }

    func start() async {
        guard !busy else { return }
        if configURL == nil { chooseConfig() }
        guard let configURL else { return }
        guard FileManager.default.isReadableFile(atPath: configURL.path) else { error = "設定ファイルを読み込めません"; return }
        phase = "starting"; status = "開始中"
        guard await AVCaptureDevice.requestAccess(for: .audio) else {
            error = "音声入力の許可が必要です"; phase = "failed"; status = "開始失敗"; return
        }
        guard phase == "starting" else { return }
        do {
            let engine = try EngineBridge(config: configURL)
            bridge = engine
            phase = "starting"; status = "開始中"; error = nil
            lines = lines.filter { !$0.partial }; livePartials = []; readings = [:]
            task = Task { [weak self] in
                guard let self else { return }
                while !Task.isCancelled {
                    do {
                        let snapshot = try engine.snapshot()
                        self.apply(snapshot)
                        if ["stopped", "failed"].contains(snapshot.phase) { break }
                    } catch {
                        self.error = error.localizedDescription
                        self.stop()
                        break
                    }
                    try? await Task.sleep(for: .milliseconds(200))
                }
                engine.stop()
                await Task.detached { engine.close() }.value
                self.bridge = nil
                self.connected = false
                if self.phase != "failed" { self.phase = "stopped"; self.status = "停止中" }
            }
        } catch { self.error = error.localizedDescription; phase = "failed"; status = "開始失敗" }
    }

    func apply(_ snapshot: EngineSnapshot) {
        phase = snapshot.phase
        connected = phase == "running"
        mobileURL = connected ? snapshot.mobile_url : nil
        switch phase {
        case "running": status = "収録中"
        case "starting": status = "開始中"
        case "stopping": status = "停止中…"
        case "failed": status = "停止（エラー）"
        default: status = "停止中"
        }
        if let error = snapshot.error { self.error = error }
        deviceProblem = [snapshot.input_problem, snapshot.output_problem].compactMap { $0 }.joined(separator: " / ")
        if deviceProblem?.isEmpty == true { deviceProblem = nil }
        selectedOutputs = Set(snapshot.selected_outputs ?? snapshot.selected_output.map { [$0] } ?? [])
        if !snapshot.recordings_dir.isEmpty { recordingsDirectory = URL(fileURLWithPath: snapshot.recordings_dir) }
        if !snapshot.segments_dir.isEmpty { segmentsDirectory = URL(fileURLWithPath: snapshot.segments_dir) }
        let names = Dictionary(snapshot.channels.map { ($0.id, $0.name) }, uniquingKeysWith: { _, last in last })
        let namesChanged = names.contains { channels[$0.key] != $0.value }
        channels.merge(names, uniquingKeysWith: { _, new in new })
        readings = Dictionary(snapshot.channels.map { channel in
            (channel.id, ChannelReading(levelDB: Int(min(0, max(-160, channel.level_db))), receiving: channel.voice, transcribe: channel.transcribe,
                                        thresholdDB: channel.threshold_db, warning: channel.quality_warning, recordingError: channel.recording_error))
        }, uniquingKeysWith: { _, last in last })
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let plain = ISO8601DateFormatter()
        let next = snapshot.channels.flatMap { channel in
            channel.transcripts.compactMap { entry -> TranscriptLine? in
                guard let date = formatter.date(from: entry.time) ?? plain.date(from: entry.time) else { return nil }
                let id = entry.partial ? "partial-\(channel.id)" : "\(channel.id)-\(entry.time)-\(entry.seconds)-\(entry.text)"
                return TranscriptLine(id: id, channel: channel.id, date: date, text: entry.text, partial: entry.partial)
            }
        }.sorted { $0.date < $1.date }
        let accepted = next.filter { $0.date > transcriptCutoff }
        let fresh = accepted.filter { !$0.partial && persistedLines[$0.id] == nil }
        for line in fresh { persistedLines[line.id] = line }
        if !fresh.isEmpty || namesChanged { persist(.transcripts(fresh, names)) }
        let partials = accepted.filter(\.partial)
        if !fresh.isEmpty || partials != livePartials {
            livePartials = partials
            lines = (Array(persistedLines.values) + partials).sorted { $0.date < $1.date }
        }
    }

    func stop() {
        guard let bridge else {
            if phase == "starting" { phase = "stopped"; status = "停止中" }
            return
        }
        phase = "stopping"; status = "停止中…"; connected = false
        bridge.stop()
    }
    func toggleOutput(_ channel: Int) {
        guard connected, channels[channel] != nil else { return }
        // 次のポーリングを待たず更新し、連続したキー入力でも確実に反転する。
        let enabled = !selectedOutputs.contains(channel)
        if enabled { selectedOutputs.insert(channel) } else { selectedOutputs.remove(channel) }
        bridge?.setOutput(channel, enabled: enabled)
    }
    func muteOutput(_ muted: Bool) { bridge?.muteOutput(muted) }

    func shutdown() { stop() }
    func shutdownAndWait() async {
        stop()
        await task?.value
        task = nil
        await flushHistory()
    }
}
