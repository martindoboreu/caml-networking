//! `caml-feed`: publish a synthetic market-data feed over UDP, or write it to
//! a pcap file of Ethernet frames for replay through raw-packet backends.

use std::fs::File;
use std::io::{self, BufWriter};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::num::NonZeroU64;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use caml_protocol::{MAX_PAYLOAD_1500_MTU, patch_send_time};
use caml_sim::pcap::{PcapWriter, UdpFrameSpec, build_udp_frame};
use caml_sim::{FaultInjector, FaultPlan, FeedConfig, FeedSimulator};
use clap::Parser;
use socket2::{Domain, Protocol, Socket, Type};

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Destination address (unicast or IPv4 multicast group).
    #[arg(long, default_value = "127.0.0.1:40000")]
    target: SocketAddr,

    /// Number of data packets to generate.
    #[arg(long, default_value_t = 100_000)]
    packets: u64,

    /// Target data packets per second (0 = as fast as possible).
    #[arg(long, default_value_t = 10_000.0)]
    rate: f64,

    /// Messages per packet (1..=36 for a 1500-byte MTU).
    #[arg(long, default_value_t = 4)]
    msgs_per_packet: u16,

    /// Number of synthetic instruments.
    #[arg(long, default_value_t = 8)]
    instruments: u32,

    /// RNG seed for reproducible data.
    #[arg(long, default_value_t = 0xCA41)]
    seed: u64,

    /// Session epoch (default: current wall clock in ns, so each run is a
    /// newer session than the last).
    #[arg(long)]
    session_epoch: Option<NonZeroU64>,

    /// Send a heartbeat whenever no packet has been sent for this many
    /// milliseconds (0 disables). Must be below the receiver's
    /// --session-idle-ms.
    #[arg(long, default_value_t = 100)]
    heartbeat_ms: u64,

    /// Do not send the end-of-session heartbeat (simulates a crash).
    #[arg(long)]
    no_end_of_session: bool,

    /// Interface address for multicast transmission.
    #[arg(long, default_value = "127.0.0.1")]
    multicast_if: Ipv4Addr,

    /// Multicast TTL (1 = stay on the local subnet).
    #[arg(long, default_value_t = 1)]
    multicast_ttl: u32,

    /// Write Ethernet frames to this pcap file instead of sending.
    #[arg(long)]
    pcap_out: Option<String>,

    /// Source address written into pcap frames.
    #[arg(long, default_value = "192.0.2.1:50000")]
    pcap_src: SocketAddrV4,

    /// 802.1Q VLAN id for pcap frames.
    #[arg(long)]
    pcap_vlan: Option<u16>,

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
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

#[derive(Default)]
struct SendStats {
    datagrams: u64,
    bytes: u64,
    heartbeats: u64,
    refused: u64,
}

/// Where packets go: a UDP socket, or a pcap file with synthetic timestamps.
enum Output {
    Socket(UdpSocket),
    Pcap {
        writer: PcapWriter<BufWriter<File>>,
        spec: UdpFrameSpec,
        frame: Vec<u8>,
        /// Synthetic capture time of the next frame.
        ts_ns: u64,
    },
}

impl Output {
    fn send(&mut self, packet: &mut [u8], stats: &mut SendStats) -> io::Result<()> {
        match self {
            Self::Socket(socket) => {
                // Stamp as late as possible: immediately before the syscall.
                patch_send_time(packet, wall_clock_ns())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                match socket.send(packet) {
                    Ok(_) => {}
                    // No receiver yet (or it exited). Count and keep
                    // publishing, as a multicast publisher would.
                    Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                        stats.refused += 1;
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                }
            }
            Self::Pcap {
                writer,
                spec,
                frame,
                ts_ns,
            } => {
                patch_send_time(packet, *ts_ns)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                build_udp_frame(spec, packet, frame)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
                writer.write_frame(*ts_ns, frame)?;
            }
        }
        stats.datagrams += 1;
        stats.bytes += packet.len() as u64;
        Ok(())
    }

    fn is_pcap(&self) -> bool {
        matches!(self, Self::Pcap { .. })
    }

    fn set_time(&mut self, ns: u64) {
        if let Self::Pcap { ts_ns, .. } = self {
            *ts_ns = ns;
        }
    }
}

