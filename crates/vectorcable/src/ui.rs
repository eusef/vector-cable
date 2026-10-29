//! The terminal UI, brief section 8: header row, readiness cards, Link
//! panel, status region and key hints, drawn from a [`View`] with ratatui.
//! Every verdict is a glyph plus a word; colour never carries meaning alone.
//! Plain words come first on each tab: radio control (CAT), keying (PTT),
//! audio drive (ALC).

use std::net::{IpAddr, SocketAddr};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap};
use ratatui::Frame;

use vbcore::rigrelay::{self, RelayState};
use vbcore::runner::Snapshot;
use vbcore::session::{Link, Mode};
use vbcore::status::{dash, LadderState, PeerSlot, RigctlAccess, RigctlState};

use crate::theme::{Theme, ThemeName};

/// Longest sentence a readiness card shows: it fits on one row of a card at
/// 80 columns (card 40 wide, border and padding take 4).
#[cfg(test)]
pub const CARD_SENTENCE_MAX: usize = 36;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Status,
    Setup,
    Check,
    Levels,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Status, Tab::Setup, Tab::Check, Tab::Levels];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Status => "Status",
            Tab::Setup => "Setup",
            Tab::Check => "Check",
            Tab::Levels => "Levels",
        }
    }

    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    pub fn next(self) -> Tab {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    pub fn previous(self) -> Tab {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }
}

/// Status vocabulary from the brief: glyph plus word, with an ASCII fallback
/// when the locale is not UTF-8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    Ok,
    Check,
    Fix,
    Locked,
    Transmitting,
    ListenOnly,
}

impl Glyph {
    pub fn text(self, ascii: bool) -> &'static str {
        match (self, ascii) {
            (Glyph::Ok, false) => "\u{2713}",
            (Glyph::Ok, true) => "[ok]",
            (Glyph::Check, false) => "\u{25b2}",
            (Glyph::Check, true) => "[!]",
            (Glyph::Fix, false) => "\u{2717}",
            (Glyph::Fix, true) => "[x]",
            (Glyph::Locked, false) => "\u{25a0}",
            (Glyph::Locked, true) => "[lock]",
            (Glyph::Transmitting, false) => "\u{25cf}",
            (Glyph::Transmitting, true) => "[tx]",
            (Glyph::ListenOnly, false) => "\u{25cb}",
            (Glyph::ListenOnly, true) => "[o]",
        }
    }
}

/// The one-word answer on a readiness card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Ready,
    Locked,
    Transmitting,
    /// Not ready yet: still connecting or checking. A warning, never a fault.
    NotReadyPending,
    /// Not ready because something is wrong; the card says what to do.
    NotReadyFault,
    ListenOnly,
}

impl Verdict {
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Ready => "Ready",
            Verdict::Locked => "Locked",
            Verdict::Transmitting => "Transmitting",
            Verdict::NotReadyPending | Verdict::NotReadyFault => "Not Ready",
            Verdict::ListenOnly => "Listen Only",
        }
    }

    pub fn glyph(self) -> Glyph {
        match self {
            Verdict::Ready => Glyph::Ok,
            Verdict::Locked => Glyph::Locked,
            Verdict::Transmitting => Glyph::Transmitting,
            Verdict::NotReadyPending => Glyph::Check,
            Verdict::NotReadyFault => Glyph::Fix,
            Verdict::ListenOnly => Glyph::ListenOnly,
        }
    }

    /// Glyph plus word.
    pub fn text(self, ascii: bool) -> String {
        format!("{} {}", self.glyph().text(ascii), self.word())
    }

    #[cfg(test)]
    pub const ALL: [Verdict; 6] = [
        Verdict::Ready,
        Verdict::Locked,
        Verdict::Transmitting,
        Verdict::NotReadyPending,
        Verdict::NotReadyFault,
        Verdict::ListenOnly,
    ];
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    pub title: &'static str,
    pub verdict: Verdict,
    /// One plain-words sentence under the verdict, at most [`CARD_SENTENCE_MAX`].
    pub detail: String,
    /// What to do, when the verdict is Not Ready for a fault ("Fix: " is added).
    pub fix: Option<String>,
}

const FIX_BRIDGE: &str = "Check bridge power and network";
const FIX_TAILSCALE: &str = "Start Tailscale on this computer";

/// Tailscale gives out addresses in 100.64.0.0/10.
fn on_tailnet(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v4) if v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64)
}

/// The fix for a lost bridge. A bridge on a Tailscale address reached from an
/// address off the tailnet means Tailscale is not running here.
fn fix_bridge(s: &Snapshot) -> String {
    let tailscale_off = on_tailnet(s.bridge.ip()) && s.local_ip.is_some_and(|ip| !on_tailnet(ip));
    if tailscale_off {
        FIX_TAILSCALE
    } else {
        FIX_BRIDGE
    }
    .to_string()
}

fn dbfs(v: Option<f64>) -> String {
    dash(v.map(|d| format!("{d:.0}")))
}

/// Receive readiness: RX audio arriving from the bridge.
pub fn receive_card(s: &Snapshot) -> Card {
    let (verdict, detail, fix) = match s.link {
        Link::Searching => (
            Verdict::NotReadyPending,
            "Looking for the bridge".to_string(),
            None,
        ),
        Link::Connected if s.rx.received == 0 => (
            Verdict::NotReadyPending,
            "Waiting for RX audio".to_string(),
            None,
        ),
        Link::Connected => (
            Verdict::Ready,
            format!("RX audio from the bridge, {} dBFS", dbfs(s.rx_dbfs)),
            None,
        ),
        Link::RxPaused => (
            Verdict::NotReadyPending,
            "Bridge up, RX audio paused".to_string(),
            None,
        ),
        Link::Lost => (
            Verdict::NotReadyFault,
            "Bridge lost".to_string(),
            Some(fix_bridge(s)),
        ),
    };
    Card {
        title: "Receive",
        verdict,
        detail,
        fix,
    }
}

/// Transmit readiness: the slot, the link and the bridge's transmit checks.
/// Ready only once the radio control (CAT) check is known to pass, or the
/// rig is VOX; unknown or pending is Not Ready with a warning.
pub fn transmit_card(s: &Snapshot) -> Card {
    let cap_tripped = s
        .status
        .as_ref()
        .is_some_and(|st| st.tx_audio_muted == Some(true));
    let (verdict, detail, fix) = match (s.mode, s.link) {
        (Mode::ListenOnly, _) if s.slot_held_by_other => (
            Verdict::ListenOnly,
            "Another client holds the slot".to_string(),
            None,
        ),
        (Mode::ListenOnly, _) => (Verdict::ListenOnly, "Press t to transmit".to_string(), None),
        (Mode::Transmit, Link::Searching) => (
            Verdict::NotReadyPending,
            "Looking for the bridge".to_string(),
            None,
        ),
        (Mode::Transmit, Link::Lost) => (
            Verdict::NotReadyFault,
            "Link lost: TX stopped".to_string(),
            Some(fix_bridge(s)),
        ),
        (Mode::Transmit, _) if s.streaming => (
            Verdict::Transmitting,
            format!("TX audio {} dBFS", dbfs(s.tx_dbfs)),
            None,
        ),
        (Mode::Transmit, _) => match &s.ladder {
            Some(LadderState::VoxRig) => (
                Verdict::Ready,
                "VOX rig: your audio keys the radio".to_string(),
                None,
            ),
            Some(LadderState::Cat {
                tx_allowed: Some(false),
                ..
            }) => (
                Verdict::Locked,
                "Bridge transmit checks not passed".to_string(),
                None,
            ),
            Some(LadderState::Cat {
                tx_allowed: Some(true),
                ..
            }) if cap_tripped => (
                Verdict::Locked,
                "TX audio cap tripped on the bridge".to_string(),
                None,
            ),
            Some(LadderState::Cat {
                tx_allowed: Some(true),
                ..
            }) => (
                Verdict::Ready,
                "Audio from the app goes to air".to_string(),
                None,
            ),
            Some(LadderState::Unavailable {
                status: Some(code), ..
            }) => (
                Verdict::NotReadyPending,
                format!("Radio control check: HTTP {code}"),
                None,
            ),
            _ => (
                Verdict::NotReadyPending,
                "Checking radio control (CAT)".to_string(),
                None,
            ),
        },
    };
    Card {
        title: "Transmit",
        verdict,
        detail,
        fix,
    }
}

/// The status region's sentence when a verdict changes, as in the mockup.
pub fn verdict_sentence(s: &Snapshot) -> String {
    format!(
        "Receive {}. Transmit {}.",
        receive_card(s).verdict.word(),
        transmit_card(s).verdict.word()
    )
}

/// Link word with its glyph for the header: only the vocabulary words.
pub fn link_badge(link: Link) -> (Glyph, &'static str) {
    match link {
        Link::Connected => (Glyph::Ok, "Good"),
        Link::Searching | Link::RxPaused => (Glyph::Check, "Check"),
        Link::Lost => (Glyph::Fix, "Fix"),
    }
}

/// Why the link is in its state, for the status region.
pub fn link_reason(link: Link) -> &'static str {
    match link {
        Link::Connected => "replies and RX audio arriving",
        Link::Searching => "looking for the bridge",
        Link::RxPaused => "bridge up, RX audio paused",
        Link::Lost => "no reply for 10 s, reconnecting",
    }
}

/// A doctor-style check line on the Check tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckLine {
    pub name: &'static str,
    pub glyph: Glyph,
    pub word: &'static str,
    pub detail: String,
}

