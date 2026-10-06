// SPDX-License-Identifier: BSD-2-Clause
//! The virtual address space layout.
//!
//! These constants are the executable form of docs/address-space.md, and
//! they are `const` on purpose: there is no KASLR, so nothing here needs to
//! be a runtime variable. See ADR 009 for the reasoning behind each range
//! and, in particular, for why the user/kernel boundary is permanent.

use super::{VirtAddr, FRAME_SIZE};

/// Lowest valid user address.
///
/// The first 64 KiB is never mapped so that a null dereference faults --
/// including `(*p).field` with a small offset, which a single guard page
/// would miss.
pub const USER_MIN: u64 = 0x0000_0000_0001_0000;

/// Highest valid user address: the last address of the lower canonical half.
pub const USER_MAX: u64 = 0x0000_7FFF_FFFF_FFFF;

/// First address of the upper canonical half, and therefore the
/// user/kernel boundary.
///
/// Fixed permanently as of ADR 009: M8's user space, the syscall ABI and
/// the capability model are all written against it. "Is this a user
/// address?" is one comparison because the split is exactly in half.
pub const KERNEL_MIN: u64 = 0xFFFF_8000_0000_0000;

/// Base of the physical memory window.
///
/// Deliberately equal to the address Limine places its HHDM at, so that
/// switching to our own tables invalidates no previously computed pointer.
/// `vm::init` asserts that Limine agreed.
pub const PMAP_BASE: u64 = 0xFFFF_8000_0000_0000;

/// Size of the physical window: 64 TiB, more than 4-level paging can
/// address physically.
pub const PMAP_SIZE: u64 = 64 * TIB;

/// Base of the kernel heap area. Reserved in M3, used from M4.
///
/// The heap itself does not start here: the first page of the area is an
/// unmapped guard, so a pointer that walks off the bottom faults instead
/// of landing in whatever precedes the heap. See [`KHEAP_START`].
pub const KHEAP_BASE: u64 = 0xFFFF_C000_0000_0000;

/// First byte the heap may actually hand out.
///
/// One page above [`KHEAP_BASE`], leaving a guard below. Underflow in
/// pointer arithmetic is the failure this catches -- `ptr.sub(n)` past the
/// start of the first block.
pub const KHEAP_START: u64 = KHEAP_BASE + FRAME_SIZE;

/// Largest the heap may grow to, in bytes.
///
/// 64 MiB. Chosen as a limit, not a reservation: pages are mapped on
/// demand and a kernel that needs more than this has a leak rather than a
/// requirement. The bound exists so that a runaway allocation loop ends in
/// a legible panic ("the heap hit its 64 MiB limit") instead of quietly
/// consuming every frame on the machine and failing somewhere unrelated.
///
/// It also keeps the heap comfortably inside the 8 TiB KHEAP range with
/// room for the guard pages at both ends.
pub const KHEAP_MAX_SIZE: u64 = 64 * 1024 * 1024;

/// One past the last byte the heap may use.
pub const KHEAP_END: u64 = KHEAP_START + KHEAP_MAX_SIZE;

/// Start of the unmapped page above the heap's maximum extent.
///
/// The heap grows upwards, so this is where an overrun lands. Unlike the
/// lower guard, this one only becomes reachable once the heap has actually
/// grown to its limit -- but it is never mapped, so the protection does
/// not depend on how far the heap has grown.
pub const KHEAP_GUARD_TOP: u64 = KHEAP_END;

/// Base of the kernel stack area.
pub const KSTACK_BASE: u64 = 0xFFFF_C800_0000_0000;

/// Base of the MMIO area. Reserved in M3, used from M6.
pub const KMMIO_BASE: u64 = 0xFFFF_D000_0000_0000;

/// Base of the kernel image, as fixed by linker.ld and required by
/// `-C code-model=kernel`.
pub const KIMAGE_BASE: u64 = 0xFFFF_FFFF_8000_0000;

const KIB: u64 = 1024;
const TIB: u64 = 1024 * 1024 * 1024 * 1024;

// ---- Kernel stacks -----------------------------------------------------

/// Usable bytes in a kernel stack.
///
/// 16 KiB. Deep enough for the exception path and `core::fmt`, which is not
/// frugal; small enough that a per-thread stack costs little once there are
/// many threads.
pub const KSTACK_SIZE: u64 = 16 * KIB;

