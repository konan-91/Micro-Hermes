//! Worker Status Table (§4.1, Fig. 10). Flat array of 64-byte slots, each
//! holding the three Hermes metrics (loop timestamp, pending events,
//! connection count) as AtomicI64. Mapped into shared anonymous memory
//! before forking so every worker reads and writes the same physical
//! pages without locks

use std::sync::atomic::{AtomicI64, Ordering};

pub use hermes_common::NUM_WORKERS;

/// Per-worker slot, exactly one CPU cache line (64 bytes). Each worker is
/// the only writer of its own slot and the scheduler reads without locks
/// (§5.3.1), a transient race only yields slightly stale data
#[repr(C)]
pub struct WorkerSlot {
    /// CLOCK_MONOTONIC ns of the most recent loop entry, written at the
    /// top of every iteration. Used by Stage 1 hang detection
    pub last_loop_entry: AtomicI64,

    /// Events currently being processed. Incremented by the batch size
    /// after epoll_wait, decremented per handled event
    pub pending_events: AtomicI64,

    /// Open connection count, incremented on accept, decremented on close
    pub accumulated_conns: AtomicI64,

    /// Pad to 64 bytes to avoid false sharing between adjacent slots
    _pad: [u8; 64 - 3 * std::mem::size_of::<AtomicI64>()],
}

impl WorkerSlot {
    /// Snapshot all three metrics. No cross-field atomicity is needed
    /// (§5.3.1)
    pub fn snapshot(&self) -> WorkerSnapshot {
        WorkerSnapshot {
            last_loop_entry: self.last_loop_entry.load(Ordering::Relaxed),
            pending_events: self.pending_events.load(Ordering::Relaxed),
            accumulated_conns: self.accumulated_conns.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time copy of a worker's metrics, used by the scheduler
#[derive(Clone, Copy, Debug)]
pub struct WorkerSnapshot {
    pub last_loop_entry: i64,
    pub pending_events: i64,
    pub accumulated_conns: i64,
}

/// The WST, an array of padded worker slots
#[repr(C)]
pub struct Wst {
    pub slots: [WorkerSlot; NUM_WORKERS],
}

impl Wst {
    #[inline]
    pub fn slot(&self, worker_id: usize) -> &WorkerSlot {
        &self.slots[worker_id]
    }

    /// Snapshot the whole table, called by the scheduler each iteration
    pub fn snapshot_all(&self) -> [WorkerSnapshot; NUM_WORKERS] {
        std::array::from_fn(|i| self.slots[i].snapshot())
    }
}

/// Map a zeroed Wst into shared anonymous memory before fork(). The
/// zero-filled pages are a valid initial state for every field
pub fn mmap_wst() -> &'static Wst {
    let size = std::mem::size_of::<Wst>();
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        panic!("mmap Wst failed: {}", std::io::Error::last_os_error());
    }
    // Safe, the mapping is zeroed, page-aligned and never unmapped while
    // children run
    unsafe { &*(ptr as *const Wst) }
}

/// CLOCK_MONOTONIC in nanoseconds
pub fn now_monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}
