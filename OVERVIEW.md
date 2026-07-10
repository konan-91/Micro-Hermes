# micro-hermes — project overview

A single-page map of the whole simulator: what every `.rs` file does, what the
analysis pipeline produces, and an honest verdict on whether Phase 1 is
finished. Written to be read top-to-bottom in one sitting.

> **What this project is.** A *userspace* re-implementation of Hermes, Alibaba's
> eBPF Layer-7 load balancer (design writeup in [CLAUDE.md](CLAUDE.md)). Phase 1
> (this repo) simulates the entire 3-stage closed feedback loop with plain
> processes, shared memory, and sleeps — **no real sockets, epoll, or eBPF yet.**
> Phase 2 will swap the simulated kernel side for real Aya/eBPF. Every file below
> is written so Phase 2 *replaces* components rather than rewriting them.

---

## The one idea to hold in your head

It is a **closed feedback loop** running across processes:

```
 parent process ("the kernel")                 forked worker processes (one per core)
 ┌───────────────────────────┐                 ┌──────────────────────────────────┐
 │ generator: paces conns at │  push conn into │ worker: instrumented epoll loop  │
 │ the workload's CPS ───────┼──accept queue──▶│  · stamp time, process, close    │
 │ dispatcher: picks a worker│                 │  · update its WST column         │
 │  · hermes → read M_Sel    │◀── M_Sel bitmap─┤  · scheduler (Algo 1) → bitmap   │
 │  · reuseport → hash       │   (hermes only) └──────────────────────────────────┘
 │  · lifo → wait-queue model│
 └───────────────────────────┘        all shared state lives in one mmap region
```

A worker's *only* source of load is what the dispatcher put in its queue, and
the hermes dispatcher reads the bitmap the workers' schedulers write. So a
scheduling decision genuinely changes where the next connection lands — which is
exactly what the benchmarks measure. Nothing is faked around the loop.

The three **policies** are the experiment's independent variable:

| Policy | What it models | Runs a scheduler? |
|---|---|---|
| `hermes` | The full system: Algorithm 1 in workers + Algorithm 2 in the "kernel" | Yes |
| `reuseport` | `SO_REUSEPORT`'s stateless 4-tuple hash (state-blind) | No |
| `lifo` | `EPOLLEXCLUSIVE`'s LIFO wakeup (most-recently-blocked worker preferred) | No |

---

## The source files (`src/`)

Listed in the order it helps to read them. Each entry: what it is, its notable
sections, and its Phase-2 fate.

### [src/wst.rs](src/wst.rs) — the Worker Status Table
The shared data structure at the heart of everything. **Phase 2: unchanged.**
- `WorkerSlot` — one 64-byte cache line per worker holding the three Hermes
  metrics as `AtomicI64`: `last_loop_entry` (hang detection), `pending_events`
  (busy proxy), `accumulated_conns` (connection count). Padded to a full cache
  line to stop false sharing between adjacent workers.
- Lock-free rule: each worker is the *only* writer of its own slot; the scheduler
  reads all slots without a lock. Per-field atomicity only — cross-field
  consistency is intentionally *not* guaranteed (a stale read is harmless, §5.3.1).
- `snapshot_all()` — one-pass copy of the whole table for the scheduler.
- `now_monotonic_ns()` — `CLOCK_MONOTONIC` timestamp helper.

### [src/scheduler.rs](src/scheduler.rs) — Algorithm 1 (Stage 2)
Pure function: WST snapshots in, candidate bitmap out. **Phase 2: unchanged.**
- Three-stage cascading filter in fixed priority order: **Time** (drop hung
  workers) → **Conn count** (drop above-average) → **Events** (drop
  above-average). Order matters and is fixed by the paper.
- `HANG_THRESHOLD_NS = 200 ms` — implementation-defined (paper leaves it open);
  comfortably above the 5 ms timer + a healthy batch, catches injected hangs fast.
- `filter_below_baseline()` — the `avg + θ` filter with `θ = max(0.5·avg, 1.0)`.
  The `0.5` ratio is the paper's tuned optimum; the floor keeps it permissive at
  cold-start (avg ≈ 0) so it doesn't over-prune.
- Cold-start guard: `last_loop_entry == 0` counts as *alive*, not hung.
- Tests cover hang detection, cold start, each filter stage, and θ behaviour.

### [src/dispatcher.rs](src/dispatcher.rs) — the simulated kernel (Stage 3 + baselines)
Picks which worker gets each connection. Runs in the parent. **Phase 2: the
hermes path becomes an eBPF program (`SO_ATTACH_REUSEPORT_EBPF`); the two
baselines become the kernel's own mechanisms and are deleted from our code.**
- `dispatch_hermes()` — Algorithm 2: read the M_Sel bitmap, `reciprocal_scale`
  the hash into `[0, popcount)`, return the Nth set bit. Written **heap-free and
  loop-bounded on purpose** so the eBPF port is mechanical.
