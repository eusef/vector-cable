//! Audio adapters: how the core reads the `VectorBridge TX` device and writes
//! the `VectorBridge RX` device on each platform.
//!
//! The device clock drives the audio, not the session loop's timer: each
//! tick the adapter says how many RX frames the device has consumed and how
//! many TX frames it has captured, and the core renders or packetises exactly
//! that many. The playback trim then locks to the device clock.
//!
//! Phase 1 on macOS has no virtual devices yet (the HAL plug-in is Phase 2),
//! so only the null adapter exists here. The PipeWire adapter (Milestone 2)
//! and the CoreAudio and WASAPI adapters implement the same trait.

use crate::protocol::{SAMPLES_PER_PACKET, SAMPLE_RATE};
use crate::time::LocalTime;

/// Most frames one call hands over.
pub const MAX_FRAMES_PER_CALL: usize = SAMPLES_PER_PACKET * 10;
/// Frames owed after a loop stall beyond this (100 ms) are dropped, as a real
/// device drops them: paying them all back would run the cursor ahead of real
/// time and leave the output silent until the trim caught up. An ordinary late
/// tick, under 100 ms, loses nothing.
pub const MAX_CATCH_UP_FRAMES: usize = SAMPLES_PER_PACKET * 10;

/// Mono 48 kHz int16 both ways, paced by the device.
pub trait AudioAdapter: Send {
    fn name(&self) -> &str;
    /// Why the devices are not working, when they are not (a helper process
    /// died, a pipe broke). `None` while all is well.
    fn fault(&self) -> Option<String> {
        None
    }
    /// Frames the RX device has consumed since the last call and now wants,
    /// on its own clock. The core renders exactly this many.
    fn rx_demand(&mut self, now: LocalTime) -> usize;
    /// Fills `out` with frames the TX device captured since the last call, on
    /// its own clock, and returns how many were written.
    fn read_tx(&mut self, now: LocalTime, out: &mut [i16]) -> usize;
    /// Writes rendered RX audio (what the radio heard) to the RX device.
    fn write_rx(&mut self, samples: &[i16]);
}

/// No devices. A simulated device clock (nominal 48 kHz plus `device_ppm`)
/// paces both directions; TX reads silence unless a test queues samples; RX
/// writes are counted and the last block kept. Used by tests and by macOS in
/// this phase.
#[derive(Debug)]
pub struct NullAdapter {
    device_ppm: f64,
    started: Option<LocalTime>,
    rx_consumed: u64,
    tx_captured: u64,
    tx_queue: std::collections::VecDeque<i16>,
    rx_samples_written: u64,
    rx_last: Vec<i16>,
}

impl Default for NullAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl NullAdapter {
    pub fn new() -> Self {
        Self::with_device_ppm(0.0)
    }

    /// A device clock running `ppm` parts per million fast (negative: slow).
    pub fn with_device_ppm(ppm: f64) -> Self {
        NullAdapter {
            device_ppm: ppm,
            started: None,
            rx_consumed: 0,
            tx_captured: 0,
            tx_queue: std::collections::VecDeque::new(),
            rx_samples_written: 0,
            rx_last: Vec::new(),
        }
    }

    /// Frames the device clock has counted since the start.
    fn device_frames(&mut self, now: LocalTime) -> u64 {
        let start = *self.started.get_or_insert(now);
        let elapsed = (now - start).max(0.0);
        (elapsed * SAMPLE_RATE as f64 * (1.0 + self.device_ppm * 1e-6)).round() as u64
    }

    /// Queues samples the next `read_tx` calls will return.
    pub fn queue_tx(&mut self, samples: &[i16]) {
        self.tx_queue.extend(samples.iter().copied());
    }

    pub fn rx_samples_written(&self) -> u64 {
        self.rx_samples_written
    }

    pub fn rx_last(&self) -> &[i16] {
        &self.rx_last
    }
}

impl AudioAdapter for NullAdapter {
    fn name(&self) -> &str {
        "null"
    }

    fn rx_demand(&mut self, now: LocalTime) -> usize {
        let frames = self.device_frames(now);
        let due = frames.saturating_sub(self.rx_consumed) as usize;
        let n = due.min(MAX_CATCH_UP_FRAMES);
        // Whatever the loop missed beyond the cap is dropped, as a device would.
        self.rx_consumed = frames;
        n
    }

    fn read_tx(&mut self, now: LocalTime, out: &mut [i16]) -> usize {
        let frames = self.device_frames(now);
        let due = frames.saturating_sub(self.tx_captured) as usize;
        let n = due.min(out.len()).min(MAX_CATCH_UP_FRAMES);
        self.tx_captured = frames;
        for o in out[..n].iter_mut() {
            *o = self.tx_queue.pop_front().unwrap_or(0);
        }
        n
    }

    fn write_rx(&mut self, samples: &[i16]) {
        self.rx_samples_written += samples.len() as u64;
        self.rx_last.clear();
        self.rx_last.extend_from_slice(samples);
    }
}

