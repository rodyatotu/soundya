// soundya's widget for the Mac (macOS 14+): what the speaker plays, its cover,
// pause / next / previous, volume (a bar of ten steps: WidgetKit has no sliders,
// only buttons; the speaker icon mutes and brings the volume back) and a button
// that opens the speaker's full panel in the browser. Built into soundya.app/Contents/PlugIns by
// scripts/mac-widget/build.sh. It talks only to the speaker's panel (port 8765):
// GET /api/status and /cover, POST /api/cmd with one of the commands below.
// The speaker's address is set in the widget itself («Изменить виджет»).

import AppIntents
import AppKit
import SwiftUI
import WidgetKit

// MARK: - Settings of one widget

struct SpeakerConfig: WidgetConfigurationIntent {
    static var title: LocalizedStringResource = "Колонка soundya"
    static var description = IntentDescription("Адрес колонки: soundya.local или IP, например 192.168.0.112")

    @Parameter(title: "Адрес колонки", default: "soundya.local")
    var address: String
}

// MARK: - The speaker's panel

struct Status {
    var state = "stopped"
    var track = ""
    var volume: Int?
    var cover: NSImage?
    /// Set when the speaker did not answer (or refused): shown instead of the track.
    var problem: String?

    static let sample = Status(state: "playing", track: "Bonobo — Kerala", volume: 4)
}

enum Speaker {
    /// Only these go to the speaker from the widget's buttons.
    static let commands: Set<String> = Set(["toggle", "next", "prev", "volume up 1", "volume down 1"]
        + (0...10).map { "volume set \($0)" })

    /// The level the speaker icon brings back after a mute (kept per address in
    /// the widget's own sandbox defaults).
    static func unmuteLevel(_ address: String) -> Int {
        let v = UserDefaults.standard.integer(forKey: "unmute." + base(address))
        return (1...10).contains(v) ? v : 4
    }

    static func rememberLevel(_ address: String, _ level: Int) {
        if (1...10).contains(level) { UserDefaults.standard.set(level, forKey: "unmute." + base(address)) }
    }

    static func base(_ address: String) -> String {
        var a = address.trimmingCharacters(in: .whitespacesAndNewlines)
        if let r = a.range(of: "://") { a = String(a[r.upperBound...]) }
        a = a.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        if a.isEmpty { a = "soundya.local" }
        if !a.contains(":") { a += ":8765" }
        return "http://" + a
    }

    private static let session: URLSession = {
        let c = URLSessionConfiguration.ephemeral
        c.timeoutIntervalForRequest = 5
        c.timeoutIntervalForResource = 8
        return URLSession(configuration: c)
    }()

    static func status(_ address: String) async -> Status {
        let root = base(address)
        guard let url = URL(string: root + "/api/status") else { return Status(problem: "Неверный адрес") }
        do {
            let (data, resp) = try await session.data(from: url)
            if let code = (resp as? HTTPURLResponse)?.statusCode, code != 200 {
                return Status(problem: code == 401 ? "На панели колонки стоит PIN" : "Колонка ответила \(code)")
            }
            guard let j = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
                return Status(problem: "Колонка ответила непонятно")
            }
            var s = Status()
            s.state = j["state"] as? String ?? "stopped"
            s.track = j["track"] as? String ?? ""
            s.volume = j["volume"] as? Int
            if let cover = j["cover"] as? String, !cover.isEmpty, let cu = URL(string: root + cover),
               let (img, _) = try? await session.data(from: cu) {
                s.cover = NSImage(data: img)
            }
            return s
        } catch {
            // macOS 15 blocks the local network until soundya is allowed it.
            let text = "\(error)".contains("Local network") || (error as NSError).code == NSURLErrorNotConnectedToInternet
                ? "Нет доступа к локальной сети: Системные настройки → Конфиденциальность → Локальная сеть → soundya"
                : "Колонка не отвечает"
            return Status(problem: text)
        }
    }

    static func send(_ address: String, _ command: String) async {
        guard commands.contains(command), let url = URL(string: base(address) + "/api/cmd") else { return }
        var r = URLRequest(url: url)
        r.httpMethod = "POST"
        r.setValue("text/plain; charset=utf-8", forHTTPHeaderField: "Content-Type")
        r.httpBody = command.data(using: .utf8)
        _ = try? await session.data(for: r)
    }
}

