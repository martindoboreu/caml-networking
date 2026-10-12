//! `caml-bench`: reproducible benchmarks.
//!
//! * `micro`: in-process costs of the receive path (no network). Timed over
//!   large batches, so the result is not limited by clock resolution.
//! * `loopback`: repeated end-to-end runs over 127.0.0.1 with a paced
//!   sender thread, warmup exclusion, and a cross-run summary. Loopback
//!   measures the OS network stack and scheduler, **not a NIC**; never
//!   compare these numbers with physical-NIC results.
//!
//! Every run prints its environment and exact parameters. Use release
//! builds: `cargo run --release -p caml-bench -- micro`.

use std::hint::black_box;
use std::net::{SocketAddr, UdpSocket};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use caml_engine::clock::wall_clock_ns;
use caml_engine::histogram::LatencySummary;
use caml_engine::sequence::{SeqConfig, SequenceTracker};
use caml_engine::transport::{RxMeta, UdpConfig, UdpSource};
use caml_engine::{Delivery, EventSink, FeedProcessor, ProcessorConfig, RunLimits, run};
use caml_protocol::{MAX_PAYLOAD_1500_MTU, MarketEvent, patch_send_time};
use caml_sim::{FeedConfig, FeedSimulator};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand, Debug)]
enum Mode {
    /// In-process microbenchmarks of parsing, sequencing, and instrumentation.
    Micro {
        /// Repetitions per benchmark; the median is reported.
        #[arg(long, default_value_t = 7)]
        runs: usize,
    },
    /// Repeated end-to-end runs over 127.0.0.1.
    Loopback {
        /// Independent runs (fresh sockets and processor each).
        #[arg(long, default_value_t = 10)]
        runs: usize,
        /// Data packets per run, including warmup.
        #[arg(long, default_value_t = 22_000)]
        packets: u64,
        /// Leading packets excluded from latency statistics.
        #[arg(long, default_value_t = 2_000)]
        warmup: u64,
        /// Target data packets per second (open loop).
        #[arg(long, default_value_t = 10_000.0)]
        rate: f64,
        /// Messages per packet.
        #[arg(long, default_value_t = 4)]
        msgs_per_packet: u16,
        /// Record processing latency for one packet in N (0 = off).
        #[arg(long, default_value_t = 1)]
        processing_sample_every: u32,
    },
}

/// A consumer that does a little work and never allocates.
#[derive(Default)]
struct Checksum(u64);

impl EventSink for Checksum {
    #[inline]
    fn on_event(&mut self, e: &MarketEvent, d: Delivery) {
        self.0 = self
            .0
            .wrapping_mul(31)
            .wrapping_add(e.seq ^ e.quantity ^ d as u64);
    }
}

fn print_environment() {
    println!("## Environment");
    println!();
    println!("| | |");
    println!("|---|---|");
    println!(
        "| OS / arch | {} / {} |",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "| logical CPUs | {} |",
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
    println!("| compiler | {} |", env!("CAML_RUSTC_VERSION"));
    println!("| target | {} |", env!("CAML_TARGET"));
    println!(
        "| build | {} |",
        if cfg!(debug_assertions) {
            "DEBUG (numbers are meaningless; use --release)"
        } else {
            "release"
        }
    );
    println!("| caml-bench | {} |", env!("CARGO_PKG_VERSION"));
    println!();
    println!(
        "CPU model, power plan, and other load are not detected: record them alongside the output."
    );
    println!();
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(f64::NAN)
}

fn spread(v: &[f64]) -> (f64, f64) {
    let min = v.iter().copied().fold(f64::INFINITY, f64::min);
    let max = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (min, max)
}

/// Times `f` (which processes `items` items) `runs` times; returns ns/item.
fn time_per_item(runs: usize, items: usize, mut f: impl FnMut()) -> Vec<f64> {
    (0..runs.max(1))
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_nanos() as f64 / items.max(1) as f64
        })
        .collect()
}

fn row(name: &str, unit: &str, samples: Vec<f64>) {
    let (lo, hi) = spread(&samples);
    println!(
        "| {name} | {:.1} | {lo:.1} – {hi:.1} | {unit} |",
        median(samples)
    );
}

fn encode_stream(msgs: u16, packets: usize) -> Vec<Vec<u8>> {
    let mut sim = FeedSimulator::new(FeedConfig {
        msgs_per_packet: msgs,
        ..FeedConfig::default()
    })
    .unwrap_or_else(|e| panic!("bench config: {e}"));
    let mut buf = [0u8; MAX_PAYLOAD_1500_MTU];
    (0..packets)
        .map(|_| {
            let len = sim
                .next_packet(&mut buf, wall_clock_ns())
                .unwrap_or_else(|e| panic!("encode: {e}"));
            buf[..len].to_vec()
        })
        .collect()
}

