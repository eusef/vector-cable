//! VectorBridge wire protocol, version 1 (`vector-bridge/DOCS/protocol.md`).
//!
//! Audio packets on UDP 5100: a 20-byte header `<4sBBIQH` (magic `VBRG`,
//! version, direction, sequence, timestamp_us, sample_count) followed by
//! `sample_count` little-endian int16 samples. A keepalive is a TX packet with
//! 0 samples. Control messages on UDP 5101 are one JSON object per datagram.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAGIC: &[u8; 4] = b"VBRG";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 20;
pub const SAMPLE_RATE: u32 = 48_000;
pub const SAMPLES_PER_PACKET: usize = 480;
pub const PACKET_DURATION_US: u64 = 10_000;
pub const AUDIO_PACKET_LEN: usize = HEADER_LEN + SAMPLES_PER_PACKET * 2;
pub const AUDIO_PORT: u16 = 5100;
pub const CONTROL_PORT: u16 = 5101;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Pi to client: audio from the radio.
    Rx = 0,
    /// Client to Pi: audio for the radio.
    Tx = 1,
}

/// A decoded audio packet or keepalive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub direction: Direction,
    pub sequence: u32,
    pub timestamp_us: u64,
    /// Empty (keepalive) or exactly [`SAMPLES_PER_PACKET`] samples.
    pub samples: Vec<i16>,
}

/// Why a datagram was rejected. Each variant is one rule in protocol.md.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketError {
    /// Shorter than the 20-byte header.
    Short,
    BadMagic,
    BadVersion,
    BadDirection,
    /// `sample_count` other than 0 or 480.
    BadSampleCount,
    /// Payload length does not equal `sample_count * 2`.
    LengthMismatch,
}

impl std::fmt::Display for PacketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            PacketError::Short => "packet shorter than the header",
            PacketError::BadMagic => "wrong magic",
            PacketError::BadVersion => "wrong protocol version",
            PacketError::BadDirection => "direction not 0 or 1",
            PacketError::BadSampleCount => "sample count not 0 or 480",
            PacketError::LengthMismatch => "payload length does not match the sample count",
        };
        f.write_str(text)
    }
}

impl std::error::Error for PacketError {}

impl Packet {
    pub fn keepalive(sequence: u32, timestamp_us: u64) -> Packet {
        Packet {
            direction: Direction::Tx,
            sequence,
            timestamp_us,
            samples: Vec::new(),
        }
    }

    pub fn is_keepalive(&self) -> bool {
        self.samples.is_empty()
    }

    /// Encodes the packet. Panics if the sample count is neither 0 nor 480, which
    /// a caller that packetizes correctly can never produce.
    pub fn encode(&self) -> Vec<u8> {
        let count = self.samples.len();
        assert!(
            count == 0 || count == SAMPLES_PER_PACKET,
            "bridge packets carry 0 or 480 samples"
        );
        let mut out = Vec::with_capacity(HEADER_LEN + count * 2);
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.push(self.direction as u8);
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&self.timestamp_us.to_le_bytes());
        out.extend_from_slice(&(count as u16).to_le_bytes());
        for s in &self.samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    pub fn decode(data: &[u8]) -> Result<Packet, PacketError> {
        if data.len() < HEADER_LEN {
            return Err(PacketError::Short);
        }
        if &data[0..4] != MAGIC {
            return Err(PacketError::BadMagic);
        }
        if data[4] != VERSION {
            return Err(PacketError::BadVersion);
        }
        let direction = match data[5] {
            0 => Direction::Rx,
            1 => Direction::Tx,
            _ => return Err(PacketError::BadDirection),
        };
        let sequence = u32::from_le_bytes(data[6..10].try_into().unwrap());
        let timestamp_us = u64::from_le_bytes(data[10..18].try_into().unwrap());
        let count = u16::from_le_bytes(data[18..20].try_into().unwrap()) as usize;
        if count != 0 && count != SAMPLES_PER_PACKET {
            return Err(PacketError::BadSampleCount);
        }
        let payload = &data[HEADER_LEN..];
        if payload.len() != count * 2 {
            return Err(PacketError::LengthMismatch);
        }
        let samples = payload
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        Ok(Packet {
            direction,
            sequence,
            timestamp_us,
            samples,
        })
    }
}

/// Control requests a client may send on 5101.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Hello {
        client: String,
    },
    Monitor {
        client: String,
    },
    Status,
    SetGain {
        tx: Option<u8>,
        rx: Option<u8>,
    },
    SetTxAudioCap {
        seconds: u16,
    },
    /// Pi-side ask P3; only sent when the bridge advertises it (feature discovery, P4).
    Bye,
}

