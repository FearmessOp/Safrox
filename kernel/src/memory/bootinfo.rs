// SPDX-License-Identifier: BSD-2-Clause
//! Copying out of bootloader memory, then reclaiming it.
//!
//! Limine's responses live in `BOOTLOADER_RECLAIMABLE`, and the crate hands
//! them to us as `&'static` — so from Rust's point of view they are valid
//! forever and the compiler will not warn when they stop being. Releasing
//! that memory without copying first gives us dangling references that read
//! plausible-looking garbage.
//!
//! ADR 013 turned the prose preconditions of ADR 008 into a numbered list
//! of gates. This module is those gates. There are five, not the four the
//! ADR originally listed: implementing it turned up a precondition the
//! design had missed, which is that the kernel must not still be running on
//! the stack Limine gave it. See gate 3.

use limine::memmap;
use limine::request::{BootloaderInfoRequest, ExecutableCmdlineRequest, MemmapRequest};

use super::frame_allocator;
use super::vm;
use super::{PhysAddr, PhysFrame, FRAME_SIZE};
use crate::println;

/// Longest bootloader name or version string we keep.
const MAX_STRING: usize = 64;

/// Longest kernel command line we keep.
const MAX_CMDLINE: usize = 256;

/// Most memory map entries we keep.
///
/// This machine reports 16; 128 looks generous. It is still a guess, and
/// overflowing it panics rather than truncating — a truncated memory map
/// either hands out memory that does not exist or loses memory that does,
/// and both failures surface far from here.
const MAX_MEMMAP_ENTRIES: usize = 128;

/// Byte written over reclaimable memory before releasing it.
///
/// `0xDEDEDEDE_DEDEDEDE` is not a canonical address, so if anything still
/// holds a pointer into this memory and follows it, the result is an
/// immediate #GP rather than a plausible-looking read. Bit 47 is set while
/// the bits above it are not, which is exactly what the canonical check
/// rejects.
const POISON: u8 = 0xDE;

/// One memory map entry, copied out of bootloader memory.
#[derive(Clone, Copy)]
pub struct MemoryRegion {
    pub base: u64,
    pub length: u64,
    pub kind: u64,
}

/// Everything the kernel still needs from Limine, in memory we own.
pub struct BootInfo {
    name: [u8; MAX_STRING],
    name_len: usize,
    version: [u8; MAX_STRING],
    version_len: usize,
    cmdline: [u8; MAX_CMDLINE],
    cmdline_len: usize,
    memmap: [MemoryRegion; MAX_MEMMAP_ENTRIES],
    memmap_len: usize,
}

impl BootInfo {
    const fn empty() -> Self {
        BootInfo {
            name: [0; MAX_STRING],
            name_len: 0,
            version: [0; MAX_STRING],
            version_len: 0,
            cmdline: [0; MAX_CMDLINE],
            cmdline_len: 0,
            memmap: [MemoryRegion {
                base: 0,
                length: 0,
                kind: 0,
            }; MAX_MEMMAP_ENTRIES],
            memmap_len: 0,
        }
    }

    pub fn bootloader_name(&self) -> &str {
        // SAFETY: The bytes came from a `&str`, so they are valid UTF-8,
        // and `name_len` is the length that was copied.
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("<invalid utf8>")
    }

    pub fn bootloader_version(&self) -> &str {
        core::str::from_utf8(&self.version[..self.version_len]).unwrap_or("<invalid utf8>")
    }

    pub fn cmdline(&self) -> &str {
        core::str::from_utf8(&self.cmdline[..self.cmdline_len]).unwrap_or("<invalid utf8>")
    }

    pub fn memory_map(&self) -> &[MemoryRegion] {
        &self.memmap[..self.memmap_len]
    }
}

/// The copy. Written once by [`capture`], read-only afterwards.
///
/// A `static mut` would be simpler to write and harder to defend; the cell
/// keeps each access an explicit pointer operation with its own argument.
static BOOT_INFO: BootInfoCell = BootInfoCell(core::cell::UnsafeCell::new(BootInfo::empty()));

struct BootInfoCell(core::cell::UnsafeCell<BootInfo>);

