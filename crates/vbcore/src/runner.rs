//! Runs a [`Session`] over real UDP sockets on a background thread, moving
//! audio between the bridge and an [`AudioAdapter`]. The TUI and the CLI
//! read its [`Snapshot`]; tests run it against a fake bridge on loopback.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::assembler::{RxAssembler, Stats as RxStats};
use crate::audio::{peak_dbfs, AudioAdapter, MAX_FRAMES_PER_CALL};
use crate::clockcheck::ClockEstimate;
use crate::discovery::resolve;
use crate::http::{self, HttpOutcome};
use crate::pin::{self, Auth, Authed, TokenStore};
use crate::playback::{PlaybackStats, Timeline};
use crate::protocol::{Direction, HelloReply, Packet, Reply, Request};
use crate::rigrelay::{self, RelayState, Upstream};
use crate::session::{Action, Event, Link, Mode, Session, Socket};
use crate::status::{BridgeStatus, BridgeVersion, LadderState, RigctlState};
use crate::time::Clock;
use crate::tx::TxStats;

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub control_port: u16,
    pub audio_port: u16,
    /// `None` skips `/api/ladder`, `/api/rigctl` and the footer scrape.
    pub http_port: Option<u16>,
    pub client_name: String,
    pub mode: Mode,
    /// False streams silence too (`soak`).
    pub gated: bool,
    /// Local address to bind both sockets on.
    pub bind: IpAddr,
    /// The app-listing description (`POST /api/apps`), or `None` to post nothing.
    pub listing: Option<AppListing>,
    /// Where a bridge's app token is kept between runs, or `None` to hold it
    /// in memory only.
    pub tokens: Option<Arc<dyn TokenStore>>,
    /// Where the local rig control relay listens ([`rigrelay`]), or `None`
    /// for no relay.
    pub rig_relay: Option<SocketAddr>,
    /// The bridge's rigctl proxy port until `/api/rigctl` names one.
    pub rig_port: u16,
    /// Turns `host` into addresses: at start, and again while the link is down,
    /// since a name can move to a new address (a new DHCP lease).
    pub resolve: fn(&str) -> std::io::Result<Vec<IpAddr>>,
}

/// How often a name is looked up again while the link is down.
const RESOLVE_INTERVAL: Duration = Duration::from_secs(2);

/// The first address for `host` in the family of the local `bind` address.
fn pick_address(
    resolve: fn(&str) -> std::io::Result<Vec<IpAddr>>,
    host: &str,
    bind: IpAddr,
) -> std::io::Result<IpAddr> {
    resolve(host)?
        .into_iter()
        .find(|a| a.is_ipv4() == bind.is_ipv4())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no address for {host}"),
            )
        })
}

/// Looks the bridge's name up again while the link is down, every
/// [`RESOLVE_INTERVAL`], off the runner's thread (an mDNS lookup can take seconds).
struct Lookup {
    running: Option<JoinHandle<Option<IpAddr>>>,
    last: Instant,
}

impl Lookup {
    fn new() -> Lookup {
        Lookup {
            running: None,
            last: Instant::now(),
        }
    }

    /// A finished lookup's address, unless the link came back meanwhile.
    fn poll(&mut self, config: &Config, link: Link) -> Option<IpAddr> {
        let down = matches!(link, Link::Searching | Link::Lost);
        if let Some(running) = &self.running {
            if !running.is_finished() {
                return None;
            }
            let ip = self.running.take()?.join().ok().flatten();
            return ip.filter(|_| down);
        }
        if !down || config.host.parse::<IpAddr>().is_ok() || self.last.elapsed() < RESOLVE_INTERVAL
        {
            return None;
        }
        self.last = Instant::now();
        let (resolve, host, bind) = (config.resolve, config.host.clone(), config.bind);
        self.running = std::thread::Builder::new()
            .name("vbcore-resolve".into())
            .spawn(move || pick_address(resolve, &host, bind).ok())
            .ok();
        None
    }
}

/// What the bridge's Apps card shows for us (vector-bridge DOCS/app-listing.md).
/// `app_id` must equal the UDP `client` string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppListing {
    pub app_id: String,
    pub name: String,
    pub note: String,
}

impl AppListing {
    pub fn vectorcable() -> AppListing {
        AppListing {
            app_id: "vectorcable".into(),
            name: "VectorCable".into(),
            note: "Audio cable for WSJT-X, JS8Call and FLDIGI".into(),
        }
    }

    /// The POST body. `launch_url` is the bridge's own /downloads/ page at the
    /// literal address we post to (DOCS/app-listing.md on the bridge); no icon.
    pub fn body(&self, host: &str) -> String {
        serde_json::json!({
            "app_id": self.app_id,
            "name": self.name,
            "note": self.note,
            "launch_url": format!("http://{host}/downloads/"),
        })
        .to_string()
    }
}

