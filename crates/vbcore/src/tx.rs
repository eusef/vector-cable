//! TX path: packetize the TX device at 10 ms and stream only while the app
//! plays something (silence gate), with keepalives in between.
//!
//! The virtual TX device produces silence all the time; that silence must not
//! be streamed, because every transmission should start fresh at the Pi's
//! 100 ms jitter target and a continuous stream walks with clock drift
//! (`jitter.py`: 100 ms target, 400 ms max). On a VOX rig the stream itself
//! keys the radio, so closing the gate promptly is a safety rule.
//!
//! Time is passed in; this module does no I/O.

use crate::protocol::{Packet, SAMPLES_PER_PACKET, SAMPLE_RATE};
use crate::time::LocalTime;

/// Gate opens on the first sample at or above this level. ASSUMED in the brief; tune in spike 0b.
pub const GATE_OPEN_DBFS: f64 = -60.0;
/// Gate closes after this long with every sample below the open level. ASSUMED in the brief.
pub const GATE_CLOSE_HOLD_S: f64 = 0.2;
/// Keepalive interval while the gate is closed. The Pi needs one at least every 2 s and drops a
/// client silent for 5 s; 1 s matches the iPad and survives one lost datagram.
pub const KEEPALIVE_INTERVAL_S: f64 = 1.0;

