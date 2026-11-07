import SwiftUI
import AppKit

/// TUI と同じく「チャンネル見出し → 本文 → 音量 → 状態」を列内で完結させる。
struct TranscriptColumn: View {
    @ObservedObject var model: MonitorModel
    @ObservedObject var feed: TranscriptFeed
    let channel: Int
    @State private var bottomY: CGFloat = 0
    @State private var viewportHeight: CGFloat = 0

    private var lines: [TranscriptLine] { feed.lines.filter { $0.channel == channel } }
    private var reading: ChannelReading? { feed.connected ? feed.readings[channel] : nil }

    private var monitoring: Bool { feed.connected && feed.selectedOutputs.contains(channel) }
    private var glowing: Bool { monitoring && model.selected == nil && reading?.receiving == true }
    private var monitorColor: Color { glowing ? Color(red: 0.65, green: 1, blue: 0.72) : .green }

    var body: some View {
        VStack(spacing: 0) {
            Button { model.toggleOutput(channel) } label: {
                HStack(spacing: 6) {
                    Circle().fill(transcribeColor).frame(width: 7, height: 7)
                        .accessibilityLabel("Transcribe: " + transcribeText)
                        .help("Transcribe: " + transcribeText)
                    Text("\(channel + 1): \(feed.channels[channel] ?? "")")
                        .font(.system(size: 13, weight: .semibold))
                        .fixedSize(horizontal: false, vertical: true)
                        .foregroundStyle(monitoring ? monitorColor : .white)
                        .shadow(color: glowing ? .green.opacity(0.85) : .clear, radius: 5)
                    if monitoring {
                        Text(model.selected == nil ? "出力中" : "一時ミュート").font(.system(size: 9)).foregroundStyle(monitorColor)
                    }
                    Spacer(minLength: 0)
                }.padding(.horizontal, 12).padding(.vertical, 9)
                    .background(monitoring ? Color.green.opacity(glowing ? 0.25 : 0.07) : .clear)
            }.buttonStyle(.plain)
            Divider().opacity(0.5)
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(lines) { line in
                            let available = model.recording(for: line) != nil
                            Button { model.playTranscript(line) } label: {
                            HStack(alignment: .top, spacing: 5) {
                                (
                                    Text("[" + line.date.formatted(.dateTime.hour(.twoDigits(amPM: .omitted)).minute(.twoDigits).second(.twoDigits)) + "] ")
                                        .foregroundColor(line.partial ? .yellow : accent)
                                        .font(.system(size: 12, weight: .medium, design: .monospaced))
                                    + Text(line.text)
                                        .foregroundColor(line.partial ? muted : .white)
                                        .font(line.partial ? .system(size: 15).italic() : .system(size: 15))
                                )
                                .fixedSize(horizontal: false, vertical: true)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                if available {
                                    Image(systemName: "play.circle").font(.system(size: 11)).foregroundStyle(muted)
                                }
                            }
                            }.buttonStyle(.plain)
                                .disabled(!available)
                                .help(available ? "クリックしてこの交信を再生" : "対応する録音の確定を待っています")
                                .contextMenu {
                                    Button("文字起こしをコピー") {
                                        NSPasteboard.general.clearContents()
                                        NSPasteboard.general.setString(line.text, forType: .string)
                                    }
                                }
                        }
                        Color.clear.frame(height: 1).id("latest")
                            .background(GeometryReader { geometry in
                                Color.clear.preference(key: TranscriptOffsetKey.self,
                                    value: geometry.frame(in: .named("transcript-\(channel)")).maxY)
                            })
                    }.padding(12)
                }
                .coordinateSpace(name: "transcript-\(channel)")
                .background(GeometryReader { geometry in
                    Color.clear.onAppear { viewportHeight = geometry.size.height }
                        .onChange(of: geometry.size.height) { _, height in viewportHeight = height }
                })
                .background(TranscriptScrollObserver { delta in
                    if delta > 0 { model.updateTranscriptFollowing(atBottom: false) }
                    else if bottomY <= viewportHeight + 16 { model.updateTranscriptFollowing(atBottom: true) }
                })
                .onPreferenceChange(TranscriptOffsetKey.self) { bottomY = $0 }
                .onAppear { if model.following { proxy.scrollTo("latest", anchor: .bottom) } }
                .onChange(of: lines) { _, _ in if model.following { proxy.scrollTo("latest", anchor: .bottom) } }
                .onChange(of: model.following) { _, value in if value { proxy.scrollTo("latest", anchor: .bottom) } }
            }.frame(maxHeight: .infinity)
            VStack(spacing: 6) {
                if let message = reading?.recordingError {
                    Text(message).font(.system(size: 10)).foregroundStyle(.red).frame(maxWidth: .infinity, alignment: .leading)
                }
                if let message = reading?.warning {
                    Text(message).font(.system(size: 10)).foregroundStyle(.yellow).frame(maxWidth: .infinity, alignment: .leading)
                }
                GeometryReader { geometry in
                    ZStack(alignment: .leading) {
                        Rectangle().fill(.white.opacity(0.08))
                        Rectangle().fill(reading?.receiving == true ? accent : muted)
                            .frame(width: geometry.size.width * levelRatio)
                        if let reading {
                            Rectangle().fill(.red).frame(width: 1)
                                .offset(x: geometry.size.width * min(1, max(0, (reading.thresholdDB + 60) / 60)))
                        }
                    }
                }.frame(height: 4)
                ViewThatFits(in: .horizontal) {
                    HStack(spacing: 6) { levelText; Spacer(minLength: 2); inputText }
                    VStack(alignment: .leading, spacing: 3) { levelText; inputText }.frame(maxWidth: .infinity, alignment: .leading)
                }
            }.padding(.horizontal, 12).padding(.vertical, 8)
        }
        .overlay(alignment: .trailing) { Rectangle().fill(.white.opacity(0.12)).frame(width: 1) }
        .overlay(alignment: .top) {
            Rectangle().fill(monitoring ? monitorColor : .clear).frame(height: 2)
                .shadow(color: glowing ? .green : .clear, radius: 6)
        }
    }

    private var levelText: some View {
        Text(reading.map { "\($0.levelDB) dB" } ?? "— dB")
            .font(.system(size: 10, design: .monospaced)).foregroundStyle(muted)
    }
    private var inputText: some View {
        Text("VAD: " + (reading.map { $0.receiving ? "音声" : "無音" } ?? "—"))
            .font(.system(size: 10)).foregroundStyle(reading?.receiving == true ? accent : muted)
    }

    private var levelRatio: Double {
        guard let reading else { return 0 }
        return min(1, max(0, Double(reading.levelDB + 60) / 60))
    }
    private var transcribeText: String {
        guard let reading else { return "未取得" }
        switch reading.transcribe { case 0: return "無通信"; case 1: return "正常"; default: return "エラー" }
    }
    private var transcribeColor: Color {
        guard let reading else { return muted }
        switch reading.transcribe { case 0: return .yellow; case 1: return .green; default: return .red }
    }

}

private struct TranscriptOffsetKey: PreferenceKey {
    static var defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = nextValue() }
}
