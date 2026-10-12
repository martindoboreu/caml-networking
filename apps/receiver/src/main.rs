//! `caml-recv`: receive the CAML feed and report integrity and latency.
//!
//! Exit codes: 0 success, 1 error, 2 integrity failure (with `--fail-on-loss`).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use caml_engine::metrics::Snapshot;
use caml_engine::sequence::SeqConfig;
#[cfg(target_os = "linux")]
use caml_engine::transport::RecvMmsgSource;
use caml_engine::transport::{MulticastGroup, PacketSource, TransportStats, UdpConfig, UdpSource};
use caml_engine::{Delivery, EventSink, FeedProcessor, ProcessorConfig, RunLimits, run};
use caml_protocol::MarketEvent;
use clap::{Parser, ValueEnum};

/// Receive backend.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend {
    /// Portable: one `recv_from` per datagram (all platforms).
    Udp,
    /// Linux: batched `recvmmsg`, with kernel receive timestamps
    /// (`SO_TIMESTAMPNS`). Kernel drop counts are read on Linux for both
    /// backends.
    Recvmmsg,
}

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Receive backend.
    #[arg(long, value_enum, default_value_t = Backend::Udp)]
    backend: Backend,

    /// Local address to bind. For multicast on Linux, bind the group address
    /// (or 0.0.0.0) and the feed's port.
    #[arg(long, default_value = "127.0.0.1:40000")]
    bind: SocketAddr,

    /// Requested SO_RCVBUF in bytes (0 = OS default).
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    rcvbuf: usize,

    /// Maximum time one receive call blocks, in milliseconds. Bounds shutdown
    /// latency.
    #[arg(long, default_value_t = 100)]
    poll_timeout_ms: u64,

    /// IPv4 multicast group to join.
    #[arg(long)]
    group: Option<Ipv4Addr>,

    /// Interface address for the multicast join.
    #[arg(long, default_value = "127.0.0.1")]
    iface: Ipv4Addr,

    /// Set SO_REUSEADDR (lets several receivers share a multicast port).
    #[arg(long)]
    reuse_address: bool,

    /// Accept datagrams only from this source address (repeatable). Others
    /// are dropped and counted.
    #[arg(long = "allow-source")]
    allow_sources: Vec<IpAddr>,

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

    /// Exit with status 2 if any loss, rejection, or malformed datagram was
    /// detected.
    #[arg(long)]
    fail_on_loss: bool,

    /// Gap ranges remembered for late-arrival detection.
    #[arg(long, default_value_t = SeqConfig::default().max_tracked_gaps)]
    gap_capacity: usize,

    /// Largest sequence jump accepted without confirmation.
    #[arg(long, default_value_t = SeqConfig::default().max_forward_jump)]
    max_forward_jump: u64,

    /// Consecutive consistent packets needed to accept a larger jump.
    #[arg(long, default_value_t = SeqConfig::default().resync_after)]
    resync_after: u32,

    /// A first packet starting within this many messages of seq 1 means the
    /// receiver saw the session start; beyond it is a late join.
    #[arg(long, default_value_t = SeqConfig::default().join_window)]
    join_window: u64,

    /// Consecutive packets needed to adopt a different session epoch.
    #[arg(long, default_value_t = ProcessorConfig::default().session_confirm_packets)]
    session_confirm: u32,

    /// Silence (ms) after which a newer session is adopted immediately. Must
    /// exceed the publisher's heartbeat interval.
    #[arg(long, default_value_t = 500)]
    session_idle_ms: u64,

    /// Reject session epochs more than this many seconds ahead of our clock.
    #[arg(long, default_value_t = 300)]
    max_epoch_skew_secs: u64,

    /// Record processing latency for one packet in N (0 = off).
    #[arg(long, default_value_t = 1)]
    processing_sample_every: u32,
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
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(e) => {
            eprintln!("caml-recv: error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_transport(t: &TransportStats) {
    println!(
        "  transport   datagrams={} filtered_source={} truncated={} transient_errors={} kernel_drops={}",
        t.datagrams,
        t.filtered_source,
        t.truncated,
        t.transient_errors,
        t.kernel_drops
            .map_or_else(|| "n/a".to_string(), |d| d.to_string())
    );
}

/// Prints the verdict; returns whether the run was clean.
fn print_verdict(snapshot: &Snapshot, transport: &TransportStats) -> bool {
    let v = snapshot.integrity();
    let kernel_or_truncated = transport
        .truncated
        .saturating_add(transport.kernel_drops.unwrap_or(0));
    let clean = v.is_clean() && kernel_or_truncated == 0;
    println!(
        "caml-recv: verdict: {}",
        if clean {
            "CLEAN (no loss, rejection, or malformed data detected)"
        } else {
            "PROBLEMS DETECTED"
        }
    );
    println!(
        "  lost_messages={} malformed={} implausible_pkts={} rejected_session_pkts={} before_baseline={} kernel_or_truncated={}",
        v.lost_messages,
        v.malformed,
        v.implausible_packets,
        v.rejected_session_packets,
        v.before_baseline,
        kernel_or_truncated
    );
    if v.missed_before_join > 0 {
        println!(
            "  note: joined late; {} messages were sent before this receiver started",
            v.missed_before_join
        );
    }
    if transport.filtered_source > 0 {
        println!(
            "  note: {} datagrams from non-allowed sources were dropped",
            transport.filtered_source
        );
    }
    if v.unterminated_sessions > 0 {
        println!(
            "  WARNING: {} earlier session(s) were replaced without an end-of-session heartbeat (publisher crash or restart); loss of their final packets is undetectable",
            v.unterminated_sessions
        );
    }
    if snapshot.session.is_some() && !v.session_ended {
        println!(
            "  note: no end-of-session received; loss after the last heartbeat is undetectable"
        );
    }
    if transport.kernel_drops.is_none() {
        println!(
            "  note: kernel socket-buffer drops are not observable on this platform; they appear as sequence gaps"
        );
    }
    clean
}

fn real_main() -> Result<bool, String> {
    let args = Args::parse();

    let config = UdpConfig {
        bind: args.bind,
        recv_buffer_bytes: (args.rcvbuf > 0).then_some(args.rcvbuf),
        poll_timeout: Duration::from_millis(args.poll_timeout_ms),
        multicast: args.group.map(|group| MulticastGroup {
            group,
            interface: args.iface,
        }),
        allowed_sources: args.allow_sources.clone(),
        reuse_address: args.reuse_address,
    };
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

    let processor_config = ProcessorConfig {
        seq: SeqConfig {
            max_tracked_gaps: args.gap_capacity,
            max_forward_jump: args.max_forward_jump,
            resync_after: args.resync_after,
            join_window: args.join_window,
        },
        session_confirm_packets: args.session_confirm,
        session_idle_switch: Duration::from_millis(args.session_idle_ms),
        max_epoch_future_skew: Duration::from_secs(args.max_epoch_skew_secs),
        processing_sample_every: args.processing_sample_every,
    };

    let run_args = RunArgs {
        args: &args,
        config: &config,
        limits,
        processor_config,
        stop: &stop,
    };
    let bind_err = |e: std::io::Error| format!("bind {}: {e}", args.bind);
    match args.backend {
        Backend::Udp => {
            let source = UdpSource::bind(&config).map_err(bind_err)?;
            let local = source.local_addr().map_err(|e| e.to_string())?;
            let granted = source.granted_recv_buffer();
            receive(source, local, granted, &run_args)
        }
        #[cfg(target_os = "linux")]
        Backend::Recvmmsg => {
            let source = RecvMmsgSource::bind(&config).map_err(bind_err)?;
            let local = source.local_addr().map_err(|e| e.to_string())?;
            let granted = source.granted_recv_buffer();
            receive(source, local, granted, &run_args)
        }
        #[cfg(not(target_os = "linux"))]
        Backend::Recvmmsg => Err("--backend recvmmsg requires Linux".into()),
    }
}

/// Everything the receive loop needs besides the source.
struct RunArgs<'a> {
    args: &'a Args,
    config: &'a UdpConfig,
    limits: RunLimits,
    processor_config: ProcessorConfig,
    stop: &'a AtomicBool,
}

