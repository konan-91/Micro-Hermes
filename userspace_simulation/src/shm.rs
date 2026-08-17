//! Shared-memory state, everything that crosses the process boundary.
//! One SharedState is mapped with mmap(MAP_SHARED | MAP_ANONYMOUS) before
//! forking, so the parent and all workers operate on the same physical
//! pages. In phase 2 the WST stays as-is, msel becomes the M_Sel eBPF
//! map, and the queues are replaced by real sockets

use crate::wst::{Wst, NUM_WORKERS};
use std::cell::UnsafeCell;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Capacity of each simulated accept queue (the kernel's listen backlog)
pub const QUEUE_CAP: usize = 512;

/// Simulated M_Sel map, a single u64 element holding the candidate
/// bitmap. eBPF array map elements support atomic reads and writes, and
/// an AtomicU64 gives the identical guarantee here
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

/// One synthetic connection as handed from the dispatcher to a worker,
/// standing in for what accept() plus the first read would yield
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct ConnDesc {
    /// Monotonically increasing connection ID (for metrics correlation)
    pub conn_id: u64,
    /// Synthetic 4-tuple hash (the kernel precomputes this for reuseport)
    pub hash: u64,
    /// Monotonic timestamp when the connection "arrived" (SYN completed)
    pub arrival_ns: i64,
    /// Simulated L7 processing cost in microseconds. A property of the
    /// connection (SSL, compression, payload size), not the worker
    pub service_us: u32,
    /// How long the connection stays open after processing, in
    /// milliseconds (models long-lived vs short-lived connections)
    pub lifetime_ms: u32,
}

/// Lock-free SPSC ring buffer in shared memory, the simulated per-worker
/// accept queue. Single producer (the generator), single consumer (the
/// owning worker), so acquire/release on the indices is sufficient
#[repr(C)]
pub struct ConnQueue {
    /// Producer-owned write index, wraps via %
    tail: AtomicU32,
    _pad1: [u8; 60],
    /// Consumer-owned read index
    head: AtomicU32,
    _pad2: [u8; 60],
    slots: [UnsafeCell<ConnDesc>; QUEUE_CAP],
}

// Safe under SPSC discipline, slots are published with release stores and
// read after acquire loads, so no slot is ever accessed concurrently
unsafe impl Sync for ConnQueue {}

impl ConnQueue {
    /// Producer side, returns false when full (accept queue overflow)
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

    /// Consumer side
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

    /// Current depth. Racy by nature but fine for the dispatcher's
    /// heuristics and metrics
    pub fn len(&self) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Relaxed);
        tail.wrapping_sub(head) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Everything shared between the generator process and the workers
#[repr(C)]
pub struct SharedState {
    pub wst: Wst,
    pub msel: SelMap,
    /// Set to 1 by the generator once all traffic has been sent
    pub shutdown: AtomicU32,
    /// Per-worker count of connections dropped due to a full accept queue
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

/// Map a zeroed SharedState into shared anonymous memory. After fork()
/// parent and children share the same physical pages, and the zero-filled
/// pages are a valid initial state for every field
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
    // Safe, the mapping is zeroed, page-aligned and never unmapped while
    // children run
    unsafe { &*(ptr as *const SharedState) }
}

#[cfg(test)]
pub fn test_state() -> Box<SharedState> {
    // all-zero bytes are a valid SharedState (see mmap_shared_state)
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
        // Cycle more entries than the capacity to exercise index wrapping
        for i in 0..(QUEUE_CAP * 3) {
            assert!(q.push(ConnDesc { conn_id: i as u64, ..desc }));
            assert_eq!(q.pop().unwrap().conn_id, i as u64);
        }
        assert!(q.is_empty());
    }
}
