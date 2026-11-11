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

use vbcore::audio::{AudioAdapter, MonitorEvent, MonitorWorker, MAX_CATCH_UP_FRAMES};
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
    /// Starts and stops the radio monitor off the RX loop; `None` when its
    /// thread could not start (then there is no monitor).
    monitor: Option<MonitorWorker>,
    /// Where `write_rx` sends a copy of the RX audio while the monitor is on.
    monitor_tap: MonitorTap,
}

/// The monitor's feed: `None` while it is off. `write_rx` never waits on it.
type MonitorTap = Shared<Option<mpsc::SyncSender<Vec<i16>>>>;

/// RX blocks (about 10 ms each) queued for the monitor before newer ones are
/// dropped: the output's clock runs slow against ours, so what you hear has a
/// short gap now and then, and the delay stays bounded. The cable is not
/// touched.
const MONITOR_QUEUE_BLOCKS: usize = 4;
/// The monitor pipe's size in bytes (about 43 ms of audio), so a stalled
/// output cannot build up seconds of delay in the pipe.
const MONITOR_PIPE_BYTES: i32 = 4096;

/// The radio monitor's stream: `pacat` playing the RX audio on `sink`, the
/// default output when `m` was pressed. It is a stream of its own, never
/// linked to the RX cable's sink, so the output's clock cannot pace the
/// cable. It may not move, reconnect or fall back to another output, so a
/// later default change, or the output going away, cannot turn it into a
/// loop into VectorBridge TX.
pub fn monitor_args(sink: &str) -> Vec<String> {
    vec![
        "--playback".into(),
        format!("--device={sink}"),
        "--raw".into(),
        "--rate=48000".into(),
        "--channels=1".into(),
        "--format=s16le".into(),
        "--latency-msec=60".into(),
        "--client-name=VectorCable".into(),
        "--stream-name=VectorCable radio monitor".into(),
        "--property=node.dont-move=true".into(),
        "--property=node.dont-reconnect=true".into(),
        "--property=node.dont-fallback=true".into(),
    ]
}

/// One node of the PipeWire graph, as the monitor's check sees it.
struct GraphNode {
    id: u64,
    serial: Option<String>,
    name: String,
    description: Option<String>,
    link_group: Option<String>,
    /// The `target.object` (or older `node.target`) a stream asks for.
    targets: Vec<String>,
}

