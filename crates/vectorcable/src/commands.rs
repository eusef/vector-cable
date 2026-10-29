//! `status`, `doctor`, `setup <app>` and `soak`: line output, no UI.
//! Against the live bridge only `monitor`, `status` and HTTP GETs go out
//! (plus a TCP connect to 4532 in `doctor`); `soak` transmits and is never
//! run against the live bridge.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use vbcore::http::{self, HttpOutcome};
use vbcore::protocol::{HelloReply, Reply, Request, CONTROL_PORT};
use vbcore::runner::Snapshot;
use vbcore::session::{Link, Mode};
use vbcore::status::{dash, BridgeStatus, BridgeVersion, LadderState, PeerSlot, RigctlState};

use crate::app;
use crate::cli::Args;
use crate::ui::{self, App, CheckLine, Glyph};

/// One `monitor` (for the protocol number) and one `status` from a throwaway
/// socket; the monitor registration expires on the bridge in 5 s.
pub fn ask_bridge(
    addr: SocketAddr,
    timeout: Duration,
) -> (Option<HelloReply>, Option<BridgeStatus>) {
    let Ok(socket) = UdpSocket::bind(if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }) else {
        return (None, None);
    };
    let _ = socket.set_read_timeout(Some(timeout));
    let mut buf = [0u8; 4096];
    let mut ask = |request: Request| -> Option<Reply> {
        socket.send_to(&request.to_bytes(), addr).ok()?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let Ok((n, from)) = socket.recv_from(&mut buf) else {
                return None;
            };
            if from.ip() != addr.ip() || buf.first() != Some(&b'{') {
                continue;
            }
            if let Ok(r) = Reply::parse(&buf[..n]) {
                return Some(r);
            }
        }
        None
    };
    let hello = ask(Request::Monitor {
        client: "vectorcable".into(),
    })
    .and_then(|r| r.hello());
    let status = ask(Request::Status).and_then(|r| match r {
        Reply::Ok(v) => Some(BridgeStatus::from_value(&v)),
        Reply::Error(_) => None,
    });
    (hello, status)
}

fn fetch_http(host: &str) -> (Option<BridgeVersion>, LadderState, RigctlState) {
    let version = match http::get(host, 80, "/", http::DEFAULT_TIMEOUT) {
        HttpOutcome::Response { status: 200, body } => BridgeVersion::from_home_page(&body),
        _ => None,
    };
    let ladder = LadderState::parse(&http::get(host, 80, "/api/ladder", http::DEFAULT_TIMEOUT));
    let rigctl = RigctlState::parse(&http::get(host, 80, "/api/rigctl", http::DEFAULT_TIMEOUT));
    (version, ladder, rigctl)
}

/// Builds the runner's view of a bridge from one round of questions, so the
/// Check tab's rules and `doctor` are the same code.
fn snapshot_of(
    addr: SocketAddr,
    hello: Option<HelloReply>,
    status: Option<BridgeStatus>,
    version: Option<BridgeVersion>,
    ladder: LadderState,
    rigctl: RigctlState,
) -> Snapshot {
    let link = if status.is_some() || hello.is_some() {
        Link::RxPaused
    } else {
        Link::Searching
    };
    // `doctor` and `status` never hold the slot: whoever holds it is another client.
    let slot_held_by_other = matches!(status_peer(&status), Some(PeerSlot::Held(_)));
    Snapshot {
        bridge: addr,
        local_audio_port: 0,
        mode: Mode::ListenOnly,
        link,
        hello,
        status,
        ladder: Some(ladder),
        rigctl: Some(rigctl),
        version,
        rx: Default::default(),
        playback: Default::default(),
        tx: Default::default(),
        rx_dbfs: None,
        tx_dbfs: None,
        buffer_ms: None,
        trim_ppm: 0.0,
        clock: None,
        streaming: false,
        finished: false,
        listing: None,
        audio_name: audio_adapter_name(),
        audio_fault: None,
        slot_held_by_other,
        rig_relay: None,
        local_ip: None,
    }
}

fn status_peer(status: &Option<BridgeStatus>) -> Option<&PeerSlot> {
    status.as_ref().map(|st| &st.peer)
}

/// The adapter `up` would use on this machine, for the Audio devices row.
fn audio_adapter_name() -> String {
    if cfg!(target_os = "linux") {
        "pipewire".to_string()
    } else if cfg!(target_os = "macos") {
        "coreaudio".to_string()
    } else {
        "null".to_string()
    }
}

