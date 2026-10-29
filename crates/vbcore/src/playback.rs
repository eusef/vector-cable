//! RX playback timeline with drift trim.
//!
//! The RX assembler hands over chunks stamped with local capture time (the
//! Pi's `timestamp_us` through the minimum-delay clock map). The virtual RX
//! device, though, runs on the laptop's audio clock, which is off by some
//! tens of ppm. Without correction the two clocks walk apart: the buffer fills
//! or drains, and the client either re-primes (a step in latency, the walk
//! `bridge_client.py` shows) or drops audio.
//!
//! This module keeps the latency constant:
//!
//! - Every chunk is placed on an absolute sample timeline. A chunk that
//!   continues the previous one is placed contiguously, so Pi timestamp noise
//!   never cuts the waveform; a chunk whose mapped time differs from the
//!   expected position by more than [`STEP_TOLERANCE_S`] is a real re-mapping
//!   (clock map refinement, resync, Pi reboot) and is placed where the map says.
//! - A read cursor moves through that timeline at the laptop audio rate times
//!   a trim ratio. The target cursor is `(now - target_latency)` in mapped time.
//!   A PI controller nudges the ratio by ppm-level amounts so the cursor stays
//!   on target. Audio is resampled at that ratio with cubic (Catmull-Rom)
//!   interpolation.
//! - Data that arrives for a position the cursor has passed (after a stall) is
//!   dropped, never played late, and silence covers the hole. The latency
//!   therefore never steps up after a stall.
//!
//! Resampler choice: the brief suggests the `rubato` crate. It is pure Rust and
//! would check cleanly for every target, but its asynchronous resamplers work
//! chunk-wise with a ratio set per chunk, and this timeline needs a per-sample
//! fractional read cursor on a ring so a stall and a re-mapping can be handled
//! by index arithmetic. The ratio here is within a few hundred ppm of 1 and the
//! signal of interest sits below 3 kHz at 48 kHz, where Catmull-Rom cubic
//! interpolation keeps aliasing far below the noise floor of an HF channel, so a
//! 20-line interpolator is enough and adds no dependency. If a later measurement
//! shows interpolation noise, `rubato` can replace the interpolator behind the
//! same `render` call.

use crate::assembler::Chunk;
use crate::protocol::SAMPLE_RATE;
use crate::time::LocalTime;

/// Default playback latency behind the mapped capture time. Covers one packet
/// (10 ms), the assembler's 100 ms reorder hold and Wi-Fi arrival gaps of up to
/// about 0.4 s. The maintainer's call on 2026-10-06 (DOCS/STATUS.md, question 3), from
/// replays of live bench captures: 220 ms lost 8 to 14 % of the audio, 500 ms
/// 1 to 2 %. FT8 and JS8 see it as a constant DT, which the clock check subtracts.
pub const DEFAULT_TARGET_LATENCY_S: f64 = 0.5;
/// Ring capacity in seconds. Chunks further ahead than this are dropped.
pub const DEFAULT_CAPACITY_S: f64 = 4.0;
/// A mapped chunk time this far from where the run would put it is a re-mapping, not noise.
pub const STEP_TOLERANCE_S: f64 = 0.002;
/// Trim ratio bounds. 2000 ppm covers any sound card and keeps a slew audible-free.
pub const MAX_TRIM_PPM: f64 = 2000.0;
/// Proportional gain: ratio change per second of cursor error (1 ms error, 100 ppm).
pub const TRIM_KP: f64 = 0.1;
/// Integral gain, per second squared.
pub const TRIM_KI: f64 = 0.002;
/// Time constant of the low-pass on the mapped-minus-placed bias.
pub const BIAS_TAU_S: f64 = 10.0;
/// A cursor error beyond this (laptop sleep, clock jump) re-anchors instead of slewing.
pub const REANCHOR_S: f64 = 1.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaybackStats {
    /// Output samples rendered as silence because no audio was there.
    pub underrun_samples: u64,
    /// Chunks dropped because the cursor had already passed them.
    pub stale_chunks: u64,
    /// Chunks dropped because they were beyond the ring.
    pub future_chunks: u64,
    pub written_chunks: u64,
    /// Chunks placed by their mapped time instead of contiguously (re-mappings).
    pub steps: u64,
    /// Times the cursor jumped to its target instead of slewing.
    pub reanchors: u64,
    pub renders: u64,
}