/// Virtual address space consumed by one stack: the stack itself plus an
/// unmapped guard page below it.
///
/// Every kernel stack gets a guard page, without exception (ADR 009 and the
/// M3 decision list). Stacks grow downwards, so an overflow runs off the
/// bottom into the guard and faults, instead of quietly overwriting
/// whatever is underneath -- which is exactly what happened in M1, where
/// the IST stack sat at the start of `.bss` directly above the Limine
/// response structures.
pub const KSTACK_SLOT: u64 = KSTACK_SIZE + FRAME_SIZE;

/// Address range of kernel stack slot `index`, as
/// `(guard_page_start, stack_bottom, stack_top)`.
///
/// Layout of one slot, lowest address first:
///
/// ```text
///   guard_start  +------------------+  unmapped: an overflow lands here
///                |   guard page     |
///   bottom       +------------------+  first mapped byte
///                |                  |
///                |   16 KiB stack   |  grows downwards, towards the guard
///                |                  |
///   top          +------------------+  one past the last byte; RSP starts here
/// ```
pub const fn kstack_slot(index: u64) -> (VirtAddr, VirtAddr, VirtAddr) {
    let guard_start = KSTACK_BASE + index * KSTACK_SLOT;
    let bottom = guard_start + FRAME_SIZE;
    let top = bottom + KSTACK_SIZE;
    (
        VirtAddr::new(guard_start),
        VirtAddr::new(bottom),
        VirtAddr::new(top),
    )
}

/// Stack slot used for IST1, the double-fault stack.
pub const KSTACK_SLOT_IST1: u64 = 0;

/// Stack slot used for the kernel's main stack, the one `kmain` runs on.
///
/// Limine's stack is not usable past `bootinfo::reclaim`: the protocol
/// places it in `BOOTLOADER_RECLAIMABLE` memory, so reclaiming that memory
/// pulls the stack out from under the running code. This slot is where the
/// kernel moves before reclaim happens. See ADR 013.
pub const KSTACK_SLOT_MAIN: u64 = 1;

// ---- Predicates --------------------------------------------------------

/// Whether an address is canonical under 4-level paging.
///
/// Bits 63:48 must all equal bit 47. A non-canonical access raises #GP, not
/// #PF -- the check happens before the page walk -- which is why the paging
/// API reports it as a distinct error.
///
/// Reference: Intel SDM Vol. 1, 3.3.7.1 "Canonical Addressing".
pub const fn is_canonical(addr: u64) -> bool {
    // Sign-extend bit 47 across the top and see if we get the same value
    // back. Arithmetic shift on a signed type does the extension for us.
    let shifted = ((addr as i64) << 16) >> 16;
    shifted as u64 == addr
}

/// Whether an address lies in the user half.
pub const fn is_user(addr: u64) -> bool {
    addr <= USER_MAX
}

/// Whether an address lies inside the physical memory window.
pub const fn is_pmap(addr: u64) -> bool {
    addr >= PMAP_BASE && addr < PMAP_BASE + PMAP_SIZE
}

// ---- Compile-time checks ----------------------------------------------
//
// The layout is a set of constants, which means a careless edit can make
// two regions overlap with nothing to notice. These turn that into a build
// failure.

const _: () = {
    assert!(is_canonical(USER_MIN), "USER_MIN is not canonical");
    assert!(is_canonical(USER_MAX), "USER_MAX is not canonical");
    assert!(is_canonical(KERNEL_MIN), "KERNEL_MIN is not canonical");
    assert!(is_canonical(KIMAGE_BASE), "KIMAGE_BASE is not canonical");

    assert!(USER_MAX < KERNEL_MIN, "the halves overlap");
    assert!(PMAP_BASE >= KERNEL_MIN, "PMAP must be in the kernel half");
    assert!(
        PMAP_BASE + PMAP_SIZE <= KHEAP_BASE,
        "PMAP runs into the kernel heap"
    );
    assert!(KHEAP_BASE < KSTACK_BASE, "heap and stacks are out of order");
    assert!(
        KHEAP_GUARD_TOP + FRAME_SIZE <= KSTACK_BASE,
        "the heap's upper guard page runs into the kernel stack area"
    );
    assert!(
        KHEAP_MAX_SIZE.is_multiple_of(FRAME_SIZE),
        "the heap limit must be a whole number of pages"
    );
    assert!(KSTACK_BASE < KMMIO_BASE, "stacks and MMIO are out of order");
    assert!(KMMIO_BASE < KIMAGE_BASE, "MMIO runs into the kernel image");

    assert!(
        KSTACK_SIZE.is_multiple_of(FRAME_SIZE),
        "kernel stacks must be a whole number of pages"
    );
};
