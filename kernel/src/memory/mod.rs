// SPDX-License-Identifier: BSD-2-Clause
//! Physical memory: address types and the Limine memory map.
//!
//! M2 covers physical frames only. Virtual memory -- page tables, mapping,
//! the higher-half direct map as something we manage rather than inherit --
//! belongs to M3.

pub mod frame_allocator;

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use limine::memmap;
use limine::request::MemmapRequest;

use crate::println;

/// Size of a physical frame.
///
/// 4 KiB is the smallest page x86-64 supports and the only size M2 deals in.
/// Larger pages (2 MiB, 1 GiB) exist and will matter for the direct map in
/// M3, but mixing granularities in the allocator would mean tracking a size
/// per frame for no present benefit.
///
/// Reference: Intel SDM Vol. 3A, 4.5 "4-Level Paging", Table 4-14.
pub const FRAME_SIZE: u64 = 4096;

/// A physical address.
///
/// A newtype rather than a bare `u64` because physical and virtual addresses
/// are both 64-bit integers and mixing them is a bug the compiler can catch
/// for free. Nothing in the kernel can dereference one of these: that needs
/// a virtual address, which M3 will provide through the direct map.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct PhysAddr(pub u64);

impl PhysAddr {
    /// Whether this address is frame-aligned.
    pub const fn is_frame_aligned(self) -> bool {
        self.0.is_multiple_of(FRAME_SIZE)
    }
}

impl fmt::Display for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

/// A 4 KiB physical frame, identified by its index.
///
/// Storing the index rather than the address makes the bitmap arithmetic
/// direct and makes an unaligned frame unrepresentable.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct PhysFrame(pub u64);

impl PhysFrame {
    /// The frame containing `addr`.
    pub const fn containing(addr: PhysAddr) -> PhysFrame {
        PhysFrame(addr.0 / FRAME_SIZE)
    }

    /// Physical address of this frame's first byte.
    pub const fn start(self) -> PhysAddr {
        PhysAddr(self.0 * FRAME_SIZE)
    }
}

impl fmt::Display for PhysFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "frame {} @ {}", self.0, self.start())
    }
}

// ---- Address translation during boot -----------------------------------
//
// The kernel runs with paging already on, under tables Limine built. Two
// facts from the bootloader let us convert between physical and virtual
// addresses before we have page tables of our own:
//
//   * the HHDM offset: all of physical memory is mapped at a fixed offset
//   * the executable slide: where our image landed physically
//
// Both are stashed here at init and read back by the frame allocator. They
// are a loan from the bootloader; M3 replaces them with mappings we own.

/// HHDM offset, captured during [`init`].
static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Virtual-minus-physical slide of the kernel image, captured during
/// [`init`].
static KERNEL_SLIDE: AtomicU64 = AtomicU64::new(0);

/// Offset at which Limine mapped all of physical memory.
///
/// Adding this to a physical address yields a virtual address the kernel
/// can dereference.
pub fn hhdm_offset() -> u64 {
    let offset = HHDM_OFFSET.load(Ordering::Acquire);
    debug_assert!(offset != 0, "hhdm_offset() before memory::init()");
    offset
}

/// Convert a kernel *image* virtual address to its physical address.
///
/// Only valid for addresses inside the kernel image: the image is mapped
/// with its own slide, which is unrelated to the HHDM offset.
pub fn kernel_virt_to_phys(virt: u64) -> u64 {
    virt.wrapping_sub(KERNEL_SLIDE.load(Ordering::Acquire))
}

// ---- Limine memory map -------------------------------------------------

