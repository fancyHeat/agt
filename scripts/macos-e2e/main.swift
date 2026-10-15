// AGT Test Driver (local e2e harness, not shipped): shows a text view, makes itself frontmost,
// synthesizes the AGT hotkey with CGEvent, and measures key release -> pasted text appearing.
//
// Args (all optional):
//   --key <keycode>      61 = Right Option (default), 54 = Right Command, 63 = Fn, 105 = F13
//   --hold-ms <ms>       how long to hold (default 600)
//   --runs <n>           dictations (default 3)
//   --gap-ms <ms>        pause between runs after the text appeared (default 1500)
//   --timeout-ms <ms>    give up waiting for a paste (default 15000)
//   --overlap            press the key again right after releasing (holds it during the paste):
//                        checks that the paste isn't turned into Cmd+Option+V
//   --missed-release     release with an event AGT ignores (its injected-event mark): AGT must
//                        notice via its key-state poll
//   --cancel             press Escape while holding: nothing must be pasted
//   --prompt-only        just ask for Accessibility and exit
//   --out <file>         also append results here
import Cocoa

let args = CommandLine.arguments
func arg(_ name: String) -> String? {
    guard let i = args.firstIndex(of: name), i + 1 < args.count else { return nil }
    return args[i + 1]
}
func flag(_ name: String) -> Bool { args.contains(name) }

let key = CGKeyCode(Int(arg("--key") ?? "61")!)
let holdMs = Int(arg("--hold-ms") ?? "600")!
let runs = Int(arg("--runs") ?? "3")!
let gapMs = Int(arg("--gap-ms") ?? "1500")!
let timeoutMs = Int(arg("--timeout-ms") ?? "15000")!
let outPath = arg("--out")
let AGT_MARK: Int64 = 0x4147_5421

var log = ""
func say(_ s: String) {
    print(s)
    fflush(stdout)
    log += s + "\n"
    if let p = outPath, let d = (s + "\n").data(using: .utf8) {
        if let h = FileHandle(forWritingAtPath: p) {
            h.seekToEndOfFile(); h.write(d); h.closeFile()
        } else {
            FileManager.default.createFile(atPath: p, contents: d)
        }
    }
}

func now() -> Double { Double(DispatchTime.now().uptimeNanoseconds) / 1e6 }

let opts = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
let trusted = AXIsProcessTrustedWithOptions(opts)
if flag("--prompt-only") || !trusted {
    say("accessibility trusted: \(trusted)")
    exit(trusted ? 0 : 2)
}

// Flags while `key` is down (device-dependent bits, as real hardware sends them).
func downFlags(_ k: CGKeyCode) -> CGEventFlags {
    switch k {
    case 61: return CGEventFlags(rawValue: CGEventFlags.maskAlternate.rawValue | 0x40)
    case 54: return CGEventFlags(rawValue: CGEventFlags.maskCommand.rawValue | 0x10)
    case 63: return .maskSecondaryFn
    default: return []
    }
}
let isModifier = (54...63).contains(Int(key))
let source = CGEventSource(stateID: .hidSystemState)

func post(_ down: Bool, marked: Bool = false) {
    let e: CGEvent
    if isModifier {
        e = CGEvent(keyboardEventSource: source, virtualKey: key, keyDown: down)!
        e.type = .flagsChanged
        e.flags = down ? downFlags(key) : []
    } else {
        e = CGEvent(keyboardEventSource: source, virtualKey: key, keyDown: down)!
    }
    if marked { e.setIntegerValueField(.eventSourceUserData, value: AGT_MARK) }
    e.post(tap: .cghidEventTap)
}

func pressEscape() {
    for d in [true, false] {
        let e = CGEvent(keyboardEventSource: source, virtualKey: 53, keyDown: d)!
        e.post(tap: .cghidEventTap)
    }
}

class Driver: NSObject, NSApplicationDelegate {
    var window: NSWindow!
    var text: NSTextView!

    func applicationDidFinishLaunching(_ n: Notification) {
        window = NSWindow(contentRect: NSRect(x: 200, y: 300, width: 700, height: 300),
                          styleMask: [.titled, .closable], backing: .buffered, defer: false)
        window.title = "AGT Test Driver"
        let scroll = NSTextView.scrollableTextView()
        text = scroll.documentView as? NSTextView
        text.isRichText = false
        window.contentView = scroll
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        window.makeFirstResponder(text)
        Thread.detachNewThread { self.run() }
    }

    func textLength() -> Int {
        DispatchQueue.main.sync { self.text.string.count }
    }

    func frontmost() -> String {
        DispatchQueue.main.sync {
            NSWorkspace.shared.frontmostApplication?.bundleIdentifier ?? "?"
        }
    }

    func run() {
        Thread.sleep(forTimeInterval: 1.0)
        say("frontmost: \(frontmost()); key \(key), hold \(holdMs) ms, \(runs) runs")
        var latencies: [Double] = []
        for i in 1...runs {
            let before = textLength()
            post(true)
            Thread.sleep(forTimeInterval: Double(holdMs) / 1000)
            if flag("--cancel") {
                pressEscape()
                Thread.sleep(forTimeInterval: 0.2)
            }
            let released = now()
            post(false, marked: flag("--missed-release"))
            if flag("--overlap") {
                // Hold the key again (a new dictation) while the first one is pasted.
                Thread.sleep(forTimeInterval: 0.05)
                post(true)
            }
            var pasted: Double? = nil
            while now() - released < Double(timeoutMs) {
                if textLength() != before { pasted = now(); break }
                Thread.sleep(forTimeInterval: 0.001)
            }
            if flag("--overlap") {
                Thread.sleep(forTimeInterval: 0.3)
                pressEscape() // cancel the second dictation
                post(false)
            }
            let added = DispatchQueue.main.sync { String(self.text.string.dropFirst(before)) }
            if let p = pasted {
                latencies.append(p - released)
                say(String(format: "run %d: release -> text %.0f ms: %@", i, p - released, added))
            } else {
                say("run \(i): no text within \(timeoutMs) ms")
            }
            Thread.sleep(forTimeInterval: Double(gapMs) / 1000)
        }
        if !latencies.isEmpty {
            let s = latencies.sorted()
            say(String(format: "median %.0f ms, min %.0f, max %.0f (n=%d)", s[s.count / 2], s.first!, s.last!, s.count))
        }
        say("frontmost at end: \(frontmost())")
        DispatchQueue.main.async { NSApp.terminate(nil) }
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let delegate = Driver()
app.delegate = delegate
app.run()
