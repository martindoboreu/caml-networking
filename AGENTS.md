# Rules for contributors and AI agents

## Hard constraints

* **Never connect to, configure, or deploy to the CAML server
  (`camlcompute`)** without explicit written authorization from the project
  lead. That includes SSH, Tailscale, Docker, and package installs.
* Never attach XDP/eBPF programs to a physical interface. Experiments use
  `veth` pairs or loopback only, and only after authorization.
* No system-wide installs, firewall changes, or Windows feature changes
  without approval.
* No live credentials, broker integrations, or order entry.

## Engineering rules

* Keep `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
  and `cargo fmt --all -- --check` passing on every commit.
* `unsafe` is forbidden workspace-wide. A future crate that needs it (AF_XDP)
  must opt out explicitly and document a safety invariant on every `unsafe`
  block.
* The receive path must not allocate, panic on input, or drop data without
  counting it.
* `crates/protocol` stays dependency-free and I/O-free. Any wire-format
  change must update `docs/protocol.md` and the golden fixture in
  `crates/protocol/tests/wire.rs` together.
* Transport code must not depend on protocol code, and vice versa;
  `FeedProcessor` is the only place they meet.
* Every performance claim needs a reproducible command, a release build, and
  the environment it ran in (loopback, veth, or physical NIC; OS; CPU). Never
  compare numbers across environments as if they were equivalent.
* When reporting status, label each item: implemented, tested, benchmarked,
  experimental, planned, or blocked.
