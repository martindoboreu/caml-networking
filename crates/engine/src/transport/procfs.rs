//! Kernel drop counter of a UDP socket, read from `/proc/net/udp{,6}`.
//!
//! Every UDP socket line ends with `drops`, the kernel's `sk_drops` counter:
//! datagrams the kernel discarded for this socket, mostly because the
//! receive buffer was full. Reading it costs a file read and allocates, so
//! it runs only from [`PacketSource::stats`](super::PacketSource::stats),
//! which is a reporting path, never from `poll`.
//!
//! Why not `SO_RXQ_OVFL`: that option delivers the counter as a control
//! message on each received datagram, but `nix` 0.31's `recvmmsg` reuses
//! its message headers without restoring `msg_controllen`, which the kernel
//! shrinks after every call. The drop counter's control message is then
//! truncated (`MSG_CTRUNC`) and lost. Found by the first Linux CI run.

use std::os::fd::RawFd;

/// Line format (`net/ipv4/udp.c`, `udp4_format_sock`; IPv6 is the same
/// shape): `sl local rem st tx:rx tr:when retrnsmt uid timeout inode ref
/// pointer drops`.
const INODE_COLUMN: usize = 9;
const DROPS_AFTER_INODE: usize = 2; // ref, pointer, then drops

/// Socket inode of file descriptor `fd`, from `/proc/self/fd/<fd>` →
/// `socket:[<inode>]`.
fn socket_inode(fd: RawFd) -> Option<u64> {
    let link = std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()?;
    link.to_str()?
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// Finds the `drops` value for socket `inode` in one `/proc/net/udp*` table.
fn drops_in_table(table: &str, inode: u64) -> Option<u64> {
    table.lines().skip(1).find_map(|line| {
        let mut cols = line.split_whitespace();
        let ino: u64 = cols.nth(INODE_COLUMN)?.parse().ok()?;
        if ino != inode {
            return None;
        }
        cols.nth(DROPS_AFTER_INODE)?.parse().ok()
    })
}

/// Kernel drop count of the UDP socket `fd`, or `None` if it cannot be read.
pub(crate) fn socket_drops(fd: RawFd) -> Option<u64> {
    let inode = socket_inode(fd)?;
    ["/proc/net/udp", "/proc/net/udp6"].iter().find_map(|path| {
        let table = std::fs::read_to_string(path).ok()?;
        drops_in_table(&table, inode)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `/proc/net/udp` lines (header, then two sockets).
    const TABLE: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  123: 0100007F:9C40 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 4242 2 0000000000000000 0
  456: 0100007F:9C41 00000000:0000 07 00000000:00001200 00:00000000 00000000  1000        0 777 2 0000000000000000 1234
";

    #[test]
    fn finds_drops_by_inode() {
        assert_eq!(drops_in_table(TABLE, 4242), Some(0));
        assert_eq!(drops_in_table(TABLE, 777), Some(1234));
        assert_eq!(drops_in_table(TABLE, 1), None);
        assert_eq!(drops_in_table("", 777), None);
        assert_eq!(drops_in_table("header\n garbage line\n", 777), None);
    }

    #[test]
    fn reads_a_live_socket() {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        use std::os::fd::AsRawFd;
        assert!(socket_inode(s.as_raw_fd()).is_some());
        assert_eq!(socket_drops(s.as_raw_fd()), Some(0));
    }
}
