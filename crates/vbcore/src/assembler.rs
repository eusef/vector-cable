//! RX assembler and Pi clock map, ported from
//! `vector-comm/VectorComm/Bridge/BridgeRXAssembler.swift` with its behaviour
//! intact.
//!
//! Turns RX packets from the bridge, which can arrive late, reordered,
//! duplicated or not at all, into a contiguous 48 kHz stream stamped with
//! local capture time.
//!
//! - Reorder: a missing packet is waited for until the packet after it has
//!   been held `hold` seconds (100 ms, per the bridge's advice), then declared
//!   lost and replaced with silence so the stream stays contiguous. Gaps longer
//!   than `max_fill_packets` are not filled; the stream resyncs.
//! - Timing: the Pi's `timestamp_us` is mapped to local time with a
//!   minimum-delay filter over a sliding window, so network jitter does not move
//!   slot timing.
//!
//! Time is passed in by the caller, so this is a plain value type tests can drive.

use std::collections::HashMap;

use crate::protocol::{Direction, Packet, PACKET_DURATION_US, SAMPLES_PER_PACKET, SAMPLE_RATE};
use crate::time::LocalTime;

/// One packet's worth of contiguous audio with its local capture time.
#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    pub samples: Vec<i16>,
    /// Local time of the first sample.
    pub capture_start: LocalTime,
    /// True when this is silence standing in for a lost packet.
    pub filled: bool,
}

