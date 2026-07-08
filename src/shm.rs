//! Shared-memory state — everything that crosses the process boundary.
//!
//! One `SharedState` struct is mapped with `mmap(MAP_SHARED | MAP_ANONYMOUS)`
//! before forking, so the parent (load generator / simulated kernel) and all
//! worker children operate on the same physical pages.
//!
//! Phase 2 mapping of each field:
//!   • `wst`      — stays exactly as-is (userspace shared memory, §4.1).
//!   • `msel`     — becomes the `M_Sel` eBPF array map (§5.4); the API of
//!                  `SelMap` (store/load one u64) deliberately mirrors a
//!                  single-element `BPF_MAP_TYPE_ARRAY` so only the internals
//!                  of `SelMap` change (atomic store → bpf map update syscall).
//!   • `queues`   — Phase-1 only: stands in for the kernel's per-socket
//!                  accept queues. Replaced by real SO_REUSEPORT sockets.
//!   • `shutdown` — Phase-1 only test harness plumbing.

use crate::wst::{Wst, NUM_WORKERS};
use std::cell::UnsafeCell;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Capacity of each simulated accept queue (kernel: listen backlog).
pub const QUEUE_CAP: usize = 512;

/// Simulated `M_Sel` eBPF map: a single u64 element holding the candidate
/// bitmap (Algorithm 1's `Array2INT` output). eBPF array maps natively
/// support atomic int reads/writes, which is why the paper needs no locks
/// on either side — an `AtomicU64` gives the identical guarantee here.
///
/// Phase 2: replace the atomic with an aya `Array<u64>` map handle; `store`
/// becomes `map.set(0, bitmap, 0)` and `load` moves into the eBPF program.
#[repr(C)]
pub struct SelMap(AtomicU64);

impl SelMap {
    #[inline]
    pub fn store(&self, bitmap: u64) {
        self.0.store(bitmap, Ordering::Relaxed);
    }

    #[inline]
    pub fn load(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// One synthetic connection, as handed from the dispatcher to a worker.
/// Stands in for what `accept()` + the first read would yield.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct ConnDesc {
    /// Monotonically increasing connection ID (for metrics correlation).
    pub conn_id: u64,
    /// Synthetic 4-tuple hash (the kernel precomputes this for reuseport).
    pub hash: u64,
    /// Monotonic timestamp when the connection "arrived" (SYN completed).
    pub arrival_ns: i64,
    /// Simulated L7 processing cost for this connection, in microseconds.
    /// A property of the *connection* (SSL, compression, payload size), not
    /// the worker — matching the paper's per-connection cost variability.
    pub service_us: u32,
    /// How long the connection stays open after processing, in milliseconds
    /// (models long-lived vs short-lived connections; drives `conn -= 1`).
    pub lifetime_ms: u32,
}

/// Lock-free SPSC ring buffer in shared memory — the simulated per-worker
/// accept queue. Single producer (the generator process), single consumer
/// (the owning worker process), so acquire/release on head/tail indices is
/// sufficient with no locks, mirroring the WST's partitioned-writer design.
#[repr(C)]
pub struct ConnQueue {
    /// Producer-owned write index (monotonically increasing, wraps via %).
    tail: AtomicU32,
    _pad1: [u8; 60],
    /// Consumer-owned read index.
    head: AtomicU32,
    _pad2: [u8; 60],
    slots: [UnsafeCell<ConnDesc>; QUEUE_CAP],
}

// Safety: SPSC discipline — the producer only writes a slot before publishing
// it via a Release store of `tail`; the consumer only reads slots after an
// Acquire load of `tail`, and vice versa for `head`. No slot is ever accessed
// concurrently.
unsafe impl Sync for ConnQueue {}

impl ConnQueue {
    /// Producer side. Returns false when full (kernel analogue: accept-queue
    /// overflow → connection dropped).
    pub fn push(&self, desc: ConnDesc) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) as usize >= QUEUE_CAP {
            return false;
        }
        unsafe { *self.slots[tail as usize % QUEUE_CAP].get() = desc };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// Consumer side.
    pub fn pop(&self) -> Option<ConnDesc> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let desc = unsafe { *self.slots[head as usize % QUEUE_CAP].get() };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(desc)
    }

    /// Current depth. Racy by nature (either side may move concurrently) —
    /// fine for the dispatcher's heuristics and metrics.
    pub fn len(&self) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Relaxed);
        tail.wrapping_sub(head) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Everything shared between the generator process and the workers.
#[repr(C)]
pub struct SharedState {
    pub wst: Wst,
    pub msel: SelMap,
    /// Set to 1 by the generator once all traffic has been sent.
    pub shutdown: AtomicU32,
    /// Per-worker count of connections dropped due to a full accept queue.
    pub drops: [AtomicU64; NUM_WORKERS],
    pub queues: [ConnQueue; NUM_WORKERS],
}

impl SharedState {
    pub fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed) == 1
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(1, Ordering::Release);
    }
}

/// Map a zeroed `SharedState` into shared anonymous memory.
///
/// MAP_SHARED | MAP_ANONYMOUS: after fork(), parent and all children share
/// the same physical pages — no IPC, no pipes, just direct atomic reads and
/// writes (§4.1 / §5.3.1). MAP_ANONYMOUS zero-fills the pages, which is a
/// valid initial state for every field (atomics at 0, empty queues).
pub fn mmap_shared_state() -> &'static SharedState {
    let size = size_of::<SharedState>();
    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        panic!("mmap SharedState failed: {}", std::io::Error::last_os_error());
    }
    // Safety: the mapping is zero-initialized, correctly sized/aligned (mmap
    // returns page-aligned memory), and outlives all forked children (the
    // parent waitpids before exit; we never munmap while children run).
    unsafe { &*(ptr as *const SharedState) }
}

#[cfg(test)]
pub fn test_state() -> Box<SharedState> {
    // Safety: all-zero bytes are a valid SharedState (see mmap_shared_state).
    unsafe { Box::new_zeroed().assume_init() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_queue_push_pop_roundtrip() {
        let state = test_state();
        let q = &state.queues[0];
        assert!(q.is_empty());
        let desc = ConnDesc { conn_id: 7, hash: 42, arrival_ns: 1, service_us: 100, lifetime_ms: 5 };
        assert!(q.push(desc));
        assert_eq!(q.len(), 1);
        let popped = q.pop().expect("queue should have one entry");
        assert_eq!(popped.conn_id, 7);
        assert_eq!(popped.hash, 42);
        assert!(q.pop().is_none());
    }

    #[test]
    fn test_queue_full_rejects() {
        let state = test_state();
        let q = &state.queues[1];
        let desc = ConnDesc { conn_id: 0, hash: 0, arrival_ns: 0, service_us: 0, lifetime_ms: 0 };
        for _ in 0..QUEUE_CAP {
            assert!(q.push(desc));
        }
        assert!(!q.push(desc), "push into a full queue must fail (dropped conn)");
        assert_eq!(q.len(), QUEUE_CAP);
    }

    #[test]
    fn test_queue_wraps_indices() {
        let state = test_state();
        let q = &state.queues[2];
        let desc = ConnDesc { conn_id: 1, hash: 0, arrival_ns: 0, service_us: 0, lifetime_ms: 0 };
        // Cycle more entries than the capacity to exercise index wrapping.
        for i in 0..(QUEUE_CAP * 3) {
            assert!(q.push(ConnDesc { conn_id: i as u64, ..desc }));
            assert_eq!(q.pop().unwrap().conn_id, i as u64);
        }
        assert!(q.is_empty());
    }
}