impl Request {
    pub fn to_json(&self) -> String {
        let value = match self {
            Request::Hello { client } => serde_json::json!({"cmd": "hello", "client": client}),
            Request::Monitor { client } => serde_json::json!({"cmd": "monitor", "client": client}),
            Request::Status => serde_json::json!({"cmd": "status"}),
            Request::SetGain { tx, rx } => {
                let mut m = serde_json::Map::new();
                m.insert("cmd".into(), "set_gain".into());
                if let Some(tx) = tx {
                    m.insert("tx".into(), (*tx).into());
                }
                if let Some(rx) = rx {
                    m.insert("rx".into(), (*rx).into());
                }
                Value::Object(m)
            }
            Request::SetTxAudioCap { seconds } => {
                serde_json::json!({"cmd": "set_tx_audio_cap", "seconds": seconds})
            }
            Request::Bye => serde_json::json!({"cmd": "bye"}),
        };
        value.to_string()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.to_json().into_bytes()
    }
}

/// The `hello` reply, also sent for `monitor` (with `role`). Fields the bridge
/// does not send yet (P4: `version`, `features`) stay `None`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct HelloReply {
    pub device: Option<String>,
    pub protocol: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub audio_port: Option<u16>,
    pub role: Option<String>,
    pub version: Option<String>,
    #[serde(default)]
    pub features: Option<Vec<String>>,
}

impl HelloReply {
    /// True only when the bridge lists `feature` in a `features` array.
    pub fn supports(&self, feature: &str) -> bool {
        self.features
            .as_ref()
            .is_some_and(|f| f.iter().any(|x| x == feature))
    }
}

/// Any control reply: `ok` plus either the typed payload or an error string.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Ok(Value),
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplyError {
    NotJson,
    /// A JSON value that is not an object with a boolean `ok`.
    NoOk,
}

impl std::fmt::Display for ReplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplyError::NotJson => f.write_str("reply is not JSON"),
            ReplyError::NoOk => f.write_str("reply has no boolean ok"),
        }
    }
}

impl std::error::Error for ReplyError {}

impl Reply {
    pub fn parse(data: &[u8]) -> Result<Reply, ReplyError> {
        let value: Value = serde_json::from_slice(data).map_err(|_| ReplyError::NotJson)?;
        let ok = value
            .get("ok")
            .and_then(Value::as_bool)
            .ok_or(ReplyError::NoOk)?;
        if ok {
            Ok(Reply::Ok(value))
        } else {
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            Ok(Reply::Error(error))
        }
    }

    pub fn hello(&self) -> Option<HelloReply> {
        match self {
            Reply::Ok(v) => serde_json::from_value(v.clone()).ok(),
            Reply::Error(_) => None,
        }
    }