pub fn status_lines(s: &Snapshot, found_via: &str) -> Vec<String> {
    let st = s.status.as_ref();
    let tx = match &s.ladder {
        Some(LadderState::VoxRig) => "VOX rig".to_string(),
        Some(LadderState::Cat { tx_allowed, .. }) => dash(*tx_allowed),
        _ => "-".to_string(),
    };
    let peer = match st.map(|st| &st.peer) {
        Some(PeerSlot::Held(a)) => a.clone(),
        Some(PeerSlot::Empty) => "free".into(),
        _ => "-".into(),
    };
    let bridge_device = match st.and_then(|st| st.audio_device.as_ref()) {
        Some(d) => format!(
            "{}{}",
            dash(d.ok.map(|ok| if ok { "present" } else { "missing" })),
            d.error
                .as_ref()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default()
        ),
        None => dash::<String>(None),
    };
    vec![
        format!("Bridge          {} via {}", s.bridge.ip(), found_via),
        format!(
            "Version         {}",
            dash(s.version.as_ref().map(|v| v.version.clone()))
        ),
        format!(
            "Protocol        {}",
            dash(s.hello.as_ref().and_then(|h| h.protocol))
        ),
        format!(
            "Mode            {} (status asks as a monitor)",
            s.mode.word()
        ),
        format!("Peer slot       {peer}"),
        format!(
            "Monitors        {}",
            dash(st.and_then(|st| st.monitors.as_ref().map(|m| m.len())))
        ),
        format!(
            "Loss            tx {}  rx {} (bridge counters)",
            dash(st.and_then(|st| st.tx_packet_loss)),
            dash(st.and_then(|st| st.rx_packet_loss))
        ),
        format!(
            "Underruns       tx {}",
            dash(st.and_then(|st| st.tx_underruns))
        ),
        format!(
            "Overruns        rx {}",
            dash(st.and_then(|st| st.rx_overruns))
        ),
        format!("TX allowed      {tx}"),
        format!(
            "TX audio cap    {} s{}",
            dash(st.and_then(|st| st.tx_audio_cap_s)),
            if st.and_then(|st| st.tx_audio_muted) == Some(true) {
                " (tripped)"
            } else {
                ""
            }
        ),
        "Clock error     not measured (run the terminal UI with WSJT-X decoding)".to_string(),
        format!("Audio devices   {}", ui::checks(s, None)[1].detail),
        format!("Bridge device   {bridge_device}"),
        format!("Uptime          {} s", dash(st.and_then(|st| st.uptime_s))),
    ]
}

pub fn status(args: &Args) -> Result<(), String> {
    let found = app::find_bridge(args.bridge.as_deref())?;
    let addr = SocketAddr::new(found.addr, CONTROL_PORT);
    let (hello, status) = ask_bridge(addr, Duration::from_secs(2));
    let (version, ladder, rigctl) = fetch_http(&found.addr.to_string());
    let mut s = snapshot_of(addr, hello, status, version, ladder, rigctl);
    s.audio_fault = audio_devices_missing();
    for line in status_lines(&s, found.via.word()) {
        println!("{line}");
    }
    if s.status.is_none() {
        return Err("No status reply from the bridge.".into());
    }
    Ok(())
}

