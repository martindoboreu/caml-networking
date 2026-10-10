//! `caml-recv`: receive the synthetic feed and report integrity/latency.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use caml_engine::transport::{UdpConfig, UdpSource};
use caml_engine::{Delivery, EventSink, FeedProcessor, RunLimits, run};
use caml_protocol::MarketEvent;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Local address to bind.
    #[arg(long, default_value = "127.0.0.1:40000")]
    bind: SocketAddr,

    /// Requested SO_RCVBUF in bytes (0 = OS default).
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    rcvbuf: usize,

    /// Maximum time one receive call blocks, in milliseconds. Bounds shutdown
    /// latency.
    #[arg(long, default_value_t = 100)]
    poll_timeout_ms: u64,

    /// Stop after this many datagrams.
    #[arg(long)]
    max_datagrams: Option<u64>,

    /// Stop after this many seconds.
    #[arg(long)]
    duration_secs: Option<f64>,

    /// Seconds between periodic reports (0 disables them).
    #[arg(long, default_value_t = 1.0)]
    report_secs: f64,

    /// Print the first N delivered events.
    #[arg(long, default_value_t = 0)]
    print_events: u64,
}

/// Example downstream consumer: counts events and optionally prints a few.
/// Real consumers (book builders, strategies, recorders) plug in here.
struct ConsoleSink {
    print_remaining: u64,
    trades: u64,
    quotes: u64,
}

impl EventSink for ConsoleSink {
    fn on_event(&mut self, e: &MarketEvent, delivery: Delivery) {
        match e.msg_type {
            caml_protocol::MessageType::Trade => self.trades += 1,
            _ => self.quotes += 1,
        }
        if self.print_remaining > 0 {
            self.print_remaining -= 1;
            println!(
                "event seq={} {:?} instr={} px={} qty={} [{delivery:?}]",
                e.seq, e.msg_type, e.instrument_id, e.price, e.quantity
            );
        }
    }
}

fn secs(s: f64, name: &str) -> Result<Option<Duration>, String> {
    if s == 0.0 {
        Ok(None)
    } else {
        Duration::try_from_secs_f64(s)
            .map(Some)
            .map_err(|e| format!("--{name}: {e}"))
    }
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("caml-recv: error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<(), String> {
    let args = Args::parse();

    let config = UdpConfig {
        bind: args.bind,
        recv_buffer_bytes: (args.rcvbuf > 0).then_some(args.rcvbuf),
        poll_timeout: Duration::from_millis(args.poll_timeout_ms),
    };
    let mut source = UdpSource::bind(&config).map_err(|e| format!("bind {}: {e}", args.bind))?;

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc::set_handler(move || stop.store(true, Ordering::Relaxed))
            .map_err(|e| format!("installing Ctrl-C handler: {e}"))?;
    }

    let limits = RunLimits {
        max_datagrams: args.max_datagrams,
        max_duration: match args.duration_secs {
            Some(s) => secs(s, "duration-secs")?,
            None => None,
        },
        report_interval: secs(args.report_secs, "report-secs")?,
    };

    eprintln!(
        "caml-recv: listening on {} (SO_RCVBUF requested={} granted={}), Ctrl-C to stop",
        source.local_addr().map_err(|e| e.to_string())?,
        config
            .recv_buffer_bytes
            .map_or_else(|| "default".to_string(), |b| b.to_string()),
        source.granted_recv_buffer()
    );

    let mut processor = FeedProcessor::new(ConsoleSink {
        print_remaining: args.print_events,
        trades: 0,
        quotes: 0,
    });

    let start = Instant::now();
    let mut last = (start, 0u64, 0u64);
    let reason = run(&mut source, &mut processor, &stop, limits, |p| {
        let now = Instant::now();
        let c = p.snapshot().counters;
        let dt = now.duration_since(last.0).as_secs_f64();
        eprintln!(
            "[{:>7.1}s] {:>10.0} pkt/s {:>8.2} MB/s  total_pkts={} malformed={} msgs={}",
            now.duration_since(start).as_secs_f64(),
            (c.datagrams - last.1) as f64 / dt,
            (c.bytes - last.2) as f64 / dt / 1e6,
            c.datagrams,
            c.malformed,
            c.messages_delivered,
        );
        last = (now, c.datagrams, c.bytes);
    })
    .map_err(|e| format!("receive failed: {e}"))?;

    let elapsed = start.elapsed();
    let snapshot = processor.snapshot();
    let sink = processor.sink();
    println!(
        "caml-recv: stopped ({reason:?}) after {:.3}s; trades={} quotes={}",
        elapsed.as_secs_f64(),
        sink.trades,
        sink.quotes
    );
    print!("{snapshot}");
    if snapshot.packet_seq.outstanding > 0 || snapshot.counters.malformed > 0 {
        println!(
            "caml-recv: WARNING: data loss detected ({} packets / {} messages missing, {} malformed)",
            snapshot.packet_seq.outstanding,
            snapshot.message_seq.outstanding,
            snapshot.counters.malformed
        );
    }
    println!(
        "caml-recv: note: loss of the final packet(s) of a feed is undetectable without a later packet or heartbeat"
    );
    Ok(())
}
