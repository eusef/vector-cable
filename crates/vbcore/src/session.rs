//! Session state machine: Transmit (peer) and Listen Only (monitor) with every
//! wire rule from the brief, as pure logic. Events and time go in, actions
//! come out; the runner moves bytes.
//!
//! One socket registers. The bridge gives the peer slot to the last address
//! that sent `hello` or a TX packet, and its loop reads the audio socket
//! before the control socket, so a `hello` from a second socket can move RX
//! for up to a second. Everything that registers or keeps the slot therefore
//! goes from the audio socket only: `hello`, `monitor`, `bye`, keepalives and
//! TX audio. The control socket carries only requests that register nothing
//! (`status`, later `set_gain` and `set_tx_audio_cap`).
//!
//! Rules enforced here (`vector-bridge/DOCS/protocol.md`, brief section 6):
//!
//! - Transmit: `hello` then a keepalive, then audio and keepalives. A `hello`
//!   resets the Pi's TX jitter buffer (about 100 ms), so none is sent while
//!   transmitting except to reconnect after the link was lost.
//! - Listen Only: `monitor` re-sent from the socket that receives RX at least
//!   every 2 s; never any TX packet, keepalives included.
//! - Transmit to Listen Only: stop TX, `bye` when advertised, then `monitor`.
//!   A `monitor` from the peer's own address is refused (`is_peer`), so without
//!   `bye` the audio socket is re-bound first and the old peer expires in 5 s.
//! - Default to Listen Only; refuse Transmit while `status` shows the peer slot
//!   flipping (another address taking it twice in 30 s); drop back to Listen
//!   only if it starts flipping while transmitting. Starting in Transmit waits
//!   for the first `status` (at most [`START_STATUS_WAIT_S`]) and applies the
//!   same rule.
//! - Keepalives continue through RX silences (the Pi reopening its device).
//! - On quit, sleep or a lost link stop TX at once; send `bye` only when the
//!   bridge advertises it.
//! - Reconnect with no user action: re-`hello` or re-`monitor` when nothing
//!   has been heard for a while.

use std::net::SocketAddr;

use crate::protocol::{HelloReply, Packet, Reply, Request};
use crate::status::{BridgeStatus, PeerSlot};
use crate::time::LocalTime;
use crate::tx::{TxEvent, TxPath};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Monitor: hears RX, never transmits, never disturbs the iPad.
    ListenOnly,
    /// Peer: holds the slot, can transmit.
    Transmit,
}

impl Mode {
    pub fn word(self) -> &'static str {
        match self {
            Mode::ListenOnly => "Listen Only",
            Mode::Transmit => "Transmit",
        }
    }
}

/// Which local UDP socket an action goes out from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Socket {
    /// Sends `status` and other requests that register nothing; receives their replies.
    Control,
    /// Sends `hello`, `monitor`, `bye`, TX audio and keepalives; receives RX and their replies.
    Audio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// No reply from the bridge yet.
    Searching,
    /// Replies and RX audio are arriving.
    Connected,
    /// Replies arrive but RX audio has paused (the Pi reopening its device, or nobody registered).
    RxPaused,
    /// Nothing heard for [`LOST_AFTER_S`]; reconnecting.
    Lost,
}

