// Make the wayland-macos PulseAudio bridge follow the macOS default output.
//
// The bridge (scripts/pulseaudio-mac.pa) exposes every CoreAudio device as its
// own Pulse sink via module-coreaudio-detect and pins ONE static default —
// which is not necessarily the device you selected in macOS, and never follows
// when you switch output (AirPods, speakers, a monitor, ...). Native Mac apps
// track CoreAudio's default-output-device abstraction; PulseAudio bypasses it.
//
// This agent restores that behaviour: it resolves the current default output
// device, points the bridge's default sink at the matching Pulse sink, and moves
// any already-playing streams over — then repeats whenever the default changes,
// driven by a CoreAudio property listener (event-based, no polling).
//
// Started in the background by scripts/pulseaudio-mac.sh; stopped by
// `wayland-macos stop` (src/cli.rs). Talks to the daemon with `pactl` over TCP.
//
// Match key: the CoreAudio device *name* (kAudioObjectPropertyName), which
// module-coreaudio copies verbatim into the sink's `Description` — so the same
// string identifies the device on both sides (no UID is exposed by Pulse).
//
// argv: <pactl-path> <pulse-server>   e.g.  /opt/homebrew/bin/pactl tcp:127.0.0.1:4713

import CoreAudio
import Foundation

let args = CommandLine.arguments
guard args.count == 3 else {
    FileHandle.standardError.write(Data("usage: audio-follow-default <pactl> <pulse-server>\n".utf8))
    exit(2)
}
let pactlPath = args[1]
let pulseServer = args[2]

func log(_ msg: String) {
    FileHandle.standardError.write(Data("[audio-follow] \(msg)\n".utf8))
}

/// Run `pactl <args>` against the bridge; return stdout, or nil on failure.
@discardableResult
func pactl(_ pactlArgs: [String]) -> String? {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: pactlPath)
    p.arguments = pactlArgs
    var env = ProcessInfo.processInfo.environment
    env["PULSE_SERVER"] = pulseServer
    env["LC_ALL"] = "C"  // stable "Name:"/"Description:" labels regardless of locale
    p.environment = env
    let out = Pipe()
    p.standardOutput = out
    p.standardError = FileHandle.nullDevice
    do { try p.run() } catch { return nil }
    let data = out.fileHandleForReading.readDataToEndOfFile()
    p.waitUntilExit()
    guard p.terminationStatus == 0 else { return nil }
    return String(data: data, encoding: .utf8)
}

/// Name of the current macOS default output device (e.g. "AirPods Pro").
func defaultOutputDeviceName() -> String? {
    var deviceID = AudioDeviceID(0)
    var size = UInt32(MemoryLayout<AudioDeviceID>.size)
    var addr = AudioObjectPropertyAddress(
        mSelector: kAudioHardwarePropertyDefaultOutputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain)
    guard AudioObjectGetPropertyData(
        AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size, &deviceID) == noErr,
        deviceID != 0 else { return nil }

    var nameAddr = AudioObjectPropertyAddress(
        mSelector: kAudioObjectPropertyName,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain)
    var cfName: Unmanaged<CFString>?
    var nameSize = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
    guard AudioObjectGetPropertyData(deviceID, &nameAddr, 0, nil, &nameSize, &cfName) == noErr,
        let name = cfName?.takeRetainedValue() else { return nil }
    return name as String
}

/// Find the Pulse sink whose Description matches `target`; return its Name.
func sinkMatching(_ target: String) -> String? {
    guard let text = pactl(["list", "sinks"]) else { return nil }
    var name: String?
    for raw in text.split(separator: "\n", omittingEmptySubsequences: false) {
        let line = raw.trimmingCharacters(in: .whitespaces)
        if line.hasPrefix("Name:") {
            name = String(line.dropFirst("Name:".count)).trimmingCharacters(in: .whitespaces)
        } else if line.hasPrefix("Description:") {
            let desc = String(line.dropFirst("Description:".count)).trimmingCharacters(in: .whitespaces)
            if desc == target, let n = name { return n }
        }
    }
    return nil
}

/// Point the bridge's default sink at the macOS default output and move any
/// live streams onto it (set-default-sink only affects *new* streams).
func sync() {
    guard let target = defaultOutputDeviceName() else {
        log("could not read the macOS default output device")
        return
    }
    guard let sink = sinkMatching(target) else {
        log("no bridge sink matches default output \"\(target)\" (yet) — leaving default unchanged")
        return
    }
    let current = pactl(["get-default-sink"])?.trimmingCharacters(in: .whitespacesAndNewlines)
    if current == sink { return }
    pactl(["set-default-sink", sink])
    if let inputs = pactl(["list", "sink-inputs", "short"]) {
        for line in inputs.split(separator: "\n") {
            if let id = line.split(separator: "\t").first, !id.isEmpty {
                pactl(["move-sink-input", String(id), sink])
            }
        }
    }
    log("default output -> \"\(target)\" (sink \(sink))")
}

// Fire on every default-output-device change, plus once at startup.
var listenAddr = AudioObjectPropertyAddress(
    mSelector: kAudioHardwarePropertyDefaultOutputDevice,
    mScope: kAudioObjectPropertyScopeGlobal,
    mElement: kAudioObjectPropertyElementMain)
let status = AudioObjectAddPropertyListenerBlock(
    AudioObjectID(kAudioObjectSystemObject), &listenAddr, DispatchQueue.main
) { _, _ in sync() }
if status != noErr {
    log("failed to install CoreAudio listener (status \(status)); doing a one-shot sync only")
    sync()
    exit(status == noErr ? 0 : 1)
}

sync()  // align immediately
log("watching the macOS default output device")
dispatchMain()