fn micro(runs: usize) {
    print_environment();
    println!("## Micro (in-process, no network)");
    println!();
    println!(
        "Median of {runs} runs; range is min – max across runs. Each run times a batch, so results are not limited by clock resolution."
    );
    println!();
    println!("| benchmark | median | range | unit |");
    println!("|---|---:|---:|---|");

    const N: usize = 1_000_000;
    row(
        "Instant::now()",
        "ns/call",
        time_per_item(runs, N, || {
            for _ in 0..N {
                black_box(Instant::now());
            }
        }),
    );
    row(
        "wall_clock_ns()",
        "ns/call",
        time_per_item(runs, N, || {
            for _ in 0..N {
                black_box(wall_clock_ns());
            }
        }),
    );
    row(
        "RxMeta::now() (both clocks)",
        "ns/call",
        time_per_item(runs, N, || {
            for _ in 0..N {
                black_box(RxMeta::now());
            }
        }),
    );

    for (msgs, packets) in [(1u16, 500_000usize), (4, 500_000), (36, 100_000)] {
        let stream = encode_stream(msgs, packets);
        for (label, sample_every, fresh_meta) in [
            ("full instrumentation", 1u32, true),
            ("no processing sample", 0, true),
            ("parse+sequence only", 0, false),
        ] {
            let config = ProcessorConfig {
                processing_sample_every: sample_every,
                ..ProcessorConfig::default()
            };
            let samples = time_per_item(runs, packets, || {
                let mut p = FeedProcessor::with_config(Checksum::default(), config);
                let fixed = RxMeta::now();
                for b in &stream {
                    let meta = if fresh_meta { RxMeta::now() } else { fixed };
                    p.process_packet(black_box(b), meta);
                }
                black_box(p.into_sink().0);
            });
            row(
                &format!("process_packet, {msgs} msg/pkt, {label}"),
                "ns/pkt",
                samples,
            );
        }
    }

    let n = 2_000_000u64;
    let in_order: Vec<u64> = (1..=n).collect();
    let gap_every: Vec<u64> = (1..=n).map(|i| i * 2).collect();
    for (name, cap, seqs) in [
        ("SequenceTracker, in order", 1024usize, &in_order),
        (
            "SequenceTracker, new gap per message, 1024-gap table full",
            1024,
            &gap_every,
        ),
    ] {
        let samples = time_per_item(runs, seqs.len(), || {
            let mut t = SequenceTracker::new(SeqConfig {
                max_tracked_gaps: cap,
                ..SeqConfig::default()
            });
            for &s in seqs.iter() {
                black_box(t.observe(black_box(s)));
            }
            black_box(t.stats());
        });
        row(name, "ns/msg", samples);
    }
    // Late fills against a full table: open 1024 gaps, then fill them
    // newest first (the worst case for a scan from the oldest gap).
    let mut late = Vec::new();
    for round in 0..200u64 {
        let base = round * 4096;
        late.extend((0..=1024u64).map(|i| base + 1 + 2 * i));
        late.extend((0..1024u64).rev().map(|i| base + 2 + 2 * i));
    }
    let samples = time_per_item(runs, late.len(), || {
        let mut t = SequenceTracker::new(SeqConfig::default());
        for &s in &late {
            black_box(t.observe(black_box(s)));
        }
        black_box(t.stats());
    });
    row(
        "SequenceTracker, 1024 open gaps, late fills newest-first",
        "ns/msg",
        samples,
    );
    println!();
}

/// Sleeps for most of the remaining time, then spins.
fn wait_until(deadline: Instant) {
    const SPIN_WINDOW: Duration = Duration::from_millis(2);
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let remaining = deadline - now;
        if remaining > SPIN_WINDOW {
            std::thread::sleep(remaining - SPIN_WINDOW);
        } else {
            std::hint::spin_loop();
        }
    }
}

struct RunResult {
    latency: LatencySummary,
    processing: LatencySummary,
    lost: u64,
    clean: bool,
    achieved_rate: f64,
}

