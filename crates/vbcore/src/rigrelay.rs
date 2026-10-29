//! The local rig control relay: WSJT-X, JS8Call and FLDIGI point their
//! "Hamlib NET rigctl" at 127.0.0.1:4532 and VectorCable carries each
//! connection to the bridge's rigctl proxy.
//!
//! Since bridge 1.10, under `rigctl_access` `all`, a client of 4532 off the
//! Pi's Wi-Fi (the tailnet) must first send `\vb_auth <app token>\n`
//! (vector-bridge DOCS/web-api.md, "The same token on port 4532"). The apps
//! cannot, so the relay does: it sends the line, keeps the bridge's `RPRT 0`
//! from the app, then copies bytes both ways. A 1.10 bridge answers `RPRT 0`
//! where the line is not needed, so it is sent whenever the bridge is known to
//! be 1.10 or later and an app token is held. Never to an older bridge: its
//! proxy passes the line to rigctld, which runs the token's letters as
//! commands (`b` is send_morse; BRIDGE, tested on the bench, 2026-10-14). The
//! caller's [`Source`] decides; with no token the connection is carried as it
//! is.
//!
//! Since 2026-10-28 every client of 4532 that is not on the Pi itself logs in,
//! whatever `rigctl_access` says (vector-bridge DOCS/web-api.md), so on such a
//! bridge the apps reach the radio only through this relay.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The bridge's rigctl proxy port, and the one the relay listens on unless
/// the user picks another.
pub const RIG_PORT: u16 = 4532;

/// Where the apps point their rig control: loopback only, so nothing off this
/// computer reaches the bridge through the relay.
pub fn listen_address(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// The bridge admits at most 8 apps on 4532; more would only be refused.
pub const MAX_CLIENTS: usize = 8;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The bridge waits 5 s for the first line; its answer to ours comes at once,
/// or within rigctld's own timeout for `RPRT -6`.
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(8);
/// Hamlib speaks first (`\chk_vfo`, `\dump_state`) as soon as it connects.
const FIRST_LINE_TIMEOUT: Duration = Duration::from_secs(10);
/// Longer than any rigctl command line.
const MAX_FIRST_LINE: usize = 1024;
const POLL: Duration = Duration::from_millis(50);
/// A silent rigctld (just starting, or the radio just switched on) gets one
/// more try after each wait; short, as the app is waiting on its first answer.
const SILENT_RETRIES: [Duration; 2] = [Duration::from_millis(500), Duration::from_secs(1)];

/// The relay as the Setup tab shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayState {
    Listening(SocketAddr),
    /// The port could not be bound (another program holds it, often a local
    /// rigctld). A bridge that takes `\vb_auth` then gives the apps no rig
    /// control until the port is freed or another one picked; an older one
    /// takes them straight.
    Failed {
        listen: SocketAddr,
        error: String,
    },
}

/// Where one connection goes, read when the app connects so a token from a
/// log-in a moment ago is used.
#[derive(Clone, PartialEq, Eq)]
pub struct Upstream {
    pub addr: SocketAddr,
    /// The app token to log in with: `Some` only for a bridge known to take
    /// `\vb_auth` (1.10 on).
    pub token: Option<String>,
    /// The bridge says it takes `\vb_auth`, so a refusal with no token held
    /// means the user has not typed the PIN yet.
    pub vb_auth: bool,
}

impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upstream")
            .field("addr", &self.addr)
            .field("token", &self.token.as_ref().map(|_| "<set>"))
            .field("vb_auth", &self.vb_auth)
            .finish()
    }
}

pub type Source = Arc<dyn Fn() -> Upstream + Send + Sync>;

/// Called when the bridge refuses the relay on 4532 (`RPRT -9`), so the
/// caller can check the token and ask for the PIN if it was revoked.
pub type Refused = Arc<dyn Fn() + Send + Sync>;

/// The bridge's answer to `\vb_auth`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// `RPRT 0`: carry on.
    Admitted,
    /// `RPRT -9`: a bad, revoked or missing token, or 8 apps already in.
    Refused,
    /// `RPRT -6`, or a close with no answer: rigctld is not answering.
    RigSilent,
    /// Any other line, which a 1.10 bridge never sends: the app is closed.
    Unexpected,
}

