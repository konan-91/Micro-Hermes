mod scheduler;
mod worker;
mod wst;

use std::mem::size_of;
use std::ptr;
use wst::{Wst, NUM_WORKERS};

const ITERATIONS_PER_WORKER: u32 = 25;

fn main() {
    // Map the WST in shared, anonymous memory *before* forking. Linux
    // guarantees a MAP_SHARED|MAP_ANONYMOUS mapping created before fork()
    // stays backed by the same physical pages in every child process --
    // this is the entire "shared memory region" of §4.1, with no named
    // /dev/shm object and no eBPF map involved. That distinction matters:
    // it's easy to assume the WST itself must be a BPF map (I made that
    // mistake before reading the paper closely), but the kernel never
    // touches it at all.
    let size = size_of::<Wst>();
    let map_ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if map_ptr == libc::MAP_FAILED {
        panic!("mmap failed: {}", std::io::Error::last_os_error());
    }

    // MAP_ANONYMOUS memory from the kernel comes back zero-filled, and
    // zero is a valid bit pattern for every field in `Wst` (an
    // `AtomicI64::new(0)` has the same in-memory representation as a
    // zeroed i64), so this memory is already a valid `Wst` -- we don't
    // need to run a constructor over it.
    let wst: &'static Wst = unsafe { &*(map_ptr as *const Wst) };

    let mut child_pids = Vec::with_capacity(NUM_WORKERS);
    for worker_id in 0..NUM_WORKERS {
        let pid = unsafe { libc::fork() };
        match pid {
            -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),
            0 => {
                // Child: become this worker, then exit -- never return
                // from main() and risk re-running parent-only logic.
                worker::worker_loop(wst, worker_id, ITERATIONS_PER_WORKER);
                std::process::exit(0);
            }
            child_pid => child_pids.push(child_pid),
        }
    }

    // Parent: wait for every worker to finish its run.
    for pid in child_pids {
        let mut status = 0;
        unsafe {
            libc::waitpid(pid, &mut status, 0);
        }
    }
    println!("all workers finished");
}
