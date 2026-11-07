import SwiftUI

struct MonitorView: View {
    @ObservedObject var model: MonitorModel
    @ObservedObject private var feed: TranscriptFeed
    @State private var settingsShown = false
    @State private var mobileShown = false
    @AppStorage("collapsedTranscriptChannels") private var collapsedChannelIDs = ""

    private var collapsedChannels: Set<Int> {
        Set(collapsedChannelIDs.split(separator: ",").compactMap { Int($0) })
    }

    init(model: MonitorModel) {
        self.model = model
        self.feed = model.feed
    }

    var body: some View {
        VStack(spacing: 0) {
            header.padding(.horizontal, 16).padding(.vertical, 8)
            Divider()
            if let problem = feed.deviceProblem {
                Text(problem).font(.system(size: 12, weight: .semibold)).foregroundStyle(.red)
                    .frame(maxWidth: .infinity, alignment: .leading).padding(8).background(Color.red.opacity(0.12))
            }
            transcripts
            Divider()
            timeline.padding(.horizontal, 12).padding(.vertical, 3)
            Divider()
            playback.padding(.horizontal, 16).padding(.vertical, 7)
        }
        .background(Color(red: 0.035, green: 0.055, blue: 0.085))
        .tint(accent)
        .sheet(isPresented: $settingsShown) {
            AudioSettingsView(monitor: model, feed: feed)
        }

        .confirmationDialog("履歴をクリアしますか？", isPresented: Binding(
            get: { model.clearHistoryTarget != nil }, set: { if !$0 { model.clearHistoryTarget = nil } }),
            titleVisibility: .visible, presenting: model.clearHistoryTarget) { target in
                Button(target == .transcripts ? "文字起こし履歴をクリア" : "プレイバック履歴をクリア", role: .destructive) {
                    Task { await model.clearHistory(target) }
                }
                Button("キャンセル", role: .cancel) {}
            } message: { _ in Text("表示履歴をクリアします。元の録音ファイルは削除しません。") }
        .alert("確認", isPresented: Binding(get: { model.error != nil || feed.error != nil }, set: { if !$0 { model.error = nil; feed.error = nil } })) {
            Button("閉じる") { model.error = nil; feed.error = nil }
        } message: { Text(model.error ?? feed.error ?? "") }
        .onChange(of: feed.segmentsDirectory) { _, url in
            if let url { model.watchFolder(url) }
        }
        .onChange(of: feed.recordingsDirectory) { _, url in
            if let url, model.folder != url { model.open(url) }
        }
    }

    private var header: some View {
        HStack(spacing: 12) {
            Text("文字起こし").font(.system(size: 14, weight: .semibold))
            Circle().fill(feed.connected ? accent : muted).frame(width: 5, height: 5)
            Text(feed.status).font(.system(size: 11)).foregroundStyle(muted)
            Spacer()
            TimelineView(.periodic(from: Date(timeIntervalSince1970: floor(Date().timeIntervalSince1970)), by: 1)) { context in
                let time = Calendar.current.dateComponents([.minute, .second], from: context.date)
                let signaling = time.minute == 0 && (time.second ?? 60) < 5
                let illuminated = signaling && (time.second ?? 0).isMultiple(of: 2)
                HStack(spacing: 10) {
                    if model.showAnalog { AnalogClock(date: context.date).frame(width: 40, height: 40).accessibilityLabel("現在時刻のアナログ時計") }
                    if model.showDigital {
                        Text(context.date.formatted(.dateTime.hour(.twoDigits(amPM: .omitted)).minute(.twoDigits).second(.twoDigits)))
                            .font(.system(size: 24, weight: .regular, design: .monospaced)).monospacedDigit()
                    }
                }
                .opacity(signaling && !illuminated ? 0.35 : 1)
                .padding(.horizontal, 6).padding(.vertical, 3)
                .background(illuminated ? accent.opacity(0.25) : .clear, in: RoundedRectangle(cornerRadius: 5))
                .overlay(RoundedRectangle(cornerRadius: 5).stroke(illuminated ? accent : .clear, lineWidth: 1))
            }
            Button { mobileShown.toggle() } label: { Image(systemName: "qrcode") }
                .accessibilityLabel("モバイル版を開く").help("モバイル版を開く")
                .popover(isPresented: $mobileShown) { MobileShareView(url: feed.mobileURL) }
            Button { settingsShown = true } label: { Image(systemName: "slider.horizontal.3") }.accessibilityLabel("音声デバイス設定")
        }
    }