/// Reads the bridge's answer line (without its newline); `None` is a close or
/// a timeout before one.
pub fn admission(line: Option<&str>) -> Admission {
    match line.map(str::trim) {
        Some("RPRT 0") => Admission::Admitted,
        Some("RPRT -9") => Admission::Refused,
        Some("RPRT -6") | None => Admission::RigSilent,
        Some(_) => Admission::Unexpected,
    }
}

/// A web page can make the browser send an HTTP request to 127.0.0.1:4532;
/// with the token the bridge would take its lines as rig commands. No rigctl
/// command looks like this.
fn looks_like_http(line: &[u8]) -> bool {
    const METHODS: [&str; 9] = [
        "GET ", "POST ", "PUT ", "HEAD ", "OPTIONS ", "DELETE ", "PATCH ", "CONNECT ", "TRACE ",
    ];
    let text = String::from_utf8_lossy(line);
    METHODS.iter().any(|m| text.starts_with(m)) || text.contains(" HTTP/")
}

/// Same rule as the HTTP header: letters, digits, `-` and `_`, so the token
/// cannot end the line early or add a command.
fn sendable(token: &str) -> bool {
    (1..=256).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Announces a line once, until a different one replaces it, so an app
/// reconnecting every few seconds does not flood the user.
struct Notes {
    sender: Sender<String>,
    last: Mutex<Option<String>>,
}

impl Notes {
    fn say(&self, text: String) {
        let mut last = self.last.lock().unwrap();
        if last.as_deref() != Some(text.as_str()) {
            let _ = self.sender.send(text.clone());
            *last = Some(text);
        }
    }
}

/// Open connections, both sockets of each, so a stop can close them.
type Open = Arc<Mutex<HashMap<u64, (TcpStream, TcpStream)>>>;

/// One connection counted from its accept until its thread ends, so the cap
/// holds while connections are still logging in.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Binds the relay's port. Kept apart from [`spawn`] so the caller can show a
/// failure.
pub fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Accepts apps until `stop`, then closes every open connection.
pub fn spawn(
    listener: TcpListener,
    source: Source,
    refused: Refused,
    notes: Sender<String>,
    stop: Arc<AtomicBool>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("vbcore-rigrelay".into())
        .spawn(move || {
            let notes = Arc::new(Notes {
                sender: notes,
                last: Mutex::new(None),
            });
            let open: Open = Arc::default();
            let active = Arc::new(AtomicUsize::new(0));
            let next_id = AtomicU64::new(0);
            while !stop.load(Ordering::SeqCst) {
                let client = match listener.accept() {
                    Ok((client, _)) => client,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(POLL);
                        continue;
                    }
                    Err(_) => {
                        std::thread::sleep(POLL);
                        continue;
                    }
                };
                if active.load(Ordering::SeqCst) >= MAX_CLIENTS {
                    notes.say(format!(
                        "Rig control: {MAX_CLIENTS} apps are already connected through VectorCable; another was turned away."
                    ));
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                // Dropped with the closure if the thread does not start.
                let slot = Slot(active.clone());
                let id = next_id.fetch_add(1, Ordering::SeqCst);
                let (source, refused, notes, open, stop) = (
                    source.clone(),
                    refused.clone(),
                    notes.clone(),
                    open.clone(),
                    stop.clone(),
                );
                let _ = std::thread::Builder::new()
                    .name("vbcore-rigrelay-conn".into())
                    .spawn(move || {
                        let _slot = slot;
                        let link = Link {
                            source: &source,
                            refused: &refused,
                            notes: &notes,
                            stop: &stop,
                        };
                        serve(id, client, &link, &open)
                    });
            }
            for (_, (a, b)) in open.lock().unwrap().drain() {
                let _ = a.shutdown(Shutdown::Both);
                let _ = b.shutdown(Shutdown::Both);
            }
        })
}

/// Reads one line without its newline, a byte at a time so nothing past it is
/// taken from the stream. `None` on a close or a timeout; past `max` bytes, the
/// bytes so far (no answer or command is that long).
fn read_line(stream: &mut TcpStream, max: usize) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < max {
        match stream.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => return Some(line),
            Ok(1) => line.push(byte[0]),
            _ => return None,
        }
    }
    Some(line)
}

