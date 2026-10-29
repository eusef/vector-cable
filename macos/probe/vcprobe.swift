// vcprobe: spike 0a probe for the VectorBridge HAL plug-ins.
//
//   vcprobe list              devices CoreAudio lists, the four VectorBridge UIDs
//                             (hidden ones are found only by UID), and the
//                             Microphone permission state for this process
//   vcprobe loop rx|tx [sec]  play a 1 kHz tone into one side of a cable and
//                             measure it on the other side
//   vcprobe net HOST PORT     one TCP connect, to see Local Network privacy
//
// Build: swiftc -O macos/probe/vcprobe.swift -o target/macos/vcprobe
// GPL-3.0.

import AVFoundation
import CoreAudio
import Foundation

let rxApp = "VectorBridgeRX_UID"     // desktop app records from this
let rxCore = "VectorBridgeRX_2_UID"  // core writes RX audio to this (hidden)
let txApp = "VectorBridgeTX_UID"     // desktop app plays TX audio to this
let txCore = "VectorBridgeTX_2_UID"  // core reads TX audio from this (hidden)

func address(_ selector: AudioObjectPropertySelector,
             _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress(mSelector: selector, mScope: scope, mElement: kAudioObjectPropertyElementMain)
}

func stringProperty(_ id: AudioObjectID, _ selector: AudioObjectPropertySelector) -> String {
    var addr = address(selector)
    var value: Unmanaged<CFString>?
    var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
    guard AudioObjectGetPropertyData(id, &addr, 0, nil, &size, &value) == noErr, let v = value else { return "?" }
    return v.takeRetainedValue() as String
}

func uint32Property(_ id: AudioObjectID, _ selector: AudioObjectPropertySelector) -> UInt32? {
    var addr = address(selector)
    var value: UInt32 = 0
    var size = UInt32(MemoryLayout<UInt32>.size)
    guard AudioObjectGetPropertyData(id, &addr, 0, nil, &size, &value) == noErr else { return nil }
    return value
}

func channels(_ id: AudioObjectID, _ scope: AudioObjectPropertyScope) -> Int {
    var addr = address(kAudioDevicePropertyStreamConfiguration, scope)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(id, &addr, 0, nil, &size) == noErr, size > 0 else { return 0 }
    let raw = UnsafeMutableRawPointer.allocate(byteCount: Int(size), alignment: 16)
    defer { raw.deallocate() }
    guard AudioObjectGetPropertyData(id, &addr, 0, nil, &size, raw) == noErr else { return 0 }
    let list = UnsafeMutableAudioBufferListPointer(raw.assumingMemoryBound(to: AudioBufferList.self))
    return list.reduce(0) { $0 + Int($1.mNumberChannels) }
}

func sampleRate(_ id: AudioObjectID) -> Double {
    var addr = address(kAudioDevicePropertyNominalSampleRate)
    var value: Float64 = 0
    var size = UInt32(MemoryLayout<Float64>.size)
    guard AudioObjectGetPropertyData(id, &addr, 0, nil, &size, &value) == noErr else { return 0 }
    return value
}

func listedDevices() -> [AudioObjectID] {
    var addr = address(kAudioHardwarePropertyDevices)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size) == noErr else { return [] }
    var ids = [AudioObjectID](repeating: 0, count: Int(size) / MemoryLayout<AudioObjectID>.size)
    guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size, &ids) == noErr else { return [] }
    return ids
}

func device(forUID uid: String) -> AudioObjectID? {
    var addr = address(kAudioHardwarePropertyTranslateUIDToDevice)
    var cfUID = uid as CFString
    var id = AudioObjectID(kAudioObjectUnknown)
    var size = UInt32(MemoryLayout<AudioObjectID>.size)
    let status = withUnsafeMutablePointer(to: &cfUID) { ptr in
        AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &addr,
                                   UInt32(MemoryLayout<CFString>.size), ptr, &size, &id)
    }
    return (status == noErr && id != kAudioObjectUnknown) ? id : nil
}