/// Runs the receive loop on any backend and prints the final report.
fn receive<S: PacketSource>(
    mut source: S,
    local: SocketAddr,
    granted: usize,
    r: &RunArgs<'_>,
) -> Result<bool, String> {
    let (args, config, limits, processor_config, stop) =
        (r.args, r.config, r.limits, r.processor_config, r.stop);
    eprintln!(
        "caml-recv: backend={:?} listening on {}{} (SO_RCVBUF requested={} granted={}), Ctrl-C to stop",
        args.backend,
        local,
        args.group
            .map_or_else(String::new, |g| format!(" group={g} iface={}", args.iface)),
        config
            .recv_buffer_bytes
            .map_or_else(|| "default".to_string(), |b| b.to_string()),
        granted
    );

    let mut processor = FeedProcessor::with_config(
        ConsoleSink {
            print_remaining: args.print_events,
            trades: 0,
            quotes: 0,
        },
        processor_config,
    );

    let start = Instant::now();
    let mut last = (start, 0u64, 0u64);
    let reason = run(&mut source, &mut processor, stop, limits, |p| {
        let now = Instant::now();
        let c = p.counters();
        let dt = now.duration_since(last.0).as_secs_f64();
        eprintln!(
            "[{:>7.1}s] {:>10.0} pkt/s {:>8.2} MB/s  total_pkts={} malformed={} msgs={} lost={}",
            now.duration_since(start).as_secs_f64(),
            (c.datagrams - last.1) as f64 / dt,
            (c.bytes - last.2) as f64 / dt / 1e6,
            c.datagrams,
            c.malformed,
            c.messages_delivered,
            p.seq_stats().lost(),
        );
        last = (now, c.datagrams, c.bytes);
    })
    .map_err(|e| format!("receive failed: {e}"))?;

    let elapsed = start.elapsed();
    let snapshot = processor.snapshot();
    let transport = source.stats();
    let sink = processor.sink();
    println!(
        "caml-recv: stopped ({reason:?}) after {:.3}s; trades={} quotes={}",
        elapsed.as_secs_f64(),
        sink.trades,
        sink.quotes
    );
    print_transport(&transport);
    print!("{snapshot}");
    let clean = print_verdict(&snapshot, &transport);
    Ok(clean || !args.fail_on_loss)
}
