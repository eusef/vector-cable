//! Clock check: the laptop's clock error from WSJT-X decodes.
//!
//! With no internet the laptop has no NTP. WSJT-X reports every decode over
//! UDP (default `127.0.0.1:2237`, or a multicast group so it coexists with
//! GridTracker and JTAlert). Other stations are on UTC, so the median DT of
//! recent decodes, less the known RX path latency, approximates this
//! laptop's clock error. The app reports it; it never sets system time.
//!
//! Sign: `clock_error_s` is the laptop clock minus UTC. Positive means the
//! laptop is ahead (signals appear late, DT positive).

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

pub const WSJTX_MAGIC: u32 = 0xadbc_cbda;
pub const WSJTX_DECODE_TYPE: u32 = 2;
pub const DEFAULT_WSJTX_ADDR: &str = "127.0.0.1:2237";
/// Warn when the clock error is above this.
pub const WARN_ABOVE_S: f64 = 0.5;
pub const DEFAULT_WINDOW: usize = 20;

/// One WSJT-X `Decode` message (type 2).
#[derive(Clone, Debug, PartialEq)]
pub struct Decode {
    pub id: String,
    pub new: bool,
    /// Milliseconds since midnight UTC, from the laptop clock.
    pub time_ms: u32,
    pub snr: i32,
    pub dt: f64,
    pub df: u32,
    pub mode: String,
    pub message: String,
    pub low_confidence: bool,
    pub off_air: bool,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    }
    fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }
    fn f64(&mut self) -> Option<f64> {
        self.take(8)
            .map(|b| f64::from_be_bytes(b.try_into().unwrap()))
    }
    fn bool(&mut self) -> Option<bool> {
        self.take(1).map(|b| b[0] != 0)
    }
    /// Qt `utf8`: u32 length (0xffffffff for null) then bytes.
    fn utf8(&mut self) -> Option<String> {
        let len = self.u32()?;
        if len == u32::MAX {
            return Some(String::new());
        }
        self.take(len as usize)
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }
}

/// Parses a WSJT-X datagram. `None` for anything but a well-formed Decode.
pub fn parse_decode(data: &[u8]) -> Option<Decode> {
    let mut r = Reader { data, pos: 0 };
    if r.u32()? != WSJTX_MAGIC {
        return None;
    }
    let _schema = r.u32()?;
    if r.u32()? != WSJTX_DECODE_TYPE {
        return None;
    }
    let id = r.utf8()?;
    let new = r.bool()?;
    let time_ms = r.u32()?;
    let snr = r.i32()?;
    let dt = r.f64()?;
    if !dt.is_finite() {
        // A NaN or infinite DT would poison the median.
        return None;
    }
    Some(Decode {
        id,
        new,
        time_ms,
        snr,
        dt,
        df: r.u32()?,
        mode: r.utf8()?,
        message: r.utf8()?,
        low_confidence: r.bool()?,
        off_air: r.bool()?,
    })
}

/// Encodes a Decode message, for tests and tools.
pub fn encode_decode(d: &Decode) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u32).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut out = Vec::new();
    out.extend_from_slice(&WSJTX_MAGIC.to_be_bytes());
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&WSJTX_DECODE_TYPE.to_be_bytes());
    utf8(&mut out, &d.id);
    out.push(d.new as u8);
    out.extend_from_slice(&d.time_ms.to_be_bytes());
    out.extend_from_slice(&d.snr.to_be_bytes());
    out.extend_from_slice(&d.dt.to_be_bytes());
    out.extend_from_slice(&d.df.to_be_bytes());
    utf8(&mut out, &d.mode);
    utf8(&mut out, &d.message);
    out.push(d.low_confidence as u8);
    out.push(d.off_air as u8);
    out
}

/// The DTs of the last `capacity` decodes. Ported from vector-comm `DTWindow`.
/// Other stations' clocks scatter around true time, so the median, not the
/// mean, is this laptop's error.
#[derive(Clone, Debug)]
pub struct DtWindow {
    pub capacity: usize,
    values: Vec<f64>,
}

impl Default for DtWindow {
    fn default() -> Self {
        Self::new(DEFAULT_WINDOW)
    }
}

impl DtWindow {
    pub fn new(capacity: usize) -> Self {
        DtWindow {
            capacity,
            values: Vec::new(),
        }
    }

    /// Ignores a non-finite value: it would poison the median.
    pub fn add(&mut self, dt: f64) {
        if !dt.is_finite() {
            return;
        }
        self.values.push(dt);
        if self.values.len() > self.capacity {
            let excess = self.values.len() - self.capacity;
            self.values.drain(..excess);
        }
    }

    pub fn clear(&mut self) {
        self.values.clear();
    }

    pub fn values(&self) -> &[f64] {
        &self.values
    }

