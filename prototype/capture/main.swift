// Captures the Mac's mixed audio output with a Core Audio process tap
// (macOS 14.2+) and writes interleaved float32 stereo PCM to stdout.
// The Mac's own output is muted while capturing.
//
//   capture --format   prints the sample rate and exits
//   capture            streams until killed
import AudioToolbox
import CoreAudio
import Foundation

func check(_ status: OSStatus, _ what: String) {
    guard status == noErr else {
        FileHandle.standardError.write("\(what) failed: \(status)\n".data(using: .utf8)!)
        exit(1)
    }
}

func property<T>(_ object: AudioObjectID, _ selector: AudioObjectPropertySelector, _ value: inout T) -> OSStatus {
    var address = AudioObjectPropertyAddress(
        mSelector: selector, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
    var size = UInt32(MemoryLayout<T>.size)
    return AudioObjectGetPropertyData(object, &address, 0, nil, &size, &value)
}

// 1. A private tap on all processes' output, mixed to stereo.
let tapDescription = CATapDescription(stereoGlobalTapButExcludeProcesses: [])
tapDescription.uuid = UUID()
tapDescription.isPrivate = true
tapDescription.muteBehavior = .mutedWhenTapped
var tapID = AudioObjectID(kAudioObjectUnknown)
check(AudioHardwareCreateProcessTap(tapDescription, &tapID), "AudioHardwareCreateProcessTap")

var format = AudioStreamBasicDescription()
check(property(tapID, kAudioTapPropertyFormat, &format), "read tap format")
if CommandLine.arguments.contains("--format") {
    print(Int(format.mSampleRate))
    AudioHardwareDestroyProcessTap(tapID)
    exit(0)
}
let nonInterleaved = format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0
FileHandle.standardError.write(
    "tap: \(format.mSampleRate) Hz, \(format.mChannelsPerFrame) ch, \(nonInterleaved ? "non-" : "")interleaved\n"
        .data(using: .utf8)!)

// 2. A private aggregate device around the default output, carrying the tap.
var outputID = AudioObjectID(kAudioObjectUnknown)
check(property(AudioObjectID(kAudioObjectSystemObject), kAudioHardwarePropertyDefaultSystemOutputDevice, &outputID),
      "default output device")
var outputUID: CFString = "" as CFString
check(property(outputID, kAudioDevicePropertyDeviceUID, &outputUID), "output device UID")

let aggregate: [String: Any] = [
    kAudioAggregateDeviceNameKey: "ls50stream",
    kAudioAggregateDeviceUIDKey: UUID().uuidString,
    kAudioAggregateDeviceMainSubDeviceKey: outputUID as String,
    kAudioAggregateDeviceIsPrivateKey: true,
    kAudioAggregateDeviceIsStackedKey: false,
    kAudioAggregateDeviceTapAutoStartKey: true,
    kAudioAggregateDeviceSubDeviceListKey: [[kAudioSubDeviceUIDKey: outputUID as String]],
    kAudioAggregateDeviceTapListKey: [[
        kAudioSubTapDriftCompensationKey: true,
        kAudioSubTapUIDKey: tapDescription.uuid.uuidString,
    ]],
]
var deviceID = AudioObjectID(kAudioObjectUnknown)
check(AudioHardwareCreateAggregateDevice(aggregate as CFDictionary, &deviceID), "create aggregate device")

func cleanUp() {
    AudioHardwareDestroyAggregateDevice(deviceID)
    AudioHardwareDestroyProcessTap(tapID)
}

// 3. Copy each input buffer to stdout as interleaved float32.
let out = FileHandle.standardOutput
let queue = DispatchQueue(label: "capture")
var procID: AudioDeviceIOProcID?
check(AudioDeviceCreateIOProcIDWithBlock(&procID, deviceID, queue) { _, input, _, _, _ in
    let buffers = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: input))
    var data: Data
    if nonInterleaved, buffers.count >= 2,
       let left = buffers[0].mData?.assumingMemoryBound(to: Float.self),
       let right = buffers[1].mData?.assumingMemoryBound(to: Float.self) {
        let frames = Int(buffers[0].mDataByteSize) / MemoryLayout<Float>.size
        var interleaved = [Float](repeating: 0, count: frames * 2)
        for i in 0..<frames {
            interleaved[2 * i] = left[i]
            interleaved[2 * i + 1] = right[i]
        }
        data = interleaved.withUnsafeBufferPointer { Data(buffer: $0) }
    } else if let first = buffers.first, let bytes = first.mData {
        data = Data(bytes: bytes, count: Int(first.mDataByteSize))
    } else {
        return
    }
    do {
        try out.write(contentsOf: data)
    } catch {
        // The reader went away: stop cleanly so the Mac's output is unmuted.
        cleanUp()
        exit(0)
    }
}, "create IO proc")

for sig in [SIGINT, SIGTERM, SIGPIPE] {
    signal(sig, SIG_IGN)
    let source = DispatchSource.makeSignalSource(signal: sig, queue: .main)
    source.setEventHandler { cleanUp(); exit(0) }
    source.resume()
    _ = Unmanaged.passRetained(source)
}

check(AudioDeviceStart(deviceID, procID), "start device")
dispatchMain()