/// Human-readable name for a Limine memory map entry type.
///
/// Reference: Limine boot protocol, "Memory Map Feature"; the constants come
/// from the `limine` crate's `memmap` module.
fn entry_type_name(type_: u64) -> &'static str {
    match type_ {
        memmap::MEMMAP_USABLE => "usable",
        memmap::MEMMAP_RESERVED => "reserved",
        memmap::MEMMAP_ACPI_RECLAIMABLE => "acpi-reclaimable",
        memmap::MEMMAP_ACPI_NVS => "acpi-nvs",
        memmap::MEMMAP_BAD_MEMORY => "bad-memory",
        memmap::MEMMAP_BOOTLOADER_RECLAIMABLE => "bootloader-reclaimable",
        memmap::MEMMAP_EXECUTABLE_AND_MODULES => "executable-and-modules",
        memmap::MEMMAP_FRAMEBUFFER => "framebuffer",
        memmap::MEMMAP_MAPPED_RESERVED => "mapped-reserved",
        _ => "unknown",
    }
}

/// Whether the frame allocator may hand out frames from this entry.
///
/// Only `USABLE`. In particular **not** `BOOTLOADER_RECLAIMABLE`, even
/// though the name invites it: that region still holds Limine's response
/// structures, and the kernel keeps reading them -- the memory map we are
/// walking right now lives there. Handing those frames out would let a later
/// allocation overwrite data we are still using, and the failure would
/// surface far away from the cause.
///
/// Reclaiming it is a real saving (a few hundred KiB) and a real milestone:
/// it requires copying out everything we still need first. That is M3's job.
/// See docs/adr/008-bootloader-reclaimable-dokunulmaz.md.
fn is_allocatable(type_: u64) -> bool {
    type_ == memmap::MEMMAP_USABLE
}

/// Print the memory map and return the totals.
///
/// Returns `(usable_bytes, total_bytes)`.
fn report_memory_map(entries: &[&memmap::Entry]) -> (u64, u64) {
    println!("[mem] memory map: {} entries", entries.len());

    let mut usable_bytes: u64 = 0;
    let mut total_bytes: u64 = 0;

    for entry in entries {
        let base = entry.base;
        let length = entry.length;
        let end = base.saturating_add(length);

        println!(
            "[mem]   {:#018x}..{:#018x}  {:>9} KiB  {}",
            base,
            end,
            length / 1024,
            entry_type_name(entry.type_)
        );

        total_bytes = total_bytes.saturating_add(length);
        if is_allocatable(entry.type_) {
            usable_bytes = usable_bytes.saturating_add(length);
        }
    }

    (usable_bytes, total_bytes)
}

/// Bring up physical memory management.
///
/// Prints the memory map, then builds the frame allocator over the usable
/// regions. Panics if Limine did not answer the memory map request: without
/// it there is no way to know which memory exists, and guessing is worse
/// than stopping.
///
/// # Safety
///
/// Call exactly once, after the serial port is up and before anything tries
/// to allocate a frame. `hhdm` is Limine's direct-map offset and
/// `kernel_slide` is the kernel image's virtual-minus-physical difference;
/// both must be the values the bootloader reported for this boot.
pub unsafe fn init(memmap_request: &MemmapRequest, hhdm: u64, kernel_slide: u64) {
    HHDM_OFFSET.store(hhdm, Ordering::Release);
    KERNEL_SLIDE.store(kernel_slide, Ordering::Release);

    let response = memmap_request
        .response()
        .expect("Limine did not answer the memory map request");

    let entries = response.entries();
    let (usable_bytes, total_bytes) = report_memory_map(entries);

    println!(
        "[mem] total {} MiB, usable {} MiB ({} frames of {} KiB)",
        total_bytes / (1024 * 1024),
        usable_bytes / (1024 * 1024),
        usable_bytes / FRAME_SIZE,
        FRAME_SIZE / 1024
    );

    // SAFETY: Called once, on the single-threaded boot path, with a memory
    // map that Limine filled in and that we have not modified. Nothing has
    // allocated a frame yet, so the allocator is free to claim storage for
    // its own bitmap out of the memory it is about to manage.
    unsafe {
        frame_allocator::init(entries);
    }
}
