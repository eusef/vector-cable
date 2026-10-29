//! Replays a packet capture of a live RX run through the real assembler and
//! timeline, the way `runner.rs` drives them: a 10 ms tick stamps every packet
//! that arrived since the last tick with the tick's time, drains the assembler
//! and renders 480 samples. It reports where the silence came from.
//!
//! Capture on the Mac while `vectorcable up --listen` runs (classic pcap, not pcapng):
//!
//! ```text
//! sudo tcpdump -i en0 -s 0 -w rx.pcap 'udp and src host <bridge address>'
//! ```
//!
//! Then:
//!
//! ```text
//! VECTORCABLE_PCAP=rx.pcap cargo test -p vbcore --release --test replay -- --ignored --nocapture
//! ```
//!
//! `VECTORCABLE_TARGET_S` replays with another playback target (default `DEFAULT_TARGET_LATENCY_S`).

use std::collections::BTreeMap;

use vbcore::assembler::RxAssembler;
use vbcore::playback::{Timeline, DEFAULT_CAPACITY_S, DEFAULT_TARGET_LATENCY_S};
use vbcore::protocol::{Direction, Packet, SAMPLES_PER_PACKET, SAMPLE_RATE};
use vbcore::time::LocalTime;

/// Arrival time (capture clock, seconds) and RX packet, in capture order.
fn read_pcap(data: &[u8]) -> Vec<(f64, Packet)> {
    let u32_at = |le: bool, b: &[u8]| {
        let a = [b[0], b[1], b[2], b[3]];
        if le {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    };
    assert!(data.len() >= 24, "not a pcap file");
    let (le, nanos) = match u32_at(true, &data[..4]) {
        0xa1b2c3d4 => (true, false),
        0xa1b23c4d => (true, true),
        0xd4c3b2a1 => (false, false),
        0x4d3cb2a1 => (false, true),
        0x0a0d0d0a => panic!("pcapng: capture without --apple-pcapng"),
        m => panic!("not a pcap file (magic {m:#x})"),
    };
    let link = u32_at(le, &data[20..24]);
    // Ethernet (Wi-Fi on macOS reports Ethernet framing) or BSD loopback.
    let link_len = match link {
        1 => 14,
        0 => 4,
        l => panic!("link type {l} not handled"),
    };
    let mut out = Vec::new();
    let mut at = 24;
    while at + 16 <= data.len() {
        let sec = u32_at(le, &data[at..]) as f64;
        let frac = u32_at(le, &data[at + 4..]) as f64;
        let incl = u32_at(le, &data[at + 8..]) as usize;
        let t = sec + frac * if nanos { 1e-9 } else { 1e-6 };
        let frame = &data[at + 16..(at + 16 + incl).min(data.len())];
        at += 16 + incl;
        if frame.len() < link_len + 20 {
            continue;
        }
        let ip = &frame[link_len..];
        if ip[0] >> 4 != 4 || ip[9] != 17 {
            continue;
        }
        let Some(udp) = ip.get((ip[0] & 0x0f) as usize * 4..) else {
            continue;
        };
        if udp.len() < 8 {
            continue;
        }
        if let Ok(p) = Packet::decode(&udp[8..]) {
            if p.direction == Direction::Rx && !p.is_keepalive() {
                out.push((t, p));
            }
        }
    }
    out
}

#[test]
#[ignore = "needs a capture: VECTORCABLE_PCAP=path"]
fn replay_capture() {
    let path = std::env::var("VECTORCABLE_PCAP").expect("set VECTORCABLE_PCAP to a pcap file");
    let target: f64 = std::env::var("VECTORCABLE_TARGET_S")
        .ok()
        .map(|s| s.parse().expect("VECTORCABLE_TARGET_S is seconds"))
        .unwrap_or(DEFAULT_TARGET_LATENCY_S);
    let packets = read_pcap(&std::fs::read(&path).expect("read the capture"));
    assert!(!packets.is_empty(), "no RX packets in {path}");
    let t0 = packets[0].0;
    let end = packets.last().unwrap().0 - t0;

    // Network delay above the trailing 10 s minimum: how late each packet was.
    let raw: Vec<f64> = packets
        .iter()
        .map(|(t, p)| (t - t0) - p.timestamp_us as f64 / 1e6)
        .collect();
    let mut excess_hist = [0u64; 4]; // < 50 ms, 50 to 100, 100 to 200, >= 200
    let mut lo = 0;
    for i in 0..packets.len() {
        while packets[i].0 - packets[lo].0 > 10.0 {
            lo += 1;
        }
        let min = raw[lo..=i].iter().cloned().fold(f64::INFINITY, f64::min);
        let e = raw[i] - min;
        excess_hist[match e {
            e if e < 0.05 => 0,
            e if e < 0.1 => 1,
            e if e < 0.2 => 2,
            _ => 3,
        }] += 1;
    }

    // Arrival gaps over 200 ms, which the old 220 ms target could not cover. The threshold
    // stays fixed so captures compare across targets.
    let gaps: Vec<(f64, f64)> = packets
        .windows(2)
        .map(|w| (w[0].0 - t0, w[1].0 - w[0].0))
        .filter(|&(_, g)| g > 0.2)
        .collect();
    let gap_s: f64 = gaps.iter().map(|g| g.1).sum();

    let mut assembler = RxAssembler::default();
    let mut timeline = Timeline::new(target, DEFAULT_CAPACITY_S);
    let mut out = vec![0i16; SAMPLES_PER_PACKET];
    // Underrun seconds per wall-clock second, to line up with the gaps.
    let mut per_second: BTreeMap<u64, f64> = BTreeMap::new();
    let mut next = 0;
    let mut rendered = 0u64;
    let mut tick = 0u64;
    loop {
        let now = tick as f64 * 0.01;
        // One tick past the last arrival, so it is inserted too.
        if now > end + 0.01 {
            break;
        }
        while next < packets.len() && packets[next].0 - t0 <= now {
            assembler.insert(packets[next].1.clone(), LocalTime(now));
            next += 1;
        }
        for chunk in assembler.drain(LocalTime(now)) {
            timeline.push(&chunk);
        }
        let before = timeline.stats().underrun_samples;
        timeline.render(LocalTime(now), &mut out);
        let under = timeline.stats().underrun_samples - before;
        if before > 0 || under > 0 || timeline.buffer_ms().is_some() {
            rendered += out.len() as u64;
        }
        if under > 0 {
            *per_second.entry(now as u64).or_default() += under as f64 / SAMPLE_RATE as f64;
        }
        // Every 20 s, in the units `vectorcable up --plain` prints, to line up with its log.
        if tick % 2000 == 0 {
            let utc = (t0 + now).rem_euclid(86_400.0) as u64;
            println!(
                "trace {:02}:{:02}:{:02}Z underruns {} buffer {:.0} ms",
                utc / 3600,
                utc / 60 % 60,
                utc % 60,
                timeline.stats().underrun_samples / SAMPLES_PER_PACKET as u64,
                timeline.buffer_ms().unwrap_or(f64::NAN)
            );
        }
        tick += 1;
    }

    let fs = SAMPLE_RATE as f64;
    let stats = timeline.stats();
    let under_s = stats.underrun_samples as f64 / fs;
    println!("capture {path}: {:.1} s, {} RX packets", end, packets.len());
    println!("target {:.3} s", target);
    println!("assembler {:?}", assembler.stats());
    println!("playback {:?}", stats);
    println!(
        "underrun {:.1} s = {:.1} % of {:.1} s rendered",
        under_s,
        100.0 * under_s / (rendered as f64 / fs),
        rendered as f64 / fs
    );
    println!(
        "arrival gaps over 200 ms: {} totalling {:.1} s ({:.1} %)",
        gaps.len(),
        gap_s,
        100.0 * gap_s / end
    );
    let n = packets.len() as f64;
    println!(
        "delay above the 10 s minimum: <50 ms {:.1} %, 50-100 ms {:.1} %, 100-200 ms {:.1} %, >=200 ms {:.1} %",
        100.0 * excess_hist[0] as f64 / n,
        100.0 * excess_hist[1] as f64 / n,
        100.0 * excess_hist[2] as f64 / n,
        100.0 * excess_hist[3] as f64 / n
    );
    let mut worst: Vec<(u64, f64)> = per_second.into_iter().collect();
    worst.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    // Silence runs on for up to the playback target after a gap ends, so a gap counts for
    // the second it overlaps and the one after.
    println!("worst seconds (offset from first packet: underrun s, largest arrival gap in or just before it):");
    for &(sec, u) in worst.iter().take(15) {
        let g = gaps
            .iter()
            .filter(|&&(at, len)| at < (sec + 1) as f64 && at + len > sec as f64 - 1.0)
            .map(|g| g.1)
            .fold(0.0, f64::max);
        println!("  {sec:>5} s: {u:.2} s, gap {g:.2} s");
    }
}