// MARK: - Buttons

struct CommandIntent: AppIntent {
    static var title: LocalizedStringResource = "Команда колонке"
    static var isDiscoverable = false

    @Parameter(title: "Адрес") var address: String
    @Parameter(title: "Команда") var command: String

    init() {}
    init(address: String, command: String) {
        self.address = address
        self.command = command
    }

    func perform() async throws -> some IntentResult {
        await Speaker.send(address, command)
        // The speaker changes the track or the state a moment later.
        try? await Task.sleep(nanoseconds: 700_000_000)
        return .result()
    }
}

/// The speaker icon: muted (0) → back to the level before; otherwise → 0.
struct MuteIntent: AppIntent {
    static var title: LocalizedStringResource = "Выключить или вернуть звук"
    static var isDiscoverable = false

    @Parameter(title: "Адрес") var address: String
    @Parameter(title: "Громкость сейчас") var level: Int

    init() {}
    init(address: String, level: Int) {
        self.address = address
        self.level = level
    }

    func perform() async throws -> some IntentResult {
        if level > 0 {
            Speaker.rememberLevel(address, level)
            await Speaker.send(address, "volume set 0")
        } else {
            await Speaker.send(address, "volume set \(Speaker.unmuteLevel(address))")
        }
        try? await Task.sleep(nanoseconds: 700_000_000)
        return .result()
    }
}

// MARK: - Timeline

struct Entry: TimelineEntry {
    let date: Date
    let address: String
    let status: Status
}

struct Provider: AppIntentTimelineProvider {
    func placeholder(in context: Context) -> Entry {
        Entry(date: .now, address: "soundya.local", status: .sample)
    }

    func snapshot(for configuration: SpeakerConfig, in context: Context) async -> Entry {
        context.isPreview ? placeholder(in: context) : await entry(configuration)
    }

    func timeline(for configuration: SpeakerConfig, in context: Context) async -> Timeline<Entry> {
        let e = await entry(configuration)
        // WidgetKit decides how often in the end; ask for more while music plays.
        let next: TimeInterval = e.status.state == "playing" ? 60 : 300
        return Timeline(entries: [e], policy: .after(.now.addingTimeInterval(next)))
    }

    private func entry(_ c: SpeakerConfig) async -> Entry {
        Entry(date: .now, address: c.address, status: await Speaker.status(c.address))
    }
}

// MARK: - Look

private let accent = LinearGradient(colors: [Color(red: 1, green: 0.48, blue: 0.1), Color(red: 1, green: 0.18, blue: 0.43)],
                                    startPoint: .topLeading, endPoint: .bottomTrailing)

struct ControlButton: View {
    let address: String
    let command: String
    let symbol: String
    var body: some View {
        Button(intent: CommandIntent(address: address, command: command)) {
            Image(systemName: symbol).font(.system(size: 15, weight: .semibold)).frame(width: 30, height: 26)
        }
        .buttonStyle(.plain)
        .foregroundStyle(.white)
    }
}

