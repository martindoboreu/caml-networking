//! The AF_XDP receive path performs no heap allocations. Needs root (see
//! `veth.rs`); run with `scripts/xdp-veth.sh`.

#![cfg(all(target_os = "linux", feature = "xdp"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::hint::black_box;
use std::net::SocketAddrV4;

use caml_alloc_count::{CountingAllocator, count_allocations};
use common::{GROUP, PORT, Veth, drain};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

#[test]
#[ignore = "needs root and creates veth pairs: run scripts/xdp-veth.sh"]
fn poll_never_allocates() {
    let veth = Veth::new("cxdp9", 9);
    let mut src = veth.source(veth.filter());
    let tx = veth.sender();
    // Warm up: first frame through the path.
    tx.send_to(b"warm", SocketAddrV4::new(GROUP, PORT)).unwrap();
    assert_eq!(drain(&mut src, 1, |_, _| {}), 1, "{:?}", src.xdp_stats());

    for _ in 0..200 {
        tx.send_to(&[0xAB; 200], SocketAddrV4::new(GROUP, PORT))
            .unwrap();
    }
    let (got, allocations) = count_allocations(|| {
        drain(&mut src, 200, |b, m| {
            black_box((b, m));
        })
    });
    assert_eq!(got, 200, "{:?}", src.xdp_stats());
    assert_eq!(allocations, 0, "allocations on the AF_XDP receive path");
}