/// A property as text, whether `pw-dump` gives it as a string or a number.
fn prop_text(props: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    match props.get(key)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Whether `sink` may carry the monitor: the display name to show, or why
/// not. `dump` is `pw-dump` output, the whole PipeWire graph. Everything the
/// sink feeds is followed: its links (from its monitor too), the other nodes
/// of its link group (a combined sink's member streams, a loopback's other
/// half, a filter's output) and any stream's target. Refused when any of it
/// is a VectorBridge device, since the monitor would then reach VectorBridge
/// TX; also when the sink is not in the graph, since then it cannot be
/// checked.
pub fn check_monitor_output(dump: &str, sink: &str) -> Result<String, String> {
    let objects: Vec<serde_json::Value> =
        serde_json::from_str(dump).map_err(|e| format!("cannot read the PipeWire graph: {e}"))?;
    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut links: Vec<(u64, u64)> = Vec::new();
    for o in &objects {
        let (Some(kind), Some(id)) = (o["type"].as_str(), o["id"].as_u64()) else {
            continue;
        };
        let info = &o["info"];
        match kind {
            "PipeWire:Interface:Node" => {
                let Some(props) = info["props"].as_object() else {
                    continue;
                };
                nodes.push(GraphNode {
                    id,
                    serial: prop_text(props, "object.serial"),
                    name: prop_text(props, "node.name").unwrap_or_default(),
                    description: prop_text(props, "node.description")
                        .or_else(|| prop_text(props, "node.nick")),
                    link_group: prop_text(props, "node.link-group"),
                    targets: ["target.object", "node.target"]
                        .iter()
                        .filter_map(|k| prop_text(props, k))
                        .collect(),
                });
            }
            "PipeWire:Interface:Link" => {
                if let (Some(from), Some(to)) = (
                    info["output-node-id"].as_u64(),
                    info["input-node-id"].as_u64(),
                ) {
                    links.push((from, to));
                }
            }
            _ => {}
        }
    }
    let start = nodes
        .iter()
        .find(|n| n.name == sink)
        .ok_or_else(|| format!("cannot find the sound output {sink} to check it"))?;
    let shown = start
        .description
        .clone()
        .unwrap_or_else(|| sink.to_string());
    for id in [sink, shown.as_str()] {
        if let Some(why) = vbcore::audio::monitor_output_refused(id) {
            return Err(why);
        }
    }
    let mut seen = vec![start.id];
    let mut todo = vec![start.id];
    while let Some(id) = todo.pop() {
        let Some(node) = nodes.iter().find(|n| n.id == id) else {
            continue;
        };
        let mentions = |t: &str| t.to_ascii_lowercase().contains("vectorbridge");
        if id != start.id
            && (mentions(&node.name) || node.description.as_deref().is_some_and(mentions))
        {
            return Err(format!(
                "{shown} includes a VectorBridge device; choose speakers or headphones as the sound output"
            ));
        }
        let mut next: Vec<u64> = links
            .iter()
            .filter(|(from, _)| *from == id)
            .map(|(_, to)| *to)
            .collect();
        if let Some(group) = &node.link_group {
            next.extend(
                nodes
                    .iter()
                    .filter(|n| n.link_group.as_ref() == Some(group))
                    .map(|n| n.id),
            );
        }
        for target in &node.targets {
            next.extend(
                nodes
                    .iter()
                    .filter(|n| &n.name == target || n.serial.as_ref() == Some(target))
                    .map(|n| n.id),
            );
        }
        for n in next {
            if !seen.contains(&n) {
                seen.push(n);
                todo.push(n);
            }
        }
    }
    Ok(shown)
}

/// The PipeWire graph, as JSON.
fn pw_dump() -> Result<String, String> {
    let out = Command::new("pw-dump").output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "pw-dump not found: install PipeWire's tools (Debian and Ubuntu: pipewire-bin) so the sound output can be checked".to_string()
        } else {
            format!("pw-dump: {e}")
        }
    })?;
    if !out.status.success() {
        return Err(format!(
            "pw-dump: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The monitor's `pacat` while it is on; dropping it stops the stream, and
/// the thread feeding it ends with the pipe.
struct MonitorPlayer {
    helper: Helper,
}

impl Drop for MonitorPlayer {
    fn drop(&mut self) {
        let _ = self.helper.child.kill();
        let _ = self.helper.child.wait();
    }
}

/// Keeps the monitor pipe small; if the kernel refuses, the default stands.
#[cfg(target_os = "linux")]
fn shrink_pipe(pipe: &std::process::ChildStdin) {
    use std::os::fd::AsRawFd;
    // SAFETY: F_SETPIPE_SZ on a pipe this process holds open; it only
    // changes the pipe's capacity.
    unsafe {
        libc::fcntl(pipe.as_raw_fd(), libc::F_SETPIPE_SZ, MONITOR_PIPE_BYTES);
    }
}

#[cfg(not(target_os = "linux"))]
fn shrink_pipe(_pipe: &std::process::ChildStdin) {}

/// Starts the monitor on the current default output, after the check: the
/// player and the output's display name. The tap is set last, once the
/// stream has started.
fn start_monitor(tap: &MonitorTap) -> Result<(MonitorPlayer, String), String> {
    let sink = pactl_out(&["get-default-sink"])?.trim().to_string();
    if sink.is_empty() {
        return Err("no default sound output is set".into());
    }
    let name = check_monitor_output(&pw_dump()?, &sink)?;
    let helper = Helper::spawn("pacat", &monitor_args(&sink), Stdio::piped(), Stdio::null())
        .map_err(|e| e.to_string())?;
    let mut player = MonitorPlayer { helper };
    let stdin = player
        .helper
        .child
        .stdin
        .take()
        .ok_or("pacat has no input pipe")?;
    shrink_pipe(&stdin);
    // A stream that cannot open its output (it may not fall back) exits.
    std::thread::sleep(std::time::Duration::from_millis(200));
    if let Some(why) = player.helper.fault() {
        return Err(why);
    }
    let (feed, queue) = mpsc::sync_channel::<Vec<i16>>(MONITOR_QUEUE_BLOCKS);
    let mut stdin = stdin;
    std::thread::Builder::new()
        .name("vectorcable-pw-monitor".into())
        .spawn(move || {
            for block in queue {
                let bytes: Vec<u8> = block.iter().flat_map(|s| s.to_le_bytes()).collect();
                if stdin.write_all(&bytes).is_err() {
                    return;
                }
            }
        })
        .map_err(|e| format!("cannot start the radio monitor: {e}"))?;
    *tap.lock().unwrap() = Some(feed);
    Ok((player, name))
}

/// The monitor worker: whatever is playing stops first (the feed, then the
/// stream), so a repeat `on` re-picks the current default output; the last
/// `apply(false)` (on drop) stops it for good.
fn spawn_monitor(tap: MonitorTap) -> Option<MonitorWorker> {
    let mut playing: Option<MonitorPlayer> = None;
    MonitorWorker::spawn(move |on| {
        *tap.lock().unwrap() = None;
        drop(playing.take());
        if !on {
            return MonitorEvent::Off;
        }
        match start_monitor(&tap) {
            Ok((player, name)) => {
                playing = Some(player);
                MonitorEvent::On(name)
            }
            Err(why) => MonitorEvent::Refused(why),
        }
    })
    .ok()
}

/// `pactl` stdout, in English field names whatever the locale.
fn pactl_out(args: &[&str]) -> Result<String, String> {
    let out = Command::new("pactl")
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .map_err(|e| helper_error("pactl", e).to_string())?;
    if !out.status.success() {
        return Err(format!(
            "pactl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
        let monitor_tap: MonitorTap = Arc::new(Mutex::new(None));
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
            monitor: spawn_monitor(monitor_tap.clone()),
            monitor_tap,
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
        // A full queue drops the block: a gap in the monitor, never in the cable.
        if let Some(feed) = self.monitor_tap.lock().unwrap().as_ref() {
            let _ = feed.try_send(samples.to_vec());
        }
    }

    fn monitor_supported(&self) -> bool {
        self.monitor.is_some()
    }

    /// `pactl` runs on the monitor's own thread, never on the RX loop.
    fn set_monitor(&mut self, on: bool) {
        if let Some(m) = &self.monitor {
            m.request(on);
        }
    }

    fn monitor_event(&mut self) -> Option<MonitorEvent> {
        self.monitor.as_ref().and_then(MonitorWorker::event)
    }
}

impl Drop for PipeWireAdapter {
    fn drop(&mut self) {
        // The monitor stops first: dropping the worker stops its stream and
        // waits.
        drop(self.monitor.take());
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

    #[test]
    fn the_monitor_is_its_own_stream_that_never_moves_or_falls_back() {
        let args = monitor_args("alsa_output.pci-0000_00_1b.0.analog-stereo");
        assert_eq!(args[0], "--playback");
        assert!(args.contains(&"--device=alsa_output.pci-0000_00_1b.0.analog-stereo".to_string()));
        for p in ["dont-move", "dont-reconnect", "dont-fallback"] {
            assert!(args.contains(&format!("--property=node.{p}=true")), "{p}");
        }
        // Never a module linking the RX cable's sink to the output.
        assert!(!args.iter().any(|a| a.contains(RX_SINK)));
    }

    /// A `pw-dump` shaped graph: the built-in output; VectorBridge TX; a
    /// combined sink whose member streams (one linked, one only targeted) go
    /// to the built-in output and to TX; headphones whose monitor a loopback
    /// plays into TX; and a node with no info.
    const GRAPH: &str = r#"[
        {"id": 5, "type": "PipeWire:Interface:Node", "info": null},
        {"id": 57, "type": "PipeWire:Interface:Node", "info": {"props": {
            "node.name": "alsa_output.pci-0000_00_1b.0.analog-stereo",
            "node.description": "Built-in Audio Analog Stereo", "object.serial": 57}}},
        {"id": 82, "type": "PipeWire:Interface:Node", "info": {"props": {
            "node.name": "vectorbridge_tx", "node.description": "VectorBridge TX",
            "object.serial": 340}}},
        {"id": 91, "type": "PipeWire:Interface:Node", "info": {"props": {
            "node.name": "combined", "node.description": "Simultaneous output",
            "node.link-group": "combine-sink-1-13"}}},
        {"id": 77, "type": "PipeWire:Interface:Node", "info": {"props": {
            "node.name": "output.combined_alsa", "node.link-group": "combine-sink-1-13",
            "target.object": "alsa_output.pci-0000_00_1b.0.analog-stereo"}}},
        {"id": 69, "type": "PipeWire:Interface:Node", "info": {"props": {
            "node.name": "output.combined_2", "node.link-group": "combine-sink-1-13",
            "target.object": "340"}}},
        {"id": 100, "type": "PipeWire:Interface:Link",
            "info": {"output-node-id": 77, "input-node-id": 57}},
        {"id": 60, "type": "PipeWire:Interface:Node", "info": {"props": {
            "node.name": "bluez_output.headphones", "node.nick": "Headphones"}}},
        {"id": 61, "type": "PipeWire:Interface:Node", "info": {"props": {
            "node.name": "input.loopback-1", "node.link-group": "loopback-1-2"}}},
        {"id": 62, "type": "PipeWire:Interface:Node", "info": {"props": {
            "node.name": "output.loopback-1", "node.link-group": "loopback-1-2"}}},
        {"id": 102, "type": "PipeWire:Interface:Link",
            "info": {"output-node-id": 60, "input-node-id": 61}},
        {"id": 103, "type": "PipeWire:Interface:Link",
            "info": {"output-node-id": 62, "input-node-id": 82}}
    ]"#;

    #[test]
    fn the_monitor_plays_on_an_ordinary_output_and_shows_its_description() {
        assert_eq!(
            check_monitor_output(GRAPH, "alsa_output.pci-0000_00_1b.0.analog-stereo"),
            Ok("Built-in Audio Analog Stereo".into())
        );
    }

    #[test]
    fn the_monitor_refuses_any_output_that_reaches_vectorbridge() {
        let why = check_monitor_output(GRAPH, "vectorbridge_tx").unwrap_err();
        assert!(why.contains("is a VectorBridge device"), "{why}");
        // A combined sink lists no members; its link group and their
        // targets lead to TX.
        let why = check_monitor_output(GRAPH, "combined").unwrap_err();
        assert!(
            why.starts_with("Simultaneous output includes a VectorBridge device"),
            "{why}"
        );
        // A loopback from the headphones' monitor plays into TX.
        let why = check_monitor_output(GRAPH, "bluez_output.headphones").unwrap_err();
        assert!(
            why.starts_with("Headphones includes a VectorBridge device"),
            "{why}"
        );
        // Not in the graph, or no graph: it cannot be checked, so it is refused.
        assert!(check_monitor_output(GRAPH, "missing").is_err());
        assert!(check_monitor_output("", "alsa_output.x").is_err());
    }

    /// The `pw-dump` node named `name`, if any.
    #[cfg(target_os = "linux")]
    fn live_node(name_key: &str, name: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
        let dump: Vec<serde_json::Value> = serde_json::from_str(&pw_dump().unwrap()).unwrap();
        dump.into_iter()
            .filter(|o| o["type"] == "PipeWire:Interface:Node")
            .filter_map(|o| o["info"]["props"].as_object().cloned())
            .find(|p| p.get(name_key).and_then(|v| v.as_str()) == Some(name))
    }

    /// Live, on this machine's PipeWire: makes the cables, turns the monitor
    /// on and off, and checks its stream comes and goes, may not move, and
    /// leaves the RX sink's clock alone, while the calls on the RX loop's side
    /// never wait. Plays only silence.
    /// `cargo test -p vectorcable pipewire -- --ignored`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn the_monitor_plays_its_own_stream_live() {
        use std::time::{Duration, Instant};
        const STREAM: &str = "VectorCable radio monitor";
        let streams = || {
            let dump: Vec<serde_json::Value> = serde_json::from_str(&pw_dump().unwrap()).unwrap();
            dump.iter()
                .filter(|o| o["type"] == "PipeWire:Interface:Node")
                .filter(|o| o["info"]["props"]["media.name"] == STREAM)
                .count()
        };
        // The RX sink's id, and the driver of a stream into it (the cable's
        // own `pacat`): the same while the RX sink keeps its own clock.
        let rx_clock = || {
            let id = |p: serde_json::Map<String, serde_json::Value>, k: &str| {
                p.get(k).and_then(|v| v.as_u64())
            };
            let sink = live_node("node.name", RX_SINK).and_then(|p| id(p, "object.id"));
            let stream = live_node("target.object", RX_SINK).and_then(|p| id(p, "node.driver-id"));
            (sink, stream)
        };
        // Asks, then polls as the runner does, timing the RX-side calls.
        let ask = |a: &mut PipeWireAdapter, on: bool| {
            let t = Instant::now();
            a.set_monitor(on);
            let mut slowest = t.elapsed();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let t = Instant::now();
                a.write_rx(&[0; 480]);
                let e = a.monitor_event();
                slowest = slowest.max(t.elapsed());
                if let Some(e) = e {
                    assert!(
                        slowest < Duration::from_millis(2),
                        "RX side waited {slowest:?}"
                    );
                    return e;
                }
                assert!(Instant::now() < deadline, "no monitor event");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let before = streams();
        let mut a = PipeWireAdapter::new().expect("cables made");
        assert!(a.monitor_supported());
        a.write_rx(&vec![0; 48_000 * 6]);
        std::thread::sleep(Duration::from_millis(500));
        let (rx_sink, rx_driver) = rx_clock();
        assert!(
            rx_sink.is_some() && rx_driver == rx_sink,
            "the RX sink drives its own stream: {:?}",
            rx_clock()
        );
        let MonitorEvent::On(name) = ask(&mut a, true) else {
            panic!("monitor did not start")
        };
        let sink = pactl_out(&["get-default-sink"]).unwrap().trim().to_string();
        std::thread::sleep(Duration::from_millis(500));
        let output = live_node("node.name", &sink).and_then(|p| p.get("object.id")?.as_u64());
        eprintln!(
            "monitor on: {name}; RX (sink, stream driver) {:?}; output {sink} id {output:?}",
            rx_clock()
        );
        assert_eq!(streams(), before + 1);
        // The cable keeps its own clock: the RX stream still follows the RX
        // sink, not the output.
        assert_eq!(
            rx_clock(),
            (rx_sink, rx_driver),
            "the monitor re-clocked RX"
        );
        assert_ne!(rx_driver, output);
        let stream = live_node("media.name", STREAM).expect("monitor stream");
        for p in [
            "node.dont-move",
            "node.dont-reconnect",
            "node.dont-fallback",
        ] {
            let v = stream.get(p);
            assert!(
                v == Some(&serde_json::Value::Bool(true))
                    || v.and_then(|v| v.as_str()) == Some("true"),
                "{p}: {stream:?}"
            );
        }
        // A repeat on re-picks the output and leaves one stream, not two.
        assert!(matches!(ask(&mut a, true), MonitorEvent::On(_)));
        assert_eq!(streams(), before + 1);
        assert_eq!(ask(&mut a, false), MonitorEvent::Off);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(streams(), before);
        assert!(matches!(ask(&mut a, true), MonitorEvent::On(_)));
        drop(a);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(streams(), before, "drop stops the stream");
    }
}