impl Link {
    pub fn word(self) -> &'static str {
        match self {
            Link::Searching => "Searching",
            Link::Connected => "Good",
            Link::RxPaused => "RX paused",
            Link::Lost => "Lost",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Send a control request to UDP 5101 from `from`.
    Control { from: Socket, request: Request },
    /// Send a TX packet (audio or keepalive) to UDP 5100 from the audio socket.
    Audio(Packet),
    /// Bind a fresh audio socket (a new port) and tell the session its address
    /// with [`Session::audio_rebound`]; the old one is closed. Done before the
    /// actions that follow it.
    RebindAudio,
    /// Fetch `/api/ladder`, `/api/rigctl` and the home page (the runner does the HTTP).
    PollHttp,
    /// `POST /api/apps` (the app listing; the runner does the HTTP, off the audio path).
    PostListing,
    /// A deferred start-up mode request was decided.
    ModeDecided(Result<Mode, Refused>),
    /// A plain-words announcement for the status region and `--plain`.
    Announce(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A reply datagram arrived on `from`.
    ControlReply { from: Socket, reply: Reply },
    /// An RX audio packet arrived on the audio socket (the audio itself goes to the assembler).
    RxPacket,
    /// Samples read from the TX device, starting at `now`.
    TxAudio(Vec<i16>),
    /// Laptop is going to sleep: stop TX, free the slot.
    Sleep,
    /// Laptop woke up: register again.
    Wake,
}

/// Why a mode request was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused(pub String);

pub const MONITOR_INTERVAL_S: f64 = 1.0;
pub const STATUS_INTERVAL_S: f64 = 2.0;
pub const HTTP_INTERVAL_S: f64 = 5.0;
pub const RX_PAUSED_AFTER_S: f64 = 2.0;
pub const LOST_AFTER_S: f64 = 10.0;
pub const RECONNECT_INTERVAL_S: f64 = 2.0;
/// Another address taking the slot this many times inside the window means it is flipping.
pub const FLIP_WINDOW_S: f64 = 30.0;
pub const FLIP_CHANGES: usize = 2;
/// Starting in Transmit waits this long for a first `status` before saying hello.
pub const START_STATUS_WAIT_S: f64 = 2.0;
/// The app listing is re-posted this often while registered (DOCS/app-listing.md).
pub const LISTING_INTERVAL_S: f64 = 60.0;
pub const FEATURE_BYE: &str = "bye";

#[derive(Clone, Debug)]
pub struct Session {
    client_name: String,
    /// The audio socket's address as the bridge sees it.
    audio_addr: Option<SocketAddr>,
    /// Every address of ours the bridge may have seen as peer: the control
    /// socket and audio sockets since re-bound (their slot expires in 5 s).
    own_addrs: Vec<SocketAddr>,
    mode: Mode,
    link: Link,
    tx: TxPath,
    hello: Option<HelloReply>,
    status: Option<BridgeStatus>,
    peer_seen: Option<PeerSlot>,
    /// Times another address took the slot.
    foreign_takes: Vec<LocalTime>,
    last_heard: Option<LocalTime>,
    last_rx: Option<LocalTime>,
    last_monitor: Option<LocalTime>,
    last_status: Option<LocalTime>,
    last_http: Option<LocalTime>,
    last_reconnect: Option<LocalTime>,
    last_listing: Option<LocalTime>,
    /// When the first registration went out: no reply within [`LOST_AFTER_S`]
    /// of it counts as a lost link, so TX never runs against silence.
    search_since: Option<LocalTime>,
    /// A start-up mode request waiting for the first `status`, with its deadline.
    pending_mode: Option<(Mode, LocalTime)>,
    /// A registration was sent for the current mode (so `start` does not repeat it).
    register_sent: bool,
    /// The bridge accepted a `hello` or `monitor` for the current mode.
    registered: bool,
    asleep: bool,
    finished: bool,
}

impl Session {
    /// `gated` false streams silence too (`soak`). `audio_addr` and
    /// `control_addr` are our sockets' addresses as the bridge sees them, so
    /// `status.peer` can be recognised as ours.
    pub fn new(
        client_name: &str,
        gated: bool,
        audio_addr: Option<SocketAddr>,
        control_addr: Option<SocketAddr>,
    ) -> Self {
        Session {
            client_name: client_name.to_string(),
            audio_addr,
            own_addrs: audio_addr.into_iter().chain(control_addr).collect(),
            mode: Mode::ListenOnly,
            link: Link::Searching,
            tx: TxPath::new(gated),
            hello: None,
            status: None,
            peer_seen: None,
            foreign_takes: Vec::new(),
            last_heard: None,
            last_rx: None,
            last_monitor: None,
            last_status: None,
            last_http: None,
            last_reconnect: None,
            last_listing: None,
            search_since: None,
            pending_mode: None,
            register_sent: false,
            registered: false,
            asleep: false,
            finished: false,
        }
    }

    /// The runner bound a fresh audio socket (after [`Action::RebindAudio`]).
    pub fn audio_rebound(&mut self, addr: Option<SocketAddr>) {
        self.audio_addr = addr;
        if let Some(a) = addr {
            if !self.own_addrs.contains(&a) {
                self.own_addrs.push(a);
            }
        }
    }

    /// The bridge's name now points at a new address, so ours as it sees them may differ too.
    pub fn bridge_moved(&mut self, audio: Option<SocketAddr>, control: Option<SocketAddr>) {
        self.audio_rebound(audio);
        if let Some(c) = control {
            if !self.own_addrs.contains(&c) {
                self.own_addrs.push(c);
            }
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn link(&self) -> Link {
        self.link
    }

    pub fn hello(&self) -> Option<&HelloReply> {
        self.hello.as_ref()
    }

    pub fn status(&self) -> Option<&BridgeStatus> {
        self.status.as_ref()
    }

    pub fn tx(&self) -> &TxPath {
        &self.tx
    }

    /// True while TX audio packets flow.
    pub fn streaming(&self) -> bool {
        self.mode == Mode::Transmit && self.tx.streaming()
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    /// A start-up mode request is still waiting for the first `status`.
    pub fn mode_pending(&self) -> bool {
        self.pending_mode.is_some()
    }

    /// The bridge advertises `bye` (P4 feature discovery).
    pub fn bye_supported(&self) -> bool {
        self.hello.as_ref().is_some_and(|h| h.supports(FEATURE_BYE))
    }

    /// `status.peer` names an address other than ours.
    pub fn slot_held_by_other(&self) -> bool {
        match self.status.as_ref().map(|s| &s.peer) {
            Some(PeerSlot::Held(addr)) => !self.is_our_address(addr),
            _ => false,
        }
    }

    /// Another address took the slot at least [`FLIP_CHANGES`] times in the last [`FLIP_WINDOW_S`].
    pub fn slot_flipping(&self, now: LocalTime) -> bool {
        self.foreign_takes
            .iter()
            .filter(|t| now - **t <= FLIP_WINDOW_S)
            .count()
            >= FLIP_CHANGES
    }

    fn is_our_address(&self, addr: &str) -> bool {
        match addr.parse::<SocketAddr>() {
            Ok(theirs) => self.own_addrs.contains(&theirs),
            Err(_) => false,
        }
    }

    /// Asks for a mode. Transmit is refused while the slot is flipping.
    pub fn request_mode(&mut self, mode: Mode, now: LocalTime) -> Result<Vec<Action>, Refused> {
        self.pending_mode = None;
        if mode == self.mode && self.register_sent {
            return Ok(Vec::new());
        }
        if mode == Mode::Transmit && self.slot_flipping(now) {
            return Err(Refused(
                "Transmit refused: another client keeps taking the slot back. Stop VectorComm on the iPad first.".into(),
            ));
        }
        let mut actions = Vec::new();
        if mode == Mode::Transmit && self.slot_held_by_other() {
            actions.push(Action::Announce(
                "Another client holds the slot; it may take it back.".into(),
            ));
        }
        let leaving_transmit = self.mode == Mode::Transmit && mode == Mode::ListenOnly;
        self.mode = mode;
        actions.push(Action::Announce(format!("Mode {}.", mode.word())));
        if leaving_transmit {
            actions.extend(self.leave_slot());
        } else {
            self.tx.stop();
            self.registered = false;
        }
        actions.extend(self.register(now));
        Ok(actions)
    }

    /// Starts in `mode`. Listen Only registers at once. Transmit first waits
    /// for a `status` (at most [`START_STATUS_WAIT_S`]): when another client
    /// holds the slot, the session starts in Listen Only and says why, as the
    /// brief's default; the decision comes back as [`Action::ModeDecided`]
    /// from `tick`.
    pub fn start(&mut self, mode: Mode, now: LocalTime) -> Vec<Action> {
        let mut actions = vec![Action::Announce(format!(
            "Mode {}. Looking for the bridge.",
            mode.word()
        ))];
        match mode {
            Mode::ListenOnly => match self.request_mode(mode, now) {
                // The start line above already said the mode.
                Ok(a) => actions.extend(a.into_iter().filter(
                    |a| !matches!(a, Action::Announce(t) if t == &format!("Mode {}.", mode.word())),
                )),
                Err(r) => actions.push(Action::Announce(r.0)),
            },
            Mode::Transmit => {
                self.pending_mode = Some((mode, now + START_STATUS_WAIT_S));
                actions.push(Action::Announce(
                    "Checking who holds the slot before transmitting.".into(),
                ));
                self.last_status = Some(now);
                actions.push(Action::Control {
                    from: Socket::Control,
                    request: Request::Status,
                });
            }
        }
        actions
    }

    /// Gives the slot up: `bye` when advertised, otherwise a fresh audio
    /// socket so `monitor` is not refused as `is_peer` (the old peer expires
    /// in 5 s).
    fn leave_slot(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        self.tx.stop();
        if self.bye_supported() {
            actions.push(Action::Control {
                from: Socket::Audio,
                request: Request::Bye,
            });
        } else {
            actions.push(Action::RebindAudio);
        }
        self.registered = false;
        actions
    }

    /// Registers for the current mode from the audio socket: `monitor`, or
    /// `hello` followed by a keepalive so the audio socket holds the peer slot.
    fn register(&mut self, now: LocalTime) -> Vec<Action> {
        let mut actions = Vec::new();
        match self.mode {
            Mode::ListenOnly => {
                self.last_monitor = Some(now);
                actions.push(Action::Control {
                    from: Socket::Audio,
                    request: Request::Monitor {
                        client: self.client_name.clone(),
                    },
                });
            }
            Mode::Transmit => {
                actions.push(Action::Control {
                    from: Socket::Audio,
                    request: Request::Hello {
                        client: self.client_name.clone(),
                    },
                });
                actions.push(Action::Audio(self.tx.keepalive(now).packet().clone()));
            }
        }
        self.register_sent = true;
        self.last_reconnect = Some(now);
        if self.search_since.is_none() {
            self.search_since = Some(now);
        }
        actions
    }

    /// Periodic work: keepalives, `monitor` refresh, `status` polls, reconnects.
    pub fn tick(&mut self, now: LocalTime) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.finished || self.asleep {
            return actions;
        }
        let due = |last: Option<LocalTime>, every: f64| last.is_none_or(|t| now - t >= every);

        let link = self.compute_link(now);
        if link != self.link {
            let was = self.link;
            self.link = link;
            actions.push(Action::Announce(format!("Link {}.", link.word())));
            if link == Link::Lost && was != Link::Lost {
                // The next reply starts a new session on the bridge's side.
                self.registered = false;
                if self.mode == Mode::Transmit {
                    // VOX rule: a lost connection stops the TX stream at once.
                    self.tx.stop();
                    actions.push(Action::Announce("Link lost: TX stopped.".into()));
                }
            }
        }

        if let Some((mode, deadline)) = self.pending_mode {
            if self.status.is_some() || now >= deadline {
                if self.status.is_none() {
                    actions.push(Action::Announce(
                        "No status from the bridge yet; going ahead.".into(),
                    ));
                }
                // The brief's default: another client's slot is not taken at start-up.
                let held_by = match self.status.as_ref().map(|s| &s.peer) {
                    Some(PeerSlot::Held(a)) if !self.is_our_address(a) => Some(a.clone()),
                    _ => None,
                };
                let decided = match held_by {
                    Some(addr) => Err(Refused(format!(
                        "Transmit refused: another client ({addr}) holds the slot. Starting in Listen Only; press t to take it."
                    ))),
                    None => self.request_mode(mode, now),
                };
                match decided {
                    Ok(a) => {
                        actions.extend(a);
                        actions.push(Action::ModeDecided(Ok(mode)));
                    }
                    Err(r) => {
                        actions.push(Action::Announce(r.0.clone()));
                        actions.push(Action::ModeDecided(Err(r)));
                        // Fall back to Listen Only, as a refused request leaves it.
                        if let Ok(a) = self.request_mode(Mode::ListenOnly, now) {
                            actions.extend(a);
                        }
                    }
                }
            }
        } else {
            match self.mode {
                Mode::ListenOnly => {
                    if due(self.last_monitor, MONITOR_INTERVAL_S) {
                        self.last_monitor = Some(now);
                        actions.push(Action::Control {
                            from: Socket::Audio,
                            request: Request::Monitor {
                                client: self.client_name.clone(),
                            },
                        });
                    }
                }
                Mode::Transmit => {
                    if self.slot_flipping(now) {
                        self.mode = Mode::ListenOnly;
                        actions.push(Action::Announce(
                            "Another client keeps taking the slot back. Back to Listen Only. Stop VectorComm on the iPad to transmit.".into(),
                        ));
                        actions.extend(self.leave_slot());
                        actions.extend(self.register(now));
                    } else {
                        // Reconnect only when nothing has been heard: a hello resets the Pi's
                        // TX jitter buffer. The keepalive that follows it keeps the audio
                        // socket the last sender.
                        let heard = self.last_heard.is_some_and(|t| now - t < LOST_AFTER_S);
                        if !heard && due(self.last_reconnect, RECONNECT_INTERVAL_S) {
                            actions.extend(self.register(now));
                        } else if let Some(TxEvent::Keepalive(p)) = self.tx.tick(now) {
                            actions.push(Action::Audio(p));
                        }
                    }
                }
            }
        }

        if due(self.last_status, STATUS_INTERVAL_S) {
            self.last_status = Some(now);
            actions.push(Action::Control {
                from: Socket::Control,
                request: Request::Status,
            });
        }
        if due(self.last_http, HTTP_INTERVAL_S) {
            self.last_http = Some(now);
            actions.push(Action::PollHttp);
        }
        if self.registered && due(self.last_listing, LISTING_INTERVAL_S) {
            self.last_listing = Some(now);
            actions.push(Action::PostListing);
        }
        actions
    }

    fn compute_link(&self, now: LocalTime) -> Link {
        match self.last_heard {
            None => match self.search_since {
                Some(since) if now - since >= LOST_AFTER_S => Link::Lost,
                _ => Link::Searching,
            },
            Some(heard) if now - heard >= LOST_AFTER_S => Link::Lost,
            Some(_) => match self.last_rx {
                Some(rx) if now - rx < RX_PAUSED_AFTER_S => Link::Connected,
                _ => Link::RxPaused,
            },
        }
    }

    pub fn handle(&mut self, event: Event, now: LocalTime) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.finished {
            return actions;
        }
        match event {
            Event::ControlReply { from, reply } => {
                self.last_heard = Some(now);
                match reply {
                    Reply::Ok(v) => {
                        if v.get("device").is_some() || v.get("protocol").is_some() {
                            // A hello or monitor reply.
                            if let Some(h) = Reply::Ok(v).hello() {
                                self.hello = Some(h);
                            }
                            if !self.registered {
                                // A new UDP session on the bridge: describe ourselves again.
                                self.last_listing = None;
                            }
                            self.registered = true;
                        } else if from == Socket::Control
                            && v.get("rx_packets_sent").is_none()
                            && v.get("uptime_s").is_none()
                            && v.get("peer").is_none()
                            && (v.get("tx_gain").is_some()
                                || v.get("rx_gain").is_some()
                                || v.get("tx_audio_cap_s").is_some())
                        {
                            // A set_gain or set_tx_audio_cap reply: the bridge applied it.
                            let applied = BridgeStatus::from_value(&v);
                            let mut words = Vec::new();
                            if let Some(st) = self.status.as_mut() {
                                if let Some(g) = applied.tx_gain {
                                    st.tx_gain = Some(g);
                                    words.push(format!("TX gain {g:.0} %"));
                                }
                                if let Some(g) = applied.rx_gain {
                                    st.rx_gain = Some(g);
                                    if applied.tx_gain.is_none() {
                                        words.push(format!("RX gain {g:.0} %"));
                                    }
                                }
                                if let Some(c) = applied.tx_audio_cap_s {
                                    st.tx_audio_cap_s = Some(c);
                                    words.push(format!("TX audio cap {c} s"));
                                }
                            }
                            if !words.is_empty() {
                                actions.push(Action::Announce(format!(
                                    "Bridge set {}.",
                                    words.join(", ")
                                )));
                            }
                        } else if v.get("rx_packets_sent").is_some()
                            || v.get("uptime_s").is_some()
                            || v.get("peer").is_some()
                        {
                            let status = BridgeStatus::from_value(&v);
                            self.note_peer(&status.peer, now);
                            // A bridge that restarted forgot our app listing.
                            let restarted = match (
                                self.status.as_ref().and_then(|s| s.uptime_s),
                                status.uptime_s,
                            ) {
                                (Some(before), Some(after)) => after < before,
                                _ => false,
                            };
                            if restarted {
                                self.last_listing = None;
                            }
                            self.status = Some(status);
                        }
                    }
                    Reply::Error(e) => {
                        if e == "is_peer" && from == Socket::Audio && self.mode == Mode::ListenOnly
                        {
                            // Still the peer (no bye, or it was lost): a fresh socket is not.
                            actions.push(Action::RebindAudio);
                            actions.extend(self.register(now));
                        } else if e == "monitors_full" && from == Socket::Audio {
                            actions.push(Action::Announce(
                                "Bridge has two monitors already; Listen Only is not available."
                                    .into(),
                            ));
                        } else if from == Socket::Control {
                            let why = match e.as_str() {
                                "bad_gain" | "bad_tx" | "bad_rx" => {
                                    "gain must be 0 to 100 %".to_string()
                                }
                                "no_gain_given" => "no gain given".to_string(),
                                "bad_tx_audio_cap" | "bad_seconds" => {
                                    "cap must be 0 to 600 s".to_string()
                                }
                                "gain_failed" => "the bridge could not apply the gain".to_string(),
                                "service_unreachable" => {
                                    "the bridge's audio service did not answer".to_string()
                                }
                                "forbidden_origin" => {
                                    "it refused the request as coming from another site".to_string()
                                }
                                "wifi_open" => {
                                    "the bridge's access point is still open; set its Wi-Fi first"
                                        .to_string()
                                }
                                // UDP settings from off the Pi, since bridge step 1.2.
                                "pin_required" => {
                                    "it takes settings only over HTTP with its PIN".to_string()
                                }
                                other => other.to_string(),
                            };
                            actions.push(Action::Announce(format!(
                                "Bridge refused the change: {why}."
                            )));
                        }
                    }
                }
            }
            Event::RxPacket => {
                self.last_heard = Some(now);
                self.last_rx = Some(now);
            }
            Event::TxAudio(samples) => {
                if self.mode == Mode::Transmit
                    && self.pending_mode.is_none()
                    && !self.asleep
                    && self.link != Link::Lost
                {
                    for e in self.tx.push(&samples, now) {
                        actions.push(Action::Audio(e.packet().clone()));
                    }
                }
                // Listen Only, asleep or lost link: the TX device is read and discarded.
            }
            Event::Sleep => {
                self.asleep = true;
                actions.extend(self.release());
                actions.push(Action::Announce("Sleeping: TX stopped.".into()));
            }
            Event::Wake => {
                self.asleep = false;
                self.registered = false;
                actions.push(Action::Announce("Awake: reconnecting.".into()));
                actions.extend(self.register(now));
            }
        }
        actions
    }

    /// Counts only another address taking the slot. The slot going empty (a Pi
    /// restart, the iPad stopping) or coming to one of our addresses is not a flip.
    fn note_peer(&mut self, peer: &PeerSlot, now: LocalTime) {
        if *peer == PeerSlot::Unknown {
            return;
        }
        let foreign_take = match (&self.peer_seen, peer) {
            (Some(prev), PeerSlot::Held(addr)) => prev != peer && !self.is_our_address(addr),
            _ => false,
        };
        if foreign_take {
            self.foreign_takes.push(now);
            self.foreign_takes.retain(|t| now - *t <= FLIP_WINDOW_S);
        }
        self.peer_seen = Some(peer.clone());
    }

    /// Stops TX at once and frees the slot when the bridge supports `bye`.
    fn release(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        self.tx.stop();
        if self.mode == Mode::Transmit && self.registered && self.bye_supported() {
            actions.push(Action::Control {
                from: Socket::Audio,
                request: Request::Bye,
            });
        }
        self.registered = false;
        actions
    }

    /// Quit: stop TX in this same step, send `bye` only when advertised, then
    /// produce nothing more.
    pub fn quit(&mut self, _now: LocalTime) -> Vec<Action> {
        let mut actions = self.release();
        actions.push(Action::Announce("Stopped.".into()));
        self.finished = true;
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Direction;

    const OURS: &str = "10.42.0.12:40000";
    const OUR_CONTROL: &str = "10.42.0.12:40001";
    const IPAD: &str = "10.42.0.9:5100";

    fn session() -> Session {
        Session::new(
            "test",
            true,
            Some(OURS.parse().unwrap()),
            Some(OUR_CONTROL.parse().unwrap()),
        )
    }

    fn tone(n: usize) -> Vec<i16> {
        (0..n)
            .map(|i| if i % 2 == 0 { 3000 } else { -3000 })
            .collect()
    }

    fn hello_reply(features: Option<&[&str]>, role: Option<&str>) -> Reply {
        let mut v = serde_json::json!({"ok":true,"device":"VectorBridge","protocol":1,"sample_rate":48000,"channels":1,"audio_port":5100});
        if let Some(f) = features {
            v["features"] = serde_json::json!(f);
        }
        if let Some(r) = role {
            v["role"] = serde_json::json!(r);
        }
        Reply::Ok(v)
    }

    fn status_reply(peer: Option<&str>) -> Reply {
        Reply::Ok(serde_json::json!({"ok":true,"rx_packets_sent":10,"peer":peer,"uptime_s":5}))
    }

    fn is_tx_packet(a: &Action) -> bool {
        matches!(a, Action::Audio(p) if p.direction == Direction::Tx)
    }

    fn is_hello(a: &Action) -> bool {
        matches!(
            a,
            Action::Control {
                request: Request::Hello { .. },
                ..
            }
        )
    }

    fn is_monitor(a: &Action) -> bool {
        matches!(
            a,
            Action::Control {
                request: Request::Monitor { .. },
                ..
            }
        )
    }

    fn is_bye(a: &Action) -> bool {
        matches!(
            a,
            Action::Control {
                request: Request::Bye,
                ..
            }
        )
    }

    fn from_audio(a: &Action) -> bool {
        matches!(
            a,
            Action::Control {
                from: Socket::Audio,
                ..
            }
        )
    }

    fn feed_status(s: &mut Session, peers: &[Option<&str>], start: f64, every: f64) {
        for (i, p) in peers.iter().enumerate() {
            s.handle(
                Event::ControlReply {
                    from: Socket::Control,
                    reply: status_reply(*p),
                },
                LocalTime(start + every * i as f64),
            );
        }
    }

    /// Runs `seconds` of 10 ms steps feeding TX audio and an RX packet each step.
    fn run(session: &mut Session, start: f64, seconds: f64, loud: bool) -> Vec<(f64, Action)> {
        let mut out = Vec::new();
        let steps = (seconds * 100.0) as usize;
        for i in 0..steps {
            let now = LocalTime(start + i as f64 * 0.01);
            let samples = if loud { tone(480) } else { vec![0; 480] };
            for a in session.handle(Event::TxAudio(samples), now) {
                out.push((now.seconds(), a));
            }
            for a in session.handle(Event::RxPacket, now) {
                out.push((now.seconds(), a));
            }
            for a in session.tick(now) {
                out.push((now.seconds(), a));
            }
        }
        out
    }

    #[test]
    fn listen_only_never_sends_tx_and_resends_monitor_from_rx_socket_within_2_s() {
        let mut s = session();
        assert_eq!(s.mode(), Mode::ListenOnly, "default is Listen Only");
        let mut actions: Vec<(f64, Action)> = s
            .start(Mode::ListenOnly, LocalTime(0.0))
            .into_iter()
            .map(|a| (0.0, a))
            .collect();
        actions.extend(run(&mut s, 0.01, 60.0, true));
        assert!(
            !actions.iter().any(|(_, a)| is_tx_packet(a)),
            "a monitor never sends a TX packet"
        );
        let monitors: Vec<f64> = actions
            .iter()
            .filter(|(_, a)| is_monitor(a) && from_audio(a))
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(monitors[0], 0.0);
        for w in monitors.windows(2) {
            assert!(w[1] - w[0] <= 2.0 + 1e-9, "monitor gap {} s", w[1] - w[0]);
        }
        assert!(monitors.len() >= 30);
        assert!(!actions.iter().any(|(_, a)| is_hello(a)));
        assert!(!actions.iter().any(|(_, a)| is_monitor(a) && !from_audio(a)));
    }

    #[test]
    fn only_the_audio_socket_registers_and_control_only_polls_status() {
        let mut s = session();
        let mut log: Vec<(f64, Action)> = s
            .request_mode(Mode::Transmit, LocalTime(0.0))
            .unwrap()
            .into_iter()
            .map(|a| (0.0, a))
            .collect();
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(Some(&["bye"]), None),
            },
            LocalTime(0.05),
        );
        log.extend(run(&mut s, 0.1, 5.0, true));
        log.extend(
            s.request_mode(Mode::ListenOnly, LocalTime(5.2))
                .unwrap()
                .into_iter()
                .map(|a| (5.2, a)),
        );
        log.extend(run(&mut s, 5.21, 3.0, true));
        log.extend(s.quit(LocalTime(8.3)).into_iter().map(|a| (8.3, a)));
        for (t, a) in &log {
            match a {
                Action::Control { from, request } => match request {
                    Request::Status => assert_eq!(*from, Socket::Control, "status at {t}"),
                    _ => assert_eq!(*from, Socket::Audio, "{request:?} at {t}"),
                },
                Action::Audio(_) => {}
                _ => {}
            }
        }
        assert!(log.iter().any(|(_, a)| is_hello(a)));
        assert!(log.iter().any(|(_, a)| is_bye(a)));
        assert!(log.iter().any(|(_, a)| is_monitor(a)));
        assert!(log.iter().any(|(_, a)| matches!(
            a,
            Action::Control {
                request: Request::Status,
                ..
            }
        )));
        // Every hello and monitor names the client.
        for (_, a) in &log {
            if let Action::Control {
                request: Request::Hello { client } | Request::Monitor { client },
                ..
            } = a
            {
                assert_eq!(client, "test");
            }
        }
    }

    #[test]
    fn transmit_sends_hello_once_then_keepalive_and_no_hello_while_the_link_is_up() {
        let mut s = session();
        let mut log: Vec<(f64, Action)> = s
            .request_mode(Mode::Transmit, LocalTime(0.0))
            .unwrap()
            .into_iter()
            .map(|a| (0.0, a))
            .collect();
        // Hello then a keepalive in the same step, both from the audio socket.
        let hello_pos = log.iter().position(|(_, a)| is_hello(a)).unwrap();
        assert!(from_audio(&log[hello_pos].1));
        assert!(matches!(&log[hello_pos + 1].1, Action::Audio(p) if p.is_keepalive()));
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(None, None),
            },
            LocalTime(0.05),
        );
        // 20 s: tone for 9 s then silence, with RX arriving all along.
        for i in 0..2000 {
            let now = LocalTime(0.1 + i as f64 * 0.01);
            let samples = if i < 900 { tone(480) } else { vec![0; 480] };
            for a in s.handle(Event::TxAudio(samples), now) {
                log.push((now.seconds(), a));
            }
            for a in s.handle(Event::RxPacket, now) {
                log.push((now.seconds(), a));
            }
            for a in s.tick(now) {
                log.push((now.seconds(), a));
            }
        }
        let hellos: Vec<f64> = log
            .iter()
            .filter(|(_, a)| is_hello(a))
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(
            hellos,
            vec![0.0],
            "one hello; a hello resets the Pi's jitter buffer"
        );
        assert!(
            log.iter()
                .filter(|(t, a)| *t < 9.0 && is_tx_packet(a))
                .count()
                > 800
        );
        // Keepalives continue through the silence afterwards, at most 2 s apart.
        let keepalives: Vec<f64> = log
            .iter()
            .filter(|(_, a)| matches!(a, Action::Audio(p) if p.is_keepalive()))
            .map(|(t, _)| *t)
            .filter(|t| *t > 9.3)
            .collect();
        assert!(keepalives.len() >= 8, "keepalives {keepalives:?}");
        for w in keepalives.windows(2) {
            assert!(w[1] - w[0] <= 2.0 + 1e-9);
        }
    }

