//! Status model and parsers: 5101 `status`, `GET /api/ladder`, `GET /api/rigctl`
//! and the bridge version scraped from the home page footer (until P4 ships
//! it in the `hello` reply). Unknown values are `None`, shown as `--`, never 0.

use serde_json::Value;

use crate::http::HttpOutcome;

/// Who holds the peer slot, as `status` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerSlot {
    /// `status` has not answered yet, or had no `peer` field.
    Unknown,
    /// `peer` was null.
    Empty,
    /// `peer` was `"ip:port"`.
    Held(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioDevice {
    pub ok: Option<bool>,
    pub error: Option<String>,
    pub since: Option<u64>,
}

/// The `status` telemetry. Every field is optional: a bridge may be older or
/// newer than this client.
#[derive(Clone, Debug, PartialEq)]
pub struct BridgeStatus {
    pub rx_packets_sent: Option<u64>,
    pub tx_packets_received: Option<u64>,
    pub rx_packet_loss: Option<u64>,
    pub tx_packet_loss: Option<u64>,
    pub tx_late: Option<u64>,
    pub tx_reordered: Option<u64>,
    pub rx_overruns: Option<u64>,
    pub tx_underruns: Option<u64>,
    pub network_jitter_ms: Option<f64>,
    pub jitter_depth: Option<u64>,
    pub rx_peak: Option<i64>,
    pub rx_rms: Option<i64>,
    pub tx_peak: Option<i64>,
    pub tx_rms: Option<i64>,
    pub clip_count: Option<u64>,
    pub sample_rate: Option<u64>,
    pub alsa_device: Option<String>,
    pub peer: PeerSlot,
    pub monitors: Option<Vec<String>>,
    pub device_reopens: Option<u64>,
    pub audio_device: Option<AudioDevice>,
    pub uptime_s: Option<u64>,
    pub tx_gain: Option<f64>,
    pub rx_gain: Option<f64>,
    pub rx_gain_fixed: Option<bool>,
    pub tx_audio_cap_s: Option<u64>,
    pub tx_audio_muted: Option<bool>,
    pub tx_audio_cap_trips: Option<u64>,
}

impl Default for BridgeStatus {
    fn default() -> Self {
        BridgeStatus {
            rx_packets_sent: None,
            tx_packets_received: None,
            rx_packet_loss: None,
            tx_packet_loss: None,
            tx_late: None,
            tx_reordered: None,
            rx_overruns: None,
            tx_underruns: None,
            network_jitter_ms: None,
            jitter_depth: None,
            rx_peak: None,
            rx_rms: None,
            tx_peak: None,
            tx_rms: None,
            clip_count: None,
            sample_rate: None,
            alsa_device: None,
            peer: PeerSlot::Unknown,
            monitors: None,
            device_reopens: None,
            audio_device: None,
            uptime_s: None,
            tx_gain: None,
            rx_gain: None,
            rx_gain_fixed: None,
            tx_audio_cap_s: None,
            tx_audio_muted: None,
            tx_audio_cap_trips: None,
        }
    }
}

fn u(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}
fn i(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}
fn f(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}
fn b(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(Value::as_bool)
}
fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

impl BridgeStatus {
    /// Parses a `status` reply value (the object with `ok: true`).
    pub fn from_value(v: &Value) -> BridgeStatus {
        let peer = match v.get("peer") {
            None => PeerSlot::Unknown,
            Some(Value::Null) => PeerSlot::Empty,
            Some(Value::String(p)) => PeerSlot::Held(p.clone()),
            Some(_) => PeerSlot::Unknown,
        };
        let monitors = v.get("monitors").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        });
        let audio_device = v
            .get("audio_device")
            .filter(|d| d.is_object())
            .map(|d| AudioDevice {
                ok: b(d, "ok"),
                error: s(d, "error"),
                since: u(d, "since"),
            });
        BridgeStatus {
            rx_packets_sent: u(v, "rx_packets_sent"),
            tx_packets_received: u(v, "tx_packets_received"),
            rx_packet_loss: u(v, "rx_packet_loss"),
            tx_packet_loss: u(v, "tx_packet_loss"),
            tx_late: u(v, "tx_late"),
            tx_reordered: u(v, "tx_reordered"),
            rx_overruns: u(v, "rx_overruns"),
            tx_underruns: u(v, "tx_underruns"),
            network_jitter_ms: f(v, "network_jitter_ms"),
            jitter_depth: u(v, "jitter_depth"),
            rx_peak: i(v, "rx_peak"),
            rx_rms: i(v, "rx_rms"),
            tx_peak: i(v, "tx_peak"),
            tx_rms: i(v, "tx_rms"),
            clip_count: u(v, "clip_count"),
            sample_rate: u(v, "sample_rate"),
            alsa_device: s(v, "alsa_device"),
            peer,
            monitors,
            device_reopens: u(v, "device_reopens"),
            audio_device,
            uptime_s: u(v, "uptime_s"),
            tx_gain: f(v, "tx_gain"),
            rx_gain: f(v, "rx_gain"),
            rx_gain_fixed: b(v, "rx_gain_fixed"),
            tx_audio_cap_s: u(v, "tx_audio_cap_s"),
            tx_audio_muted: b(v, "tx_audio_muted"),
            tx_audio_cap_trips: u(v, "tx_audio_cap_trips"),
        }
    }

    /// Parses a raw `status` datagram. `Err` carries the bridge's error string
    /// or why the datagram could not be read.
    pub fn parse(data: &[u8]) -> Result<BridgeStatus, String> {
        match crate::protocol::Reply::parse(data).map_err(|e| e.to_string())? {
            crate::protocol::Reply::Ok(v) => Ok(BridgeStatus::from_value(&v)),
            crate::protocol::Reply::Error(e) => Err(e),
        }
    }
}

