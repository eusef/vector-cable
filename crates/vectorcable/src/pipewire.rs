//! Linux: `VectorBridge RX` (a source the apps record from) and
//! `VectorBridge TX` (a sink the apps play into) made at run time with
//! `pactl`, moved through `pacat` and `parec` pipes (pipewire-pulse). No C
//! dependency, so the cross `cargo check` from the Mac passes.
//!
//! Construction: the RX path needs somewhere to write and the apps need a
//! source to read, so it is a null sink `vectorbridge_rx_sink` that `pacat`
//! plays into, plus a remap source `vectorbridge_rx` on that sink's monitor,
//! which is what shows as "VectorBridge RX". TX is a null sink
//! `vectorbridge_tx` ("VectorBridge TX") whose monitor `parec` records.
//!
//! A helper that exits or a pipe that breaks is reported through
//! [`AudioAdapter::fault`], shown on the Check tab and by `doctor`.
//!
//! Runtime verified on Ubuntu 26.04 (PipeWire 1.6.2, 2026-10-28) in Listen
//! Only: both devices made and removed, RX audio recorded from the source.
//! Ubuntu's PipeWire ships without the three helpers; they come in
//! `pulseaudio-utils`.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::VecDeque;
use std::io::{BufRead, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use vbcore::audio::{AudioAdapter, MAX_CATCH_UP_FRAMES};
use vbcore::protocol::SAMPLE_RATE;
use vbcore::time::LocalTime;

pub const TX_SINK: &str = "vectorbridge_tx";
const RX_SINK: &str = "vectorbridge_rx_sink";
pub const RX_SOURCE: &str = "vectorbridge_rx";

/// Which cable devices the `pactl list short` output names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Present {
    pub tx: bool,
    pub rx: bool,
}

impl Present {
    /// From the `pactl list short sinks` and `... sources` listings
    /// (tab-separated, name in the second column).
    pub fn parse(sinks: &str, sources: &str) -> Present {
        let named =
            |listing: &str, name: &str| listing.lines().any(|l| l.split('\t').nth(1) == Some(name));
        Present {
            tx: named(sinks, TX_SINK),
            rx: named(sources, RX_SOURCE),
        }
    }

    /// Why the devices are not usable, if they are not.
    pub fn fault(self) -> Option<String> {
        match (self.tx, self.rx) {
            (true, true) => None,
            (false, false) => Some("VectorBridge RX and VectorBridge TX missing (run up)".into()),
            (true, false) => Some("VectorBridge RX missing (run up)".into()),
            (false, true) => Some("VectorBridge TX missing (run up)".into()),
        }
    }
}

/// The error for a helper that could not start; a missing one names the
/// package that has all three.
fn helper_error(name: &str, e: std::io::Error) -> std::io::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        std::io::Error::other(format!(
            "{name} not found: install pulseaudio-utils, which has pactl, pacat and parec"
        ))
    } else {
        std::io::Error::other(format!("{name}: {e}"))
    }
}

/// Why no device can be made at all: `pactl` is not installed.
pub fn tools_fault() -> Option<String> {
    match Command::new("pactl").arg("--version").output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Some(helper_error("pactl", e).to_string())
        }
        _ => None,
    }
}

/// What `pactl` lists now; nothing listed when `pactl` cannot run.
pub fn present_now() -> Present {
    let list = |kind: &str| {
        Command::new("pactl")
            .args(["list", "short", kind])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    Present::parse(&list("sinks"), &list("sources"))
}

type Shared<T> = Arc<Mutex<T>>;

/// One helper process with its captured stderr.
struct Helper {
    name: &'static str,
    child: Child,
    stderr: Shared<String>,
}

impl Helper {
    fn spawn(
        name: &'static str,
        args: &[String],
        stdin: Stdio,
        stdout: Stdio,
    ) -> std::io::Result<Helper> {
        let mut child = Command::new(name)
            .args(args)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| helper_error(name, e))?;
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let sink = stderr.clone();
            std::thread::Builder::new()
                .name(format!("vectorcable-pw-{name}-err"))
                .spawn(move || {
                    let mut text = String::new();
                    let _ = pipe.read_to_string(&mut text);
                    *sink.lock().unwrap() = text;
                })?;
        }
        Ok(Helper {
            name,
            child,
            stderr,
        })
    }

    /// Why the helper is not running, if it is not.
    fn fault(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let err = self.stderr.lock().unwrap();
                let why = err.trim().lines().last().unwrap_or("").to_string();
                Some(if why.is_empty() {
                    format!("{} exited ({status})", self.name)
                } else {
                    format!("{} exited: {why}", self.name)
                })
            }
            Ok(None) => None,
            Err(e) => Some(format!("{}: {e}", self.name)),
        }
    }
}

