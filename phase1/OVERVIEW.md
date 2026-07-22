# micro-hermes — project overview

A single-page map of the whole simulator: what the benchmarks are and what each
one tests, what every `.rs` file does, what the analysis pipeline produces, and
an honest verdict on the state of Phase 1. Written to be read top-to-bottom in
one sitting.

> **What this project is.** A *userspace* re-implementation of Hermes, Alibaba's
> eBPF Layer-7 load balancer (design writeup in [CLAUDE.md](../CLAUDE.md), paper in
> `Hermes_SIGCOMM25.pdf`). Phase 1 (this repo) simulates the entire 3-stage
> closed feedback loop with plain processes, shared memory, and sleeps — **no
> real sockets, epoll, or eBPF yet.** Phase 2 will swap the simulated kernel side
> for real Aya/eBPF. Every file below is written so Phase 2 *replaces* components
> rather than rewriting them.

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

---

## The benchmarks — what is being compared, and what each case tests

Every benchmark run is the same experiment: fire synthetic connections at four
workers and measure (a) how fast connections get served and (b) how evenly the
work is spread. Two things vary: the **dispatch policy** (how the "kernel"
picks a worker) and the **traffic case** (what the connections look like).
3 policies × 5 cases × 3 seeded trials = 45 runs.

**The three policies** — the experiment's independent variable:

| Policy | What it models | In plain terms |
|---|---|---|
| `hermes` | The full system: Algorithm 1 (worker-side filter) + Algorithm 2 (kernel-side pick) | Workers publish how busy they are; new connections only go to workers that look healthy and under-loaded |
| `reuseport` | `SO_REUSEPORT`'s stateless hash | Deal connections out pseudo-randomly; never look at how busy anyone is |
| `lifo` | `EPOLLEXCLUSIVE` wakeup (static wait-queue, last-registered worker at the head) | Always wake the same first-in-line worker if it's free; only spill to the next one when it's busy |

**The five traffic cases** — the paper's four regimes (high/low arrival rate ×
cheap/expensive requests) plus one scenario of our own:

