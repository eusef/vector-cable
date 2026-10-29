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

    use vbcore::audio::AudioAdapter;
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
    }

    /// What one IOProc reaches: its FIFO and a count of its callbacks.
    struct Context {
        side: Side,
        callbacks: AtomicU64,
    }

    /// Only the first buffer carries audio (the driver has one interleaved
    /// stream); any others are zeroed on output.
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

    pub struct CoreAudioAdapter {
        rx: Arc<Mutex<RxFifo>>,
        tx: Arc<Mutex<TxFifo>>,
        rx_target: usize,
        terminal: String,
        watch: Mutex<Watch>,
        mic: MicWatch,
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