/// The checks the Check tab and `doctor` share, from what the runner knows.
pub fn checks(s: &Snapshot, rigctl_port_open: Option<bool>) -> Vec<CheckLine> {
    let mut out = Vec::new();
    let (glyph, word, detail) = match s.link {
        Link::Searching => (Glyph::Check, "Check", "no reply yet".to_string()),
        Link::Lost => (Glyph::Fix, "Fix", "no reply for 10 s".to_string()),
        _ => (
            Glyph::Ok,
            "Good",
            format!(
                "{} version {} protocol {}",
                s.bridge.ip(),
                dash(s.version.as_ref().map(|v| v.version.clone())),
                dash(s.hello.as_ref().and_then(|h| h.protocol))
            ),
        ),
    };
    out.push(CheckLine {
        name: "Bridge reachable",
        glyph,
        word,
        detail,
    });
    let (glyph, word, detail) = match (&s.audio_fault, s.audio_name.as_str()) {
        (Some(fault), _) => (Glyph::Fix, "Fix", fault.clone()),
        (None, "null") => (
            Glyph::Check,
            "Check",
            "no virtual audio devices in this build (null adapter)".to_string(),
        ),
        (None, "pipewire") => (
            Glyph::Ok,
            "Good",
            "VectorBridge RX and VectorBridge TX (PipeWire)".to_string(),
        ),
        (None, "coreaudio") => (
            Glyph::Ok,
            "Good",
            "VectorBridge RX and VectorBridge TX (Core Audio)".to_string(),
        ),
        (None, name) => (Glyph::Ok, "Good", format!("{name} adapter")),
    };
    out.push(CheckLine {
        name: "Audio devices",
        glyph,
        word,
        detail,
    });
    let (glyph, word, detail) = match (&s.rigctl, rigctl_port_open) {
        (
            Some(RigctlState::Proxy {
                access,
                port,
                login_required,
                ..
            }),
            open,
        ) => {
            let access_word = match access {
                Some(RigctlAccess::All) => "all",
                Some(RigctlAccess::Wifi) => "wifi",
                Some(RigctlAccess::Ap) => "ap",
                Some(RigctlAccess::Off) => "off",
                None => "-",
            };
            let port = port.unwrap_or(4532);
            match open {
                // The maintainer, 2026-10-28: every app off the Pi logs in, so the
                // port is locked wherever it answers; the login is the lock of brief
                // section 8, so even `all` is Good (his answer, 2026-10-29).
                Some(true) if *login_required => (
                    Glyph::Ok,
                    "Good",
                    format!(
                        "TCP {port} open, access {access_word}; only apps logged in with the bridge PIN can use it (VectorCable logs in for them)"
                    ),
                ),
                // Brief section 8: reachable from all networks with no lock is Check.
                Some(true) if *access == Some(RigctlAccess::All) => (
                    Glyph::Check,
                    "Check",
                    format!("TCP {port} open to all networks (rigctl_access all)"),
                ),
                // The maintainer, 2026-10-14: `wifi` is open by design and stays so after
                // bridge 1.10, so it is Good, with who can reach it.
                Some(true) if *access == Some(RigctlAccess::Wifi) => (
                    Glyph::Ok,
                    "Good",
                    format!(
                        "TCP {port} open to this Wi-Fi network; anyone on it can tune and key through the safety proxy"
                    ),
                ),
                Some(true) => (
                    Glyph::Ok,
                    "Good",
                    format!("TCP {port} open, access {access_word}"),
                ),
                Some(false) => {
                    let fix = match access {
                        Some(RigctlAccess::Wifi) if !on_bridge_ap(s.bridge.ip()) => {
                            "set rigctl_access to all for the tailnet"
                        }
                        Some(RigctlAccess::Wifi | RigctlAccess::All) => {
                            "check the bridge's rigctl proxy"
                        }
                        _ => "set rigctl_access to wifi, or all for the tailnet",
                    };
                    (
                        Glyph::Fix,
                        "Fix",
                        format!("TCP {port} closed, access {access_word}; {fix}"),
                    )
                }
                None => (
                    Glyph::Check,
                    "Check",
                    format!("TCP {port}, access {access_word}"),
                ),
            }
        }
        (Some(RigctlState::NoProxy), _) => (
            Glyph::Check,
            "Check",
            "TCP 4532, no rigctl proxy on the bridge".into(),
        ),
        _ => (Glyph::Check, "Check", "TCP 4532, not known yet".into()),
    };
    out.push(CheckLine {
        name: "Radio control (CAT)",
        glyph,
        word,
        detail,
    });
    let (glyph, word, detail) = match s.clock {
        Some(c) if c.warn => (
            Glyph::Check,
            "Check",
            format!(
                "{:+.1} s from {} decodes; sync the clock",
                c.clock_error_s, c.decodes
            ),
        ),
        Some(c) => (
            Glyph::Ok,
            "Good",
            format!("{:+.1} s from {} decodes", c.clock_error_s, c.decodes),
        ),
        None => (Glyph::Check, "Check", "no WSJT-X decodes yet".into()),
    };
    out.push(CheckLine {
        name: "Clock error",
        glyph,
        word,
        detail,
    });
    let (glyph, word, detail) = match s.status.as_ref().map(|st| &st.peer) {
        Some(PeerSlot::Held(a)) if s.slot_held_by_other => (
            Glyph::Check,
            "Check",
            format!("another client holds the slot: {a}"),
        ),
        Some(PeerSlot::Held(_)) => (Glyph::Ok, "Good", "we hold the slot".into()),
        Some(PeerSlot::Empty) => (Glyph::Ok, "Good", "slot free".into()),
        _ => (Glyph::Check, "Check", "not known yet".into()),
    };
    out.push(CheckLine {
        name: "Peer slot",
        glyph,
        word,
        detail,
    });
    let (glyph, word, detail) = match s.status.as_ref() {
        Some(st) => {
            let cap = st.tx_audio_cap_s.unwrap_or(0);
            if st.tx_audio_muted == Some(true) {
                (
                    Glyph::Fix,
                    "Fix",
                    "TX audio cap tripped; raise it on Levels".into(),
                )
            } else if s.mode == Mode::Transmit && cap > 0 && cap < 120 {
                (
                    Glyph::Check,
                    "Check",
                    format!("cap {cap} s is shorter than a WSJT-X cycle"),
                )
            } else {
                (
                    Glyph::Ok,
                    "Good",
                    format!("cap {} s", dash(st.tx_audio_cap_s)),
                )
            }
        }
        None => (Glyph::Check, "Check", "not known yet".into()),
    };
    out.push(CheckLine {
        name: "TX audio cap",
        glyph,
        word,
        detail,
    });
    out
}

/// Which app the Setup tab shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum App {
    WsjtX,
    Js8Call,
    Fldigi,
}

impl App {
    pub fn name(self) -> &'static str {
        match self {
            App::WsjtX => "WSJT-X",
            App::Js8Call => "JS8Call",
            App::Fldigi => "FLDIGI",
        }
    }

    pub fn parse(s: &str) -> Option<App> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "wsjt-x" | "wsjtx" => Some(App::WsjtX),
            "js8call" | "js8" => Some(App::Js8Call),
            "fldigi" => Some(App::Fldigi),
            _ => None,
        }
    }

    /// Settings step by step: (where, what to set).
    pub fn steps(self, rigctl: &str, vox: bool) -> Vec<(String, String)> {
        let ptt = if vox {
            "VOX, the audio keys the radio".to_string()
        } else {
            "CAT, radio control keys the radio".to_string()
        };
        match self {
            App::WsjtX | App::Js8Call => vec![
                ("Settings > Audio > Input".into(), "VectorBridge RX".into()),
                ("Settings > Audio > Output".into(), "VectorBridge TX".into()),
                ("Settings > Radio > Rig".into(), "Hamlib NET rigctl".into()),
                ("Settings > Radio > Network Server".into(), rigctl.into()),
                ("Settings > Radio > PTT method".into(), ptt),
            ],
            App::Fldigi => vec![
                (
                    "Configure > Soundcard > Devices > Capture".into(),
                    "VectorBridge RX".into(),
                ),
                (
                    "Configure > Soundcard > Devices > Playback".into(),
                    "VectorBridge TX".into(),
                ),
                (
                    "Configure > Rig Control > Hamlib > Device".into(),
                    "Hamlib NET rigctl".into(),
                ),
                (
                    "Configure > Rig Control > Hamlib > Server".into(),
                    rigctl.into(),
                ),
                ("Configure > Rig Control > PTT method".into(), ptt),
            ],
        }
    }
}

/// First line of the Setup tab and `setup`: the plain words before the acronyms.
pub fn setup_intro(app: App) -> String {
    format!(
        "{}: audio devices, radio control (CAT) and keying (PTT).",
        app.name()
    )
}

/// The gain control the Levels tab has selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelControl {
    TxGain,
    RxGain,
    TxCap,
}

impl LevelControl {
    pub fn next(self) -> LevelControl {
        match self {
            LevelControl::TxGain => LevelControl::RxGain,
            LevelControl::RxGain => LevelControl::TxCap,
            LevelControl::TxCap => LevelControl::TxGain,
        }
    }

    pub fn previous(self) -> LevelControl {
        self.next().next()
    }
}

/// Levels can change once the bridge's status has reported the current values.
pub fn levels_known(s: &Snapshot) -> bool {
    s.status
        .as_ref()
        .is_some_and(|st| st.tx_gain.is_some() && st.tx_audio_cap_s.is_some())
}

/// Packet loss as a percentage of packets expected, a dash before any data.
pub fn loss_percent(lost: Option<u64>, received: Option<u64>) -> String {
    match (lost, received) {
        (Some(l), Some(r)) if l + r > 0 => format!("{:.1} %", 100.0 * l as f64 / (l + r) as f64),
        _ => format!("{} %", dash::<String>(None)),
    }
}

/// Everything the renderer needs: the runner's snapshot plus UI state.
#[derive(Clone, Debug)]
pub struct View {
    pub snapshot: Snapshot,
    pub tab: Tab,
    pub theme: Theme,
    pub ascii: bool,
    /// `HH:mm:ssZ`.
    pub clock: String,
    /// The status region: the latest announcements, newest last.
    pub status_lines: Vec<String>,
    /// A framed alert over the status region (a regression), until dismissed.
    pub alert: Option<String>,
    pub app: App,
    pub level: LevelControl,
    /// A level change waiting for the first-change confirmation.
    pub confirm: Option<String>,
    pub rigctl_port_open: Option<bool>,
    pub help: bool,
    /// Digits typed so far while the bridge PIN is being entered (never the digits).
    pub pin_entry: Option<usize>,
}

