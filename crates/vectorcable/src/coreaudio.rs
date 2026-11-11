//! macOS: the core's side of the two VectorBridge cables (the HAL plug-ins
//! from `macos/driver`). Each cable is a BlackHole fork with two devices: the
//! one the desktop app sees, and a hidden mirror the core opens by UID.
//!
//! | Cable | The app uses | The core uses |
//! |---|---|---|
//! | RX | `VectorBridgeRX_UID` (input) | `VectorBridgeRX_2_UID` (output, hidden): the core writes here |
//! | TX | `VectorBridgeTX_UID` (output) | `VectorBridgeTX_2_UID` (input, hidden): the core reads here |
//!
//! Both are 2 channel Float32 at 48 kHz, one interleaved buffer (FACT, spike
//! 0a). The core is mono: RX goes to both channels; TX takes the left one.
//!
//! Pacing: the RX device's IOProc pulls from a FIFO, and `rx_demand` asks the
//! core for what tops the FIFO back up to a small target, so the device clock
//! sets the rate. The TX IOProc pushes what it captures; `read_tx` drains it.
//!
//! Plain FFI to CoreAudio, CoreFoundation and the Objective-C runtime (for
//! the Microphone permission), so there is no new dependency. The pure parts
//! build everywhere for tests; the FFI is macOS only.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::VecDeque;

use vbcore::audio::MAX_CATCH_UP_FRAMES;
use vbcore::protocol::{SAMPLES_PER_PACKET, SAMPLE_RATE};

use crate::ui::{CheckLine, Glyph};

/// The device the desktop app records from.
pub const RX_APP_UID: &str = "VectorBridgeRX_UID";
/// The hidden device the core writes RX audio to.
pub const RX_CORE_UID: &str = "VectorBridgeRX_2_UID";
/// The device the desktop app plays TX audio to.
pub const TX_APP_UID: &str = "VectorBridgeTX_UID";
/// The hidden device the core reads TX audio from.
pub const TX_CORE_UID: &str = "VectorBridgeTX_2_UID";

/// Most frames either FIFO keeps (1 s); beyond it the oldest are dropped, as
/// a device would.
const FIFO_CAP: usize = SAMPLE_RATE as usize;

/// Frames to keep queued for the RX device: two of its IO buffers plus one
/// core tick, so a tick that lands just after a buffer was pulled still
/// finds the next one full.
pub fn rx_target(buffer_frames: usize) -> usize {
    2 * buffer_frames + SAMPLES_PER_PACKET
}

/// What the core should render now: whatever tops the FIFO up to the target.
/// In steady state that is exactly what the device pulled since the last tick.
pub fn rx_demand_for(queued: usize, target: usize) -> usize {
    target.saturating_sub(queued).min(MAX_CATCH_UP_FRAMES)
}

fn to_f32(s: i16) -> f32 {
    s as f32 / 32768.0
}

fn to_i16(x: f32) -> i16 {
    (x * 32768.0)
        .round()
        .clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

/// Mono RX audio waiting for the RX device.
#[derive(Debug, Default)]
pub struct RxFifo {
    q: VecDeque<i16>,
}

impl RxFifo {
    pub fn with_capacity() -> RxFifo {
        RxFifo {
            q: VecDeque::with_capacity(FIFO_CAP),
        }
    }

    pub fn len(&self) -> usize {
        self.q.len()
    }

    pub fn push(&mut self, samples: &[i16]) {
        self.q.extend(samples.iter().copied());
        while self.q.len() > FIFO_CAP {
            self.q.pop_front();
        }
    }

    /// Fills an interleaved device buffer, the same sample on every channel;
    /// silence where the FIFO runs dry. Returns the frames that had audio.
    pub fn fill(&mut self, out: &mut [f32], channels: usize) -> usize {
        let channels = channels.max(1);
        let mut had = 0;
        for frame in out.chunks_mut(channels) {
            let s = match self.q.pop_front() {
                Some(s) => {
                    had += 1;
                    to_f32(s)
                }
                None => 0.0,
            };
            frame.fill(s);
        }
        had
    }
}

/// Mono TX audio the TX device captured, waiting for the core.
#[derive(Debug, Default)]
pub struct TxFifo {
    q: VecDeque<i16>,
}

impl TxFifo {
    pub fn with_capacity() -> TxFifo {
        TxFifo {
            q: VecDeque::with_capacity(FIFO_CAP),
        }
    }

    /// Takes the left channel of an interleaved device buffer.
    pub fn capture(&mut self, input: &[f32], channels: usize) {
        let frames = input.chunks_exact(channels.max(1));
        // A core that stalled loses the oldest audio, as a device would. Room
        // is made first, so the real-time thread never grows the queue.
        let keep = frames.len().min(FIFO_CAP);
        let skip = frames.len() - keep;
        let drop = (self.q.len() + keep).saturating_sub(FIFO_CAP);
        self.q.drain(..drop);
        self.q
            .extend(frames.skip(skip).map(|frame| to_i16(frame[0])));
    }

    pub fn drain_into(&mut self, out: &mut [i16]) -> usize {
        let n = out.len().min(self.q.len());
        for (o, s) in out[..n].iter_mut().zip(self.q.drain(..n)) {
            *o = s;
        }
        n
    }
}

/// Input frames (48 kHz) the radio monitor queues before it plays, and goes
/// back to after the queue has grown past [`MONITOR_HIGH`].
const MONITOR_TARGET: usize = SAMPLE_RATE as usize * 80 / 1000;
/// Above this the monitor drops its oldest audio, down to the target: the
/// output's clock runs slow against the RX device's.
const MONITOR_HIGH: usize = SAMPLE_RATE as usize * 120 / 1000;

/// The radio monitor's side tap: mono RX audio at 48 kHz on the RX device's
/// clock, played on the system output at that device's own rate and clock.
/// Linear interpolation resamples (radio audio is far below either Nyquist).
/// The two clocks drift: a queue past [`MONITOR_HIGH`] is cut back to
/// [`MONITOR_TARGET`]; one that runs dry plays silence and waits for the
/// target again. Either way a short gap in what you hear, never in the cable.
#[derive(Debug)]
pub struct MonitorFifo {
    q: VecDeque<f32>,
    /// Input frames per output frame: 48000 / the output's rate.
    step: f64,
    /// Where the next output frame falls between `q[0]` and `q[1]`.
    pos: f64,
    playing: bool,
    /// Output frames filled so far, audio or silence.
    pulled: u64,
}

impl MonitorFifo {
    pub fn new(out_rate: f64) -> MonitorFifo {
        MonitorFifo {
            q: VecDeque::with_capacity(MONITOR_HIGH + MAX_CATCH_UP_FRAMES),
            step: SAMPLE_RATE as f64 / out_rate,
            pos: 0.0,
            playing: false,
            pulled: 0,
        }
    }

    pub fn push(&mut self, samples: &[i16]) {
        self.q.extend(samples.iter().map(|&s| to_f32(s)));
        if self.q.len() > MONITOR_HIGH {
            let drop = self.q.len() - MONITOR_TARGET;
            self.q.drain(..drop);
        }
    }

    /// Fills an interleaved output buffer, the same sample on every channel.
    /// Returns the frames that had audio.
    pub fn fill(&mut self, out: &mut [f32], channels: usize) -> usize {
        let channels = channels.max(1);
        if !self.playing && self.q.len() >= MONITOR_TARGET {
            self.playing = true;
        }
        let mut had = 0;
        for frame in out.chunks_mut(channels) {
            if self.playing && self.q.len() < 2 {
                self.playing = false;
                self.pos = 0.0;
            }
            let s = if self.playing {
                had += 1;
                let s = self.q[0] + (self.q[1] - self.q[0]) * self.pos as f32;
                self.pos += self.step;
                while self.pos >= 1.0 {
                    self.q.pop_front();
                    self.pos -= 1.0;
                    if self.q.len() < 2 {
                        // Dry: wait for the target again, from the next sample.
                        self.playing = false;
                        self.pos = 0.0;
                        break;
                    }
                }
                s
            } else {
                0.0
            };
            frame.fill(s);
        }
        self.pulled += (out.len() / channels) as u64;
        had
    }
}

/// Copies the first channel of an interleaved buffer to every channel of
/// another buffer of the same frame count (a non-interleaved device has one
/// buffer per channel, and each must carry the monitor).
pub fn copy_first_channel(src: &[f32], src_channels: usize, dst: &mut [f32], dst_channels: usize) {
    let (sc, dc) = (src_channels.max(1), dst_channels.max(1));
    for (d, s) in dst.chunks_mut(dc).zip(src.chunks(sc)) {
        d.fill(s[0]);
    }
}

/// One Core Audio device as the monitor's guard sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceId {
    pub name: String,
    pub uid: String,
}