/// Transmit permission from `GET /api/ladder`.
#[derive(Clone, Debug, PartialEq)]
pub enum LadderState {
    /// A CAT rig: `tx_allowed` says whether the transmit checks passed or were bypassed.
    Cat {
        tx_allowed: Option<bool>,
        watchdog_s: Option<u64>,
        max_tx_s: Option<u64>,
    },
    /// `503`: no rig control; the audio keys the radio.
    VoxRig,
    /// Any other failure. A dropped connection counts as 502; a bridge that
    /// cannot be reached at all has no status (shown as a dash).
    Unavailable { status: Option<u16>, detail: String },
}

impl LadderState {
    pub fn parse(outcome: &HttpOutcome) -> LadderState {
        match outcome {
            HttpOutcome::Response { status: 200, body } => {
                let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                LadderState::Cat {
                    tx_allowed: b(&v, "tx_allowed"),
                    watchdog_s: u(&v, "watchdog_s"),
                    max_tx_s: u(&v, "max_tx_s"),
                }
            }
            HttpOutcome::Response { status: 503, .. } => LadderState::VoxRig,
            HttpOutcome::Response { status, body } => {
                let detail = serde_json::from_str::<Value>(body)
                    .ok()
                    .and_then(|v| s(&v, "error"))
                    .unwrap_or_default();
                LadderState::Unavailable {
                    status: Some(*status),
                    detail,
                }
            }
            // `GET /api/ladder` can close the connection with no reply (vector-bridge issue #26).
            HttpOutcome::ConnectionDropped => LadderState::Unavailable {
                status: Some(502),
                detail: "connection dropped".into(),
            },
            HttpOutcome::Unreachable(e) => LadderState::Unavailable {
                status: None,
                detail: e.clone(),
            },
        }
    }
}

/// Who can reach the rigctl safety proxy on TCP 4532.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RigctlAccess {
    Off,
    /// Loopback and the access point's `10.42.0.1` only (the default before
    /// bridge roadmap 1.3).
    Ap,
    /// Loopback and whichever Wi-Fi the bridge is on (its access point or a
    /// joined network), never the tailnet (the default from bridge 1.3).
    Wifi,
    All,
}

