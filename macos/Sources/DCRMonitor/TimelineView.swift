import SwiftUI
import AppKit

let accent = Color(red: 0.35, green: 0.88, blue: 0.75)
let panel = Color(red: 0.075, green: 0.10, blue: 0.14)
let muted = Color(red: 0.55, green: 0.61, blue: 0.69)

enum TimelineMetrics {
    static let rowHeight: CGFloat = 16
    static let rulerHeight: CGFloat = 16
    static func height(channels: Int) -> CGFloat {
        rulerHeight + CGFloat(channels) * rowHeight
    }
}

struct TimelineChart: View {
    @ObservedObject var model: MonitorModel
    @ObservedObject var feed: TranscriptFeed
    let rowHeight = TimelineMetrics.rowHeight
    let rulerHeight = TimelineMetrics.rulerHeight
    var body: some View {
        HStack(spacing: 0) {
            VStack(spacing: 0) {
                Text("").font(.system(size: 10, weight: .bold, design: .monospaced)).foregroundStyle(muted).frame(height: rulerHeight)
                ForEach(model.channels, id: \.self) { channel in
                    let monitoring = feed.connected && feed.selectedOutputs.contains(channel)
                    let glowing = monitoring && model.selected == nil && feed.readings[channel]?.receiving == true
                    let monitorColor = glowing ? Color(red: 0.65, green: 1, blue: 0.72) : Color.green
                    Button { model.toggleOutput(channel) } label: {
                        HStack {
                            Circle().fill(monitoring ? monitorColor : muted.opacity(0.4)).frame(width: 6, height: 6)
                                .shadow(color: glowing ? .green : .clear, radius: 5)
                            Text("\(channel + 1)ch").font(.system(size: 10, weight: .semibold, design: .monospaced))
                                .foregroundStyle(monitoring ? monitorColor : .white)
                        }.frame(maxWidth: .infinity, minHeight: rowHeight)
                            .background(monitoring ? Color.green.opacity(glowing ? 0.25 : 0.07) : .clear)
                    }.buttonStyle(.plain)
                }
            }.frame(width: 62)
            GeometryReader { geometry in
                let window = model.window
                let visible = model.visibleRecordings.filter { window.overlaps($0) }
                ZStack {
                    Canvas { context, size in
                        for tick in 0...6 {
                            let x = size.width * Double(tick) / 6
                            var line = Path()
                            line.move(to: CGPoint(x: x, y: rulerHeight))
                            line.addLine(to: CGPoint(x: x, y: size.height))
                            context.stroke(line, with: .color(.white.opacity(0.07)), lineWidth: 1)
                            let date = window.date(at: x, width: size.width)
                            let label = Text(date.formatted(.dateTime.hour(.twoDigits(amPM: .omitted)).minute(.twoDigits).second(.twoDigits)))
                                .font(.system(size: 10, design: .monospaced)).foregroundColor(muted)
                            context.draw(label, at: CGPoint(x: min(max(x, 27), size.width - 27), y: rulerHeight / 2))
                        }
                        for (row, channel) in model.channels.enumerated() {
                            let y = rulerHeight + Double(row) * rowHeight
                            let background = CGRect(x: 0, y: y, width: size.width, height: rowHeight - 1)
                            context.fill(Path(background), with: .color(.white.opacity(row % 2 == 0 ? 0.025 : 0.01)))
                            for recording in visible where recording.channel == channel {
                                let x = max(0, window.x(at: recording.start, width: size.width))
                                let endX = min(size.width, window.x(at: recording.end, width: size.width))
                                let rect = CGRect(x: x, y: y + (recording.isSegment ? 3 : 6), width: max(2, endX - x), height: recording.isSegment ? 10 : 4)
                                let color = recording.isSegment ? accent : Color.blue.opacity(0.45)
                                context.fill(Path(roundedRect: rect, cornerRadius: 2), with: .color(color.opacity(model.selected?.id == recording.id ? 1 : 0.7)))
                                if model.selected?.id == recording.id { context.stroke(Path(roundedRect: rect, cornerRadius: 2), with: .color(.white), lineWidth: 1.5) }
                            }
                        }
                        if let time = model.selectedTime {
                            let x = window.x(at: time, width: size.width)
                            if x >= 0 && x <= size.width {
                                var cursor = Path()
                                cursor.move(to: CGPoint(x: x, y: rulerHeight))
                                cursor.addLine(to: CGPoint(x: x, y: size.height))
                                context.stroke(cursor, with: .color(.orange), lineWidth: 2)
                            }
                        }
                    }
                    TimelineInput(onScroll: { model.scroll($0) }, onZoom: { model.zoom($0) }, onClick: { point in
                        let row = Int((point.y - rulerHeight) / rowHeight)
                        guard point.y >= rulerHeight, row >= 0, row < model.channels.count else { return }
                        model.click(channel: model.channels[row], date: window.date(at: point.x, width: geometry.size.width))
                    })
                }
            }
        }.frame(height: TimelineMetrics.height(channels: model.channels.count))
    }
}

struct TimelineInput: NSViewRepresentable {
    var onScroll: (Double) -> Void
    var onZoom: (Double) -> Void
    var onClick: (CGPoint) -> Void
    func makeNSView(context: Context) -> InputView {
        let view = InputView()
        view.setAccessibilityElement(true)
        view.setAccessibilityRole(.group)
        view.setAccessibilityLabel("交信タイムライン。スクロールで時間を移動")
        return view
    }
    func updateNSView(_ view: InputView, context: Context) {
        view.onScroll = onScroll; view.onZoom = onZoom; view.onClick = onClick
    }
    final class InputView: NSView {
        var onScroll: ((Double) -> Void)?
        var onZoom: ((Double) -> Void)?
        var onClick: ((CGPoint) -> Void)?
        override var isFlipped: Bool { true }
        override func scrollWheel(with event: NSEvent) {
            let delta = abs(event.scrollingDeltaX) > abs(event.scrollingDeltaY) ? event.scrollingDeltaX : event.scrollingDeltaY
            onScroll?(Double(delta) * (event.hasPreciseScrollingDeltas ? 1 : 12))
        }
        override func magnify(with event: NSEvent) { onZoom?(max(0.1, 1 - Double(event.magnification))) }
        override func mouseUp(with event: NSEvent) { onClick?(convert(event.locationInWindow, from: nil)) }
    }
}

struct TranscriptScrollObserver: NSViewRepresentable {
    var onScroll: (CGFloat) -> Void
    func makeNSView(context: Context) -> ObserverView { ObserverView() }
    func updateNSView(_ view: ObserverView, context: Context) { view.onScroll = onScroll }
    final class ObserverView: NSView {
        var onScroll: ((CGFloat) -> Void)?
        private var monitor: Any?
        override func hitTest(_ point: NSPoint) -> NSView? { nil }
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if let monitor { NSEvent.removeMonitor(monitor); self.monitor = nil }
            guard window != nil else { return }
            monitor = NSEvent.addLocalMonitorForEvents(matching: .scrollWheel) { [weak self] event in
                if let self, event.window == self.window, self.bounds.contains(self.convert(event.locationInWindow, from: nil)) {
                    self.onScroll?(event.scrollingDeltaY)
                }
                return event
            }
        }
        deinit { if let monitor { NSEvent.removeMonitor(monitor) } }
    }
}