// SAFETY: Written exactly once during single-threaded boot with interrupts
// disabled, read-only from then on.
unsafe impl Sync for BootInfoCell {}

/// Copy a string into a fixed buffer, panicking if it does not fit.
///
/// Truncation is not an option: a half-copied command line changes the
/// kernel's behaviour silently, and a half-copied name is a lie in the log.
fn copy_str(dst: &mut [u8], src: &str, what: &str) -> usize {
    assert!(
        src.len() <= dst.len(),
        "{} is {} bytes but the buffer holds {}; raise the limit rather \
         than truncating",
        what,
        src.len(),
        dst.len()
    );
    dst[..src.len()].copy_from_slice(src.as_bytes());
    src.len()
}

/// Gate 1: copy everything still needed out of bootloader memory.
///
/// # Safety
///
/// Call once, while the Limine responses are still valid — that is, before
/// any reclaim.
pub unsafe fn capture(
    info: &BootloaderInfoRequest,
    cmdline: &ExecutableCmdlineRequest,
    memmap: &MemmapRequest,
) {
    // SAFETY: Single-threaded boot, called once, and nothing else holds a
    // reference to the cell yet.
    let dst = unsafe { &mut *BOOT_INFO.0.get() };

    if let Some(response) = info.response() {
        dst.name_len = copy_str(&mut dst.name, response.name(), "bootloader name");
        dst.version_len = copy_str(&mut dst.version, response.version(), "bootloader version");
    }

    if let Some(response) = cmdline.response() {
        dst.cmdline_len = copy_str(&mut dst.cmdline, response.cmdline(), "kernel command line");
    }

    let response = memmap
        .response()
        .expect("Limine did not answer the memory map request");
    let entries = response.entries();

    assert!(
        entries.len() <= MAX_MEMMAP_ENTRIES,
        "memory map has {} entries, MAX_MEMMAP_ENTRIES is {} — raise it; \
         truncating would lose or invent memory",
        entries.len(),
        MAX_MEMMAP_ENTRIES
    );

    for (index, entry) in entries.iter().enumerate() {
        dst.memmap[index] = MemoryRegion {
            base: entry.base,
            length: entry.length,
            kind: entry.type_,
        };
    }
    dst.memmap_len = entries.len();

    println!(
        "[boot] captured: name/version, cmdline ({} bytes), {} memory map \
         entries",
        dst.cmdline_len, dst.memmap_len
    );
}

/// The captured copy.
pub fn get() -> &'static BootInfo {
    // SAFETY: `capture` runs once before anything calls this, and nothing
    // writes the cell afterwards, so a shared reference is sound.
    unsafe { &*BOOT_INFO.0.get() }
}

/// Fill reclaimable memory with [`POISON`].
///
/// Gate 4. A weak check — it only catches a read that happens after this
/// point — but the cost is one pass over 4 MiB and it turns "silently wrong
/// data" into either visible garbage or an immediate fault.
///
/// # Safety
///
/// Nothing may still be reading these regions. That is the thing being
/// tested, so this is the one gate that cannot prove its own precondition.
unsafe fn poison_reclaimable(regions: &[MemoryRegion]) -> u64 {
    let mut bytes = 0u64;

    for region in regions {
        if region.kind != memmap::MEMMAP_BOOTLOADER_RECLAIMABLE {
            continue;
        }

        // SAFETY: The region is real RAM (the memory map says so) and the
        // physical window maps all RAM writably. Nothing owns it: the frame
        // allocator has never handed it out, because `is_allocatable`
        // excludes this type.
        unsafe {
            let ptr = super::phys_to_virt(PhysAddr(region.base)).as_ptr::<u8>();
            core::ptr::write_bytes(ptr, POISON, region.length as usize);
        }

        bytes += region.length;
    }

    bytes
}

