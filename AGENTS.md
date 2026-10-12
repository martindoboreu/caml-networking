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

* Keep these passing on every commit (CI runs them on Ubuntu 24.04 and
  Windows):
  `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `cargo test --workspace --release`,
  `cargo +1.88 check --workspace --all-targets` (MSRV), and `cargo deny check`.
  Linux-only code must also pass clippy for `x86_64-unknown-linux-gnu`.
  On Linux or WSL2, `bash scripts/check.sh` runs them all. Line coverage of
  the library crates (`bash scripts/coverage.sh`) must not drop below the
  90% floor CI enforces; new code comes with tests.
* `unsafe` is forbidden workspace-wide. A crate that needs it must opt out
  explicitly in its own `[lints]` table (`unsafe_code = "deny"`, allowed only
  where needed), deny `clippy::undocumented_unsafe_blocks`, and document a
  safety invariant on every `unsafe` block. Current exception:
  `caml-alloc-count` (test-only). Planned: the AF_XDP backend crate. Prefer
  safe wrappers (`nix`, `socket2`) to raw FFI.
* The receive path must not allocate (enforced by
  `crates/engine/tests/alloc.rs`), panic on any input (enforced by the
  property and fuzz tests; wire arithmetic is checked, see
  `clippy::arithmetic_side_effects` in `sequence.rs` and `pipeline.rs`), or
  drop data without counting it.
* Any change to sequencing or session semantics adds or updates a test in
  `crates/engine/tests/adversarial.rs` or `properties.rs`, and an entry in
  `docs/decisions.md`.
* `crates/protocol` and `crates/frame` stay dependency-free and I/O-free. Any
  wire-format change must update `docs/protocol.md` and the golden fixtures in
  `crates/protocol/tests/wire.rs` together.
* Transport code must not depend on protocol code, and vice versa;
  `FeedProcessor` is the only place they meet.
* Every performance claim needs a reproducible command, a release build, and
  the environment it ran in (loopback, veth, or physical NIC; OS; CPU). Never
  compare numbers across environments as if they were equivalent. Use
  `caml-bench`, and record results in `docs/benchmarks.md`.
* When reporting status, label each item: implemented, tested, CI-only
  (tests exist but have only run on CI, or not yet), benchmarked,
  experimental, planned, or blocked.