pub struct PipeWireAdapter {
    modules: Vec<String>,
    play: Shared<Helper>,
    rec: Shared<Helper>,
    rx_writer: Sender<Vec<i16>>,
    tx_captured: Shared<VecDeque<i16>>,
    /// Set by the pipe threads when a pipe breaks.
    pipe_fault: Shared<Option<String>>,
    started: Option<LocalTime>,
    rx_consumed: u64,
}

fn load_module(args: &[&str]) -> std::io::Result<String> {
    let out = Command::new("pactl")
        .arg("load-module")
        .args(args)
        .output()
        .map_err(|e| helper_error("pactl", e))?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "pactl load-module {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn load_modules() -> std::io::Result<Vec<String>> {
    let mut modules = Vec::new();
    let steps: [Vec<String>; 3] = [
        vec![
            "module-null-sink".into(),
            format!("sink_name={TX_SINK}"),
            "sink_properties=device.description=VectorBridge\\ TX".into(),
            "channels=1".into(),
            "rate=48000".into(),
        ],
        vec![
            "module-null-sink".into(),
            format!("sink_name={RX_SINK}"),
            "sink_properties=device.description=VectorBridge\\ RX\\ (internal)".into(),
            "channels=1".into(),
            "rate=48000".into(),
        ],
        vec![
            "module-remap-source".into(),
            format!("master={RX_SINK}.monitor"),
            format!("source_name={RX_SOURCE}"),
            "source_properties=device.description=VectorBridge\\ RX".into(),
            "channels=1".into(),
        ],
    ];
    for args in &steps {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match load_module(&args) {
            Ok(id) => modules.push(id),
            Err(e) => {
                unload(&modules);
                return Err(e);
            }
        }
    }
    Ok(modules)
}

impl PipeWireAdapter {
    pub fn new() -> std::io::Result<PipeWireAdapter> {
        let modules = load_modules()?;
        let common = [
            "--raw",
            "--rate=48000",
            "--channels=1",
            "--format=s16le",
            "--latency-msec=20",
        ];
        let mut play_args: Vec<String> = vec!["--playback".into(), format!("--device={RX_SINK}")];
        play_args.extend(common.iter().map(|s| s.to_string()));
        let mut play = match Helper::spawn("pacat", &play_args, Stdio::piped(), Stdio::null()) {
            Ok(p) => p,
            Err(e) => {
                unload(&modules);
                return Err(e);
            }
        };
        let mut rec_args: Vec<String> = vec![format!("--device={TX_SINK}.monitor")];
        rec_args.extend(common.iter().map(|s| s.to_string()));
        let mut rec = match Helper::spawn("parec", &rec_args, Stdio::null(), Stdio::piped()) {
            Ok(r) => r,
            Err(e) => {
                let _ = play.child.kill();
                unload(&modules);
                return Err(e);
            }
        };
        let pipe_fault: Shared<Option<String>> = Arc::new(Mutex::new(None));
        let (rx_writer, rx_queue) = mpsc::channel::<Vec<i16>>();
        let mut spawned: Vec<std::io::Result<()>> = Vec::new();
        if let Some(mut stdin) = play.child.stdin.take() {
            let fault = pipe_fault.clone();
            let r = std::thread::Builder::new()
                .name("vectorcable-pw-play".into())
                .spawn(move || {
                    for block in rx_queue {
                        let bytes: Vec<u8> = block.iter().flat_map(|s| s.to_le_bytes()).collect();
                        if let Err(e) = stdin.write_all(&bytes) {
                            *fault.lock().unwrap() = Some(format!("pacat pipe broke: {e}"));
                            return;
                        }
                    }
                })
                .map(|_| ());
            spawned.push(r);
        }
        let tx_captured = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stdout) = rec.child.stdout.take() {
            let queue = tx_captured.clone();
            let fault = pipe_fault.clone();
            let r = std::thread::Builder::new()
                .name("vectorcable-pw-rec".into())
                .spawn(move || {
                    let mut reader = std::io::BufReader::new(stdout);
                    loop {
                        let chunk = match reader.fill_buf() {
                            Ok(c) if !c.is_empty() => c.to_vec(),
                            Ok(_) => {
                                *fault.lock().unwrap() =
                                    Some("parec stopped sending TX audio".into());
                                return;
                            }
                            Err(e) => {
                                *fault.lock().unwrap() = Some(format!("parec pipe broke: {e}"));
                                return;
                            }
                        };
                        let n = chunk.len() - chunk.len() % 2;
                        reader.consume(n);
                        let mut q = queue.lock().unwrap();
                        for pair in chunk[..n].chunks_exact(2) {
                            q.push_back(i16::from_le_bytes([pair[0], pair[1]]));
                        }
                        // A reader that stalled drops the oldest second, as a device would.
                        let cap = SAMPLE_RATE as usize;
                        while q.len() > cap {
                            q.pop_front();
                        }
                    }
                })
                .map(|_| ());
            spawned.push(r);
        }
        if let Some(Err(e)) = spawned.into_iter().find(|r| r.is_err()) {
            let _ = play.child.kill();
            let _ = play.child.wait();
            let _ = rec.child.kill();
            let _ = rec.child.wait();
            unload(&modules);
            return Err(e);
        }
        Ok(PipeWireAdapter {
            modules,
            play: Arc::new(Mutex::new(play)),
            rec: Arc::new(Mutex::new(rec)),
            rx_writer,
            tx_captured,
            pipe_fault,
            started: None,
            rx_consumed: 0,
        })
    }

    fn device_frames(&mut self, now: LocalTime) -> u64 {
        let start = *self.started.get_or_insert(now);
        ((now - start).max(0.0) * SAMPLE_RATE as f64).round() as u64
    }
}

