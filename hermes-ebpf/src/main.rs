//! Algorithm 2 (§5.4), kernel-side connection dispatch. Attached to the
//! worker group's SO_REUSEPORT sockets via SO_ATTACH_REUSEPORT_EBPF and
//! run once per inbound connection in place of the kernel's default
//! stateless-hash socket pick.
//!
//! Returning SK_PASS without selecting a socket makes the kernel run its
//! normal hash selection, which implements the paper's n <= 1 fallback
//! rule with no extra code

#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::sk_action::SK_PASS,
    macros::{map, sk_reuseport},
    maps::{Array, ReusePortSockArray},
    programs::SkReuseportContext,
};
use hermes_common::NUM_WORKERS;

const IPPROTO_TCP: u32 = 6;

/// M_Sel, the userspace-computed candidate bitmap. Bit i set means worker
/// i survived Algorithm 1. Concurrent writers are safe because array-map
/// element writes are atomic and only the most recent value matters
#[map(name = "m_sel")]
static M_SEL: Array<u64> = Array::pinned(1, 0);

/// M_socket, worker id -> listening socket. Populated once by the loader
/// at startup and never written again
#[map(name = "m_socket")]
static M_SOCKET: ReusePortSockArray = ReusePortSockArray::pinned(NUM_WORKERS as u32, 0);

#[sk_reuseport]
pub fn hermes_select(ctx: SkReuseportContext) -> u32 {
    match try_select(&ctx) {
        Some(worker_id) => match M_SOCKET.select_reuseport(&ctx, worker_id) {
            Ok(()) => SK_PASS,
            // Socket missing from the map (startup/shutdown race). Fail
            // open to the default hash rather than dropping the connection
            Err(_) => SK_PASS,
        },
        // n <= 1 candidates or non-TCP traffic, let the kernel's default
        // hash pick
        None => SK_PASS,
    }
}

/// Algorithm 2 minus the final select_reuseport call. Returns the chosen
/// worker id, or None to fall back to the kernel's default hash
#[inline(always)]
fn try_select(ctx: &SkReuseportContext) -> Option<u32> {
    if ctx.ip_protocol() != IPPROTO_TCP {
        return None;
    }
    let bitmap = *M_SEL.get(0)?;
    let n = bitmap.count_ones();
    if n <= 1 {
        return None;
    }
    let nth = reciprocal_scale(ctx.hash(), n);
    find_nth_set_bit(bitmap, nth)
}

/// The kernel's reciprocal_scale(), maps a 32-bit hash uniformly into
/// [0, n) with one multiply and one shift, no division by a runtime value
/// (which the verifier forbids)
#[inline(always)]
fn reciprocal_scale(hash: u32, n: u32) -> u32 {
    (((hash as u64) * n as u64) >> 32) as u32
}

/// Position of the nth (0-based) set bit. NUM_WORKERS is a small constant
/// so LLVM fully unrolls this, which keeps the verifier happy
#[inline(always)]
fn find_nth_set_bit(bitmap: u64, nth: u32) -> Option<u32> {
    let mut seen = 0u32;
    for i in 0..NUM_WORKERS as u32 {
        if bitmap & (1u64 << i) != 0 {
            if seen == nth {
                return Some(i);
            }
            seen += 1;
        }
    }
    None
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[unsafe(link_section = "license")]
#[unsafe(no_mangle)]
static LICENSE: [u8; 13] = *b"Dual MIT/GPL\0";
