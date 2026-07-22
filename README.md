# micro-hermes

A Rust (Aya) reimplementation of **Hermes**, Alibaba's closed-source eBPF
Layer-7 load balancer, built for an MSc dissertation. Design writeup in
[CLAUDE.md](CLAUDE.md), paper in `hermes_paper.pdf`.

This repo now holds two things:

- **Phase 2 (repo root, this README)** — the real implementation: a real
  `#[sk_reuseport]` eBPF program attached via `SO_ATTACH_REUSEPORT_EBPF`,
  real `SO_REUSEPORT` sockets, a real `EPOLLEXCLUSIVE` baseline, and a real
  epoll event loop. **Linux only** — see [Requirements](#requirements)
  below. Start with **[OVERVIEW.md](OVERVIEW.md)** for the full technical
  map: workspace layout, what every crate does, how each dispatch policy
  actually runs on the kernel, and — importantly — a straight account of
  what's been verified vs. what still needs a Linux box to confirm.
- **[phase1/](phase1/)** — the original pure-userspace simulation (no real
  sockets, epoll, or eBPF), which produced the dissertation's Phase 1
  comparative benchmarks. Preserved as-is and still builds standalone:
  `cd phase1 && cargo build --release`. Its own [phase1/OVERVIEW.md](phase1/OVERVIEW.md)
  documents that version; [analysis/](analysis/) holds its notebook,
  figures, and result CSVs.

## Requirements

Phase 2 needs Linux (developed against current Ubuntu) plus the Aya
toolchain:

```bash
rustup install stable
rustup toolchain install nightly --component rust-src
cargo install cargo-binstall && cargo binstall bpf-linker
```

Full setup notes, first-build order, and what to check as you go are in
[OVERVIEW.md § Requirements & Ubuntu setup](OVERVIEW.md#requirements--ubuntu-setup)
and [OVERVIEW.md § Status](OVERVIEW.md#status-what-to-verify-first).

## Quick start

```bash
cargo build --release -p hermes -p hermes-bench

# terminal 1 — the load balancer (needs root for eBPF load/attach)
sudo -E target/release/hermes --policy hermes --port 7878

# terminal 2 — real TCP traffic against it
target/release/hermes-bench --port 7878 --case 2 --load heavy --out conns.csv
```

`--policy` is `hermes` | `reuseport` | `lifo`. `hermes-bench --case` selects
one of the paper's four traffic profiles plus a fifth (synchronized burst on
long-lived connections) — see [OVERVIEW.md](OVERVIEW.md) for what each case
tests. For the full benchmark matrix: [benchmark/run_all.sh](benchmark/run_all.sh).

## Layout

```
hermes-common/   shared types (wire protocol, map names, NUM_WORKERS)
hermes-ebpf/     the eBPF program (Algorithm 2 / Stage 3)
hermes/          loader + worker binary (Stages 1 and 2)
hermes-bench/    real-socket load generator
benchmark/       orchestration scripts + results
phase1/          the original userspace simulator (standalone)
analysis/        phase 1's notebook, figures, tables
```

See [OVERVIEW.md](OVERVIEW.md) for the deep dive on every file.
