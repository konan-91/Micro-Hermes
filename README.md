# micro-hermes

A userspace simulation of **Hermes**, Alibaba's closed-source eBPF Layer-7
load balancer, built for an MSc dissertation replicating it in Rust (Aya).

This is **Phase 1**: a pure-userspace simulation of the full 3-stage
closed feedback loop (Worker Status Table → cascading-filter scheduler →
connection dispatch), with no real sockets, epoll, or eBPF yet. It exists to
validate the scheduling algorithm and produce comparative benchmarks against
`lifo` (epoll-exclusive-like) and `reuseport` (stateless-hash) baselines
before Phase 2 rewrites the kernel/eBPF-facing parts for real.

See [CLAUDE.md](CLAUDE.md) for the full design writeup this implementation
follows.

## Architecture

The process layout mirrors the real system's kernel/userspace split, so the
Phase-2 rewrite replaces components rather than restructuring:

```
┌─ parent process ─ "the kernel" ───────────────────────────────┐
│  generator.rs   paces connection arrivals at the workload CPS │
│  dispatcher.rs  picks a worker per connection:                │
│                   hermes    → Algorithm 2 over the M_Sel bitmap│
│                   reuseport → stateless 4-tuple hash          │
│                   lifo      → epoll-exclusive wait-queue model│
└───────┬───────────────────────────────────────────────────────┘
        │ per-worker accept queues (SPSC rings, shared memory)
┌───────▼─ forked worker processes (one per slot) ──────────────┐
│  worker.rs      instrumented event loop (Fig. 9): stamp time, │
│                 epoll_wait analogue, process events, close    │
│                 expired conns, update WST                     │
│  scheduler.rs   Algorithm 1 (hermes only) → bitmap → M_Sel    │
└────────────────────────────────────────────────────────────────┘
     shared mmap region (shm.rs): WST + M_Sel + queues
```

The feedback loop is real: a worker's load comes **only** from connections
the dispatcher placed in its queue, and the hermes dispatcher reads the
bitmap that workers' schedulers write — so scheduling decisions directly
shape load distribution, which is what the benchmarks measure.

### File map & Phase-2 fate

| File | Role | Phase 2 |
|---|---|---|
| [src/wst.rs](src/wst.rs) | Worker Status Table: per-worker 64-byte slot of three `AtomicI64` metrics (`time`, `busy`, `conn`) in shared memory | **Unchanged** |
| [src/scheduler.rs](src/scheduler.rs) | Algorithm 1: Time → Conn → Event cascading filter, θ/avg = 0.5, → bitmap | **Unchanged** (pure function) |
| [src/shm.rs](src/shm.rs) | One mmap'd `SharedState`: WST, `SelMap` (simulated `M_Sel` eBPF map), SPSC accept queues, shutdown flag | `SelMap` internals become an aya map handle; queues/shutdown deleted (real sockets) |
| [src/worker.rs](src/worker.rs) | Instrumented event loop (Fig. 9), one forked process per worker | Queue-poll becomes real `epoll_wait` on a `SO_REUSEPORT` socket; instrumentation + `schedule_and_sync` carry over |
| [src/dispatcher.rs](src/dispatcher.rs) | Kernel stand-in: Algorithm 2 (written loop-bounded/heap-free for mechanical eBPF porting) + both baselines | Hermes path → eBPF program via `SO_ATTACH_REUSEPORT_EBPF`; baselines → the kernel's own mechanisms |
| [src/generator.rs](src/generator.rs) | Paces synthetic connections at the case's CPS, dispatches, pushes to queues | Replaced by a real traffic generator (wrk etc.) |
| [src/workload.rs](src/workload.rs) | The four paper traffic profiles (CPS × processing cost × lifetime) + hang injection | Replaced by real client-side load profiles |
| [src/metrics.rs](src/metrics.rs) | CSV pipeline: per-iteration tick rows + per-connection latency rows, shard/merge across fork | Reusable as-is for benchmarking |
| [src/main.rs](src/main.rs) | mmap, fork workers, run generator, merge metrics, print summary | Rewired around real sockets |