| Case | Traffic | What it tests, in one sentence |
|---|---|---|
| 1 | Many connections/sec, each cheap (1 ms) | With trivial load, does anything break? Latency should tie; the interesting output is *fairness* — LIFO funnels everything to one worker even here. |
| 2 | Many connections/sec, each expensive, total load ≈ 112% of capacity, plus a 400 ms stall injected into one worker | The stress test: when workers are drowning and one freezes, does the policy notice and route around it? (Reuseport can't — it keeps hashing onto the frozen worker.) |
| 3 | Few connections/sec, cheap, but connections stay open forever | The paper's headline production scenario: dispatch mistakes are permanent, so end-of-run connection balance directly exposes each policy's character. |
| 4 | Few connections/sec, each expensive (~75% utilisation) | Can the policy avoid queueing a new connection behind a slow request on an already-busy worker? |
| 5 | Case 3's accumulation, then **every open connection fires one request at the same instant** | Our extension beyond the paper: the "synchronized burst" (market open, mass push notification). Requests on an established connection are pinned to the worker owning it, so however unevenly connections were hoarded is how unevenly the burst must be served — this is the direct evidence for *why* LIFO's concentration is dangerous, and the stated reason the WST's connection-count metric exists. |

**The two metrics**, in plain terms:

- **Latency** — from the moment a connection (or burst request) arrives to the
  moment its processing finishes: queueing + service. Reported as median and
  **p99** (the time 99% of connections beat — the tail that SLOs care about).
- **Balance** — the standard deviation of per-worker connection counts (the
  paper's own Fig. 13 metric). 0 = perfectly even; big = someone is hoarding.

The headline finding (numbers and figures live in the notebook and
`pissertation.txt` §6): no policy wins everywhere, but **hermes is the only one
that never loses badly** — reuseport's latency blows up when workers are busy
(Cases 2/4), LIFO's fairness collapses when they aren't (Cases 1/3) and its
hoarding costs a ~4× latency penalty when the hoarded connections wake up
(Case 5). That adaptability is the paper's core claim, reproduced.

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
- `dispatch_lifo()` — models epoll-exclusive faithfully to the paper's §2.2 /
  Fig. A3: the socket's wait queue is ordered by `epoll_ctl` registration (each
  insertion at the *head*), so priority is **static** — the last-registered
  (highest-index) idle worker always wins, and a wakeup stops at the first
  non-busy entry. If nobody is idle, hand to the shortest backlog (proxy for the
  shared accept queue). This reproduces the concentration pathology exactly:
  under Cases 1/3/5 the head worker takes essentially every connection.

### [src/worker.rs](src/worker.rs) — the instrumented event loop (Stage 1)
One forked process per worker; the userspace analogue of Fig. 9's epoll loop.
**Phase 2: the queue poll becomes real `epoll_wait` on a `SO_REUSEPORT` socket;
the instrumentation and `schedule_and_sync` carry over.**
- Loop body maps line-for-line to Fig. 9: stamp `last_loop_entry` → poll the
  queue → `pending_events += batch` → process each event (a `sleep` for the
  connection's service cost) → `pending_events -= 1` → close expired connections
  → (hermes only) run the scheduler and write M_Sel.
- The 5 ms `epoll_wait` timeout applies under **every** policy — the paper's LB
  runs this timer in all epoll modes (Fig. 5b measured it under epoll exclusive);
  for Hermes it additionally guarantees scheduler liveness (§5.3.2).
- Hang injection (Case 2): worker 0 sleeps mid-loop *without* re-stamping — the
  exact signature Stage 1 is meant to detect — then recovers naturally.
- Burst injection (Case 5): at the configured instant, every connection this
  worker holds open generates one ready follow-up event; the backlog is drained
  through the normal loop in `MAX_EVENTS` batches ahead of the accept queue,
  like a saturated run-to-completion worker. Connection affinity is enforced by
  construction — only the owning worker ever sees these events.
- `poll_accept_queue()` — the `epoll_wait` stand-in; returns on first event (up to
  `MAX_EVENTS`) or after the timeout.

### [src/generator.rs](src/generator.rs) — connection arrivals
The parent's traffic source; the *only* source of work. **Phase 2: replaced by
real clients (wrk etc.) — nothing here ports.**
- Absolute-deadline pacing at the case's CPS (oversleep doesn't accumulate drift).
- Per connection: build a synthetic 4-tuple hash (counter × Knuth constant, xor'd
  with the trial seed), sample the service cost, call `dispatch`, push to the
  chosen queue (or count a drop if the queue is full = kernel SYN drop).

### [src/workload.rs](src/workload.rs) — the five traffic profiles
The experiment's scenarios (see the benchmark table above). **Phase 2: replaced
by real load profiles.**
- `WorkloadConfig::for_case()` — the paper's four regimes plus Case 5, sized to
  finish in a few seconds on a laptop while keeping the CPS × cost relationships.
- `ProcessingTime` — `Fixed` or `Bimodal` (models highly variable L7 cost);
  sampled deterministically from the hash, so no `rand` dependency.
- `HangSpec` (Case 2) and `BurstSpec` (Case 5) — the two fault/stress injections.

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
  plots; **conns** (one row per completed event: arrival→dequeue→done, with a
  `kind` column distinguishing ordinary accepts from Case-5 burst follow-ups)
  drives latency/P99 plots.
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

- **[analysis/run_benchmarks.sh](../analysis/run_benchmarks.sh)** — runs the full
  3 policies × 5 cases × N trials matrix, one distinct `SEED` per trial, skipping
  files that already exist (resumable).
- **[analysis/hermes_analysis.ipynb](../analysis/hermes_analysis.ipynb)** — the one
  thing you run (*Restart & Run All*). Generates the data if missing, then every
  figure and table, each with a **dissertation-ready caption + design
  justification** paragraph written to lift straight into the write-up. The
  seven figures are each distinct:
  1. latency CDFs (full distribution), 2. P99 bars (the headline number),
  3. balance-over-time (the paper's Fig. 13 metric), 4. concentration profile
  (the *shape* of imbalance a single SD hides), 5. hang detection (the mechanism
  demo), 6. cascade stages (each filter earning its place), 7. the Case-5 burst
  CDF (the cost of concentration, measured). Section 9 mechanically checks the
  paper's qualitative claims and prints a PASS/FAIL verdict table — currently
  **8/8 PASS**.
- **`figures/`** — every figure as PNG (300 dpi, for drafts/slides) *and* PDF
  (vector, for LaTeX `\includegraphics`).
- **`tables/`** — `summary_stats` (CSV + ready LaTeX `tabular`),
  `burst_stats.csv`, `validation_verdicts.csv`.
- **`results/*.csv`** — raw per-run output, git-ignored, regenerated on demand.

One caveat the notebook documents and the write-up carries (pissertation.txt
§6.5): in Cases 2 and 4 the `lifo` baseline posts the *best* raw latency,
whereas the paper ranks epoll-exclusive below Hermes. That is a **simulation
boundary, not a bug** — a userspace sim can't reproduce epoll-exclusive's real
kernel costs (O(#ports) wait-queue traversal, wakeup overhead across many
listen sockets), and with those absent its all-busy path approximates an ideal
central queue. Case 5 was added precisely to supply the measured counterweight:
the concentration behind that low latency costs LIFO a ~4× penalty the moment
its hoarded connections become active.

---

## Is it finished?

**Phase 1 (the userspace simulator): yes — complete and healthy.**

| Check | Status |
|---|---|
| All three policies implemented (hermes / reuseport / lifo) | ✅ |
| Algorithm 1 (cascading filter) + Algorithm 2 (eBPF-portable dispatch) | ✅ |
| WST, M_Sel, SPSC queues in real shared memory, lock-free per §5.3.1 | ✅ |
| Instrumented event loop maps line-for-line to Fig. 9 | ✅ |
| Five workload cases, incl. injected hang (Case 2) and burst (Case 5) | ✅ |
| Metrics → CSV → notebook → 7 figures + 3 tables | ✅ |
| `cargo test` — 17 tests | ✅ pass |
| `cargo build --release` | ✅ clean, no warnings |
| Notebook's qualitative-validation checks vs. the paper | ✅ all 8 PASS |
| Benchmarks re-run on the current code; figures/tables regenerated | ✅ 2026-07-14 |
| Results written up in `pissertation.txt` §6 | ✅ |

### What is genuinely left — none of it Phase-1 code
1. Optionally bump `TRIALS` (3 → 5+) in the notebook's first cell for tighter
   error bars before final submission figures.
2. **Phase 2 (future):** replace the simulated kernel side with real Aya/eBPF —
   `SO_ATTACH_REUSEPORT_EBPF`, real `SO_REUSEPORT` sockets in a
   `REUSEPORT_SOCKARRAY`, real `epoll_wait`. The "Phase 2 fate" note on each file
   above is the porting checklist; `wst.rs`, `scheduler.rs`, and `metrics.rs`
   survive untouched.

---

## Fidelity audit against the paper

A full pass of `Hermes_SIGCOMM25.pdf` against this codebase (2026-07-10,
re-verified 2026-07-14). Verified faithful, clause by clause: the WST layout
and lock-free rules (§4.1, §5.3.1); the Fig. 9 instrumentation points and their
order; Algorithm 1's Time → Conn → Event cascade with `< avg + θ`, θ/avg = 0.5
(Fig. 15 optimum), averaging over the surviving set; Algorithm 2's popcount
gate, exact kernel `reciprocal_scale`, and n ≤ 1 fallback; and the §5.3.2
scheduler properties (embedded in every worker, frequency scales with load,
5 ms liveness timer). The 200 ms hang threshold and the θ ≥ 1 cold-start floor
are implementation-defined (the paper leaves both open) and documented in
`scheduler.rs`.

**One deviation was found and corrected** (2026-07-10). The epoll-exclusive
baseline previously preferred the *most-recently-looped* idle worker — a
dynamic priority. The paper is explicit that the wait-queue order is
**static**: workers are inserted at the head of the socket's wait list by
`epoll_ctl()` at registration, so the last-*registered* worker is permanently
preferred (§2.2, Fig. A2/A3). `dispatch_lifo` now picks the highest-index
(last-forked ⇒ last-registered) idle worker. This also removed the need for a
second deviation: with static priority, idle wakeups can't reshuffle the
queue, so the paper's 5 ms `epoll_wait` timer now applies under every policy.
The benchmark matrix was regenerated after the fix, and the corrected model
shows the textbook pathology: LIFO sends *every* connection to the head worker
under Cases 1, 3 and 5.

**One deliberate extension beyond the paper**: Case 5 (the synchronized burst)
is not one of the paper's four evaluation regimes. It operationalises the
paper's own rationale for the `conn` metric (§5.2.1 — guarding against bursts
on accumulated idle connections) into a measurable scenario, because the four
latency benchmarks alone cannot show the cost of connection hoarding.