/// How the last `POST /api/apps` went. Only `Failed` is worth a word to the
/// user; the rest are normal states of a bridge (DOCS/app-listing.md).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListingState {
    /// 200 `listed:true`: the Apps row shows.
    Listed,
    /// 200 `listed:false`: stored; the row appears when our stream is live.
    Stored,
    /// 403 `wifi_open`: the access point has no passphrase yet.
    WifiOpen,
    /// 404: a bridge without the app listing.
    NotSupported,
    /// 401 `pin_required` and no PIN to log in with.
    PinNeeded,
    /// The log-in refused the PIN (`bad_pin`); it is forgotten.
    BadPin,
    /// 429 `locked`: too many wrong PINs; seconds to wait.
    Locked(u64),
    /// The card has no admin PIN yet, so it refuses every POST.
    PinUnset,
    /// Timed out or refused: the bridge answers UDP but not TCP 80.
    Failed(String),
}

impl ListingState {
    pub fn parse(outcome: &HttpOutcome) -> ListingState {
        match outcome {
            HttpOutcome::Response { status: 200, body } => {
                let listed = serde_json::from_str::<serde_json::Value>(body)
                    .ok()
                    .and_then(|v| v.get("listed").and_then(|l| l.as_bool()));
                if listed == Some(true) {
                    ListingState::Listed
                } else {
                    ListingState::Stored
                }
            }
            HttpOutcome::Response { status: 403, body } if body.contains("wifi_open") => {
                ListingState::WifiOpen
            }
            HttpOutcome::Response { status: 404, .. } => ListingState::NotSupported,
            HttpOutcome::Response { status, body } => {
                ListingState::Failed(format!("HTTP {status}: {}", body.trim()))
            }
            HttpOutcome::ConnectionDropped => ListingState::Failed("connection dropped".into()),
            HttpOutcome::Unreachable(e) => ListingState::Failed(e.clone()),
        }
    }

    pub fn word(&self) -> String {
        match self {
            ListingState::Listed => "listed".into(),
            ListingState::Stored => "stored, not live yet".into(),
            ListingState::WifiOpen => "waiting: Wi-Fi has no passphrase".into(),
            ListingState::NotSupported => "not supported by this bridge".into(),
            ListingState::PinNeeded => "needs the bridge PIN".into(),
            ListingState::BadPin => "wrong bridge PIN".into(),
            ListingState::Locked(s) => format!("bridge PIN locked, retry in {s} s"),
            ListingState::PinUnset => "set a PIN on the bridge web page".into(),
            ListingState::Failed(e) => e.clone(),
        }
    }

    /// The user can fix this one by typing the bridge PIN.
    pub fn wants_pin(&self) -> bool {
        matches!(self, ListingState::PinNeeded | ListingState::BadPin)
    }
}

impl Config {
    pub fn new(host: &str) -> Config {
        Config {
            host: host.to_string(),
            control_port: crate::protocol::CONTROL_PORT,
            audio_port: crate::protocol::AUDIO_PORT,
            http_port: Some(80),
            client_name: "vectorcable".to_string(),
            mode: Mode::ListenOnly,
            gated: true,
            bind: IpAddr::from([0, 0, 0, 0]),
            listing: Some(AppListing::vectorcable()),
            tokens: None,
            rig_relay: None,
            rig_port: rigrelay::RIG_PORT,
            resolve,
        }
    }
}

/// Everything the UI shows, copied out under a lock.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub bridge: SocketAddr,
    pub local_audio_port: u16,
    pub mode: Mode,
    pub link: Link,
    pub hello: Option<HelloReply>,
    pub status: Option<BridgeStatus>,
    pub ladder: Option<LadderState>,
    pub rigctl: Option<RigctlState>,
    pub version: Option<BridgeVersion>,
    pub rx: RxStats,
    pub playback: PlaybackStats,
    pub tx: TxStats,
    pub rx_dbfs: Option<f64>,
    pub tx_dbfs: Option<f64>,
    pub buffer_ms: Option<f64>,
    pub trim_ppm: f64,
    pub clock: Option<ClockEstimate>,
    pub streaming: bool,
    pub finished: bool,
    /// The last app-listing POST, once one was made.
    pub listing: Option<ListingState>,
    /// The audio adapter's name and its fault, if any.
    pub audio_name: String,
    pub audio_fault: Option<String>,
    /// `status.peer` names an address that is not one of ours.
    pub slot_held_by_other: bool,
    /// The local rig control relay, `None` when not asked for.
    pub rig_relay: Option<RelayState>,
    /// This machine's address on the route to the bridge, checked again every few
    /// seconds while the link is down (a stopped VPN moves the route off it).
    pub local_ip: Option<IpAddr>,
}