/// Ten steps of the volume bar: pressing step N sets the volume to N.
struct VolumeBar: View {
    let address: String
    let level: Int
    var body: some View {
        HStack(spacing: 3) {
            Button(intent: MuteIntent(address: address, level: level)) {
                Image(systemName: level == 0 ? "speaker.slash.fill" : "speaker.wave.2.fill")
                    .font(.system(size: 12, weight: .semibold)).frame(width: 22, height: 18)
            }
            .buttonStyle(.plain)
            .foregroundStyle(level == 0 ? Color.white.opacity(0.5) : .white)
            ForEach(1...10, id: \.self) { n in
                Button(intent: CommandIntent(address: address, command: "volume set \(n)")) {
                    Capsule().fill(n <= level ? AnyShapeStyle(accent) : AnyShapeStyle(Color.white.opacity(0.18)))
                        .frame(height: 6).frame(maxWidth: .infinity, minHeight: 18)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
            Text("\(level)").font(.caption2.monospacedDigit()).foregroundStyle(.white.opacity(0.8)).frame(width: 16)
        }
    }
}

/// Opens the speaker's full panel (http://address:8765) in the browser.
struct PanelLink: View {
    let address: String
    var body: some View {
        if let url = URL(string: Speaker.base(address)) {
            Link(destination: url) {
                Image(systemName: "slider.horizontal.3").font(.system(size: 13, weight: .semibold))
                    .frame(width: 26, height: 22)
            }
            .foregroundStyle(.white.opacity(0.85))
            .help("Открыть панель soundya")
        }
    }
}

struct WidgetView: View {
    @Environment(\.widgetFamily) var family
    let entry: Entry

    var playing: Bool { entry.status.state == "playing" }
    var title: String {
        if let p = entry.status.problem { return p }
        return entry.status.track.isEmpty ? "Ничего не играет" : entry.status.track
    }

    var cover: some View {
        Group {
            if let img = entry.status.cover {
                Image(nsImage: img).resizable().aspectRatio(contentMode: .fill)
            } else {
                ZStack { accent; Image(systemName: "music.note").font(.title2).foregroundStyle(.white) }
            }
        }
        .clipShape(RoundedRectangle(cornerRadius: 10))
    }

    var buttons: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 2) {
                if family != .systemSmall { ControlButton(address: entry.address, command: "prev", symbol: "backward.fill") }
                ControlButton(address: entry.address, command: "toggle", symbol: playing ? "pause.fill" : "play.fill")
                ControlButton(address: entry.address, command: "next", symbol: "forward.fill")
                Spacer(minLength: 4)
                PanelLink(address: entry.address)
            }
            if let v = entry.status.volume {
                if family == .systemSmall {
                    // No room for the bar: steps down / up and the mute.
                    HStack(spacing: 2) {
                        ControlButton(address: entry.address, command: "volume down 1", symbol: "minus")
                        Button(intent: MuteIntent(address: entry.address, level: v)) {
                            Text(v == 0 ? "тихо" : "\(v)").font(.caption.monospacedDigit()).frame(minWidth: 30, minHeight: 22)
                        }
                        .buttonStyle(.plain).foregroundStyle(.white)
                        ControlButton(address: entry.address, command: "volume up 1", symbol: "plus")
                    }
                } else {
                    VolumeBar(address: entry.address, level: v)
                }
            }
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 10) {
                if family != .systemSmall { cover.frame(width: 58, height: 58) }
                VStack(alignment: .leading, spacing: 2) {
                    Text("soundya").font(.caption2.weight(.heavy)).foregroundStyle(accent)
                    Text(title).font(.system(size: 13, weight: .semibold)).lineLimit(2)
                        .foregroundStyle(.white)
                    if entry.status.problem != nil {
                        Text(entry.address).font(.caption2).foregroundStyle(.white.opacity(0.6)).lineLimit(1)
                    }
                }
            }
            Spacer(minLength: 0)
            if entry.status.problem == nil { buttons } else { HStack { Spacer(); PanelLink(address: entry.address) } }
        }
        .containerBackground(for: .widget) {
            LinearGradient(colors: [Color(red: 0.11, green: 0.08, blue: 0.16), Color(red: 0.05, green: 0.05, blue: 0.09)],
                           startPoint: .top, endPoint: .bottom)
        }
    }
}

struct SoundyaWidget: Widget {
    var body: some WidgetConfiguration {
        AppIntentConfiguration(kind: "soundya.player", intent: SpeakerConfig.self, provider: Provider()) { entry in
            WidgetView(entry: entry)
        }
        .configurationDisplayName("soundya")
        .description("Что играет на колонке: пауза, дальше, громкость.")
        .supportedFamilies([.systemSmall, .systemMedium])
    }
}

@main
struct SoundyaWidgets: WidgetBundle {
    var body: some Widget { SoundyaWidget() }
}
