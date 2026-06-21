use std::sync::atomic::AtomicI64;

/// Number of simulated worker processes. The real Hermes paper runs O(10)
/// workers per L7 LB device, one pinned per CPU core (§2.1). We keep this
/// small for local development; it can become a CLI option later.
pub const NUM_WORKERS: usize = 4;

/// Per-worker status, matching the three metrics Hermes selects in §5.2.1:
/// timestamp of entering the event loop, pending event count, and
/// accumulated connection count (Fig. 10 labels these Time / Event / Conn).
///
/// Every field is an independent `AtomicI64` so a worker can update its
/// own slot, and the embedded scheduler in every worker can read every
/// slot, without either side ever taking a lock (§5.3.1). The paper's
/// argument for why this is safe: a read racing with a write only ever
/// produces a status reading that's a few nanoseconds stale, which has
/// negligible effect on a scheduling decision.
#[repr(C)]
pub struct WorkerSlot {
    pub last_loop_entry: AtomicI64,
    pub pending_events: AtomicI64,
    pub accumulated_conns: AtomicI64,
    /// Padding so each slot occupies a full 64-byte cache line. This is
    /// NOT in the original paper -- it's a deliberate addition on our
    /// part to stop two neighbouring workers' hot counters from sharing
    /// a cache line and causing false-sharing contention on multicore
    /// hardware. Worth a sentence of justification in your Design or
    /// Implementation chapter as your own contribution.
    _pad: [u8; 64 - 3 * std::mem::size_of::<i64>()],
}

impl WorkerSlot {
    /// Not called by `main.rs` right now (we rely on zero-filled mmap
    /// memory instead -- see the comment on `Wst` construction there),
    /// but kept here so the zero-is-valid assumption is explicit and
    /// documented rather than implicit.
    #[allow(dead_code)]
    pub const fn new() -> Self {
        Self {
            last_loop_entry: AtomicI64::new(0),
            pending_events: AtomicI64::new(0),
            accumulated_conns: AtomicI64::new(0),
            _pad: [0u8; 64 - 3 * std::mem::size_of::<i64>()],
        }
    }
}

/// The Worker Status Table: one `WorkerSlot` per worker, laid out
/// contiguously. The whole struct is intended to live in memory mapped
/// `MAP_SHARED`, so every worker process maps the *same physical pages*
/// and sees each other's writes immediately (§4.1, Stage 1). Note there
/// is no eBPF map and no kernel involvement anywhere in this struct --
/// that only shows up later, for the much smaller scheduling-result
/// bitmap (see scheduler.rs).
#[repr(C)]
pub struct Wst {
    pub slots: [WorkerSlot; NUM_WORKERS],
}

impl Wst {
    pub fn slot(&self, worker_id: usize) -> &WorkerSlot {
        &self.slots[worker_id]
    }
}

/// Monotonic clock, shared across all processes on this machine. Unlike
/// wall-clock time it can't jump backwards (e.g. due to NTP correction),
/// which matters here because we're comparing timestamps written by
/// different processes against `now()` read by yet another process.
pub fn now_monotonic_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}