#[derive(Clone, Debug)]
pub struct Timeline {
    fs: f64,
    target_latency: f64,
    samples: Vec<i16>,
    /// Absolute sample index stored in each ring slot, or `i64::MIN` when empty.
    tags: Vec<i64>,
    capacity: usize,
    /// Absolute (placed) sample position being played, fractional.
    cursor: Option<f64>,
    /// End of the last placed chunk (exclusive), for contiguity.
    run_end: Option<i64>,
    /// Low-passed mapped minus placed position, in samples.
    bias: f64,
    bias_set: bool,
    ratio: f64,
    integral: f64,
    last_render: Option<LocalTime>,
    /// Frames of the last ordinary render (a tick, not a stall): what the
    /// device asks for per call, so a re-anchor can land the next render on target.
    nominal_block: f64,
    played_audio: bool,
    /// Set by `link_lost`, cleared by the next chunk.
    lost: bool,
    stats: PlaybackStats,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new(DEFAULT_TARGET_LATENCY_S, DEFAULT_CAPACITY_S)
    }
}

impl Timeline {
    pub fn new(target_latency: f64, capacity_s: f64) -> Self {
        let fs = SAMPLE_RATE as f64;
        let capacity = (capacity_s * fs) as usize;
        Timeline {
            fs,
            target_latency,
            samples: vec![0; capacity],
            tags: vec![i64::MIN; capacity],
            capacity,
            cursor: None,
            run_end: None,
            bias: 0.0,
            bias_set: false,
            ratio: 1.0,
            integral: 0.0,
            last_render: None,
            nominal_block: 0.0,
            played_audio: false,
            lost: false,
            stats: PlaybackStats::default(),
        }
    }

    /// The link is lost: like the silence before the first RX audio, the silence from
    /// here until RX audio returns is not an underrun, and there is no buffer to show.
    pub fn link_lost(&mut self) {
        self.played_audio = false;
        self.lost = true;
    }

    pub fn stats(&self) -> PlaybackStats {
        self.stats
    }

    /// Current trim, in parts per million above nominal.
    pub fn trim_ppm(&self) -> f64 {
        (self.ratio - 1.0) * 1e6
    }

    /// Audio queued ahead of the cursor, in milliseconds. `None` before the first chunk
    /// and while the link is lost.
    pub fn buffer_ms(&self) -> Option<f64> {
        if self.lost {
            return None;
        }
        Some((self.run_end? as f64 - self.cursor?) / self.fs * 1000.0)
    }

    pub fn target_latency(&self) -> f64 {
        self.target_latency
    }

    /// Capture time, in mapped local time, of the sample at the cursor.
    pub fn cursor_capture_time(&self) -> Option<LocalTime> {
        Some(LocalTime((self.cursor? + self.bias) / self.fs))
    }

    fn slot(&self, index: i64) -> usize {
        index.rem_euclid(self.capacity as i64) as usize
    }

    fn sample_at(&self, index: i64) -> Option<i16> {
        let s = self.slot(index);
        (self.tags[s] == index).then_some(self.samples[s])
    }

    /// Adds one assembled chunk.
    pub fn push(&mut self, chunk: &Chunk) {
        let len = chunk.samples.len() as i64;
        if len == 0 {
            return;
        }
        self.lost = false;
        let mapped = (chunk.capture_start.seconds() * self.fs).round() as i64;
        let mut stepped = false;
        let placed = match self.run_end {
            Some(end)
                if ((mapped as f64 - self.bias) - end as f64).abs()
                    <= STEP_TOLERANCE_S * self.fs =>
            {
                end
            }
            Some(_) => {
                self.stats.steps += 1;
                stepped = true;
                mapped
            }
            None => mapped,
        };
        let residual = (mapped - placed) as f64;
        if stepped || !self.bias_set {
            // A new run starts on the mapped clock, so the bias restarts at zero. The
            // cursor moves by the same amount, so the latency (and the trim the
            // controller has settled on) carry over instead of being re-learnt.
            let old_bias = self.bias;
            self.bias = residual;
            self.bias_set = true;
            if let Some(c) = self.cursor {
                self.cursor = Some(c + (old_bias - self.bias));
            }
        } else {
            // One chunk is 10 ms; a first-order low-pass with BIAS_TAU_S.
            let alpha = (len as f64 / self.fs) / BIAS_TAU_S;
            self.bias += alpha * (residual - self.bias);
        }
        self.run_end = Some(placed + len);

        if let Some(cursor) = self.cursor {
            if (placed + len) as f64 <= cursor.floor() {
                self.stats.stale_chunks += 1;
                return;
            }
            if placed as f64 > cursor + (self.capacity as i64 - len) as f64 {
                self.stats.future_chunks += 1;
                return;
            }
        }
        for (k, &s) in chunk.samples.iter().enumerate() {
            let index = placed + k as i64;
            let slot = self.slot(index);
            self.samples[slot] = s;
            self.tags[slot] = index;
        }
        self.stats.written_chunks += 1;
    }