    #[test]
    fn lost_link_reconnects_with_hello_from_the_audio_socket() {
        let mut s = session();
        s.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(None, None),
            },
            LocalTime(0.05),
        );
        let mut log = Vec::new();
        for i in 0..2000 {
            let now = LocalTime(0.1 + i as f64 * 0.01);
            for a in s.tick(now) {
                log.push((now.seconds(), a));
            }
        }
        let hellos: Vec<&(f64, Action)> = log.iter().filter(|(_, a)| is_hello(a)).collect();
        assert!(hellos.len() >= 4, "reconnect hellos every 2 s once lost");
        assert!(hellos.iter().all(|(t, a)| *t >= 10.0 && from_audio(a)));
        for (t, _) in &hellos {
            let same_step: Vec<&Action> = log
                .iter()
                .filter(|(lt, _)| lt == t)
                .map(|(_, a)| a)
                .collect();
            let pos = same_step.iter().position(|a| is_hello(a)).unwrap();
            assert!(
                same_step[pos + 1..]
                    .iter()
                    .any(|a| matches!(a, Action::Audio(p) if p.is_keepalive())),
                "keepalive after hello at {t}"
            );
        }
    }

    #[test]
    fn transmit_to_listen_only_with_bye_sends_bye_then_monitor() {
        let mut s = session();
        s.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(Some(&["bye"]), None),
            },
            LocalTime(0.1),
        );
        s.handle(Event::TxAudio(tone(480)), LocalTime(0.2));
        assert!(s.streaming());
        let actions = s.request_mode(Mode::ListenOnly, LocalTime(0.3)).unwrap();
        assert!(!s.streaming());
        let bye = actions.iter().position(is_bye).unwrap();
        let monitor = actions.iter().position(is_monitor).unwrap();
        assert!(bye < monitor, "bye before monitor");
        assert!(from_audio(&actions[bye]) && from_audio(&actions[monitor]));
        assert!(!actions.contains(&Action::RebindAudio));
        assert!(!actions.iter().any(is_tx_packet));
        let after = run(&mut s, 0.31, 3.0, true);
        assert!(!after.iter().any(|(_, a)| is_tx_packet(a)));
    }

    #[test]
    fn transmit_to_listen_only_without_bye_rebinds_the_audio_socket_first() {
        let mut s = session();
        s.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(None, None),
            },
            LocalTime(0.1),
        );
        s.handle(Event::TxAudio(tone(480)), LocalTime(0.2));
        let actions = s.request_mode(Mode::ListenOnly, LocalTime(0.3)).unwrap();
        let rebind = actions
            .iter()
            .position(|a| *a == Action::RebindAudio)
            .unwrap();
        let monitor = actions.iter().position(is_monitor).unwrap();
        assert!(rebind < monitor, "rebind before monitor");
        assert!(!actions.iter().any(is_bye));
        assert!(!actions.iter().any(is_tx_packet));
        // The runner reports the new address; the old one still counts as ours while it expires.
        s.audio_rebound(Some("10.42.0.12:40002".parse().unwrap()));
        feed_status(&mut s, &[Some(OURS), None, Some(IPAD)], 1.0, 2.0);
        assert!(
            !s.slot_flipping(LocalTime(6.0)),
            "our old socket and empty are not takes"
        );
        assert!(s.slot_held_by_other());
        feed_status(&mut s, &[Some("10.42.0.12:40002")], 7.0, 2.0);
        assert!(!s.slot_held_by_other());
    }

    #[test]
    fn is_peer_refusal_of_monitor_rebinds_and_retries() {
        let mut s = session();
        s.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(Some(&["bye"]), None),
            },
            LocalTime(0.1),
        );
        s.request_mode(Mode::ListenOnly, LocalTime(0.3)).unwrap();
        // The bye was lost: the bridge still has us as the peer.
        let actions = s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: Reply::Error("is_peer".into()),
            },
            LocalTime(0.4),
        );
        let rebind = actions
            .iter()
            .position(|a| *a == Action::RebindAudio)
            .unwrap();
        let monitor = actions.iter().position(is_monitor).unwrap();
        assert!(rebind < monitor);
        // In Transmit an is_peer error means nothing (we are the peer on purpose).
        let mut t = session();
        t.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        let none = t.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: Reply::Error("is_peer".into()),
            },
            LocalTime(0.4),
        );
        assert!(none.is_empty());
    }

    #[test]
    fn transmit_refused_while_status_shows_peer_slot_flipping() {
        // (d) Two foreign addresses trading the slot: refused.
        let mut s = session();
        s.start(Mode::ListenOnly, LocalTime(0.0));
        feed_status(
            &mut s,
            &[
                Some(IPAD),
                Some("10.42.0.7:5100"),
                Some(IPAD),
                Some("10.42.0.7:5100"),
            ],
            0.0,
            2.0,
        );
        assert!(s.slot_flipping(LocalTime(8.0)));
        let refused = s.request_mode(Mode::Transmit, LocalTime(8.0)).unwrap_err();
        assert!(refused.0.starts_with("Transmit refused"));
        assert_eq!(s.mode(), Mode::ListenOnly);

        // The iPad taking the slot back from us twice is the real case: refused too.
        let mut s = session();
        s.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        feed_status(
            &mut s,
            &[Some(OURS), Some(IPAD), Some(OURS), Some(IPAD)],
            0.0,
            2.0,
        );
        let actions = s.tick(LocalTime(6.01));
        assert_eq!(s.mode(), Mode::ListenOnly, "dropped back to Listen Only");
        assert!(actions.iter().any(|a| is_monitor(a) && from_audio(a)));
        assert!(!actions.iter().any(is_tx_packet));
        assert!(s.request_mode(Mode::Transmit, LocalTime(7.0)).is_err());
    }

    #[test]
    fn slot_going_empty_or_coming_to_any_of_our_addresses_is_not_flipping() {
        // (a) Pi service restart while transmitting: us, null, us. Stay in Transmit.
        let mut s = session();
        s.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        feed_status(
            &mut s,
            &[Some(OURS), None, Some(OURS), None, Some(OURS)],
            0.0,
            2.0,
        );
        assert!(!s.slot_flipping(LocalTime(9.0)));
        s.tick(LocalTime(9.0));
        assert_eq!(s.mode(), Mode::Transmit);

        // (b) Operator stops VectorComm, then picks Transmit: iPad, null, then us.
        let mut s = session();
        s.start(Mode::ListenOnly, LocalTime(0.0));
        feed_status(&mut s, &[Some(IPAD), Some(IPAD), None], 0.0, 2.0);
        assert!(s.request_mode(Mode::Transmit, LocalTime(5.0)).is_ok());
        feed_status(&mut s, &[Some(OURS), Some(OURS)], 6.0, 2.0);
        s.tick(LocalTime(8.5));
        assert_eq!(s.mode(), Mode::Transmit);

        // (c) Transmit, Listen Only, Transmit with no other client: never refused.
        let mut s = session();
        s.start(Mode::ListenOnly, LocalTime(0.0));
        assert!(s.request_mode(Mode::Transmit, LocalTime(1.0)).is_ok());
        feed_status(&mut s, &[Some(OURS)], 2.0, 2.0);
        assert!(s.request_mode(Mode::ListenOnly, LocalTime(3.0)).is_ok());
        feed_status(&mut s, &[None], 4.0, 2.0);
        assert!(s.request_mode(Mode::Transmit, LocalTime(5.0)).is_ok());
        feed_status(&mut s, &[Some(OURS)], 6.0, 2.0);
        s.tick(LocalTime(7.0));
        assert_eq!(s.mode(), Mode::Transmit);

        // The control socket showing as peer (it should never now) counts as us, not foreign.
        let mut s = session();
        s.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        feed_status(
            &mut s,
            &[Some(OURS), Some(OUR_CONTROL), Some(OURS), Some(OUR_CONTROL)],
            0.0,
            2.0,
        );
        assert!(!s.slot_flipping(LocalTime(7.0)));
        assert!(!s.slot_held_by_other());

        // A steady other peer is not flipping: Transmit is allowed with a warning.
        let mut steady = session();
        steady.start(Mode::ListenOnly, LocalTime(0.0));
        feed_status(&mut steady, &[Some(IPAD); 4], 0.0, 2.0);
        assert!(!steady.slot_flipping(LocalTime(8.0)));
        assert!(steady.slot_held_by_other());
        let actions = steady.request_mode(Mode::Transmit, LocalTime(8.0)).unwrap();
        assert!(actions
            .iter()
            .any(|a| matches!(a, Action::Announce(m) if m.contains("holds the slot"))));
        assert!(actions
            .iter()
            .any(|a| matches!(a, Action::Audio(p) if p.is_keepalive())));

        // Our own port on another host is not us.
        feed_status(&mut steady, &[Some("10.42.0.99:40000")], 10.0, 2.0);
        assert!(steady.slot_held_by_other());
        feed_status(&mut steady, &[Some(OURS)], 12.0, 2.0);
        assert!(!steady.slot_held_by_other());
    }

    #[test]
    fn starting_in_transmit_waits_for_status_then_applies_the_slot_rule() {
        // Status shows the slot free: hello follows at once.
        let mut s = session();
        let start = s.start(Mode::Transmit, LocalTime(0.0));
        assert!(s.mode_pending());
        assert!(start.iter().any(|a| matches!(
            a,
            Action::Control {
                from: Socket::Control,
                request: Request::Status
            }
        )));
        assert!(!start.iter().any(is_hello));
        assert!(s.tick(LocalTime(0.5)).iter().all(|a| !is_hello(a)));
        assert!(
            s.handle(Event::TxAudio(tone(480)), LocalTime(0.6))
                .is_empty(),
            "no TX while pending"
        );
        s.handle(
            Event::ControlReply {
                from: Socket::Control,
                reply: status_reply(None),
            },
            LocalTime(0.7),
        );
        let actions = s.tick(LocalTime(0.71));
        assert!(!s.mode_pending());
        assert!(actions.iter().any(is_hello));
        assert!(actions.contains(&Action::ModeDecided(Ok(Mode::Transmit))));
        assert_eq!(s.mode(), Mode::Transmit);

        // The slot held by our own control socket from an earlier run: still ours.
        let mut s = session();
        s.start(Mode::Transmit, LocalTime(0.0));
        feed_status(&mut s, &[Some(OUR_CONTROL)], 0.5, 2.0);
        let actions = s.tick(LocalTime(0.51));
        assert!(actions.iter().any(is_hello));
        assert_eq!(s.mode(), Mode::Transmit);

        // No status at all: goes ahead after the wait.
        let mut s = session();
        s.start(Mode::Transmit, LocalTime(0.0));
        assert!(!s.tick(LocalTime(1.9)).iter().any(is_hello));
        let actions = s.tick(LocalTime(2.0));
        assert!(actions.iter().any(is_hello));
        assert!(actions.contains(&Action::ModeDecided(Ok(Mode::Transmit))));

        // Another client holds the slot: Listen Only, and the reason is said.
        // One status reply is all that can arrive inside the 2 s wait.
        let mut s = session();
        s.start(Mode::Transmit, LocalTime(0.0));
        feed_status(&mut s, &[Some(IPAD)], 0.4, 2.0);
        let actions = s.tick(LocalTime(0.41));
        assert!(!actions.iter().any(is_hello));
        assert!(actions.iter().any(|a| is_monitor(a) && from_audio(a)));
        let refused = actions
            .iter()
            .find_map(|a| match a {
                Action::ModeDecided(Err(r)) => Some(r.0.clone()),
                _ => None,
            })
            .expect("refused");
        assert!(
            refused.contains(IPAD) && refused.contains("Listen Only"),
            "{refused}"
        );
        assert!(actions.contains(&Action::Announce(refused)));
        assert_eq!(s.mode(), Mode::ListenOnly);
        // The user may still take the slot on purpose, with the warning.
        let taken = s.request_mode(Mode::Transmit, LocalTime(3.0)).unwrap();
        assert!(taken.iter().any(is_hello));
        assert!(taken
            .iter()
            .any(|a| matches!(a, Action::Announce(m) if m.contains("holds the slot"))));
    }

    /// A bridge that never answers must not be transmitted to for ever: with no
    /// reply inside the lost-link timeout since the first hello, TX stops and
    /// hello keeps its reconnect cadence.
    #[test]
    fn no_reply_since_start_counts_as_lost_and_stops_tx() {
        let mut s = session();
        s.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        let mut log = Vec::new();
        for i in 1..3000 {
            let now = LocalTime(i as f64 * 0.01);
            for a in s.handle(Event::TxAudio(tone(480)), now) {
                log.push((now.seconds(), a));
            }
            for a in s.tick(now) {
                log.push((now.seconds(), a));
            }
        }
        let last_tx = log
            .iter()
            .filter(|(_, a)| matches!(a, Action::Audio(p) if !p.is_keepalive()))
            .map(|(t, _)| *t)
            .fold(0.0, f64::max);
        assert!(
            last_tx < LOST_AFTER_S + 0.02,
            "TX went on until {last_tx} s with no reply"
        );
        assert_eq!(s.link(), Link::Lost);
        assert!(log
            .iter()
            .any(|(_, a)| *a == Action::Announce("Link lost: TX stopped.".into())));
        let late_hellos: Vec<f64> = log
            .iter()
            .filter(|(t, a)| *t >= LOST_AFTER_S && is_hello(a))
            .map(|(t, _)| *t)
            .collect();
        assert!(late_hellos.len() >= 8, "{late_hellos:?}");
        for w in late_hellos.windows(2) {
            assert!(
                (w[1] - w[0] - RECONNECT_INTERVAL_S).abs() < 0.02,
                "{late_hellos:?}"
            );
        }
        // The first reply ends the search and TX may resume.
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(None, None),
            },
            LocalTime(30.0),
        );
        s.tick(LocalTime(30.01));
        assert_ne!(s.link(), Link::Lost);
        assert!(s
            .handle(Event::TxAudio(tone(480)), LocalTime(30.02))
            .iter()
            .any(is_tx_packet));
    }

    #[test]
    fn soak_with_gate_off_says_hello_and_streams_silence() {
        let mut s = Session::new(
            "soak",
            false,
            Some(OURS.parse().unwrap()),
            Some(OUR_CONTROL.parse().unwrap()),
        );
        let actions = s.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        assert!(actions.iter().any(is_hello));
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(Some(&["bye"]), None),
            },
            LocalTime(0.05),
        );
        let log = run(&mut s, 0.1, 3.0, false);
        assert!(
            log.iter().filter(|(_, a)| is_tx_packet(a)).count() > 250,
            "silence streams"
        );
        assert!(s.streaming());
        let quit = s.quit(LocalTime(3.2));
        assert!(quit.iter().any(is_bye), "bye was discovered");
    }

    #[test]
    fn soak_with_gate_off_keeps_alive_and_reconnects_after_a_lost_link() {
        let mut s = Session::new(
            "soak",
            false,
            Some(OURS.parse().unwrap()),
            Some(OUR_CONTROL.parse().unwrap()),
        );
        s.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(None, None),
            },
            LocalTime(0.05),
        );
        let mut log = run(&mut s, 0.1, 2.0, false);
        // The bridge goes silent while silence keeps being fed.
        for i in 0..1500 {
            let now = LocalTime(2.1 + i as f64 * 0.01);
            for a in s.handle(Event::TxAudio(vec![0; 480]), now) {
                log.push((now.seconds(), a));
            }
            for a in s.tick(now) {
                log.push((now.seconds(), a));
            }
        }
        assert_eq!(s.link(), Link::Lost);
        assert!(
            !s.streaming(),
            "stopped on the lost link even with the gate off"
        );
        let after_lost: Vec<&(f64, Action)> = log.iter().filter(|(t, _)| *t > 12.5).collect();
        assert!(
            after_lost.iter().any(|(_, a)| is_hello(a)),
            "reconnect hellos"
        );
        assert!(
            after_lost
                .iter()
                .any(|(_, a)| matches!(a, Action::Audio(p) if p.is_keepalive())),
            "keepalives"
        );
        assert!(
            !after_lost
                .iter()
                .any(|(_, a)| matches!(a, Action::Audio(p) if !p.is_keepalive())),
            "no audio while lost"
        );
        // Back: silence streams again.
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(None, None),
            },
            LocalTime(20.0),
        );
        s.tick(LocalTime(20.0));
        let resumed = s.handle(Event::TxAudio(vec![0; 480]), LocalTime(20.01));
        assert!(resumed.iter().any(is_tx_packet));
    }

    #[test]
    fn lost_link_stops_tx_and_audio_resumes_when_the_bridge_is_back() {
        let mut s = session();
        s.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(None, None),
            },
            LocalTime(0.1),
        );
        let mut log = run(&mut s, 0.2, 3.0, true);
        for i in 0..1500 {
            let now = LocalTime(3.2 + i as f64 * 0.01);
            for a in s.handle(Event::TxAudio(tone(480)), now) {
                log.push((now.seconds(), a));
            }
            for a in s.tick(now) {
                log.push((now.seconds(), a));
            }
        }
        assert_eq!(s.link(), Link::Lost);
        assert!(!s.streaming());
        let last_audio = log
            .iter()
            .filter(|(_, a)| matches!(a, Action::Audio(p) if !p.is_keepalive()))
            .map(|(t, _)| *t)
            .fold(0.0, f64::max);
        assert!(last_audio <= 13.19 + 1e-9, "audio stopped at {last_audio}");
        assert!(last_audio > 13.0);
        assert!(log
            .iter()
            .any(|(_, a)| matches!(a, Action::Announce(m) if m == "Link lost: TX stopped.")));
        assert!(log.iter().any(|(t, a)| *t > 14.0 && is_hello(a)));
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(None, None),
            },
            LocalTime(20.0),
        );
        s.tick(LocalTime(20.0));
        assert_ne!(s.link(), Link::Lost);
        let resumed = s.handle(Event::TxAudio(tone(480)), LocalTime(20.01));
        assert!(resumed.iter().any(is_tx_packet));
    }

    #[test]
    fn quit_stops_tx_in_the_same_step_and_sends_bye_only_when_advertised() {
        let mut s = session();
        s.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(None, None),
            },
            LocalTime(0.1),
        );
        let mid = s.handle(Event::TxAudio(tone(480)), LocalTime(0.2));
        assert!(mid.iter().any(is_tx_packet));
        assert!(s.streaming());
        let actions = s.quit(LocalTime(0.21));
        assert!(!s.streaming());
        assert!(!actions.iter().any(is_tx_packet));
        assert!(!actions.iter().any(is_bye));
        assert!(s
            .handle(Event::TxAudio(tone(480)), LocalTime(0.22))
            .is_empty());
        assert!(s.tick(LocalTime(5.0)).is_empty());

        let mut s = session();
        s.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(Some(&["monitor", "bye"]), None),
            },
            LocalTime(0.1),
        );
        s.handle(Event::TxAudio(tone(480)), LocalTime(0.2));
        let actions = s.quit(LocalTime(0.21));
        assert_eq!(
            actions
                .iter()
                .filter(|a| is_bye(a) && from_audio(a))
                .count(),
            1
        );
        assert!(!actions.iter().any(is_tx_packet));

        // A monitor has no slot to free: no bye even when advertised.
        let mut m = session();
        m.start(Mode::ListenOnly, LocalTime(0.0));
        m.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(Some(&["bye"]), Some("monitor")),
            },
            LocalTime(0.1),
        );
        assert!(!m.quit(LocalTime(1.0)).iter().any(is_bye));
    }

    #[test]
    fn sleep_stops_tx_and_wake_reconnects_without_user_action() {
        let mut s = session();
        s.request_mode(Mode::Transmit, LocalTime(0.0)).unwrap();
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(Some(&["bye"]), None),
            },
            LocalTime(0.1),
        );
        s.handle(Event::TxAudio(tone(480)), LocalTime(0.2));
        let sleep = s.handle(Event::Sleep, LocalTime(0.3));
        assert!(sleep.iter().any(|a| is_bye(a) && from_audio(a)));
        assert!(!s.streaming());
        assert!(s
            .handle(Event::TxAudio(tone(480)), LocalTime(0.4))
            .is_empty());
        assert!(
            s.tick(LocalTime(3.0)).is_empty(),
            "nothing goes out while asleep"
        );
        let wake = s.handle(Event::Wake, LocalTime(100.0));
        assert!(wake.iter().any(|a| is_hello(a) && from_audio(a)));
    }

    #[test]
    fn link_state_follows_replies_and_rx_and_lost_link_reconnects() {
        let mut s = session();
        s.start(Mode::ListenOnly, LocalTime(0.0));
        assert_eq!(s.link(), Link::Searching);
        s.handle(
            Event::ControlReply {
                from: Socket::Audio,
                reply: hello_reply(None, Some("monitor")),
            },
            LocalTime(0.1),
        );
        s.tick(LocalTime(0.2));
        assert_eq!(s.link(), Link::RxPaused);
        s.handle(Event::RxPacket, LocalTime(0.3));
        s.tick(LocalTime(0.4));
        assert_eq!(s.link(), Link::Connected);
        let actions = s.tick(LocalTime(12.0));
        assert_eq!(s.link(), Link::Lost);
        assert!(actions
            .iter()
            .any(|a| matches!(a, Action::Announce(m) if m == "Link Lost.")));
        assert!(actions.iter().any(|a| is_monitor(a) && from_audio(a)));
        s.handle(Event::RxPacket, LocalTime(12.5));
        s.tick(LocalTime(12.6));
        assert_eq!(s.link(), Link::Connected);
    }

    /// A set_gain or set_tx_audio_cap reply on the control socket is announced
    /// as applied and updates the shown values; a refusal is announced as such.
    #[test]
    fn level_replies_are_confirmed_or_refused_in_words() {
        let mut s = session();
        s.start(Mode::ListenOnly, LocalTime(0.0));
        feed_status(&mut s, &[None], 0.1, 1.0);
        let applied = s.handle(
            Event::ControlReply {
                from: Socket::Control,
                reply: Reply::Ok(serde_json::json!({"ok":true,"tx_gain":55,"rx_gain":50})),
            },
            LocalTime(0.2),
        );
        assert!(
            applied.contains(&Action::Announce("Bridge set TX gain 55 %.".into())),
            "{applied:?}"
        );
        assert_eq!(s.status().unwrap().tx_gain, Some(55.0));
        let cap = s.handle(
            Event::ControlReply {
                from: Socket::Control,
                reply: Reply::Ok(serde_json::json!({"ok":true,"tx_audio_cap_s":120})),
            },
            LocalTime(0.3),
        );
        assert!(cap.contains(&Action::Announce("Bridge set TX audio cap 120 s.".into())));
        assert_eq!(s.status().unwrap().tx_audio_cap_s, Some(120));
        let refused = s.handle(
            Event::ControlReply {
                from: Socket::Control,
                reply: Reply::Error("bad_tx_audio_cap".into()),
            },
            LocalTime(0.4),
        );
        assert!(refused.iter().any(
            |a| matches!(a, Action::Announce(m) if m.starts_with("Bridge refused the change"))
        ));
        // A status reply still parses as status, not as a level reply.
        let st = s.handle(
            Event::ControlReply {
                from: Socket::Control,
                reply: Reply::Ok(serde_json::json!({"ok":true,"rx_packets_sent":5,"peer":null,"uptime_s":9,"tx_gain":40})),
            },
            LocalTime(0.5),
        );
        assert!(st.iter().all(|a| !matches!(a, Action::Announce(_))));
        assert_eq!(s.status().unwrap().tx_gain, Some(40.0));
    }
}