/// What each connection needs from the relay.
struct Link<'a> {
    source: &'a Source,
    refused: &'a Refused,
    notes: &'a Notes,
    stop: &'a AtomicBool,
}

fn serve(id: u64, mut client: TcpStream, link: &Link, open: &Open) {
    let (notes, stop) = (link.notes, link.stop);
    // On macOS an accepted socket inherits the listener's non-blocking flag.
    if client.set_nonblocking(false).is_err() {
        return;
    }
    let _ = client.set_nodelay(true);
    // The app's first line is read before anything reaches the bridge, so a
    // browser's request is turned away before the token is sent.
    let _ = client.set_read_timeout(Some(FIRST_LINE_TIMEOUT));
    let Some(first) = read_line(&mut client, MAX_FIRST_LINE) else {
        return;
    };
    if looks_like_http(&first) {
        notes.say("Rig control: turned away a web request to VectorCable's rig control port; only apps like WSJT-X may use it.".into());
        return;
    }
    let _ = client.set_read_timeout(None);
    let upstream = (link.source)();
    let token = upstream.token.as_deref().filter(|t| sendable(t));
    let bridge = match token {
        Some(token) => log_in(&upstream, token, link),
        None => connect(&upstream, notes),
    };
    let Some(mut bridge) = bridge else {
        return;
    };
    let mut first = first;
    first.push(b'\n');
    if bridge.write_all(&first).is_err() {
        return;
    }
    // A bridge that wants the token and got none refuses the app's first
    // line; Hamlib shows that only as a timeout, so say what to do.
    if upstream.vb_auth && token.is_none() {
        let _ = bridge.set_read_timeout(Some(ADMISSION_TIMEOUT));
        let reply = reply_start(&mut bridge);
        if reply == b"RPRT -9\n" {
            (link.refused)();
            notes.say("Rig control needs the bridge PIN: press p on the Status tab and type it, then try the app again (Test CAT in WSJT-X).".into());
        }
        if client.write_all(&reply).is_err() {
            return;
        }
        let _ = bridge.set_read_timeout(None);
    }
    let (Ok(client_w), Ok(bridge_w)) = (client.try_clone(), bridge.try_clone()) else {
        return;
    };
    let (Ok(c_reg), Ok(b_reg)) = (client.try_clone(), bridge.try_clone()) else {
        return;
    };
    open.lock().unwrap().insert(id, (c_reg, b_reg));
    // A stop between the accept and here would miss this connection.
    if stop.load(Ordering::SeqCst) {
        let _ = client.shutdown(Shutdown::Both);
        let _ = bridge.shutdown(Shutdown::Both);
    }
    let closed = Arc::new(AtomicBool::new(false));
    let quit = Arc::new(AtomicBool::new(is_quit(&first)));
    let (up_closed, up_quit) = (closed.clone(), quit.clone());
    let up = std::thread::Builder::new()
        .name("vbcore-rigrelay-up".into())
        .spawn(move || pipe(client, bridge_w, &up_closed, Some(&up_quit)));
    let bridge_closed_first = pipe(bridge, client_w, &closed, None);
    // The app closing is routine, and so is the bridge closing after the app's
    // quit; the bridge closing otherwise cuts the app's rig control, which
    // Hamlib shows only as an error, so say where it came from.
    if bridge_closed_first && !quit.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
        notes.say("Rig control: the bridge closed an app's connection, so that app has lost rig control; check the bridge, then reconnect the app (Test CAT in WSJT-X).".into());
    }
    if let Ok(t) = up {
        let _ = t.join();
    }
    open.lock().unwrap().remove(&id);
}

/// Opens the connection to the bridge's 4532, saying why when it cannot.
fn connect(upstream: &Upstream, notes: &Notes) -> Option<TcpStream> {
    match TcpStream::connect_timeout(&upstream.addr, CONNECT_TIMEOUT) {
        Ok(s) => {
            let _ = s.set_nodelay(true);
            Some(s)
        }
        Err(e) => {
            notes.say(format!(
                "Rig control: could not reach the bridge at {} ({e}).",
                upstream.addr
            ));
            None
        }
    }
}