/// Peak level in dBFS, or `None` for digital silence (shown as a dash, never 0).
/// Meters show audio level in dBFS, never signal strength.
pub fn peak_dbfs(samples: &[i16]) -> Option<f64> {
    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    if peak == 0 {
        return None;
    }
    Some(20.0 * (peak as f64 / i16::MAX as f64).log10())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assembler::Chunk;
    use crate::playback::Timeline;

    #[test]
    fn null_adapter_paces_both_directions_on_its_device_clock() {
        let mut a = NullAdapter::with_device_ppm(0.0);
        assert_eq!(a.rx_demand(LocalTime(0.0)), 0, "nothing elapsed yet");
        assert_eq!(a.rx_demand(LocalTime(0.01)), 480);
        assert_eq!(a.rx_demand(LocalTime(0.025)), 720);
        // A late tick under 100 ms loses nothing.
        assert_eq!(a.rx_demand(LocalTime(0.12)), 4560);
        // A stalled loop gets at most the cap; the rest is dropped, not paid back later.
        assert_eq!(a.rx_demand(LocalTime(1.0)), MAX_CATCH_UP_FRAMES);
        assert_eq!(a.rx_demand(LocalTime(1.01)), 480);
        a.queue_tx(&[1, 2, 3]);
        let mut out = [9i16; 480];
        assert_eq!(a.read_tx(LocalTime(0.0), &mut out), 0);
        assert_eq!(a.read_tx(LocalTime(0.01), &mut out), 480);
        assert_eq!(&out[..5], &[1, 2, 3, 0, 0]);
        a.write_rx(&[7; 480]);
        assert_eq!(a.rx_samples_written(), 480);
        assert_eq!(a.rx_last()[0], 7);

        // A fast device wants more frames per wall second.
        let mut fast = NullAdapter::with_device_ppm(1000.0);
        fast.rx_demand(LocalTime(0.0));
        let total: usize = (1..=10_000)
            .map(|k| fast.rx_demand(LocalTime(k as f64 * 0.01)))
            .sum();
        assert_eq!(total, 4_804_800, "100 s at +1000 ppm");
    }

    /// A loop stall over 1 s (a laptop hiccup) must not run the cursor ahead of real time:
    /// after it, audio keeps playing at the same offset without a long silence.
    #[test]
    fn loop_stall_does_not_run_the_cursor_ahead() {
        let mut adapter = NullAdapter::new();
        let mut timeline = Timeline::default();
        let mut out = Vec::new();
        let mut sample_index: u64 = 0;
        let mut t = 0.0;
        let mut underruns_after_stall = 0;
        let mut i = 0;
        let mut pushed_until = 0.0;
        while t < 20.0 {
            let now = LocalTime(t);
            // Everything the socket buffered while the loop was frozen arrives in a burst.
            while pushed_until <= t + 1e-9 {
                let samples: Vec<i16> = (0..480)
                    .map(|k| ((sample_index + k) % 200) as i16 * 100)
                    .collect();
                sample_index += 480;
                timeline.push(&Chunk {
                    samples,
                    capture_start: LocalTime(pushed_until - 0.005),
                    filled: false,
                });
                pushed_until += 0.01;
            }
            let want = adapter.rx_demand(now);
            out.resize(want, 0);
            let before = timeline.stats().underrun_samples;
            timeline.render(now, &mut out);
            if t > 6.0 {
                underruns_after_stall += timeline.stats().underrun_samples - before;
            }
            i += 1;
            // The loop freezes for 1.5 s at 5 s.
            t = if i == 500 { 6.5 } else { t + 0.01 };
        }
        // A 1.5 s clock jump is a snap, not a slew; at most that one re-anchor.
        assert!(timeline.stats().reanchors <= 1, "{:?}", timeline.stats());
        assert!(
            underruns_after_stall <= 480,
            "silent after the stall: {underruns_after_stall} samples"
        );
        assert!(
            timeline.trim_ppm().abs() < 500.0,
            "trim {}",
            timeline.trim_ppm()
        );
    }

    /// The trim must track the device clock: a device running fast consumes
    /// more frames per wall second than the Pi sends, so the resampler has to
    /// stretch by the same amount. Perfect network, no jitter.
    #[test]
    fn playback_trim_locks_to_the_device_clock() {
        for device_ppm in [200.0, -200.0] {
            let mut adapter = NullAdapter::with_device_ppm(device_ppm);
            let mut timeline = Timeline::default();
            let mut out = Vec::new();
            let mut sample_index: u64 = 0;
            let mut settled_underruns = 0;
            let steps = 120 * 100; // 120 s of 10 ms ticks
            for i in 0..steps {
                let now = LocalTime(i as f64 * 0.01);
                let samples: Vec<i16> = (0..480)
                    .map(|k| ((sample_index + k) % 200) as i16 * 100)
                    .collect();
                sample_index += 480;
                timeline.push(&Chunk {
                    samples,
                    capture_start: now - 0.005,
                    filled: false,
                });
                let want = adapter.rx_demand(now);
                out.resize(want, 0);
                timeline.render(now, &mut out);
                adapter.write_rx(&out);
                if i == 100 {
                    // The first second fills the latency target; nothing after may underrun.
                    settled_underruns = timeline.stats().underrun_samples;
                }
            }
            let trim = timeline.trim_ppm();
            assert!(
                (trim + device_ppm).abs() < 25.0,
                "device {device_ppm} ppm: trim {trim} ppm"
            );
            assert_eq!(timeline.stats().underrun_samples, settled_underruns);
        }
    }

    #[test]
    fn peak_dbfs_is_none_for_silence() {
        assert_eq!(peak_dbfs(&[0, 0]), None);
        assert!((peak_dbfs(&[i16::MAX]).unwrap()).abs() < 1e-9);
        assert!((peak_dbfs(&[0, -3277]).unwrap() + 20.0).abs() < 0.01);
    }
}