fn unload(modules: &[String]) {
    for id in modules.iter().rev() {
        let _ = Command::new("pactl")
            .args(["unload-module", id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl AudioAdapter for PipeWireAdapter {
    fn name(&self) -> &str {
        "pipewire"
    }

    /// A helper that exited comes first (it says why); then a broken pipe.
    fn fault(&self) -> Option<String> {
        if let Some(f) = self.play.lock().unwrap().fault() {
            return Some(f);
        }
        if let Some(f) = self.rec.lock().unwrap().fault() {
            return Some(f);
        }
        self.pipe_fault.lock().unwrap().clone()
    }

    /// The playback pipe takes frames at the device rate; the wall clock
    /// paces what is rendered into it, and the pipe buffer absorbs the rest.
    fn rx_demand(&mut self, now: LocalTime) -> usize {
        let frames = self.device_frames(now);
        let due = frames.saturating_sub(self.rx_consumed) as usize;
        self.rx_consumed = frames;
        due.min(MAX_CATCH_UP_FRAMES)
    }

    fn read_tx(&mut self, _now: LocalTime, out: &mut [i16]) -> usize {
        let mut q = self.tx_captured.lock().unwrap();
        let n = out.len().min(q.len());
        for o in out[..n].iter_mut() {
            *o = q.pop_front().unwrap_or(0);
        }
        n
    }

    fn write_rx(&mut self, samples: &[i16]) {
        let _ = self.rx_writer.send(samples.to_vec());
    }
}

impl Drop for PipeWireAdapter {
    fn drop(&mut self) {
        for helper in [&self.play, &self.rec] {
            let mut h = helper.lock().unwrap();
            let _ = h.child.kill();
            let _ = h.child.wait();
        }
        unload(&self.modules);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_parses_the_pactl_listings_and_names_what_is_missing() {
        let sinks = "0\talsa_output.pci\tmodule-alsa-card.c\ts32le 2ch 48000Hz\tRUNNING\n\
                     51\tvectorbridge_tx\tmodule-null-sink.c\ts16le 1ch 48000Hz\tIDLE\n";
        let sources = "1\tvectorbridge_rx\tmodule-remap-source.c\ts16le 1ch 48000Hz\tIDLE\n";
        assert_eq!(
            Present::parse(sinks, sources),
            Present { tx: true, rx: true }
        );
        assert_eq!(Present::parse(sinks, sources).fault(), None);
        assert_eq!(
            Present::parse("", "").fault().as_deref(),
            Some("VectorBridge RX and VectorBridge TX missing (run up)")
        );
        assert_eq!(
            Present::parse(sinks, "").fault().as_deref(),
            Some("VectorBridge RX missing (run up)")
        );
        assert_eq!(
            Present::parse("", sources).fault().as_deref(),
            Some("VectorBridge TX missing (run up)")
        );
        // The name must be the whole second column, not a substring.
        assert!(!Present::parse("9\tvectorbridge_tx_old\tx\n", "").tx);
    }

    #[test]
    fn a_missing_helper_names_the_package() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(
            helper_error("pacat", missing).to_string(),
            "pacat not found: install pulseaudio-utils, which has pactl, pacat and parec"
        );
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(helper_error("parec", denied)
            .to_string()
            .starts_with("parec: "));
    }
}
