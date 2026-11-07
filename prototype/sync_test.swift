// Measures A/V sync of the default output device acoustically.
//
// Schedules short 2 kHz beeps at exact host times on the default output (the way a
// video player schedules audio; CoreAudio already includes the device's reported
// latency), records the microphone with host-time stamps, and finds when each beep
// actually arrived. Error = arrival - scheduled time: negative = audio early
// ("audio ahead of video"), positive = audio late.
//
//   sync_test "<output device name>" [beeps]     (microphone: the MacBook's built-in one)
import AVFoundation
import AudioToolbox
import CoreAudio
import Foundation

let args = Array(CommandLine.arguments.dropFirst())
let outputName = args.first ?? "KEF LS50W"
let beeps = args.count > 1 ? Int(args[1]) ?? 5 : 5
let toneHz = 2000.0
let spacing = 4.0 // seconds between beeps (well beyond any plausible error)

func deviceIDs() -> [AudioObjectID] {
    var a = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDevices, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
    var size: UInt32 = 0
    AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject), &a, 0, nil, &size)
    var ids = [AudioObjectID](repeating: 0, count: Int(size) / 4)
    AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &a, 0, nil, &size, &ids)
    return ids
}
func name(_ id: AudioObjectID) -> String {
    var a = AudioObjectPropertyAddress(mSelector: kAudioObjectPropertyName, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
    var value: Unmanaged<CFString>?
    var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
    guard AudioObjectGetPropertyData(id, &a, 0, nil, &size, &value) == noErr, let value else { return "" }
    return value.takeRetainedValue() as String
}
func u32(_ id: AudioObjectID, _ sel: AudioObjectPropertySelector, _ scope: AudioObjectPropertyScope) -> UInt32 {
    var a = AudioObjectPropertyAddress(mSelector: sel, mScope: scope, mElement: kAudioObjectPropertyElementMain)
    var v: UInt32 = 0, size = UInt32(4)
    AudioObjectGetPropertyData(id, &a, 0, nil, &size, &v)
    return v
}
func useDevice(_ engineNode: AVAudioIONode, _ id: AudioObjectID) {
    var id = id
    let status = AudioUnitSetProperty(engineNode.audioUnit!, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, &id, 4)
    if status != noErr { print("could not select device \(id): \(status)"); exit(1) }
}
guard let outID = deviceIDs().first(where: { name($0) == outputName }),
      let micID = deviceIDs().first(where: { name($0) == "MacBook Pro Microphone" }) else {
    print("devices: \(deviceIDs().map(name))"); exit(1)
}
let scopeOut = kAudioObjectPropertyScopeOutput

func secondsToTicks(_ s: Double) -> UInt64 { AVAudioTime.hostTime(forSeconds: s) }
func ticksToSeconds(_ t: UInt64) -> Double { AVAudioTime.seconds(forHostTime: t) }

// Microphone (default input) on its own engine
let input = AVAudioEngine()
useDevice(input.inputNode, micID)
let inFormat = input.inputNode.outputFormat(forBus: 0)
let inRate = inFormat.sampleRate
var captured: [(host: UInt64, samples: [Float])] = []
let lock = NSLock()
input.inputNode.installTap(onBus: 0, bufferSize: 1024, format: inFormat) { buffer, when in
    guard let data = buffer.floatChannelData?[0] else { return }
    let samples = Array(UnsafeBufferPointer(start: data, count: Int(buffer.frameLength)))
    lock.lock(); captured.append((when.hostTime, samples)); lock.unlock()
}

// Beeps on the default output
let output = AVAudioEngine()
useDevice(output.outputNode, outID)
let player = AVAudioPlayerNode()
output.attach(player)
let outRate = output.outputNode.outputFormat(forBus: 0).sampleRate
let format = AVAudioFormat(standardFormatWithSampleRate: outRate, channels: 2)!
output.connect(player, to: output.mainMixerNode, format: format)
let beepFrames = AVAudioFrameCount(outRate * 0.03)
let beep = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: beepFrames)!
beep.frameLength = beepFrames
for i in 0..<Int(beepFrames) {
    let envelope = sin(Double.pi * Double(i) / Double(beepFrames)) // smooth edges
    let s = Float(0.6 * envelope * sin(2 * Double.pi * toneHz * Double(i) / outRate))
    beep.floatChannelData![0][i] = s
    beep.floatChannelData![1][i] = s
}

