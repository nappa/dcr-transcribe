import SwiftUI
import AVFoundation
import AppKit

@MainActor
final class MonitorModel: ObservableObject {
    @Published var recordings: [Recording] = []
    @Published var folder: URL?
    @Published var message = "録音フォルダを開いてください"
    @Published var error: String?
    @Published var selectedChannel = 0
    @Published var selected: Recording?
    @Published var following = true
    @Published var windowEnd = Date()
    @Published var span: Double = 300
    @Published var position: Double = 0
    @Published var playing = false
    @Published var loading = false
    @Published var speed: Float = 1
    @Published var showAnalog = true
    @Published var showDigital = true
    let feed: TranscriptFeed
    private let history: HistoryStore
    private var roots = Set<URL>()
    private var indexedRecordings: [String: Recording] = [:]
    private var playbackCutoff = Date.distantPast
    private var persistence: Task<Void, Never>?
    @Published var clearHistoryTarget: HistoryTarget?
    enum HistoryTarget { case transcripts, playback }
    private let library = RecordingLibrary()
    private let player = AVPlayer()
    private var scanTask: Task<Void, Never>?
    private var timeObserver: Any?
    private var endObserver: NSObjectProtocol?
    private var failureObserver: NSObjectProtocol?
    private var statusObserver: NSKeyValueObservation?
    private var generation = UUID()

    var channels: [Int] {
        let ids = Set(recordings.map(\.channel)).union(feed.channels.keys).union(feed.lines.map(\.channel))
        return ids.isEmpty ? [0, 1, 2] : ids.sorted()
    }
    var visibleRecordings: [Recording] { recordings }
    var latest: Date { recordings.map(\.end).max() ?? Date() }
    var window: TimelineWindow { TimelineWindow(end: windowEnd, duration: span) }
    var selectedTime: Date? { selected?.start.addingTimeInterval(position) }

    init(historyURL: URL? = nil) {
        let store = HistoryStore(url: historyURL)
        history = store
        feed = TranscriptFeed(history: store)
        timeObserver = player.addPeriodicTimeObserver(forInterval: CMTime(seconds: 0.2, preferredTimescale: 600), queue: .main) { [weak self] time in
            MainActor.assumeIsolated {
                guard let self, self.selected != nil, time.seconds.isFinite else { return }
                self.position = min(max(0, time.seconds), self.selected?.duration ?? time.seconds)
            }
        }
    }

