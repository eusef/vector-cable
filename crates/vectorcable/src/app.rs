//! Runs the cable: finds the bridge, spawns the runner and drives either the
//! terminal UI or the `--plain` line output. Quit on `q`, Ctrl-C and SIGHUP
//! stops TX at once (the runner's stop sends the last step in the same call).

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use vbcore::audio::{AudioAdapter, NullAdapter};
use vbcore::clockcheck::{self, ClockCheck, ClockEstimate};
use vbcore::discovery::{self, Found};
use vbcore::pin::TokenStore;
use vbcore::playback::DEFAULT_TARGET_LATENCY_S;
use vbcore::protocol::CONTROL_PORT;
use vbcore::runner::{Command, CommandReply, Config, PinFor, Runner, Snapshot};
use vbcore::session::Mode;

use crate::cli::Args;
use crate::config;
use crate::theme::{underline_colour_supported, Depth, Theme, ThemeName};
use crate::ui::{self, App, LevelControl, Tab, View};

/// Set by SIGHUP, SIGINT and SIGTERM.
pub static QUIT: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_signal(_: libc::c_int) {
    QUIT.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
pub fn install_signals() {
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        let h = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGHUP, h);
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGTERM, h);
    }
}

#[cfg(not(unix))]
pub fn install_signals() {}

/// `HH:mm:ssZ` now, UTC.
pub fn utc_clock() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        % 86_400;
    format!(
        "{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// The locale says UTF-8, or the glyphs fall back to ASCII.
pub fn locale_is_utf8(env: &dyn Fn(&str) -> Option<String>) -> bool {
    for k in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Some(v) = env(k) {
            if !v.is_empty() {
                let v = v.to_ascii_lowercase();
                return v.contains("utf-8") || v.contains("utf8");
            }
        }
    }
    cfg!(windows)
}