## Requirements

- Rust (stable), any OS with `libc` support (developed on macOS, targets
  Linux — no OS-specific code paths).
- No eBPF toolchain needed yet — that's Phase 2.

## Running

```bash
cargo run
```

All configuration is via environment variables:

| Variable | Values | Default | Meaning |
|---|---|---|---|
| `POLICY` | `hermes` \| `lifo` \| `reuseport` | `hermes` | Dispatch mechanism under test |
| `WORKLOAD_CASE` | `1` \| `2` \| `3` \| `4` \| `default` | `default` | Traffic profile (see below) |
| `METRICS_PATH` | file path | `metrics.csv` | Per-iteration tick CSV |
| `CONNS_PATH` | file path | `conns.csv` | Per-connection latency CSV |
| `VERBOSE` | `1` | off | Print every worker loop iteration |
| `SEED` | integer | `0` | Perturbs the synthetic connection-hash stream (for distinct benchmark trials) |

### Policies

- **`hermes`** — workers run Algorithm 1 (Time → Conn → Event cascading
  filter over the WST) at the end of every loop iteration and publish the
  candidate bitmap to the simulated `M_Sel` map; the dispatcher runs
  Algorithm 2 (kernel `reciprocal_scale` of the connection hash into the
  candidate count, pick the Nth set bit). If ≤ 1 candidate survives, it
  falls back to plain reuseport hashing, per the paper.
- **`lifo`** — models epoll-exclusive's wakeup mechanics: an idle worker
  blocking in `epoll_wait` sits in the listen socket's wait queue, insertion
  is at the *head*, and only the first entry is woken. The dispatcher
  therefore picks the most-recently-blocked idle worker (empty queue, no
  pending events, newest loop timestamp); when nobody is idle it hands the
  connection to the shortest backlog, approximating the shared accept queue.
  Baseline workers block until work arrives (like `epoll_wait(-1)`) rather
  than on Hermes's 5 ms scheduler timer, so the wait-queue order stays
  stable — this is what produces the real mechanism's concentration
  pathology.
- **`reuseport`** — `SO_REUSEPORT`'s stateless hash: `reciprocal_scale`
  of the 4-tuple hash across all workers, no awareness of worker state.

### Workload cases

The four traffic profiles from the paper (CLAUDE.md §10). Each is defined by
connections/sec and a per-connection processing-cost distribution (cost is a
property of the connection — SSL, compression — not the worker):

| Case | Profile | Notes |
|---|---|---|
| `1` | High CPS, low cost | Stress/spike; ~10% utilization |
| `2` | High CPS, high cost | Compression-heavy; sustained overload (offered load ≈ 4.5 vs capacity 4); worker 0 injects a 400 ms hang to exercise Stage-1 hang detection |
| `3` | Low CPS, low cost | Long-lived connections (60 s lifetime; finance/chat) — final open-conn balance is the headline metric |
| `4` | Low CPS, high cost | SSL/regex-heavy; ~75% utilization |
| `default` | Mixed, short | Quick smoke test, not paper validation |

### Reproducing the comparison matrix

Everything for benchmarking and graphing lives in [analysis/](analysis/):
`analysis/run_benchmarks.sh` runs the full policy × case × trial matrix, and
`analysis/hermes_analysis.ipynb` is an annotated notebook that generates all
dissertation figures (PNG + PDF) and tables (CSV + LaTeX) in one *Run All* —
see [analysis/README.md](analysis/README.md).

### Expected qualitative results (validation targets, CLAUDE.md §10)

- **Case 1**: latencies indistinguishable; `lifo` concentrates load badly
  (one worker can get 0 connections), `hermes` balances best.