impl RigctlAccess {
    pub fn parse(text: &str) -> Option<RigctlAccess> {
        match text {
            "off" => Some(RigctlAccess::Off),
            "ap" => Some(RigctlAccess::Ap),
            "wifi" => Some(RigctlAccess::Wifi),
            "all" => Some(RigctlAccess::All),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RigctlState {
    Proxy {
        access: Option<RigctlAccess>,
        port: Option<u16>,
        listening: Option<Vec<String>>,
        /// `"vb_auth": true`: the proxy takes `\vb_auth <app token>` as a
        /// first line (bridge 1.10 on). Anything else means never send it.
        vb_auth: bool,
        /// `"login_required": true`: every client of 4532 but the Pi's own
        /// logs in, whatever `access` says (the maintainer ruled 2026-10-28).
        /// Absent on a 1.10 bridge, where only tailnet clients under `all` did.
        login_required: bool,
    },
    /// `503 no_rigctl_proxy`.
    NoProxy,
    /// A dropped connection counts as 502; unreachable has no status.
    Unavailable { status: Option<u16>, detail: String },
}

impl RigctlState {
    pub fn parse(outcome: &HttpOutcome) -> RigctlState {
        match outcome {
            HttpOutcome::Response { status: 200, body } => {
                let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                RigctlState::Proxy {
                    access: s(&v, "access").and_then(|a| RigctlAccess::parse(&a)),
                    port: u(&v, "port").and_then(|p| u16::try_from(p).ok()),
                    listening: v.get("listening").and_then(Value::as_array).map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    }),
                    vb_auth: v.get("vb_auth") == Some(&Value::Bool(true)),
                    login_required: v.get("login_required") == Some(&Value::Bool(true)),
                }
            }
            HttpOutcome::Response { status: 503, .. } => RigctlState::NoProxy,
            HttpOutcome::Response { status, body } => RigctlState::Unavailable {
                status: Some(*status),
                detail: body.clone(),
            },
            HttpOutcome::ConnectionDropped => RigctlState::Unavailable {
                status: Some(502),
                detail: "connection dropped".into(),
            },
            HttpOutcome::Unreachable(e) => RigctlState::Unavailable {
                status: None,
                detail: e.clone(),
            },
        }
    }
}

/// `VectorBridge 0.1.4 (e2e8228)` from the home page footer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeVersion {
    pub version: String,
    pub commit: Option<String>,
}

impl BridgeVersion {
    /// Scrapes `<p class="ver">VectorBridge <version> (<commit>)</p>` from the home page.
    /// A source checkout prints the bare version with no commit.
    pub fn from_home_page(html: &str) -> Option<BridgeVersion> {
        let marker = "VectorBridge ";
        let start = html.find("class=\"ver\"")?;
        let rest = &html[start..];
        let text_start = rest.find(marker)? + marker.len();
        let text = &rest[text_start..];
        let end = text.find('<').unwrap_or(text.len());
        let text = text[..end].trim();
        if text.is_empty() {
            return None;
        }
        let (version, commit) = match text.split_once(" (") {
            Some((v, c)) => (v.trim(), c.strip_suffix(')').map(|c| c.trim().to_string())),
            None => (text, None),
        };
        if version.is_empty() || !version.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            return None;
        }
        Some(BridgeVersion {
            version: version.to_string(),
            commit: commit.filter(|c| !c.is_empty()),
        })
    }
}