    /// Renders `out.len()` samples for the RX device at local time `now` (the
    /// time the first output sample will be heard, as far as the caller knows).
    pub fn render(&mut self, now: LocalTime, out: &mut [i16]) {
        self.stats.renders += 1;
        let target = (now.seconds() - self.target_latency) * self.fs - self.bias;
        let dt = self.last_render.map(|t| now - t).unwrap_or(0.0).max(0.0);
        self.last_render = Some(now);

        let cursor = match self.cursor {
            None => {
                self.cursor = Some(target);
                target
            }
            Some(c) => c,
        };
        let error_s = (target - cursor) / self.fs;
        let cursor = if !self.played_audio || error_s.abs() > REANCHOR_S {
            // Nothing meaningful is playing yet (startup, while the clock map settles) or the
            // clock jumped: snap instead of slewing.
            self.integral = 0.0;
            self.ratio = 1.0;
            if self.played_audio {
                self.stats.reanchors += 1;
                // The setpoint is "the cursor is on target when a render starts". The
                // frames owed now (the device buffer refilling after the jump) are
                // placed so the next ordinary render starts on its target.
                let block = if self.nominal_block > 0.0 {
                    self.nominal_block
                } else {
                    out.len() as f64
                };
                target - out.len() as f64 + block
            } else {
                target
            }
        } else {
            if dt > 0.0 && dt < 0.1 {
                self.nominal_block = out.len() as f64;
            }
            self.integral += error_s * dt;
            let trim = TRIM_KP * error_s + TRIM_KI * self.integral;
            let max = MAX_TRIM_PPM * 1e-6;
            let trim = trim.clamp(-max, max);
            if (TRIM_KP * error_s + TRIM_KI * self.integral - trim).abs() > 0.0 {
                // Anti-windup: hold the integral while clamped.
                self.integral -= error_s * dt;
            }
            self.ratio = 1.0 + trim;
            cursor
        };

        let mut pos = cursor;
        for o in out.iter_mut() {
            let i = pos.floor() as i64;
            let frac = pos - i as f64;
            let y1 = self.sample_at(i);
            let y2 = self.sample_at(i + 1);
            *o = match (y1, y2) {
                (Some(y1), Some(y2)) => {
                    let y0 = self.sample_at(i - 1).unwrap_or(y1);
                    let y3 = self.sample_at(i + 2).unwrap_or(y2);
                    self.played_audio = true;
                    catmull_rom(y0 as f64, y1 as f64, y2 as f64, y3 as f64, frac)
                }
                (Some(y1), None) => {
                    self.played_audio = true;
                    y1
                }
                (None, _) => {
                    // Silence before the first RX audio is not an underrun.
                    if self.played_audio {
                        self.stats.underrun_samples += 1;
                    }
                    0
                }
            };
            pos += self.ratio;
        }
        self.cursor = Some(pos);
    }
}