fn open_socket(args: &Args) -> Result<UdpSocket, String> {
    let target = args.target;
    // Loopback (and loopback-interface multicast) targets use a loopback
    // source address so that no externally reachable socket is opened and no
    // firewall prompt appears on Windows.
    let local_only = target.ip().is_loopback()
        || (target.ip().is_multicast() && args.multicast_if.is_loopback());
    let bind_ip: IpAddr = match (local_only, target.is_ipv4()) {
        (true, true) => Ipv4Addr::LOCALHOST.into(),
        (true, false) => Ipv6Addr::LOCALHOST.into(),
        (false, true) => Ipv4Addr::UNSPECIFIED.into(),
        (false, false) => Ipv6Addr::UNSPECIFIED.into(),
    };
    let bind = SocketAddr::new(bind_ip, 0);
    let socket = Socket::new(Domain::for_address(bind), Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| format!("socket: {e}"))?;
    socket
        .bind(&bind.into())
        .map_err(|e| format!("bind {bind}: {e}"))?;
    if let IpAddr::V4(group) = target.ip()
        && group.is_multicast()
    {
        socket
            .set_multicast_if_v4(&args.multicast_if)
            .map_err(|e| format!("multicast interface {}: {e}", args.multicast_if))?;
        socket
            .set_multicast_ttl_v4(args.multicast_ttl)
            .map_err(|e| format!("multicast ttl: {e}"))?;
        socket
            .set_multicast_loop_v4(true)
            .map_err(|e| format!("multicast loop: {e}"))?;
    }
    let socket: UdpSocket = socket.into();
    // A connected UDP socket skips the per-send destination lookup and
    // reports ICMP "port unreachable" as ConnectionRefused on later sends.
    socket
        .connect(target)
        .map_err(|e| format!("connect {target}: {e}"))?;
    Ok(socket)
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

    let start_wall = wall_clock_ns();
    let session_epoch = args
        .session_epoch
        .map_or(start_wall.max(1), NonZeroU64::get);
    let mut sim = FeedSimulator::new(FeedConfig {
        session_epoch,
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

    let mut out = match &args.pcap_out {
        Some(path) => {
            let SocketAddr::V4(dst) = args.target else {
                return Err("--pcap-out requires an IPv4 --target".into());
            };
            let file = File::create(path).map_err(|e| format!("create {path}: {e}"))?;
            let writer =
                PcapWriter::new(BufWriter::new(file)).map_err(|e| format!("write {path}: {e}"))?;
            let mut spec = UdpFrameSpec::new(args.pcap_src, dst);
            spec.vlan = args.pcap_vlan;
            Output::Pcap {
                writer,
                spec,
                frame: Vec::with_capacity(2048),
                ts_ns: start_wall,
            }
        }
        None => Output::Socket(open_socket(&args)?),
    };

    eprintln!(
        "caml-feed: session_epoch={session_epoch} {} packets={} rate={} msgs/pkt={}{}",
        match &args.pcap_out {
            Some(p) => format!("pcap={p} dst={}", args.target),
            None => format!("target={}", args.target),
        },
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

    let heartbeat =
        (args.heartbeat_ms > 0 && !out.is_pcap()).then(|| Duration::from_millis(args.heartbeat_ms));
    let mut stats = SendStats::default();
    let mut buf = [0u8; MAX_PAYLOAD_1500_MTU];
    let mut hb_buf = [0u8; MAX_PAYLOAD_1500_MTU];
    let start = Instant::now();
    let mut last_send = start;
    let gap_ns = if args.rate > 0.0 {
        1e9 / args.rate
    } else {
        1_000.0
    };

    for i in 1..=args.packets {
        let offset = Duration::from_secs_f64((i - 1) as f64 * gap_ns / 1e9);
        if out.is_pcap() {
            out.set_time(start_wall.saturating_add(offset.as_nanos() as u64));
        } else if args.rate > 0.0 {
            // Pace open-loop against an absolute schedule; heartbeat while
            // idle so receivers can detect loss of the last packets.
            let deadline = start + offset;
            loop {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                if let Some(hb) = heartbeat
                    && now.duration_since(last_send) >= hb
                {
                    let len = sim
                        .next_heartbeat(&mut hb_buf, wall_clock_ns())
                        .map_err(|e| format!("encode: {e}"))?;
                    out.send(&mut hb_buf[..len], &mut stats)
                        .map_err(|e| format!("send: {e}"))?;
                    stats.heartbeats += 1;
                    last_send = now;
                    continue;
                }
                let mut remaining = deadline - now;
                if let Some(hb) = heartbeat {
                    remaining = remaining.min(hb.saturating_sub(now.duration_since(last_send)));
                }
                if remaining > Duration::from_millis(2) {
                    std::thread::sleep(remaining - Duration::from_millis(2));
                } else {
                    std::hint::spin_loop();
                }
            }
        }
        let len = sim
            .next_packet(&mut buf, wall_clock_ns())
            .map_err(|e| format!("encode: {e}"))?;
        injector
            .process(i, &mut buf[..len], |p| out.send(p, &mut stats))
            .map_err(|e| format!("send: {e}"))?;
        last_send = Instant::now();
    }
    injector
        .finish(|p| out.send(p, &mut stats))
        .map_err(|e| format!("send: {e}"))?;
    if !args.no_end_of_session {
        let len = sim
            .end_of_session(&mut buf, wall_clock_ns())
            .map_err(|e| format!("encode: {e}"))?;
        out.send(&mut buf[..len], &mut stats)
            .map_err(|e| format!("send: {e}"))?;
        stats.heartbeats += 1;
    }
    if let Output::Pcap { writer, .. } = out {
        writer
            .into_inner()
            .map_err(|e| format!("flush pcap: {e}"))?;
    }
    let elapsed = start.elapsed().as_secs_f64();

    println!(
        "caml-feed: sent {} datagrams ({} data packets, {} messages, {} heartbeats incl. end-of-session, {} bytes) in {:.3}s = {:.0} pkt/s",
        stats.datagrams,
        sim.data_packets(),
        sim.next_seq() - 1,
        stats.heartbeats,
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
