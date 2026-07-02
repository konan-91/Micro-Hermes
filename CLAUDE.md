My MSc dissertation project is replicating a closed-source eBPF layer 7 load balancer made by Alibaba in Rust (Aya).

IMPORTANT: right now, I'm making a *userspace simulation* of the load balancer*. I want it to be as close as possible to the 
real thing, just in userspace. Once it works and I've done benchmarking etc, then I'll move on to rewriting the parts which
simulate kernel and eBPF activity.

## 1. Problem Statement

L7 LBs dispatch TCP connections from kernel accept-queues to userspace worker
processes (one worker pinned per CPU core, run-to-completion over epoll). Existing
kernel dispatch mechanisms are blind to userspace load:

- **Epoll exclusive** (Linux 4.5+, `EPOLLEXCLUSIVE`): wait queue is a kernel list;
  workers are inserted at the **head** on `epoll_ctl()`. On wakeup, the queue is
  traversed and the **first idle** worker found is woken, then traversal stops. Net
  effect: **LIFO wakeup** — most-recently-registered worker is preferred →
  connections concentrate on a few workers → CPU imbalance, worker hangs.
- **Reuseport** (`SO_REUSEPORT`, Linux 3.9+): each worker binds its own socket to
  the same port; kernel picks a socket via **stateless hash of the 4-tuple**. Good
  even distribution in theory, but: hash collisions under heavy hitters, and **no
  awareness of worker health** — keeps dispatching to hung/crashed workers.
- **Epoll roundrobin (rr)**: proposed fix (moves woken worker to tail of list);
  never merged into mainline kernel (cache-unfriendly).

L7 processing cost is highly variable per connection (SSL, compression, protocol
translation) — unlike L4/packet-only load which correlates with queue depth. Kernel
cannot estimate this. **Hermes's core idea**: treat userspace worker runtime status
as a first-class scheduling input, fed back into the kernel via eBPF, overriding
reuseport's default socket selection.

## 2. Overall Architecture — 3-Stage Closed Feedback Loop

1. **Stage 1 — Worker status update**: each worker updates its live status (avail,
   busy, conn) into a shared-memory **Worker Status Table (WST)** during normal
   epoll event processing.
2. **Stage 2 — Connection scheduling (userspace)**: each worker, at the end of its
   own epoll loop, runs a lightweight scheduler that reads the **entire WST**,
   applies a 3-stage cascading filter to compute a **coarse-grained candidate set**
   of available workers, and pushes the result (as a bitmap) into an eBPF map.
3. **Stage 3 — Connection dispatch (kernel/eBPF)**: on each new connection (SYN
   completing handshake), an eBPF program attached via `SO_ATTACH_REUSEPORT_EBPF`
   reads the bitmap and does **fine-grained selection** (one specific worker) using
   a hash of the connection's 4-tuple, overriding reuseport's default socket pick.

Every worker embeds **both** a status-updater and a scheduler — there is no
separate/dedicated scheduler process (would waste a core and be a SPOF). Multiple
workers' schedulers run independently and concurrently.

## 3. Worker Status Table (WST)

- Located in **shared anonymous memory**, visible to all worker processes
  (inter-process, e.g. `mmap` shared across forked workers).
- Layout: **rows = metrics, columns = workers** (one column per worker).
- Three metrics per worker, each stored as an **`atomic<int>`** (or bool-as-int):

| Metric | Type | Meaning | Updated at |
|---|---|---|---|
| `avail` | bool | Worker not hung/crashed | Derived from `time` (see below); implicit rather than stored, or updated at loop entry |
| `time` | timestamp | Time worker last **entered** the epoll `while` loop | Every iteration, loop entry (line 12 in event loop, before `epoll_wait`) |
| `busy` | int | Pending events triggered but not yet handled (i.e., queued-but-unprocessed epoll events) | `+= event_num` after `epoll_wait()` returns; `-= 1` after each event is handled |
| `conn` | int | Accumulated concurrent connections on this worker | `+= 1` on `accept()`; `-= 1` on connection close/err/fin |

**Why these three metrics** (rationale, keep for design fidelity):
- `time`: hang detection. A hung worker stops re-entering the loop. Compare
  `currentTime() - time_i > Threshold` → mark unavailable. (**Threshold value is
  not specified numerically in the paper — implementation-defined constant.**)