const CARD_HEIGHT: u16 = 5;

pub fn draw(frame: &mut Frame, view: &View) {
    let area = frame.area();
    frame.render_widget(Block::default().style(view.theme.background()), area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);
    draw_header(frame, rows[0], view);
    match view.tab {
        Tab::Status => draw_status_tab(frame, rows[1], view),
        Tab::Setup => draw_setup_tab(frame, rows[1], view),
        Tab::Check => draw_check_tab(frame, rows[1], view),
        Tab::Levels => draw_levels_tab(frame, rows[1], view),
    }
    draw_status_region(frame, rows[2], view);
    draw_hints(frame, rows[3], view);
}

fn strong(t: &Theme) -> Style {
    t.on(
        &crate::theme::TEXT_PRIMARY,
        &crate::theme::PANEL_FILL,
        Modifier::BOLD,
    )
}

fn draw_header(frame: &mut Frame, area: Rect, view: &View) {
    let t = &view.theme;
    let mut spans = vec![
        Span::styled(" VectorCable", strong(t)),
        Span::styled("  ", t.panel()),
    ];
    for tab in Tab::ALL {
        let label = format!(" {} ", tab.title());
        if tab == view.tab {
            spans.push(Span::styled(label, t.selected_tab()));
        } else {
            spans.push(Span::styled(label, t.secondary()));
        }
        spans.push(Span::styled(" ", t.panel()));
    }
    if view.snapshot.streaming {
        spans.push(Span::styled(" TX ", t.tx_pill()));
    }
    let (glyph, word) = link_badge(view.snapshot.link);
    let link_style = match glyph {
        Glyph::Ok => t.good(),
        Glyph::Fix => t.fault(),
        _ => t.warning(),
    };
    let right = vec![
        Span::styled("Link ", t.secondary()),
        Span::styled(format!("{} {}", glyph.text(view.ascii), word), link_style),
        Span::styled("  ", t.panel()),
        Span::styled(view.clock.clone(), t.text()),
        Span::styled(" ", t.panel()),
    ];
    let right_len: usize = right.iter().map(|s| s.content.chars().count()).sum();
    let left_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = (area.width as usize).saturating_sub(left_len + right_len);
    spans.push(Span::styled(" ".repeat(pad), t.panel()));
    spans.extend(right);
    frame.render_widget(Paragraph::new(Line::from(spans)).style(t.panel()), area);
}

/// A bordered panel with one cell of padding inside, where the width allows.
fn panel<'a>(view: &View, title: &'a str, area: Rect) -> Block<'a> {
    let t = &view.theme;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(t.border())
        .style(t.panel())
        .title(Span::styled(format!(" {title} "), strong(t)));
    let block = if area.width >= 8 {
        block.padding(Padding::horizontal(1))
    } else {
        block
    };
    if view.ascii {
        block.border_type(BorderType::Plain)
    } else {
        block.border_type(BorderType::Rounded)
    }
}

fn verdict_style(view: &View, verdict: Verdict) -> Style {
    let t = &view.theme;
    match verdict {
        Verdict::Ready => t.good(),
        Verdict::Locked | Verdict::ListenOnly => t.locked(),
        Verdict::Transmitting => t.tx_pill(),
        Verdict::NotReadyPending => t.warning(),
        Verdict::NotReadyFault => t.fault(),
    }
}

fn draw_card(frame: &mut Frame, area: Rect, view: &View, card: &Card) {
    let block = panel(view, card.title, area);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = vec![
        Line::from(Span::styled(
            card.verdict.text(view.ascii),
            verdict_style(view, card.verdict),
        )),
        Line::from(Span::styled(card.detail.clone(), view.theme.text())),
    ];
    if let Some(fix) = &card.fix {
        lines.push(Line::from(Span::styled(
            format!("Fix: {fix}"),
            view.theme.fault(),
        )));
    }
    frame.render_widget(Paragraph::new(lines).style(view.theme.panel()), inner);
}

fn draw_status_tab(frame: &mut Frame, area: Rect, view: &View) {
    let receive = receive_card(&view.snapshot);
    let transmit = transmit_card(&view.snapshot);
    if area.width >= 80 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(CARD_HEIGHT), Constraint::Min(3)])
            .split(area);
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[0]);
        draw_card(frame, cols[0], view, &receive);
        draw_card(frame, cols[1], view, &transmit);
        draw_link_panel(frame, rows[1], view);
    } else {
        // Narrow: the cards stack, the Link panel takes what is left.
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(CARD_HEIGHT),
                Constraint::Length(CARD_HEIGHT),
                Constraint::Min(3),
            ])
            .split(area);
        draw_card(frame, rows[0], view, &receive);
        draw_card(frame, rows[1], view, &transmit);
        draw_link_panel(frame, rows[2], view);
    }
}

fn meter(view: &View, label: &str, dbfs_value: Option<f64>, width: usize) -> Line<'static> {
    let t = &view.theme;
    let (fill_ch, track_ch) = if view.ascii {
        ("#", "-")
    } else {
        ("\u{2588}", "\u{2591}")
    };
    let width = width.max(4);
    let filled = match dbfs_value {
        Some(d) => (((d + 60.0) / 60.0).clamp(0.0, 1.0) * width as f64).round() as usize,
        None => 0,
    };
    Line::from(vec![
        Span::styled(format!("{label} "), t.secondary()),
        Span::styled(fill_ch.repeat(filled), t.meter_fill()),
        Span::styled(track_ch.repeat(width - filled), t.meter_track()),
        Span::styled(format!(" {} dBFS", dbfs(dbfs_value)), t.secondary()),
    ])
}

fn line_width(line: &Line) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Packs short labelled values into rows that fit `width`, two spaces apart.
fn pack(items: Vec<Vec<Span<'static>>>, width: usize, t: &Theme) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();
    for item in items {
        let item_width: usize = item.iter().map(|s| s.content.chars().count()).sum();
        match rows.last_mut() {
            Some(row) if line_width(row) + 2 + item_width <= width => {
                row.spans.push(Span::styled("  ", t.panel()));
                row.spans.extend(item);
            }
            _ => rows.push(Line::from(item)),
        }
    }
    rows
}

fn draw_link_panel(frame: &mut Frame, area: Rect, view: &View) {
    let s = &view.snapshot;
    let t = &view.theme;
    let block = panel(view, "Link", area);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let status = s.status.as_ref();
    let have_rx = s.rx.received > 0;
    let have_tx = s.tx.audio_packets > 0;
    let rx_loss = loss_percent(Some(s.rx.lost), Some(s.rx.received));
    let tx_loss = loss_percent(
        status.and_then(|st| st.tx_packet_loss),
        status.and_then(|st| st.tx_packets_received),
    );
    let pair = |label: &str, value: String, style: Style| {
        vec![
            Span::styled(format!("{label} "), t.secondary()),
            Span::styled(value, style),
        ]
    };
    let clock_style = match s.clock {
        Some(c) if c.warn => t.warning(),
        _ => t.text(),
    };
    let clock_value = match s.clock {
        Some(c) if c.warn => format!(
            "{:+.1} s {} Check",
            c.clock_error_s,
            Glyph::Check.text(view.ascii)
        ),
        Some(c) => format!("{:+.1} s", c.clock_error_s),
        None => dash::<String>(None),
    };
    let items = vec![
        pair(
            "Bridge",
            format!(
                "{}  version {}  protocol {}",
                s.bridge.ip(),
                dash(s.version.as_ref().map(|v| v.version.clone())),
                dash(s.hello.as_ref().and_then(|h| h.protocol))
            ),
            t.text(),
        ),
        pair("Mode", s.mode.word().to_string(), t.text()),
        pair(
            "Apps row",
            dash(s.listing.as_ref().map(|l| {
                if l.wants_pin() {
                    format!("{} (press p)", l.word())
                } else {
                    l.word()
                }
            })),
            t.text(),
        ),
        pair("Loss rx", rx_loss, t.text()),
        pair("tx", tx_loss, t.text()),
        pair(
            "Underruns here",
            dash(have_rx.then_some(s.playback.underrun_samples / 480)),
            t.text(),
        ),
        pair(
            "bridge",
            dash(status.and_then(|st| st.tx_underruns).filter(|_| have_tx)),
            t.text(),
        ),
        pair(
            "Buffer",
            format!("{} ms", dash(s.buffer_ms.map(|b| format!("{b:.0}")))),
            t.text(),
        ),
        pair(
            "Walk",
            format!(
                "{} ppm",
                dash(have_rx.then(|| format!("{:+.0}", s.trim_ppm)))
            ),
            t.text(),
        ),
        pair("Clock", clock_value, clock_style),
    ];
    let mut lines = pack(items, inner.width as usize, t);
    let width = (inner.width as usize).saturating_sub(13).min(40);
    lines.push(meter(view, "RX", s.rx_dbfs, width));
    lines.push(meter(view, "TX", s.tx_dbfs, width));
    frame.render_widget(Paragraph::new(lines).style(t.panel()), inner);
}

fn draw_setup_tab(frame: &mut Frame, area: Rect, view: &View) {
    let t = &view.theme;
    let block = panel(view, "Setup", area);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let s = &view.snapshot;
    let rigctl = rigctl_address(s);
    let vox = matches!(s.ladder, Some(LadderState::VoxRig));
    let mut lines = vec![
        Line::from(Span::styled(setup_intro(view.app), t.text())),
        Line::from(vec![
            Span::styled("App  ", t.secondary()),
            Span::styled("w WSJT-X   j JS8Call   f FLDIGI   (chosen: ", t.text()),
            Span::styled(view.app.name(), strong(t)),
            Span::styled(")", t.text()),
        ]),
        Line::from(""),
    ];
    for (i, (where_, what)) in view.app.steps(&rigctl, vox).iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!("{}. ", i + 1), t.secondary()),
            Span::styled(where_.clone(), t.secondary()),
            Span::styled(": ", t.secondary()),
            Span::styled(what.clone(), t.text()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(relay_note(s), t.secondary())));
    lines.push(Line::from(Span::styled(rigctl_note(s), t.secondary())));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .style(t.panel()),
        inner,
    );
}