- **Case 2**: `hermes` clearly beats `reuseport` on latency (reuseport keeps
  hashing onto overloaded/hung workers). `lifo` posts good latency here
  because the simulation cannot reproduce epoll-exclusive's real kernel
  costs (O(#ports) wait-queue traversal, thundering-herd wakeups) — with
  those absent, its all-busy path approximates an optimal central queue.
  This is a known, accepted simulation artifact.
- **Case 3**: open-connection SD ranks hermes < reuseport ≪ lifo
  (reproduces the paper's production result: Hermes 20 < reuseport 50 ≪
  exclusive 3200, with lifo assigning *zero* connections to some workers).
- **Case 4**: hermes ≈ lifo (slightly behind, matching the paper's noted
  feedback lag vs exclusive's promptness) and both far ahead of reuseport.

## Output

### Console summary

```
── Run summary (hermes / case 2) ─────────────────────
  generated=400  completed=400  dropped=0
  worker 0: dispatched=  112  completed=  112  open_at_exit=   4  dropped=   0
  ...
  balance: completed SD = 7.78   open-conn SD = 2.18  (lower = better, Fig. 13)
  latency (arrival→done): mean = 586.3ms  p50 = 420.3ms  p99 = 1649.1ms  max = 1785.3ms
```

### Ticks CSV (`metrics.csv`)

One row per worker event-loop iteration — for balance-over-time plots:

```
timestamp_ns, worker_id, iter, bitmap_hex, bitmap_bin, after_stage1..3,
queue_len, policy, w{0..N}_conns, w{0..N}_events, conn_sd, events_sd
```

Stage/bitmap columns are zero for the baselines (they run no scheduler).

### Conns CSV (`conns.csv`)

One row per completed connection — for latency CDFs / P99 comparisons:

```
conn_id, worker_id, arrival_ns, dequeue_ns, done_ns,
queue_wait_us, service_us, latency_us, policy
```

`latency_us = done - arrival` (queue wait + processing). Both files load
directly into pandas/matplotlib.

## Architecture notes

- **All cross-process state is real shared memory** — one
  `mmap(MAP_SHARED | MAP_ANONYMOUS)` region holding the WST (each worker
  writes only its own 64-byte cache-line slot, everyone reads lock-free),
  the `M_Sel` bitmap (single `AtomicU64`, mirroring an eBPF array map's
  atomic int guarantee), and the accept queues (SPSC rings: single producer
  = generator, single consumer = owning worker, so acquire/release ordering
  suffices with no locks). No locks anywhere, matching §5.3.1.
- **Metrics collection is *not* shared memory.** Each worker buffers rows
  locally and writes private shard files on exit; the parent merges after
  `waitpid`. (A `Mutex<Vec>` in `MAP_SHARED` memory is unsound across
  `fork()` — the Vec's heap buffer isn't in the shared region, and macOS's
  `os_unfair_lock` panics with `EINVAL` cross-process.)
- **Simulation compensations** (documented in code): `MAX_EVENTS` is small
  (4) because simulated per-event costs are ms-scale sleeps, not the real
  µs-scale — large batches would stretch iterations past the 200 ms hang
  threshold and starve the scheduler of fresh data; baseline workers block
  until work arrives (modelling `epoll_wait(-1)`) instead of using Hermes's
  5 ms timer, because any periodic idle wakeup re-inserts phase-locked
  forked workers at the wait-queue head in lock-step, rotating the LIFO
  concentration target and artificially evening out per-worker totals.

## Testing

```bash
cargo test
```

Covers Algorithm 1 (hang detection, cold start, conn/event filters, θ
behavior), Algorithm 2 (`reciprocal_scale` range, Nth-set-bit, single/zero
candidate fallback), the LIFO wait-queue model, and the SPSC queue
(roundtrip, overflow, index wrapping).

## Roadmap

Phase 2 replaces the simulated kernel side with the real one: an Aya-based
eBPF program attached via `SO_ATTACH_REUSEPORT_EBPF`, real `SO_REUSEPORT`
listening sockets (one per worker, populated into a
`BPF_MAP_TYPE_REUSEPORT_SOCKARRAY`), and a real epoll event loop — reusing
the WST layout, Algorithm 1, and the Algorithm-2 logic implemented here.
