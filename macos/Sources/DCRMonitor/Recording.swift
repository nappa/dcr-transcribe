import Foundation
import AVFoundation

struct Recording: Identifiable, Equatable, Sendable, Codable {
    var id: String { url.path }
    let url: URL
    let channel: Int
    let start: Date
    let duration: Double
    let isSegment: Bool
    let estimated: Bool
    var end: Date { start.addingTimeInterval(duration) }

    func offset(at date: Date) -> Double {
        min(max(0, date.timeIntervalSince(start)), max(0, duration - 0.01))
    }
}

struct SegmentMetadata: Codable {
    let channel: Int
    let start_unix_ms: UInt64
    let duration_ms: UInt64
}

struct ScanResult: Sendable {
    var recordings: [Recording]
    var skipped: Int
}

/// オーディオ本体を展開せずに索引を読む。UI の MainActor からは呼ばない。
actor RecordingLibrary {
    private var cache: [URL: (Date, Recording)] = [:]

    func scan(_ folder: URL) throws -> ScanResult { try scan([folder]) }

    func scan(_ folders: [URL]) throws -> ScanResult {
        let fm = FileManager.default
        let keys: [URLResourceKey] = [.contentModificationDateKey, .isRegularFileKey]
        var found: [Recording] = []
        var seen = Set<URL>()
        var skipped = 0
        for folder in folders {
            guard let enumerator = fm.enumerator(at: folder, includingPropertiesForKeys: keys,
                                                 options: [.skipsHiddenFiles]) else {
                throw CocoaError(.fileReadNoSuchFile)
            }
            for case let url as URL in enumerator {
                try Task.checkCancellation()
                guard ["wav", "aac", "m4a"].contains(url.pathExtension.lowercased()) else { continue }
                guard (try? url.resourceValues(forKeys: Set(keys)).isRegularFile) == true else { continue }
                let modified = (try? url.resourceValues(forKeys: Set(keys)).contentModificationDate) ?? .distantPast
                seen.insert(url)
                if let (date, recording) = cache[url], date == modified {
                    found.append(recording)
                    continue
                }
                do {
                    if let recording = try read(url) {
                        found.append(recording)
                        cache[url] = (modified, recording)
                    } else { skipped += 1 }
                } catch { skipped += 1 }
            }
        }
        cache = cache.filter { seen.contains($0.key) }
        return ScanResult(recordings: found.sorted { $0.start < $1.start }, skipped: skipped)
    }

    private func read(_ url: URL) throws -> Recording? {
        let metadataURL = url.appendingPathExtension("json")
        if FileManager.default.fileExists(atPath: metadataURL.path) {
            let metadata = try JSONDecoder().decode(SegmentMetadata.self, from: Data(contentsOf: metadataURL))
            guard metadata.channel >= 0, metadata.duration_ms > 0 else { return nil }
            return Recording(url: url, channel: metadata.channel,
                             start: Date(timeIntervalSince1970: Double(metadata.start_unix_ms) / 1000),
                             duration: Double(metadata.duration_ms) / 1000, isSegment: true, estimated: true)
        }
        // 旧録音はファイル名の時刻で表示する。欠落や時計補正に対応する正確な位置対応ではない。
        let name = url.deletingPathExtension().lastPathComponent
        let parts = name.split(separator: "_").map(String.init)
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.dateFormat = "yyyyMMdd_HHmmss"
        formatter.isLenient = false
        let channel: Int
        let start: Date
        let segment: Bool
        if parts.count >= 4, parts[0] == "channel", let id = Int(parts[1]), id >= 0,
           let date = formatter.date(from: parts[2] + "_" + parts[3]) {
            channel = id; start = date; segment = false
        } else if parts.count >= 4, parts[0].hasPrefix("ch"), let id = Int(parts[0].dropFirst(2)), id >= 0,
                  let milliseconds = Double(parts[2]), milliseconds >= 0, milliseconds < 1000,
                  let session = formatter.date(from: url.deletingLastPathComponent().lastPathComponent) {
            let day = url.deletingLastPathComponent().lastPathComponent.prefix(8)
            guard var date = formatter.date(from: String(day) + "_" + parts[1]) else { return nil }
            // 旧セッション名はローカル時刻。日付またぎは開始時刻から推定する。
            if date.timeIntervalSince(session) < -60 {
                guard let nextDay = Calendar.current.date(byAdding: .day, value: 1, to: date) else { return nil }
                date = nextDay
            }
            channel = id; start = date.addingTimeInterval(milliseconds / 1000); segment = true
        } else { return nil }
        // 書き込み中の全録音は対象外。区間録音は確定後にメタデータが公開される。
        let modified = try url.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate ?? Date()
        guard Date().timeIntervalSince(modified) > 15 else { return nil }
        let audio = try AVAudioFile(forReading: url)
        let duration = Double(audio.length) / audio.fileFormat.sampleRate
        guard duration.isFinite, duration > 0 else { return nil }
        return Recording(url: url, channel: channel, start: start, duration: duration, isSegment: segment, estimated: true)
    }
}

struct TimelineWindow {
    var end: Date
    var duration: Double
    var start: Date { end.addingTimeInterval(-duration) }
    func date(at x: Double, width: Double) -> Date {
        start.addingTimeInterval(min(max(x / max(width, 1), 0), 1) * duration)
    }
    func x(at date: Date, width: Double) -> Double {
        date.timeIntervalSince(start) / duration * width
    }
    func overlaps(_ recording: Recording) -> Bool { recording.end > start && recording.start < end }
}