/// The rigctl host and port the apps should use: VectorCable's local relay,
/// unless it could not start and the bridge takes apps straight. With no runner (`setup`), the relay's address,
/// since the apps need VectorCable running for the cables anyway.
pub fn rigctl_address(s: &Snapshot) -> String {
    match &s.rig_relay {
        Some(RelayState::Listening(addr)) => addr.to_string(),
        Some(RelayState::Failed { .. }) if bridge_takes_apps_straight(s) => {
            bridge_rigctl_address(s)
        }
        Some(RelayState::Failed { listen, .. }) => listen.to_string(),
        None => rigrelay::listen_address(rigrelay::RIG_PORT).to_string(),
    }
}

/// A bridge without `vb_auth` still takes an app straight on 4532. One with it
/// (or one not read yet) may take an app off the Pi only once it logs in (every
/// such app since the maintainer's ruling of 2026-10-28), so only through the
/// relay.
fn bridge_takes_apps_straight(s: &Snapshot) -> bool {
    matches!(&s.rigctl, Some(RigctlState::Proxy { vb_auth: false, .. }))
}

/// The bridge's own rigctl proxy, which the relay carries the apps to.
fn bridge_rigctl_address(s: &Snapshot) -> String {
    let port = match &s.rigctl {
        Some(RigctlState::Proxy { port: Some(p), .. }) => *p,
        _ => rigrelay::RIG_PORT,
    };
    format!("{}:{port}", s.bridge.ip())
}

/// How to move the relay off a port another program holds.
fn other_port_advice(listen: SocketAddr) -> String {
    let next = listen
        .port()
        .checked_add(1)
        .unwrap_or(rigrelay::RIG_PORT + 1);
    let file = crate::config::path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "VectorCable's config file".into());
    format!(
        "quit that program, or start VectorCable with --rig-port {next} (or put rig_port = {next} in {file}) and point the app at 127.0.0.1:{next}"
    )
}

/// `setup` found the relay's port taken on this computer.
pub fn relay_port_busy_note(s: &Snapshot, listen: SocketAddr) -> String {
    let start = format!(
        "Port {} on this computer is in use: by VectorCable's relay if VectorCable is running; otherwise by another program (often a local rigctld)",
        listen.port()
    );
    if bridge_takes_apps_straight(s) {
        format!(
            "{start}, and then point the app straight at the bridge at {}.",
            bridge_rigctl_address(s)
        )
    } else {
        format!(
            "{start}. The bridge takes rig control only through VectorCable, so if it is another program, {}.",
            other_port_advice(listen)
        )
    }
}

/// What the relay does, or why the apps have no rig control or go straight
/// to the bridge.
pub fn relay_note(s: &Snapshot) -> String {
    let bridge = bridge_rigctl_address(s);
    match &s.rig_relay {
        Some(RelayState::Failed { listen, error }) if bridge_takes_apps_straight(s) => format!(
            "VectorCable's rig control relay could not start ({listen}: {error}), so the app goes straight to the bridge at {bridge}."
        ),
        Some(RelayState::Failed { listen, error }) => format!(
            "VectorCable's rig control relay could not start ({listen}: {error}), and the bridge takes rig control only through it: {}, then restart VectorCable.",
            other_port_advice(*listen)
        ),
        _ => format!(
            "While VectorCable runs it carries rig control to the bridge at {bridge}, logging in with the bridge's app token where the bridge asks for it."
        ),
    }
}

/// The bridge's own access point hands out 10.42.x.x (FACT: 10.42.0.1 and
/// 10.42.1.1 seen on the bench).
fn on_bridge_ap(ip: std::net::IpAddr) -> bool {
    matches!(ip, std::net::IpAddr::V4(v4) if v4.octets()[..2] == [10, 42])
}

pub fn rigctl_note(s: &Snapshot) -> String {
    const LOCKED: &str =
        "only apps logged in with the bridge PIN can use it, which VectorCable does for them";
    match &s.rigctl {
        Some(RigctlState::Proxy {
            access: Some(RigctlAccess::All),
            login_required: true,
            ..
        }) => format!("Radio control (CAT) is open to every network (rigctl_access all); {LOCKED}."),
        Some(RigctlState::Proxy {
            access: Some(RigctlAccess::Wifi),
            login_required: true,
            ..
        }) => format!("Radio control (CAT) answers on the bridge's Wi-Fi (its access point or the network it joined), not the tailnet; {LOCKED}."),
        Some(RigctlState::Proxy {
            access: Some(RigctlAccess::All),
            ..
        }) => "Radio control (CAT) is open to every network (rigctl_access all).".to_string(),
        Some(RigctlState::Proxy {
            access: Some(RigctlAccess::Ap),
            ..
        }) => "Radio control (CAT) answers only on the bridge's access point; elsewhere set rigctl_access to wifi on the bridge (all for the tailnet).".to_string(),
        Some(RigctlState::Proxy {
            access: Some(RigctlAccess::Wifi),
            ..
        }) => "Radio control (CAT) answers on the bridge's Wi-Fi (its access point or the network it joined), not the tailnet; anyone on that Wi-Fi can tune and key through the safety proxy.".to_string(),
        Some(RigctlState::Proxy {
            access: Some(RigctlAccess::Off),
            ..
        }) => "Radio control (CAT) is off on the bridge; turn rigctl_access on, or use VOX.".to_string(),
        Some(RigctlState::NoProxy) => "This bridge has no rigctl proxy: use VOX.".to_string(),
        _ => "Radio control (CAT) access not known yet.".to_string(),
    }
}

fn check_style(view: &View, glyph: Glyph) -> Style {
    match glyph {
        Glyph::Ok => view.theme.good(),
        Glyph::Fix => view.theme.fault(),
        Glyph::Locked | Glyph::ListenOnly => view.theme.locked(),
        Glyph::Transmitting => view.theme.tx_pill(),
        Glyph::Check => view.theme.warning(),
    }
}

/// Where a check's detail starts: name (20), glyph and word (9), two spaces.
const CHECK_DETAIL_COL: usize = 31;

/// `text` broken at spaces into lines of at most `width` characters; a word
/// longer than `width` is split.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let mut word: Vec<char> = word.chars().collect();
        while !word.is_empty() {
            let used = line.chars().count();
            let gap = usize::from(used > 0);
            if used + gap + word.len() <= width {
                if gap == 1 {
                    line.push(' ');
                }
                line.extend(word.drain(..));
            } else if used > 0 {
                out.push(std::mem::take(&mut line));
            } else {
                line.extend(word.drain(..width));
                out.push(std::mem::take(&mut line));
            }
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

fn draw_check_tab(frame: &mut Frame, area: Rect, view: &View) {
    let t = &view.theme;
    let block = panel(view, "Check", area);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // A detail too long for the row continues under the detail column, not
    // under the names (test 10: readable at 80 x 24).
    let detail_width = (inner.width as usize).saturating_sub(CHECK_DETAIL_COL);
    let mut lines: Vec<Line> = Vec::new();
    for c in checks(&view.snapshot, view.rigctl_port_open) {
        let parts = if detail_width >= 16 {
            wrap_words(&c.detail, detail_width)
        } else {
            vec![c.detail.clone()]
        };
        let mut parts = parts.into_iter();
        lines.push(Line::from(vec![
            Span::styled(format!("{:<20}", c.name), t.text()),
            Span::styled(
                format!("{:<9}", format!("{} {}", c.glyph.text(view.ascii), c.word)),
                check_style(view, c.glyph),
            ),
            Span::styled(
                format!("  {}", parts.next().unwrap_or_default()),
                t.secondary(),
            ),
        ]));
        for part in parts {
            lines.push(Line::from(Span::styled(
                format!("{:CHECK_DETAIL_COL$}{part}", ""),
                t.secondary(),
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .style(t.panel()),
        inner,
    );
}

fn draw_levels_tab(frame: &mut Frame, area: Rect, view: &View) {
    let t = &view.theme;
    let s = &view.snapshot;
    let block = panel(view, "Levels", area);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = (inner.width as usize).saturating_sub(13).min(40);
    let status = s.status.as_ref();
    let row = |name: &str, value: String, control: LevelControl| {
        let marker = if view.level == control { ">" } else { " " };
        Line::from(vec![
            Span::styled(format!("{marker} "), t.text()),
            Span::styled(format!("{name:<14}"), t.secondary()),
            Span::styled(
                value,
                if view.level == control {
                    strong(t)
                } else {
                    t.text()
                },
            ),
        ])
    };
    let rx_fixed = status.and_then(|st| st.rx_gain_fixed) == Some(true);
    let mut lines = vec![
        meter(view, "RX", s.rx_dbfs, width),
        meter(view, "TX", s.tx_dbfs, width),
        Line::from(""),
        row(
            "TX gain",
            format!(
                "{} %",
                dash(status.and_then(|st| st.tx_gain).map(|g| format!("{g:.0}")))
            ),
            LevelControl::TxGain,
        ),
        row(
            "RX gain",
            format!(
                "{} %{}",
                dash(status.and_then(|st| st.rx_gain).map(|g| format!("{g:.0}"))),
                if rx_fixed {
                    " (fixed on the bridge)"
                } else {
                    ""
                }
            ),
            LevelControl::RxGain,
        ),
        row(
            "TX audio cap",
            format!("{} s", dash(status.and_then(|st| st.tx_audio_cap_s))),
            LevelControl::TxCap,
        ),
        Line::from(""),
        Line::from(Span::styled(
            "TX gain sets the audio drive (ALC): keep the radio's ALC near zero.",
            t.secondary(),
        )),
    ];
    if levels_known(s) {
        lines.push(Line::from(Span::styled(
            "Up and Down change the selected value; Left and Right move between them.",
            t.secondary(),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "Waiting for the bridge to report the current levels.",
            t.warning(),
        )));
    }
    if let Some(c) = &view.confirm {
        lines.push(Line::from(Span::styled(
            format!("{c} This changes the level for every client. y to confirm, n to cancel."),
            t.warning(),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .style(t.panel()),
        inner,
    );
}

fn draw_status_region(frame: &mut Frame, area: Rect, view: &View) {
    let t = &view.theme;
    if let Some(alert) = &view.alert {
        // Three rows: a bordered box would leave one, so the alert is a
        // titled band and the text wraps across the rows below the title.
        frame.render_widget(Clear, area);
        frame.render_widget(Block::default().style(t.panel()), area);
        let title = Rect { height: 1, ..area };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(" Alert", strong(t)))).style(t.panel()),
            title,
        );
        let inner = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(1),
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{alert}  (Enter to dismiss)"),
                t.fault(),
            )))
            .wrap(Wrap { trim: false })
            .style(t.panel()),
            inner,
        );
        return;
    }
    if let Some(digits) = view.pin_entry {
        // The same band as an alert: the digits never reach the view, only a mask.
        frame.render_widget(Clear, area);
        frame.render_widget(Block::default().style(t.panel()), area);
        let title = Rect { height: 1, ..area };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(" Bridge PIN", strong(t)))).style(t.panel()),
            title,
        );
        let inner = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(1),
        };
        let mask = if view.ascii { "*" } else { "•" }.repeat(digits);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(format!("PIN: {mask}_"), t.text())),
                Line::from(Span::styled(
                    "4 to 8 digits. Enter to log in, Esc to cancel.",
                    t.secondary(),
                )),
            ])
            .style(t.panel()),
            inner,
        );
        return;
    }
    let lines: Vec<Line> = view
        .status_lines
        .iter()
        .rev()
        .take(area.height as usize)
        .rev()
        .map(|l| Line::from(Span::styled(format!(" {l}"), t.text())))
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .style(t.panel()),
        area,
    );
}

