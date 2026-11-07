import SwiftUI
import AppKit

@main
struct DCRMonitorApp: App {
    @NSApplicationDelegateAdaptor(MonitorAppDelegate.self) private var appDelegate
    @StateObject private var model = MonitorModel(historyURL: HistoryStore.defaultURL)
    @State private var didLaunch = false
    @AppStorage("collapsedTranscriptChannels") private var collapsedChannelIDs = ""

    private func channelVisible(_ channel: Int) -> Binding<Bool> {
        Binding(get: {
            !collapsedChannelIDs.split(separator: ",").compactMap { Int($0) }.contains(channel)
        }, set: { visible in
            var ids = Set(collapsedChannelIDs.split(separator: ",").compactMap { Int($0) })
            if visible { ids.remove(channel) } else { ids.insert(channel) }
            collapsedChannelIDs = ids.sorted().map(String.init).joined(separator: ",")
        })
    }
    var body: some Scene {
        Window("DCR Monitor", id: "main") {
            MonitorView(model: model)
                .frame(minWidth: 780, minHeight: 600)
                .preferredColorScheme(.dark)
                .onAppear {
                    NSApplication.shared.setActivationPolicy(.regular)
                    NSApplication.shared.activate(ignoringOtherApps: true)
                    appDelegate.model = model
                    guard !didLaunch else { return }
                    didLaunch = true
                    if let i = CommandLine.arguments.firstIndex(of: "--config"), CommandLine.arguments.count > i + 1 {
                        let configURL = URL(fileURLWithPath: CommandLine.arguments[i + 1])
                        model.feed.configURL = configURL
                        UserDefaults.standard.set(configURL, forKey: "engineConfigURL")
                    }
                    Task {
                        await model.restoreHistory()
                        if let i = CommandLine.arguments.firstIndex(of: "--recordings"), CommandLine.arguments.count > i + 1 {
                            model.open(URL(fileURLWithPath: CommandLine.arguments[i + 1]))
                        }
                        await model.feed.start()
                    }
                }
                .onReceive(NotificationCenter.default.publisher(for: NSApplication.willTerminateNotification)) { _ in model.shutdown() }
        }
        .defaultSize(width: 1180, height: 800)
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("録音フォルダを開く…") { model.chooseFolder() }.keyboardShortcut("o")
            }
            CommandMenu("再生") {
                Button("再生 / 一時停止") { model.togglePlayback() }.keyboardShortcut(.space, modifiers: [])
                Button("最新へ戻る") { model.goLatest() }.keyboardShortcut(.escape, modifiers: [])
                Button("前の交信") { model.navigate(-1) }.keyboardShortcut(.leftArrow, modifiers: [])
                Button("次の交信") { model.navigate(1) }.keyboardShortcut(.rightArrow, modifiers: [])
                Divider()
                ForEach(0..<9) { channel in
                    Button("\(channel + 1)ch 音声出力オン／オフ") { model.toggleOutput(channel) }
                        .keyboardShortcut(KeyEquivalent(Character(String(channel + 1))), modifiers: [])
                }
            }
            CommandMenu("履歴") {
                Button("文字起こし履歴をクリア…") { model.clearHistoryTarget = .transcripts }
                Button("プレイバック履歴をクリア…") { model.clearHistoryTarget = .playback }
            }
            CommandMenu("表示") {
                Menu("表示チャンネル") {
                    Button("すべて表示") { collapsedChannelIDs = "" }
                    ForEach(model.channels, id: \.self) { channel in
                        Toggle("\(channel + 1)ch", isOn: channelVisible(channel))
                    }
                }
                Divider()
                Toggle("アナログ時計", isOn: $model.showAnalog)
                Toggle("デジタル時計", isOn: $model.showDigital)
            }
        }
    }
}

@MainActor
final class MonitorAppDelegate: NSObject, NSApplicationDelegate {
    weak var model: MonitorModel?
    private var terminating = false
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard let model else { return .terminateNow }
        if terminating { return .terminateLater }
        terminating = true
        model.shutdown()
        Task {
            await model.feed.shutdownAndWait()
            await model.flushHistory()
            sender.reply(toApplicationShouldTerminate: true)
        }
        return .terminateLater
    }
}
