# micro-hermes — project overview (Phase 2)

A single-page map of the real eBPF implementation: the workspace layout, what
every crate/file does, how the three dispatch policies actually run on the
kernel now, the wire protocol, the benchmark harness, and an honest account
of what's built vs. what still needs your hands (and a Linux box) to finish.
Written to be read top-to-bottom in one sitting.

> **What this project is.** A Rust (Aya) reimplementation of Hermes,
> Alibaba's closed-source eBPF Layer-7 load balancer (design writeup in
> [CLAUDE.md](CLAUDE.md), paper in `hermes_paper.pdf`). **Phase 1**
> (preserved in [phase1/](phase1/), still builds and runs standalone)
> simulated the entire 3-stage closed feedback loop in pure userspace — one
> process, `fork()`, shared memory, no real sockets or eBPF — to validate the
> scheduling algorithm and produce the dissertation's comparative benchmarks.
> **Phase 2 (this document, the repo root)** replaces every simulated
> kernel-facing piece with the real thing: a real `#[sk_reuseport]` eBPF
> program attached via `SO_ATTACH_REUSEPORT_EBPF`, real `SO_REUSEPORT`
> sockets, a real `EPOLLEXCLUSIVE` baseline, and a real epoll event loop. The
> two things that were always meant to survive unchanged — the Worker Status
> Table and Algorithm 1 (the cascading filter) — do, byte-for-byte in spirit.