impl Snapshot {
    fn new(bridge: SocketAddr, local_audio_port: u16, mode: Mode, audio_name: String) -> Snapshot {
        Snapshot {
            bridge,
            local_audio_port,
            mode,
            link: Link::Searching,
            hello: None,
            status: None,
            ladder: None,
            rigctl: None,
            version: None,
            rx: RxStats::default(),
            playback: PlaybackStats::default(),
            tx: TxStats::default(),
            rx_dbfs: None,
            tx_dbfs: None,
            buffer_ms: None,
            trim_ppm: 0.0,
            clock: None,
            streaming: false,
            finished: false,
            listing: None,
            audio_name,
            audio_fault: None,
            slot_held_by_other: false,
            rig_relay: None,
            local_ip: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    SetMode(Mode),
    Sleep,
    Wake,
    /// Mixer levels, percent 0 to 100: `POST /api/gain` with the session token,
    /// or UDP `set_gain` from the control socket on a bridge without it.
    SetGain {
        tx: Option<u8>,
        rx: Option<u8>,
    },
    /// The transmit audio cap, 0 disables it: `POST /api/tx_audio_cap`, or UDP
    /// `set_tx_audio_cap` on a bridge without it.
    SetTxAudioCap {
        seconds: u16,
    },
    /// A settings POST answered (internal). `moves` here and below is
    /// `Auth.moves` when the request began; a request older than the
    /// bridge's last move is dropped.
    Setting {
        request: Request,
        answer: Authed,
        moves: u64,
    },
    /// `POST /api/apps` answered, with a warning for the user if any (internal).
    Listing(ListingState, Option<String>, u64),
    /// The bridge's admin PIN, typed by the user: log in with it once and
    /// re-post the listing. Only the token is kept.
    SetPin(String),
    /// The bridge's `/api/ladder` and friends arrived (internal).
    Http {
        ladder: LadderState,
        rigctl: RigctlState,
        version: Option<BridgeVersion>,
        moves: u64,
    },
    /// A clock estimate from the WSJT-X listener (fed by the app).
    Clock(Option<ClockEstimate>),
    /// The bridge refused the rig control relay's app token, or its missing
    /// one, on 4532 (internal).
    RigTokenRefused,
}

/// Result of a `SetMode` request, delivered through `Runner::replies`.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandReply {
    ModeSet(Mode),
    Refused(String),
}

pub struct Runner {
    stop: Arc<AtomicBool>,
    snapshot: Arc<Mutex<Snapshot>>,
    commands: Sender<Command>,
    pub announcements: Receiver<String>,
    pub replies: Receiver<CommandReply>,
    thread: Option<JoinHandle<()>>,
    relay: Option<JoinHandle<()>>,
}

pub const TICK: Duration = Duration::from_millis(10);

impl Runner {
    /// Resolves the host, binds the sockets and starts the loop.
    pub fn spawn(config: Config, audio: Box<dyn AudioAdapter>) -> std::io::Result<Runner> {
        let ip = pick_address(config.resolve, &config.host, config.bind)?;
        let control = UdpSocket::bind(SocketAddr::new(config.bind, 0))?;
        let audio_socket = UdpSocket::bind(SocketAddr::new(config.bind, 0))?;
        control.set_nonblocking(true)?;
        audio_socket.set_nonblocking(true)?;
        let local_audio_port = audio_socket.local_addr()?.port();
        let audio_name = audio.name().to_string();
        let snapshot = Arc::new(Mutex::new(Snapshot::new(
            SocketAddr::new(ip, config.control_port),
            local_audio_port,
            config.mode,
            audio_name,
        )));
        let stop = Arc::new(AtomicBool::new(false));
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ann_tx, ann_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let auth = Arc::new(Mutex::new(Auth::default()));
        let relay = config.rig_relay.and_then(|listen| {
            let source = relay_source(config.rig_port, snapshot.clone(), auth.clone());
            let refused_tx = cmd_tx.clone();
            let refused: rigrelay::Refused = Arc::new(move || {
                let _ = refused_tx.send(Command::RigTokenRefused);
            });
            let started = rigrelay::bind(listen).and_then(|listener| {
                let bound = listener.local_addr()?;
                let thread =
                    rigrelay::spawn(listener, source, refused, ann_tx.clone(), stop.clone())?;
                Ok((bound, thread))
            });
            let (state, thread) = match started {
                Ok((bound, thread)) => (RelayState::Listening(bound), Some(thread)),
                Err(e) => (
                    RelayState::Failed {
                        listen,
                        error: e.to_string(),
                    },
                    None,
                ),
            };
            snapshot.lock().unwrap().rig_relay = Some(state);
            thread
        });
        let worker = Worker {
            config,
            ip,
            control,
            audio_socket,
            audio,
            stop: stop.clone(),
            snapshot: snapshot.clone(),
            commands: cmd_rx,
            command_sender: cmd_tx.clone(),
            announcements: ann_tx,
            replies: reply_tx,
            auth,
            pin: None,
            listing_in_flight: false,
            listing_again: false,
            rig_token_checked: None,
            lookup: Lookup::new(),
            last_route: Instant::now(),
        };
        let thread = std::thread::Builder::new()
            .name("vbcore-runner".into())
            .spawn(move || worker.run())?;
        Ok(Runner {
            stop,
            snapshot,
            commands: cmd_tx,
            announcements: ann_rx,
            replies: reply_rx,
            thread: Some(thread),
            relay,
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap().clone()
    }

    pub fn command(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// Stops TX at once, sends `bye` when advertised, and joins the thread.
    pub fn stop(mut self) -> Snapshot {
        self.join();
        self.snapshot()
    }

    fn join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.relay.take() {
            let _ = t.join();
        }
    }
}

/// The local IP the kernel would use to reach `remote`, found by connecting
/// a throwaway UDP socket (no datagram is sent).
fn local_address_towards(remote: &SocketAddr) -> Option<IpAddr> {
    let bind: IpAddr = if remote.is_ipv4() {
        IpAddr::from([0, 0, 0, 0])
    } else {
        IpAddr::from([0u16; 8])
    };
    let probe = UdpSocket::bind(SocketAddr::new(bind, 0)).ok()?;
    probe.connect(remote).ok()?;
    Some(probe.local_addr().ok()?.ip())
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.join();
    }
}

/// Points the relay and the UI at the bridge's new address. A token goes only
/// to the bridge that issued it, so the relay sends none until the HTTP calls
/// have read the `device_id` there and loaded the token stored for that bridge;
/// answers to requests made before the move are dropped (`Auth.moves`).
fn point_at(
    snapshot: &Mutex<Snapshot>,
    auth: &Mutex<Auth>,
    bridge: SocketAddr,
    local_ip: Option<IpAddr>,
) {
    auth.lock().unwrap().bridge_moved();
    let mut s = snapshot.lock().unwrap();
    s.bridge = bridge;
    s.local_ip = local_ip;
    s.rigctl = None;
}

/// Where the relay takes each app: the bridge's current address and rigctl proxy
/// port, and the app token only when `/api/rigctl` says the proxy takes
/// `\vb_auth` (an older one runs the line as rig commands) and the token is a 1.8
/// app token, issued with a `device_id` (a session token is refused on 4532).
fn relay_source(
    rig_port: u16,
    snapshot: Arc<Mutex<Snapshot>>,
    auth: Arc<Mutex<Auth>>,
) -> rigrelay::Source {
    Arc::new(move || {
        let (ip, port, vb_auth) = {
            let snap = snapshot.lock().unwrap();
            let ip = snap.bridge.ip();
            match &snap.rigctl {
                Some(RigctlState::Proxy { port, vb_auth, .. }) => {
                    (ip, port.unwrap_or(rig_port), *vb_auth)
                }
                _ => (ip, rig_port, false),
            }
        };
        let token = if vb_auth {
            let a = auth.lock().unwrap();
            a.device_id.as_ref().and(a.token.clone())
        } else {
            None
        };
        Upstream {
            addr: SocketAddr::new(ip, port),
            token,
            vb_auth,
        }
    })
}

struct Worker {
    config: Config,
    ip: IpAddr,
    control: UdpSocket,
    audio_socket: UdpSocket,
    audio: Box<dyn AudioAdapter>,
    stop: Arc<AtomicBool>,
    snapshot: Arc<Mutex<Snapshot>>,
    commands: Receiver<Command>,
    command_sender: Sender<Command>,
    announcements: Sender<String>,
    replies: Sender<CommandReply>,
    auth: Arc<Mutex<Auth>>,
    /// A PIN the user typed, handed to the next listing POST and gone with it.
    pin: Option<String>,
    /// One listing POST at a time, so one wrong PIN counts once toward the lockout.
    listing_in_flight: bool,
    /// A PIN arrived while a POST ran: drop that result and post again.
    listing_again: bool,
    /// When a refusal on 4532 last had the token checked.
    rig_token_checked: Option<Instant>,
    lookup: Lookup,
    last_route: Instant,
}

/// A refusal on 4532 checks the token at most this often.
const RIG_TOKEN_CHECK_EVERY: Duration = Duration::from_secs(30);

/// The HTTP form of a settings request: its path and JSON body.
fn setting_post(request: &Request) -> Option<(&'static str, String)> {
    match request {
        Request::SetGain { tx, rx } => {
            let mut m = serde_json::Map::new();
            if let Some(t) = tx {
                m.insert("tx".into(), (*t).into());
            }
            if let Some(r) = rx {
                m.insert("rx".into(), (*r).into());
            }
            Some(("/api/gain", serde_json::Value::Object(m).to_string()))
        }
        Request::SetTxAudioCap { seconds } => Some((
            "/api/tx_audio_cap",
            serde_json::json!({ "seconds": seconds }).to_string(),
        )),
        _ => None,
    }
}

/// A settings POST's answer as the control reply the session reads.
/// `/api/gain` answers `tx` and `rx`; the session reads the status names.
fn setting_reply(status: u16, body: &str) -> Reply {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    if status == 200 {
        let mut m = serde_json::Map::new();
        for (from, to) in [
            ("tx", "tx_gain"),
            ("rx", "rx_gain"),
            ("tx_audio_cap_s", "tx_audio_cap_s"),
        ] {
            if let Some(x) = v.get(from) {
                m.insert(to.into(), x.clone());
            }
        }
        return Reply::Ok(serde_json::Value::Object(m));
    }
    Reply::Error(
        v.get("error")
            .and_then(|e| e.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("HTTP {status}")),
    )
}

impl Worker {
    fn run(mut self) {
        let clock = Clock::new();
        let mut control_addr = SocketAddr::new(self.ip, self.config.control_port);
        let mut audio_addr = SocketAddr::new(self.ip, self.config.audio_port);
        // Our sockets' addresses as the bridge sees them, for recognising `status.peer`.
        let local_ip = self.local_ip(&audio_addr);
        self.snapshot.lock().unwrap().local_ip = local_ip;
        let local_audio_addr = self.local_addr(local_ip, &self.audio_socket);
        let local_control_addr = self.local_addr(local_ip, &self.control);
        let mut session = Session::new(
            &self.config.client_name,
            self.config.gated,
            local_audio_addr,
            local_control_addr,
        );
        let mut assembler = RxAssembler::default();
        let mut timeline = Timeline::default();
        let mut tx_block = vec![0i16; MAX_FRAMES_PER_CALL];
        let mut rx_block: Vec<i16> = Vec::with_capacity(MAX_FRAMES_PER_CALL);
        let mut buf = vec![0u8; 2048];
        let mut http_in_flight = false;
        let mut rx_dbfs = None;
        let mut clock_estimate = None;

        // Transmit at start-up waits for a first status so the slot rule applies;
        // the decision arrives as Action::ModeDecided and is reported like SetMode.
        let actions = session.start(self.config.mode, clock.now());
        self.perform(
            &mut session,
            actions,
            &control_addr,
            &audio_addr,
            &mut http_in_flight,
        );

        let start = Instant::now();
        let mut ticks: u64 = 0;
        while !self.stop.load(Ordering::SeqCst) {
            let now = clock.now();
            let mut actions = Vec::new();

            // Commands from the UI.
            loop {
                match self.commands.try_recv() {
                    Ok(Command::SetMode(mode)) => match session.request_mode(mode, now) {
                        Ok(a) => {
                            actions.extend(a);
                            let _ = self.replies.send(CommandReply::ModeSet(mode));
                        }
                        Err(r) => {
                            let _ = self.replies.send(CommandReply::Refused(r.0.clone()));
                            actions.push(Action::Announce(r.0));
                        }
                    },
                    Ok(Command::Sleep) => actions.extend(session.handle(Event::Sleep, now)),
                    Ok(Command::SetGain { tx, rx }) => {
                        self.change_setting(Request::SetGain { tx, rx }, &mut actions)
                    }
                    Ok(Command::SetTxAudioCap { seconds }) => {
                        self.change_setting(Request::SetTxAudioCap { seconds }, &mut actions)
                    }
                    Ok(Command::Setting { moves, .. }) if moves != self.moves() => {
                        actions.push(Action::Announce(
                            "The bridge moved before it answered; make the change again.".into(),
                        ))
                    }
                    Ok(Command::Setting {
                        request, answer, ..
                    }) => match answer {
                        Authed::PinRequired { pin_set } => {
                            actions.push(Action::Announce(
                                if pin_set {
                                    "The bridge needs its PIN for that change: press p, enter \
                                     it, then make the change again."
                                } else {
                                    "The bridge has no PIN yet: set one on its web page, then \
                                     make the change again."
                                }
                                .into(),
                            ));
                            // Brings back "press p" on the Status tab (or says the
                            // card has no PIN).
                            actions.push(Action::PostListing);
                        }
                        // A bridge from before `/api/tx_audio_cap` still takes UDP.
                        Authed::Answered(HttpOutcome::Response { status: 404, .. }) => actions
                            .push(Action::Control {
                                from: Socket::Control,
                                request,
                            }),
                        Authed::Answered(HttpOutcome::Response { status, body }) => {
                            let reply = setting_reply(status, &body);
                            actions.extend(session.handle(
                                Event::ControlReply {
                                    from: Socket::Control,
                                    reply,
                                },
                                now,
                            ));
                        }
                        Authed::Answered(HttpOutcome::ConnectionDropped) => actions.push(
                            Action::Announce("The bridge dropped the change; try again.".into()),
                        ),
                        Authed::Answered(HttpOutcome::Unreachable(e)) => actions.push(
                            Action::Announce(format!("The bridge did not get the change: {e}.")),
                        ),
                    },
                    Ok(Command::Listing(state, warning, moves)) => {
                        self.listing_in_flight = false;
                        actions.extend(warning.map(Action::Announce));
                        // A listing for the old address is posted again at the new one.
                        if self.listing_again || moves != self.moves() {
                            self.listing_again = false;
                            actions.push(Action::PostListing);
                        } else {
                            self.snapshot.lock().unwrap().listing = Some(state);
                        }
                    }
                    Ok(Command::SetPin(p)) => {
                        let can_post =
                            self.config.http_port.is_some() && self.config.listing.is_some();
                        if can_post && pin::valid_pin(&p) {
                            self.pin = Some(p);
                            self.auth.lock().unwrap().token = None;
                            self.snapshot.lock().unwrap().listing = None;
                            if self.listing_in_flight {
                                self.listing_again = true;
                            } else {
                                actions.push(Action::PostListing);
                            }
                        }
                    }
                    Ok(Command::RigTokenRefused) => {
                        // Posting the listing again checks the token: a revoked one is
                        // forgotten and the PIN asked for; a good one (8 apps already
                        // connected) changes nothing. Once a while, as apps retry.
                        let asking = {
                            let snap = self.snapshot.lock().unwrap();
                            snap.listing.as_ref().is_some_and(|l| l.wants_pin())
                        };
                        let due = self
                            .rig_token_checked
                            .is_none_or(|at| at.elapsed() >= RIG_TOKEN_CHECK_EVERY);
                        if !asking && due && !self.listing_in_flight {
                            self.rig_token_checked = Some(Instant::now());
                            actions.push(Action::PostListing);
                        }
                    }
                    Ok(Command::Wake) => actions.extend(session.handle(Event::Wake, now)),
                    Ok(Command::Http { moves, .. }) if moves != self.moves() => {
                        http_in_flight = false;
                    }
                    Ok(Command::Http {
                        ladder,
                        rigctl,
                        version,
                        ..
                    }) => {
                        http_in_flight = false;
                        let mut s = self.snapshot.lock().unwrap();
                        s.ladder = Some(ladder);
                        s.rigctl = Some(rigctl);
                        if version.is_some() {
                            s.version = version;
                        }
                    }
                    Ok(Command::Clock(estimate)) => clock_estimate = estimate,
                    Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                }
            }

            // Datagrams from the bridge.
            while let Ok((n, from)) = self.audio_socket.recv_from(&mut buf) {
                if from.ip() != self.ip {
                    continue;
                }
                let data = &buf[..n];
                if data.first() == Some(&b'{') {
                    if let Ok(reply) = Reply::parse(data) {
                        actions.extend(session.handle(
                            Event::ControlReply {
                                from: Socket::Audio,
                                reply,
                            },
                            now,
                        ));
                    }
                } else if let Ok(packet) = Packet::decode(data) {
                    if packet.direction == Direction::Rx && !packet.is_keepalive() {
                        rx_dbfs = peak_dbfs(&packet.samples);
                        assembler.insert(packet, now);
                        actions.extend(session.handle(Event::RxPacket, now));
                    }
                }
            }
            while let Ok((n, from)) = self.control.recv_from(&mut buf) {
                if from.ip() != self.ip {
                    continue;
                }
                if let Ok(reply) = Reply::parse(&buf[..n]) {
                    actions.extend(session.handle(
                        Event::ControlReply {
                            from: Socket::Control,
                            reply,
                        },
                        now,
                    ));
                }
            }

            // The device clock paces the audio: packetise what TX captured, render what RX wants.
            let n = self.audio.read_tx(now, &mut tx_block);
            let tx_dbfs = if n > 0 {
                peak_dbfs(&tx_block[..n])
            } else {
                None
            };
            if n > 0 {
                actions.extend(session.handle(Event::TxAudio(tx_block[..n].to_vec()), now));
            }
            for chunk in assembler.drain(now) {
                timeline.push(&chunk);
            }
            let want = self.audio.rx_demand(now);
            if want > 0 {
                rx_block.clear();
                rx_block.resize(want, 0);
                timeline.render(now, &mut rx_block);
                self.audio.write_rx(&rx_block);
            }

            actions.extend(session.tick(now));
            if session.link() == Link::Lost {
                timeline.link_lost();
            }
            if matches!(session.link(), Link::Searching | Link::Lost)
                && self.last_route.elapsed() >= RESOLVE_INTERVAL
            {
                self.last_route = Instant::now();
                let ip = self.local_ip(&audio_addr);
                self.snapshot.lock().unwrap().local_ip = ip;
            }
            if let Some(ip) = self.lookup.poll(&self.config, session.link()) {
                if ip != self.ip {
                    self.ip = ip;
                    control_addr = SocketAddr::new(ip, self.config.control_port);
                    audio_addr = SocketAddr::new(ip, self.config.audio_port);
                    let local_ip = self.local_ip(&audio_addr);
                    session.bridge_moved(
                        self.local_addr(local_ip, &self.audio_socket),
                        self.local_addr(local_ip, &self.control),
                    );
                    point_at(&self.snapshot, &self.auth, control_addr, local_ip);
                    actions.push(Action::Announce(format!(
                        "{} now points at {ip}: trying there.",
                        self.config.host
                    )));
                }
            }
            self.perform(
                &mut session,
                actions,
                &control_addr,
                &audio_addr,
                &mut http_in_flight,
            );

            {
                let mut s = self.snapshot.lock().unwrap();
                s.mode = session.mode();
                s.link = session.link();
                s.hello = session.hello().cloned();
                s.status = session.status().cloned();
                s.rx = assembler.stats();
                s.playback = timeline.stats();
                s.tx = session.tx().stats();
                s.rx_dbfs = rx_dbfs;
                s.tx_dbfs = tx_dbfs;
                s.buffer_ms = timeline.buffer_ms();
                s.trim_ppm = timeline.trim_ppm();
                s.clock = clock_estimate;
                s.streaming = session.streaming();
                s.audio_fault = self.audio.fault();
                s.slot_held_by_other = session.slot_held_by_other();
            }

            ticks += 1;
            let next = start + TICK * ticks as u32;
            let now_i = Instant::now();
            if next > now_i {
                std::thread::sleep(next - now_i);
            }
        }

        let actions = session.quit(clock.now());
        self.perform(
            &mut session,
            actions,
            &control_addr,
            &audio_addr,
            &mut http_in_flight,
        );
        let mut s = self.snapshot.lock().unwrap();
        s.streaming = false;
        s.finished = true;
    }