- Fallback rule: if ≤ 1 candidate survives, fall back to plain reuseport hashing
  (a lone candidate would soak up every connection between scheduler updates).
- `reuseport_hash()` — the stateless baseline.
- `dispatch_lifo()` — models epoll-exclusive: prefer the most-recently-blocked
  *idle* worker (empty queue + zero pending events + newest timestamp); if none is
  idle, hand to the shortest backlog. This reproduces the concentration pathology.

### [src/worker.rs](src/worker.rs) — the instrumented event loop (Stage 1)
One forked process per worker; the userspace analogue of Fig. 9's epoll loop.
**Phase 2: the queue poll becomes real `epoll_wait` on a `SO_REUSEPORT` socket;
the instrumentation and `schedule_and_sync` carry over.**
- Loop body maps line-for-line to Fig. 9: stamp `last_loop_entry` → poll the
  queue → `pending_events += batch` → process each event (a `sleep` for the
  connection's service cost) → `pending_events -= 1` → close expired connections
  → (hermes only) run the scheduler and write M_Sel.
- Hang injection: on Case 2, worker 0 sleeps mid-loop *without* re-stamping — the
  exact signature Stage 1 is meant to detect — then recovers naturally.
- `poll_accept_queue()` — the `epoll_wait` stand-in; returns on first event (up to
  `MAX_EVENTS`) or after the timeout.

### [src/generator.rs](src/generator.rs) — connection arrivals
The parent's traffic source; the *only* source of work. **Phase 2: replaced by
real clients (wrk etc.) — nothing here ports.**
- Absolute-deadline pacing at the case's CPS (oversleep doesn't accumulate drift).
- Per connection: build a synthetic 4-tuple hash (counter × Knuth constant, xor'd
  with the trial seed), sample the service cost, call `dispatch`, push to the
  chosen queue (or count a drop if the queue is full = kernel SYN drop).

### [src/workload.rs](src/workload.rs) — the four traffic profiles
The experiment's scenarios. **Phase 2: replaced by real load profiles.**
- `WorkloadConfig::for_case()` — the paper's four regimes (CLAUDE.md §10), sized
  to finish in a few seconds on a laptop while keeping the CPS × cost
  relationships: Case 1 high-CPS/cheap, Case 2 high-CPS/expensive + injected hang,
  Case 3 low-CPS/long-lived, Case 4 low-CPS/expensive.
- `ProcessingTime` — `Fixed` or `Bimodal` (models highly variable L7 cost);
  sampled deterministically from the hash, so no `rand` dependency.

### [src/shm.rs](src/shm.rs) — the shared-memory region
Everything that crosses the process boundary, in one `mmap(MAP_SHARED |
MAP_ANONYMOUS)` mapping created before `fork()`.
- `SelMap` — the simulated `M_Sel` eBPF map: a single `AtomicU64`. Its `store`/
  `load` API deliberately mirrors a one-element `BPF_MAP_TYPE_ARRAY` so **Phase 2
  only swaps the internals** (atomic → bpf map syscall).
- `ConnQueue` — a lock-free SPSC ring (single producer = generator, single
  consumer = worker), standing in for the kernel accept queue. **Phase 2: deleted
  — real sockets.** Tested for roundtrip, overflow, and index wrapping.
- `SharedState` — bundles the WST, M_Sel, drop counters, shutdown flag, queues.
- `mmap_shared_state()` — maps and zero-fills it.

### [src/metrics.rs](src/metrics.rs) — the CSV pipeline
Turns runs into data. **Phase 2: reusable as-is.**
- Two row types → two files: **ticks** (one row per loop iteration: WST snapshot,
  queue depth, Algorithm-1 stage survivors + bitmap) drives balance-over-time
  plots; **conns** (one row per completed connection: arrival→dequeue→done) drives
  latency/P99 plots.
- Fork-safe collection: each worker buffers rows locally and writes a **private
  shard file** on exit; the parent merges after `waitpid`. Shared-memory metrics
  would be unsound across `fork()` (a `Vec`'s heap isn't in the mapping; macOS
  `os_unfair_lock` panics cross-process) — so this mirrors the WST's
  "each worker writes only its own column" idea, but with files.
- `summarize_conns` / `std_dev` / `percentile` — the console-summary stats.

### [src/main.rs](src/main.rs) — the wiring
Parse env config (`POLICY`, `WORKLOAD_CASE`, `SEED`, paths) → mmap shared state →
fork one worker per slot → run the generator in the parent → `waitpid` → merge
metric shards → print the run summary. **Phase 2: rewired around real sockets.**