/// Why the default output must not carry the monitor, or None. `output` is
/// the device itself; `parts` are the devices inside it, at any depth, when
/// it is a Multi-Output or Aggregate device. A VectorBridge device anywhere
/// would loop RX into TX, and one that cannot be named cannot be checked.
pub fn monitor_refused(output: &DeviceId, parts: &[DeviceId]) -> Option<String> {
    use vbcore::audio::monitor_output_refused;
    let named = |d: &DeviceId| !d.name.is_empty() || !d.uid.is_empty();
    if !named(output) {
        return Some("the sound output has no name or UID to check".into());
    }
    let shown = if output.name.is_empty() {
        &output.uid
    } else {
        &output.name
    };
    if let Some(why) =
        monitor_output_refused(&output.name).or_else(|| monitor_output_refused(&output.uid))
    {
        return Some(why);
    }
    let bridge =
        |d: &DeviceId| monitor_output_refused(&d.name).or_else(|| monitor_output_refused(&d.uid));
    if parts.iter().any(|d| bridge(d).is_some()) {
        return Some(format!(
            "{shown} includes a VectorBridge device; choose speakers or headphones as the sound output"
        ));
    }
    (!parts.iter().all(named))
        .then(|| format!("{shown} includes a device with no name or UID to check"))
}

/// Which of the four driver devices Core Audio knows by UID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Installed {
    pub rx_app: bool,
    pub rx_core: bool,
    pub tx_app: bool,
    pub tx_core: bool,
}

impl Installed {
    /// Why the cables are not usable, if they are not.
    pub fn fault(self) -> Option<String> {
        let rx = self.rx_app && self.rx_core;
        let tx = self.tx_app && self.tx_core;
        let fix = "install the driver with install-mac.sh";
        match (rx, tx) {
            (true, true) => None,
            (false, false) => Some(format!(
                "VectorBridge RX and VectorBridge TX missing: {fix}"
            )),
            (false, true) => Some(format!("VectorBridge RX missing: {fix}")),
            (true, false) => Some(format!("VectorBridge TX missing: {fix}")),
        }
    }
}

/// `AVAuthorizationStatus` for audio capture (reading the TX device).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mic {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
    Unknown,
}

impl Mic {
    pub fn from_status(status: isize) -> Mic {
        match status {
            0 => Mic::NotDetermined,
            1 => Mic::Restricted,
            2 => Mic::Denied,
            3 => Mic::Authorized,
            _ => Mic::Unknown,
        }
    }

    /// Without the permission the TX device reads silence.
    pub fn blocks_tx(self) -> bool {
        matches!(self, Mic::Denied | Mic::Restricted)
    }
}

/// The terminal app macOS names in the Microphone prompt, from `TERM_PROGRAM`.
pub fn terminal_app(term_program: Option<&str>) -> String {
    match term_program {
        Some("Apple_Terminal") => "Terminal".into(),
        Some("iTerm.app") => "iTerm2".into(),
        Some("ghostty") => "Ghostty".into(),
        Some("WarpTerminal") => "Warp".into(),
        Some("vscode") => "Visual Studio Code".into(),
        Some(other) if !other.is_empty() => other.into(),
        _ => "your terminal app".into(),
    }
}

const MIC_SETTINGS: &str = "System Settings > Privacy & Security > Microphone";

/// The fault `up` shows while the permission keeps TX silent.
pub fn mic_fault(terminal: &str) -> String {
    format!("Microphone denied for {terminal}, so TX is silent: allow it in {MIC_SETTINGS}, then restart {terminal}")
}

/// The first-run screen goes up only before the prompt's first appearance,
/// and only for the Core Audio adapter (the null adapter reads no TX).
pub fn needs_mic_screen(mic: Mic, audio: Option<&str>) -> bool {
    mic == Mic::NotDetermined && audio.is_none()
}

/// Brief section 8: one plain screen, before any network or audio access,
/// naming the app the prompts will name. Each line fits 80 columns.
pub fn mic_screen(terminal: &str, term_program: Option<&str>) -> Vec<String> {
    let mut lines = vec![
        "VectorCable: macOS asks one question first".to_string(),
        String::new(),
        "VectorCable reads the VectorBridge TX device to hear what WSJT-X, JS8Call".to_string(),
        "or FLDIGI sends to the radio. macOS treats that as the microphone, so it".to_string(),
        format!("will ask next whether {terminal} may use the microphone."),
        String::new(),
        "  Allow         TX audio reaches the radio.".to_string(),
        "  Don't Allow   TX is silent. Listen Only still works.".to_string(),
        String::new(),
        format!("To change it later: {MIC_SETTINGS},"),
        format!("then restart {terminal}."),
    ];
    // TN3179: Terminal.app is exempt from Local Network privacy; other
    // terminals are not (INFERRED in the brief).
    if term_program != Some("Apple_Terminal") {
        lines.push(String::new());
        lines.push(format!(
            "macOS may also ask whether {terminal} may find devices on"
        ));
        lines
            .push("your local network. Allow it, so VectorCable can reach the bridge.".to_string());
    }
    lines
}

/// `doctor`'s Permissions row on macOS. Local Network has no API to ask, so
/// Bridge reachable stands in for it.
pub fn permissions_check(mic: Mic, terminal: &str) -> CheckLine {
    let (glyph, word, detail) = match mic {
        Mic::Authorized => (
            Glyph::Ok,
            "Good",
            format!("Microphone allowed for {terminal}"),
        ),
        Mic::Denied | Mic::Restricted => (Glyph::Fix, "Fix", mic_fault(terminal)),
        Mic::NotDetermined => (
            Glyph::Check,
            "Check",
            format!("Microphone not asked yet: the first up asks for {terminal}; allow it, or TX is silent"),
        ),
        Mic::Unknown => (
            Glyph::Check,
            "Check",
            format!("Microphone state unknown for {terminal}: see {MIC_SETTINGS}"),
        ),
    };
    CheckLine {
        name: "Permissions",
        glyph,
        word,
        detail,
    }
}

/// `doctor`'s Driver row on macOS.
pub fn driver_check(installed: Installed) -> CheckLine {
    let (glyph, word, detail) = match installed.fault() {
        None => (
            Glyph::Ok,
            "Good",
            "VectorBridge RX and VectorBridge TX installed".to_string(),
        ),
        Some(fault) => (Glyph::Fix, "Fix", fault),
    };
    CheckLine {
        name: "Driver",
        glyph,
        word,
        detail,
    }
}