    /// `Auth.moves`: how often the bridge's name has moved to another address.
    fn moves(&self) -> u64 {
        self.auth.lock().unwrap().moves
    }

    /// The IP the bridge sees us from: `Config.bind` when it names an interface,
    /// otherwise the one the kernel routes towards the bridge.
    fn local_ip(&self, bridge: &SocketAddr) -> Option<IpAddr> {
        if self.config.bind.is_unspecified() {
            local_address_towards(bridge)
        } else {
            Some(self.config.bind)
        }
    }

    /// Since bridge step 1.2 the bridge takes UDP `set_gain` and
    /// `set_tx_audio_cap` only from the Pi itself, so settings go over HTTP with
    /// the session token. UDP stays for a bridge with no HTTP port configured,
    /// and (on a 404) for one older than the endpoint; never after a 401.
    fn change_setting(&mut self, request: Request, actions: &mut Vec<Action>) {
        let (Some(port), Some((path, body))) = (self.config.http_port, setting_post(&request))
        else {
            actions.push(Action::Control {
                from: Socket::Control,
                request,
            });
            return;
        };
        let host = self.ip.to_string();
        let sender = self.command_sender.clone();
        let auth = self.auth.clone();
        let tokens = self.config.tokens.clone();
        let moves = self.moves();
        let spawned = std::thread::Builder::new()
            .name("vbcore-setting".into())
            .spawn(move || {
                let web = pin::Web {
                    host: &host,
                    port,
                    auth: &auth,
                    tokens: tokens.as_deref(),
                    timeout: http::DEFAULT_TIMEOUT,
                    moves,
                };
                let answer = pin::post_authed(&web, path, &body);
                let _ = sender.send(Command::Setting {
                    request,
                    answer,
                    moves,
                });
            });
        if spawned.is_err() {
            actions.push(Action::Announce(
                "Could not send the change to the bridge.".into(),
            ));
        }
    }