    private var transcripts: some View {
        GeometryReader { geometry in
            let ids = model.channels.filter { !collapsedChannels.contains($0) }
            let width = max(220, geometry.size.width / CGFloat(max(1, ids.count)))
            ScrollView(.horizontal) {
                HStack(spacing: 0) {
                    ForEach(ids, id: \.self) { channel in
                        TranscriptColumn(model: model, feed: feed, channel: channel)
                            .frame(width: width, height: geometry.size.height)
                            .clipped()
                    }
                }
            }
        }.frame(maxHeight: .infinity)
    }

    private var timeline: some View {
        VStack(spacing: 1) {
            HStack(spacing: 10) {
                Text(model.window.start.formatted(.dateTime.month(.twoDigits).day(.twoDigits))).font(.system(size: 10, design: .monospaced)).foregroundStyle(muted)
                if !model.following { Text("過去を閲覧中").font(.system(size: 10)).foregroundStyle(.orange) }
                Spacer()
                Button { model.zoom(0.5) } label: { Image(systemName: "plus.magnifyingglass") }
                Button { model.zoom(2) } label: { Image(systemName: "minus.magnifyingglass") }
                Button { model.goLatest() } label: { Image(systemName: "arrow.right.to.line") }
                    .accessibilityLabel("最新へ戻る").help("最新へ戻る")
            }.controlSize(.mini)
            TimelineChart(model: model, feed: feed)
        }
    }

    private var playback: some View {
        HStack(spacing: 12) {
            Button { model.navigate(-1) } label: { Image(systemName: "backward.end.fill") }
            Button { model.togglePlayback() } label: { Image(systemName: model.loading ? "hourglass" : model.playing ? "pause.fill" : "play.fill") }
                .disabled(model.recordings.isEmpty || model.loading)
            Button { model.navigate(1) } label: { Image(systemName: "forward.end.fill") }
            if let selected = model.selected {
                Text("\(selected.channel + 1)ch").font(.system(size: 11, design: .monospaced)).foregroundStyle(accent)
            }
            Slider(value: Binding(get: { model.position }, set: { model.seek($0) }), in: 0...max(0.01, model.selected?.duration ?? 1))
                .disabled(model.selected == nil || model.loading).accessibilityLabel("再生位置")
            Text(duration(model.position) + " / " + duration(model.selected?.duration ?? 0))
                .font(.system(size: 10, design: .monospaced)).foregroundStyle(muted)
            Picker("速度", selection: Binding(get: { model.speed }, set: { model.setSpeed($0) })) {
                Text("0.75×").tag(Float(0.75)); Text("1×").tag(Float(1)); Text("1.5×").tag(Float(1.5)); Text("2×").tag(Float(2))
            }.labelsHidden().frame(width: 70)
        }.controlSize(.small)
    }

    private func duration(_ seconds: Double) -> String {
        let total = Int(max(0, seconds))
        return String(format: "%02d:%02d", total / 60, total % 60)
    }
}

struct AnalogClock: View {
    let date: Date
    var body: some View {
        Canvas { context, size in
            let center = CGPoint(x: size.width / 2, y: size.height / 2)
            let radius = min(size.width, size.height) / 2 - 2
            context.stroke(Path(ellipseIn: CGRect(x: center.x - radius, y: center.y - radius, width: radius * 2, height: radius * 2)), with: .color(muted.opacity(0.4)), lineWidth: 1)
            for i in 0..<60 {
                let angle = Double(i) * .pi / 30 - .pi / 2
                let outer = radius - 4
                let inner = outer - (i % 5 == 0 ? 6.0 : 2.5)
                var mark = Path()
                mark.move(to: CGPoint(x: center.x + cos(angle) * inner, y: center.y + sin(angle) * inner))
                mark.addLine(to: CGPoint(x: center.x + cos(angle) * outer, y: center.y + sin(angle) * outer))
                context.stroke(mark, with: .color(muted), lineWidth: i % 5 == 0 ? 1.5 : 0.5)
            }
            let c = Calendar.current.dateComponents([.hour, .minute, .second], from: date)
            let seconds = Double(c.second ?? 0), minutes = Double(c.minute ?? 0), hours = Double((c.hour ?? 0) % 12)
            func hand(_ fraction: Double, _ length: Double, _ color: Color, _ width: Double) {
                let angle = fraction * .pi * 2 - .pi / 2
                var path = Path()
                path.move(to: center)
                path.addLine(to: CGPoint(x: center.x + cos(angle) * radius * length, y: center.y + sin(angle) * radius * length))
                context.stroke(path, with: .color(color), style: StrokeStyle(lineWidth: width, lineCap: .round))
            }
            hand((hours + minutes / 60) / 12, 0.45, .white, 3)
            hand((minutes + seconds / 60) / 60, 0.67, .white, 2)
            hand(seconds / 60, 0.80, accent, 1)
            context.fill(Path(ellipseIn: CGRect(x: center.x - 2, y: center.y - 2, width: 4, height: 4)), with: .color(accent))
        }
    }
}