#[cfg(target_os = "macos")]
pub use mac::{installed_now, mic_now, CoreAudioAdapter};

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{c_char, c_void, CString};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use vbcore::audio::{AudioAdapter, MonitorEvent, MonitorWorker};
    use vbcore::time::LocalTime;

    use super::*;

    type OsStatus = i32;
    type ObjectId = u32;
    type CfStringRef = *const c_void;
    type IoProc = unsafe extern "C" fn(
        ObjectId,
        *const c_void,
        *const AudioBufferList,
        *const c_void,
        *mut AudioBufferList,
        *const c_void,
        *mut c_void,
    ) -> OsStatus;

    #[repr(C)]
    struct PropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[repr(C)]
    struct AudioBuffer {
        channels: u32,
        bytes: u32,
        data: *mut c_void,
    }

    /// The C struct is variable length; only the first buffer is declared.
    #[repr(C)]
    struct AudioBufferList {
        count: u32,
        buffers: [AudioBuffer; 1],
    }

    const fn fourcc(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }
    const SYSTEM_OBJECT: ObjectId = 1;
    const UNKNOWN_OBJECT: ObjectId = 0;
    const SCOPE_GLOBAL: u32 = fourcc(b"glob");
    const SCOPE_INPUT: u32 = fourcc(b"inpt");
    const SCOPE_OUTPUT: u32 = fourcc(b"outp");
    const ELEMENT_MAIN: u32 = 0;
    const TRANSLATE_UID_TO_DEVICE: u32 = fourcc(b"uidd");
    const NOMINAL_SAMPLE_RATE: u32 = fourcc(b"nsrt");
    const STREAM_CONFIGURATION: u32 = fourcc(b"slay");
    const BUFFER_FRAME_SIZE: u32 = fourcc(b"fsiz");
    const DEFAULT_OUTPUT_DEVICE: u32 = fourcc(b"dOut");
    const DEVICE_UID: u32 = fourcc(b"uid ");
    const OBJECT_NAME: u32 = fourcc(b"lnam");
    const AGGREGATE_ACTIVE_SUBDEVICES: u32 = fourcc(b"agrp");
    const IOPROC_STREAM_USAGE: u32 = fourcc(b"suse");
    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyDataSize(
            object: ObjectId,
            address: *const PropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
        ) -> OsStatus;
        fn AudioObjectGetPropertyData(
            object: ObjectId,
            address: *const PropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> OsStatus;
        fn AudioObjectSetPropertyData(
            object: ObjectId,
            address: *const PropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: u32,
            data: *const c_void,
        ) -> OsStatus;
        fn AudioDeviceCreateIOProcID(
            device: ObjectId,
            proc_: IoProc,
            client: *mut c_void,
            id: *mut Option<IoProc>,
        ) -> OsStatus;
        fn AudioDeviceDestroyIOProcID(device: ObjectId, id: IoProc) -> OsStatus;
        fn AudioDeviceStart(device: ObjectId, id: Option<IoProc>) -> OsStatus;
        fn AudioDeviceStop(device: ObjectId, id: Option<IoProc>) -> OsStatus;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            s: *const c_char,
            encoding: u32,
        ) -> CfStringRef;
        fn CFStringGetCString(
            s: CfStringRef,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "AVFoundation", kind = "framework")]
    extern "C" {
        static AVMediaTypeAudio: *const c_void;
    }

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *const c_void;
        fn sel_registerName(name: *const c_char) -> *const c_void;
        fn objc_msgSend();
    }

    fn address(selector: u32, scope: u32) -> PropertyAddress {
        PropertyAddress {
            selector,
            scope,
            element: ELEMENT_MAIN,
        }
    }

    /// The device with this UID, hidden ones included.
    fn device_for_uid(uid: &str) -> Option<ObjectId> {
        let c = CString::new(uid).ok()?;
        // SAFETY: plain CoreFoundation and CoreAudio calls with live pointers;
        // the CFString is released before returning.
        unsafe {
            let cf =
                CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), CF_STRING_ENCODING_UTF8);
            if cf.is_null() {
                return None;
            }
            let addr = address(TRANSLATE_UID_TO_DEVICE, SCOPE_GLOBAL);
            let mut id: ObjectId = UNKNOWN_OBJECT;
            let mut size = std::mem::size_of::<ObjectId>() as u32;
            let status = AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &addr,
                std::mem::size_of::<CfStringRef>() as u32,
                &cf as *const CfStringRef as *const c_void,
                &mut size,
                &mut id as *mut ObjectId as *mut c_void,
            );
            CFRelease(cf);
            (status == 0 && id != UNKNOWN_OBJECT).then_some(id)
        }
    }

    fn property<T: Default>(device: ObjectId, selector: u32, scope: u32) -> Option<T> {
        let addr = address(selector, scope);
        let mut value = T::default();
        let mut size = std::mem::size_of::<T>() as u32;
        // SAFETY: `value` is a plain number of the size passed.
        let status = unsafe {
            AudioObjectGetPropertyData(
                device,
                &addr,
                0,
                std::ptr::null(),
                &mut size,
                &mut value as *mut T as *mut c_void,
            )
        };
        (status == 0).then_some(value)
    }

    /// A CFString property (a name or UID) as a Rust string.
    fn string_property(object: ObjectId, selector: u32) -> Option<String> {
        let addr = address(selector, SCOPE_GLOBAL);
        let mut cf: CfStringRef = std::ptr::null();
        let mut size = std::mem::size_of::<CfStringRef>() as u32;
        // SAFETY: Core Audio hands back a CFString we own (released here);
        // the copy is NUL terminated within the buffer it was given.
        unsafe {
            let status = AudioObjectGetPropertyData(
                object,
                &addr,
                0,
                std::ptr::null(),
                &mut size,
                &mut cf as *mut CfStringRef as *mut c_void,
            );
            if status != 0 || cf.is_null() {
                return None;
            }
            let mut buf = [0 as c_char; 1024];
            let ok = CFStringGetCString(
                cf,
                buf.as_mut_ptr(),
                buf.len() as isize,
                CF_STRING_ENCODING_UTF8,
            );
            CFRelease(cf);
            (ok != 0).then(|| {
                std::ffi::CStr::from_ptr(buf.as_ptr())
                    .to_string_lossy()
                    .into_owned()
            })
        }
    }

    /// The devices an Aggregate or Multi-Output device is playing through now;
    /// empty for a plain device.
    fn sub_devices(device: ObjectId) -> Vec<ObjectId> {
        let addr = address(AGGREGATE_ACTIVE_SUBDEVICES, SCOPE_GLOBAL);
        let mut size: u32 = 0;
        // SAFETY: an array of object ids read into a buffer of the size Core
        // Audio gave, and only the ids that fit are kept.
        unsafe {
            if AudioObjectGetPropertyDataSize(device, &addr, 0, std::ptr::null(), &mut size) != 0 {
                return Vec::new();
            }
            let mut ids = vec![UNKNOWN_OBJECT; size as usize / std::mem::size_of::<ObjectId>()];
            let mut size = (ids.len() * std::mem::size_of::<ObjectId>()) as u32;
            if ids.is_empty()
                || AudioObjectGetPropertyData(
                    device,
                    &addr,
                    0,
                    std::ptr::null(),
                    &mut size,
                    ids.as_mut_ptr() as *mut c_void,
                ) != 0
            {
                return Vec::new();
            }
            ids.truncate(size as usize / std::mem::size_of::<ObjectId>());
            ids
        }
    }

    fn device_id(device: ObjectId) -> DeviceId {
        DeviceId {
            name: string_property(device, OBJECT_NAME).unwrap_or_default(),
            uid: string_property(device, DEVICE_UID).unwrap_or_default(),
        }
    }

    /// Every device inside `device`, at any depth (an aggregate can hold one).
    fn parts_of(device: ObjectId) -> Vec<DeviceId> {
        let (mut parts, mut seen, mut todo) = (Vec::new(), vec![device], vec![(device, 0)]);
        while let Some((d, depth)) = todo.pop() {
            // Deeper than this is not a real setup; stop rather than loop.
            if depth >= 4 {
                continue;
            }
            for sub in sub_devices(d) {
                if !seen.contains(&sub) {
                    seen.push(sub);
                    parts.push(device_id(sub));
                    todo.push((sub, depth + 1));
                }
            }
        }
        parts
    }

    /// The input-stream flags of `proc_`'s `AudioHardwareIOProcStreamUsage`
    /// on `device` (a pointer, a count, a u32 per stream), first set all off
    /// when `off`. None when the device has no input streams.
    fn input_stream_usage(device: ObjectId, proc_: IoProc, off: bool) -> Option<Vec<u32>> {
        let addr = address(IOPROC_STREAM_USAGE, SCOPE_INPUT);
        let head = std::mem::size_of::<*const c_void>() + std::mem::size_of::<u32>();
        let mut size: u32 = 0;
        // SAFETY: the struct is read into an 8-byte aligned buffer of the size
        // Core Audio gave; its proc field is set before the read, as the HAL
        // requires, and only flags within that size are touched.
        unsafe {
            if AudioObjectGetPropertyDataSize(device, &addr, 0, std::ptr::null(), &mut size) != 0
                || (size as usize) <= head
            {
                return None;
            }
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let base = buf.as_mut_ptr() as *mut u8;
            *(base as *mut IoProc) = proc_;
            if AudioObjectGetPropertyData(
                device,
                &addr,
                0,
                std::ptr::null(),
                &mut size,
                base as *mut c_void,
            ) != 0
            {
                return None;
            }
            let count = *(base.add(std::mem::size_of::<*const c_void>()) as *const u32) as usize;
            let count = count.min((size as usize).saturating_sub(head) / 4);
            let flags = base.add(head) as *mut u32;
            if count == 0 {
                return None;
            }
            if off {
                for i in 0..count {
                    *flags.add(i) = 0;
                }
                AudioObjectSetPropertyData(
                    device,
                    &addr,
                    0,
                    std::ptr::null(),
                    size,
                    base as *const c_void,
                );
            }
            Some((0..count).map(|i| *flags.add(i)).collect())
        }
    }

    /// Channels on one side of a device, summed over its streams.
    fn channels(device: ObjectId, scope: u32) -> usize {
        let addr = address(STREAM_CONFIGURATION, scope);
        let mut size: u32 = 0;
        // SAFETY: the list is read into a buffer of the size Core Audio gave,
        // 8-byte aligned, and walked only within that size.
        unsafe {
            if AudioObjectGetPropertyDataSize(device, &addr, 0, std::ptr::null(), &mut size) != 0
                || (size as usize) < std::mem::size_of::<u32>()
            {
                return 0;
            }
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            if AudioObjectGetPropertyData(
                device,
                &addr,
                0,
                std::ptr::null(),
                &mut size,
                buf.as_mut_ptr() as *mut c_void,
            ) != 0
            {
                return 0;
            }
            let list = buf.as_ptr() as *const AudioBufferList;
            let count = (*list).count as usize;
            let first = std::ptr::addr_of!((*list).buffers) as *const AudioBuffer;
            let fits = (size as usize)
                .saturating_sub(std::mem::offset_of!(AudioBufferList, buffers))
                / std::mem::size_of::<AudioBuffer>();
            (0..count.min(fits))
                .map(|i| (*first.add(i)).channels as usize)
                .sum()
        }
    }

    /// Which of the four driver devices Core Audio knows now.
    pub fn installed_now() -> Installed {
        Installed {
            rx_app: device_for_uid(RX_APP_UID).is_some(),
            rx_core: device_for_uid(RX_CORE_UID).is_some(),
            tx_app: device_for_uid(TX_APP_UID).is_some(),
            tx_core: device_for_uid(TX_CORE_UID).is_some(),
        }
    }

    /// `[AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio]`
    /// for this process (macOS attributes it to the terminal app).
    pub fn mic_now() -> Mic {
        // SAFETY: the class and selector exist in AVFoundation, which is
        // linked; objc_msgSend is called through the method's own signature.
        unsafe {
            let class = objc_getClass(c"AVCaptureDevice".as_ptr());
            if class.is_null() {
                return Mic::Unknown;
            }
            let sel = sel_registerName(c"authorizationStatusForMediaType:".as_ptr());
            let send: unsafe extern "C" fn(*const c_void, *const c_void, *const c_void) -> isize =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            Mic::from_status(send(class, sel, AVMediaTypeAudio))
        }
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        // A panic cannot cross the IOProc, so never unwrap a poisoned lock.
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    enum Side {
        Output(Arc<Mutex<RxFifo>>),
        Input(Arc<Mutex<TxFifo>>),
        Monitor(Tap),
    }

    /// The radio monitor's FIFO while it is on, shared by `write_rx`, the
    /// monitor's IOProc and the worker that opens and closes it. Nothing slow
    /// (and no allocation) happens under its lock.
    type Tap = Arc<Mutex<Option<MonitorFifo>>>;

    /// What one IOProc reaches: its FIFO and a count of its callbacks.
    struct Context {
        side: Side,
        callbacks: AtomicU64,
    }

    /// Only the first buffer carries audio (the driver has one interleaved
    /// stream); any others are zeroed on output. The monitor's output may be
    /// any device, so every buffer it has carries the monitor.
    unsafe extern "C" fn io_proc(
        _device: ObjectId,
        _now: *const c_void,
        input: *const AudioBufferList,
        _input_time: *const c_void,
        output: *mut AudioBufferList,
        _output_time: *const c_void,
        client: *mut c_void,
    ) -> OsStatus {
        // SAFETY: `client` is the Context this proc was created with, alive
        // until after the proc is stopped and destroyed; the buffer lists are
        // Core Audio's, valid for this call.
        let ctx = &*(client as *const Context);
        ctx.callbacks.fetch_add(1, Ordering::Relaxed);
        match &ctx.side {
            Side::Output(fifo) => {
                if output.is_null() {
                    return 0;
                }
                // From the raw pointer: the list runs past its one declared buffer.
                let first = std::ptr::addr_of_mut!((*output).buffers) as *mut AudioBuffer;
                for i in 0..(*output).count as usize {
                    let b = &mut *first.add(i);
                    if b.data.is_null() {
                        continue;
                    }
                    let out =
                        std::slice::from_raw_parts_mut(b.data as *mut f32, b.bytes as usize / 4);
                    if i == 0 {
                        lock(fifo).fill(out, b.channels as usize);
                    } else {
                        out.fill(0.0);
                    }
                }
            }
            Side::Monitor(fifo) => {
                if output.is_null() || (*output).count == 0 {
                    return 0;
                }
                let first = std::ptr::addr_of_mut!((*output).buffers) as *mut AudioBuffer;
                let b0 = &mut *first;
                if b0.data.is_null() {
                    return 0;
                }
                let ch0 = b0.channels as usize;
                let out0 =
                    std::slice::from_raw_parts_mut(b0.data as *mut f32, b0.bytes as usize / 4);
                match lock(fifo).as_mut() {
                    Some(f) => {
                        f.fill(out0, ch0);
                    }
                    None => out0.fill(0.0),
                }
                for i in 1..(*output).count as usize {
                    let b = &mut *first.add(i);
                    if b.data.is_null() {
                        continue;
                    }
                    let out =
                        std::slice::from_raw_parts_mut(b.data as *mut f32, b.bytes as usize / 4);
                    copy_first_channel(out0, ch0, out, b.channels as usize);
                }
            }
            Side::Input(fifo) => {
                if input.is_null() || (*input).count == 0 {
                    return 0;
                }
                let b = &(*input).buffers[0];
                if !b.data.is_null() {
                    let data =
                        std::slice::from_raw_parts(b.data as *const f32, b.bytes as usize / 4);
                    lock(fifo).capture(data, b.channels as usize);
                }
            }
        }
        0
    }

    /// A running IOProc on one device; stopped and destroyed on drop.
    struct Running {
        device: ObjectId,
        id: IoProc,
        context: Box<Context>,
    }

    impl Running {
        fn start(device: ObjectId, side: Side, what: &str) -> Result<Running, String> {
            let context = Box::new(Context {
                side,
                callbacks: AtomicU64::new(0),
            });
            let mut id: Option<IoProc> = None;
            // SAFETY: the context outlives the proc (see Drop).
            unsafe {
                let status = AudioDeviceCreateIOProcID(
                    device,
                    io_proc,
                    &*context as *const Context as *mut c_void,
                    &mut id,
                );
                let Some(id) = id.filter(|_| status == 0) else {
                    return Err(format!(
                        "{what}: Core Audio would not attach (error {status})"
                    ));
                };
                // The monitor only plays: an output that is also a microphone
                // (a headset, AirPods) must not have its input turned on.
                if matches!(context.side, Side::Monitor(_)) {
                    input_stream_usage(device, id, true);
                }
                let status = AudioDeviceStart(device, Some(id));
                if status != 0 {
                    AudioDeviceDestroyIOProcID(device, id);
                    return Err(format!(
                        "{what}: Core Audio would not start (error {status})"
                    ));
                }
                Ok(Running {
                    device,
                    id,
                    context,
                })
            }
        }

        fn callbacks(&self) -> u64 {
            self.context.callbacks.load(Ordering::Relaxed)
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            // SAFETY: stop waits out a callback in flight; only then is the
            // proc destroyed and the context freed.
            unsafe {
                AudioDeviceStop(self.device, Some(self.id));
                AudioDeviceDestroyIOProcID(self.device, self.id);
            }
        }
    }

    /// A device whose callbacks stop for this long has gone (driver removed,
    /// Core Audio restarted).
    const STALL: Duration = Duration::from_secs(1);

    /// The Microphone state is asked again this often while running.
    const MIC_RECHECK: Duration = Duration::from_secs(1);

    /// When each IOProc's callback count last moved.
    struct Watch {
        rx: (u64, Instant),
        tx: (u64, Instant),
    }

    /// Asks for the Microphone state every [`MIC_RECHECK`] on its own thread.
    /// The answer comes from the TCC daemon and takes 17 to 40 ms (FACT,
    /// 2026-10-06), longer than the RX FIFO holds, so asking on the core's
    /// loop starved the RX device and the timeline fell behind its target.
    struct MicWatch {
        state: Arc<Mutex<Mic>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl MicWatch {
        fn start(first: Mic) -> Result<MicWatch, String> {
            let state = Arc::new(Mutex::new(first));
            let stop = Arc::new(AtomicBool::new(false));
            let (s, st) = (state.clone(), stop.clone());
            let thread = std::thread::Builder::new()
                .name("vectorcable-mic".into())
                .spawn(move || loop {
                    std::thread::park_timeout(MIC_RECHECK);
                    if st.load(Ordering::Relaxed) {
                        break;
                    }
                    let mic = mic_now();
                    *lock(&s) = mic;
                })
                .map_err(|e| format!("cannot start the Microphone check: {e}"))?;
            Ok(MicWatch {
                state,
                stop,
                thread: Some(thread),
            })
        }

        fn get(&self) -> Mic {
            *lock(&self.state)
        }
    }

    impl Drop for MicWatch {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                t.thread().unpark();
                let _ = t.join();
            }
        }
    }

    fn open(uid: &str, scope: u32, what: &str) -> Result<(ObjectId, usize), String> {
        let device = device_for_uid(uid)
            .ok_or_else(|| format!("{what} not found: install the driver with install-mac.sh"))?;
        let rate: f64 = property(device, NOMINAL_SAMPLE_RATE, SCOPE_GLOBAL).unwrap_or(0.0);
        if rate != SAMPLE_RATE as f64 {
            return Err(format!("{what} runs at {rate} Hz, not {SAMPLE_RATE}"));
        }
        let channels = channels(device, scope);
        if channels == 0 {
            return Err(format!("{what} has no channels on the side the core uses"));
        }
        Ok((device, channels))
    }

    /// The radio monitor while it is on: an IOProc on the system's default
    /// output (as it was when the monitor was turned on). Its callbacks are
    /// not watched by `fault`: a monitor that stops leaves the cable fine.
    struct Monitor {
        name: String,
        /// Held for its Drop, which stops the IOProc.
        _running: Running,
    }

    impl Monitor {
        /// Slow (a Bluetooth output can take a second): only on the worker.
        fn start(tap: &Tap) -> Result<Monitor, String> {
            let device = property::<ObjectId>(SYSTEM_OBJECT, DEFAULT_OUTPUT_DEVICE, SCOPE_GLOBAL)
                .filter(|&d| d != UNKNOWN_OBJECT)
                .ok_or("there is no default sound output")?;
            let output = device_id(device);
            if let Some(why) = monitor_refused(&output, &parts_of(device)) {
                return Err(why);
            }
            let name = if output.name.is_empty() {
                output.uid
            } else {
                output.name
            };
            let rate: f64 = property(device, NOMINAL_SAMPLE_RATE, SCOPE_GLOBAL).unwrap_or(0.0);
            if !rate.is_finite() || rate < 8000.0 {
                return Err(format!("{name} reports no sample rate"));
            }
            if channels(device, SCOPE_OUTPUT) == 0 {
                return Err(format!("{name} has no output channels"));
            }
            let fresh = MonitorFifo::new(rate);
            let old = lock(tap).replace(fresh);
            drop(old);
            match Running::start(device, Side::Monitor(tap.clone()), &name) {
                Ok(running) => Ok(Monitor {
                    name,
                    _running: running,
                }),
                Err(why) => {
                    let old = lock(tap).take();
                    drop(old);
                    Err(why)
                }
            }
        }
    }

    /// The monitor's worker: whatever is open closes first (stopping an
    /// IOProc is slow too), so a repeat `on` re-picks the default output; the
    /// last `apply(false)`, on drop, closes it for good.
    fn spawn_monitor(tap: Tap) -> Result<MonitorWorker, String> {
        let mut current: Option<Monitor> = None;
        MonitorWorker::spawn(move |on| {
            let old = lock(&tap).take();
            drop(old);
            // Stops the open output's IOProc, if any.
            drop(current.take());
            if !on {
                return MonitorEvent::Off;
            }
            match Monitor::start(&tap) {
                Ok(m) => {
                    let name = m.name.clone();
                    current.replace(m);
                    MonitorEvent::On(name)
                }
                Err(why) => MonitorEvent::Refused(why),
            }
        })
        .map_err(|e| format!("cannot start the radio monitor: {e}"))
    }

    pub struct CoreAudioAdapter {
        rx: Arc<Mutex<RxFifo>>,
        tx: Arc<Mutex<TxFifo>>,
        rx_target: usize,
        terminal: String,
        watch: Mutex<Watch>,
        mic: MicWatch,
        tap: Tap,
        /// Dropped before the cable devices: it closes the monitor and waits.
        monitor: MonitorWorker,
        // Dropped last: stop the devices before the FIFOs go.
        rx_proc: Running,
        tx_proc: Running,
    }

    impl CoreAudioAdapter {
        /// Opens both hidden devices and starts them. Opening the TX input
        /// waits for the user to answer the Microphone prompt the first time.
        pub fn new() -> Result<CoreAudioAdapter, String> {
            let (rx_dev, _) = open(RX_CORE_UID, SCOPE_OUTPUT, "VectorBridge RX (core side)")?;
            let (tx_dev, _) = open(TX_CORE_UID, SCOPE_INPUT, "VectorBridge TX (core side)")?;
            let buffer: u32 = property(rx_dev, BUFFER_FRAME_SIZE, SCOPE_GLOBAL).unwrap_or(512);
            let rx = Arc::new(Mutex::new(RxFifo::with_capacity()));
            let tx = Arc::new(Mutex::new(TxFifo::with_capacity()));
            let rx_proc = Running::start(rx_dev, Side::Output(rx.clone()), "VectorBridge RX")?;
            let tx_proc = Running::start(tx_dev, Side::Input(tx.clone()), "VectorBridge TX")?;
            let now = Instant::now();
            let tap: Tap = Arc::new(Mutex::new(None));
            Ok(CoreAudioAdapter {
                rx,
                tx,
                rx_target: rx_target(buffer as usize),
                terminal: terminal_app(std::env::var("TERM_PROGRAM").ok().as_deref()),
                watch: Mutex::new(Watch {
                    rx: (0, now),
                    tx: (0, now),
                }),
                mic: MicWatch::start(mic_now())?,
                monitor: spawn_monitor(tap.clone())?,
                tap,
                rx_proc,
                tx_proc,
            })
        }
    }

    impl AudioAdapter for CoreAudioAdapter {
        fn name(&self) -> &str {
            "coreaudio"
        }

        /// A device that stopped calling back comes first, then the
        /// Microphone permission (TX reads silence without it). While the
        /// Microphone prompt is unanswered the TX input may not call back,
        /// which is not a stall.
        fn fault(&self) -> Option<String> {
            let now = Instant::now();
            let w = &mut *lock(&self.watch);
            let mic = self.mic.get();
            if mic == Mic::NotDetermined {
                w.tx.1 = now;
            }
            for (beat, count, what) in [
                (&mut w.rx, self.rx_proc.callbacks(), "VectorBridge RX"),
                (&mut w.tx, self.tx_proc.callbacks(), "VectorBridge TX"),
            ] {
                if count != beat.0 {
                    *beat = (count, now);
                } else if now - beat.1 > STALL {
                    return Some(format!(
                        "{what} stopped: the driver was removed or Core Audio restarted (restart VectorCable)"
                    ));
                }
            }
            mic.blocks_tx().then(|| mic_fault(&self.terminal))
        }

        fn rx_demand(&mut self, _now: LocalTime) -> usize {
            rx_demand_for(lock(&self.rx).len(), self.rx_target)
        }

        fn read_tx(&mut self, _now: LocalTime, out: &mut [i16]) -> usize {
            lock(&self.tx).drain_into(out)
        }

        fn write_rx(&mut self, samples: &[i16]) {
            lock(&self.rx).push(samples);
            if let Some(f) = lock(&self.tap).as_mut() {
                f.push(samples);
            }
        }

        fn monitor_supported(&self) -> bool {
            true
        }

        /// Returns at once: the worker opens and closes the output.
        fn set_monitor(&mut self, on: bool) {
            self.monitor.request(on);
        }

        fn monitor_event(&mut self) -> Option<MonitorEvent> {
            self.monitor.event()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The "app" end of a cable, built from the same parts.
        fn app_side(uid: &str, scope: u32, side: Side) -> Running {
            let (dev, _) = open(uid, scope, uid).expect("driver installed");
            Running::start(dev, side, uid).unwrap()
        }

        fn tone(n: usize, start: usize) -> Vec<i16> {
            (start..start + n)
                .map(|k| {
                    let t = k as f64 / SAMPLE_RATE as f64;
                    (0.5 * 32767.0 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as i16
                })
                .collect()
        }

        fn rms(x: &[i16]) -> f64 {
            (x.iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
        }

        /// Needs the driver and Microphone permission for the terminal:
        /// `cargo test -p vectorcable coreaudio -- --ignored`.
        #[test]
        #[ignore]
        fn tone_crosses_both_cables_through_the_adapter() {
            let mut adapter = CoreAudioAdapter::new().expect("adapter opens");
            assert_eq!(
                adapter.mic.get(),
                Mic::Authorized,
                "allow the Microphone first"
            );
            // The app records VectorBridge RX and plays into VectorBridge TX.
            let heard = Arc::new(Mutex::new(TxFifo::with_capacity()));
            let _rx_app = app_side(RX_APP_UID, SCOPE_INPUT, Side::Input(heard.clone()));
            let played = Arc::new(Mutex::new(RxFifo::with_capacity()));
            let _tx_app = app_side(TX_APP_UID, SCOPE_OUTPUT, Side::Output(played.clone()));

            let start = Instant::now();
            let (mut rx_sent, mut tx_sent, mut ticks) = (0usize, 0usize, 0u32);
            let mut demanded = 0usize;
            let mut slowest_fault = Duration::ZERO;
            let mut heard_all = Vec::new();
            let mut tx_got = Vec::new();
            let mut block = vec![0i16; 4800];
            while start.elapsed() < Duration::from_secs(4) {
                let now = LocalTime(start.elapsed().as_secs_f64());
                let want = adapter.rx_demand(now);
                if start.elapsed() > Duration::from_millis(500) {
                    demanded += want;
                }
                adapter.write_rx(&tone(want, rx_sent));
                rx_sent += want;
                let more = rx_demand_for(lock(&played).len(), adapter.rx_target);
                lock(&played).push(&tone(more, tx_sent));
                tx_sent += more;
                let n = adapter.read_tx(now, &mut block);
                tx_got.extend_from_slice(&block[..n]);
                let n = lock(&heard).drain_into(&mut block);
                heard_all.extend_from_slice(&block[..n]);
                // The core calls this every tick: it must never cost a device buffer.
                let asked = Instant::now();
                assert_eq!(adapter.fault(), None);
                slowest_fault = slowest_fault.max(asked.elapsed());
                ticks += 1;
                std::thread::sleep(
                    (start + Duration::from_millis(10) * ticks)
                        .saturating_duration_since(Instant::now()),
                );
            }
            let expected = 0.5 / 2f64.sqrt();
            for (what, got) in [("rx", &heard_all), ("tx", &tx_got)] {
                assert!(
                    got.len() > 3 * SAMPLE_RATE as usize,
                    "{what}: {} frames",
                    got.len()
                );
                let r = rms(&got[SAMPLE_RATE as usize..]);
                assert!((r - expected).abs() < 0.05 * expected, "{what}: rms {r}");
            }
            assert!(
                slowest_fault < Duration::from_millis(5),
                "fault() took {slowest_fault:?}; the RX FIFO holds about 30 ms"
            );
            // 3.5 s at the device's 48 kHz, give or take a buffer at each end.
            let nominal = 3.5 * SAMPLE_RATE as f64;
            assert!(
                (demanded as f64 - nominal).abs() < 0.02 * nominal,
                "demanded {demanded} frames in 3.5 s"
            );
        }

        /// Polls `monitor_event` for up to 5 s, as the runner does each tick.
        fn next_event(adapter: &mut CoreAudioAdapter) -> MonitorEvent {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(e) = adapter.monitor_event() {
                    return e;
                }
                assert!(Instant::now() < deadline, "no monitor event");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// Needs the driver and a default output that is not VectorBridge.
        /// Plays silence: `cargo test -p vectorcable coreaudio -- --ignored`.
        #[test]
        #[ignore]
        fn monitor_plays_on_the_default_output_without_changing_rx_pacing() {
            let mut adapter = CoreAudioAdapter::new().expect("adapter opens");
            assert!(adapter.monitor_supported());
            let start = Instant::now();
            let (mut demanded, mut ticks) = (0usize, 0u32);
            let mut slowest = Duration::ZERO;
            let mut on_after = None;
            adapter.set_monitor(true);
            while start.elapsed() < Duration::from_secs(3) {
                // The RX loop's calls, the monitor ones included, while the
                // output opens: none may wait on it.
                let asked = Instant::now();
                let want = adapter.rx_demand(LocalTime(start.elapsed().as_secs_f64()));
                adapter.write_rx(&vec![0; want]);
                if let Some(e) = adapter.monitor_event() {
                    assert!(matches!(e, MonitorEvent::On(_)), "{e:?}");
                    on_after = Some(start.elapsed());
                }
                assert_eq!(adapter.fault(), None);
                slowest = slowest.max(asked.elapsed());
                if start.elapsed() > Duration::from_millis(500) {
                    demanded += want;
                }
                ticks += 1;
                std::thread::sleep(
                    (start + Duration::from_millis(10) * ticks)
                        .saturating_duration_since(Instant::now()),
                );
            }
            let on_after = on_after.expect("monitor turned on");
            eprintln!("monitor on after {on_after:?}; slowest RX tick {slowest:?}");
            assert!(
                slowest < Duration::from_millis(5),
                "an RX tick took {slowest:?}"
            );
            // The RX device still sets the pace, as without the monitor.
            let nominal = 2.5 * SAMPLE_RATE as f64;
            assert!(
                (demanded as f64 - nominal).abs() < 0.02 * nominal,
                "demanded {demanded} frames in 2.5 s"
            );
            // The output pulled what it plays: at least a second's worth.
            let pulled = lock(&adapter.tap).as_ref().expect("tap open").pulled;
            assert!(pulled > 24_000, "the output pulled {pulled} frames");
            // A repeat on re-picks the output; off closes it.
            adapter.set_monitor(true);
            assert!(matches!(next_event(&mut adapter), MonitorEvent::On(_)));
            assert!(lock(&adapter.tap).is_some());
            adapter.set_monitor(false);
            assert_eq!(next_event(&mut adapter), MonitorEvent::Off);
            assert!(lock(&adapter.tap).is_none());
        }

        /// The monitor's IOProc leaves an output's microphone off (a headset or
        /// AirPods is one device with both). VectorBridge RX has input streams
        /// to try it on; it plays silence there.
        #[test]
        #[ignore]
        fn monitor_ioproc_leaves_input_streams_off() {
            let dev = device_for_uid(RX_APP_UID).expect("driver installed");
            assert!(channels(dev, SCOPE_INPUT) > 0);
            let running =
                Running::start(dev, Side::Monitor(Arc::new(Mutex::new(None))), "test").unwrap();
            let flags = input_stream_usage(dev, running.id, false).expect("input streams");
            assert!(
                !flags.is_empty() && flags.iter().all(|&f| f == 0),
                "{flags:?}"
            );
            // Control: any other IOProc has them on, as Core Audio starts it.
            let rx = Arc::new(Mutex::new(RxFifo::with_capacity()));
            let other = Running::start(dev, Side::Output(rx), "control").unwrap();
            let flags = input_stream_usage(dev, other.id, false).expect("input streams");
            assert!(flags.iter().all(|&f| f != 0), "control: {flags:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rx_goes_to_both_channels_and_runs_dry_into_silence() {
        let mut f = RxFifo::default();
        f.push(&[16384, -32768]);
        let mut out = [9.0f32; 6];
        assert_eq!(f.fill(&mut out, 2), 2);
        assert_eq!(out, [0.5, 0.5, -1.0, -1.0, 0.0, 0.0]);
        assert_eq!(f.len(), 0);
    }

    #[test]
    fn tx_takes_the_left_channel_and_clamps() {
        let mut f = TxFifo::default();
        f.capture(&[0.5, -0.9, 2.0, 0.0, -1.0, 0.3], 2);
        let mut out = [0i16; 4];
        assert_eq!(f.drain_into(&mut out), 3);
        assert_eq!(&out[..3], &[16384, i16::MAX, i16::MIN]);
    }

    #[test]
    fn fifos_keep_at_most_one_second() {
        let mut rx = RxFifo::default();
        rx.push(&vec![1; FIFO_CAP + 10]);
        assert_eq!(rx.len(), FIFO_CAP);
        let mut tx = TxFifo::default();
        tx.capture(&vec![0.0; 2 * (FIFO_CAP + 10)], 2);
        let mut out = vec![0i16; 2 * FIFO_CAP];
        assert_eq!(tx.drain_into(&mut out), FIFO_CAP);
    }

    /// The FIFO level, not the wall clock, sets the pace: what the device
    /// pulled is what the core is asked for, and a stall is not paid back.
    #[test]
    fn rx_demand_tops_up_to_the_target() {
        let target = rx_target(512);
        assert_eq!(target, 1504);
        let mut f = RxFifo::default();
        assert_eq!(rx_demand_for(f.len(), target), target);
        f.push(&vec![0; target]);
        let mut out = vec![0.0f32; 2 * 512];
        f.fill(&mut out, 2);
        assert_eq!(rx_demand_for(f.len(), target), 512);
        assert_eq!(rx_demand_for(target + 100, target), 0);
        assert_eq!(rx_demand_for(0, 100_000), MAX_CATCH_UP_FRAMES);
    }

    fn tone_48k(n: usize, start: usize, hz: f64) -> Vec<i16> {
        (start..start + n)
            .map(|k| {
                let t = k as f64 / SAMPLE_RATE as f64;
                (0.5 * 32767.0 * (2.0 * std::f64::consts::PI * hz * t).sin()) as i16
            })
            .collect()
    }

    /// Runs the monitor for `secs`: the RX side pushes 10 ms blocks at 48 kHz,
    /// the output pulls stereo 512-frame buffers at `rate` * (1 + `ppm`).
    /// Returns the left channel heard, the frames that had audio, and the
    /// most the queue ever held.
    fn run_monitor(rate: f64, ppm: f64, secs: usize) -> (Vec<f32>, usize, usize) {
        let mut m = MonitorFifo::new(rate);
        let pulled_per_s = rate * (1.0 + ppm * 1e-6);
        let (mut sent, mut owed, mut had, mut most) = (0usize, 0.0f64, 0usize, 0usize);
        let mut heard = Vec::new();
        let mut buf = vec![0.0f32; 2 * 512];
        for _ in 0..secs * 100 {
            m.push(&tone_48k(SAMPLES_PER_PACKET, sent, 1000.0));
            sent += SAMPLES_PER_PACKET;
            most = most.max(m.q.len());
            owed += pulled_per_s / 100.0;
            while owed >= 512.0 {
                owed -= 512.0;
                had += m.fill(&mut buf, 2);
                for f in buf.chunks(2) {
                    assert_eq!(f[0], f[1]);
                    heard.push(f[0]);
                }
            }
        }
        (heard, had, most)
    }

    #[test]
    fn monitor_keeps_tone_frequency_and_level_at_44k1() {
        let (heard, _, _) = run_monitor(44_100.0, 0.0, 3);
        let tail = &heard[44_100..];
        let ups = tail
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        let hz = ups as f64 * 44_100.0 / tail.len() as f64;
        assert!((hz - 1000.0).abs() < 2.0, "{hz} Hz");
        let rms =
            (tail.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / tail.len() as f64).sqrt();
        let expected = 0.5 / 2f64.sqrt();
        assert!((rms - expected).abs() < 0.02 * expected, "rms {rms}");
    }

    #[test]
    fn monitor_at_48k_passes_samples_through_unchanged() {
        let mut m = MonitorFifo::new(48_000.0);
        let input = tone_48k(MONITOR_TARGET, 0, 1000.0);
        m.push(&input);
        let mut out = vec![0.0f32; 2 * 100];
        assert_eq!(m.fill(&mut out, 2), 100);
        for (k, f) in out.chunks(2).enumerate() {
            assert_eq!(f[0], to_f32(input[k]));
        }
    }

    #[test]
    fn monitor_waits_for_its_target_then_runs_dry_into_silence() {
        let mut m = MonitorFifo::new(48_000.0);
        m.push(&vec![16384; MONITOR_TARGET - 1]);
        let mut out = vec![9.0f32; 4];
        assert_eq!(m.fill(&mut out, 1), 0, "not primed yet");
        assert_eq!(out, [0.0; 4]);
        m.push(&[16384]);
        let mut out = vec![9.0f32; MONITOR_TARGET + 10];
        let had = m.fill(&mut out, 1);
        assert!(had < MONITOR_TARGET && had + 2 >= MONITOR_TARGET, "{had}");
        assert!(out[..had].iter().all(|&x| x == 0.5));
        assert!(out[had..].iter().all(|&x| x == 0.0));
    }

    /// The output's clock is not the RX device's: either way the queue stays
    /// bounded and nearly all of the time is heard.
    #[test]
    fn monitor_queue_stays_bounded_with_output_drift() {
        for ppm in [-500.0, 500.0] {
            let (heard, had, most) = run_monitor(44_100.0, ppm, 300);
            assert!(most <= MONITOR_HIGH, "{ppm} ppm: queue reached {most}");
            let gap = 1.0 - had as f64 / heard.len() as f64;
            assert!(gap < 0.002, "{ppm} ppm: {gap} of the time silent");
        }
    }

    #[test]
    fn monitor_fills_every_buffer_of_a_non_interleaved_device() {
        let left = [0.25f32, 0.5, -1.0];
        let mut right = [9.0f32; 3];
        copy_first_channel(&left, 1, &mut right, 1);
        assert_eq!(right, left);
        let stereo = [0.1f32, 0.1, 0.2, 0.2];
        let mut quad = [9.0f32; 8];
        copy_first_channel(&stereo, 2, &mut quad, 4);
        assert_eq!(quad, [0.1, 0.1, 0.1, 0.1, 0.2, 0.2, 0.2, 0.2]);
    }

    fn dev(name: &str, uid: &str) -> DeviceId {
        DeviceId {
            name: name.into(),
            uid: uid.into(),
        }
    }

    #[test]
    fn monitor_refuses_a_vectorbridge_output_by_name_or_uid() {
        let speakers = dev("MacBook Pro Speakers", "BuiltInSpeakerDevice");
        assert_eq!(monitor_refused(&speakers, &[]), None);
        let why = monitor_refused(&dev("VectorBridge TX", "VectorBridgeTX_UID"), &[]).unwrap();
        assert!(
            why.starts_with("VectorBridge TX is a VectorBridge device"),
            "{why}"
        );
        assert!(monitor_refused(&dev("Cable Out", "VectorBridgeRX_UID"), &[]).is_some());
        assert!(monitor_refused(&dev("vectorbridge rx", "x"), &[]).is_some());
        // Unnamed, it cannot be told apart from a VectorBridge device.
        assert!(monitor_refused(&dev("", ""), &[]).is_some());
        assert_eq!(monitor_refused(&dev("", "BuiltInSpeakerDevice"), &[]), None);
    }

    /// A Multi-Output or Aggregate device is refused when anything inside it
    /// is VectorBridge, or cannot be named; plain parts are fine.
    #[test]
    fn monitor_refuses_a_combined_output_that_includes_vectorbridge() {
        let multi = dev("Multi-Output Device", "~:AMS2_StackedOutput:0");
        let speakers = dev("MacBook Pro Speakers", "BuiltInSpeakerDevice");
        let tx = dev("", "VectorBridgeTX_UID");
        assert_eq!(
            monitor_refused(&multi, std::slice::from_ref(&speakers)),
            None
        );
        let why = monitor_refused(&multi, &[speakers.clone(), tx]).unwrap();
        assert_eq!(
            why,
            "Multi-Output Device includes a VectorBridge device; choose speakers or headphones as the sound output"
        );
        let why = monitor_refused(&multi, &[speakers, dev("", "")]).unwrap();
        assert!(why.contains("no name or UID"), "{why}");
    }

    /// A buffer that ends with the queue down to its last sample stops
    /// playing there, and the next waits for the target instead of guessing.
    #[test]
    fn monitor_that_runs_dry_at_a_buffer_end_waits_for_the_target() {
        // Two input frames per output frame: 3840 queued last 1920 frames.
        let mut m = MonitorFifo::new(24_000.0);
        m.push(&vec![16384; MONITOR_TARGET]);
        let mut out = vec![9.0f32; MONITOR_TARGET / 2];
        assert_eq!(m.fill(&mut out, 1), MONITOR_TARGET / 2);
        assert!(!m.playing, "dry at the buffer's end");
        m.push(&[16384; 10]);
        let mut out = vec![9.0f32; 4];
        assert_eq!(m.fill(&mut out, 1), 0, "waits for the target");
        assert_eq!(out, [0.0; 4]);
        assert_eq!(m.pulled, (MONITOR_TARGET / 2 + 4) as u64);
    }

    #[test]
    fn doctor_rows_name_the_fix() {
        let all = Installed {
            rx_app: true,
            rx_core: true,
            tx_app: true,
            tx_core: true,
        };
        let d = driver_check(all);
        assert_eq!((d.glyph, d.word), (Glyph::Ok, "Good"));
        let d = driver_check(Installed {
            tx_core: false,
            ..all
        });
        assert_eq!(d.word, "Fix");
        assert_eq!(
            d.detail,
            "VectorBridge TX missing: install the driver with install-mac.sh"
        );
        let none = Installed {
            rx_app: false,
            rx_core: false,
            tx_app: false,
            tx_core: false,
        };
        assert!(driver_check(none)
            .detail
            .starts_with("VectorBridge RX and VectorBridge TX missing"));

        let term = terminal_app(Some("iTerm.app"));
        assert_eq!(term, "iTerm2");
        assert_eq!(terminal_app(Some("Apple_Terminal")), "Terminal");
        assert_eq!(terminal_app(None), "your terminal app");
        let p = permissions_check(Mic::Authorized, &term);
        assert_eq!(
            (p.word, p.detail.as_str()),
            ("Good", "Microphone allowed for iTerm2")
        );
        let p = permissions_check(Mic::from_status(2), &term);
        assert_eq!(p.word, "Fix");
        assert!(p
            .detail
            .contains("System Settings > Privacy & Security > Microphone"));
        assert!(p.detail.contains("restart iTerm2"));
        assert_eq!(permissions_check(Mic::from_status(0), &term).word, "Check");
        assert!(Mic::Restricted.blocks_tx() && !Mic::NotDetermined.blocks_tx());
    }

    #[test]
    fn the_first_run_screen_shows_once_and_names_the_terminal() {
        assert!(needs_mic_screen(Mic::NotDetermined, None));
        for mic in [Mic::Authorized, Mic::Denied, Mic::Restricted, Mic::Unknown] {
            assert!(!needs_mic_screen(mic, None), "{mic:?}");
        }
        assert!(!needs_mic_screen(Mic::NotDetermined, Some("null")));

        let terminal = mic_screen("Terminal", Some("Apple_Terminal"));
        let term2 = mic_screen("iTerm2", Some("iTerm.app"));
        // An unknown TERM_PROGRAM is shown as given; leave room for a long one.
        let vscode = mic_screen("Visual Studio Code Insiders", Some("vscode"));
        assert!(terminal
            .iter()
            .any(|l| l.contains("whether Terminal may use the microphone")));
        assert!(terminal
            .iter()
            .any(|l| l.contains("Listen Only still works")));
        assert!(!terminal.iter().any(|l| l.contains("local")));
        for screen in [&term2, &vscode] {
            assert!(screen.iter().any(|l| l.contains("local")));
        }
        assert!(term2.iter().any(|l| l.contains("restart iTerm2")));
        // 80 x 24 with the "Enter: continue" line below it; words, no colour.
        for screen in [&terminal, &term2, &vscode] {
            assert!(screen.len() + 2 <= 24);
            for l in screen.iter() {
                assert!(l.chars().count() <= 80, "{l}");
                assert!(!l.contains('\x1b'));
            }
        }
    }
}
