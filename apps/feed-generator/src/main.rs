//! `caml-feed`: publish a synthetic market-data feed over UDP.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::num::NonZeroU64;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use caml_protocol::MAX_PAYLOAD_1500_MTU;
use caml_sim::{FaultInjector, FaultPlan, FeedConfig, FeedSimulator};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Destination address.
    #[arg(long, default_value = "127.0.0.1:40000")]
    target: SocketAddr,

    /// Number of data packets to generate.
    #[arg(long, default_value_t = 100_000)]
    packets: u64,

    /// Target data packets per second (0 = as fast as possible).
    #[arg(long, default_value_t = 10_000.0)]
    rate: f64,

    /// Messages per packet (max 36 for a 1500-byte MTU).
    #[arg(long, default_value_t = 4)]
    msgs_per_packet: u16,

    /// Number of synthetic instruments.
    #[arg(long, default_value_t = 8)]
    instruments: u32,

    /// RNG seed for reproducible data.
    #[arg(long, default_value_t = 0xCA41)]
    seed: u64,

    /// Session id (default: derived from the current time, so each run is a
    /// new session).
    #[arg(long)]
    session_id: Option<u32>,

    /// Heartbeats sent after the last data packet so the receiver can detect
    /// loss of the final data packets.
    #[arg(long, default_value_t = 1)]
    trailing_heartbeats: u32,

    /// Drop every Nth data packet.
    #[arg(long)]
    drop_every: Option<NonZeroU64>,

    /// Corrupt every Nth data packet.
    #[arg(long)]
    corrupt_every: Option<NonZeroU64>,

    /// Send every Nth data packet twice.
    #[arg(long)]
    duplicate_every: Option<NonZeroU64>,

    /// Send every Nth data packet after its successor.
    #[arg(long)]
    reorder_every: Option<NonZeroU64>,
}

/// Must be the same clock the receiver uses (`caml_engine::clock`).
fn wall_clock_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Sleeps for most of the remaining time, then spins for the last stretch.
/// OS sleep granularity (often ~1 ms on Windows) is too coarse for pacing at
/// tens of thousands of packets per second.
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

#[derive(Default)]
struct SendStats {
    datagrams: u64,
    bytes: u64,
    refused: u64,
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("caml-feed: error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<(), String> {
    let args = Args::parse();
    if !(args.rate >= 0.0 && args.rate.is_finite()) {
        return Err("--rate must be a finite non-negative number".into());
    }

    let session_id = args
        .session_id
        .unwrap_or((wall_clock_ns() / 1_000_000) as u32);
    let mut sim = FeedSimulator::new(FeedConfig {
        session_id,
        instruments: args.instruments,
        msgs_per_packet: args.msgs_per_packet,
        max_payload: MAX_PAYLOAD_1500_MTU,
        seed: args.seed,
    })
    .map_err(|e| e.to_string())?;

    let plan = FaultPlan {
        drop_every: args.drop_every,
        corrupt_every: args.corrupt_every,
        duplicate_every: args.duplicate_every,
        reorder_every: args.reorder_every,
    };
    let mut injector = FaultInjector::new(plan, MAX_PAYLOAD_1500_MTU);

    // Loopback targets use a loopback source address so that no externally
    // reachable socket is opened (and no firewall prompt appears on Windows).
    let bind_ip: IpAddr = match (args.target.ip().is_loopback(), args.target.is_ipv4()) {
        (true, true) => Ipv4Addr::LOCALHOST.into(),
        (true, false) => Ipv6Addr::LOCALHOST.into(),
        (false, true) => Ipv4Addr::UNSPECIFIED.into(),
        (false, false) => Ipv6Addr::UNSPECIFIED.into(),
    };
    let bind = SocketAddr::new(bind_ip, 0);
    let socket = UdpSocket::bind(bind).map_err(|e| format!("bind {bind}: {e}"))?;
    // A connected UDP socket skips the per-send destination lookup and
    // reports ICMP "port unreachable" as ConnectionRefused on later sends.
    socket
        .connect(args.target)
        .map_err(|e| format!("connect {}: {e}", args.target))?;

    eprintln!(
        "caml-feed: session={session_id} target={} packets={} rate={} msgs/pkt={}{}",
        args.target,
        args.packets,
        if args.rate == 0.0 {
            "max".to_string()
        } else {
            format!("{}/s", args.rate)
        },
        args.msgs_per_packet,
        if plan.is_empty() {
            String::new()
        } else {
            format!(" faults={plan:?}")
        }
    );

    let mut stats = SendStats::default();
    let mut send = |p: &[u8]| -> io::Result<()> {
        match socket.send(p) {
            Ok(_) => {
                stats.datagrams += 1;
                stats.bytes += p.len() as u64;
                Ok(())
            }
            // No receiver yet (or it exited). Count and keep publishing, as a
            // multicast publisher would.
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                stats.refused += 1;
                Ok(())
            }
            Err(e) => Err(e),
        }
    };

    let mut buf = [0u8; MAX_PAYLOAD_1500_MTU];
    let start = Instant::now();
    for i in 1..=args.packets {
        if args.rate > 0.0 {
            wait_until(start + Duration::from_secs_f64((i - 1) as f64 / args.rate));
        }
        let len = sim
            .next_packet(&mut buf, wall_clock_ns())
            .map_err(|e| format!("encode: {e}"))?;
        injector
            .process(i, &mut buf[..len], &mut send)
            .map_err(|e| format!("send: {e}"))?;
    }
    injector
        .finish(&mut send)
        .map_err(|e| format!("send: {e}"))?;
    for _ in 0..args.trailing_heartbeats {
        let len = sim
            .next_heartbeat(&mut buf, wall_clock_ns())
            .map_err(|e| format!("encode: {e}"))?;
        send(&buf[..len]).map_err(|e| format!("send: {e}"))?;
    }
    let elapsed = start.elapsed().as_secs_f64();

    println!(
        "caml-feed: sent {} datagrams ({} data packets, {} messages, {} bytes) in {:.3}s = {:.0} pkt/s",
        stats.datagrams,
        args.packets,
        sim.next_msg_seq() - 1,
        stats.bytes,
        elapsed,
        stats.datagrams as f64 / elapsed
    );
    if !plan.is_empty() {
        println!("caml-feed: faults applied: {:?}", injector.counts());
    }
    if stats.refused > 0 {
        println!(
            "caml-feed: WARNING: {} sends refused (no receiver listening?)",
            stats.refused
        );
    }
    Ok(())
}
