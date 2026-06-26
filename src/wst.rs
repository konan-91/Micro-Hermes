/// Defines the Worker Status Table: a flat array of 64-byte WorkerSlot structs,
/// each holding three AtomicI64 metrics (loop timestamp, pending events, connections).
/// Utilises shared memory so that all workers can read/write to the same table, since
/// each worker needs to run the scheduler itself (separate scheduler architecutre was
/// rejected as it uses a separate CPU core which is expensive at enterprise scale). 

use std::sync::atomic::AtomicI64;

// Number of simulated worker processes. 
// The Hermes paper scales this to O(10) workers per L7 LB device, 
// pinning one per CPU core (§2.1). Kept small here for local testing.
pub const NUM_WORKERS: usize = 4;

// Tracks per-worker status using the three metrics defined in Hermes §5.2.1 
// and Figure 10: Time, Event, and Conn.
// 
// Lock-free design (§5.3.1): Each field is an independent AtomicI64. 
// Workers update their own slots, and the scheduler reads them without locks. 
// A race condition only results in slightly stale data, which has a negligible 
// impact on scheduling decisions.
#[repr(C)] // Fix struct layout and ensure blocks placed one after the other.
pub struct WorkerSlot {
    pub last_loop_entry: AtomicI64,
    pub pending_events: AtomicI64,
    pub accumulated_conns: AtomicI64,

    // Pad the struct to exactly 64 bytes to match the CPU cache line size.
    // Without this, the memory layout would be contiguous and multiple
    // WorkerSlot elements would share the same cache line. Concurrent
    // updates to these neighboring slots would cause false sharing,
    // where cores redundantly invalidate each other's cache lines and
    // force expensive reloads from RAM (now one line per struct).
    _pad: [u8; 64 - 3 * std::mem::size_of::<i64>()],
}

impl WorkerSlot {
    // This function is not explicitly called because our shared memory (via mmap) 
    // is automatically zero-initialized by the operating system. It is kept here 
    // to explicitly document that a zero-filled memory block is the correct, 
    // valid starting state for this struct.
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

// The Worker Status Table (WST) from Hermes §4.1.
// This structure holds the array of padded worker slots. It is mapped into 
// shared memory (MAP_SHARED) so that separate, isolated worker processes 
// can read and write to the exact same physical memory space without locks.
// Note: This table is purely for userspace metrics; the Linux kernel and 
// eBPF network maps do not interact with this structure.
#[repr(C)]
pub struct Wst {
    pub slots: [WorkerSlot; NUM_WORKERS],
}

impl Wst {
    // Helper function to easily look up a specific worker's slot by its ID.
    pub fn slot(&self, worker_id: usize) -> &WorkerSlot {
        &self.slots[worker_id]
    }
}

// Returns the current system time in nanoseconds using a monotonic clock.
// Monotonic clock starts at system boot and can never jump backwards,
// unlike network clocks, which guarantees safe & accurate time intervals 
// when comparing timestamps. 
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