/// Formats an unknown value as a dash, following the Vector content rule.
pub fn dash<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "--".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parses_known_fields_and_unknown_as_none() {
        let data = br#"{"ok":true,"rx_packets_sent":1200,"tx_packet_loss":3,"network_jitter_ms":1.25,
            "peer":"10.42.0.12:5100","monitors":["10.42.0.13:6000"],"alsa_device":"hw:1,0",
            "audio_device":{"ok":true,"error":null,"since":1700000000},"tx_gain":40,"rx_gain_fixed":true,
            "tx_audio_cap_s":30,"tx_audio_muted":false}"#;
        let st = BridgeStatus::parse(data).unwrap();
        assert_eq!(st.rx_packets_sent, Some(1200));
        assert_eq!(st.tx_packet_loss, Some(3));
        assert_eq!(st.network_jitter_ms, Some(1.25));
        assert_eq!(st.peer, PeerSlot::Held("10.42.0.12:5100".into()));
        assert_eq!(st.monitors, Some(vec!["10.42.0.13:6000".to_string()]));
        assert_eq!(
            st.audio_device,
            Some(AudioDevice {
                ok: Some(true),
                error: None,
                since: Some(1700000000)
            })
        );
        assert_eq!(st.tx_gain, Some(40.0));
        assert_eq!(st.rx_gain_fixed, Some(true));
        assert_eq!(st.tx_audio_cap_s, Some(30));
        // Absent fields are None, never zero.
        assert_eq!(st.tx_underruns, None);
        assert_eq!(st.rx_overruns, None);
        assert_eq!(st.uptime_s, None);
        assert_eq!(st.tx_audio_cap_trips, None);
        assert_eq!(dash(st.tx_underruns), "--");
        assert_eq!(dash(st.tx_packet_loss), "3");
    }

    #[test]
    fn status_peer_null_is_empty_and_missing_is_unknown() {
        assert_eq!(
            BridgeStatus::parse(br#"{"ok":true,"peer":null}"#)
                .unwrap()
                .peer,
            PeerSlot::Empty
        );
        assert_eq!(
            BridgeStatus::parse(br#"{"ok":true}"#).unwrap().peer,
            PeerSlot::Unknown
        );
        assert_eq!(
            BridgeStatus::parse(br#"{"ok":false,"error":"bad_request"}"#),
            Err("bad_request".into())
        );
        assert!(BridgeStatus::parse(b"garbage").is_err());
    }

    #[test]
    fn ladder_200_gives_tx_allowed_503_is_vox_rig_and_dropped_is_502() {
        let ok = HttpOutcome::Response {
            status: 200,
            body: r#"{"ok":true,"tx_allowed":true,"watchdog_s":3,"max_tx_s":15}"#.into(),
        };
        assert_eq!(
            LadderState::parse(&ok),
            LadderState::Cat {
                tx_allowed: Some(true),
                watchdog_s: Some(3),
                max_tx_s: Some(15)
            }
        );
        let partial = HttpOutcome::Response {
            status: 200,
            body: r#"{"ok":true}"#.into(),
        };
        assert_eq!(
            LadderState::parse(&partial),
            LadderState::Cat {
                tx_allowed: None,
                watchdog_s: None,
                max_tx_s: None
            }
        );
        let vox = HttpOutcome::Response {
            status: 503,
            body: r#"{"ok":false,"error":"no rig control"}"#.into(),
        };
        assert_eq!(LadderState::parse(&vox), LadderState::VoxRig);
        assert_eq!(
            LadderState::parse(&HttpOutcome::ConnectionDropped),
            LadderState::Unavailable {
                status: Some(502),
                detail: "connection dropped".into()
            }
        );
        assert_eq!(
            LadderState::parse(&HttpOutcome::Unreachable("no route".into())),
            LadderState::Unavailable {
                status: None,
                detail: "no route".into()
            },
            "unreachable has no HTTP status; shown as a dash"
        );
        assert_eq!(
            RigctlState::parse(&HttpOutcome::Unreachable("no route".into())),
            RigctlState::Unavailable {
                status: None,
                detail: "no route".into()
            }
        );
        let bad = HttpOutcome::Response {
            status: 502,
            body: r#"{"ok":false,"error":"rigctld: timeout"}"#.into(),
        };
        assert_eq!(
            LadderState::parse(&bad),
            LadderState::Unavailable {
                status: Some(502),
                detail: "rigctld: timeout".into()
            }
        );
    }

    #[test]
    fn rigctl_parses_access_and_503_is_no_proxy() {
        let ok = HttpOutcome::Response {
            status: 200,
            body: r#"{"ok":true,"access":"all","port":4532,"listening":["0.0.0.0"]}"#.into(),
        };
        assert_eq!(
            RigctlState::parse(&ok),
            RigctlState::Proxy {
                access: Some(RigctlAccess::All),
                port: Some(4532),
                listening: Some(vec!["0.0.0.0".into()]),
                vb_auth: false,
                login_required: false
            }
        );
        let wifi = HttpOutcome::Response {
            status: 200,
            body: r#"{"ok":true,"access":"wifi","port":4532,"listening":["0.0.0.0"]}"#.into(),
        };
        assert!(matches!(
            RigctlState::parse(&wifi),
            RigctlState::Proxy {
                access: Some(RigctlAccess::Wifi),
                ..
            }
        ));
        let odd = HttpOutcome::Response {
            status: 200,
            body: r#"{"ok":true,"access":"sometimes"}"#.into(),
        };
        assert_eq!(
            RigctlState::parse(&odd),
            RigctlState::Proxy {
                access: None,
                port: None,
                listening: None,
                vb_auth: false,
                login_required: false
            }
        );
        let none = HttpOutcome::Response {
            status: 503,
            body: r#"{"ok":false,"error":"no_rigctl_proxy"}"#.into(),
        };
        assert_eq!(RigctlState::parse(&none), RigctlState::NoProxy);
    }

    /// Only a literal `"vb_auth": true` marks a proxy that takes the token
    /// line (BRIDGE, 2026-10-14); an older proxy would run it as commands.
    #[test]
    fn rigctl_vb_auth_is_only_a_literal_true() {
        let vb_auth = |body: &str| {
            matches!(
                RigctlState::parse(&HttpOutcome::Response {
                    status: 200,
                    body: body.into(),
                }),
                RigctlState::Proxy { vb_auth: true, .. }
            )
        };
        assert!(vb_auth(r#"{"ok":true,"access":"all","vb_auth":true}"#));
        assert!(!vb_auth(r#"{"ok":true,"access":"all"}"#));
        assert!(!vb_auth(r#"{"ok":true,"access":"all","vb_auth":"true"}"#));
        assert!(!vb_auth(r#"{"ok":true,"access":"all","vb_auth":1}"#));
        assert!(!vb_auth("not json"));
    }

    /// `login_required` reads the same way: only a literal `true`.
    #[test]
    fn rigctl_login_required_is_only_a_literal_true() {
        let required = |body: &str| {
            matches!(
                RigctlState::parse(&HttpOutcome::Response {
                    status: 200,
                    body: body.into(),
                }),
                RigctlState::Proxy {
                    login_required: true,
                    ..
                }
            )
        };
        assert!(required(
            r#"{"access":"wifi","port":4532,"vb_auth":true,"login_required":true}"#
        ));
        assert!(!required(r#"{"access":"wifi","port":4532,"vb_auth":true}"#));
        assert!(!required(r#"{"access":"wifi","login_required":"true"}"#));
    }

    #[test]
    fn footer_scrape_reads_version_and_commit_or_bare_version() {
        let html = r#"<html><body><h1>VectorBridge</h1><p class="ver">VectorBridge 0.1.4 (e2e8228)</p></body></html>"#;
        assert_eq!(
            BridgeVersion::from_home_page(html),
            Some(BridgeVersion {
                version: "0.1.4".into(),
                commit: Some("e2e8228".into())
            })
        );
        let bare = r#"<p class="ver">VectorBridge 0.1.4</p>"#;
        assert_eq!(
            BridgeVersion::from_home_page(bare),
            Some(BridgeVersion {
                version: "0.1.4".into(),
                commit: None
            })
        );
        assert_eq!(BridgeVersion::from_home_page("<p>VectorBridge</p>"), None);
        assert_eq!(
            BridgeVersion::from_home_page(r#"<p class="ver">VectorBridge </p>"#),
            None
        );
    }
}