**This code only builds and runs on Linux**, and only the eBPF-touching
crate (`hermes`) needs that; see [Requirements](#requirements--ubuntu-setup)
before trying to build anything. Everything in this document describes code
that has been written and reviewed against the real Aya API surface
(verified against `aya-rs/aya`'s source on GitHub, not from memory) but
**has not been compiled or run** — I don't have a Linux machine with a BPF
toolchain in this environment. See [Status](#status-what-to-verify-first) for
exactly what that means for you.

---

## The one idea to hold in your head

Same closed feedback loop as Phase 1, but every arrow is now a real kernel
mechanism instead of a shared-memory stand-in:

```
 hermes (loader, then N forked workers)              real Linux kernel
 ┌────────────────────────────────┐
 │ loader (parent, pre-fork):      │
 │  · builds the socket topology   │     SO_REUSEPORT sockets / one
 │    for the chosen policy        │◀──▶ shared listener (policy-dependent)
 │  · [hermes only] loads+attaches │
 │    hermes-ebpf, populates       │◀──▶ SK_REUSEPORT program, attached via
 │    M_socket, pins M_Sel/M_socket│     SO_ATTACH_REUSEPORT_EBPF
 └──────────────┬───────────────────┘
                │ fork(): each child inherits the whole fd table
 ┌──────────────▼───────────────────────────────────────────────┐
 │ worker i (real process, real epoll loop):                    │
 │  · epoll_wait on its own listener (or the shared one, lifo)  │
 │  · accept4 / read / write real TCP, parse the wire protocol  │
 │  · stamp WST (shared mmap, unchanged from phase 1)            │
 │  · [hermes only] Algorithm 1 → write bitmap into pinned M_Sel│
 └────────────────────────────────────────────────────────────────┘
        new connection arrives ──▶ kernel picks a worker:
          hermes:    eBPF program reads M_Sel, Algorithm 2, overrides pick
          reuseport: kernel's default stateless 4-tuple hash, no eBPF at all
          lifo:      kernel's real EPOLLEXCLUSIVE wait-queue wakeup

 hermes-bench (separate process, real TCP client) ──▶ hermes-common wire protocol ──▶ worker i
```

The feedback loop is still real end-to-end, just one layer further down:
Algorithm 1 (userspace, per worker) still writes the exact same bitmap it did
in Phase 1; that bitmap now lives in a real `BPF_MAP_TYPE_ARRAY` instead of
an `AtomicU64`, and it's a real eBPF program — not simulator code — that
reads it back on the kernel's own connection-dispatch path.

---

## Workspace layout

Phase 2 is a Cargo workspace at the repo root, following the current
[`aya-template`](https://github.com/aya-rs/aya-template) conventions (I
pulled the template's actual current files from GitHub rather than working
from memory, since Aya's build story — `aya-build` compiling the eBPF crate
via `build.rs`, no separate `xtask` needed anymore — has changed over time):

```
micro_hermes/
├── Cargo.toml              workspace root (hermes-ebpf excluded from
│                            default-members: needs nightly + bpf-linker)
├── hermes-common/           #![no_std] shared types — the only crate every
│                            other crate (including the eBPF one) depends on
├── hermes-ebpf/              the real eBPF program (Algorithm 2 / Stage 3)
├── hermes/                   the loader + worker binary (Stages 1 and 2,
│                            plus kernel-facing setup)
├── hermes-bench/             real-socket load generator (replaces
│                            phase1/src/generator.rs)
├── benchmark/                orchestration scripts + results (replaces
│                            analysis/run_benchmarks.sh for phase 2)
├── phase1/                   the untouched Phase 1 simulator, still builds
│                            standalone: `cd phase1 && cargo build`
└── analysis/                 Phase 1's notebook, figures, and result CSVs —
                             historical record backing the dissertation's
                             already-written Phase 1 section. NOT yet
                             updated to read benchmark/results/ — see Status.
```

**Why four small crates instead of one.** This is the standard Aya shape,
not extra ceremony: eBPF programs compile to a completely different target
(`bpfel-unknown-none`, no `std`, no allocator) than the userspace loader, so
they *must* be separate crates. `hermes-common` exists so both sides agree on
map names, `NUM_WORKERS`, and the client↔worker wire format without
duplicating constants. `hermes-bench` is separate from `hermes` because nothing
about a load-testing client needs `aya` as a dependency, and keeping it
separate means you can build and run the benchmark client on any machine
that can reach the LB over TCP — including this Mac, right now:
`cargo build -p hermes-bench` (see [Status](#status-what-to-verify-first)).

---

## `hermes-common` — shared types

The only crate every other crate depends on (`#![no_std]`, so it costs
nothing to pull into the eBPF program).

- `NUM_WORKERS: usize = 4` — one per simulated core, same as Phase 1. Change
  this and rebuild everything if you want to test with a different core
  count; it's a compile-time constant on both the eBPF and userspace sides
  so `M_socket`'s size and the WST's column count always agree.
- `DEFAULT_PORT`, `BPFFS_DIR`, `PIN_PROGRAM`, `PIN_M_SEL`, `PIN_M_SOCKET` —
  the pin path (`/sys/fs/bpf/hermes/...`) and map/program names, shared so
  `hermes-ebpf`'s `#[map(name = "m_sel")]` and `hermes`'s
  `MapData::from_pin(...)` can never drift apart.
- `encode_request` / `decode_request` / `encode_response` / `decode_response`
  — the wire protocol between `hermes-bench` and a worker. Deliberately tiny
  (24-byte request, 16-byte response, hand-rolled little-endian encoding, no
  serialization framework): this project validates *dispatch*, not a
  protocol stack. A request carries `seq`, `service_us` (the per-connection
  L7 cost the generator sampled — see the Phase-1→2 fidelity note below),
  and the client's own send timestamp; the response echoes `seq` and
  `send_ns` back and adds `worker_id`, so the client can compute true
  end-to-end latency and identify which worker served it, purely from its
  own clock (see `hermes-bench/src/client.rs`'s `now_ns` doc comment for why
  no clock sync between client and server is needed).

---

## `hermes-ebpf` — Stage 3, the real eBPF program

[hermes-ebpf/src/main.rs](hermes-ebpf/src/main.rs) — a direct, mechanical
port of Phase 1's `dispatch_hermes` / `reciprocal_scale` / `find_nth_set_bit`
(`phase1/src/dispatcher.rs`), which were deliberately written heap-free and
loop-bounded specifically so this port would be mechanical. It is:

- **A `#[sk_reuseport]` program** (`BPF_PROG_TYPE_SK_REUSEPORT`), the eBPF
  program type built for exactly this: overriding `SO_REUSEPORT`'s socket
  selection. Attached to the worker group's sockets via
  `SO_ATTACH_REUSEPORT_EBPF` (`hermes/src/loader.rs`).
- **Two maps**: `m_sel` (`BPF_MAP_TYPE_ARRAY<u64>`, one element — the
  candidate bitmap Algorithm 1 computes) and `m_socket`
  (`BPF_MAP_TYPE_REUSEPORT_SOCKARRAY` — worker ID → socket, populated once
  by the loader before any worker forks). Both declared `.pinned(...)` so the
  loader's `default_map_pin_directory` pins them under `/sys/fs/bpf/hermes/`
  — see below for why that matters.
- **Algorithm 2, verbatim**: read the bitmap, `count_ones()`, if ≤ 1 return
  `SK_PASS` (see below), else `reciprocal_scale` the connection's 4-tuple
  hash (`ctx.hash()` — the kernel computes this from the real SYN, nothing
  synthetic) into `[0, n)`, find the Nth set bit, `select_reuseport` that
  worker's socket.
- **The paper's fallback rule falls out for free.** §5.4 says: if the
  userspace scheduler couldn't confidently narrow the candidate set to more
  than one worker, fall back to the kernel's default reuseport hash. In
  Phase 1 that meant literally calling the hash function as a fallback path.
  In Phase 2, `SK_PASS` *is* "the kernel runs its default selection" — for
  an `SK_REUSEPORT` program, returning `SK_PASS` without calling
  `select_reuseport` means exactly that. So the eBPF program never
  reimplements the fallback hash at all; it just doesn't call the override
  helper.
- **`NUM_WORKERS` (4) is small enough that the Nth-set-bit loop needs no
  verifier gymnastics** — LLVM fully unrolls a 4-iteration loop over a
  compile-time constant bound, so there's no `bpf_loop()` helper or manual
  bound-proof needed, unlike a loop over a runtime-sized collection.

**What I did not add**: `aya-log-ebpf` instrumentation. The dispatch logic
is small enough to verify externally (via which `worker_id` shows up in
`hermes-bench`'s conns CSV, and the WST tick CSVs), and wiring up
`aya-log`'s live log-draining loop cleanly without `tokio` (which the
`hermes` binary deliberately avoids — see below) adds real complexity for
debugging value you may not need. If you do want it while debugging on the
real box, `hermes-ebpf/src/main.rs` is exactly where `debug!(&ctx, ...)`
calls would go; you'd add `aya-log-ebpf` to `hermes-ebpf/Cargo.toml` and
`aya-log` + a log-draining loop to `hermes`'s loader.

---

## `hermes` — Stages 1 and 2, the loader and the real worker

### [hermes/src/loader.rs](hermes/src/loader.rs) — kernel-facing setup

The real counterpart of Phase 1's `dispatcher.rs`. Phase 1's `Policy` enum
picked between three *simulated* mechanisms; in Phase 2 only `hermes` is
code this project writes at all — `reuseport` and `lifo` are the kernel's
own mechanisms, selected purely by which **socket topology** the loader
hands to the workers, built once in the parent, before `fork()`:

| Policy | Topology | Why |
|---|---|---|
| `hermes` / `reuseport` | N distinct sockets, each `SO_REUSEPORT`, same port | Each worker owns one column of `M_socket`. `SO_REUSEPORT` must be set **before** `bind(2)` — the kernel only admits a socket into the group at bind time, and setting it after is silently ignored. This is why `std::net::TcpListener` can't be used here (no hook between `socket()` and `bind()`); `loader.rs` builds sockets with raw `libc` calls instead. |
| `lifo` | **One** socket, no `SO_REUSEPORT` | Every worker adds it to its own epoll instance with `EPOLLEXCLUSIVE`. This *is* the real mechanism the paper's baseline describes (§2.2) — the kernel's wait-queue is genuinely shared and genuinely LIFO (insertion at the head, wakeup stops at the first idle waiter). Nothing simulates that; it falls out of the kernel doing what it always does. |

Sockets are created in the parent and inherited via `fork()` — worker *i*
already has `listen_fds[i]` open in its own process the moment it's forked,
no fd-passing required (POSIX fork semantics: each child gets its own
independent fd-table entry referencing the same underlying open file
description; closing in one process doesn't affect another's copy).

For `hermes`, `load_and_attach_ebpf()`:
1. Bumps `RLIMIT_MEMLOCK` (pre-5.11-ish kernels charge eBPF map memory
   against it).
2. `EbpfLoader::new().default_map_pin_directory(BPFFS_DIR).load(...)` —
   loads the compiled object (embedded at build time via
   `aya::include_bytes_aligned!`, produced by `hermes/build.rs` calling
   `aya_build::build_ebpf`), pinning `m_sel`/`m_socket` under
   `/sys/fs/bpf/hermes/` because they're declared `.pinned(...)` on the eBPF
   side.
3. Populates `M_socket[i] = listen_fds[i]` for every worker.
4. Loads and attaches the `SK_REUSEPORT` program via any one socket in the
   group (attaching through one socket attaches the whole group).
5. Drops its `aya::Ebpf` handle. This is safe and deliberate: the program
   stays attached as long as the group's sockets exist (owned by the
   forked workers), and the maps stay reachable because they're **pinned**,
   not because this handle is alive — which is exactly why each worker can
   reopen `M_Sel` fresh after `fork()` with `MapData::from_pin` +
   `Map::from_map_data` + `Array::try_from`, no fd-passing from the loader
   needed (see `worker.rs`).

### [hermes/src/worker.rs](hermes/src/worker.rs) — Stage 1, the real event loop

One forked process per worker, real `epoll`/`accept4`/`read`/`write` on real
TCP sockets, raw `libc` calls (no `mio`/`tokio` — see the design note
below). The Fig. 9 mapping from Phase 1 carries over hook-for-hook, just
with real syscalls behind each hook:

| Fig. 9 line | Phase 1 | Phase 2 |
|---|---|---|
| 12: `shm_avail_update` | stamp `last_loop_entry` | same — WST is unchanged shared memory |
| 13: `epoll_wait(timer=5ms)` | poll a simulated queue | real `epoll_wait`, 5 ms timeout, **edge-triggered** (`EPOLLET`) |
| 14: `shm_busy_count(+N)` | `pending_events += batch.len()` | `+= n` where `n` is the number of ready **fds** `epoll_wait` returned — matches Fig. 9's granularity exactly (one list entry per ready fd, not per accept/request that fd yields once drained) |
| 25: accept + `conn_count(+1)` | pop `ConnDesc` | `accept4()` drained to `EAGAIN` (ET semantics: a burst arriving in one wakeup would be missed otherwise) |
| handler | `sleep(service_us)` | parse the wire request, `sleep(service_us)` — see the fidelity note below |
| 18: `shm_busy_count(-1)` | per handled event | per fully-handled fd (after all its pending accepts/requests are drained) |
| 37: close + `conn_count(-1)` | lifetime expiry | real EOF / `EPOLLHUP` / `EPOLLERR` / bad-magic → `close()` |
| 20: `schedule_and_sync()` | write `AtomicU64` | Algorithm 1 (unchanged) → `aya::maps::Array::set(0, bitmap, 0)` on the reopened pinned `M_Sel` |

Hang injection (Case 2) carries over in spirit but changed shape: Phase 1's
generator could reach into a worker directly (same process, forked). Phase 2's
load generator is a separate process talking only over TCP, so it has no way
to make a specific worker stall. Instead, hang injection is a startup-time
property of the LB itself: `HERMES_HANG_INJECT=<worker_id>:<at_ms>:<duration_ms>`
on the `hermes` process, parsed in `main.rs` and handed to the matching
worker at fork — `benchmark/run_case.sh` sets this automatically for Case 2.

**Design note: no `tokio`/`mio` in this crate, unlike `hermes-bench`.** The
worker loop is a tight, synchronous, one-thread-per-process epoll loop —
exactly Fig. 9's shape — and `fork()` after any async runtime has spun up
background threads is unsafe (only the calling thread survives into the
child; if another thread held a lock at fork time, it stays locked forever
in the child). Keeping `hermes` single-threaded until after every `fork()`
sidesteps that class of bug entirely, and a raw epoll loop is simple enough
that `tokio` would add ceremony without buying anything here.

**The one deliberately-simplified corner of the real I/O path**: response
writes use a short retry-on-`EAGAIN` spin rather than arming `EPOLLOUT` and
finishing asynchronously. For a 16-byte response over localhost this is
never actually exercised (the send buffer is never full), but it's flagged
in the code (`write_all` in `worker.rs`) as the one place a fully
production-grade implementation would need more machinery.

### [hermes/src/wst.rs](hermes/src/wst.rs) and [hermes/src/scheduler.rs](hermes/src/scheduler.rs)

**Unchanged from Phase 1**, ported with only doc-comment updates. This was
always the point: the WST (`mmap(MAP_SHARED | MAP_ANONYMOUS)`, one 64-byte
cache-line-padded slot per worker, three `AtomicI64` metrics, per-worker
write ownership, lock-free scheduler reads) and Algorithm 1 (Time → Conn →
Event cascading filter, `θ = max(0.5·avg, 1.0)`, `HANG_THRESHOLD_NS = 200ms`)
were never simulation code — they were always meant to be exactly this in
the real system too. `scheduler.rs`'s unit tests (hang detection, cold
start, each filter stage, θ behavior) carry over unchanged and still pass
against plain `cargo test -p hermes-common` reasoning... actually they run
as part of `cargo test -p hermes`, which needs Linux — see Status.

### [hermes/src/metrics.rs](hermes/src/metrics.rs) — tick CSV

Same per-iteration tick schema as Phase 1 (WST snapshot + Algorithm-1 stage
survivors + bitmap), but **streamed**, not buffered: Phase 1's worker ran
for a fixed benchmark duration and wrote its shard once at exit; this
worker is a long-running server, so `TickWriter` opens its file up front and
flushes every 64 rows. Connection *latency* is no longer measured here at
all — see the next section.

### [hermes/src/main.rs](hermes/src/main.rs) — CLI + fork orchestration

`clap`-based CLI (`--policy`, `--port`, `--metrics-dir`, `--verbose`).
Registers a `signal_hook::flag`-based shutdown flag for `SIGINT`/`SIGTERM`
**before** `fork()` — deliberately: `signal_hook::flag::register` installs a
raw `sigaction` handler with no background thread, and both the signal
disposition and the `Arc<AtomicBool>`'s backing memory are inherited by
`fork()` (COW), so each forked child ends up with its own independently
-flippable copy of the flag at no extra registration cost, and a signal
delivered to any one process only ever flips that process's own copy (see
the long comment in `main.rs` for the full reasoning — this is exactly the
kind of thing worth getting right rather than reaching for a busier
primitive). On shutdown: forwards `SIGTERM` to every child explicitly (in
case the signal reached only the parent), waits for them, and — for the
`hermes` policy — removes the bpffs pin directory so a re-run doesn't see
stale state.

---

## `hermes-bench` — the real load generator

Replaces Phase 1's in-process `generator.rs` entirely; **this is the "total
rework" the benchmarking needed**, per your instruction. Talks real TCP to
whatever's listening on `--port` — it has no idea which policy is running on
the other end, which is the point: dispatch policy only affects *which*
`worker_id` ends up serving each connection, visible in the response.

- [hermes-bench/src/workload.rs](hermes-bench/src/workload.rs) — the same
  five traffic profiles as Phase 1's `workload.rs` (`WorkloadConfig`,
  `ProcessingTime::{Fixed,Bimodal}`, the Table-3 CPS sweep), numbers
  unchanged since they describe the *experiment*, not the simulation.
  `HangSpec` moved to the `hermes` crate (see above); `lifetime` now bounds
  how long **this client** keeps a connection open rather than a worker-side
  close timer, capped at `min(lifetime, duration + 500ms)` so Case 3/5's
  60-second "long-lived connection" profile doesn't make an automated
  benchmark run hang for 60 seconds.
- [hermes-bench/src/client.rs](hermes-bench/src/client.rs) — one `tokio`
  task per connection: connect, send the initial request, time the round
  trip from the client's own clock, then (Case 5 only) `tokio::select!`
  between its own close deadline and the synchronized burst deadline,
  firing exactly one follow-up request if the burst wins. This is a more
  realistic operationalization of Case 5 than Phase 1's could be: the burst
  is now genuinely "every client that still has a connection open fires a
  real request over it at the same instant," not a worker-internal event
  queue.
- [hermes-bench/src/main.rs](hermes-bench/src/main.rs) — absolute-deadline
  CPS pacing (unchanged technique from Phase 1's generator), spawns a
  detached task per connection, and a dedicated writer task streams rows to
  the conns CSV and accumulates a final latency summary (mean/p50/p99/max),
  printed on exit.

**This crate builds and runs on macOS right now** (verified: `cargo build -p
hermes-bench` and `cargo test -p hermes-common -p hermes-bench` both pass
clean on this machine) — you can point it at any TCP echo-ish service that
speaks the wire protocol, including a `hermes` LB once you've built that on
Ubuntu, or a small stub server for local iteration.

---

## Wire protocol, and where the phase-1→2 boundary is drawn

The worker's request handler still does `sleep(service_us)` to simulate L7
processing cost, exactly like Phase 1 — **this project does not implement a
real SSL/compression/routing stack**, and that's a deliberate scope
decision, not a shortcut I forgot to fix. What's real in Phase 2 is the
*dispatch mechanism* (real kernel, real sockets, real eBPF) — that's what
Hermes's contribution actually is; a real HTTP/TLS stack underneath it would
be a second, unrelated dissertation. The wire protocol
(`hermes-common::{encode,decode}_{request,response}`) exists specifically to
carry the same controlled, reproducible per-connection cost model Phase 1
used across a real socket, so the five traffic cases stay exactly comparable
between phases. If you do want real workload variability later, the request
header has an unused byte of headroom and the natural extension point is
obvious: add a payload after the header and let the worker's cost depend on
what it actually did with it.

---

## `benchmark/` — orchestration

Phase 1's `analysis/run_benchmarks.sh` ran the whole matrix in-process
(`fork()`, no separate binaries to coordinate). Phase 2 has two real,
independent processes — the LB needs root for eBPF, the client doesn't — so
orchestration is a shell layer:

- [benchmark/run_case.sh](benchmark/run_case.sh) — one (policy, case, load,
  trial) point: starts `hermes` under `sudo` (with `HERMES_HANG_INJECT` set
  automatically for Case 2), polls the port until it's actually listening
  (eBPF load/attach takes a moment), runs `hermes-bench` against it, stops
  the LB, and collects both sides' CSVs into `benchmark/results/` with
  **the same filename convention Phase 1 used**
  (`{policy}_case{case}_{load}_trial{n}_{conns,w{i}_ticks}.csv`) —
  specifically so the existing analysis notebook needs minimal changes to
  read either phase's output.
- [benchmark/run_all.sh](benchmark/run_all.sh) — the full 3 × 5 × (3 or 1) ×
  `TRIALS` matrix, resumable (skips result files that already exist), same
  shape as Phase 1's driver. Primes `sudo`'s credential cache once up front
  since every point needs it.

---

## Requirements & Ubuntu setup

Everything below is what the [Aya book](https://aya-rs.dev/book/) and
`bpf-linker`'s own README currently say (checked against their source, not
memory, dated 2026-07):

```bash
# Toolchains
rustup install stable
rustup toolchain install nightly --component rust-src

# bpf-linker (needed to link the eBPF object)
cargo install cargo-binstall   # or your preferred method
cargo binstall bpf-linker

# bpftool, only if you ever need to regenerate kernel struct bindings
# (not needed to build/run what's here)
```

Then:

```bash
cargo build --release -p hermes -p hermes-bench   # builds hermes-ebpf
                                                    # transitively via build.rs
sudo -E target/release/hermes --policy hermes --port 7878
# in another terminal:
target/release/hermes-bench --port 7878 --case 2 --load heavy --out conns.csv
```

`hermes` needs root (or `CAP_BPF` + `CAP_NET_ADMIN`) to load/attach eBPF and
to bind under `/sys/fs/bpf`; `hermes-bench` needs neither. There's no
blanket `sudo` wrapper in `.cargo/config.toml` — I deliberately removed the
`aya-template`'s default `runner = "sudo -E"` (which applies to *every*
target in the workspace) because it would silently sudo-wrap `cargo test -p
hermes-bench` and prompt for a password on commands that have no business
needing root. Run `sudo` explicitly on the one binary that needs it.

---

## Status: what to verify first

I don't have a Linux machine with a BPF toolchain available in this
environment, so **nothing in `hermes` or `hermes-ebpf` has been compiled**.
Everything was written against the real, current Aya API — I pulled
`aya-rs/aya`'s actual source for `SkReuseportContext`, `ReusePortSockArray`
(both the eBPF-side map and the userspace-side wrapper), `SkReuseport::load`
/ `attach`, `Map::from_map_data`, and the `aya-template`'s current
`Cargo.toml`/`build.rs` shape from GitHub while writing this, rather than
from memory, specifically to minimize the chance of API drift — but "I read
the source" is not "it compiled." First-build friction is likely to be
small (missing feature flags, a renamed method between the `0.14`/`0.2.x`
versions pinned in `Cargo.toml` and whatever's current when you build) but
should be easy to fix by following the compiler.

What's confirmed working right now, on this machine:
- `cargo check -p hermes-common -p hermes-bench` — clean.
- `cargo test -p hermes-common -p hermes-bench` — clean (no tests yet in
  either beyond what compiles; `hermes`'s `scheduler.rs` unit tests, ported
  unchanged from Phase 1, only run once you can build on Linux).
- `phase1/` — still builds and runs exactly as before
  (`cd phase1 && cargo build --release`).

What's untested and needs your hands on the real box, roughly in the order
I'd tackle them:

1. **First build.** `cargo build --release -p hermes -p hermes-bench` on
   Ubuntu with the toolchain above. Fix whatever the compiler finds.
2. **First run, no traffic.** `sudo -E target/release/hermes --policy
   reuseport` (no eBPF involved — the simplest path) — confirm it binds,
   forks 4 workers, and `hermes-bench --case 1` against it produces sane
   latencies and a roughly even `worker_id` distribution in the conns CSV.
3. **`lifo` next** (still no eBPF) — confirm `EPOLLEXCLUSIVE` registration
   succeeds (it's gated to Linux 4.5+, should be fine on any current Ubuntu)
   and that load concentrates on one worker under light/repeated-connection
   traffic, the way Phase 1's simulation predicted from the paper.
4. **`hermes` last** — this is the one that needs root for real:
   `/sys/fs/bpf` mounted as bpffs (it is by default on any systemd Ubuntu),
   `SO_ATTACH_REUSEPORT_EBPF` succeeding, `M_socket` populated correctly.
   `bpftool map dump pinned /sys/fs/bpf/hermes/m_sel` and `bpftool prog
   show` are your friends here if dispatch looks wrong.
5. **Re-validate `HANG_THRESHOLD_NS` (200ms) and `θ/avg = 0.5`.** These were
   tuned in Phase 1 against *simulated* timing (ms-scale sleeps standing in
   for real work). Real syscall/epoll/scheduler latency on actual hardware
   is a different regime; the values are very likely still reasonable
   (200ms is generous relative to real epoll_wait/accept/read latencies,
   which run in µs) but worth sanity-checking against real measurements
   before trusting them for new dissertation figures.
6. **The analysis notebook.** `analysis/hermes_analysis.ipynb` reads Phase
   1's exact CSV schema and I have **not** touched it — editing a Jupyter
   notebook's JSON blindly, without being able to run and check it, seemed
   more likely to corrupt it than help. The schema changes you'll need to
   account for:
   - Ticks CSV: `queue_len` → `open_conns` (real socket count instead of a
     simulated queue depth). Everything else — `bitmap_hex`, `bitmap_bin`,
     `after_stage{1,2,3}`, per-worker `w{i}_conns`/`w{i}_events`, `conn_sd`/
     `events_sd` — is identical.
   - Conns CSV: Phase 1 had `dequeue_ns`/`queue_wait_us` (there's no
     separate "dequeue from a simulated queue" step anymore — a real
     connection is just accepted and handled). Phase 2's schema is
     `conn_id,seq,kind,worker_id,send_ns,recv_ns,latency_us,service_us,label`
     (see `hermes-bench/src/client.rs::csv_header`); `kind` is `accept` |
     `burst` | `error` | `drop` (Phase 1 only had `accept`/`burst`). The
     notebook's latency/P99/CDF logic only ever needed `latency_us` +
     `worker_id` + case metadata, so this should be a small, mechanical
     update, not a rewrite — but I'd rather you do it with a running kernel
     to check figures against than have me guess blind.
   - Given all of that, I'd suggest treating `benchmark/results/` as a
     fresh dataset with its own small analysis pass rather than trying to
     force it through the exact same notebook cells Phase 1 used — the
     qualitative validation checks (§10 table, PASS/FAIL verdicts) are
     still the right thing to reproduce, just possibly worth a clean
     notebook rather than a patched one.
7. **`NUM_WORKERS = 4`.** Matches Phase 1; bump it in `hermes-common` (and
   rebuild everything) if you want it to match the real box's core count
   for more representative numbers.

Smaller, optional follow-ups noted inline in the code where they'd go:
`aya-log-ebpf` instrumentation in the eBPF program (none currently — see
the `hermes-ebpf` section above); `EPOLLOUT`-based backpressure on writes
(currently a bounded retry spin, fine for 16-byte localhost responses); a
`hermesctl`-style CLI for inspecting the live WST/`M_Sel` state or cleaning
up stale bpffs pins (currently just `rm -rf /sys/fs/bpf/hermes` by hand, or
automatic on a clean `hermes` shutdown).

---

## Fidelity audit against the paper (Phase 2 additions)

Phase 1's audit (WST layout, Algorithm 1's cascade, Algorithm 2's popcount
gate and exact `reciprocal_scale`, the 5ms liveness timer) stands — none of
that code changed. What's newly faithful in Phase 2, beyond what a userspace
simulation could ever demonstrate:

- **The `n ≤ 1` fallback is now literally the mechanism the paper
  describes**, not code that reimplements it: `SK_PASS` without calling
  `select_reuseport` *is* "the kernel silently falls back to plain
  reuseport hashing" (§5.4), enforced by the kernel itself, not by a
  fallback function this project wrote.
- **The `lifo` baseline's LIFO concentration is now the real kernel
  mechanism**, not a model of it. Phase 1's `dispatch_lifo` was an explicit
  written-from-the-paper's-description approximation (documented in
  `phase1/OVERVIEW.md`'s fidelity section as a corrected deviation from an
  earlier draft). Phase 2 has no equivalent function to get right or wrong
  — `EPOLLEXCLUSIVE` registration order *is* the wait-queue order, by
  construction.
- **Dispatch overhead claims (§7's flamegraph table) are now falsifiable**
  in a way Phase 1's simulation couldn't support: the real `hermes-ebpf`
  program is pure bitwise ops + one map lookup + one helper call, matching
  the paper's claim that the kernel dispatcher is by far the cheapest
  component. Phase 1 could only assert this from the paper; Phase 2 could
  actually be profiled (flamegraph/`perf`) against it once running, if you
  want that comparison for the dissertation.