---

## The analysis pipeline (`analysis/`)

This is the part you asked me to scrutinise for redundancy. Verdict: **it is
correct and nothing here is dead weight** — the reasoning for each piece:

- **[analysis/run_benchmarks.sh](analysis/run_benchmarks.sh)** — runs the full
  3 policies × 4 cases × N trials matrix, one distinct `SEED` per trial, skipping
  files that already exist (resumable). Correct and needed.
- **[analysis/hermes_analysis.ipynb](analysis/hermes_analysis.ipynb)** — the one
  thing you run (*Restart & Run All*). Generates the data if missing, then every
  figure and table, each with a **dissertation-ready caption + design
  justification** paragraph. **Keep the notebook:** it is the analysis
  deliverable, and the inline prose is written to lift straight into your
  write-up. The six figures are each distinct and non-redundant:
  1. latency CDFs (full distribution), 2. P99 bars (the headline number),
  3. balance-over-time (the paper's Fig. 13 metric), 4. concentration profile
  (the *shape* of imbalance a single SD hides), 5. hang detection (the mechanism
  demo), 6. cascade stages (each filter earning its place). Cell 9 mechanically
  checks the paper's qualitative claims and prints a PASS/FAIL verdict table.
- **`figures/` holding both PNG *and* PDF — keep both, this is not redundant.**
  PDF is vector and is what you `\includegraphics` into a LaTeX dissertation; PNG
  (300 dpi) is for drafts, Markdown preview, and slides. Standard academic
  practice; the small duplication buys real convenience.
- **`tables/` holding CSV *and* `.tex` — keep both.** The `.tex` is a ready
  `tabular`/`table` environment for the write-up; the CSV is for spot-checking and
  re-processing.
- **`results/*.csv`** — raw per-run output, git-ignored, regenerated on demand.

One caveat the notebook already documents and you should carry into the
write-up: in Cases 2 and 4 the `lifo` baseline posts the *best* raw latency,
whereas the paper ranks epoll-exclusive below Hermes. That is a **simulation
boundary, not a bug** — a userspace sim can't reproduce epoll-exclusive's real
kernel costs (O(#ports) wait-queue traversal, thundering-herd wakeups across many
listen sockets), and with those absent its all-busy path approximates an ideal
central queue. Present the latency numbers alongside this limitation.

---

## Is it finished?

**Phase 1 (the userspace simulator): yes — complete and healthy.**

| Check | Status |
|---|---|
| All three policies implemented (hermes / reuseport / lifo) | ✅ |
| Algorithm 1 (cascading filter) + Algorithm 2 (eBPF-portable dispatch) | ✅ |
| WST, M_Sel, SPSC queues in real shared memory, lock-free per §5.3.1 | ✅ |
| Instrumented event loop maps line-for-line to Fig. 9 | ✅ |
| All four workload cases + injected hang | ✅ |
| Metrics → CSV → notebook → 6 figures + 2 tables | ✅ |
| `cargo test` — 17 tests | ✅ pass |
| `cargo build --release` | ✅ clean, no warnings |
| Notebook's qualitative-validation checks vs. the paper | ✅ all 7 PASS |

I verified the last three just now. There is **no missing simulator work and no
dead code to strip** — the pieces that look removable (`SelMap`'s indirection,
the baseline blocking behaviour, small `MAX_EVENTS`, dual figure formats) are all
deliberate and documented, mostly to keep the Phase-2 port mechanical.

### What is genuinely left — none of it Phase-1 code
1. **Re-run the benchmark matrix today** for your final numbers
   (`analysis/run_benchmarks.sh`, then *Restart & Run All* in the notebook). The
   committed `results/` and figures are from a prior run and will be regenerated.
   Bump `TRIALS` (3 → 5+) in the notebook's first cell for tighter error bars.
2. **Write up the results** using the figures, tables, and the notebook's inline
   justifications — including the epoll-exclusive simulation-boundary caveat above.
3. **Phase 2 (future, not today):** replace the simulated kernel side with real
   Aya/eBPF — `SO_ATTACH_REUSEPORT_EBPF`, real `SO_REUSEPORT` sockets in a
   `REUSEPORT_SOCKARRAY`, real `epoll_wait`. The "Phase 2 fate" note on each file
   above is your porting checklist; `wst.rs`, `scheduler.rs`, and `metrics.rs`
   survive untouched.

### Housekeeping done in this pass
Removed stray manual-run outputs (`conns.csv`, `metrics.csv`), a redundant
root-level `results/` directory (the real one is `analysis/results/`), and
`.DS_Store` junk. All were untracked and regenerable.