    pub fn median(&self) -> Option<f64> {
        if self.values.is_empty() {
            return None;
        }
        let mut sorted = self.values.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mid = sorted.len() / 2;
        Some(if sorted.len() % 2 == 0 {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        } else {
            sorted[mid]
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockEstimate {
    /// Laptop clock minus UTC, in seconds.
    pub clock_error_s: f64,
    pub decodes: usize,
    pub warn: bool,
}

/// Accumulates decodes and reports the clock error.
#[derive(Clone, Debug)]
pub struct ClockCheck {
    window: DtWindow,
    /// Known RX path latency (bridge to app), subtracted from the median DT.
    rx_latency_s: f64,
}

impl ClockCheck {
    pub fn new(rx_latency_s: f64) -> Self {
        ClockCheck {
            window: DtWindow::default(),
            rx_latency_s,
        }
    }

    /// Skips off-air decodes and non-finite DT.
    pub fn note(&mut self, decode: &Decode) {
        if decode.off_air || !decode.dt.is_finite() {
            return;
        }
        self.window.add(decode.dt);
    }

    pub fn clear(&mut self) {
        self.window.clear();
    }

    pub fn estimate(&self) -> Option<ClockEstimate> {
        let median = self.window.median()?;
        let clock_error_s = median - self.rx_latency_s;
        Some(ClockEstimate {
            clock_error_s,
            decodes: self.window.values().len(),
            warn: clock_error_s.abs() > WARN_ABOVE_S,
        })
    }
}

/// Formats an estimate the way the TUI and `vectorcable status` show it.
pub fn describe(estimate: Option<ClockEstimate>) -> String {
    match estimate {
        None => "Clock: -- (no decodes yet)".to_string(),
        Some(e) => format!(
            "Clock: {:+.1} s (from {} decodes)",
            e.clock_error_s, e.decodes
        ),
    }
}

/// Binds a listener for WSJT-X UDP reports. A multicast address joins the
/// group on every interface (`0.0.0.0`).
pub fn bind_listener(addr: &str) -> std::io::Result<UdpSocket> {
    let addr: SocketAddr = addr
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e}")))?;
    match addr {
        SocketAddr::V4(v4) if v4.ip().is_multicast() => {
            let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, v4.port()))?;
            socket.join_multicast_v4(v4.ip(), &Ipv4Addr::UNSPECIFIED)?;
            Ok(socket)
        }
        other => UdpSocket::bind(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(dt: f64) -> Decode {
        Decode {
            id: "WSJT-X".into(),
            new: true,
            time_ms: 45_000_000,
            snr: -12,
            dt,
            df: 1500,
            mode: "~".into(),
            message: "CQ W1AW FN31".into(),
            low_confidence: false,
            off_air: false,
        }
    }

    #[test]
    fn wsjtx_decode_round_trip_and_rejects_other_messages() {
        let d = decode(-1.3);
        let bytes = encode_decode(&d);
        assert_eq!(&bytes[0..4], &[0xad, 0xbc, 0xcb, 0xda]);
        assert_eq!(parse_decode(&bytes), Some(d.clone()));
        // Heartbeat (type 0) is not a decode.
        let mut hb = bytes.clone();
        hb[8..12].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(parse_decode(&hb), None);
        assert_eq!(parse_decode(&bytes[..30]), None);
        assert_eq!(parse_decode(b"nope"), None);
    }

    /// Ported from vector-comm `testDTWindowKeepsLastTwentyAndTakesMedian`.
    #[test]
    fn decode_with_non_finite_dt_is_rejected() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let bytes = encode_decode(&decode(bad));
            assert_eq!(parse_decode(&bytes), None, "dt {bad}");
        }
        assert!(parse_decode(&encode_decode(&decode(0.3))).is_some());
    }

    #[test]
    fn non_finite_dt_is_ignored_by_window_and_clock_check() {
        let mut w = DtWindow::new(5);
        w.add(0.5);
        w.add(f64::NAN);
        w.add(f64::INFINITY);
        assert_eq!(w.values(), &[0.5]);
        let mut c = ClockCheck::new(0.0);
        c.note(&decode(f64::NAN));
        assert_eq!(c.estimate(), None);
        c.note(&decode(0.5));
        assert_eq!(c.estimate().map(|e| e.decodes), Some(1));
    }

    #[test]
    fn dt_window_keeps_last_twenty_and_takes_median() {
        let mut w = DtWindow::default();
        assert_eq!(w.median(), None);
        for dt in 0..25 {
            w.add(dt as f64);
        }
        assert_eq!(w.values().len(), 20);
        assert_eq!(w.median(), Some(14.5)); // 5 through 24
        w.add(100.0);
        assert_eq!(w.median(), Some(15.5)); // 6 through 24 plus one outlier
    }

    /// Acceptance test 9 in miniature: the laptop clock 1.5 s slow (behind UTC) makes every
    /// decode arrive early by 1.5 s, plus the 0.22 s RX latency and some scatter.
    #[test]
    fn clock_check_median_dt_has_right_sign_and_size() {
        let mut check = ClockCheck::new(0.22);
        assert_eq!(check.estimate(), None);
        let scatter = [
            0.05, -0.12, 0.31, 0.0, -0.04, 0.9, 0.02, -0.07, 0.11, -0.2, 0.03,
        ];
        for s in scatter {
            check.note(&parse_decode(&encode_decode(&decode(-1.5 + 0.22 + s))).unwrap());
        }
        // An off-air (replayed file) decode never counts.
        let mut replay = decode(5.0);
        replay.off_air = true;
        check.note(&replay);
        let e = check.estimate().unwrap();
        assert_eq!(e.decodes, 11);
        assert!(
            (e.clock_error_s - (-1.5)).abs() < 0.3,
            "error {}",
            e.clock_error_s
        );
        assert!(e.warn);
        assert_eq!(
            describe(Some(e)),
            format!("Clock: {:+.1} s (from 11 decodes)", e.clock_error_s)
        );

        // The laptop 0.3 s fast: positive, no warning.
        let mut fast = ClockCheck::new(0.22);
        for s in scatter {
            fast.note(&decode(0.3 + 0.22 + s));
        }
        let e = fast.estimate().unwrap();
        assert!((e.clock_error_s - 0.3).abs() < 0.1);
        assert!(!e.warn);
        assert_eq!(describe(None), "Clock: -- (no decodes yet)");
    }
}