func describe(_ id: AudioObjectID) -> String {
    let hidden = uint32Property(id, kAudioDevicePropertyIsHidden) ?? 0
    return "id \(id)  \"\(stringProperty(id, kAudioObjectPropertyName))\"  in \(channels(id, kAudioObjectPropertyScopeInput))ch"
        + "  out \(channels(id, kAudioObjectPropertyScopeOutput))ch  \(Int(sampleRate(id))) Hz  hidden \(hidden == 0 ? "no" : "yes")"
}

func list() {
    print("Listed devices:")
    for id in listedDevices() {
        print("  \(describe(id))  uid \(stringProperty(id, kAudioDevicePropertyDeviceUID))")
    }
    print("VectorBridge devices by UID:")
    for uid in [rxApp, rxCore, txApp, txCore] {
        if let id = device(forUID: uid) {
            print("  \(uid): \(describe(id))")
        } else {
            print("  \(uid): not found")
        }
    }
    let status: String
    switch AVCaptureDevice.authorizationStatus(for: .audio) {
    case .authorized: status = "authorized"
    case .denied: status = "denied"
    case .restricted: status = "restricted"
    case .notDetermined: status = "not determined"
    @unknown default: status = "unknown"
    }
    print("Microphone permission for this process: \(status)")
}

// Shared between the IO callbacks and the main thread. The callbacks run on
// one serial queue, so they never race each other; main reads after stop.
final class Loop {
    let rate = 48_000.0
    let freq = 1_000.0
    let amplitude: Float = 0.5
    var phase = 0.0
    var written = 0
    var captured: [Float] = []
    var readCallbacks = 0
}

func loop(_ cable: String, seconds: Double) -> Int32 {
    let (writerUID, readerUID) = cable == "rx" ? (rxCore, rxApp) : (txApp, txCore)
    guard let writer = device(forUID: writerUID) else { print("not found: \(writerUID)"); return 1 }
    guard let reader = device(forUID: readerUID) else { print("not found: \(readerUID)"); return 1 }
    print("cable \(cable): write \(writerUID) -> read \(readerUID)")
    // Without Microphone permission the input reads silence, which would look
    // like a broken cable.
    let mic = AVCaptureDevice.authorizationStatus(for: .audio)
    if mic == .denied || mic == .restricted {
        print("Microphone permission is denied for this terminal app; allow it in System Settings, Privacy & Security, Microphone")
        return 3
    }

    let state = Loop()
    let queue = DispatchQueue(label: "vcprobe.io")
    var writeProc: AudioDeviceIOProcID?
    var readProc: AudioDeviceIOProcID?

    var status = AudioDeviceCreateIOProcIDWithBlock(&writeProc, writer, queue) { _, _, _, outData, _ in
        for buffer in UnsafeMutableAudioBufferListPointer(outData) {
            guard let data = buffer.mData?.assumingMemoryBound(to: Float.self) else { continue }
            let chans = Int(buffer.mNumberChannels)
            let frames = Int(buffer.mDataByteSize) / (MemoryLayout<Float>.size * max(chans, 1))
            for f in 0..<frames {
                let s = state.amplitude * Float(sin(state.phase))
                state.phase += 2 * Double.pi * state.freq / state.rate
                if state.phase > 2 * Double.pi { state.phase -= 2 * Double.pi }
                for c in 0..<chans { data[f * chans + c] = s }
            }
            state.written += frames
        }
    }
    guard status == noErr, let wp = writeProc else { print("writer IOProc failed: \(status)"); return 1 }

    status = AudioDeviceCreateIOProcIDWithBlock(&readProc, reader, queue) { _, inData, _, _, _ in
        state.readCallbacks += 1
        for buffer in UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: inData)) {
            guard let data = buffer.mData?.assumingMemoryBound(to: Float.self) else { continue }
            let chans = Int(buffer.mNumberChannels)
            let frames = Int(buffer.mDataByteSize) / (MemoryLayout<Float>.size * max(chans, 1))
            for f in 0..<frames { state.captured.append(data[f * chans]) }
        }
    }
    guard status == noErr, let rp = readProc else { print("reader IOProc failed: \(status)"); return 1 }

    status = AudioDeviceStart(writer, wp)
    guard status == noErr else { print("writer start failed: \(status)"); return 1 }
    status = AudioDeviceStart(reader, rp)
    guard status == noErr else { print("reader start failed: \(status)"); AudioDeviceStop(writer, wp); return 1 }
    Thread.sleep(forTimeInterval: seconds)
    AudioDeviceStop(reader, rp)
    AudioDeviceStop(writer, wp)
    AudioDeviceDestroyIOProcID(reader, rp)
    AudioDeviceDestroyIOProcID(writer, wp)

    // Skip the first 0.5 s (start-up), measure the rest.
    let samples = queue.sync { state.captured }
    let written = queue.sync { state.written }
    let callbacks = queue.sync { state.readCallbacks }
    let skip = min(samples.count, 24_000)
    let tail = Array(samples[skip...])
    let rms = tail.isEmpty ? 0 : sqrt(tail.reduce(0) { $0 + Double($1) * Double($1) } / Double(tail.count))
    let at1k = goertzelRMS(tail, freq: 1_000, rate: 48_000)
    let at1500 = goertzelRMS(tail, freq: 1_500, rate: 48_000)
    let expected = Double(state.amplitude) / 2.0.squareRoot()
    print(String(format: "written %d frames, read %d frames in %d callbacks", written, samples.count, callbacks))
    print(String(format: "measured RMS %.4f (expected %.4f), 1 kHz %.4f, 1.5 kHz %.4f", rms, expected, at1k, at1500))
    let pass = abs(rms - expected) < 0.1 * expected && at1k > 0.9 * expected && at1500 < 0.01
    print(pass ? "PASS: the tone crossed the cable" : "FAIL: the tone did not cross the cable intact")
    return pass ? 0 : 2
}