- `busy`: proxies processing load. Packet size per event is only known *after*
  processing (unavailable at schedule time), and handler-type alone was found
  insufficient for estimating cost; **event count alone correlated well enough**
  with processing time, so it was chosen as the sole responsiveness proxy.
- `conn`: guards against **future** overload from synchronized traffic bursts on
  many currently-idle long-lived connections, and against per-worker fixed-size
  connection-pool exhaustion (both observed as real incidents in production).

**Why compute metrics/scheduling in userspace, not kernel**: kernel has global
visibility but no application-specific context; keeping the scheduler in userspace
avoids eBPF's limited programmability (no loops/recursion/complex hashing) and
allows dynamic policy updates (production system exposes an HTTP control
interface), fallback logic, and rapid iteration. Only the **final scheduling
result** (a bitmap) crosses into the kernel — not raw metrics.

### Concurrency / lock-free design (critical for correctness)
- Shared memory is **partitioned per worker** — each worker only ever writes its
  own column, so no write-write contention, no locks needed between workers for
  writes.
- The scheduler reads the **entire WST** without taking read-write locks — updates
  from other workers can race with a read. This is accepted **by design**:
  - Empirically, reads take "tens of ns", status updates happen "every few ms" →
    probability of hitting a mid-update read is very low.
  - Even if a stale/mid-update value is read occasionally, it does not
    meaningfully change scheduling decisions ("most recent data better reflects
    current status" — stale reads are not catastrophic, just suboptimal).
- To prevent **torn/dirty reads of a single field** (not cross-field
  consistency — that's intentionally not guaranteed), each of the 3 status
  variables (`time`, `busy`/event, `conn`) is stored individually as
  `atomic<int>`, guaranteeing per-field read/write atomicity only.
- **No explicit locks anywhere** in the WST or in the userspace→kernel handoff.

## 4. Modified Epoll Event Loop (Stage 1 instrumentation)

Original loop instrumented with **only a few added lines** (marked `+`below,
from Fig. 9 of paper):

```c
// initialize
ep_fd = epoll_create();
for (ls : listen_fds) {
    event->handler = accept_handler;
    epoll_ctl(ep_fd, EPOLL_CTL_ADD, ls, event);
}

// infinite event loop
while (1) {
+   shm_avail_update(current_time);              // record loop-entry timestamp
    event_num = epoll_wait(ep_fd, event_list, MAX_EVENTS, timer); // timer = 5ms
+   shm_busy_count(event_num);                    // busy += event_num
    for (event : event_list) {
        event->handler(event);
+       shm_busy_count(-1);                       // busy -= 1 per handled event
    }
+   schedule_and_sync();                          // Stage 2, run at END of loop
}

accept_handler() {
    conn_fd = accept();
+   shm_conn_count(1);                            // conn += 1
    event->handler = other_handler;
    epoll_ctl(ep_fd, EPOLL_CTL_ADD, conn_fd, event);
}

other_handler() {
    if (err || fin) {
        epoll_ctl(ep_fd, EPOLL_CTL_DEL, conn_fd, event);
        close(conn_fd);
+       shm_conn_count(-1);                       // conn -= 1
    }
}
```

**Placement of `schedule_and_sync()` is deliberate**: it must run **at the end**
of the loop (after the current event batch is processed), not the beginning —
placing it at the start risks scheduling based on stale "idle" status right
before a burst of events arrives via the upcoming `epoll_wait()`.

**Timer/timeout**: `epoll_wait()` timeout = **5 ms**, guaranteeing every worker
re-enters the loop and runs the scheduler **at least once every 5ms** even with
zero I/O activity (keeps hang-detection and scheduling live under idle
conditions).

## 5. Stage 2 — Scheduler (Algorithm 1)

```
Function schedule_and_sync():
    W ← {w1, ..., wn}
    time, event, conn ← Read_SHM()
    W ← FilterTime(time, W)      // 1st: drop hung/unavailable workers
    W ← FilterCount(conn, W)     // 2nd: drop workers with above-avg conn count
    W ← FilterCount(event, W)    // 3rd: drop workers with above-avg busy count
    SelWorker ← Array2INT(W)     // encode surviving worker set as a bitmap→int
    BPF_MAP_UPDATE(SelWorker, M_Sel)

Function FilterTime(R, W):
    return { wi | currentTime() - R_i < Threshold, wi ∈ W }

Function FilterCount(R, W):
    Avg ← CalculateAverage({ R_i | wi ∈ W })
    return { wi | R_i < Avg + θ, wi ∈ W }
```

- **Cascading / prioritized filter order matters** and is fixed as:
  1. **Time filter** (liveness) — first, since dispatching to a dead worker is the
     worst outcome.
  2. **Connection-count filter** — second; production data showed long-lived
     connections are common, and evenly spreading them matters more for stability
     than instantaneous busy-ness (prevents future synchronized-burst overload).
  3. **Event/busy-count filter** — third (latency optimization, lowest priority).
- **Offset θ**: added to the average in `FilterCount` to avoid over-pruning down
  to too few candidate workers (which would itself cause concentration).
  Empirically tuned: **`θ / Avg = 0.5` gives best P99 latency and throughput**
  (too small → connections concentrate on very few coarse-filtered workers →
  bad latency/throughput; too large → high-load workers get selected anyway →
  degraded performance).
- **Complexity**: O(n) — single pass over workers, no nested loops.
- **Result encoding**: candidate set encoded as a **bitmap** (`1` = selected,
  `0` = not), packed into a single int (e.g. `{1,1,0,0,1}` → workers 1,2,5). A
  raw boolean array would need explicit locking under concurrent writers; the
  **bitmap-as-int** representation lets concurrent scheduler instances (one per
  worker) write via `atomic<int>` with **no locks**.
- **Call frequency**: scales *up* with load (as `epoll_wait()` blocks for shorter
  durations under heavier traffic, the loop — and thus the scheduler — runs more
  often). This is a **desired property**: higher load needs faster-refreshing
  scheduling decisions. Can reach **~20k scheduler calls/sec under heavy
  workload**.
- **Ratio of workers passing the coarse filter** *decreases* as load increases
  (more workers become "busy" and get filtered out).

## 6. Stage 3 — eBPF Connection Dispatch (Algorithm 2)

```
input: eBPF map M_Sel (userspace-selected worker bitmap, int),
       eBPF map M_socket (worker-ID → socket mapping)

Function conn_dispatch_socket_select(M_Sel, M_socket):
    C ← bpf_map_lookup_elem(M_Sel)
    n ← CountNonZeroBits(C)                       // Hamming weight of bitmap
    if n > 1:
        N_th ← reciprocal_scale(4tuple.hash, n)   // scale conn hash into [1, n]
        ID ← FindNthNonZeroBit(C, N_th)           // locate the Nth set bit → worker ID
        return bpf_sk_select_reuseport(M_socket, ID)
    // else: fall through — kernel uses default reuseport hashing (already initialized)
```

### eBPF maps
| Map | Type | Purpose |
|---|---|---|
| `M_Sel` | `BPF_MAP_TYPE_ARRAY`, single int element | Carries the userspace-computed candidate-worker bitmap (int-encoded). eBPF array maps natively support atomic int r/w → no locks needed on either the userspace-write or kernel-read side. |
| `M_socket` | `BPF_MAP_TYPE_REUSEPORT_SOCKARRAY` | Maps worker ID → underlying socket. Populated once during Hermes program initialization. |

### Kernel hook
- Attach point: **`SO_ATTACH_REUSEPORT_EBPF`** socket option (available since
  Linux 4.5), which lets an eBPF program **override the default hash-based
  reuseport socket selection** for a `SO_REUSEPORT` group. Requires
  `SO_REUSEPORT` already enabled with each worker owning its own dedicated
  listening socket bound to the shared port.
- Final worker selection communicated to the kernel via
  **`bpf_sk_select_reuseport(M_socket, ID)`** helper.
- **Fallback rule**: if the coarse-grained candidate count `n ≤ 1` (i.e.
  userspace scheduler could not confidently select >1 available worker — e.g.
  during a lag between scheduler runs or degenerate cases), the kernel silently
  falls back to **plain reuseport hashing** (the default mechanism, always kept
  initialized as a safety net).
- **Two-stage filtering rationale**: new-connection arrival rate can reach
  **O(100K)/s**, far higher than the combined scheduler update frequency. If
  userspace passed only a *single* "best" worker at a time, the kernel would
  dispatch **all** new connections to that one worker until the next update →
  overload. Hence userspace does **coarse-grained** filtering (multiple
  candidates) and the kernel does **fine-grained** per-connection selection
  (hash-based pick among candidates) to spread load between scheduler updates.

## 7. Overhead Profile (for validation/sanity-checking a Rust port)

Measured via CPU flamegraph, total overhead **0.674%–2.436%** CPU utilization
depending on load:

| Component | Light | Medium | Heavy |
|---|---|---|---|
| Userspace: counter (shm_*_count updates, atomic<int>) | 0.122% | 0.412% | 0.897% |
| Userspace: scheduler (`schedule_and_sync`) | 0.272% | 0.381% | 0.531% |
| Userspace: system call (eBPF map update syscalls) | 0.275% | 0.590% | 0.965% |
| Kernel: dispatcher (eBPF bitwise ops) | 0.005% | 0.019% | 0.043% |

- Dispatcher (kernel eBPF path) is by far the cheapest component (pure bitwise
  ops). Counter overhead grows with connection count (atomic increments). System
  call overhead (updating eBPF maps: syscall + context switch) is actually the
  **largest** single contributor under heavy load.
- Sustained heavy load is rare in practice (LBs scale out proactively); overhead
  is below 1% most of the time.

## 8. Exception Handling

### Case 1 — Single worker hangs
Root cause modeled: edge-triggered epoll requires a worker to **fully drain** a
socket's buffer once notified, or it misses future events. If downstream
processing (SSL, compression) is slower than upstream data arrival, worker gets
stuck in a read/process loop and never returns to the main event loop /
`schedule_and_sync()` — starving both new and existing connections assigned to
it. (Observed real incident: latency 30ms → 440s.)

Mitigations:
- **New connections**: the existing `time`-based hang detection (Stage 1) marks
  the worker unavailable and Stage 2 stops selecting it. Contrast: epoll
  exclusive also naturally avoids assigning to a busy worker; **reuseport does
  not** — its stateless hash keeps sending new connections to the hung worker,
  making things worse.
- **Existing connections** (cannot be migrated across workers due to
  one-worker-per-CPU-core pinning / connection affinity): Hermes proactively
  sends **TCP RST** to terminate a subset of the connections pinned to an
  overloaded/hung core, forcing clients to reconnect and get rescheduled to a
  healthy worker by the normal dispatch path. Accepted trade-off: L7 clients
  care more about eventual request success than raw L4 connection stability.
  Tenants that **repeatedly** trigger hangs get migrated to an isolated sandbox
  VM group (physical isolation to limit blast radius to other tenants).

### Case 2 — All workers hang / cluster-wide CPU saturation
Node-local scheduling becomes ineffective; escalate to cluster-wide handling.
- **Malicious traffic (SYN flood / CC attack)**: anomaly detection identifies the
  offending tenant's traffic pattern, migrates that tenant to an isolated
  sandbox, freeing up the original workers.
- **Legitimate traffic surge** (VMs deployed via *shuffle-sharded* groups —
  each tenant instance lives on a subset of VMs): **3-phase progressive
  scaling**:
  1. **Phase 1 — scale-out**: redistribute the overloaded instance's traffic
     across other **existing** VM groups first.
  2. **Phase 2 — scale-up**: if Phase 1 insufficient, add more VMs to the
     existing groups.
  3. **Phase 3 — provision**: if still insufficient, provision brand-new VMs /
     new groups to absorb the overflow.

## 9. Known Production Pitfalls (relevant to a faithful reimplementation)

1. **Round-robin backend restart skew**: When a tenant's backend server list is
   updated (scale-out/in), every worker independently **restarts round-robin
   distribution from the first server in the updated list**. Under epoll
   exclusive, one worker handled the bulk of traffic, so RR self-balanced
   naturally over a large request volume. Under Hermes, traffic is evenly spread
   across *all* workers, so each handles fewer requests — after a list update,
   all workers restart RR from the same point simultaneously, disproportionately
   hammering the first few backend servers. **Fix**: randomize each worker's RR
   starting offset after every backend-list update.
2. **Reduced backend connection reuse**: Epoll exclusive funneled most traffic
   through a few workers → high per-worker connection-pool reuse to backends.
   Hermes's even spread means each worker reuses backend connections less,
   which matters especially for costly long-distance TCP/TLS handshakes to
   on-prem backends. **Fix**: use a backend connection pool **shared across
   workers** instead of one pool per worker.
3. **Worker-crash blast radius**: reuseport can keep routing to a crashed
   worker for the full failure-detection window (production: "tens of
   seconds"), impacting ~1/N of traffic (N = worker count) during that window.
   Epoll exclusive concentrates load onto few workers, so a crash there is
   worse (observed incident: single worker crash on HTTP/2→WebSocket upgrade
   edge case forced re-establishment of >70% of connections, tens of minutes to
   recover). Hermes avoids both failure modes by balancing load across all
   workers *and* using timestamp-based hang detection to quickly route around
   an unresponsive worker.
4. **Multi-tenancy / many-ports does not fix epoll-exclusive's imbalance**:
   tempting idea — statically pin different "last-added" workers per port to
   scatter load — fails in practice because (a) which ports get bursty traffic
   is unpredictable over time and #ports (O(10K)) ≫ #workers (O(10)), so
   collisions on the same "last-added" worker are near-certain; (b) tenant
   traffic is heavily skewed (top 3 tenants observed at 40%/28%/22% and
   23%/10%/4% of regional traffic in two regions) so a few dominant tenants
   will concentrate load regardless of port-to-worker static assignment. This
   is why Hermes's **dynamic, userspace-status-driven** scheduling is necessary
   rather than any static/hash-based scheme.

## 10. Expected Qualitative Behavior vs. Baselines (for validation)

Four traffic-pattern cases, each characterized by Connections-Per-Second (CPS)
and average per-connection processing time at the LB:

| Case | Traffic profile | Winner ranking |
|---|---|---|
| 1 | High CPS, low avg processing time (stress test / traffic spikes) | Reuseport ≳ Hermes > Epoll exclusive. Exclusive: idle workers + LIFO wakeup → concentration; dispatch overhead is O(1) for Hermes/reuseport vs O(#ports) for exclusive (exclusive registers all ports on one epoll instance; Hermes/reuseport give each worker's epoll instance a single port). Hermes best under heavy load specifically. |
| 2 | High CPS, high avg processing time (e.g. compression-heavy) | Hermes > Epoll exclusive > Reuseport. High processing time keeps workers busy/hung; Hermes actively avoids dispatching to busy/hung workers. Reuseport's stateless hash keeps queuing onto already-overloaded workers. Exclusive degrades rapidly under heavy load. |
| 3 | Low CPS, low avg processing time (long-lived connections — finance/chat apps) | Hermes ≈ Reuseport > Epoll exclusive (badly). Exclusive's LIFO wakeup concentrates long-lived connections on a few workers → overload on subsequent bursts. Hermes shows more balanced distribution than reuseport under heavy load specifically, due to userspace awareness. |
| 4 | Low CPS, high avg processing time (e.g. web services w/ SSL handshake + regex routing) | Hermes ≈ Epoll exclusive > Reuseport (badly). Established expensive connections can't be migrated; reuseport's stateless hash keeps queuing new connections onto already-overloaded workers. Hermes has slightly higher delay than exclusive under high load since its closed-loop detection of unavailable workers has some lag vs exclusive's immediate promptness. |

Net takeaway used for validating a reimplementation: **no single existing
mechanism (exclusive or reuseport) wins in all four cases; Hermes should track
close to the best performer in every case** (this adaptability is Hermes's core
value proposition — verify this property, not just raw throughput numbers,
when validating a Rust port).

Aggregate production comparison (2-day sample): standard deviation of
per-worker CPU utilization — exclusive 26%, reuseport 2.7%, Hermes 2.7%.
Standard deviation of per-worker connection counts — exclusive 3200, reuseport
50, **Hermes 20** (best — because Hermes actively selects for low connection
count, whereas reuseport's hash-balance is degraded in practice by connections
of uneven/varying duration).