do {
    try input.start()
    try output.start()
} catch {
    print("Could not start audio: \(error)")
    exit(1)
}
player.play()
var rate = 0.0; var rateSize = UInt32(8)
var rateAddr = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyNominalSampleRate, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
AudioObjectGetPropertyData(outID, &rateAddr, 0, nil, &rateSize, &rate)
let latency = u32(outID, kAudioDevicePropertyLatency, scopeOut), safety = u32(outID, kAudioDevicePropertySafetyOffset, scopeOut)
let bufferFrames = u32(outID, kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal)
print(String(format: "Output '%@' %.0f Hz: device latency %.3f s + safety %.3f s + buffer %.3f s = %.3f s",
             outputName, rate, Double(latency) / rate, Double(safety) / rate, Double(bufferFrames) / rate,
             Double(latency + safety + bufferFrames) / rate))
print("Mic '\(name(micID))' \(Int(inRate)) Hz; playing \(beeps) beeps, \(Int(spacing)) s apart...")

// Let the output pipeline settle, then schedule beeps at exact host times
let start = mach_absolute_time() + secondsToTicks(2.5)
var scheduled: [UInt64] = []
for i in 0..<beeps {
    let t = start + secondsToTicks(Double(i) * spacing)
    scheduled.append(t)
    player.scheduleBuffer(beep, at: AVAudioTime(hostTime: t), options: [], completionHandler: nil)
}
Thread.sleep(forTimeInterval: 2.5 + Double(beeps) * spacing + 1.5)
output.stop()
input.stop()

// Goertzel energy of the tone in 2 ms steps over 5 ms windows; onset = first window
// above a threshold set from the quietest part of each search range.
lock.lock()
let chunks = captured
lock.unlock()
let allSamples = chunks.flatMap(\.samples)
print(String(format: "Mic captured %.1f s, peak level %.4f", Double(allSamples.count) / inRate, allSamples.map { abs($0) }.max() ?? 0))
var hosts: [Double] = []
var mic: [Float] = []
for chunk in chunks {
    let base = ticksToSeconds(chunk.host)
    for (i, s) in chunk.samples.enumerated() {
        hosts.append(base + Double(i) / inRate)
        mic.append(s)
    }
}
func toneEnergy(at index: Int, length: Int) -> Double {
    let k = 2 * cos(2 * Double.pi * toneHz / inRate)
    var s1 = 0.0, s2 = 0.0
    for i in index..<min(index + length, mic.count) {
        let s0 = Double(mic[i]) + k * s1 - s2
        s2 = s1; s1 = s0
    }
    return s1 * s1 + s2 * s2 - k * s1 * s2
}
let window = Int(inRate * 0.005), step = Int(inRate * 0.002)
var errors: [Double] = []
for t in scheduled {
    let target = ticksToSeconds(t)
    guard let from = hosts.firstIndex(where: { $0 >= target - 1.9 }),
          let to = hosts.firstIndex(where: { $0 >= target + 1.9 }) else { continue }
    var energies: [(Int, Double)] = []
    var i = from
    while i < to - window { energies.append((i, toneEnergy(at: i, length: window))); i += step }
    let sorted = energies.map(\.1).sorted()
    let floor = sorted[sorted.count / 10], peak = sorted.last ?? 0
    guard peak > floor * 30 else { print(String(format: "  beep at %.2f s: not heard", target - ticksToSeconds(start))); continue }
    let threshold = floor + (peak - floor) * 0.2
    if let onset = energies.first(where: { $0.1 > threshold }) {
        let error = hosts[onset.0] - target
        errors.append(error)
        print(String(format: "  beep %d: arrived %+.3f s vs scheduled", errors.count, error))
    }
}
if errors.isEmpty {
    print("No beeps detected (microphone permission? volume?)")
} else {
    let median = errors.sorted()[errors.count / 2]
    print(String(format: "RESULT median sync error %+.3f s (%@)", median,
                 median < 0 ? "audio early: report less latency" : "audio late: report more latency"))
}