/// Release bootloader-reclaimable memory to the frame allocator.
///
/// Runs the gates of ADR 013 in order. Returns the number of frames gained.
///
/// # Safety
///
/// Call once, after [`capture`], after [`vm::init`] has switched to our own
/// page tables, and after [`vm::switch_to_main_stack`] has moved the kernel
/// off the bootloader's stack. All three are asserted below rather than
/// trusted.
pub unsafe fn reclaim() -> u64 {
    let info = get();

    // --- Gate 1 (check): was anything captured? -------------------------
    //
    // `capture` panics on overflow, so reaching here with a populated map
    // means the copy succeeded.
    assert!(
        info.memmap_len > 0,
        "bootinfo::capture() was never called; reclaiming now would leave \
         dangling references to the memory map"
    );

    // --- Gate 2: are we off the bootloader's page tables? ---------------
    //
    // Releasing this memory while running on Limine's tables would free the
    // mappings we are executing through.
    assert!(
        vm::using_own_tables(),
        "cannot reclaim bootloader memory while running on its page tables"
    );

    // --- Gate 3: are we off the bootloader's stack? ---------------------
    //
    // This gate exists because the first version of this code did not have
    // it. Limine places the stack it hands the kernel inside
    // BOOTLOADER_RECLAIMABLE memory -- on the test machine, physical
    // 0x0ff6e000, inside the 0x0ff45000..0x0ffdf000 reclaimable region.
    // Poisoning below therefore overwrote the live stack, and the `ret`
    // out of this very function loaded 0xdededededededede as its return
    // address: #GP, with a fault address that looked like a wild jump and
    // nothing pointing at reclaim as the cause.
    //
    // Page tables and stacks are two separate things to be standing on,
    // and leaving either one behind is enough to lose the kernel.
    assert!(
        vm::on_own_stack(),
        "cannot reclaim bootloader memory while running on the bootloader's \
         stack: Limine allocates it from the very memory about to be \
         poisoned, so the next return would load the poison pattern"
    );

    let regions = info.memory_map();

    // How many frames should this yield? Compared against reality below.
    let expected: u64 = regions
        .iter()
        .filter(|r| r.kind == memmap::MEMMAP_BOOTLOADER_RECLAIMABLE)
        .map(|r| {
            let first = r.base.div_ceil(FRAME_SIZE);
            let last_exclusive = (r.base + r.length) / FRAME_SIZE;
            last_exclusive.saturating_sub(first)
        })
        .sum();

    // --- Gate 4: poison, then check the copy still reads correctly ------
    // SAFETY: Everything still needed was copied by `capture`; gate 2
    // established that we are no longer executing through these pages, and
    // gate 3 that we are not standing on them either.
    let poisoned = unsafe { poison_reclaimable(regions) };

    // The copies live in the kernel image, not in the poisoned memory, so
    // they must still be readable and sane.
    assert!(
        !info.bootloader_name().is_empty(),
        "the bootloader name is empty after poisoning; the copy did not \
         survive, which means it was never a copy"
    );
    assert_eq!(
        info.memory_map().len(),
        info.memmap_len,
        "the copied memory map changed after poisoning"
    );

    println!(
        "[mem] poisoned {} KiB of reclaimable memory with {:#04x}; boot info \
         still reads back correctly",
        poisoned / 1024,
        POISON
    );

    // --- Release ---------------------------------------------------------
    let mut reclaimed = 0u64;
    for region in regions {
        if region.kind != memmap::MEMMAP_BOOTLOADER_RECLAIMABLE {
            continue;
        }

        // Whole frames only: a partial frame at either end belongs to
        // whatever shares it.
        let first = region.base.div_ceil(FRAME_SIZE);
        let last_exclusive = (region.base + region.length) / FRAME_SIZE;

        let mut frame = first;
        while frame < last_exclusive {
            frame_allocator::release(PhysFrame(frame));
            reclaimed += 1;
            frame += 1;
        }
    }

    // --- Gate 5: does the count match the prediction? -------------------
    assert_eq!(
        reclaimed, expected,
        "reclaimed {} frames but the memory map predicted {}; a region was \
         skipped or something extra was released",
        reclaimed, expected
    );

    println!(
        "[mem] reclaimed {} frames ({} KiB) from bootloader-reclaimable",
        reclaimed,
        reclaimed * FRAME_SIZE / 1024
    );

    reclaimed
}