/// The int16 count matching [`GATE_OPEN_DBFS`]: 32767 * 10^(-60/20) = 32.77.
pub fn gate_open_threshold() -> i16 {
    (i16::MAX as f64 * 10f64.powf(GATE_OPEN_DBFS / 20.0)).ceil() as i16
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxEvent {
    /// A 480-sample TX packet to send on the audio socket.
    Audio(Packet),
    /// A 0-sample TX packet to send on the audio socket.
    Keepalive(Packet),
}

impl TxEvent {
    pub fn packet(&self) -> &Packet {
        match self {
            TxEvent::Audio(p) | TxEvent::Keepalive(p) => p,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxStats {
    pub audio_packets: u64,
    pub keepalives: u64,
    pub gate_opens: u64,
    pub gate_closes: u64,
}

#[derive(Clone, Debug)]
pub struct TxPath {
    gated: bool,
    open: bool,
    /// Gate off: audio has been pushed since the last `stop()`.
    active: bool,
    quiet_since: Option<LocalTime>,
    sequence: u32,
    pending: Vec<i16>,
    /// Local time of the first pending sample.
    pending_start: Option<LocalTime>,
    last_send: Option<LocalTime>,
    threshold: i16,
    stats: TxStats,
}

impl Default for TxPath {
    fn default() -> Self {
        Self::new(true)
    }
}

impl TxPath {
    /// `gated` false streams everything, silence included (`vectorcable soak`).
    pub fn new(gated: bool) -> Self {
        TxPath {
            gated,
            open: false,
            active: false,
            quiet_since: None,
            sequence: 0,
            pending: Vec::with_capacity(SAMPLES_PER_PACKET),
            pending_start: None,
            last_send: None,
            threshold: gate_open_threshold(),
            stats: TxStats::default(),
        }
    }

    pub fn stats(&self) -> TxStats {
        self.stats
    }

    /// True while TX audio packets are being produced.
    pub fn streaming(&self) -> bool {
        if self.gated {
            self.open
        } else {
            self.active
        }
    }

    pub fn gated(&self) -> bool {
        self.gated
    }

    /// Feeds samples read from the TX device. `now` is the local time of
    /// `samples[0]`. Returns the packets to send, in order.
    pub fn push(&mut self, samples: &[i16], now: LocalTime) -> Vec<TxEvent> {
        let mut out = Vec::new();
        if !samples.is_empty() {
            self.active = true;
        }
        let mut offset = 0usize;
        while offset < samples.len() {
            if self.pending.is_empty() {
                self.pending_start = Some(now + offset as f64 / SAMPLE_RATE as f64);
            }
            let take = (SAMPLES_PER_PACKET - self.pending.len()).min(samples.len() - offset);
            self.pending
                .extend_from_slice(&samples[offset..offset + take]);
            offset += take;
            if self.pending.len() == SAMPLES_PER_PACKET {
                let start = self.pending_start.take().unwrap_or(now);
                let packet =
                    std::mem::replace(&mut self.pending, Vec::with_capacity(SAMPLES_PER_PACKET));
                if let Some(event) = self.gate(packet, start) {
                    out.push(event);
                }
            }
        }
        out
    }

    fn gate(&mut self, samples: Vec<i16>, start: LocalTime) -> Option<TxEvent> {
        let loud = samples
            .iter()
            .any(|&s| s.unsigned_abs() >= self.threshold as u16);
        if self.gated {
            let was_open = self.open;
            if loud {
                if !self.open {
                    self.open = true;
                    self.stats.gate_opens += 1;
                }
                self.quiet_since = None;
            } else if self.open {
                let since = *self.quiet_since.get_or_insert(start);
                let packet_end = start + SAMPLES_PER_PACKET as f64 / SAMPLE_RATE as f64;
                if packet_end - since >= GATE_CLOSE_HOLD_S - 1e-9 {
                    self.open = false;
                    self.quiet_since = None;
                    self.stats.gate_closes += 1;
                }
            }
            if !was_open && !loud {
                return None;
            }
        }
        let packet = Packet {
            direction: crate::protocol::Direction::Tx,
            sequence: self.sequence,
            timestamp_us: (start.seconds() * 1e6).round().max(0.0) as u64,
            samples,
        };
        self.sequence = self.sequence.wrapping_add(1);
        self.last_send = Some(start);
        self.stats.audio_packets += 1;
        Some(TxEvent::Audio(packet))
    }

    /// Call regularly: returns a keepalive when the gate is closed and one is due.
    pub fn tick(&mut self, now: LocalTime) -> Option<TxEvent> {
        if self.streaming() {
            return None;
        }
        let due = self
            .last_send
            .is_none_or(|t| now - t >= KEEPALIVE_INTERVAL_S);
        if !due {
            return None;
        }
        Some(self.keepalive(now))
    }

    /// A keepalive right now, whatever the gate is doing: used after `hello` so
    /// the audio socket is the last sender and so holds the peer slot.
    pub fn keepalive(&mut self, now: LocalTime) -> TxEvent {
        self.last_send = Some(now);
        self.stats.keepalives += 1;
        // A keepalive carries the next sequence without consuming it (keepalives take no TX
        // sequence numbers; the Pi ignores the field on a 0-sample packet).
        TxEvent::Keepalive(Packet::keepalive(
            self.sequence,
            (now.seconds() * 1e6).round().max(0.0) as u64,
        ))
    }

    /// Stops TX at once: closes the gate and discards partial audio. Nothing is
    /// sent after this until new loud audio arrives.
    pub fn stop(&mut self) {
        if self.open {
            self.stats.gate_closes += 1;
        }
        self.open = false;
        self.active = false;
        self.quiet_since = None;
        self.pending.clear();
        self.pending_start = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, amplitude: i16) -> Vec<i16> {
        (0..n)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect()
    }

    /// Feeds `seconds` of `samples` in 10 ms blocks, ticking between blocks.
    fn run(path: &mut TxPath, start: f64, blocks: &[Vec<i16>]) -> Vec<(f64, TxEvent)> {
        let mut out = Vec::new();
        for (i, block) in blocks.iter().enumerate() {
            let now = LocalTime(start + i as f64 * 0.01);
            if let Some(k) = path.tick(now) {
                out.push((now.seconds(), k));
            }
            for e in path.push(block, now) {
                out.push((now.seconds(), e));
            }
        }
        out
    }

    #[test]
    fn silence_gate_streams_no_audio_only_keepalives_at_most_2_s_apart() {
        let mut path = TxPath::default();
        let blocks: Vec<Vec<i16>> = (0..1000).map(|_| vec![0; 480]).collect(); // 10 s of digital silence
        let events = run(&mut path, 100.0, &blocks);
        assert!(events
            .iter()
            .all(|(_, e)| matches!(e, TxEvent::Keepalive(_))));
        assert!(!events.is_empty());
        let times: Vec<f64> = events.iter().map(|(t, _)| *t).collect();
        assert!(times[0] - 100.0 < 2.0);
        for w in times.windows(2) {
            assert!(
                w[1] - w[0] <= 2.0 + 1e-9,
                "keepalives {} s apart",
                w[1] - w[0]
            );
        }
        assert!(times.last().unwrap() > &108.0);
        assert_eq!(path.stats().audio_packets, 0);
        // Dither-level noise below -60 dBFS is still silence.
        let quiet: Vec<Vec<i16>> = (0..100)
            .map(|_| tone(480, gate_open_threshold() - 1))
            .collect();
        let events = run(&mut path, 200.0, &quiet);
        assert!(events
            .iter()
            .all(|(_, e)| matches!(e, TxEvent::Keepalive(_))));
        assert_eq!(path.stats().audio_packets, 0);
    }

    #[test]
    fn silence_gate_opens_on_first_loud_sample() {
        let mut path = TxPath::default();
        let mut blocks: Vec<Vec<i16>> = (0..50).map(|_| vec![0; 480]).collect();
        let mut first_loud = vec![0i16; 480];
        first_loud[123] = gate_open_threshold(); // exactly -60 dBFS counts as loud
        blocks.push(first_loud.clone());
        blocks.extend((0..10).map(|_| tone(480, 3000)));
        let events = run(&mut path, 0.0, &blocks);
        let audio: Vec<&Packet> = events
            .iter()
            .filter_map(|(_, e)| match e {
                TxEvent::Audio(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(audio.len(), 11);
        assert_eq!(
            audio[0].samples, first_loud,
            "the packet holding the first loud sample is sent"
        );
        assert_eq!(audio[0].sequence, 0);
        assert_eq!(audio[10].sequence, 10);
        assert_eq!(audio[0].timestamp_us, 500_000);
        assert_eq!(path.stats().gate_opens, 1);
        assert!(path.streaming());
    }

    #[test]
    fn silence_gate_closes_200_ms_after_tone_stops_then_keeps_alive() {
        let mut path = TxPath::default();
        let mut blocks: Vec<Vec<i16>> = (0..30).map(|_| tone(480, 3000)).collect(); // 300 ms tone
        blocks.extend((0..300).map(|_| vec![0; 480])); // 3 s silence
        let events = run(&mut path, 10.0, &blocks);
        let audio_times: Vec<f64> = events
            .iter()
            .filter(|(_, e)| matches!(e, TxEvent::Audio(_)))
            .map(|(t, _)| *t)
            .collect();
        // 30 tone packets plus the 20 packets (200 ms) of the close hold.
        assert_eq!(audio_times.len(), 50);
        assert!((audio_times.last().unwrap() - 10.49).abs() < 1e-9);
        assert_eq!(path.stats().gate_closes, 1);
        assert!(!path.streaming());
        let keepalive_times: Vec<f64> = events
            .iter()
            .filter(|(_, e)| matches!(e, TxEvent::Keepalive(_)))
            .map(|(t, _)| *t)
            .collect();
        // One keepalive at session start, before any audio; none while the gate was open.
        assert!(keepalive_times.iter().all(|t| *t == 10.0 || *t > 10.49));
        assert!(!keepalive_times.is_empty());
        for w in keepalive_times.windows(2) {
            assert!(w[1] - w[0] <= 2.0 + 1e-9);
        }
        // A packet's own sequence continues from the last audio packet.
        let last = events.iter().rev().find_map(|(_, e)| match e {
            TxEvent::Keepalive(p) => Some(p.sequence),
            _ => None,
        });
        assert_eq!(last, Some(50));
    }

    #[test]
    fn gate_off_stops_streaming_after_stop_so_keepalives_resume() {
        let mut tx = TxPath::new(false);
        assert!(!tx.streaming(), "nothing pushed yet");
        tx.push(&[0; 480], LocalTime(0.0));
        assert!(tx.streaming());
        assert!(
            tx.tick(LocalTime(0.5)).is_none(),
            "no keepalive while streaming"
        );
        tx.stop();
        assert!(
            !tx.streaming(),
            "stopped: not streaming even with the gate off"
        );
        assert!(tx.tick(LocalTime(1.0)).is_some(), "keepalives resume");
        tx.push(&[0; 480], LocalTime(2.0));
        assert!(tx.streaming(), "resumes on the next audio");
    }

    #[test]
    fn gate_off_streams_silence_for_soak() {
        let mut path = TxPath::new(false);
        let blocks: Vec<Vec<i16>> = (0..100).map(|_| vec![0; 480]).collect();
        let events = run(&mut path, 0.0, &blocks);
        // One keepalive before the first audio block, then every block as audio.
        let audio = events
            .iter()
            .filter(|(_, e)| matches!(e, TxEvent::Audio(_)))
            .count();
        assert_eq!(audio, 100);
        assert!(events.len() <= 101);
        assert!(path.streaming());
    }

    #[test]
    fn stop_closes_gate_at_once_and_drops_partial_audio() {
        let mut path = TxPath::default();
        path.push(&tone(480 + 100, 3000), LocalTime(0.0));
        assert!(path.streaming());
        path.stop();
        assert!(!path.streaming());
        assert!(path.push(&tone(380, 1), LocalTime(0.02)).is_empty());
        assert_eq!(path.stats().audio_packets, 1);
    }
}
