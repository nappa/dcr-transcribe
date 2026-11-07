import Foundation
import Darwin

struct HistorySnapshot {
    var lines: [TranscriptLine] = []
    var recordings: [Recording] = []
    var channels: [Int: String] = [:]
    var roots: [URL] = []
    var transcriptCutoff = Date.distantPast
    var playbackCutoff = Date.distantPast
}

enum HistoryEvent: Codable {
    case transcripts([TranscriptLine], [Int: String])
    case recordings([Recording])
    case root(URL)
    case clearTranscripts(Date)
    case clearPlayback(Date)
    case importedLog(URL)
}

/// 追記形式で保存する。途中まで書かれた最後の行だけを復旧時に取り除く。
actor HistoryStore {
    static var defaultURL: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("DCRMonitor/history.jsonl")
    }
    private let url: URL?
    private var loaded = false
    private var file: FileHandle?
    private var lines: [String: TranscriptLine] = [:]
    private var recordings: [String: Recording] = [:]
    private var channels: [Int: String] = [:]
    private var roots = Set<URL>()
    private var importedLogs = Set<URL>()
    private var transcriptCutoff = Date.distantPast
    private var playbackCutoff = Date.distantPast

    init(url: URL?) { self.url = url }
    deinit { try? file?.close() }

    func restore() throws -> HistorySnapshot {
        try load()
        return HistorySnapshot(lines: lines.values.sorted { $0.date < $1.date },
                               recordings: recordings.values.sorted { $0.start < $1.start },
                               channels: channels, roots: roots.sorted { $0.path < $1.path },
                               transcriptCutoff: transcriptCutoff, playbackCutoff: playbackCutoff)
    }

    func append(_ event: HistoryEvent) throws {
        try load()
        if let file {
            var data = try JSONEncoder().encode(event)
            data.append(0x0a)
            try file.write(contentsOf: data)
            try file.synchronize()
        }
        apply(event)
        switch event {
        case .clearTranscripts, .clearPlayback: try? compact()
        default: break
        }
    }

    private func compact() throws {
        guard let url else { return }
        var events: [HistoryEvent] = [.clearTranscripts(transcriptCutoff), .clearPlayback(playbackCutoff),
                                      .transcripts(Array(lines.values), channels), .recordings(Array(recordings.values))]
        events += roots.map(HistoryEvent.root)
        events += importedLogs.map(HistoryEvent.importedLog)
        let temp = url.deletingLastPathComponent().appendingPathComponent(".history-" + UUID().uuidString)
        let fm = FileManager.default
        guard fm.createFile(atPath: temp.path, contents: nil, attributes: [.posixPermissions: 0o600]) else { throw CocoaError(.fileWriteUnknown) }
        defer { try? fm.removeItem(at: temp) }
        let handle = try FileHandle(forWritingTo: temp)
        defer { try? handle.close() }
        for event in events {
            var data = try JSONEncoder().encode(event)
            data.append(0x0a)
            try handle.write(contentsOf: data)
        }
        try handle.synchronize()
        guard rename(temp.path, url.path) == 0 else { throw CocoaError(.fileWriteUnknown) }
        try? file?.close()
        file = nil
        loaded = false
        try load()
    }

    /// 旧版の確定文字起こしを一度だけ取り込む。ログ本文そのものは履歴へ保存しない。
    func importLog(_ url: URL) throws {
        try load()
        let url = url.standardizedFileURL
        guard !importedLogs.contains(url), FileManager.default.fileExists(atPath: url.path) else { return }
        struct Entry: Decodable { let channel: Int; let timestamp: String; let timestamp_seconds: Double; let text: String; let is_partial: Bool }
        let handle = try FileHandle(forReadingFrom: url)
        defer { try? handle.close() }
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let plain = ISO8601DateFormatter()
        var pending = Data()
        var batch: [TranscriptLine] = []
        while let data = try handle.read(upToCount: 65536), !data.isEmpty {
            pending.append(data)
            while let end = pending.firstIndex(of: 0x0a) {
                let row = pending[..<end]
                if let start = row.firstIndex(of: 0x7b),
                   let entry = try? JSONDecoder().decode(Entry.self, from: Data(row[start...])), !entry.is_partial,
                   let date = formatter.date(from: entry.timestamp) ?? plain.date(from: entry.timestamp), date > transcriptCutoff {
                    let id = "\(entry.channel)-\(entry.timestamp)-\(entry.timestamp_seconds)-\(entry.text)"
                    if lines[id] == nil {
                        batch.append(TranscriptLine(id: id, channel: entry.channel, date: date, text: entry.text, partial: false))
                    }
                }
                pending.removeSubrange(...end)
                if batch.count >= 128 { try append(.transcripts(batch, [:])); batch.removeAll(keepingCapacity: true) }
            }
            if pending.count > 1_048_576 { pending.removeAll(keepingCapacity: false) }
        }
        if !batch.isEmpty { try append(.transcripts(batch, [:])) }
        try append(.importedLog(url))
    }

    private func load() throws {
        guard !loaded else { return }
        guard let url else { loaded = true; return }
        let fm = FileManager.default
        try fm.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true,
                               attributes: [.posixPermissions: 0o700])
        if !fm.fileExists(atPath: url.path) {
            guard fm.createFile(atPath: url.path, contents: nil, attributes: [.posixPermissions: 0o600]) else {
                throw CocoaError(.fileWriteUnknown)
            }
        }
        let data = try Data(contentsOf: url)
        lines = [:]; recordings = [:]; channels = [:]; roots = []; importedLogs = []
        transcriptCutoff = .distantPast; playbackCutoff = .distantPast
        let completeEnd = data.lastIndex(of: 0x0a).map { $0 + 1 } ?? 0
        // 完了行の破損は黙って捨てず、元のファイルを保ったままエラーにする。
        for line in data.prefix(completeEnd).split(separator: 0x0a) {
            apply(try JSONDecoder().decode(HistoryEvent.self, from: Data(line)))
        }
        let handle = try FileHandle(forUpdating: url)
        if completeEnd < data.count { try handle.truncate(atOffset: UInt64(completeEnd)) }
        try handle.seekToEnd()
        file = handle
        loaded = true
    }

    private func apply(_ event: HistoryEvent) {
        switch event {
        case let .transcripts(entries, names):
            channels.merge(names, uniquingKeysWith: { _, new in new })
            for entry in entries where !entry.partial && entry.date > transcriptCutoff { lines[entry.id] = entry }
        case let .recordings(entries):
            for entry in entries where entry.start > playbackCutoff { recordings[entry.id] = entry }
        case let .root(url): roots.insert(url.standardizedFileURL)
        case let .importedLog(url): importedLogs.insert(url.standardizedFileURL)
        case let .clearTranscripts(date):
            transcriptCutoff = max(transcriptCutoff, date)
            lines = lines.filter { $0.value.date > transcriptCutoff }
        case let .clearPlayback(date):
            playbackCutoff = max(playbackCutoff, date)
            recordings = recordings.filter { $0.value.start > playbackCutoff }
        }
    }
}