impl Chunk {
    /// Local time just after the last sample.
    pub fn capture_end(&self) -> LocalTime {
        self.capture_start + self.samples.len() as f64 / SAMPLE_RATE as f64
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub received: u64,
    pub lost: u64,
    pub late: u64,
    pub duplicates: u64,
    pub resyncs: u64,
}

/// A packet this much older than the newest one means the Pi rebooted (its
/// monotonic clock restarted).
pub const REBOOT_AGE_US: u64 = 5_000_000;
pub const DEFAULT_HOLD_S: f64 = 0.1;
pub const DEFAULT_MAX_FILL_PACKETS: u32 = 50;
pub const DEFAULT_CLOCK_WINDOW_S: f64 = 10.0;

#[derive(Clone, Debug)]
pub struct RxAssembler {
    pub hold: f64,
    pub max_fill_packets: u32,
    stats: Stats,
    pending: HashMap<u32, (Packet, LocalTime)>,
    next_sequence: Option<u32>,
    last_timestamp_us: Option<u64>,
    /// Newest Pi timestamp inserted, used to tell a straggler from a restarted stream.
    newest_timestamp_us: Option<u64>,
    clock: PiClockMap,
}

impl Default for RxAssembler {
    fn default() -> Self {
        Self::new(
            DEFAULT_HOLD_S,
            DEFAULT_MAX_FILL_PACKETS,
            DEFAULT_CLOCK_WINDOW_S,
        )
    }
}

impl RxAssembler {
    pub fn new(hold: f64, max_fill_packets: u32, clock_window: f64) -> Self {
        RxAssembler {
            hold,
            max_fill_packets,
            stats: Stats::default(),
            pending: HashMap::new(),
            next_sequence: None,
            last_timestamp_us: None,
            newest_timestamp_us: None,
            clock: PiClockMap::new(clock_window),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn clock(&self) -> &PiClockMap {
        &self.clock
    }

    pub fn insert(&mut self, packet: Packet, arrived_at: LocalTime) {
        if packet.direction != Direction::Rx || packet.samples.len() != SAMPLES_PER_PACKET {
            return;
        }
        if let Some(next) = self.next_sequence {
            let distance = packet.sequence.wrapping_sub(next) as i32;
            if distance < 0 {
                let restarted = self.newest_timestamp_us.is_some_and(|newest| {
                    packet.timestamp_us > newest || newest - packet.timestamp_us > REBOOT_AGE_US
                });
                if restarted {
                    // Behind in sequence but not an old packet: the Pi service restarted (RX
                    // sequence starts again at 0 while its clock runs on) or the Pi rebooted
                    // (clock restarted too).
                    self.stats.lost += self.pending.len() as u64;
                    self.reset();
                    self.stats.resyncs += 1;
                } else {
                    self.stats.late += 1;
                    return;
                }
            }
        }
        if self.next_sequence.is_none() {
            self.next_sequence = Some(packet.sequence);
        }
        if self.pending.contains_key(&packet.sequence) {
            self.stats.duplicates += 1;
            return;
        }
        self.stats.received += 1;
        self.newest_timestamp_us = Some(
            self.newest_timestamp_us
                .unwrap_or(0)
                .max(packet.timestamp_us),
        );
        self.clock.observe(packet.timestamp_us, arrived_at);
        self.pending.insert(packet.sequence, (packet, arrived_at));
    }

    /// Releases every chunk that is ready at `now`, in sequence order.
    pub fn drain(&mut self, now: LocalTime) -> Vec<Chunk> {
        let mut out = Vec::new();
        while let Some(next) = self.next_sequence {
            if let Some((packet, _)) = self.pending.remove(&next) {
                out.push(Chunk {
                    capture_start: self.clock.local_time(packet.timestamp_us),
                    samples: packet.samples,
                    filled: false,
                });
                self.last_timestamp_us = Some(packet.timestamp_us);
                self.next_sequence = Some(next.wrapping_add(1));
                continue;
            }
            let Some(oldest) = self
                .pending
                .values()
                .map(|(_, at)| *at)
                .min_by(|a, b| a.partial_cmp(b).unwrap())
            else {
                break;
            };
            if now - oldest < self.hold {
                break;
            }
            let Some(nearest) = self
                .pending
                .keys()
                .copied()
                .min_by_key(|k| k.wrapping_sub(next))
            else {
                break;
            };
            let gap = nearest.wrapping_sub(next);
            match self.last_timestamp_us {
                Some(last) if gap <= self.max_fill_packets => {
                    for k in 1..=gap as u64 {
                        let stamp = last + PACKET_DURATION_US * k;
                        out.push(Chunk {
                            samples: vec![0; SAMPLES_PER_PACKET],
                            capture_start: self.clock.local_time(stamp),
                            filled: true,
                        });
                    }
                    self.last_timestamp_us = Some(last + PACKET_DURATION_US * gap as u64);
                }
                _ => self.stats.resyncs += 1,
            }
            self.stats.lost += gap as u64;
            self.next_sequence = Some(nearest);
        }
        out
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.next_sequence = None;
        self.last_timestamp_us = None;
        self.newest_timestamp_us = None;
        self.clock = PiClockMap::new(self.clock.window);
    }
}

/// Maps the Pi's monotonic microseconds to local time.
///
/// Each packet gives `arrival - timestamp`, which is the clock offset plus that
/// packet's delay. The smallest value in the window is the least-delayed
/// packet, so it is the best offset estimate. A Pi packet cannot leave before
/// its last sample, so one packet duration is taken back off. What remains
/// (fixed network and Pi output delay) is a constant that the RX offset
/// setting removes. A long stall only raises offsets, which the minimum
/// ignores; a Pi reboot resets the whole assembler.
#[derive(Clone, Debug)]
pub struct PiClockMap {
    pub window: f64,
    /// Observations with increasing arrival and increasing offset; the front is the window minimum.
    candidates: Vec<(LocalTime, f64)>,
    head: usize,
}

impl PiClockMap {
    pub fn new(window: f64) -> Self {
        PiClockMap {
            window,
            candidates: Vec::new(),
            head: 0,
        }
    }

    pub fn observe(&mut self, timestamp_us: u64, arrived_at: LocalTime) {
        let offset = arrived_at.seconds() - timestamp_us as f64 / 1e6;
        while self.candidates.len() > self.head
            && self.candidates[self.candidates.len() - 1].1 >= offset
        {
            self.candidates.pop();
        }
        self.candidates.push((arrived_at, offset));
        while self.head + 1 < self.candidates.len()
            && arrived_at - self.candidates[self.head].0 > self.window
        {
            self.head += 1;
        }
        if self.head > 256 {
            self.candidates.drain(..self.head);
            self.head = 0;
        }
    }

    pub fn minimum_offset(&self) -> Option<f64> {
        self.candidates.get(self.head).map(|c| c.1)
    }

    pub fn local_time(&self, timestamp_us: u64) -> LocalTime {
        let packet = PACKET_DURATION_US as f64 / 1e6;
        LocalTime(timestamp_us as f64 / 1e6 + self.minimum_offset().unwrap_or(0.0) - packet)
    }
}

#[cfg(test)]
mod tests {
    //! Ported from `vector-comm/VectorCommTests/BridgeProtocolTests.swift`
    //! (`BridgeRXAssemblerTests`).
    use super::*;

    const T0: LocalTime = LocalTime(800_000_000.0);

    fn packet(seq: u32, ts: u64, fill: i16) -> Packet {
        Packet {
            direction: Direction::Rx,
            sequence: seq,
            timestamp_us: ts,
            samples: vec![fill; SAMPLES_PER_PACKET],
        }
    }

    fn first(chunks: &[Chunk]) -> Vec<i16> {
        chunks.iter().map(|c| c.samples[0]).collect()
    }

    /// V2: sequence numbers run across the 2^32 wrap without a gap or a resync.
    #[test]
    fn assembler_runs_across_sequence_wrap() {
        let mut a = RxAssembler::default();
        let mut out = Vec::new();
        for (i, seq) in [u32::MAX - 1, u32::MAX, 0, 1].into_iter().enumerate() {
            let at = T0 + i as f64 * 0.01;
            a.insert(packet(seq, i as u64 * 10_000, i as i16 + 1), at);
            out.extend(a.drain(at));
        }
        assert_eq!(first(&out), [1, 2, 3, 4]);
        assert_eq!(
            a.stats(),
            Stats {
                received: 4,
                ..Default::default()
            }
        );
    }

    /// V2: reordered packets come out in order; a missing one is held 100 ms, then filled with
    /// silence; the lost packet arriving later is dropped; a long gap resyncs without filling.
    #[test]
    fn assembler_reorders_fills_gaps_and_resyncs_long_gaps() {
        let mut a = RxAssembler::new(0.1, 50, 10.0);
        let ts = |s: u32| s as u64 * 10_000;
        a.insert(packet(10, ts(10), 10), T0);
        a.insert(packet(12, ts(12), 12), T0 + 0.01);
        a.insert(packet(11, ts(11), 11), T0 + 0.02);
        assert_eq!(first(&a.drain(T0 + 0.02)), [10, 11, 12]);

        // 13 and 14 go missing.
        a.insert(packet(15, ts(15), 15), T0 + 0.05);
        assert!(a.drain(T0 + 0.149).is_empty(), "still inside the hold");
        let filled = a.drain(T0 + 0.151);
        assert_eq!(
            filled.iter().map(|c| c.filled).collect::<Vec<_>>(),
            [true, true, false]
        );
        assert_eq!(first(&filled), [0, 0, 15]);
        assert!((filled[1].capture_start - filled[0].capture_start - 0.01).abs() < 1e-6);
        assert!((filled[2].capture_start - filled[1].capture_start - 0.01).abs() < 1e-6);

        a.insert(packet(13, ts(13), 1), T0 + 0.16);
        assert!(a.drain(T0 + 0.16).is_empty());

        // A 10 s pause (Pi reopening its device) is not filled.
        a.insert(packet(1016, ts(1016), 16), T0 + 10.2);
        let resumed = a.drain(T0 + 10.35);
        assert_eq!(
            resumed.iter().map(|c| c.filled).collect::<Vec<_>>(),
            [false]
        );
        assert_eq!(
            a.stats(),
            Stats {
                received: 5,
                lost: 1002,
                late: 1,
                duplicates: 0,
                resyncs: 1
            }
        );
    }

    /// V2: a packet behind the stream is a straggler if its timestamp is older, and a restart if
    /// the Pi service restarted (newer timestamp, sequence back to 0) or the Pi rebooted (clock
    /// back near 0).
    #[test]
    fn assembler_tells_straggler_from_restart_and_reboot() {
        let pi_start: u64 = 600_000_000;
        let mut a = RxAssembler::default();
        for seq in 100u32..200 {
            a.insert(packet(seq, pi_start + seq as u64 * 10_000, 1), T0);
        }
        let _ = a.drain(T0);

        // 60 packets (600 ms) late: dropped, the stream carries on.
        a.insert(packet(140, pi_start + 140 * 10_000, 1), T0 + 0.01);
        assert_eq!(a.stats().late, 1);
        assert_eq!(a.stats().resyncs, 0);

        // Service restart only 30 packets in: sequence 0, clock still running.
        a.insert(packet(0, pi_start + 3_000_000, 7), T0 + 1.0);
        assert_eq!(first(&a.drain(T0 + 1.0)), [7]);
        assert_eq!(a.stats().resyncs, 1);

        // Pi reboot: sequence 0 again and the clock restarted near zero.
        a.insert(packet(1, pi_start + 3_010_000, 1), T0 + 1.01);
        a.insert(packet(0, 20_000_000, 9), T0 + 60.0);
        let rebooted = a.drain(T0 + 60.0);
        assert_eq!(first(&rebooted), [9]);
        assert_eq!(a.stats().resyncs, 2);
        assert!(
            (rebooted[0].capture_start - (T0 + 60.0) + 0.01).abs() < 1e-6,
            "the clock map starts over after a reboot"
        );
    }

    /// V2: capture times follow the Pi clock, not arrival, under jitter, reordering, a 130 ms
    /// stall and a 1.5 s Wi-Fi stall.
    #[test]
    fn assembler_capture_time_follows_pi_clock_under_jitter_and_stalls() {
        let mut rng = Lcg(42);
        let fixed_delay = 0.005;
        let pi_start: u64 = 5_000_000_000;
        let mut arrivals: Vec<(u32, LocalTime)> = Vec::new();
        for i in 0..3000u32 {
            let jitter = if i % 20 == 0 {
                0.0
            } else {
                rng.next_f64() * 0.09
            };
            let mut at = T0 + (i as f64 * 0.01 + 0.01 + fixed_delay + jitter);
            // A Wi-Fi stall holds everything sent during it, then delivers it in order as a burst.
            let sent = i as f64 * 0.01 + 0.01;
            for (start, length) in [(15.0, 0.13), (25.0, 1.5)] {
                if sent >= start - 0.1 && sent < start + length {
                    at = T0 + (start + length + (sent - start + 0.1) * 0.01);
                }
            }
            arrivals.push((i, at));
        }
        arrivals.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        let mut a = RxAssembler::default();
        let mut out = Vec::new();
        for (seq, at) in arrivals {
            a.insert(packet(seq, pi_start + seq as u64 * 10_000, 1), at);
            out.extend(a.drain(at));
        }
        out.extend(a.drain(T0 + 100.0));

        assert_eq!(out.len(), 3000);
        assert_eq!(a.stats().lost, 0);
        // The mapped start is the true start plus the fixed transport delay, which the RX offset removes.
        let worst = out
            .iter()
            .enumerate()
            .map(|(i, c)| (c.capture_start - (T0 + (i as f64 * 0.01 + fixed_delay))).abs())
            .fold(0.0, f64::max);
        assert!(worst < 0.0005, "worst {worst}");
    }

    #[test]
    fn clock_map_minimum_tracks_window_and_subtracts_one_packet() {
        let mut m = PiClockMap::new(10.0);
        assert_eq!(m.minimum_offset(), None);
        assert_eq!(m.local_time(1_000_000), LocalTime(0.99));
        m.observe(1_000_000, LocalTime(101.05));
        m.observe(1_010_000, LocalTime(101.02)); // least delayed: offset 100.01
        m.observe(1_020_000, LocalTime(101.09));
        assert!((m.minimum_offset().unwrap() - 100.01).abs() < 1e-9);
        assert!((m.local_time(2_000_000) - LocalTime(102.0)).abs() < 1e-9);
        // 11 s later the old minimum has left the window.
        m.observe(12_000_000, LocalTime(112.05));
        assert!((m.minimum_offset().unwrap() - 100.05).abs() < 1e-9);
    }

    /// Small deterministic generator, standing in for Swift's SeededGenerator.
    pub(crate) struct Lcg(pub u64);
    impl Lcg {
        pub fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 11
        }
        pub fn next_f64(&mut self) -> f64 {
            (self.next_u64() & ((1 << 53) - 1)) as f64 / (1u64 << 53) as f64
        }
    }
}
