import SwiftUI

struct AudioSettingsView: View {
    @ObservedObject var monitor: MonitorModel
    @ObservedObject var feed: TranscriptFeed
    @StateObject private var model = AudioSettingsModel()
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("音声デバイス設定").font(.headline)
            if model.settings != nil {
                Form {
                    devicePicker("入力デバイス", selection: $model.input, devices: model.settings?.inputs ?? [])
                    devicePicker("出力デバイス", selection: $model.output, devices: model.settings?.outputs ?? [])
                    Picker("サンプルレート", selection: $model.rate) {
                        if !model.rates.contains(model.rate) {
                            Text("\(model.rate) Hz（非対応）").tag(model.rate)
                        }
                        ForEach(model.rates, id: \.self) { rate in Text("\(rate) Hz").tag(rate) }
                    }
                }.disabled(model.working)
                if model.rates.isEmpty {
                    Text("選択した入力・出力で共通のサンプルレートを取得できません。接続を確認し、再読み込みしてください。")
                        .font(.caption).foregroundStyle(.orange)
                }
                if let input = model.settings?.device(model.input, input: true), let channels = model.settings?.audio.channels,
                   let physical = input.ranges.filter({ $0.min <= model.rate && model.rate <= $0.max }).map(\.channels).max(), physical < channels {
                    Text("この入力は\(physical)chです。設定の\(channels)chのうち、残りは無音になります。")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
            if let error = model.error { Text(error).font(.callout).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true) }
            Text(model.saved ? "保存しました。次回のアプリ起動時に適用されます。" : "変更は保存後、次回のアプリ起動時に適用されます。")
                .font(.caption).foregroundStyle(model.saved ? .green : .secondary)
            HStack {
                Button("設定ファイルを選択…") { feed.chooseConfig() }.disabled(feed.busy || model.working)
                Button("再読み込み") { Task { await model.load(feed.configURL) } }.disabled(model.working || feed.configURL == nil)
                if model.working { ProgressView().controlSize(.small) }
            }
            HStack {
                Button("録音フォルダを選択") { monitor.chooseFolder() }
                Spacer()
                Button("閉じる") { dismiss() }.keyboardShortcut(.cancelAction).disabled(model.working)
                Button("保存") { Task { await model.save() } }.keyboardShortcut(.defaultAction).disabled(!model.canSave)
            }
        }.padding(24).frame(width: 520)
        .task(id: feed.configURL) { await model.load(feed.configURL) }
    }

    private func devicePicker(_ title: String, selection: Binding<String>, devices: [AudioSettings.Device]) -> some View {
        Picker(title, selection: selection) {
            Text("システムのデフォルト").tag("default")
            if selection.wrappedValue != "default", !devices.contains(where: { $0.name == selection.wrappedValue }) {
                Text(selection.wrappedValue + "（未接続）").tag(selection.wrappedValue)
            }
            ForEach(devices) { device in Text(device.name).tag(device.name) }
        }
    }
}