// RMS amplitude of one frequency component (Goertzel).
func goertzelRMS(_ x: [Float], freq: Double, rate: Double) -> Double {
    guard !x.isEmpty else { return 0 }
    let w = 2 * Double.pi * freq / rate
    let coeff = 2 * cos(w)
    var s1 = 0.0, s2 = 0.0
    for v in x {
        let s0 = Double(v) + coeff * s1 - s2
        s2 = s1
        s1 = s0
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2
    return (power.squareRoot() * 2 / Double(x.count)) / 2.0.squareRoot()
}

func net(_ host: String, _ port: UInt16) -> Int32 {
    var hints = addrinfo(ai_flags: 0, ai_family: AF_INET, ai_socktype: SOCK_STREAM, ai_protocol: 0,
                         ai_addrlen: 0, ai_canonname: nil, ai_addr: nil, ai_next: nil)
    var res: UnsafeMutablePointer<addrinfo>?
    guard getaddrinfo(host, String(port), &hints, &res) == 0, let ai = res else {
        print("cannot resolve \(host)"); return 1
    }
    defer { freeaddrinfo(res) }
    let fd = socket(ai.pointee.ai_family, ai.pointee.ai_socktype, ai.pointee.ai_protocol)
    defer { close(fd) }
    var tv = timeval(tv_sec: 3, tv_usec: 0)
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
    if connect(fd, ai.pointee.ai_addr, ai.pointee.ai_addrlen) == 0 {
        print("connected to \(host):\(port)")
        return 0
    }
    let err = errno
    print("connect to \(host):\(port) failed: \(String(cString: strerror(err))) (errno \(err))")
    return 2
}

let args = CommandLine.arguments
switch args.count > 1 ? args[1] : "" {
case "list":
    list()
case "loop" where args.count >= 3 && (args[2] == "rx" || args[2] == "tx"):
    exit(loop(args[2], seconds: args.count >= 4 ? Double(args[3]) ?? 3 : 3))
case "net" where args.count == 4:
    exit(net(args[2], UInt16(args[3]) ?? 80))
default:
    print("usage: vcprobe list | loop rx|tx [seconds] | net HOST PORT")
    exit(64)
}