/// Connects and sends `\vb_auth`; a silent rigctld is tried again after each
/// of [`SILENT_RETRIES`]. The answers and the waits between them fit in one
/// [`ADMISSION_TIMEOUT`]; only a slow reconnect (its own [`CONNECT_TIMEOUT`])
/// adds to it, so the app waits no longer than for a single try before.
/// `None` once the app is to be closed.
fn log_in(upstream: &Upstream, token: &str, link: &Link) -> Option<TcpStream> {
    let notes = link.notes;
    let deadline = Instant::now() + ADMISSION_TIMEOUT;
    let mut waits = SILENT_RETRIES.iter();
    loop {
        let mut bridge = connect(upstream, notes)?;
        let left = deadline.saturating_duration_since(Instant::now());
        let _ = bridge.set_read_timeout(Some(left.max(POLL)));
        let answer = bridge
            .write_all(format!("\\vb_auth {token}\n").as_bytes())
            .ok()
            .and_then(|_| read_line(&mut bridge, 64))
            .map(|l| String::from_utf8_lossy(&l).into_owned());
        match admission(answer.as_deref()) {
            Admission::Admitted => {
                notes.say(
                    "Rig control: an app is connected to the bridge through VectorCable.".into(),
                );
                let _ = bridge.set_read_timeout(None);
                return Some(bridge);
            }
            Admission::Refused => {
                (link.refused)();
                notes.say("Rig control: the bridge refused VectorCable's app token (revoked, PIN changed, or 8 apps already connected); VectorCable is checking it, and if the Status tab asks for the PIN, press p.".into());
                return None;
            }
            // A stop is no news; a try that timed out leaves no time for another.
            Admission::RigSilent if link.stop.load(Ordering::SeqCst) => return None,
            Admission::RigSilent => match waits.next() {
                Some(wait) if Instant::now() + *wait < deadline => std::thread::sleep(*wait),
                _ => {
                    notes.say("Rig control: the bridge's rigctld is not answering; check the radio is on and its CAT cable is in.".into());
                    return None;
                }
            },
            Admission::Unexpected => {
                notes.say(format!(
                    "Rig control: the bridge at {} gave an unexpected answer to VectorCable's log-in; the app was disconnected.",
                    upstream.addr
                ));
                return None;
            }
        }
    }
}