    /// The `cmd` this reply answers cannot be told from the datagram; the caller
    /// matches replies to requests by order on its socket.
    pub fn value(&self) -> Option<&Value> {
        match self {
            Reply::Ok(v) => Some(v),
            Reply::Error(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(seq: u32) -> Packet {
        let samples = (0..SAMPLES_PER_PACKET)
            .map(|i| (i as i16).wrapping_mul(37))
            .collect();
        Packet {
            direction: Direction::Rx,
            sequence: seq,
            timestamp_us: 123_456_789,
            samples,
        }
    }

    #[test]
    fn codec_round_trip_audio_and_keepalive() {
        let a = audio(7);
        let bytes = a.encode();
        assert_eq!(bytes.len(), AUDIO_PACKET_LEN);
        assert_eq!(&bytes[0..4], b"VBRG");
        assert_eq!(bytes[4], 1);
        assert_eq!(bytes[5], 0);
        assert_eq!(Packet::decode(&bytes).unwrap(), a);

        let k = Packet::keepalive(u32::MAX, 5);
        let kb = k.encode();
        assert_eq!(kb.len(), HEADER_LEN);
        assert_eq!(&kb[18..20], &[0, 0]);
        let decoded = Packet::decode(&kb).unwrap();
        assert!(decoded.is_keepalive());
        assert_eq!(decoded, k);
    }

    #[test]
    fn codec_header_is_little_endian_struct_4sbbiqh() {
        // Layout check against struct.Struct("<4sBBIQH").
        let p = Packet {
            direction: Direction::Tx,
            sequence: 0x0403_0201,
            timestamp_us: 0x0807_0605_0403_0201,
            samples: vec![],
        };
        let b = p.encode();
        assert_eq!(&b[6..10], &[1, 2, 3, 4]);
        assert_eq!(&b[10..18], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn codec_rejects_every_malformed_case_in_protocol_md() {
        let good = audio(1).encode();

        assert_eq!(Packet::decode(&good[..19]), Err(PacketError::Short));

        let mut magic = good.clone();
        magic[0] = b'X';
        assert_eq!(Packet::decode(&magic), Err(PacketError::BadMagic));

        let mut version = good.clone();
        version[4] = 2;
        assert_eq!(Packet::decode(&version), Err(PacketError::BadVersion));

        let mut direction = good.clone();
        direction[5] = 2;
        assert_eq!(Packet::decode(&direction), Err(PacketError::BadDirection));

        let mut count = good.clone();
        count[18..20].copy_from_slice(&479u16.to_le_bytes());
        assert_eq!(Packet::decode(&count), Err(PacketError::BadSampleCount));

        // Right count, wrong payload length: one byte short and one byte long.
        assert_eq!(
            Packet::decode(&good[..good.len() - 1]),
            Err(PacketError::LengthMismatch)
        );
        let mut long = good.clone();
        long.push(0);
        assert_eq!(Packet::decode(&long), Err(PacketError::LengthMismatch));

        // A keepalive header with a payload is also a length mismatch.
        let mut keepalive_with_payload = Packet::keepalive(0, 0).encode();
        keepalive_with_payload.extend_from_slice(&[0, 0]);
        assert_eq!(
            Packet::decode(&keepalive_with_payload),
            Err(PacketError::LengthMismatch)
        );
    }

    #[test]
    fn control_requests_match_protocol_md() {
        assert_eq!(
            Request::Hello {
                client: "vectorcable".into()
            }
            .to_json(),
            r#"{"client":"vectorcable","cmd":"hello"}"#
        );
        assert_eq!(
            Request::Monitor {
                client: "vc".into()
            }
            .to_json(),
            r#"{"client":"vc","cmd":"monitor"}"#
        );
        assert_eq!(Request::Status.to_json(), r#"{"cmd":"status"}"#);
        assert_eq!(
            Request::SetGain {
                tx: Some(40),
                rx: None
            }
            .to_json(),
            r#"{"cmd":"set_gain","tx":40}"#
        );
        assert_eq!(
            Request::SetGain {
                tx: Some(1),
                rx: Some(2)
            }
            .to_json(),
            r#"{"cmd":"set_gain","rx":2,"tx":1}"#
        );
        assert_eq!(
            Request::SetTxAudioCap { seconds: 120 }.to_json(),
            r#"{"cmd":"set_tx_audio_cap","seconds":120}"#
        );
        assert_eq!(Request::Bye.to_json(), r#"{"cmd":"bye"}"#);
    }

    #[test]
    fn hello_reply_parses_and_unknown_features_are_none() {
        let data = br#"{"ok":true,"device":"VectorBridge","protocol":1,"sample_rate":48000,"channels":1,"audio_port":5100}"#;
        let reply = Reply::parse(data).unwrap();
        let hello = reply.hello().unwrap();
        assert_eq!(hello.device.as_deref(), Some("VectorBridge"));
        assert_eq!(hello.protocol, Some(1));
        assert_eq!(hello.audio_port, Some(5100));
        assert_eq!(hello.role, None);
        assert_eq!(hello.version, None);
        assert_eq!(hello.features, None);
        assert!(!hello.supports("bye"));

        let with_features = br#"{"ok":true,"protocol":1,"role":"monitor","version":"0.2.0","features":["monitor","bye"]}"#;
        let hello = Reply::parse(with_features).unwrap().hello().unwrap();
        assert_eq!(hello.role.as_deref(), Some("monitor"));
        assert!(hello.supports("bye"));
        assert!(!hello.supports("tx_audio_cap"));
    }

    #[test]
    fn error_replies_and_bad_datagrams() {
        assert_eq!(
            Reply::parse(br#"{"ok":false,"error":"monitors_full"}"#),
            Ok(Reply::Error("monitors_full".into()))
        );
        assert_eq!(
            Reply::parse(br#"{"ok":false}"#),
            Ok(Reply::Error("unknown".into()))
        );
        assert_eq!(Reply::parse(b"not json"), Err(ReplyError::NotJson));
        assert_eq!(Reply::parse(br#"{"device":"x"}"#), Err(ReplyError::NoOk));
        assert_eq!(Reply::parse(b"[1,2]"), Err(ReplyError::NoOk));
    }
}