    fn local_addr(&self, ip: Option<IpAddr>, socket: &UdpSocket) -> Option<SocketAddr> {
        ip.zip(socket.local_addr().ok())
            .map(|(ip, bound)| SocketAddr::new(ip, bound.port()))
    }

    /// A fresh audio socket on a new port; the old one closes with the swap.
    fn rebind_audio(&mut self, session: &mut Session, bridge_audio: &SocketAddr) {
        if let Ok(fresh) = UdpSocket::bind(SocketAddr::new(self.config.bind, 0)) {
            if fresh.set_nonblocking(true).is_ok() {
                self.audio_socket = fresh;
            }
        }
        let ip = self.local_ip(bridge_audio);
        let addr = self.local_addr(ip, &self.audio_socket);
        session.audio_rebound(addr);
        if let Some(a) = addr {
            self.snapshot.lock().unwrap().local_audio_port = a.port();
        }
    }

    fn perform(
        &mut self,
        session: &mut Session,
        actions: Vec<Action>,
        control_addr: &SocketAddr,
        audio_addr: &SocketAddr,
        http_in_flight: &mut bool,
    ) {
        for action in actions {
            match action {
                Action::RebindAudio => self.rebind_audio(session, audio_addr),
                Action::ModeDecided(Ok(mode)) => {
                    let _ = self.replies.send(CommandReply::ModeSet(mode));
                }
                Action::ModeDecided(Err(r)) => {
                    let _ = self.replies.send(CommandReply::Refused(r.0));
                }
                Action::Control { from, request } => {
                    let socket = match from {
                        Socket::Control => &self.control,
                        Socket::Audio => &self.audio_socket,
                    };
                    // A rebooting Pi raises "No route to host"; one lost datagram must not stop the loop.
                    let _ = socket.send_to(&request.to_bytes(), control_addr);
                }
                Action::Audio(packet) => {
                    let _ = self.audio_socket.send_to(&packet.encode(), audio_addr);
                }
                Action::PollHttp => {
                    if let (Some(port), false) = (self.config.http_port, *http_in_flight) {
                        *http_in_flight = true;
                        let host = self.ip.to_string();
                        let sender = self.command_sender.clone();
                        let moves = self.moves();
                        let _ = std::thread::Builder::new()
                            .name("vbcore-http".into())
                            .spawn(move || {
                                let ladder = LadderState::parse(&http::get(
                                    &host,
                                    port,
                                    "/api/ladder",
                                    http::DEFAULT_TIMEOUT,
                                ));
                                let rigctl = RigctlState::parse(&http::get(
                                    &host,
                                    port,
                                    "/api/rigctl",
                                    http::DEFAULT_TIMEOUT,
                                ));
                                let version =
                                    match http::get(&host, port, "/", http::DEFAULT_TIMEOUT) {
                                        HttpOutcome::Response { status: 200, body } => {
                                            BridgeVersion::from_home_page(&body)
                                        }
                                        _ => None,
                                    };
                                let _ = sender.send(Command::Http {
                                    ladder,
                                    rigctl,
                                    version,
                                    moves,
                                });
                            });
                    }
                }
                Action::PostListing if self.listing_in_flight => {}
                Action::PostListing => {
                    if let (Some(port), Some(listing)) =
                        (self.config.http_port, self.config.listing.as_ref())
                    {
                        self.listing_in_flight = true;
                        // The same literal address as the UDP sockets use, so the bridge
                        // matches the POST to our stream by source IP.
                        let host = match self.ip {
                            std::net::IpAddr::V6(v6) => format!("[{v6}]"),
                            v4 => v4.to_string(),
                        };
                        let body = listing.body(&host);
                        let sender = self.command_sender.clone();
                        let auth = self.auth.clone();
                        let tokens = self.config.tokens.clone();
                        let pin = self.pin.take();
                        let moves = self.moves();
                        let spawned = std::thread::Builder::new()
                            .name("vbcore-listing".into())
                            .spawn(move || {
                                let web = pin::Web {
                                    host: &host,
                                    port,
                                    auth: &auth,
                                    tokens: tokens.as_deref(),
                                    timeout: http::DEFAULT_TIMEOUT,
                                    moves,
                                };
                                let (state, warning) =
                                    pin::post_listing(&web, "/api/apps", &body, pin);
                                let _ = sender.send(Command::Listing(state, warning, moves));
                            });
                        if spawned.is_err() {
                            self.listing_in_flight = false;
                        }
                    }
                }
                Action::Announce(text) => {
                    let _ = self.announcements.send(text);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy_with_vb_auth() -> RigctlState {
        RigctlState::Proxy {
            access: None,
            port: None,
            listening: None,
            vb_auth: true,
            login_required: false,
        }
    }

    #[test]
    fn a_moved_bridge_gets_no_token_from_the_relay_until_its_own_is_read() {
        let old: SocketAddr = "192.0.2.1:4730".parse().unwrap();
        let new: SocketAddr = "192.0.2.2:4730".parse().unwrap();
        let snapshot = Arc::new(Mutex::new(Snapshot::new(
            old,
            0,
            Mode::ListenOnly,
            String::new(),
        )));
        snapshot.lock().unwrap().rigctl = Some(proxy_with_vb_auth());
        let auth = Arc::new(Mutex::new(Auth {
            token: Some("oldbridgetoken".into()),
            device_id: Some("fedcba9876543210".into()),
            moves: 0,
        }));
        let source = relay_source(4532, snapshot.clone(), auth.clone());
        let before = source();
        assert_eq!(before.addr, SocketAddr::new(old.ip(), 4532));
        assert_eq!(before.token.as_deref(), Some("oldbridgetoken"));

        point_at(&snapshot, &auth, new, None);
        let after = source();
        assert_eq!(after.addr, SocketAddr::new(new.ip(), 4532));
        assert_eq!((after.token, after.vb_auth), (None, false));
        // A fresh `/api/rigctl` at the new address names `\vb_auth`; still no
        // token until a POST there reads its `device_id`.
        snapshot.lock().unwrap().rigctl = Some(proxy_with_vb_auth());
        let polled = source();
        assert_eq!((polled.token, polled.vb_auth), (None, true));
        assert_eq!(snapshot.lock().unwrap().bridge, new);
        assert_eq!(auth.lock().unwrap().moves, 1);
    }

    fn looked_up(_: &str) -> std::io::Result<Vec<IpAddr>> {
        Ok(vec!["192.0.2.7".parse().unwrap()])
    }

    /// A lookup started while the link was down and allowed to finish.
    fn finished_lookup(config: &Config) -> Lookup {
        let mut lookup = Lookup {
            running: None,
            last: Instant::now() - RESOLVE_INTERVAL,
        };
        assert_eq!(lookup.poll(config, Link::Lost), None);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !lookup.running.as_ref().expect("started").is_finished() {
            assert!(Instant::now() < deadline, "lookup never finished");
            std::thread::sleep(Duration::from_millis(1));
        }
        lookup
    }

    #[test]
    fn a_lookup_ending_after_the_link_came_back_is_dropped() {
        let mut config = Config::new("bridge.example");
        config.resolve = looked_up;
        let ip: IpAddr = "192.0.2.7".parse().unwrap();

        assert_eq!(finished_lookup(&config).poll(&config, Link::Lost), Some(ip));
        let mut lookup = finished_lookup(&config);
        assert_eq!(lookup.poll(&config, Link::Connected), None);
        assert!(
            lookup.running.is_none(),
            "the late answer is spent, not kept"
        );

        // Up, or named by address: no lookup starts.
        let mut idle = Lookup {
            running: None,
            last: Instant::now() - RESOLVE_INTERVAL,
        };
        assert_eq!(idle.poll(&config, Link::Connected), None);
        assert!(idle.running.is_none());
        let literal = Config {
            host: "192.0.2.9".into(),
            ..config.clone()
        };
        assert_eq!(idle.poll(&literal, Link::Lost), None);
        assert!(idle.running.is_none());
    }

    #[test]
    fn a_settings_answer_reads_as_the_control_reply() {
        assert_eq!(
            setting_reply(200, r#"{"ok":true,"tx":55,"rx":40}"#),
            Reply::Ok(serde_json::json!({"tx_gain":55,"rx_gain":40}))
        );
        assert_eq!(
            setting_reply(400, r#"{"ok":false,"error":"bad_seconds"}"#),
            Reply::Error("bad_seconds".into())
        );
        assert_eq!(
            setting_reply(502, "Bad Gateway"),
            Reply::Error("HTTP 502".into())
        );
        assert_eq!(
            setting_post(&Request::SetGain {
                tx: None,
                rx: Some(40)
            }),
            Some(("/api/gain", r#"{"rx":40}"#.to_string()))
        );
    }
}