fn catmull_rom(y0: f64, y1: f64, y2: f64, y3: f64, t: f64) -> i16 {
    let a = -0.5 * y0 + 1.5 * y1 - 1.5 * y2 + 0.5 * y3;
    let b = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c = -0.5 * y0 + 0.5 * y2;
    let y = ((a * t + b) * t + c) * t + y1;
    y.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assembler::RxAssembler;
    use crate::protocol::{Direction, Packet, SAMPLES_PER_PACKET};

    struct Lcg(u64);
    impl Lcg {
        fn next_f64(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 11) & ((1 << 53) - 1)) as f64 / (1u64 << 53) as f64
        }
    }

    struct Outcome {
        offset0: f64,
        worst_dev: f64,
        underruns_outside_outages: u64,
        /// Underrun samples inside each outage window, in `outages` order.
        outage_underruns: Vec<u64>,
        stats: PlaybackStats,
        final_ppm: f64,
    }

    /// The Pi stream and its mishaps. Times are true seconds.
    struct Scenario {
        minutes: f64,
        /// Laptop audio device clock error.
        audio_ppm: f64,
        /// Pi monotonic clock error (absorbed by the clock map).
        pi_ppm: f64,
        /// Pi sound card clock error (absorbed by the trim).
        card_ppm: f64,
        /// `(start, len)`: packets sent in the window are held and delivered in a burst at the end.
        stalls: Vec<(f64, f64)>,
        /// `(start, len)`: packets sent in the window are lost.
        gaps: Vec<(f64, f64)>,
        /// Chance that any one packet is lost on its own (scattered loss, zero-filled).
        loss: f64,
        /// Playback latency behind the mapped capture time.
        target: f64,
        /// The Pi restarts at this time: its clock and sequence numbers start again from zero
        /// and no packets are sent for `reboot_len`.
        reboot: Option<(f64, f64)>,
    }

    impl Scenario {
        fn plain(minutes: f64, audio_ppm: f64) -> Scenario {
            Scenario {
                minutes,
                audio_ppm,
                pi_ppm: -30.0,
                card_ppm: 20.0,
                stalls: Vec::new(),
                gaps: Vec::new(),
                loss: 0.0,
                target: DEFAULT_TARGET_LATENCY_S,
                reboot: None,
            }
        }

        /// Windows in which audio is allowed to be missing, with a settle time after each.
        fn outages(&self) -> Vec<(f64, f64)> {
            let settle = self.target + 0.05;
            let mut out: Vec<(f64, f64)> = Vec::new();
            for &(start, len) in &self.stalls {
                // A short stall sits inside the jitter buffer; only a long one empties it.
                if len + 0.03 + 0.005 > self.target {
                    out.push((start, len + settle));
                }
            }
            for &(start, len) in &self.gaps {
                out.push((start, len + settle));
            }
            if let Some((start, len)) = self.reboot {
                // The clock map needs its first jitter-free packets after a restart.
                out.push((start, len + settle + 0.5));
            }
            out
        }
    }

    /// Time code: a 1 s sawtooth whose sample value gives the capture time modulo 1 s to
    /// 50 us, so the rendered audio itself says what the listener hears and when.
    fn time_code(t: f64) -> i16 {
        (t.rem_euclid(1.0) * 20_000.0 - 10_000.0).round() as i16
    }

    fn decode_time_code(v: i16) -> f64 {
        (v as f64 + 10_000.0) / 20_000.0
    }

    /// Runs the scenario. The Pi's sound card captures the time code at 48 kHz on its own
    /// clock and the Pi stamps each packet from its monotonic clock, then sends it with a 5 ms
    /// fixed delay plus uniform jitter up to 30 ms (every 20th packet jitter-free). The laptop's
    /// wall clock is true time; its audio device asks for 480 samples every 10 ms of its own
    /// clock. The offset is measured on the rendered audio: output time minus the capture time
    /// the first rendered sample of each block decodes to.
    fn simulate(sc: &Scenario) -> Outcome {
        let fs = SAMPLE_RATE as f64;
        let total_packets = (sc.minutes * 60.0 * 100.0) as u32;
        let mut rng = Lcg(7);
        // Its own generator, so a scenario without loss sees the same jitter as before.
        let mut loss_rng = Lcg(11);
        // (arrival, sequence, timestamp_us, true capture time)
        let mut arrivals: Vec<(f64, u32, u64, f64)> = Vec::with_capacity(total_packets as usize);
        let packet_period = 0.01 * (1.0 + sc.card_ppm * 1e-6);
        let mut sequence = 0u32;
        let mut pi_epoch = 0.0;
        let mut rebooted = false;
        for i in 0..total_packets {
            let capture_true = i as f64 * packet_period;
            let sent = capture_true + packet_period;
            if let Some((start, len)) = sc.reboot {
                if sent >= start && sent < start + len {
                    continue;
                }
                if sent >= start + len && !rebooted {
                    rebooted = true;
                    sequence = 0;
                    pi_epoch = start;
                }
            }
            if sc.gaps.iter().any(|&(g, l)| sent >= g && sent < g + l)
                || (sc.loss > 0.0 && loss_rng.next_f64() < sc.loss)
            {
                sequence = sequence.wrapping_add(1);
                continue;
            }
            let jitter = if i % 20 == 0 {
                0.0
            } else {
                rng.next_f64() * 0.03
            };
            let mut at = sent + 0.005 + jitter;
            for &(start, len) in &sc.stalls {
                if sent >= start && sent < start + len {
                    // The burst still crosses the 5 ms wire; nothing arrives faster than that.
                    at = start + len + 0.005 + (sent - start) * 0.01;
                }
            }
            let ts = ((capture_true - pi_epoch) * (1.0 + sc.pi_ppm * 1e-6) * 1e6).round() as u64;
            arrivals.push((at, sequence, ts, capture_true));
            sequence = sequence.wrapping_add(1);
        }
        arrivals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        let outages = sc.outages();
        let in_outage = |t: f64| outages.iter().any(|&(s, l)| t >= s && t < s + l);
        let mut assembler = RxAssembler::default();
        let mut timeline = Timeline::new(sc.target, DEFAULT_CAPACITY_S);
        let mut out = vec![0i16; SAMPLES_PER_PACKET];
        let render_period = 0.01 / (1.0 + sc.audio_ppm * 1e-6);
        let mut next_render = 0.0;
        let mut render_no = 0u64;
        let mut arrival_i = 0;
        let mut offset0: Option<f64> = None;
        let mut worst_dev: f64 = 0.0;
        let mut worst_at = 0.0;
        let mut underruns_outside_outages = 0u64;
        let mut outage_underruns = vec![0u64; outages.len()];
        let end = sc.minutes * 60.0;
        while next_render < end {
            let now = next_render;
            while arrival_i < arrivals.len() && arrivals[arrival_i].0 <= now {
                let (at, seq, ts, capture_true) = arrivals[arrival_i];
                let samples = (0..SAMPLES_PER_PACKET)
                    .map(|k| time_code(capture_true + k as f64 / fs))
                    .collect();
                assembler.insert(
                    Packet {
                        direction: Direction::Rx,
                        sequence: seq,
                        timestamp_us: ts,
                        samples,
                    },
                    LocalTime(at),
                );
                for chunk in assembler.drain(LocalTime(at)) {
                    timeline.push(&chunk);
                }
                arrival_i += 1;
            }
            for chunk in assembler.drain(LocalTime(now)) {
                timeline.push(&chunk);
            }
            let before = timeline.stats().underrun_samples;
            timeline.render(LocalTime(now), &mut out);
            let under = timeline.stats().underrun_samples - before;
            if now >= 2.0 {
                match outages.iter().position(|&(s, l)| now >= s && now < s + l) {
                    Some(i) => outage_underruns[i] += under,
                    None => underruns_outside_outages += under,
                }
            }
            // Decode the first rendered sample. Skip blocks with missing audio and blocks
            // starting at the sawtooth wrap, where the cubic interpolates across the edge:
            // the first three samples must climb the ramp one sample step apart.
            let decoded = decode_time_code(out[0]);
            // (The code quantum is 50 us; a sample step is 21 us.)
            let step = 1.0 / fs;
            let on_ramp = (1..5)
                .all(|k| (decode_time_code(out[k]) - decoded - k as f64 * step).abs() < 0.0001);
            let near_wrap = !(0.002..0.998).contains(&decoded) || !on_ramp;
            // A zero-filled lost packet decodes to mid-second and passes the ramp test
            // (5 samples of silence miss it by at most 84 us). The ramp climbs 0.42 code
            // units a sample, so 5 zeros in a row are never live audio.
            let filled = out[..5].iter().all(|&v| v == 0);
            if now >= 2.0 && under == 0 && !near_wrap && !filled && !in_outage(now) {
                // The code repeats every second, so read the offset within half a second of
                // the target.
                let mut offset = now.rem_euclid(1.0) - decoded;
                if offset < sc.target - 0.5 {
                    offset += 1.0;
                } else if offset > sc.target + 0.5 {
                    offset -= 1.0;
                }
                match offset0 {
                    None => offset0 = Some(offset),
                    Some(o0) => {
                        if (offset - o0).abs() > worst_dev {
                            worst_dev = (offset - o0).abs();
                            worst_at = now;
                        }
                    }
                }
            }
            render_no += 1;
            next_render = render_no as f64 * render_period;
        }
        eprintln!(
            "audio {} ppm: offset0 {:.4} worst_dev {:.4} at {:.1} s, trim {:.1} ppm, {:?}",
            sc.audio_ppm,
            offset0.unwrap_or(f64::NAN),
            worst_dev,
            worst_at,
            timeline.trim_ppm(),
            timeline.stats()
        );
        Outcome {
            offset0: offset0.unwrap(),
            worst_dev,
            underruns_outside_outages,
            outage_underruns,
            stats: timeline.stats(),
            final_ppm: timeline.trim_ppm(),
        }
    }

    /// 30 min at `audio_ppm` with jitter, a 100 to 130 ms stall about once a minute, and one
    /// 2 s stall at 10 min.
    fn drift_scenario(audio_ppm: f64) -> Scenario {
        let mut sc = Scenario::plain(30.0, audio_ppm);
        sc.stalls = (0..30)
            .map(|k| (k as f64 * 60.0 + 17.0, 0.1 + 0.03 * (k % 2) as f64))
            .collect();
        sc.stalls.push((600.0, 2.0));
        sc
    }

    fn check(audio_ppm: f64) {
        let sc = drift_scenario(audio_ppm);
        let o = simulate(&sc);
        // The fixed 5 ms transport delay stays in the mapped time (the clock map removes
        // only jitter), so the air-to-ear offset is the target latency plus 5 ms.
        assert!(
            (o.offset0 - DEFAULT_TARGET_LATENCY_S - 0.005).abs() < 0.003,
            "start offset {}",
            o.offset0
        );
        assert!(
            o.worst_dev < 0.010,
            "rendered audio walked {} s from its start offset",
            o.worst_dev
        );
        assert_eq!(
            o.underruns_outside_outages, 0,
            "underruns outside the 2 s stall (short stalls must sit inside the buffer): {:?}",
            o.stats
        );
        assert_eq!(
            o.stats.reanchors, 0,
            "a re-anchor is a step walk: {:?}",
            o.stats
        );
        // The 2 s stall's audio past the target is dropped: about one chunk per 10 ms.
        let dropped = (2.0 + 0.015 - sc.target) / 0.01;
        assert!(
            (o.stats.stale_chunks as f64) > dropped - 30.0
                && (o.stats.stale_chunks as f64) < dropped + 40.0,
            "stall audio dropped, not played late (about {dropped:.0} chunks): {:?}",
            o.stats
        );
        // Locked: the trim settles at the laptop-minus-sound-card clock difference.
        let expected_ppm = -audio_ppm - 20.0;
        assert!(
            (o.final_ppm - expected_ppm).abs() < 15.0,
            "trim {} ppm, expected about {}",
            o.final_ppm,
            expected_ppm
        );
    }

    #[test]
    fn drift_trim_holds_rendered_audio_offset_at_plus_100_ppm_with_jitter_and_stalls() {
        check(100.0);
    }

    #[test]
    fn drift_trim_holds_rendered_audio_offset_at_minus_100_ppm_with_jitter_and_stalls() {
        check(-100.0);
    }

    /// A lost run of more than 50 packets makes the assembler resync, and a Pi restart
    /// re-maps its clock from zero. Neither may re-learn the latency: after the audio is back
    /// the offset is where it was and the stream does not stutter.
    #[test]
    fn resync_and_pi_reboot_keep_the_latency_and_do_not_stutter() {
        let mut sc = Scenario::plain(5.0, 60.0);
        sc.gaps = vec![(60.0, 1.0)]; // 100 packets lost
        sc.reboot = Some((150.0, 3.0));
        let o = simulate(&sc);
        assert!(
            (o.offset0 - DEFAULT_TARGET_LATENCY_S - 0.005).abs() < 0.003,
            "start offset {}",
            o.offset0
        );
        assert!(
            o.worst_dev < 0.010,
            "offset walked {} s across the gap or the reboot",
            o.worst_dev
        );
        assert_eq!(o.stats.reanchors, 0, "{:?}", o.stats);
        assert!(o.stats.steps >= 2, "a resync and a reboot: {:?}", o.stats);
        assert_eq!(o.underruns_outside_outages, 0, "{:?}", o.stats);
        // Inside each outage only the missing audio itself is silent: no burst afterwards.
        let fs = SAMPLE_RATE as f64;
        for (i, &(_, len)) in sc.outages().iter().enumerate() {
            let allowed = ((len - DEFAULT_TARGET_LATENCY_S - 0.05 + 0.1) * fs) as u64;
            assert!(
                o.outage_underruns[i] <= allowed,
                "outage {i}: {} underrun samples, at most {allowed}",
                o.outage_underruns[i]
            );
        }
    }

    /// Baseline for the bench Pi's 2.4 GHz link on 2026-10-06 (DOCS/STATUS.md, "Next step"):
    /// about 2 % scattered loss plus a Wi-Fi stall of 0.5 to 4 s every 10 to 30 s. The
    /// timeline drops audio that arrives after its play time, so each stall costs its length
    /// less the slack in the target latency, and scattered loss is zero-filled without
    /// counting as underrun. The underrun fraction therefore tracks the share of time spent
    /// in stalls, not the loss rate. The same link at the old 220 ms target and at today's
    /// default shows what the larger target buys. Run with `--nocapture` to see the numbers.
    #[test]
    fn baseline_underruns_track_stall_time_under_bursty_wifi_loss() {
        let mut sc = Scenario::plain(10.0, 50.0);
        sc.loss = 0.02;
        let end = sc.minutes * 60.0;
        let mut rng = Lcg(13);
        let mut start = 5.0;
        loop {
            start += 10.0 + 20.0 * rng.next_f64();
            let len = 0.5 + 3.5 * rng.next_f64();
            if start + len > end - 2.0 {
                break;
            }
            sc.stalls.push((start, len));
        }
        assert!(sc.stalls.len() >= 20, "{} stalls", sc.stalls.len());

        let fs = SAMPLE_RATE as f64;
        let stall_s: f64 = sc.stalls.iter().map(|&(_, len)| len).sum();
        let mut underrun = Vec::new();
        for target in [0.22, DEFAULT_TARGET_LATENCY_S] {
            sc.target = target;
            let o = simulate(&sc);
            let underrun_s = o.stats.underrun_samples as f64 / fs;
            // A packet is sent one packet after capture and crosses a 5 ms wire, so a stall
            // silences its length plus 15 ms, less the target latency.
            let expected_s: f64 = sc
                .stalls
                .iter()
                .map(|&(_, len)| (len + 0.015 - target).max(0.0))
                .sum();
            eprintln!(
                "baseline at {target} s: {} stalls, {:.1} % of the time in stalls, {:.1} % \
                 underrun ({underrun_s:.1} s, expected about {expected_s:.1} s), {:.0} % loss, {:?}",
                sc.stalls.len(),
                100.0 * stall_s / end,
                100.0 * underrun_s / end,
                100.0 * sc.loss,
                o.stats
            );
            // Every underrun sits in a stall: scattered loss is filled, never heard as a dropout.
            assert_eq!(o.underruns_outside_outages, 0, "{target} s: {:?}", o.stats);
            // Jitter can delay the first packet after a stall by up to 30 ms more, and a loss
            // there holds the burst another 100 ms.
            let slack = 0.15 * sc.stalls.len() as f64;
            assert!(
                (underrun_s - expected_s).abs() < slack,
                "{target} s: underrun {underrun_s:.2} s, expected {expected_s:.2} s within {slack:.2} s"
            );
            // The latency holds through every stall: no re-anchor, no walk.
            assert_eq!(o.stats.reanchors, 0, "{target} s: {:?}", o.stats);
            assert!(
                o.worst_dev < 0.010,
                "{target} s: offset walked {} s",
                o.worst_dev
            );
            underrun.push(underrun_s);
        }
        // Each stall long enough to empty both buffers loses the difference in target less.
        let bought: f64 = sc
            .stalls
            .iter()
            .map(|&(_, len)| {
                (len + 0.015 - 0.22).max(0.0) - (len + 0.015 - DEFAULT_TARGET_LATENCY_S).max(0.0)
            })
            .sum();
        let saved = underrun[0] - underrun[1];
        assert!(
            (saved - bought).abs() < 0.15 * sc.stalls.len() as f64,
            "the larger target saved {saved:.2} s, expected about {bought:.2} s"
        );
    }

    #[test]
    fn catmull_rom_passes_through_knots_and_interpolates_a_line() {
        assert_eq!(catmull_rom(0.0, 10.0, 20.0, 30.0, 0.0), 10);
        assert_eq!(catmull_rom(0.0, 10.0, 20.0, 30.0, 0.5), 15);
        assert_eq!(catmull_rom(0.0, 10.0, 20.0, 30.0, 0.999), 20);
        assert_eq!(
            catmull_rom(-40000.0, -40000.0, -40000.0, -40000.0, 0.3),
            i16::MIN
        );
    }

    #[test]
    fn timeline_drops_stale_chunks_and_counts_underruns() {
        let mut t = Timeline::new(0.1, 1.0);
        let mut out = vec![0i16; 480];
        t.render(LocalTime(10.0), &mut out); // cursor at 9.9 s
        assert_eq!(
            t.stats().underrun_samples,
            0,
            "silence before any audio is not an underrun"
        );
        let stale = Chunk {
            samples: vec![5; 480],
            capture_start: LocalTime(9.0),
            filled: false,
        };
        t.push(&stale);
        assert_eq!(t.stats().stale_chunks, 1);
        let live = Chunk {
            samples: vec![5; 480],
            capture_start: LocalTime(9.91),
            filled: false,
        };
        t.push(&live);
        t.render(LocalTime(10.01), &mut out);
        assert!(out.contains(&5));
        assert!(t.buffer_ms().unwrap() < 10.5 && t.buffer_ms().unwrap() > -0.5);
    }

    #[test]
    fn a_lost_link_stops_the_underrun_count_and_hides_the_buffer_until_audio_returns() {
        let mut t = Timeline::new(0.1, 1.0);
        let mut out = vec![0i16; 480];
        t.render(LocalTime(10.0), &mut out); // cursor at 9.9 s
        t.push(&Chunk {
            samples: vec![5; 480],
            capture_start: LocalTime(9.91),
            filled: false,
        });
        t.render(LocalTime(10.01), &mut out);
        assert!(out.contains(&5));
        // Starved but not lost: silence counts.
        t.render(LocalTime(10.02), &mut out);
        t.render(LocalTime(10.03), &mut out);
        let counted = t.stats().underrun_samples;
        assert!(counted > 0);
        assert!(t.buffer_ms().unwrap() < 0.0);

        t.link_lost();
        for k in 0..100 {
            t.render(LocalTime(10.03 + k as f64 * 0.01), &mut out);
        }
        assert_eq!(t.stats().underrun_samples, counted, "no count while lost");
        assert_eq!(t.buffer_ms(), None, "no runaway buffer while lost");

        t.push(&Chunk {
            samples: vec![7; 480],
            capture_start: LocalTime(10.93),
            filled: false,
        });
        assert!(t.buffer_ms().is_some(), "the buffer shows again with audio");
        t.render(LocalTime(11.03), &mut out);
        assert!(out.contains(&7), "audio plays after the loss");
        t.render(LocalTime(11.04), &mut out);
        t.render(LocalTime(11.05), &mut out);
        assert!(
            t.stats().underrun_samples > counted,
            "underruns count again once audio has returned"
        );
    }

    /// After a clock jump the cursor snaps so that the next ordinary render
    /// starts on its target: no residual error, whatever size the catch-up render was.
    #[test]
    fn reanchor_lands_the_next_render_on_target() {
        let mut t = Timeline::default();
        let mut idx: u64 = 0;
        let mut out = vec![0i16; 480];
        let mut push = |t: &mut Timeline, at: f64| {
            let samples: Vec<i16> = (0..480).map(|k| ((idx + k) % 100) as i16 * 50).collect();
            idx += 480;
            t.push(&Chunk {
                samples,
                capture_start: LocalTime(at - 0.005),
                filled: false,
            });
        };
        for i in 0..300 {
            let now = i as f64 * 0.01;
            push(&mut t, now);
            t.render(LocalTime(now), &mut out);
        }
        assert_eq!(t.stats().reanchors, 0);
        let before = t.trim_ppm();
        // The loop freezes for 2 s; everything arrives in a burst, then a 100 ms catch-up render.
        let resume = 5.0;
        let mut at = 3.0;
        while at <= resume {
            push(&mut t, at);
            at += 0.01;
        }
        let mut big = vec![0i16; 4800];
        t.render(LocalTime(resume), &mut big);
        assert_eq!(t.stats().reanchors, 1);
        // The next ordinary render starts on target: the trim barely moves.
        for i in 1..=50 {
            let now = resume + i as f64 * 0.01;
            push(&mut t, now);
            t.render(LocalTime(now), &mut out);
        }
        assert!(
            (t.trim_ppm() - before).abs() < 100.0,
            "trim {} after re-anchor, {} before",
            t.trim_ppm(),
            before
        );
        assert_eq!(t.stats().reanchors, 1);
    }
}