/// `platform` is the Permissions and Driver rows, which go after Audio devices.
pub fn doctor_lines(
    s: &Snapshot,
    port_open: Option<bool>,
    platform: Vec<CheckLine>,
) -> Vec<String> {
    let mut checks = ui::checks(s, port_open);
    let at = 2.min(checks.len());
    checks.splice(at..at, platform);
    checks
        .iter()
        .map(|c| {
            format!(
                "{:<20} {:<8} {}",
                c.name,
                format!("{} {}", c.glyph.text(true), c.word),
                c.detail
            )
        })
        .collect()
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn check_row(name: &'static str, detail: &str) -> CheckLine {
    CheckLine {
        name,
        glyph: Glyph::Check,
        word: "Check",
        detail: detail.to_string(),
    }
}

/// The Permissions and Driver rows for this machine.
fn platform_checks() -> Vec<CheckLine> {
    #[cfg(target_os = "macos")]
    {
        use crate::coreaudio;
        let terminal = coreaudio::terminal_app(std::env::var("TERM_PROGRAM").ok().as_deref());
        vec![
            coreaudio::permissions_check(coreaudio::mic_now(), &terminal),
            coreaudio::driver_check(coreaudio::installed_now()),
        ]
    }
    #[cfg(target_os = "linux")]
    {
        vec![
            check_row("Permissions", "not needed"),
            check_row("Driver", "PipeWire devices are made at run time"),
        ]
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        vec![
            check_row("Permissions", "not checked in this build"),
            check_row("Driver", "not available in this build"),
        ]
    }
}

pub fn doctor(args: &Args) -> Result<(), String> {
    let found = app::find_bridge(args.bridge.as_deref());
    let Ok(found) = found else {
        println!(
            "{:<20} {:<8} {}",
            "Bridge reachable",
            "[x] Fix",
            found.unwrap_err()
        );
        return Err("Bridge not found.".into());
    };
    let addr = SocketAddr::new(found.addr, CONTROL_PORT);
    let (hello, status) = ask_bridge(addr, Duration::from_secs(2));
    let (version, ladder, rigctl) = fetch_http(&found.addr.to_string());
    let port_open = Some(app::rigctl_port_open(found.addr, 4532));
    let mut s = snapshot_of(addr, hello, status, version, ladder, rigctl);
    s.audio_fault = audio_fault_now();
    for line in doctor_lines(&s, port_open, platform_checks()) {
        println!("{line}");
    }
    Ok(())
}

/// Linux: which cable devices `pactl` lists are missing. macOS: which driver
/// devices Core Audio does not know. Elsewhere the null adapter has none to miss.
fn audio_devices_missing() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        crate::pipewire::tools_fault().or_else(|| crate::pipewire::present_now().fault())
    }
    #[cfg(target_os = "macos")]
    {
        crate::coreaudio::installed_now().fault()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// Makes the real devices once to see whether they work (Linux: PipeWire),
/// then drops them. macOS: the driver devices, then the Microphone permission
/// (opening them here would raise the Microphone prompt from `doctor`).
/// Elsewhere there are no devices to fault.
fn audio_fault_now() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        use vbcore::audio::AudioAdapter;
        // A running `up` (or a crashed one) owns any device listed: never
        // load again, that makes a duplicate. Report what is missing instead.
        let present = crate::pipewire::present_now();
        if present.tx || present.rx {
            return present.fault();
        }
        match crate::pipewire::PipeWireAdapter::new() {
            Ok(adapter) => {
                std::thread::sleep(Duration::from_millis(300));
                adapter.fault()
            }
            Err(e) => Some(e.to_string()),
        }
    }
    #[cfg(target_os = "macos")]
    {
        use crate::coreaudio;
        coreaudio::installed_now().fault().or_else(|| {
            coreaudio::mic_now().blocks_tx().then(|| {
                coreaudio::mic_fault(&coreaudio::terminal_app(
                    std::env::var("TERM_PROGRAM").ok().as_deref(),
                ))
            })
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

pub fn setup_lines(app: App, s: &Snapshot) -> Vec<String> {
    let rigctl = ui::rigctl_address(s);
    let vox = matches!(s.ladder, Some(LadderState::VoxRig));
    let mut out = vec![ui::setup_intro(app), String::new()];
    for (i, (where_, what)) in app.steps(&rigctl, vox).iter().enumerate() {
        out.push(format!("{}. {where_}: {what}", i + 1));
    }
    out.push(String::new());
    out.push(ui::relay_note(s));
    out.push(ui::rigctl_note(s));
    out
}

pub fn setup(args: &Args, app_name: &str) -> Result<(), String> {
    let app = App::parse(app_name)
        .ok_or_else(|| format!("unknown app {app_name}: use wsjt-x, js8call or fldigi"))?;
    let (addr, via, s) = match app::find_bridge(args.bridge.as_deref()) {
        Ok(found) => {
            let addr = SocketAddr::new(found.addr, CONTROL_PORT);
            let (version, ladder, rigctl) = fetch_http(&found.addr.to_string());
            (
                addr,
                found.via.word(),
                snapshot_of(addr, None, None, version, ladder, rigctl),
            )
        }
        Err(_) => {
            let addr: SocketAddr = SocketAddr::new([10, 42, 0, 1].into(), CONTROL_PORT);
            let s = snapshot_of(
                addr,
                None,
                None,
                None,
                LadderState::Unavailable {
                    status: None,
                    detail: "not reached".into(),
                },
                RigctlState::Unavailable {
                    status: None,
                    detail: "not reached".into(),
                },
            );
            (addr, "not found; assuming the access point", s)
        }
    };
    // Where the apps go: the relay's port as VectorCable will listen on it.
    let listen = app::rig_listen(args);
    let mut s = s;
    s.rig_relay = Some(vbcore::rigrelay::RelayState::Listening(listen));
    println!("Bridge {} ({via})", addr.ip());
    for line in setup_lines(app, &s) {
        println!("{line}");
    }
    // Free now means the relay can take it when VectorCable starts.
    if std::net::TcpListener::bind(listen).is_err() {
        println!("{}", ui::relay_port_busy_note(&s, listen));
    }
    Ok(())
}

/// The soak report: loss, laptop underruns, Pi tx_underruns and rx_overruns.
pub fn soak_report(s: &Snapshot, minutes: u64) -> Vec<String> {
    let st = s.status.as_ref();
    vec![
        format!(
            "Soak            {minutes} min, gate off, {} packets sent",
            s.tx.audio_packets
        ),
        format!(
            "Loss            {} (bridge tx_packet_loss)",
            dash(st.and_then(|st| st.tx_packet_loss))
        ),
        format!("Laptop underruns {}", s.playback.underrun_samples / 480),
        format!(
            "Pi tx_underruns {}",
            dash(st.and_then(|st| st.tx_underruns))
        ),
        format!("Pi rx_overruns  {}", dash(st.and_then(|st| st.rx_overruns))),
        format!("Link            {}", s.link.word()),
    ]
}

pub fn soak(args: &Args) -> Result<(), String> {
    app::install_signals();
    let minutes = args.minutes.unwrap_or(1);
    app::first_run_screen(args, false)?;
    let started = app::start(args, Mode::Transmit, false)?;
    println!(
        "Soak: transmitting silence with the gate off to {} for {minutes} min. Ctrl-C stops.",
        started.found.addr
    );
    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    while Instant::now() < deadline && !app::QUIT.load(std::sync::atomic::Ordering::SeqCst) {
        while let Ok(a) = started.runner.announcements.try_recv() {
            println!("{} {a}", app::utc_clock());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let snapshot = started.runner.stop();
    for line in soak_report(&snapshot, minutes) {
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Snapshot {
        let st = BridgeStatus {
            tx_packet_loss: Some(3),
            tx_underruns: Some(1),
            rx_overruns: Some(0),
            peer: PeerSlot::Empty,
            ..BridgeStatus::default()
        };
        snapshot_of(
            "10.42.0.1:5101".parse().unwrap(),
            Some(HelloReply {
                protocol: Some(1),
                ..HelloReply::default()
            }),
            Some(st),
            Some(BridgeVersion {
                version: "0.9.3".into(),
                commit: None,
            }),
            LadderState::VoxRig,
            RigctlState::NoProxy,
        )
    }

    #[test]
    fn status_doctor_setup_and_soak_print_plain_words() {
        let s = snap();
        let lines = status_lines(&s, "given");
        assert!(
            lines.iter().any(|l| l == "Version         0.9.3"),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l == "Protocol        1"));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("Loss            tx 3  rx --")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l == "Underruns       tx 1"));
        assert!(lines.iter().any(|l| l == "Overruns        rx 0"));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("Mode            Listen Only")));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("Clock error     not measured")));
        assert!(
            lines.iter().any(|l| l.starts_with("Audio devices   ")
                && (l.contains("null adapter")
                    || l.contains("PipeWire")
                    || l.contains("Core Audio"))),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l == "TX allowed      VOX rig"));
        let d = doctor_lines(
            &s,
            Some(false),
            vec![
                check_row("Permissions", "not needed"),
                check_row("Driver", "made at run time"),
            ],
        );
        assert!(
            d[0].starts_with("Bridge reachable     [ok] Good"),
            "{}",
            d[0]
        );
        assert!(d.iter().any(|l| l.starts_with("Audio devices")), "{d:?}");
        assert!(
            d.iter().any(|l| l.starts_with("Radio control (CAT)")),
            "{d:?}"
        );
        assert!(
            d[2].starts_with("Permissions          [!] Check not needed"),
            "{d:?}"
        );
        assert!(d[3].starts_with("Driver"), "{d:?}");
        let setup = setup_lines(App::Js8Call, &s);
        assert!(setup[0].contains("radio control (CAT) and keying (PTT)"));
        assert!(setup.iter().any(|l| l.contains("10.42.0.1:4532")));
        assert!(setup.iter().any(|l| l.contains("VOX")));
        let mut soak = s.clone();
        soak.playback.underrun_samples = 960;
        let r = soak_report(&soak, 2);
        assert!(r.iter().any(|l| l == "Laptop underruns 2"));
        assert!(r.iter().any(|l| l == "Pi tx_underruns 1"));
    }
}