    func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.message = "recordings フォルダ、または区間録音のセッションフォルダを選択"
        if panel.runModal() == .OK, let url = panel.url { open(url) }
    }

    func restoreHistory() async {
        do {
            if let config = feed.configURL {
                do { try await history.importLog(config.deletingLastPathComponent().appendingPathComponent("dcr-transcribe.log")) }
                catch { self.error = "旧ログを取り込めません: \(error.localizedDescription)" }
            }
            let saved = try await history.restore()
            feed.restore(saved)
            playbackCutoff = saved.playbackCutoff
            indexedRecordings = Dictionary(saved.recordings.map { ($0.id, $0) }, uniquingKeysWith: { _, new in new })
            recordings = saved.recordings
            roots.formUnion(saved.roots)
            folder = saved.roots.first
            startScanning()
        } catch { self.error = "履歴を復元できません: \(error.localizedDescription)" }
    }

    private func persist(_ event: HistoryEvent) {
        let previous = persistence
        persistence = Task {
            await previous?.value
            do { try await history.append(event) }
            catch { self.error = "履歴を保存できません: \(error.localizedDescription)" }
        }
    }

    func flushHistory() async {
        await persistence?.value
        await feed.flushHistory()
    }

    func clearHistory(_ target: HistoryTarget) async {
        let date = Date()
        do {
            switch target {
            case .transcripts: try await feed.clearHistory(at: date)
            case .playback:
                await persistence?.value
                try await history.append(.clearPlayback(date))
                stopPlayback()
                playbackCutoff = date
                indexedRecordings = indexedRecordings.filter { $0.value.start > date }
                recordings = indexedRecordings.values.sorted { $0.start < $1.start }
            }
        } catch { self.error = "履歴をクリアできません: \(error.localizedDescription)" }
    }

    func watchFolder(_ url: URL) {
        let url = url.standardizedFileURL
        if roots.insert(url).inserted { persist(.root(url)) }
        if scanTask == nil { startScanning() }
    }

    func open(_ url: URL) {
        stopPlayback()
        folder = url.standardizedFileURL
        following = true
        watchFolder(url)
    }

    private func startScanning() {
        scanTask?.cancel()
        guard !roots.isEmpty else { scanTask = nil; return }
        scanTask = Task { [weak self] in
            guard let self else { return }
            while !Task.isCancelled {
                do {
                    let available = self.roots.filter { FileManager.default.fileExists(atPath: $0.path) }
                    let folders = available.filter { child in !available.contains { $0 != child && child.path.hasPrefix($0.path + "/") } }
                    let result = try await self.library.scan(Array(folders))
                    guard !Task.isCancelled else { return }
                    let updates = result.recordings.filter { $0.start > self.playbackCutoff && self.indexedRecordings[$0.id] != $0 }
                    if !updates.isEmpty {
                        for recording in updates { self.indexedRecordings[recording.id] = recording }
                        self.persist(.recordings(updates))
                        self.recordings = self.indexedRecordings.values.sorted { $0.start < $1.start }
                    }
                    if self.following { self.windowEnd = self.latest.addingTimeInterval(self.span * 0.03) }
                } catch is CancellationError { return }
                catch { self.error = "フォルダの読み込みに失敗: \(error.localizedDescription)" }
                try? await Task.sleep(for: .seconds(3))
            }
        }
    }

    func scroll(_ delta: Double) {
        following = false
        let proposed = windowEnd.addingTimeInterval(-delta * span / 700)
        let earliest = (recordings.map(\.start).min() ?? Date()).addingTimeInterval(span * 0.1)
        windowEnd = min(max(proposed, earliest), max(latest, Date()).addingTimeInterval(span * 0.1))
    }

    func updateTranscriptFollowing(atBottom: Bool) {
        following = atBottom
        if atBottom { windowEnd = latest.addingTimeInterval(span * 0.03) }
    }

    func zoom(_ factor: Double) { span = min(43200, max(30, span * factor)) }

    func goLatest() {
        stopPlayback()
        following = true
        windowEnd = latest.addingTimeInterval(span * 0.03)
    }

    func stopPlayback() {
        generation = UUID()
        player.pause()
        player.replaceCurrentItem(with: nil)
        feed.muteOutput(false)
        clearItemObservers()
        selected = nil
        position = 0
        playing = false
        loading = false
    }

    private func clearItemObservers() {
        if let endObserver { NotificationCenter.default.removeObserver(endObserver) }
        if let failureObserver { NotificationCenter.default.removeObserver(failureObserver) }
        endObserver = nil
        failureObserver = nil
        statusObserver = nil
    }

    func play(_ recording: Recording, at offset: Double = 0) {
        stopPlayback()
        error = nil
        selected = recording
        feed.muteOutput(true)
        selectedChannel = recording.channel
        following = false
        position = min(max(offset, 0), max(0, recording.duration - 0.01))
        loading = true
        let token = generation
        let item = AVPlayerItem(url: recording.url)
        endObserver = NotificationCenter.default.addObserver(forName: AVPlayerItem.didPlayToEndTimeNotification, object: item, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.generation == token else { return }
                self.goLatest()
            }
        }
        failureObserver = NotificationCenter.default.addObserver(forName: AVPlayerItem.failedToPlayToEndTimeNotification, object: item, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.generation == token else { return }
                self.playing = false
                self.loading = false
                self.error = "再生に失敗しました。音声ファイルを確認してください。"
            }
        }
        let initialOffset = position
        statusObserver = item.observe(\.status, options: [.new, .initial]) { [weak self] item, _ in
            Task { @MainActor in
                guard let self, self.generation == token else { return }
                switch item.status {
                case .readyToPlay:
                    self.player.seek(to: CMTime(seconds: initialOffset, preferredTimescale: 600), toleranceBefore: .zero, toleranceAfter: .zero) { [weak self] finished in
                        Task { @MainActor in
                            guard let self, self.generation == token, finished else { return }
                            self.loading = false
                            self.playing = true
                            self.player.playImmediately(atRate: self.speed)
                        }
                    }
                case .failed:
                    self.loading = false
                    self.playing = false
                    self.error = item.error?.localizedDescription ?? "この音声を再生できません"
                default: break
                }
            }
        }
        player.replaceCurrentItem(with: item)
    }

    func togglePlayback() {
        guard selected != nil else { navigate(1); return }
        guard !loading else { return }
        playing.toggle()
        if playing { player.playImmediately(atRate: speed) } else { player.pause() }
    }

    func seek(_ seconds: Double) {
        guard let selected, !loading else { return }
        position = min(max(seconds, 0), max(0, selected.duration - 0.01))
        player.seek(to: CMTime(seconds: position, preferredTimescale: 600), toleranceBefore: .zero, toleranceAfter: .zero)
    }

    func setSpeed(_ value: Float) {
        speed = value
        if playing { player.rate = value }
    }

    func navigate(_ direction: Int) {
        let choices = visibleRecordings.filter { $0.channel == selectedChannel }
        guard !choices.isEmpty else { return }
        let index: Int
        if let selected, let current = choices.firstIndex(of: selected) {
            index = min(max(current + direction, 0), choices.count - 1)
        } else {
            index = choices.firstIndex { $0.end >= window.start } ?? choices.count - 1
        }
        let recording = choices[index]
        if !window.overlaps(recording) { windowEnd = recording.start.addingTimeInterval(span * 0.65) }
        play(recording)
    }

    func toggleOutput(_ channel: Int) {
        feed.toggleOutput(channel)
    }

    func recording(for line: TranscriptLine) -> Recording? {
        let matches = recordings.filter { $0.channel == line.channel && $0.start <= line.date && $0.end > line.date }
        return matches.first(where: \.isSegment) ?? matches.first
    }

    func playTranscript(_ line: TranscriptLine) {
        guard let recording = recording(for: line) else { return }
        if !window.overlaps(recording) { windowEnd = line.date.addingTimeInterval(span * 0.65) }
        play(recording, at: recording.offset(at: line.date))
    }

    func click(channel: Int, date: Date) {
        selectedChannel = channel
        // 区間録音を優先し、同じ位置の全録音と二重に再生しない。
        let matches = visibleRecordings.filter { $0.channel == channel && $0.start <= date && $0.end > date }
        if let recording = matches.sorted(by: { $0.isSegment && !$1.isSegment }).first {
            play(recording, at: recording.offset(at: date))
        }
    }

    func shutdown() {
        feed.shutdown()
        scanTask?.cancel()
        stopPlayback()
        if let timeObserver { player.removeTimeObserver(timeObserver) }
        timeObserver = nil
    }
}
