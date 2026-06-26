/// Allocates the WST in shared anonymous memory via mmap, forks one child process per worker,
/// waits for all to finish. There is currently no eBPF or real TCP at this stage

mod scheduler;
mod worker;
mod wst;

use std::mem::size_of;
use std::ptr;
use wst::{Wst, NUM_WORKERS};

const ITERATIONS_PER_WORKER: u32 = 25;

fn main() {
    // Map the Worker Status Table (WST) in shared anonymous memory before forking. 
    // This ensures all child processes share the same physical memory without 
    // needing an external eBPF map at this stage` (§4.1)`
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

    // Anonymous memory is zero-filled by default. Since a zeroed i64 
    // represents a valid initial state for our atomics, we don't need a constructor.
    let wst: &'static Wst = unsafe { &*(map_ptr as *const Wst) };

    let mut child_pids = Vec::with_capacity(NUM_WORKERS);
    for worker_id in 0..NUM_WORKERS {
        let pid = unsafe { libc::fork() };
        match pid {
            -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),
            0 => {
                // Child process: run the worker loop and exit immediately 
                // to prevent executing parent logic.
                worker::worker_loop(wst, worker_id, ITERATIONS_PER_WORKER);
                std::process::exit(0);
            }
            child_pid => child_pids.push(child_pid),
        }
    }

    // Parent process: block until all child workers finish.
    for pid in child_pids {
        let mut status = 0;
        unsafe {
            libc::waitpid(pid, &mut status, 0);
        }
    }
    println!("all workers finished");
}