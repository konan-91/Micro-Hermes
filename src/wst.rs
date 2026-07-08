//! Worker Status Table (§4.1, Fig. 10).
//!
//! Flat array of 64-byte WorkerSlot structs, each holding the three Hermes
//! metrics (loop timestamp, pending events, connection count) as AtomicI64.
//! Mapped into shared anonymous memory via mmap before forking so that every
//! worker process reads and writes the same physical pages without locks.
//!
//! Note: this table is purely for userspace scheduling.  The eBPF maps
//! (MSel, Msocket) that synchronise results to the kernel come in Phase 2.

use std::sync::atomic::{AtomicI64, Ordering};

/// Number of simulated worker processes.
/// Hermes pins one worker per CPU core (§2.1); kept small for local testing.
pub const NUM_WORKERS: usize = 4;

/// Per-worker slot — exactly one CPU cache line (64 bytes).
///
/// Lock-free design (§5.3.1): each worker owns its own slot and is the only
/// writer; the scheduler reads all slots without a lock.  A transient race
/// yields slightly stale data, which has negligible impact on decisions.
#[repr(C)]
pub struct WorkerSlot {
    /// Timestamp (ns, CLOCK_MONOTONIC) of the most recent loop entry.
    /// Written at the top of every epoll iteration (Fig. 9 line 12).
    /// Used by Stage-1 hang detection: if `now - last_loop_entry > threshold`
    /// the worker is considered stuck.
    pub last_loop_entry: AtomicI64,

    /// Number of events currently being processed (pending_events / "busy").
    /// Incremented by the event batch size after epoll_wait (line 14),
    /// decremented by 1 for each processed event (line 18).
    pub pending_events: AtomicI64,

    /// Accumulated (concurrent) connection count ("conn").
    /// Incremented on accept (line 25), decremented on close (line 37).
    pub accumulated_conns: AtomicI64,

    /// Padding to reach exactly 64 bytes, preventing false sharing between
    /// adjacent slots when multiple cores write concurrently.
    _pad: [u8; 64 - 3 * std::mem::size_of::<AtomicI64>()],
}

impl WorkerSlot {
    /// Canonical zero state — matches what mmap MAP_ANONYMOUS gives us.
    /// Kept for documentation; the OS zero-fills the mapping automatically.
    #[allow(dead_code)]
    pub const fn new() -> Self {
        Self {
            last_loop_entry: AtomicI64::new(0),
            pending_events: AtomicI64::new(0),
            accumulated_conns: AtomicI64::new(0),
            _pad: [0u8; 64 - 3 * std::mem::size_of::<AtomicI64>()],
        }
    }

    /// Snapshot all three metrics atomically enough for scheduling purposes.
    /// No cross-field atomicity guarantee is needed — see §5.3.1 argument.
    pub fn snapshot(&self) -> WorkerSnapshot {
        WorkerSnapshot {
            last_loop_entry: self.last_loop_entry.load(Ordering::Relaxed),
            pending_events: self.pending_events.load(Ordering::Relaxed),
            accumulated_conns: self.accumulated_conns.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time copy of a worker's metrics, used by the scheduler.
#[derive(Clone, Copy, Debug)]
pub struct WorkerSnapshot {
    pub last_loop_entry: i64,
    pub pending_events: i64,
    pub accumulated_conns: i64,
}

/// The WST — array of padded worker slots (§4.1).
#[repr(C)]
pub struct Wst {
    pub slots: [WorkerSlot; NUM_WORKERS],
}

impl Wst {
    #[inline]
    pub fn slot(&self, worker_id: usize) -> &WorkerSlot {
        &self.slots[worker_id]
    }

    /// Snapshot the entire table in one pass.  Called by the scheduler at
    /// the end of each epoll iteration (Fig. 9 line 20 → Algorithm 1).
    pub fn snapshot_all(&self) -> [WorkerSnapshot; NUM_WORKERS] {
        std::array::from_fn(|i| self.slots[i].snapshot())
    }
}

/// Current monotonic time in nanoseconds (CLOCK_MONOTONIC).
/// Never jumps backwards, safe for computing intervals.
pub fn now_monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    // Casts kept deliberately: tv_sec/tv_nsec widths differ across targets
    // (this code runs on macOS now and Linux in Phase 2).
    #[allow(clippy::unnecessary_cast)]
    {
        ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
    }
}