pub fn env_var(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

/// The audio adapter for this platform, or the null adapter when asked.
pub fn adapter(name: Option<&str>) -> Result<(Box<dyn AudioAdapter>, String), String> {
    match name {
        Some("null") => Ok((
            Box::new(NullAdapter::new()),
            "null adapter: no audio devices".into(),
        )),
        Some(other) => Err(format!("unknown audio adapter {other}")),
        None => default_adapter(),
    }
}

#[cfg(target_os = "linux")]
fn default_adapter() -> Result<(Box<dyn AudioAdapter>, String), String> {
    match crate::pipewire::PipeWireAdapter::new() {
        Ok(a) => Ok((
            Box::new(a),
            "PipeWire: VectorBridge RX and VectorBridge TX".into(),
        )),
        Err(e) => Err(format!(
            "PipeWire devices not available ({e}); use --audio null to run without audio"
        )),
    }
}

#[cfg(target_os = "macos")]
fn default_adapter() -> Result<(Box<dyn AudioAdapter>, String), String> {
    match crate::coreaudio::CoreAudioAdapter::new() {
        Ok(a) => Ok((
            Box::new(a),
            "Core Audio: VectorBridge RX and VectorBridge TX".into(),
        )),
        Err(e) => Err(format!(
            "VectorBridge devices not available ({e}); use --audio null to run without audio"
        )),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn default_adapter() -> Result<(Box<dyn AudioAdapter>, String), String> {
    Ok((
        Box::new(NullAdapter::new()),
        "no virtual audio devices in this build: null adapter".into(),
    ))
}

pub fn find_bridge(given: Option<&str>) -> Result<Found, String> {
    discovery::discover(given, CONTROL_PORT, discovery::DEFAULT_PROBE_TIMEOUT).ok_or_else(|| {
        match given {
            Some(h) => format!("Bridge {h} did not answer on UDP {CONTROL_PORT}."),
            None => "No bridge found: not on its access point, and vectorbridge.local did not answer. Try --bridge HOST.".to_string(),
        }
    })
}

/// Listens for WSJT-X decode reports on its default port and turns them into
/// clock estimates. Silent when the port is taken.
fn spawn_clock_listener() -> Receiver<Option<ClockEstimate>> {
    let (tx, rx) = mpsc::channel();
    if let Ok(socket) = clockcheck::bind_listener("127.0.0.1:2237") {
        let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
        std::thread::Builder::new()
            .name("vectorcable-clock".into())
            .spawn(move || {
                let mut check = ClockCheck::new(DEFAULT_TARGET_LATENCY_S);
                let mut buf = [0u8; 2048];
                loop {
                    if let Ok((n, _)) = socket.recv_from(&mut buf) {
                        if let Some(d) = clockcheck::parse_decode(&buf[..n]) {
                            check.note(&d);
                            if tx.send(check.estimate()).is_err() {
                                return;
                            }
                        }
                    } else if tx.send(check.estimate()).is_err() {
                        return;
                    }
                }
            })
            .ok();
    }
    rx
}

pub struct Started {
    pub runner: Runner,
    pub found: Found,
    pub audio_note: String,
}

/// Before the Microphone prompt's first appearance, explains it (brief
/// section 8). With `wait`, Enter continues and `q`, Esc or Ctrl-C quits;
/// returns false to quit. Without it (`--plain`, `soak`) it prints and goes on.
#[cfg(target_os = "macos")]
pub fn first_run_screen(args: &Args, wait: bool) -> Result<bool, String> {
    use crate::coreaudio;
    if !coreaudio::needs_mic_screen(coreaudio::mic_now(), args.audio.as_deref()) {
        return Ok(true);
    }
    let term_program = env_var("TERM_PROGRAM");
    let terminal = coreaudio::terminal_app(term_program.as_deref());
    let mut out = std::io::stdout();
    for line in coreaudio::mic_screen(&terminal, term_program.as_deref()) {
        let _ = writeln!(out, "{line}");
    }
    if !wait {
        let _ = out.flush();
        return Ok(true);
    }
    let _ = write!(out, "\nEnter: continue    q: quit ");
    let _ = out.flush();
    crossterm::terminal::enable_raw_mode().map_err(|e| format!("terminal: {e}"))?;
    let go = loop {
        if QUIT.load(Ordering::SeqCst) {
            break false;
        }
        match event::poll(Duration::from_millis(200)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => break false,
        }
        if let Ok(Event::Key(k)) = event::read() {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match k.code {
                KeyCode::Enter => break true,
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break false,
                KeyCode::Char('q') | KeyCode::Esc => break false,
                _ => {}
            }
        }
    };
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = writeln!(out);
    Ok(go)
}

#[cfg(not(target_os = "macos"))]
pub fn first_run_screen(_: &Args, _: bool) -> Result<bool, String> {
    Ok(true)
}

/// Where bridge app tokens are kept: the Keychain on macOS, an owner-only file
/// on Linux; elsewhere, or with no home folder, only in memory, so the PIN is
/// asked for on each run.
#[cfg(target_os = "macos")]
fn token_store() -> Option<Arc<dyn TokenStore>> {
    Some(Arc::new(crate::keychain::Keychain::new()))
}

#[cfg(target_os = "linux")]
fn token_store() -> Option<Arc<dyn TokenStore>> {
    let path = crate::tokenfile::path()?;
    Some(Arc::new(crate::tokenfile::TokenFile::new(path)))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn token_store() -> Option<Arc<dyn TokenStore>> {
    None
}

/// Where the rig control relay listens: `--rig-port`, else the config file's
/// `rig_port`, else 4532.
pub fn rig_listen(args: &Args) -> std::net::SocketAddr {
    let port = args
        .rig_port
        .or_else(|| config::load().rig_port)
        .unwrap_or(vbcore::rigrelay::RIG_PORT);
    vbcore::rigrelay::listen_address(port)
}

pub fn start(args: &Args, mode: Mode, gated: bool) -> Result<Started, String> {
    let found = find_bridge(args.bridge.as_deref())?;
    let (audio, audio_note) = adapter(args.audio.as_deref())?;
    let mut config = Config::new(&found.addr.to_string());
    config.mode = mode;
    config.gated = gated;
    config.tokens = token_store();
    config.rig_relay = Some(rig_listen(args));
    let runner = Runner::spawn(config, audio).map_err(|e| format!("start: {e}"))?;
    Ok(Started {
        runner,
        found,
        audio_note,
    })
}

/// `--plain`: one line per event, a summary every 5 s, until a signal.
pub fn run_plain(args: &Args) -> Result<(), String> {
    install_signals();
    let mode = if args.listen {
        Mode::ListenOnly
    } else {
        Mode::Transmit
    };
    first_run_screen(args, false)?;
    let started = start(args, mode, true)?;
    let out = std::io::stdout();
    let mut out = out.lock();
    let _ = writeln!(
        out,
        "VectorCable {}  bridge {} ({}) via {}  {}",
        crate::version(),
        started.found.addr,
        started.found.host,
        started.found.via.word(),
        started.audio_note
    );
    let clock_rx = spawn_clock_listener();
    let mut last_summary = Instant::now();
    let mut last_snapshot = None;
    // Apps retry CAT every few seconds; the hint is printed once per run.
    let mut pin_hint_shown = false;
    loop {
        if QUIT.load(Ordering::SeqCst) {
            break;
        }
        while let Ok(a) = started.runner.announcements.try_recv() {
            let _ = writeln!(out, "{} {a}", utc_clock());
        }
        for line in plain_change_lines(&mut last_snapshot, started.runner.snapshot()) {
            let _ = writeln!(out, "{} {line}", utc_clock());
        }
        while let Ok(r) = started.runner.replies.try_recv() {
            match r {
                CommandReply::Refused(why) => {
                    let _ = writeln!(out, "{} Refused: {why}", utc_clock());
                }
                CommandReply::ModeSet(m) => {
                    let _ = writeln!(out, "{} Mode set: {}.", utc_clock(), m.word());
                }
                CommandReply::PinWanted(_) if pin_hint_shown => {}
                CommandReply::PinWanted(why) => {
                    pin_hint_shown = true;
                    let _ = writeln!(
                        out,
                        "{} The bridge PIN is needed {}; --plain cannot take it: run \
                         vectorcable without --plain and press p. {}",
                        utc_clock(),
                        pin_reason(why),
                        ui::pin_hint(&started.runner.snapshot(), bridge_name(&started.found))
                    );
                }
            }
        }
        while let Ok(e) = clock_rx.try_recv() {
            started.runner.command(Command::Clock(e));
        }
        if last_summary.elapsed() >= Duration::from_secs(5) {
            last_summary = Instant::now();
            let _ = writeln!(
                out,
                "{} {}",
                utc_clock(),
                summary(&started.runner.snapshot())
            );
        }
        let _ = out.flush();
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = writeln!(out, "{} Stopping: TX off.", utc_clock());
    let _ = out.flush();
    let _ = started.runner.stop();
    Ok(())
}

/// `--plain`'s copy of the TUI status region: the link and verdict sentences
/// whenever they change (brief section 8, "Announce changes").
fn plain_change_lines(last: &mut Option<Snapshot>, now: Snapshot) -> Vec<String> {
    let lines = change_lines(last.as_ref(), &now);
    *last = Some(now);
    lines
}

/// One line of state for `--plain`.
pub fn summary(s: &Snapshot) -> String {
    let (glyph, word) = ui::link_badge(s.link);
    let status = s.status.as_ref();
    let have_rx = s.rx.received > 0;
    format!(
        "link {} {} ({})  mode {}  loss rx {}  tx {}  underruns here {}  bridge {}  buffer {} ms  rx {} dBFS  tx {} dBFS  apps row {}",
        glyph.text(false),
        word,
        ui::link_reason(s.link),
        s.mode.word(),
        ui::loss_percent(Some(s.rx.lost), Some(s.rx.received)),
        ui::loss_percent(
            status.and_then(|st| st.tx_packet_loss),
            status.and_then(|st| st.tx_packets_received)
        ),
        vbcore::status::dash(have_rx.then_some(s.playback.underrun_samples / 480)),
        vbcore::status::dash(
            status
                .and_then(|st| st.tx_underruns)
                .filter(|_| s.tx.audio_packets > 0)
        ),
        vbcore::status::dash(s.buffer_ms.map(|b| format!("{b:.0}"))),
        vbcore::status::dash(s.rx_dbfs.map(|d| format!("{d:.0}"))),
        vbcore::status::dash(s.tx_dbfs.map(|d| format!("{d:.0}"))),
        vbcore::status::dash(s.listing.as_ref().map(|l| l.word().to_string())),
    )
}

/// The bridge accepts a TX audio cap of 0 to 600 s.
const TX_CAP_MAX_S: f64 = 600.0;

/// The status region's words when the link or a verdict changes: the verdict
/// sentence from the mockup, with the link reason when the link changed.
fn change_lines(was: Option<&Snapshot>, now: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();
    if was.map(|w| w.link) != Some(now.link) {
        let (glyph, word) = ui::link_badge(now.link);
        out.push(format!(
            "Link {} {}: {}.",
            glyph.text(false),
            word,
            ui::link_reason(now.link)
        ));
    }
    let sentence = ui::verdict_sentence(now);
    if was.map(ui::verdict_sentence).as_deref() != Some(sentence.as_str()) {
        out.push(sentence);
    }
    let pin = ui::pin_status(now);
    if let Some(p) = pin
        .as_ref()
        .filter(|_| was.map(ui::pin_status) != Some(pin.clone()))
    {
        out.push(format!(
            "Bridge PIN {} {}: {}.",
            p.glyph.text(false),
            p.word,
            p.label
        ));
    }
    out
}

/// UI state the key handler changes.
struct Ui {
    tab: Tab,
    theme: Theme,
    app: App,
    level: LevelControl,
    /// (description, command) waiting for y/n.
    pending: Option<(String, Command)>,
    level_confirmed: bool,
    alert: Option<String>,
    help: bool,
    /// The bridge PIN being typed (`p`), digits only.
    pin_entry: Option<String>,
    /// Why the PIN is asked for (`ask_for_pin`): the band shows it, but keys
    /// keep their jobs until `p` starts the entry.
    pin_reason: Option<String>,
    /// When the user last pressed Esc on the PIN entry.
    pin_declined: Option<Instant>,
    status_lines: Vec<String>,
    bell_due: bool,
    quit: bool,
}

/// The name the bridge was found by, for the PIN hint; `None` when it was
/// found by address, so the hint uses the bridge's current address.
fn bridge_name(found: &Found) -> Option<&str> {
    Some(found.host.trim()).filter(|h| !h.is_empty() && h.parse::<std::net::IpAddr>().is_err())
}

/// After Esc, a PIN asked for by an app (not by a key) waits this long
/// before it is asked for again.
const PIN_ASK_AGAIN: Duration = Duration::from_secs(60);

/// Words after "The bridge PIN is needed".
fn pin_reason(why: PinFor) -> &'static str {
    match why {
        PinFor::Setting => "to change the levels",
        PinFor::RigControl => "for rig control (CAT and PTT)",
    }
}

/// Asks for the PIN because something needs it: the band says why and where
/// the PIN is set, and `p` starts typing it. The band takes no other key, so
/// tabs (1 to 4) and Listen Only keep working. `by_key` is a key the user just
/// pressed; an app's retries do not ask again within a minute of Esc.
fn ask_for_pin(ui: &mut Ui, reason: &str, by_key: bool) {
    if ui.pin_entry.is_some() || ui.pin_reason.is_some() {
        return;
    }
    let declined = ui
        .pin_declined
        .is_some_and(|at| at.elapsed() < PIN_ASK_AGAIN);
    if declined && !by_key {
        return;
    }
    ui.pin_reason = Some(reason.to_string());
    ui.status_lines.push(format!(
        "The bridge PIN is needed {reason}: press p to type it."
    ));
}

/// The bridge stopped taking VectorCable's login (the PIN changed, or the
/// token was revoked): ask for the PIN again. A first `PinNeeded` at start-up
/// is shown on the Status tab, not asked for. Once logged in, an ask that is
/// still showing goes away.
fn note_pin_lost(ui: &mut Ui, was: Option<&Snapshot>, now: &Snapshot) {
    use vbcore::runner::ListingState as L;
    let logged_in = |s: &Snapshot| matches!(s.listing, Some(L::Listed | L::Stored));
    if logged_in(now) && ui.pin_entry.is_none() {
        ui.pin_reason = None;
    }
    if was.is_some_and(logged_in) && now.listing.as_ref().is_some_and(|l| l.wants_pin()) {
        ask_for_pin(
            ui,
            "again: the bridge no longer takes VectorCable's login",
            false,
        );
    }
}

/// What a key does. Pure, so the rules are testable. `q` and Ctrl-C quit
/// whatever is on screen; the caller stops TX at once.
fn handle_key(ui: &mut Ui, s: &Snapshot, code: KeyCode, modifiers: KeyModifiers) -> Vec<Command> {
    let mut cmds = Vec::new();
    let ctrl_c = modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c');
    if ctrl_c || code == KeyCode::Char('q') {
        ui.quit = true;
        return cmds;
    }
    if ui.alert.is_some() {
        if matches!(code, KeyCode::Enter | KeyCode::Esc) {
            ui.alert = None;
        }
        return cmds;
    }
    if let Some(entry) = ui.pin_entry.as_mut() {
        match code {
            KeyCode::Char(c) if c.is_ascii_digit() && entry.len() < 8 => entry.push(c),
            KeyCode::Backspace => {
                entry.pop();
            }
            KeyCode::Enter if vbcore::pin::valid_pin(entry) => {
                cmds.push(Command::SetPin(entry.clone()));
                ui.pin_entry = None;
                ui.pin_reason = None;
                ui.status_lines
                    .push("Bridge PIN sent; logging in to list VectorCable.".into());
            }
            KeyCode::Enter => ui
                .status_lines
                .push("The bridge PIN is 4 to 8 digits.".into()),
            KeyCode::Esc => {
                ui.pin_entry = None;
                ui.pin_reason = None;
                ui.pin_declined = Some(Instant::now());
                ui.status_lines.push("Bridge PIN cancelled.".into());
            }
            _ => {}
        }
        return cmds;
    }
    if let Some((desc, cmd)) = ui.pending.clone() {
        match code {
            KeyCode::Char('y') => {
                ui.pending = None;
                ui.level_confirmed = true;
                ui.status_lines.push(format!("{desc} requested."));
                cmds.push(cmd);
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                ui.pending = None;
                ui.status_lines.push("Level change cancelled.".into());
            }
            _ => {}
        }
        return cmds;
    }
    match code {
        KeyCode::Char('1') => ui.tab = Tab::Status,
        KeyCode::Char('2') => ui.tab = Tab::Setup,
        KeyCode::Char('3') => ui.tab = Tab::Check,
        KeyCode::Char('4') => ui.tab = Tab::Levels,
        KeyCode::Tab => ui.tab = ui.tab.next(),
        KeyCode::BackTab => ui.tab = ui.tab.previous(),
        KeyCode::Char('t') => cmds.push(Command::SetMode(Mode::Transmit)),
        KeyCode::Char('l') => cmds.push(Command::SetMode(Mode::ListenOnly)),
        // The radio monitor: RX audio also on the default output. Off at every launch.
        KeyCode::Char('m') => cmds.push(Command::SetMonitor(s.monitor.is_none())),
        KeyCode::Char('n') => {
            ui.theme.name = ui.theme.name.toggled();
            // The rest of the file (the relay port) is kept.
            let mut saved = config::load();
            saved.theme = Some(ui.theme.name);
            let _ = config::save(&saved);
            ui.status_lines
                .push(format!("Theme {}.", ui.theme.name.word()));
        }
        KeyCode::Char('?') => ui.help = !ui.help,
        // Keeps `pin_reason`, so the entry still says why.
        KeyCode::Char('p') => ui.pin_entry = Some(String::new()),
        KeyCode::Esc if ui.pin_reason.is_some() => {
            ui.pin_reason = None;
            ui.pin_declined = Some(Instant::now());
            ui.status_lines
                .push("Bridge PIN not entered; press p when you want to.".into());
        }
        KeyCode::Char('w') if ui.tab == Tab::Setup => ui.app = App::WsjtX,
        KeyCode::Char('j') if ui.tab == Tab::Setup => ui.app = App::Js8Call,
        KeyCode::Char('f') if ui.tab == Tab::Setup => ui.app = App::Fldigi,
        KeyCode::Up | KeyCode::Down if ui.tab == Tab::Levels && !ui::levels_known(s) => {
            ui.status_lines
                .push("Waiting for the bridge to report the current levels.".into());
        }
        // Without a login the bridge refuses the change: ask for the PIN first.
        KeyCode::Up | KeyCode::Down
            if ui.tab == Tab::Levels
                && s.listing.as_ref().is_some_and(|l| l.wants_pin())
                && !(ui.level == LevelControl::RxGain
                    && s.status.as_ref().and_then(|st| st.rx_gain_fixed) == Some(true)) =>
        {
            ask_for_pin(ui, pin_reason(PinFor::Setting), true);
        }
        KeyCode::Up | KeyCode::Down if ui.tab == Tab::Levels => {
            let up = code == KeyCode::Up;
            let status = s.status.as_ref();
            let step = |v: f64, by: f64, max: f64| {
                let n = if up { v + by } else { v - by };
                n.clamp(0.0, max)
            };
            let change = match ui.level {
                LevelControl::TxGain => {
                    let v = step(status.and_then(|st| st.tx_gain).unwrap_or(50.0), 5.0, 100.0);
                    Some((
                        format!("TX gain {v:.0} %."),
                        Command::SetGain {
                            tx: Some(v as u8),
                            rx: None,
                        },
                    ))
                }
                LevelControl::RxGain => {
                    if status.and_then(|st| st.rx_gain_fixed) == Some(true) {
                        ui.status_lines
                            .push("RX gain is fixed on this bridge.".into());
                        None
                    } else {
                        let v = step(status.and_then(|st| st.rx_gain).unwrap_or(50.0), 5.0, 100.0);
                        Some((
                            format!("RX gain {v:.0} %."),
                            Command::SetGain {
                                tx: None,
                                rx: Some(v as u8),
                            },
                        ))
                    }
                }
                LevelControl::TxCap => {
                    let v = step(
                        status.and_then(|st| st.tx_audio_cap_s).unwrap_or(0) as f64,
                        10.0,
                        TX_CAP_MAX_S,
                    );
                    Some((
                        format!("TX audio cap {v:.0} s."),
                        Command::SetTxAudioCap { seconds: v as u16 },
                    ))
                }
            };
            if let Some((desc, cmd)) = change {
                if ui.level_confirmed {
                    ui.status_lines.push(format!("{desc} requested."));
                    cmds.push(cmd);
                } else {
                    // The first change asks: it changes the level for every client.
                    ui.pending = Some((desc, cmd));
                }
            }
        }
        KeyCode::Right | KeyCode::Enter if ui.tab == Tab::Levels => ui.level = ui.level.next(),
        KeyCode::Left if ui.tab == Tab::Levels => ui.level = ui.level.previous(),
        _ => {}
    }
    cmds
}

/// Whether a verdict that was Ready (or Transmitting) has stopped being so.
fn got_worse(was: ui::Verdict, now: ui::Verdict) -> bool {
    use ui::Verdict::*;
    matches!(was, Ready | Transmitting) && matches!(now, NotReadyPending | NotReadyFault | Locked)
}

/// A regression, brief section 8: a verdict that was Ready and stopped.
/// Nothing that was never Ready (a start-up refusal, the first search)
/// opens the alert.
fn note_regression(ui: &mut Ui, was: Option<&Snapshot>, now: &Snapshot, announced: &[String]) {
    let Some(was) = was else { return };
    let receive = got_worse(ui::receive_card(was).verdict, ui::receive_card(now).verdict);
    let transmit = got_worse(
        ui::transmit_card(was).verdict,
        ui::transmit_card(now).verdict,
    );
    if !(receive || transmit) {
        return;
    }
    let text = announced
        .iter()
        .find(|a| a.contains("TX stopped") || a.starts_with("Transmit refused"))
        .cloned()
        .unwrap_or_else(|| ui::verdict_sentence(now));
    if ui.alert.is_none() {
        ui.bell_due = true;
    }
    ui.alert = Some(text);
}

/// The terminal UI.
pub fn run_tui(args: &Args) -> Result<(), String> {
    install_signals();
    let mode = if args.listen {
        Mode::ListenOnly
    } else {
        Mode::Transmit
    };
    let saved = config::load();
    let theme_name = args.theme.or(saved.theme).unwrap_or(ThemeName::Standard);
    let depth = Depth::detect(&env_var);
    let ascii = !locale_is_utf8(&env_var);
    if !first_run_screen(args, true)? {
        return Ok(());
    }
    let started = start(args, mode, true)?;
    let clock_rx = spawn_clock_listener();
    let rigctl_probe = spawn_rigctl_probe(started.found.addr);
    let mut ui = Ui {
        tab: Tab::Status,
        theme: Theme {
            underline_colour: underline_colour_supported(&env_var),
            ..Theme::new(theme_name, depth)
        },
        app: App::WsjtX,
        level: LevelControl::TxGain,
        pending: None,
        level_confirmed: false,
        alert: None,
        help: false,
        pin_entry: None,
        pin_reason: None,
        pin_declined: None,
        status_lines: vec![format!(
            "Bridge {} via {}. {}",
            started.found.addr,
            started.found.via.word(),
            started.audio_note
        )],
        bell_due: false,
        quit: false,
    };
    let mut terminal = ratatui::init();
    let mut rigctl_port_open = None;
    let mut last_snapshot: Option<Snapshot> = None;
    let mut meter_at = Instant::now();
    let mut meters = (None, None);
    let result = loop {
        if QUIT.load(Ordering::SeqCst) || ui.quit {
            break Ok(());
        }
        let snapshot = started.runner.snapshot();
        let mut announced = Vec::new();
        while let Ok(a) = started.runner.announcements.try_recv() {
            announced.push(a);
        }
        while let Ok(r) = started.runner.replies.try_recv() {
            match r {
                CommandReply::Refused(why) => announced.push(why),
                CommandReply::ModeSet(m) => announced.push(format!("Mode set: {}.", m.word())),
                // A level change was just keyed; rig control is an app retrying.
                // Locked: a PIN now only meets the lockout again; the Status
                // tab shows the wait.
                CommandReply::PinWanted(_)
                    if matches!(
                        snapshot.listing,
                        Some(vbcore::runner::ListingState::Locked(_))
                    ) => {}
                CommandReply::PinWanted(why) => {
                    ask_for_pin(&mut ui, pin_reason(why), why == PinFor::Setting)
                }
            }
        }
        note_pin_lost(&mut ui, last_snapshot.as_ref(), &snapshot);
        note_regression(&mut ui, last_snapshot.as_ref(), &snapshot, &announced);
        ui.status_lines.extend(announced);
        ui.status_lines
            .extend(change_lines(last_snapshot.as_ref(), &snapshot));
        last_snapshot = Some(snapshot.clone());
        if ui.status_lines.len() > 50 {
            let drop = ui.status_lines.len() - 50;
            ui.status_lines.drain(..drop);
        }
        while let Ok(e) = clock_rx.try_recv() {
            started.runner.command(Command::Clock(e));
        }
        while let Ok(open) = rigctl_probe.try_recv() {
            rigctl_port_open = Some(open);
        }
        if !args.reduce_motion || meter_at.elapsed() >= Duration::from_secs(1) {
            meter_at = Instant::now();
            meters = (snapshot.rx_dbfs, snapshot.tx_dbfs);
        }
        let mut shown = snapshot.clone();
        shown.rx_dbfs = meters.0;
        shown.tx_dbfs = meters.1;
        let view = View {
            snapshot: shown,
            tab: ui.tab,
            theme: ui.theme,
            ascii,
            clock: utc_clock(),
            status_lines: ui.status_lines.clone(),
            alert: ui.alert.clone(),
            app: ui.app,
            level: ui.level,
            confirm: ui.pending.as_ref().map(|(d, _)| d.clone()),
            rigctl_port_open,
            help: ui.help,
            pin_entry: ui.pin_entry.as_ref().map(|e| e.len()),
            pin_reason: ui.pin_reason.clone(),
            bridge_name: bridge_name(&started.found).map(String::from),
        };
        if ui.bell_due {
            ui.bell_due = false;
            let _ = std::io::stdout().write_all(b"\x07");
        }
        if let Err(e) = terminal.draw(|f| ui::draw(f, &view)) {
            break Err(format!("draw: {e}"));
        }
        match event::poll(Duration::from_millis(50)) {
            Ok(true) => {
                if let Ok(Event::Key(k)) = event::read() {
                    if k.kind != KeyEventKind::Release {
                        for cmd in handle_key(&mut ui, &snapshot, k.code, k.modifiers) {
                            started.runner.command(cmd);
                        }
                    }
                }
            }
            Ok(false) => {}
            Err(e) => break Err(format!("input: {e}")),
        }
    };
    // TX off first: a hung terminal must not delay the stop.
    let _ = started.runner.stop();
    ratatui::restore();
    println!("Stopped: TX off.");
    result
}

/// A plain TCP connect to the rig control port, nothing sent: the most a check may do.
fn spawn_rigctl_probe(addr: std::net::IpAddr) -> Receiver<bool> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("vectorcable-rigctl-probe".into())
        .spawn(move || loop {
            let open = rigctl_port_open(addr, 4532);
            if tx.send(open).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_secs(30));
        })
        .ok();
    rx
}

pub fn rigctl_port_open(addr: std::net::IpAddr, port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::new(addr, port),
        Duration::from_millis(800),
    )
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vbcore::session::Link;
    use vbcore::status::{BridgeStatus, LadderState};

    fn snapshot() -> Snapshot {
        Snapshot {
            bridge: "10.42.0.1:5101".parse().unwrap(),
            local_audio_port: 1,
            mode: Mode::ListenOnly,
            link: Link::Connected,
            hello: None,
            status: Some(BridgeStatus {
                tx_gain: Some(50.0),
                rx_gain_fixed: Some(true),
                tx_audio_cap_s: Some(60),
                ..BridgeStatus::default()
            }),
            ladder: None,
            rigctl: None,
            relay_rigctl: None,
            version: None,
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
            audio_name: "null".into(),
            audio_fault: None,
            monitor: None,
            slot_held_by_other: false,
            rig_relay: None,
            local_ip: None,
        }
    }

    fn ui() -> Ui {
        Ui {
            tab: Tab::Status,
            theme: Theme::new(ThemeName::Standard, Depth::None),
            app: App::WsjtX,
            level: LevelControl::TxGain,
            pending: None,
            level_confirmed: false,
            alert: None,
            help: false,
            pin_entry: None,
            pin_reason: None,
            pin_declined: None,
            status_lines: Vec::new(),
            bell_due: false,
            quit: false,
        }
    }

    #[test]
    fn keys_switch_tabs_modes_and_quit() {
        let s = snapshot();
        let mut u = ui();
        handle_key(&mut u, &s, KeyCode::Char('4'), KeyModifiers::NONE);
        assert_eq!(u.tab, Tab::Levels);
        handle_key(&mut u, &s, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(u.tab, Tab::Status);
        handle_key(&mut u, &s, KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(u.tab, Tab::Levels);
        assert_eq!(
            handle_key(&mut u, &s, KeyCode::Char('t'), KeyModifiers::NONE),
            vec![Command::SetMode(Mode::Transmit)]
        );
        assert_eq!(
            handle_key(&mut u, &s, KeyCode::Char('l'), KeyModifiers::NONE),
            vec![Command::SetMode(Mode::ListenOnly)]
        );
        handle_key(&mut u, &s, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(u.quit);
        let mut u = ui();
        handle_key(&mut u, &s, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(u.quit);
    }

    /// `m` asks for the radio monitor on when it is off, and off when it is on,
    /// from any tab.
    #[test]
    fn m_toggles_the_radio_monitor_from_any_tab() {
        let mut s = snapshot();
        let mut u = ui();
        for tab in ['1', '2', '3', '4'] {
            handle_key(&mut u, &s, KeyCode::Char(tab), KeyModifiers::NONE);
            assert_eq!(
                handle_key(&mut u, &s, KeyCode::Char('m'), KeyModifiers::NONE),
                vec![Command::SetMonitor(true)],
                "tab {tab}"
            );
        }
        s.monitor = Some("Speakers".into());
        assert_eq!(
            handle_key(&mut u, &s, KeyCode::Char('m'), KeyModifiers::NONE),
            vec![Command::SetMonitor(false)]
        );
    }

    /// `p` opens the bridge PIN entry: digits go to the PIN (not the tabs), a
    /// short PIN is not sent, Enter sends a 4 to 8 digit one, Esc cancels.
    #[test]
    fn p_takes_a_4_to_8_digit_bridge_pin_and_esc_cancels() {
        let s = snapshot();
        let mut u = ui();
        let key = |u: &mut Ui, code| handle_key(u, &s, code, KeyModifiers::NONE);
        key(&mut u, KeyCode::Char('p'));
        for c in ['1', '2', '3'] {
            assert!(key(&mut u, KeyCode::Char(c)).is_empty());
        }
        assert_eq!(u.tab, Tab::Status);
        assert!(key(&mut u, KeyCode::Enter).is_empty());
        assert_eq!(u.pin_entry.as_deref(), Some("123"));
        key(&mut u, KeyCode::Char('x'));
        key(&mut u, KeyCode::Char('4'));
        assert_eq!(
            key(&mut u, KeyCode::Enter),
            vec![Command::SetPin("1234".into())]
        );
        assert!(u.pin_entry.is_none());
        key(&mut u, KeyCode::Char('p'));
        key(&mut u, KeyCode::Char('9'));
        key(&mut u, KeyCode::Backspace);
        assert_eq!(u.pin_entry.as_deref(), Some(""));
        assert!(key(&mut u, KeyCode::Esc).is_empty());
        assert!(u.pin_entry.is_none());
    }

    /// A level change with no bridge login asks for the PIN, says why, and
    /// sends nothing; `p` then types it, and the change goes out once it is in.
    #[test]
    fn a_level_change_without_the_pin_asks_for_it_and_sends_nothing() {
        use vbcore::runner::ListingState;
        let mut s = snapshot();
        s.listing = Some(ListingState::PinNeeded);
        let mut u = ui();
        u.tab = Tab::Levels;
        u.level_confirmed = true;
        assert!(handle_key(&mut u, &s, KeyCode::Up, KeyModifiers::NONE).is_empty());
        assert_eq!(u.pin_reason.as_deref(), Some("to change the levels"));
        assert!(u.pin_entry.is_none(), "asking takes no keys until p");
        assert!(u.status_lines.contains(
            &"The bridge PIN is needed to change the levels: press p to type it.".to_string()
        ));
        handle_key(&mut u, &s, KeyCode::Char('p'), KeyModifiers::NONE);
        assert_eq!(u.pin_entry.as_deref(), Some(""));
        assert_eq!(u.pin_reason.as_deref(), Some("to change the levels"));
        for c in "2468".chars() {
            handle_key(&mut u, &s, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(
            handle_key(&mut u, &s, KeyCode::Enter, KeyModifiers::NONE),
            vec![Command::SetPin("2468".into())]
        );
        assert!(u.pin_entry.is_none() && u.pin_reason.is_none());
        s.listing = Some(ListingState::Listed);
        assert_eq!(
            handle_key(&mut u, &s, KeyCode::Up, KeyModifiers::NONE).len(),
            1
        );
    }

    /// An app asking (rig control) shows the ask, but not again within a
    /// minute of Esc; a key the user presses always shows it.
    #[test]
    fn an_app_asking_for_the_pin_shows_the_ask_but_not_again_right_after_esc() {
        let s = snapshot();
        let mut u = ui();
        ask_for_pin(&mut u, pin_reason(PinFor::RigControl), false);
        assert_eq!(
            u.pin_reason.as_deref(),
            Some("for rig control (CAT and PTT)")
        );
        // A second ask while one shows changes nothing.
        ask_for_pin(&mut u, pin_reason(PinFor::Setting), true);
        assert_eq!(
            u.pin_reason.as_deref(),
            Some("for rig control (CAT and PTT)")
        );
        assert!(handle_key(&mut u, &s, KeyCode::Esc, KeyModifiers::NONE).is_empty());
        assert!(u.pin_reason.is_none() && u.pin_entry.is_none());
        ask_for_pin(&mut u, pin_reason(PinFor::RigControl), false);
        assert!(u.pin_reason.is_none(), "an app retrying does not ask again");
        ask_for_pin(&mut u, pin_reason(PinFor::Setting), true);
        assert!(u.pin_reason.is_some());
        u.pin_reason = None;
        u.pin_declined = Some(Instant::now() - PIN_ASK_AGAIN);
        ask_for_pin(&mut u, pin_reason(PinFor::RigControl), false);
        assert!(u.pin_reason.is_some(), "a minute later it asks again");
    }

    /// While the PIN is asked for, every key keeps its job: 1 to 4 switch
    /// tabs (no digit reaches a PIN), Enter picks the next level, `l` goes to
    /// Listen Only. Only `p` starts typing, and only Esc dismisses it.
    #[test]
    fn an_ask_for_the_pin_never_takes_the_other_keys() {
        let s = snapshot();
        let mut u = ui();
        ask_for_pin(&mut u, pin_reason(PinFor::RigControl), false);
        for (key, tab) in [('2', Tab::Setup), ('3', Tab::Check), ('4', Tab::Levels)] {
            assert!(handle_key(&mut u, &s, KeyCode::Char(key), KeyModifiers::NONE).is_empty());
            assert_eq!(u.tab, tab);
        }
        assert!(handle_key(&mut u, &s, KeyCode::Enter, KeyModifiers::NONE).is_empty());
        assert_eq!(u.level, LevelControl::TxGain.next());
        assert_eq!(
            handle_key(&mut u, &s, KeyCode::Char('l'), KeyModifiers::NONE),
            vec![Command::SetMode(Mode::ListenOnly)]
        );
        assert!(u.pin_entry.is_none());
        assert!(u.pin_reason.is_some(), "the ask still shows");
        // Typing, started with p, takes digits and ignores letters as before.
        handle_key(&mut u, &s, KeyCode::Char('p'), KeyModifiers::NONE);
        handle_key(&mut u, &s, KeyCode::Char('1'), KeyModifiers::NONE);
        assert!(handle_key(&mut u, &s, KeyCode::Char('l'), KeyModifiers::NONE).is_empty());
        assert_eq!(u.pin_entry.as_deref(), Some("1"));
    }

    /// The PIN hint names the bridge by the name it was found by, never by
    /// an address string, which the live address replaces.
    #[test]
    fn the_bridge_name_is_a_found_name_not_an_address() {
        use vbcore::discovery::Via;
        let found = |host: &str| Found {
            host: host.into(),
            addr: "10.42.0.1".parse().unwrap(),
            via: Via::Given,
        };
        assert_eq!(
            bridge_name(&found("vectorbridge.local")),
            Some("vectorbridge.local")
        );
        assert_eq!(bridge_name(&found("10.42.0.1")), None);
        assert_eq!(bridge_name(&found("fd00::7")), None);
        assert_eq!(bridge_name(&found("")), None);
    }

    /// Fixed RX gain says so, PIN or not.
    #[test]
    fn fixed_rx_gain_says_so_without_asking_for_the_pin() {
        let mut s = snapshot();
        s.listing = Some(vbcore::runner::ListingState::PinNeeded);
        let mut u = ui();
        u.tab = Tab::Levels;
        u.level = LevelControl::RxGain;
        assert!(handle_key(&mut u, &s, KeyCode::Up, KeyModifiers::NONE).is_empty());
        assert!(u.pin_reason.is_none() && u.pin_entry.is_none());
        assert!(u
            .status_lines
            .contains(&"RX gain is fixed on this bridge.".to_string()));
    }

    /// A login the bridge stops taking asks for the PIN again; the first
    /// "needs the PIN" at start-up only shows on the Status tab. A login
    /// clears an ask still showing.
    #[test]
    fn a_lost_login_asks_for_the_pin_but_start_up_does_not() {
        use vbcore::runner::ListingState;
        let mut was = snapshot();
        let mut now = snapshot();
        now.listing = Some(ListingState::PinNeeded);
        let mut u = ui();
        note_pin_lost(&mut u, None, &now);
        note_pin_lost(&mut u, Some(&was), &now);
        assert!(u.pin_reason.is_none());
        was.listing = Some(ListingState::Listed);
        note_pin_lost(&mut u, Some(&was), &now);
        assert!(u.pin_reason.as_deref().unwrap().starts_with("again"));
        assert!(u.pin_entry.is_none());
        note_pin_lost(&mut u, Some(&now), &was);
        assert!(u.pin_reason.is_none(), "logged in again: the ask goes");
        // The status region says so when the PIN state changes.
        assert!(change_lines(Some(&was), &now)
            .contains(&"Bridge PIN \u{25b2} Check: not entered.".to_string()));
        assert!(!change_lines(Some(&now), &now)
            .iter()
            .any(|l| l.starts_with("Bridge PIN")));
    }

    /// The first level change asks for confirmation; later ones go straight out.
    #[test]
    fn first_level_change_asks_to_confirm_then_changes_apply_at_once() {
        let s = snapshot();
        let mut u = ui();
        u.tab = Tab::Levels;
        assert!(handle_key(&mut u, &s, KeyCode::Up, KeyModifiers::NONE).is_empty());
        assert_eq!(
            u.pending.as_ref().map(|(d, _)| d.as_str()),
            Some("TX gain 55 %.")
        );
        // Other keys are ignored until answered, except quit; n cancels.
        assert!(handle_key(&mut u, &s, KeyCode::Char('1'), KeyModifiers::NONE).is_empty());
        assert_eq!(u.tab, Tab::Levels);
        handle_key(&mut u, &s, KeyCode::Char('n'), KeyModifiers::NONE);
        assert!(u.pending.is_none() && !u.level_confirmed);
        handle_key(&mut u, &s, KeyCode::Down, KeyModifiers::NONE);
        let sent = handle_key(&mut u, &s, KeyCode::Char('y'), KeyModifiers::NONE);
        assert_eq!(
            sent,
            vec![Command::SetGain {
                tx: Some(45),
                rx: None
            }]
        );
        assert!(u.level_confirmed);
        assert_eq!(
            handle_key(&mut u, &s, KeyCode::Up, KeyModifiers::NONE),
            vec![Command::SetGain {
                tx: Some(55),
                rx: None
            }]
        );
        // RX gain fixed on the bridge: nothing to send. The cap steps by 10 s.
        handle_key(&mut u, &s, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(u.level, LevelControl::RxGain);
        assert!(handle_key(&mut u, &s, KeyCode::Up, KeyModifiers::NONE).is_empty());
        handle_key(&mut u, &s, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(
            handle_key(&mut u, &s, KeyCode::Down, KeyModifiers::NONE),
            vec![Command::SetTxAudioCap { seconds: 50 }]
        );
        assert!(u
            .status_lines
            .iter()
            .any(|l| l == "TX audio cap 50 s. requested."));
        assert!(!u.status_lines.iter().any(|l| l.contains("sent")));
        handle_key(&mut u, &s, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(u.level, LevelControl::RxGain);
        // The cap never goes past 600 s.
        let mut capped = snapshot();
        capped.status.as_mut().unwrap().tx_audio_cap_s = Some(595);
        u.level = LevelControl::TxCap;
        assert_eq!(
            handle_key(&mut u, &capped, KeyCode::Up, KeyModifiers::NONE),
            vec![Command::SetTxAudioCap { seconds: 600 }]
        );
        // Nothing changes until status has reported the current values.
        let mut unknown = snapshot();
        unknown.status = None;
        let mut u = ui();
        u.tab = Tab::Levels;
        assert!(handle_key(&mut u, &unknown, KeyCode::Up, KeyModifiers::NONE).is_empty());
        assert!(u.pending.is_none());
        assert!(u.status_lines[0].starts_with("Waiting for the bridge"));
    }

    /// `q` and Ctrl-C quit whatever is on screen: an alert or a confirmation
    /// never holds the TX stop back.
    #[test]
    fn quit_keys_always_quit_even_with_an_alert_or_a_confirmation_open() {
        let s = snapshot();
        for (code, mods) in [
            (KeyCode::Char('q'), KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut u = ui();
            u.alert = Some("Link lost: TX stopped.".into());
            handle_key(&mut u, &s, code, mods);
            assert!(u.quit, "{code:?} with an alert open");
            let mut u = ui();
            u.pending = Some((
                "TX gain 55 %.".into(),
                Command::SetGain {
                    tx: Some(55),
                    rx: None,
                },
            ));
            handle_key(&mut u, &s, code, mods);
            assert!(u.quit, "{code:?} with a confirmation open");
        }
    }

    #[test]
    fn the_status_region_repeats_link_and_verdict_changes_in_words() {
        let s = snapshot();
        let first = change_lines(None, &s);
        assert_eq!(
            first,
            vec![
                "Link \u{2713} Good: replies and RX audio arriving.".to_string(),
                "Receive Not Ready. Transmit Listen Only.".to_string()
            ]
        );
        assert!(change_lines(Some(&s), &s).is_empty(), "no change, no words");
        let mut later = s.clone();
        later.rx.received = 10;
        assert_eq!(
            change_lines(Some(&s), &later),
            vec!["Receive Ready. Transmit Listen Only.".to_string()]
        );
        let mut lost = later.clone();
        lost.link = Link::Lost;
        let lines = change_lines(Some(&later), &lost);
        assert_eq!(
            lines[0],
            "Link \u{2717} Fix: no reply for 10 s, reconnecting."
        );
        assert_eq!(lines[1], "Receive Not Ready. Transmit Listen Only.");
    }

    #[test]
    fn plain_prints_the_status_region_lines_once_per_change() {
        let s = snapshot();
        let mut last = None;
        assert_eq!(
            plain_change_lines(&mut last, s.clone()),
            change_lines(None, &s)
        );
        assert!(plain_change_lines(&mut last, s.clone()).is_empty());
        let mut later = s.clone();
        later.rx.received = 10;
        assert_eq!(
            plain_change_lines(&mut last, later),
            vec!["Receive Ready. Transmit Listen Only.".to_string()]
        );
    }

    #[test]
    fn a_lost_link_or_a_tx_stop_raises_the_alert_and_rings_once() {
        let mut ready = snapshot();
        ready.mode = Mode::Transmit;
        ready.rx.received = 10;
        ready.ladder = Some(LadderState::VoxRig);
        assert_eq!(
            ui::verdict_sentence(&ready),
            "Receive Ready. Transmit Ready."
        );
        let mut lost = ready.clone();
        lost.link = Link::Lost;
        let mut u = ui();
        note_regression(&mut u, Some(&ready), &lost, &[]);
        assert!(u.alert.is_some() && u.bell_due);
        assert_eq!(
            u.alert.as_deref(),
            Some("Receive Not Ready. Transmit Not Ready.")
        );
        u.bell_due = false;
        note_regression(
            &mut u,
            Some(&ready),
            &lost,
            &["Link lost: TX stopped.".into()],
        );
        assert!(!u.bell_due, "rings once while the alert shows");
        assert_eq!(u.alert.as_deref(), Some("Link lost: TX stopped."));
        handle_key(&mut u, &lost, KeyCode::Enter, KeyModifiers::NONE);
        assert!(u.alert.is_none());
        // Ready to Not Ready (RX paused) and Ready to Locked (cap tripped) alert too.
        let mut paused = ready.clone();
        paused.link = Link::RxPaused;
        let mut u = ui();
        note_regression(&mut u, Some(&ready), &paused, &[]);
        assert_eq!(
            u.alert.as_deref(),
            Some("Receive Not Ready. Transmit Ready.")
        );
        let mut tripped = ready.clone();
        tripped.ladder = Some(LadderState::Cat {
            tx_allowed: Some(true),
            watchdog_s: None,
            max_tx_s: None,
        });
        tripped.status.as_mut().unwrap().tx_audio_muted = Some(true);
        let mut u = ui();
        note_regression(&mut u, Some(&ready), &tripped, &[]);
        assert_eq!(u.alert.as_deref(), Some("Receive Ready. Transmit Locked."));
        // A start-up refusal was never Ready: no alert, no bell.
        let searching = snapshot();
        let mut u = ui();
        note_regression(
            &mut u,
            None,
            &searching,
            &["Transmit refused: another client (10.42.0.9:5100) holds the slot. Starting in Listen Only; press t to take it.".into()],
        );
        note_regression(&mut u, Some(&searching), &searching, &[]);
        assert!(u.alert.is_none() && !u.bell_due);
        // Not Ready to Ready is not a regression.
        let mut u = ui();
        note_regression(&mut u, Some(&lost), &ready, &[]);
        assert!(u.alert.is_none());
    }

    #[test]
    fn clock_and_locale_helpers() {
        let c = utc_clock();
        assert_eq!(c.len(), 9);
        assert!(c.ends_with('Z') && &c[2..3] == ":" && &c[5..6] == ":");
        let env = |vars: &[(&str, &str)]| {
            let vars: Vec<(String, String)> = vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        };
        assert!(locale_is_utf8(&env(&[("LANG", "en_GB.UTF-8")])));
        assert!(!locale_is_utf8(&env(&[("LANG", "C")])));
        assert!(!locale_is_utf8(&env(&[
            ("LC_ALL", "C"),
            ("LANG", "en_GB.UTF-8")
        ])));
        let line = summary(&snapshot());
        let none = vbcore::status::dash::<String>(None);
        assert!(
            line.contains("link \u{2713} Good (replies and RX audio arriving)")
                && line.contains(&format!("loss rx {none} %  tx {none} %"))
                && line.contains(&format!("underruns here {none}  bridge {none}"))
                && line.contains("apps row -"),
            "{line}"
        );
    }
}