/// The start of the bridge's first answer: up to and including its first
/// newline, or whatever came before a close, a timeout or 64 bytes. All of it
/// still goes to the app.
fn reply_start(stream: &mut TcpStream) -> Vec<u8> {
    let mut reply = Vec::new();
    let mut byte = [0u8; 1];
    while reply.len() < 64 {
        match stream.read(&mut byte) {
            Ok(1) => {
                reply.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            _ => break,
        }
    }
    reply
}

/// Copies until either side closes, then closes both so the other direction
/// ends too. True when this direction ended first: `closed` is shared by both
/// directions and set before either closes anything. With `quit`, the app's
/// direction notes whether its last line was a quit.
fn pipe(
    mut from: TcpStream,
    mut to: TcpStream,
    closed: &AtomicBool,
    quit: Option<&AtomicBool>,
) -> bool {
    match quit {
        Some(q) => copy_noting_quit(&mut from, &mut to, q),
        None => {
            let _ = io::copy(&mut from, &mut to);
        }
    }
    let first = !closed.swap(true, Ordering::SeqCst);
    let _ = from.shutdown(Shutdown::Both);
    let _ = to.shutdown(Shutdown::Both);
    first
}

/// A rigctld quit: `q`, `Q` or `\quit`. Hamlib sends `q` as it closes, and
/// rigctld answers by closing.
fn is_quit(line: &[u8]) -> bool {
    matches!(String::from_utf8_lossy(line).trim(), "q" | "Q" | "\\quit")
}

/// Copies the app's bytes to the bridge, setting `quit` when the last whole
/// line was a quit. Set before the bytes go, so before the bridge's close.
fn copy_noting_quit(from: &mut TcpStream, to: &mut TcpStream, quit: &AtomicBool) {
    let mut buf = [0u8; 4096];
    let mut line = Vec::new();
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\n' {
                quit.store(is_quit(&line), Ordering::SeqCst);
                line.clear();
            } else if line.len() < 64 {
                line.push(b);
            }
        }
        if to.write_all(&buf[..n]).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc::{self, Receiver};
    use std::time::Instant;

    /// A bridge's 4532: records the first line, answers it with `first`
    /// (nothing when `None`), then answers `f` with a frequency.
    struct FakeRig {
        addr: SocketAddr,
        lines: Arc<Mutex<Vec<String>>>,
    }

    impl FakeRig {
        fn start(first: Option<&'static str>, close_after_first: bool) -> FakeRig {
            FakeRig::start_each(move |_| (first, close_after_first))
        }

        /// The answer to `\vb_auth`, and whether to close after it, picked by
        /// the connection's number from 0.
        fn start_each(
            pick: impl Fn(usize) -> (Option<&'static str>, bool) + Send + 'static,
        ) -> FakeRig {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let lines = Arc::new(Mutex::new(Vec::new()));
            let t_lines = lines.clone();
            std::thread::spawn(move || {
                for (n, stream) in listener.incoming().enumerate() {
                    let Ok(mut stream) = stream else { return };
                    let lines = t_lines.clone();
                    let (first, close_after_first) = pick(n);
                    std::thread::spawn(move || {
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut answered_first = false;
                        let mut line = String::new();
                        while reader.read_line(&mut line).unwrap_or(0) > 0 {
                            lines.lock().unwrap().push(line.trim_end().to_string());
                            if !answered_first && line.starts_with("\\vb_auth ") {
                                answered_first = true;
                                if let Some(a) = first {
                                    let _ = stream.write_all(format!("{a}\n").as_bytes());
                                }
                                if close_after_first {
                                    return;
                                }
                            } else if line.trim_end() == "f" {
                                let _ = stream.write_all(b"14074000\n");
                            }
                            line.clear();
                        }
                    });
                }
            });
            FakeRig { addr, lines }
        }

        fn lines(&self) -> Vec<String> {
            self.lines.lock().unwrap().clone()
        }
    }

    struct Running {
        addr: SocketAddr,
        notes: Receiver<String>,
        /// How often the relay reported a refusal.
        refusals: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                t.join().unwrap();
            }
        }
    }

    /// A token is only ever held for a bridge with the marker.
    fn relay(rig: &FakeRig, token: Option<&str>) -> Running {
        relay_to(rig.addr, token, token.is_some())
    }

    fn relay_to(rig: SocketAddr, token: Option<&str>, vb_auth: bool) -> Running {
        let listener = bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let upstream = Upstream {
            addr: rig,
            token: token.map(String::from),
            vb_auth,
        };
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let refusals = Arc::new(AtomicUsize::new(0));
        let counted = refusals.clone();
        let thread = spawn(
            listener,
            Arc::new(move || upstream.clone()),
            Arc::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
            }),
            tx,
            stop.clone(),
        )
        .unwrap();
        Running {
            addr,
            notes: rx,
            refusals,
            stop,
            thread: Some(thread),
        }
    }

    fn ask(app: &mut TcpStream, command: &str) -> Option<String> {
        app.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        app.write_all(format!("{command}\n").as_bytes()).unwrap();
        let mut reader = BufReader::new(app.try_clone().unwrap());
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(n) if n > 0 => Some(line.trim_end().to_string()),
            _ => None,
        }
    }

    fn note(r: &Running) -> String {
        r.notes.recv_timeout(Duration::from_secs(3)).unwrap()
    }

    #[test]
    fn the_answer_to_vb_auth_reads_as_the_bridge_contract_says() {
        assert_eq!(admission(Some("RPRT 0")), Admission::Admitted);
        assert_eq!(admission(Some("RPRT -9")), Admission::Refused);
        assert_eq!(admission(Some("RPRT -6\r")), Admission::RigSilent);
        assert_eq!(admission(None), Admission::RigSilent);
        assert_eq!(admission(Some("RPRT -1")), Admission::Unexpected);
    }

    /// The token line goes first, the bridge's `RPRT 0` never reaches the app,
    /// and the app's commands and the bridge's answers pass both ways.
    #[test]
    fn the_token_goes_first_and_the_admission_stays_with_the_relay() {
        let rig = FakeRig::start(Some("RPRT 0"), false);
        let r = relay(&rig, Some("app-token"));
        let mut app = TcpStream::connect(r.addr).unwrap();
        assert_eq!(ask(&mut app, "f").as_deref(), Some("14074000"));
        assert_eq!(rig.lines(), ["\\vb_auth app-token", "f"]);
        assert!(note(&r).contains("connected to the bridge"));
    }

    /// With no app token (before a log-in, or a bridge before 1.8) the app's
    /// bytes go through untouched.
    #[test]
    fn without_a_token_the_connection_is_carried_as_it_is() {
        let rig = FakeRig::start(None, false);
        let r = relay(&rig, None);
        let mut app = TcpStream::connect(r.addr).unwrap();
        assert_eq!(ask(&mut app, "f").as_deref(), Some("14074000"));
        assert_eq!(rig.lines(), ["f"]);
    }

    /// A token that could break the line is never sent.
    #[test]
    fn a_token_that_could_add_a_command_is_not_sent() {
        let rig = FakeRig::start(None, false);
        let r = relay(&rig, Some("x\nT 1"));
        let mut app = TcpStream::connect(r.addr).unwrap();
        assert_eq!(ask(&mut app, "f").as_deref(), Some("14074000"));
        assert_eq!(rig.lines(), ["f"]);
    }

    /// `RPRT -9`: the app is closed and the user told how to log in again.
    #[test]
    fn a_refused_token_closes_the_app_and_says_press_p() {
        let rig = FakeRig::start(Some("RPRT -9"), true);
        let r = relay(&rig, Some("old-token"));
        let mut app = TcpStream::connect(r.addr).unwrap();
        assert_eq!(ask(&mut app, "f"), None);
        assert!(note(&r).contains("press p"));
        assert_eq!(r.refusals.load(Ordering::SeqCst), 1, "the token is checked");
    }

    /// `RPRT -6`: rigctld on the bridge is silent; after the retries the user
    /// is pointed at the radio.
    #[test]
    fn a_silent_rigctld_closes_the_app_and_points_at_the_radio() {
        let rig = FakeRig::start(Some("RPRT -6"), true);
        let r = relay(&rig, Some("app-token"));
        let mut app = TcpStream::connect(r.addr).unwrap();
        app.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
        app.write_all(b"f\n").unwrap();
        let mut buf = [0u8; 8];
        assert!(
            matches!(app.read(&mut buf), Ok(0)),
            "closed after the retries"
        );
        assert!(note(&r).contains("radio is on"));
        let tries = SILENT_RETRIES.len() + 1;
        assert_eq!(rig.lines(), vec!["\\vb_auth app-token"; tries]);
        assert_eq!(r.refusals.load(Ordering::SeqCst), 0);
    }

    /// A bridge that takes the connection but never answers the log-in: one
    /// try, closed within one admission timeout, not one per retry.
    #[test]
    fn a_log_in_that_times_out_is_not_tried_again() {
        let rig = FakeRig::start(None, false);
        let r = relay(&rig, Some("app-token"));
        let mut app = TcpStream::connect(r.addr).unwrap();
        let started = Instant::now();
        app.set_read_timeout(Some(ADMISSION_TIMEOUT * 2)).unwrap();
        app.write_all(b"f\n").unwrap();
        let mut buf = [0u8; 8];
        assert!(matches!(app.read(&mut buf), Ok(0)));
        assert!(started.elapsed() < ADMISSION_TIMEOUT + Duration::from_secs(1));
        assert_eq!(rig.lines(), ["\\vb_auth app-token"]);
        assert!(note(&r).contains("radio is on"));
    }

    /// rigctld coming up while the app waits: a retry logs in and the app's
    /// first command gets its answer, with nothing said about the silence.
    #[test]
    fn a_rigctld_that_wakes_during_the_retries_answers_the_app() {
        let rig = FakeRig::start_each(|n| match n {
            0 => (Some("RPRT -6"), true),
            _ => (Some("RPRT 0"), false),
        });
        let r = relay(&rig, Some("app-token"));
        let mut app = TcpStream::connect(r.addr).unwrap();
        app.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
        assert_eq!(ask(&mut app, "f").as_deref(), Some("14074000"));
        assert_eq!(
            rig.lines(),
            ["\\vb_auth app-token", "\\vb_auth app-token", "f"]
        );
        assert!(note(&r).contains("connected to the bridge"));
    }

    /// The bridge closing an admitted connection is said, so the app's rig
    /// control error has a cause on screen; an app closing is not news.
    #[test]
    fn the_bridge_closing_a_connection_is_said_and_an_app_closing_is_not() {
        let rig = FakeRig::start(Some("RPRT 0"), false);
        let r = relay(&rig, Some("app-token"));
        let mut app = TcpStream::connect(r.addr).unwrap();
        assert_eq!(ask(&mut app, "f").as_deref(), Some("14074000"));
        assert!(note(&r).contains("connected to the bridge"));
        drop(app);
        assert!(
            r.notes.recv_timeout(Duration::from_millis(500)).is_err(),
            "an app closing is not news"
        );

        let r = relay_to(one_command_rig(), Some("app-token"), true);
        let mut app = TcpStream::connect(r.addr).unwrap();
        assert_eq!(ask(&mut app, "f").as_deref(), Some("14074000"));
        assert!(note(&r).contains("connected to the bridge"));
        assert!(note(&r).contains("the bridge closed"));

        // Hamlib's `q` as it closes: rigctld closes first, and that is routine.
        let r = relay_to(one_command_rig(), Some("app-token"), true);
        let mut app = TcpStream::connect(r.addr).unwrap();
        assert_eq!(ask(&mut app, "q"), None);
        assert!(note(&r).contains("connected to the bridge"));
        assert!(
            r.notes.recv_timeout(Duration::from_millis(500)).is_err(),
            "a close after the app's quit is not news"
        );
    }

    #[test]
    fn a_quit_reads_in_each_spelling_and_nothing_else_does() {
        assert!(is_quit(b"q"));
        assert!(is_quit(b"Q\r"));
        assert!(is_quit(b"\\quit"));
        assert!(!is_quit(b"f"));
        assert!(!is_quit(b"quiet"));
    }

    /// A bridge that admits the token, answers one command (`f` with a
    /// frequency, `q` with nothing) and closes.
    fn one_command_rig() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let rig = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            writer.write_all(b"RPRT 0\n").unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line.trim() == "f" {
                writer.write_all(b"14074000\n").unwrap();
            }
        });
        rig
    }

    /// A 1.10 bridge whose rigctld is down closes with no answer (BRIDGE,
    /// 2026-10-14): that reads as rigctld not answering.
    #[test]
    fn a_close_with_no_answer_points_at_the_radio() {
        let rig = FakeRig::start(None, true);
        let r = relay(&rig, Some("app-token"));
        let mut app = TcpStream::connect(r.addr).unwrap();
        assert_eq!(ask(&mut app, "f"), None);
        assert!(note(&r).contains("radio is on"));
    }

    /// Any other answer closes the app; nothing it sends reaches the bridge.
    #[test]
    fn an_unexpected_answer_closes_the_app() {
        let rig = FakeRig::start(Some("RPRT -1"), false);
        let r = relay(&rig, Some("app-token"));
        let mut app = TcpStream::connect(r.addr).unwrap();
        assert_eq!(ask(&mut app, "f"), None);
        assert!(note(&r).contains("unexpected answer"));
        assert_eq!(rig.lines(), ["\\vb_auth app-token"]);
    }

    /// The same failure is announced once, not on every reconnect.
    #[test]
    fn a_repeated_failure_is_announced_once() {
        let rig = FakeRig::start(Some("RPRT -9"), true);
        let r = relay(&rig, Some("old-token"));
        for _ in 0..3 {
            let mut app = TcpStream::connect(r.addr).unwrap();
            assert_eq!(ask(&mut app, "f"), None);
        }
        assert!(note(&r).contains("press p"));
        assert!(r.notes.recv_timeout(Duration::from_millis(300)).is_err());
    }

    /// A browser's request is turned away before anything reaches the bridge.
    #[test]
    fn a_web_request_is_turned_away_before_the_bridge_is_reached() {
        let rig = FakeRig::start(Some("RPRT 0"), false);
        let r = relay(&rig, Some("app-token"));
        let mut app = TcpStream::connect(r.addr).unwrap();
        assert_eq!(ask(&mut app, "POST / HTTP/1.1\r"), None);
        assert!(note(&r).contains("web request"));
        assert!(rig.lines().is_empty());
        assert!(looks_like_http(b"GET /x HTTP/1.1\r"));
        assert!(!looks_like_http(b"\\chk_vfo"));
        assert!(!looks_like_http(b"T 1"));
    }

    /// The cap counts connections still logging in, so a burst cannot pass it.
    #[test]
    fn a_ninth_app_is_turned_away_while_eight_are_connecting() {
        let rig = FakeRig::start(Some("RPRT 0"), false);
        let r = relay(&rig, Some("app-token"));
        // Eight apps that have not sent their first line yet.
        let waiting: Vec<TcpStream> = (0..MAX_CLIENTS)
            .map(|_| TcpStream::connect(r.addr).unwrap())
            .collect();
        let mut ninth = TcpStream::connect(r.addr).unwrap();
        assert!(note(&r).contains("turned away"));
        assert_eq!(ask(&mut ninth, "f"), None);
        drop(waiting);
        // Their slots come back once they close.
        let started = Instant::now();
        loop {
            let mut app = TcpStream::connect(r.addr).unwrap();
            if ask(&mut app, "f").as_deref() == Some("14074000") {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(3));
        }
    }

    /// An answer longer than any `RPRT` line is unexpected, not silence.
    #[test]
    fn an_overlong_answer_is_unexpected() {
        let rig = FakeRig::start(Some(&*"x".repeat(100).leak()), true);
        let r = relay(&rig, Some("app-token"));
        let mut app = TcpStream::connect(r.addr).unwrap();
        assert_eq!(ask(&mut app, "f"), None);
        assert!(note(&r).contains("unexpected answer"));
    }

    /// A 1.10 bridge on the tailnet: a first line other than `\vb_auth` gets
    /// `RPRT -9` and a close.
    fn refusing_rig() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut line = String::new();
                let _ = BufReader::new(stream.try_clone().unwrap()).read_line(&mut line);
                let _ = stream.write_all(b"RPRT -9\n");
            }
        });
        addr
    }

    /// With the marker but no token (the PIN not typed yet) the bridge refuses
    /// the app: the user is told to press p, and the app still gets the refusal.
    #[test]
    fn a_refusal_with_no_token_says_press_p() {
        let r = relay_to(refusing_rig(), None, true);
        let mut app = TcpStream::connect(r.addr).unwrap();
        assert_eq!(ask(&mut app, "\\chk_vfo").as_deref(), Some("RPRT -9"));
        assert!(note(&r).contains("needs the bridge PIN: press p"));
        assert_eq!(r.refusals.load(Ordering::SeqCst), 1);
    }

    /// With the marker but no token, a bridge that needs no log-in (its own
    /// Wi-Fi) answers as usual and nothing is said.
    #[test]
    fn no_token_where_none_is_needed_goes_through_quietly() {
        let rig = FakeRig::start(None, false);
        let r = relay_to(rig.addr, None, true);
        let mut app = TcpStream::connect(r.addr).unwrap();
        assert_eq!(ask(&mut app, "f").as_deref(), Some("14074000"));
        assert_eq!(ask(&mut app, "f").as_deref(), Some("14074000"));
        assert_eq!(rig.lines(), ["f", "f"]);
        assert!(r.notes.recv_timeout(Duration::from_millis(300)).is_err());
    }

    /// A stop closes connections that are open.
    #[test]
    fn a_stop_closes_open_connections() {
        let rig = FakeRig::start(Some("RPRT 0"), false);
        let r = relay(&rig, Some("app-token"));
        let mut app = TcpStream::connect(r.addr).unwrap();
        assert_eq!(ask(&mut app, "f").as_deref(), Some("14074000"));
        drop(r);
        let started = Instant::now();
        app.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 8];
        assert!(matches!(app.read(&mut buf), Ok(0) | Err(_)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