fn loopback_run(
    packets: u64,
    warmup: u64,
    rate: f64,
    msgs: u16,
    processing_sample_every: u32,
) -> Result<RunResult, String> {
    let mut rx = UdpSource::bind(&UdpConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        recv_buffer_bytes: Some(4 << 20),
        poll_timeout: Duration::from_millis(50),
        ..UdpConfig::default()
    })
    .map_err(|e| format!("bind: {e}"))?;
    let target = rx.local_addr().map_err(|e| e.to_string())?;
    let tx = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    tx.connect(target).map_err(|e| e.to_string())?;

    let config = ProcessorConfig {
        processing_sample_every,
        ..ProcessorConfig::default()
    };
    let mut processor = FeedProcessor::with_config(Checksum::default(), config);
    let stop = AtomicBool::new(false);
    let budget = Duration::from_secs_f64(packets as f64 / rate.max(1.0) * 2.0 + 10.0);

    let sender = || -> Result<f64, String> {
        let mut sim = FeedSimulator::new(FeedConfig {
            session_epoch: wall_clock_ns(),
            msgs_per_packet: msgs,
            ..FeedConfig::default()
        })
        .map_err(|e| e.to_string())?;
        let mut buf = [0u8; MAX_PAYLOAD_1500_MTU];
        let start = Instant::now();
        for i in 0..packets {
            wait_until(start + Duration::from_secs_f64(i as f64 / rate));
            let len = sim
                .next_packet(&mut buf, wall_clock_ns())
                .map_err(|e| e.to_string())?;
            patch_send_time(&mut buf[..len], wall_clock_ns()).map_err(|e| e.to_string())?;
            tx.send(&buf[..len]).map_err(|e| e.to_string())?;
        }
        let elapsed = start.elapsed().as_secs_f64();
        let len = sim
            .end_of_session(&mut buf, wall_clock_ns())
            .map_err(|e| e.to_string())?;
        tx.send(&buf[..len]).map_err(|e| e.to_string())?;
        Ok(packets as f64 / elapsed)
    };

    let (sent_rate, recv) = std::thread::scope(|s| {
        let receiver = s.spawn(|| -> Result<(), String> {
            let limits = RunLimits {
                max_datagrams: Some(warmup),
                max_duration: Some(budget),
                report_interval: None,
            };
            run(&mut rx, &mut processor, &stop, limits, |_| {}).map_err(|e| e.to_string())?;
            processor.reset_latency();
            let limits = RunLimits {
                max_datagrams: Some(packets + 1),
                ..limits
            };
            run(&mut rx, &mut processor, &stop, limits, |_| {}).map_err(|e| e.to_string())?;
            Ok(())
        });
        let rate = sender();
        (
            rate,
            receiver
                .join()
                .unwrap_or_else(|_| Err("receiver thread panicked".into())),
        )
    });
    recv?;
    let achieved_rate = sent_rate?;
    let s = processor.snapshot();
    let v = s.integrity();
    Ok(RunResult {
        latency: s.send_to_rx_latency,
        processing: s.processing_latency,
        lost: v.lost_messages,
        clean: v.is_clean() && v.session_ended,
        achieved_rate,
    })
}

fn loopback(
    runs: usize,
    packets: u64,
    warmup: u64,
    rate: f64,
    msgs: u16,
    processing_sample_every: u32,
) -> Result<(), String> {
    if !(rate > 0.0 && rate.is_finite()) {
        return Err("--rate must be a positive number".into());
    }
    if warmup >= packets {
        return Err("--warmup must be smaller than --packets".into());
    }
    print_environment();
    println!("## Loopback (127.0.0.1; OS stack + scheduler, not a NIC)");
    println!();
    println!(
        "Parameters: runs={runs} packets={packets} warmup={warmup} rate={rate}/s msgs/pkt={msgs} processing_sample_every={processing_sample_every}"
    );
    println!();
    println!(
        "| run | sent pkt/s | send→rx p50 | p99 | p99.9 | max | processing p50 | p99 | lost | clean |"
    );
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|:---:|");
    let us = |ns: u64| ns as f64 / 1_000.0;
    let mut cols: [Vec<f64>; 6] = Default::default();
    for i in 1..=runs {
        let r = loopback_run(packets, warmup, rate, msgs, processing_sample_every)?;
        println!(
            "| {i} | {:.0} | {:.1} | {:.1} | {:.1} | {:.1} | {:.2} | {:.2} | {} | {} |",
            r.achieved_rate,
            us(r.latency.p50),
            us(r.latency.p99),
            us(r.latency.p999),
            us(r.latency.max),
            us(r.processing.p50),
            us(r.processing.p99),
            r.lost,
            if r.clean { "yes" } else { "NO" }
        );
        for (col, v) in cols.iter_mut().zip([
            r.latency.p50,
            r.latency.p99,
            r.latency.p999,
            r.latency.max,
            r.processing.p50,
            r.processing.p99,
        ]) {
            col.push(us(v));
        }
    }
    println!();
    println!("Across runs (µs): median [min – max]");
    println!();
    for (name, col) in [
        "send→rx p50",
        "send→rx p99",
        "send→rx p99.9",
        "send→rx max",
        "processing p50",
        "processing p99",
    ]
    .iter()
    .zip(cols)
    {
        let (lo, hi) = spread(&col);
        println!("- {name}: {:.2} [{lo:.2} – {hi:.2}]", median(col));
    }
    println!();
    println!(
        "Processing latency is sampled with `Instant`; on a platform with a coarse clock (e.g. 100 ns on Windows) its p50 is resolution-bound. Use `micro` for in-process costs."
    );
    Ok(())
}

fn main() -> ExitCode {
    let args = Args::parse();
    let result = match args.mode {
        Mode::Micro { runs } => {
            micro(runs);
            Ok(())
        }
        Mode::Loopback {
            runs,
            packets,
            warmup,
            rate,
            msgs_per_packet,
            processing_sample_every,
        } => loopback(
            runs,
            packets,
            warmup,
            rate,
            msgs_per_packet,
            processing_sample_every,
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("caml-bench: error: {e}");
            ExitCode::FAILURE
        }
    }
}