fn draw_hints(frame: &mut Frame, area: Rect, view: &View) {
    let t = &view.theme;
    let hints = if view.help {
        " 1-4 Tab tabs  t tx  l listen  n night  w/j/f app  arrows  q quit"
    } else {
        " 1-4 tabs  t transmit  l listen  n Night Ops  ? help  q quit"
    };
    let theme_word = match view.theme.name {
        ThemeName::Standard => "",
        ThemeName::NightOps => "  Night Ops",
    };
    // Only when the bridge wants it, and not on the longer help line (80 columns).
    let pin_hint = match &view.snapshot.listing {
        Some(l) if l.wants_pin() && !view.help => "  p PIN",
        _ => "",
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(hints, t.secondary()),
            Span::styled(pin_hint, t.secondary()),
            Span::styled(theme_word, t.secondary()),
        ]))
        .style(t.panel()),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{self, Depth, Theme, ThemeName};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use ratatui::Terminal;
    use vbcore::assembler::Stats as RxStats;
    use vbcore::clockcheck::ClockEstimate;
    use vbcore::playback::PlaybackStats;
    use vbcore::protocol::HelloReply;
    use vbcore::status::BridgeStatus;
    use vbcore::tx::TxStats;

    #[allow(dead_code)]
    mod source {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/build/tokens.rs"));
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            bridge: "10.42.0.1:5101".parse().unwrap(),
            local_audio_port: 40000,
            mode: Mode::ListenOnly,
            link: Link::Searching,
            hello: Some(HelloReply {
                protocol: Some(1),
                ..HelloReply::default()
            }),
            status: Some(BridgeStatus::default()),
            ladder: None,
            rigctl: None,
            version: None,
            rx: RxStats::default(),
            playback: PlaybackStats::default(),
            tx: TxStats::default(),
            rx_dbfs: Some(-20.0),
            tx_dbfs: None,
            buffer_ms: Some(220.0),
            trim_ppm: 12.0,
            clock: None,
            streaming: false,
            finished: false,
            listing: None,
            audio_name: "null".into(),
            audio_fault: None,
            slot_held_by_other: false,
            rig_relay: None,
            local_ip: None,
        }
    }

    #[test]
    fn a_lost_tailnet_bridge_reached_from_off_the_tailnet_says_start_tailscale() {
        let mut s = snapshot();
        s.mode = Mode::Transmit;
        s.link = Link::Lost;
        s.bridge = "100.64.0.1:5101".parse().unwrap();
        s.local_ip = Some("192.0.2.247".parse().unwrap());
        for card in [receive_card(&s), transmit_card(&s)] {
            assert_eq!(card.fix.as_deref(), Some(FIX_TAILSCALE), "{}", card.title);
        }
        // Tailscale up: the route to the bridge leaves from the tailnet address.
        s.local_ip = Some("100.64.0.2".parse().unwrap());
        assert_eq!(receive_card(&s).fix.as_deref(), Some(FIX_BRIDGE));
        // A bridge on the LAN, or no route known: the usual fix.
        s.local_ip = None;
        assert_eq!(receive_card(&s).fix.as_deref(), Some(FIX_BRIDGE));
        s.bridge = "192.0.2.17:5101".parse().unwrap();
        s.local_ip = Some("192.0.2.247".parse().unwrap());
        assert_eq!(transmit_card(&s).fix.as_deref(), Some(FIX_BRIDGE));
        // 100.128.0.0 is outside Tailscale's range.
        s.bridge = "100.128.0.1:5101".parse().unwrap();
        assert_eq!(receive_card(&s).fix.as_deref(), Some(FIX_BRIDGE));
    }

    fn view(theme: Theme) -> View {
        View {
            snapshot: snapshot(),
            tab: Tab::Status,
            theme,
            ascii: false,
            clock: "13:19:42Z".into(),
            status_lines: vec!["Mode Listen Only. Looking for the bridge.".into()],
            alert: None,
            app: App::WsjtX,
            level: LevelControl::TxGain,
            confirm: None,
            rigctl_port_open: None,
            help: false,
            pin_entry: None,
        }
    }

    fn render_at(view: &View, width: u16, height: u16) -> (Vec<String>, Buffer) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, view)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut rows = Vec::new();
        for y in 0..height {
            let mut row = String::new();
            for x in 0..width {
                row.push_str(buffer[(x, y)].symbol());
            }
            rows.push(row);
        }
        (rows, buffer)
    }

    fn render(view: &View) -> (Vec<String>, Buffer) {
        render_at(view, 80, 24)
    }

    fn themes() -> [Theme; 3] {
        [
            Theme::new(ThemeName::Standard, Depth::TrueColor),
            Theme::new(ThemeName::NightOps, Depth::TrueColor),
            Theme::new(ThemeName::Standard, Depth::None),
        ]
    }

    type Apply = Box<dyn Fn(&mut Snapshot)>;

    /// Every state the two cards can be in: (name, expected Receive verdict,
    /// expected Transmit verdict, how to make it).
    fn scenarios() -> Vec<(&'static str, Verdict, Verdict, Apply)> {
        let connected = |s: &mut Snapshot| {
            s.mode = Mode::Transmit;
            s.link = Link::Connected;
            s.rx.received = 100;
            s.ladder = Some(LadderState::Cat {
                tx_allowed: Some(true),
                watchdog_s: None,
                max_tx_s: None,
            });
        };
        vec![
            (
                "listen only, searching",
                Verdict::NotReadyPending,
                Verdict::ListenOnly,
                Box::new(|_| {}),
            ),
            (
                "listen only, slot held",
                Verdict::NotReadyPending,
                Verdict::ListenOnly,
                Box::new(|s| s.slot_held_by_other = true),
            ),
            (
                "transmit, searching",
                Verdict::NotReadyPending,
                Verdict::NotReadyPending,
                Box::new(|s| s.mode = Mode::Transmit),
            ),
            (
                "connected, no RX yet, ladder unknown",
                Verdict::NotReadyPending,
                Verdict::NotReadyPending,
                Box::new(|s| {
                    s.mode = Mode::Transmit;
                    s.link = Link::Connected;
                }),
            ),
            (
                "ladder unavailable",
                Verdict::Ready,
                Verdict::NotReadyPending,
                Box::new(move |s| {
                    connected(s);
                    s.ladder = Some(LadderState::Unavailable {
                        status: Some(502),
                        detail: String::new(),
                    });
                }),
            ),
            (
                "CAT ready",
                Verdict::Ready,
                Verdict::Ready,
                Box::new(connected),
            ),
            (
                "VOX ready",
                Verdict::Ready,
                Verdict::Ready,
                Box::new(move |s| {
                    connected(s);
                    s.ladder = Some(LadderState::VoxRig);
                }),
            ),
            (
                "locked by the transmit checks",
                Verdict::Ready,
                Verdict::Locked,
                Box::new(move |s| {
                    connected(s);
                    s.ladder = Some(LadderState::Cat {
                        tx_allowed: Some(false),
                        watchdog_s: None,
                        max_tx_s: None,
                    });
                }),
            ),
            (
                "locked by the cap",
                Verdict::Ready,
                Verdict::Locked,
                Box::new(move |s| {
                    connected(s);
                    s.status = Some(BridgeStatus {
                        tx_audio_muted: Some(true),
                        ..BridgeStatus::default()
                    });
                }),
            ),
            (
                "transmitting",
                Verdict::Ready,
                Verdict::Transmitting,
                Box::new(move |s| {
                    connected(s);
                    s.streaming = true;
                    s.tx_dbfs = Some(-12.0);
                }),
            ),
            (
                "RX paused",
                Verdict::NotReadyPending,
                Verdict::Ready,
                Box::new(move |s| {
                    connected(s);
                    s.link = Link::RxPaused;
                }),
            ),
            (
                "lost",
                Verdict::NotReadyFault,
                Verdict::NotReadyFault,
                Box::new(move |s| {
                    connected(s);
                    s.link = Link::Lost;
                }),
            ),
        ]
    }

    /// The header row, both cards, the Link panel, the status region and the
    /// key hints at 80 x 24 in Standard, Night Ops and with no colour.
    #[test]
    fn status_tab_renders_header_cards_link_panel_and_hints_at_80_by_24() {
        for theme in themes() {
            let v = view(theme);
            let (rows, buffer) = render(&v);
            let header = &rows[0];
            assert!(header.starts_with(" VectorCable"), "{header}");
            for tab in Tab::ALL {
                assert!(header.contains(tab.title()), "{header}");
            }
            assert!(header.contains("Link \u{25b2} Check"), "{header}");
            assert!(header.ends_with("13:19:42Z "), "{header}");
            let all = rows.join("\n");
            assert!(all.contains("Receive") && all.contains("Transmit"));
            assert!(all.contains("\u{25b2} Not Ready"), "{all}");
            assert!(all.contains("\u{25cb} Listen Only"), "{all}");
            assert!(all.contains("Press t to transmit"));
            let none = dash::<String>(None);
            assert!(
                all.contains(&format!("Bridge 10.42.0.1  version {none}  protocol 1")),
                "{all}"
            );
            assert!(all.contains("Mode Listen Only"));
            assert!(all.contains("Buffer 220 ms"));
            assert!(
                all.contains(&format!("Loss rx {none} %  tx {none} %")),
                "{all}"
            );
            assert!(all.contains(&format!("Underruns here {none}  bridge {none}")));
            assert!(all.contains(&format!("Walk {none} ppm")), "{all}");
            assert!(all.contains("-20 dBFS") && all.contains(&format!("{none} dBFS")));
            assert!(all.contains("Looking for the bridge."));
            assert!(
                rows[23].contains("1-4 tabs  t transmit  l listen  n Night Ops  ? help  q quit")
            );
            assert!(
                all.contains("\u{256d}") && all.contains("\u{256f}"),
                "rounded corners"
            );
            // One cell of padding inside the panels: the verdict is two cells in.
            assert_eq!(buffer[(2, 2)].symbol(), "\u{25b2}", "{}", rows[2]);
            if theme.depth == Depth::None {
                for y in 0..24 {
                    for x in 0..80 {
                        let c = &buffer[(x, y)];
                        assert_eq!(c.fg, Color::Reset, "colour at {x},{y}");
                        assert_eq!(c.bg, Color::Reset, "colour at {x},{y}");
                    }
                }
                let x = rows[0].find(" Status ").unwrap() as u16 + 1;
                let hdr = &buffer[(x, 0)];
                assert!(
                    hdr.modifier.contains(Modifier::REVERSED),
                    "selected tab in reverse video: {:?}",
                    hdr.modifier
                );
                assert!(!hdr.modifier.contains(Modifier::UNDERLINED));
            } else {
                assert_ne!(buffer[(0, 0)].bg, Color::Reset, "panel fill");
            }
        }
    }

    /// Every verdict the cards can show appears as glyph plus word, and
    /// every sentence on a card fits whole on one row at 80 x 24.
    #[test]
    fn every_verdict_is_a_glyph_plus_a_word_and_every_sentence_fits_at_80_by_24() {
        let mut seen = Vec::new();
        for theme in themes() {
            for ascii in [false, true] {
                for (name, receive, transmit, apply) in &scenarios() {
                    let mut v = view(theme);
                    v.ascii = ascii;
                    apply(&mut v.snapshot);
                    let rc = receive_card(&v.snapshot);
                    let tc = transmit_card(&v.snapshot);
                    assert_eq!(rc.verdict, *receive, "{name}");
                    assert_eq!(tc.verdict, *transmit, "{name}");
                    let (rows, _) = render(&v);
                    for card in [&rc, &tc] {
                        let mut sentences = vec![card.verdict.text(ascii), card.detail.clone()];
                        if let Some(fix) = &card.fix {
                            sentences.push(format!("Fix: {fix}"));
                        }
                        for sentence in sentences {
                            assert!(
                                sentence.chars().count() <= CARD_SENTENCE_MAX,
                                "{name}: {sentence:?} is longer than {CARD_SENTENCE_MAX}"
                            );
                            assert!(
                                rows.iter().any(|r| r.contains(&sentence)),
                                "{name}: {sentence:?} cut or missing in\n{}",
                                rows.join("\n")
                            );
                        }
                    }
                    seen.push(rc.verdict);
                    seen.push(tc.verdict);
                }
            }
        }
        for verdict in Verdict::ALL {
            assert!(seen.contains(&verdict), "{verdict:?} never rendered");
        }
        assert_eq!(Glyph::Locked.text(true), "[lock]");
        let mut s = snapshot();
        s.mode = Mode::Transmit;
        s.link = Link::Connected;
        assert_eq!(
            transmit_card(&s).detail,
            "Checking radio control (CAT)",
            "unknown ladder is pending, not Ready"
        );
        assert_eq!(
            verdict_sentence(&s),
            "Receive Not Ready. Transmit Not Ready."
        );
        s.rx.received = 1;
        s.ladder = Some(LadderState::VoxRig);
        assert_eq!(verdict_sentence(&s), "Receive Ready. Transmit Ready.");
    }

    /// The bridge PIN band shows a mask, never the digits, and fits 80 x 24;
    /// the hint line offers `p PIN` only when the bridge wants one.
    #[test]
    fn pin_entry_shows_a_mask_and_the_hint_appears_only_when_needed() {
        let mut v = view(themes()[0]);
        let (rows, _) = render_at(&v, 80, 24);
        assert!(!rows[23].contains("p PIN"));
        v.snapshot.listing = Some(vbcore::runner::ListingState::PinNeeded);
        let (rows, _) = render_at(&v, 80, 24);
        assert!(rows[23].contains("p PIN"), "{}", rows[23]);
        assert!(rows
            .iter()
            .any(|r| r.contains("needs the bridge PIN (press p)")));
        v.pin_entry = Some(4);
        let (rows, _) = render_at(&v, 80, 24);
        let all = rows.join("\n");
        assert!(
            all.contains("Bridge PIN") && all.contains("PIN: \u{2022}\u{2022}\u{2022}\u{2022}_"),
            "{all}"
        );
        assert!(all.contains("Enter to log in, Esc to cancel."), "{all}");
    }

    /// Narrow terminals stack the cards; both cards and the whole Link panel
    /// stay visible at 79 x 24 and 60 x 30.
    #[test]
    fn stacked_cards_and_the_link_panel_are_fully_visible_at_79_by_24_and_60_by_30() {
        for (w, h) in [(79u16, 24u16), (60, 30)] {
            let mut v = view(themes()[0]);
            // Lost: both cards carry a Fix line, the tallest case.
            scenarios().last().unwrap().3(&mut v.snapshot);
            let (rows, _) = render_at(&v, w, h);
            let all = rows.join("\n");
            let find = |text: &str| {
                rows.iter()
                    .position(|r| r.contains(text))
                    .unwrap_or_else(|| panic!("{text:?} missing at {w}x{h}:\n{all}"))
            };
            let receive = find("\u{256d} Receive ");
            let transmit = find("\u{256d} Transmit ");
            let link = find("\u{256d} Link ");
            assert!(receive < transmit && transmit < link, "{all}");
            assert_eq!(find("\u{2717} Not Ready"), receive + 1);
            assert_eq!(find("Fix: Check bridge power and network"), receive + 3);
            assert_eq!(find("Link lost: TX stopped"), transmit + 2);
            for label in [
                "Bridge 10.42.0.1",
                "Mode Transmit",
                "Apps row",
                "Loss rx",
                "Underruns here",
                "Buffer",
                "Walk",
                "Clock",
                "RX \u{2588}",
                "TX \u{2591}",
            ] {
                let row = find(label);
                assert!(row > link, "{label} at {w}x{h}");
            }
            let tx_meter = find("TX \u{2591}");
            let bottom = rows
                .iter()
                .enumerate()
                .skip(tx_meter)
                .find(|(_, r)| r.starts_with("\u{2570}"))
                .map(|(i, _)| i)
                .unwrap_or_else(|| panic!("Link panel bottom border missing at {w}x{h}:\n{all}"));
            assert!(bottom > tx_meter, "{all}");
            assert!(
                bottom < h as usize - 4,
                "status region below the Link panel"
            );
            assert!(rows[h as usize - 1].contains("q quit"));
            for r in &rows {
                assert_eq!(r.chars().count(), w as usize);
            }
        }
    }

    #[test]
    fn other_tabs_render_their_content_and_the_alert_box_replaces_the_status_region() {
        let mut v = view(themes()[0]);
        v.tab = Tab::Setup;
        let (rows, _) = render(&v);
        let all = rows.join("\n");
        assert!(
            all.contains("VectorBridge RX") && all.contains("10.42.0.1:4532"),
            "{all}"
        );
        assert!(
            all.contains("radio control (CAT) and keying (PTT)"),
            "{all}"
        );
        assert!(
            all.contains("PTT method: CAT, radio control keys the radio"),
            "{all}"
        );
        assert!(all.contains("Hamlib NET rigctl"));
        v.tab = Tab::Check;
        let (rows, _) = render(&v);
        let all = rows.join("\n");
        assert!(
            all.contains("Bridge reachable") && all.contains("Peer slot"),
            "{all}"
        );
        assert!(all.contains("Radio control (CAT)"), "{all}");
        assert!(all.contains("TCP 4532"), "{all}");
        assert!(all.contains("\u{25b2} Check"));
        v.tab = Tab::Levels;
        let (rows, _) = render(&v);
        let all = rows.join("\n");
        assert!(
            all.contains("Waiting for the bridge to report the current levels."),
            "{all}"
        );
        v.snapshot.status = Some(BridgeStatus {
            tx_gain: Some(50.0),
            rx_gain: Some(50.0),
            tx_audio_cap_s: Some(60),
            ..BridgeStatus::default()
        });
        v.confirm = Some("TX gain 51 %.".into());
        let (rows, _) = render(&v);
        let all = rows.join("\n");
        assert!(
            all.contains("> TX gain") && all.contains("every client"),
            "{all}"
        );
        assert!(all.contains("audio drive (ALC)"), "{all}");
        assert!(all.contains("Left and Right move between them."), "{all}");
        let rx_row = rows.iter().find(|r| r.contains("RX gain")).unwrap();
        assert!(
            rx_row.contains("\u{2502}   RX gain"),
            "unselected rows keep their indent: {rx_row:?}"
        );
        v.tab = Tab::Status;
        v.alert = Some("Transmit refused: another client (10.42.0.9:5100) holds the slot. Starting in Listen Only; press t to take it.".into());
        let (rows, _) = render(&v);
        let all = rows.join("\n");
        assert!(
            all.contains("Alert") && all.contains("(Enter to dismiss)"),
            "long alerts wrap, the dismiss hint stays: {all}"
        );
        assert!(all.contains("press t to take it."), "{all}");
        v.alert = None;
        v.status_lines = vec![
            "Transmit refused: another client (10.42.0.9:5100) holds the slot. Starting in Listen Only; press t to take it.".into(),
        ];
        let (rows, _) = render(&v);
        let all = rows.join("\n");
        assert!(
            all.contains("press t to take it."),
            "status lines wrap: {all}"
        );
        v.help = true;
        v.theme = themes()[1];
        let (rows, _) = render(&v);
        assert!(
            rows[23].trim_end().ends_with("q quit  Night Ops"),
            "help hints fit at 80 columns: {:?}",
            rows[23]
        );
        assert!(
            App::parse("wsjtx") == Some(App::WsjtX) && App::parse("fldigi") == Some(App::Fldigi)
        );
        assert_eq!(Tab::Levels.next(), Tab::Status);
        assert_eq!(Tab::Status.previous(), Tab::Levels);
        assert_eq!(LevelControl::TxGain.previous(), LevelControl::TxCap);
        assert_eq!(loss_percent(Some(1), Some(199)), "0.5 %");
        assert_eq!(
            loss_percent(Some(0), Some(0)),
            format!("{} %", dash::<String>(None))
        );
        assert_eq!(
            loss_percent(None, Some(5)),
            format!("{} %", dash::<String>(None))
        );
    }

    /// The Link panel's Clock value and the Check tab show a clock over 0.5 s
    /// as a warning (amber Check), never a fault.
    #[test]
    fn a_clock_warning_is_amber_check_in_the_link_panel_and_the_check_tab() {
        let theme = themes()[0];
        let mut v = view(theme);
        v.snapshot.clock = Some(ClockEstimate {
            clock_error_s: 0.7,
            decodes: 5,
            warn: true,
        });
        let (rows, buffer) = render(&v);
        let y = rows
            .iter()
            .position(|r| r.contains("Clock +0.7 s"))
            .unwrap();
        let x = rows[y].find("+0.7").unwrap();
        assert!(
            rows[y].contains("Clock +0.7 s \u{25b2} Check"),
            "{}",
            rows[y]
        );
        assert_eq!(
            buffer[(x as u16, y as u16)].fg,
            theme.color(&theme::WARNING).unwrap()
        );
        let lines = checks(&v.snapshot, None);
        let clock = lines.iter().find(|c| c.name == "Clock error").unwrap();
        assert_eq!((clock.glyph, clock.word), (Glyph::Check, "Check"));
        assert!(clock.detail.contains("sync the clock"));
    }

    /// Test 10: at 80 x 24 a long check detail continues under the detail
    /// column, never under the check names.
    #[test]
    fn long_check_details_wrap_under_the_detail_column_at_80_by_24() {
        let mut v = view(Theme::new(ThemeName::Standard, Depth::None));
        v.tab = Tab::Check;
        v.snapshot.audio_name = "coreaudio".into();
        v.snapshot.rigctl = Some(RigctlState::Proxy {
            access: Some(RigctlAccess::All),
            port: Some(4532),
            listening: None,
            vb_auth: false,
            login_required: false,
        });
        v.rigctl_port_open = Some(true);
        v.snapshot.status = Some(BridgeStatus {
            peer: PeerSlot::Held("10.42.0.66:59580".into()),
            ..BridgeStatus::default()
        });
        v.snapshot.slot_held_by_other = true;
        let (rows, _) = render(&v);
        let col = |row: &str, s: &str| {
            let at = row.find(s).unwrap();
            row[..at].chars().count()
        };
        let y = rows
            .iter()
            .position(|r| r.contains("Audio devices"))
            .unwrap();
        let detail_col = col(&rows[y], "Audio devices") + CHECK_DETAIL_COL;
        for (head, tail) in [
            ("Audio devices", "Audio)"),
            ("Radio control (CAT)", "all)"),
            ("Peer slot", "10.42.0.66:59580"),
        ] {
            let y = rows.iter().position(|r| r.contains(head)).unwrap();
            let next = &rows[y + 1];
            assert!(!rows[y].contains(tail), "{}", rows[y]);
            assert_eq!(col(next, tail), detail_col, "{next}");
            let left: String = next.chars().take(detail_col).skip(2).collect();
            assert!(left.trim().is_empty(), "{next}");
        }
    }

    /// Brief section 8: 4532 reachable from all networks with no lock is Check.
    #[test]
    fn rigctl_open_to_all_networks_is_check() {
        let mut s = snapshot();
        s.rigctl = Some(RigctlState::Proxy {
            access: Some(RigctlAccess::All),
            port: Some(4532),
            listening: None,
            vb_auth: false,
            login_required: false,
        });
        let line = checks(&s, Some(true)).remove(2);
        assert_eq!((line.glyph, line.word), (Glyph::Check, "Check"));
        assert_eq!(
            line.detail,
            "TCP 4532 open to all networks (rigctl_access all)"
        );
    }

    /// Access limited to the bridge's own access point with the port open is Good.
    #[test]
    fn rigctl_open_on_the_access_point_only_is_good() {
        let mut s = snapshot();
        s.rigctl = Some(RigctlState::Proxy {
            access: Some(RigctlAccess::Ap),
            port: Some(4532),
            listening: None,
            vb_auth: false,
            login_required: false,
        });
        let line = checks(&s, Some(true)).remove(2);
        assert_eq!((line.glyph, line.word), (Glyph::Ok, "Good"));
        assert_eq!(line.detail, "TCP 4532 open, access ap");
    }

    /// The apps use the local relay; only when it could not start do they go
    /// straight to the bridge's proxy port.
    #[test]
    fn setup_points_the_apps_at_the_relay_unless_it_failed() {
        let mut s = snapshot();
        s.bridge = "100.64.0.1:5101".parse().unwrap();
        s.rigctl = Some(RigctlState::Proxy {
            access: Some(RigctlAccess::All),
            port: Some(4600),
            listening: None,
            vb_auth: true,
            login_required: false,
        });
        assert_eq!(rigctl_address(&s), "127.0.0.1:4532");
        assert!(relay_note(&s).contains("bridge at 100.64.0.1:4600"));
        s.rig_relay = Some(RelayState::Listening("127.0.0.1:4532".parse().unwrap()));
        assert_eq!(rigctl_address(&s), "127.0.0.1:4532");
        let steps = App::WsjtX.steps(&rigctl_address(&s), false);
        assert!(steps.contains(&(
            "Settings > Radio > Network Server".into(),
            "127.0.0.1:4532".into()
        )));
        let listen: SocketAddr = "127.0.0.1:4532".parse().unwrap();
        s.rig_relay = Some(RelayState::Failed {
            listen,
            error: "address in use".into(),
        });
        // The bridge takes no app straight, so the apps wait for the relay.
        assert_eq!(rigctl_address(&s), "127.0.0.1:4532");
        let note = relay_note(&s);
        assert!(
            note.contains("could not start (127.0.0.1:4532: address in use)"),
            "{note}"
        );
        assert!(note.contains("--rig-port 4533"), "{note}");
        assert!(!note.contains("straight"), "{note}");
        let busy = relay_port_busy_note(&s, listen);
        assert!(busy.contains("only through VectorCable"), "{busy}");
        assert!(busy.contains("point the app at 127.0.0.1:4533"), "{busy}");
        // An older bridge, without the marker, still takes the app straight.
        s.rigctl = Some(RigctlState::Proxy {
            access: Some(RigctlAccess::All),
            port: Some(4600),
            listening: None,
            vb_auth: false,
            login_required: false,
        });
        assert_eq!(rigctl_address(&s), "100.64.0.1:4600");
        assert!(relay_note(&s).contains("straight to the bridge at 100.64.0.1:4600"));
        assert!(
            relay_port_busy_note(&s, listen).contains("straight at the bridge at 100.64.0.1:4600")
        );
    }

    /// The maintainer, 2026-10-28: with `login_required` the port is locked in
    /// every mode, so it is Good, even under `all` (his answer, 2026-10-29), and
    /// nobody is said to key unchecked.
    #[test]
    fn rigctl_with_login_required_is_good_and_says_who_can_use_it() {
        let mut s = snapshot();
        for access in [RigctlAccess::Wifi, RigctlAccess::All] {
            s.rigctl = Some(RigctlState::Proxy {
                access: Some(access),
                port: Some(4532),
                listening: None,
                vb_auth: true,
                login_required: true,
            });
            let line = checks(&s, Some(true)).remove(2);
            assert_eq!((line.glyph, line.word), (Glyph::Ok, "Good"), "{access:?}");
            assert!(
                line.detail.contains("only apps logged in"),
                "{}",
                line.detail
            );
            let note = rigctl_note(&s);
            assert!(note.contains("only apps logged in"), "{note}");
            assert!(!note.contains("anyone"), "{note}");
        }
    }

    /// The maintainer, 2026-10-14: `wifi` open is Good with who can reach it, on the
    /// bridge's own access point or a joined network.
    #[test]
    fn rigctl_wifi_open_is_good_with_who_can_reach_it() {
        let mut s = snapshot();
        s.rigctl = Some(RigctlState::Proxy {
            access: Some(RigctlAccess::Wifi),
            port: Some(4532),
            listening: None,
            vb_auth: false,
            login_required: false,
        });
        for bridge in ["10.42.0.1:5101", "192.0.2.15:5101"] {
            s.bridge = bridge.parse().unwrap();
            let line = checks(&s, Some(true)).remove(2);
            assert_eq!((line.glyph, line.word), (Glyph::Ok, "Good"), "{bridge}");
            assert_eq!(
                line.detail,
                "TCP 4532 open to this Wi-Fi network; anyone on it can tune and key through the safety proxy"
            );
        }
    }

    /// Off the bridge's Wi-Fi (the tailnet), `wifi` closes 4532; only `all` opens it.
    #[test]
    fn rigctl_wifi_closed_off_the_access_point_points_to_all_for_the_tailnet() {
        let mut s = snapshot();
        s.rigctl = Some(RigctlState::Proxy {
            access: Some(RigctlAccess::Wifi),
            port: Some(4532),
            listening: None,
            vb_auth: false,
            login_required: false,
        });
        s.bridge = "100.64.0.1:5101".parse().unwrap();
        let line = checks(&s, Some(false)).remove(2);
        assert_eq!((line.glyph, line.word), (Glyph::Fix, "Fix"));
        assert_eq!(
            line.detail,
            "TCP 4532 closed, access wifi; set rigctl_access to all for the tailnet"
        );
    }

    #[test]
    fn checks_cover_the_doctor_list() {
        let mut s = snapshot();
        s.link = Link::Connected;
        s.rigctl = Some(RigctlState::Proxy {
            access: Some(RigctlAccess::Ap),
            port: Some(4532),
            listening: None,
            vb_auth: false,
            login_required: false,
        });
        s.status = Some(BridgeStatus {
            peer: PeerSlot::Held("10.42.0.9:5100".into()),
            tx_audio_cap_s: Some(30),
            ..BridgeStatus::default()
        });
        s.slot_held_by_other = true;
        s.mode = Mode::Transmit;
        let names: Vec<&str> = checks(&s, Some(false)).iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            [
                "Bridge reachable",
                "Audio devices",
                "Radio control (CAT)",
                "Clock error",
                "Peer slot",
                "TX audio cap"
            ]
        );
        let lines = checks(&s, Some(false));
        assert_eq!(lines[1].word, "Check");
        assert!(lines[1].detail.contains("null adapter"));
        assert_eq!(lines[2].word, "Fix");
        assert!(lines[2].detail.starts_with("TCP 4532 closed"));
        assert!(lines[4].detail.contains("10.42.0.9:5100"));
        assert!(lines[5].detail.contains("shorter than a WSJT-X cycle"));
        // The slot flag, not the port, decides who holds the slot.
        s.slot_held_by_other = false;
        assert_eq!(checks(&s, None)[4].detail, "we hold the slot");
        // The real adapter, and its fault, show on the Audio devices row.
        s.audio_name = "pipewire".into();
        assert_eq!(checks(&s, None)[1].word, "Good");
        assert!(checks(&s, None)[1].detail.contains("PipeWire"));
        s.audio_fault = Some("pacat exited: Connection refused".into());
        let audio = &checks(&s, None)[1];
        assert_eq!((audio.glyph, audio.word), (Glyph::Fix, "Fix"));
        assert_eq!(audio.detail, "pacat exited: Connection refused");
    }

    fn rgb(theme: &Theme, c: Color) -> Option<source::Rgb8> {
        match c {
            Color::Rgb(r, g, b) => Some(source::Rgb8(r, g, b)),
            Color::Indexed(i) => Some(source::xterm(i)),
            Color::Reset => rgb(theme, theme.color(&theme::BACKGROUND)?),
            _ => None,
        }
    }

    const BORDER_CHARS: &str = "\u{2500}\u{2502}\u{256d}\u{256e}\u{2570}\u{256f}";
    const METER_CHARS: &str = "\u{2588}\u{2591}";

    /// One row's cells grouped into runs of one style: (x, text, style).
    fn runs(buffer: &Buffer, y: u16) -> Vec<(u16, String, Style)> {
        let mut out: Vec<(u16, String, Style)> = Vec::new();
        for x in 0..buffer.area.width {
            let c = &buffer[(x, y)];
            let style = c.style();
            match out.last_mut() {
                Some((_, text, last)) if *last == style => text.push_str(c.symbol()),
                _ => out.push((x, c.symbol().to_string(), style)),
            }
        }
        out
    }

    /// Colour roles, cell by cell, over every tab and verdict state in both
    /// themes at 24-bit and 256 colours: red only where the brief allows it,
    /// Ready and Good in status-ok, the selected tab as items 5 and 13 say,
    /// and every drawn text pair at 4.5:1 or better.
    #[test]
    fn colour_roles_and_contrast_hold_in_every_tab_theme_and_depth() {
        let depths = [Depth::TrueColor, Depth::Indexed256];
        let names = [ThemeName::Standard, ThemeName::NightOps];
        let mut drawn_pairs = std::collections::BTreeSet::new();
        let mut red_text_seen = 0;
        for name in names {
            for depth in depths {
                let theme = Theme::new(name, depth);
                let mut reds: Vec<Color> = [&theme::TX, &theme::TX_FILL, &theme::TX_TEXT]
                    .iter()
                    .filter_map(|t| theme.color(t))
                    .collect();
                if name == ThemeName::NightOps {
                    reds.push(theme.color(&theme::ACCENT).unwrap());
                }
                let ok = theme.color(&theme::STATUS_OK).unwrap();
                let selected = theme.selected_tab();
                for tab in Tab::ALL {
                    for (scenario, _, _, apply) in &scenarios() {
                        let mut v = view(theme);
                        v.tab = tab;
                        apply(&mut v.snapshot);
                        v.rigctl_port_open = Some(false);
                        v.snapshot.rigctl = Some(RigctlState::Proxy {
                            access: Some(RigctlAccess::Ap),
                            port: Some(4532),
                            listening: None,
                            vb_auth: false,
                            login_required: false,
                        });
                        v.snapshot.audio_fault = Some("parec exited".into());
                        if v.snapshot.link == Link::Lost {
                            v.alert = Some("Link lost: TX stopped.".into());
                        }
                        let (rows, buffer) = render(&v);
                        let where_ = format!("{name:?} {depth:?} {tab:?} {scenario}");
                        // The selected tab carries the selected-tab style (items 5 and 13).
                        let label = format!(" {} ", tab.title());
                        let x = rows[0].find(&label).unwrap() as u16;
                        let cell = &buffer[(x + 1, 0)];
                        assert_eq!(cell.fg, selected.fg.unwrap(), "{where_}");
                        assert_eq!(cell.bg, selected.bg.unwrap(), "{where_}");
                        assert_eq!(cell.underline_color, selected.underline_color.unwrap());
                        assert!(cell.modifier.contains(Modifier::UNDERLINED), "{where_}");
                        for y in 0..buffer.area.height {
                            for (x, text, style) in runs(&buffer, y) {
                                let trimmed = text.trim();
                                let is_border = !trimmed.is_empty()
                                    && trimmed.chars().all(|c| BORDER_CHARS.contains(c));
                                let is_meter = !trimmed.is_empty()
                                    && trimmed.chars().all(|c| METER_CHARS.contains(c));
                                let fg = style.fg.unwrap_or(Color::Reset);
                                let bg = style.bg.unwrap_or(Color::Reset);
                                let red_fg = reds.contains(&fg);
                                let red_bg = reds.contains(&bg);
                                if is_border {
                                    // Night Ops panel borders may be red, nothing else here.
                                    assert!(
                                        !red_fg || name == ThemeName::NightOps,
                                        "{where_}: red border at {x},{y}"
                                    );
                                    assert!(!red_bg, "{where_}: red fill behind a border");
                                    continue;
                                }
                                if let Some(u) = style.underline_color {
                                    assert!(
                                        !reds.contains(&u) || text == label,
                                        "{where_}: red underline on {text:?}"
                                    );
                                }
                                if trimmed.is_empty() {
                                    assert!(
                                        !red_bg || text == " TX ",
                                        "{where_}: red fill at {x},{y}"
                                    );
                                    continue;
                                }
                                let allowed_red = trimmed == "TX"
                                    || trimmed == "\u{25cf} Transmitting"
                                    || trimmed == "\u{2717} Not Ready"
                                    || trimmed == "\u{2717} Fix"
                                    || trimmed.starts_with("Fix: ")
                                    || trimmed == "parec exited"
                                    || trimmed.starts_with("Link lost: TX stopped.");
                                if red_fg || red_bg {
                                    assert!(allowed_red, "{where_}: red on {trimmed:?} at {x},{y}");
                                    red_text_seen += 1;
                                }
                                if trimmed == "\u{2713} Ready" || trimmed == "\u{2713} Good" {
                                    assert_eq!(fg, ok, "{where_}: {trimmed}");
                                }
                                if is_meter {
                                    continue;
                                }
                                let f = rgb(&theme, fg).unwrap();
                                let b = rgb(&theme, bg).unwrap();
                                let ratio = source::contrast(f, b);
                                assert!(
                                    ratio >= source::MIN_CONTRAST,
                                    "{where_}: {trimmed:?} at {x},{y} is {ratio:.2}:1 ({fg:?} on {bg:?})"
                                );
                                drawn_pairs.insert(format!("{fg:?} on {bg:?}"));
                            }
                        }
                    }
                }
            }
        }
        assert!(red_text_seen > 0);
        assert!(drawn_pairs.len() >= 8, "{drawn_pairs:?}");
    }
}
