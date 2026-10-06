// SPDX-License-Identifier: BSD-2-Clause
//! Page tables: entries, flags, and map/unmap/translate.
//!
//! Mechanism only. This module never decides *where* something should be
//! mapped, never searches for free address space, and never frees a frame
//! on the caller's behalf. Those are policy, and policy belongs to the
//! caller -- which from M11 will be a user-space component talking through
//! a capability. See ADR 011.
//!
//! Reference throughout: Intel SDM Vol. 3A, Chapter 4 "Paging", in
//! particular 4.5 "4-Level Paging" and Figure 4-11 (entry formats).

use core::arch::asm;
use core::fmt;

use bitflags::bitflags;

use super::layout;
use super::{PhysAddr, PhysFrame, VirtAddr, FRAME_SIZE};

/// Entries in one page table. 512 × 8 bytes = one 4 KiB frame.
pub const ENTRY_COUNT: usize = 512;

/// Where a frame can be reached in the kernel's address space.
///
/// Set once by `vm::init` and used by every page-table walk: the tables
/// themselves are ordinary frames, and reading one means going through the
/// physical window.
///
/// This is a function rather than a constant because during early boot the
/// window is Limine's HHDM; afterwards it is ours, at the same address
/// (ADR 010). Keeping it behind a call means the switch changes nothing
/// here.
fn phys_to_virt(phys: PhysAddr) -> VirtAddr {
    VirtAddr::new(super::pmap_base() + phys.0)
}

// ---- Flags -------------------------------------------------------------

bitflags! {
    /// Permissions and attributes for a mapping.
    ///
    /// Deliberately not the hardware bit layout: on x86 `EXEC` is the
    /// *absence* of NX, and `READ` is not a bit at all. Translating here
    /// means callers never have to remember that, and it leaves room to
    /// reject nonsensical combinations (W^X) at the API boundary.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct PageFlags: u64 {
        /// Readable. Implied by presence on x86, but explicit in the API:
        /// a mapping with no access bits at all is a mistake worth
        /// catching.
        const READ = 1 << 0;
        /// Writable.
        const WRITE = 1 << 1;
        /// Executable. Becomes the absence of the hardware NX bit.
        const EXEC = 1 << 2;
        /// Reachable from CPL 3.
        const USER = 1 << 3;
        /// Survives a CR3 reload. Only meaningful with CR4.PGE set.
        const GLOBAL = 1 << 4;
        /// Uncacheable. For MMIO, where a cached write is a lost write.
        const NO_CACHE = 1 << 5;
        /// Write-through rather than write-back.
        const WRITE_THROUGH = 1 << 6;
    }
}

impl PageFlags {
    /// Check the combination makes sense.
    ///
    /// Two rules: something must be accessible, and nothing may be both
    /// writable and executable. W^X is enforced here rather than by
    /// convention, so a mapping that violates it cannot be created by
    /// accident.
    fn validate(self) -> Result<(), MapError> {
        if !self.intersects(PageFlags::READ | PageFlags::WRITE | PageFlags::EXEC) {
            return Err(MapError::InvalidFlags(self));
        }
        if self.contains(PageFlags::WRITE | PageFlags::EXEC) {
            return Err(MapError::InvalidFlags(self));
        }
        Ok(())
    }
}

// ---- Hardware entry ----------------------------------------------------

bitflags! {
    /// Bits of a page-table entry, as the CPU reads them.
    ///
    /// Reference: Intel SDM Vol. 3A, Figure 4-11 and Table 4-19.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct EntryFlags: u64 {
        /// Bit 0, P: the entry is valid.
        const PRESENT = 1 << 0;
        /// Bit 1, R/W: writes allowed through this entry.
        const WRITABLE = 1 << 1;
        /// Bit 2, U/S: CPL 3 may use this entry.
        const USER = 1 << 2;
        /// Bit 3, PWT: page-level write-through.
        const WRITE_THROUGH = 1 << 3;
        /// Bit 4, PCD: page-level cache disable.
        const NO_CACHE = 1 << 4;
        /// Bit 5, A: set by the CPU on access.
        const ACCESSED = 1 << 5;
        /// Bit 6, D: set by the CPU on write (leaf entries only).
        const DIRTY = 1 << 6;
        /// Bit 7, PS: this entry maps a large page rather than a table.
        /// Meaningful at PDPT (1 GiB) and PD (2 MiB) level.
        const HUGE = 1 << 7;
        /// Bit 8, G: global; not flushed on a CR3 reload when CR4.PGE is
        /// set.
        const GLOBAL = 1 << 8;
        /// Bit 63, XD: execute-disable.
        ///
        /// Only honoured when EFER.NXE is set. With NXE clear this is a
        /// *reserved* bit and any access through the entry faults, which
        /// is why cpu::init enables NXE before any table is built.
        const NO_EXECUTE = 1 << 63;
    }
}

/// Physical address bits of an entry: 51:12.
///
/// The architecture allows up to 52 physical address bits; the low 12 are
/// always zero because the target is frame-aligned.
const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// One page-table entry.
///
/// `#[repr(transparent)]` so an array of these is exactly what the CPU
/// expects to walk.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct Entry(u64);

impl Entry {
    /// An absent entry.
    const fn empty() -> Self {
        Entry(0)
    }

    fn is_present(self) -> bool {
        self.0 & EntryFlags::PRESENT.bits() != 0
    }

    fn is_huge(self) -> bool {
        self.0 & EntryFlags::HUGE.bits() != 0
    }

    fn frame(self) -> PhysFrame {
        PhysFrame::containing(PhysAddr(self.0 & ADDRESS_MASK))
    }

    fn flags(self) -> EntryFlags {
        EntryFlags::from_bits_truncate(self.0)
    }

    fn new(frame: PhysFrame, flags: EntryFlags) -> Self {
        Entry(frame.start().0 | flags.bits())
    }
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_present() {
            write!(f, "Entry({} {:?})", self.frame(), self.flags())
        } else {
            write!(f, "Entry(absent)")
        }
    }
}

/// Translate API-level flags into hardware bits for a leaf entry.
fn leaf_entry_flags(flags: PageFlags) -> EntryFlags {
    let mut bits = EntryFlags::PRESENT;

    if flags.contains(PageFlags::WRITE) {
        bits |= EntryFlags::WRITABLE;
    }
    if !flags.contains(PageFlags::EXEC) {
        // NX is the inverse of EXEC. This is the whole reason PageFlags
        // exists as a separate type.
        bits |= EntryFlags::NO_EXECUTE;
    }
    if flags.contains(PageFlags::USER) {
        bits |= EntryFlags::USER;
    }
    if flags.contains(PageFlags::GLOBAL) {
        bits |= EntryFlags::GLOBAL;
    }
    if flags.contains(PageFlags::NO_CACHE) {
        bits |= EntryFlags::NO_CACHE;
    }
    if flags.contains(PageFlags::WRITE_THROUGH) {
        bits |= EntryFlags::WRITE_THROUGH;
    }

    bits
}

/// Translate hardware bits back into API-level flags.
fn page_flags_from_entry(bits: EntryFlags) -> PageFlags {
    let mut flags = PageFlags::READ;

    if bits.contains(EntryFlags::WRITABLE) {
        flags |= PageFlags::WRITE;
    }
    if !bits.contains(EntryFlags::NO_EXECUTE) {
        flags |= PageFlags::EXEC;
    }
    if bits.contains(EntryFlags::USER) {
        flags |= PageFlags::USER;
    }
    if bits.contains(EntryFlags::GLOBAL) {
        flags |= PageFlags::GLOBAL;
    }
    if bits.contains(EntryFlags::NO_CACHE) {
        flags |= PageFlags::NO_CACHE;
    }
    if bits.contains(EntryFlags::WRITE_THROUGH) {
        flags |= PageFlags::WRITE_THROUGH;
    }

    flags
}

/// Flags for an intermediate (non-leaf) entry.
///
/// Permissive at every level: the CPU intersects the permissions along the
/// walk, so restricting an intermediate entry would restrict every leaf
/// beneath it. The leaf decides. NX in particular must stay clear here, or
/// nothing under this subtree could ever be executable.
fn intermediate_entry_flags(user: bool) -> EntryFlags {
    let mut bits = EntryFlags::PRESENT | EntryFlags::WRITABLE;
    if user {
        bits |= EntryFlags::USER;
    }
    bits
}

// ---- Page table --------------------------------------------------------

/// One level of the page-table tree: 512 entries filling a 4 KiB frame.
///
/// The level is *not* in the type. A `PageTable<Level4>` design would catch
/// level confusion at compile time, but it pushes generics into every
/// signature that touches a table, and the walk is a loop over levels --
/// which a type-level encoding turns into recursion or a macro. The level
/// is a runtime parameter instead.
#[derive(Clone)]
#[repr(C, align(4096))]
pub struct PageTable {
    entries: [Entry; ENTRY_COUNT],
}

impl PageTable {
    /// A table with no present entries.
    pub const fn empty() -> Self {
        PageTable {
            entries: [Entry::empty(); ENTRY_COUNT],
        }
    }
}

/// Index into the page table at `level` for `addr`.
///
/// Level 4 is the PML4, level 1 the PT. Each level consumes nine bits,
/// starting at bit 12 for the PT.
///
/// Reference: Intel SDM Vol. 3A, Figure 4-8 "Linear-Address Translation to
/// a 4-KByte Page using 4-Level Paging".
fn table_index(addr: VirtAddr, level: u8) -> usize {
    debug_assert!((1..=4).contains(&level));
    let shift = 12 + 9 * (level as u32 - 1);
    ((addr.0 >> shift) & 0x1FF) as usize
}

// ---- Frame source ------------------------------------------------------

/// Where `map` gets frames for intermediate tables.
///
/// A trait, not a call into the global allocator, because the kernel must
/// not allocate on its own behalf: the frame comes out of the caller's
/// budget. Today the implementor wraps M2's bitmap allocator; from M5 it
/// will be an Untyped capability, and the code in this module will not
/// change.
pub trait FrameSource {
    /// Take a frame, or `None` if none are available.
    fn alloc_frame(&mut self) -> Option<PhysFrame>;
}

// ---- TLB guard ---------------------------------------------------------

/// Proof obligation: a mapping changed and the TLB has not been told.
///
/// Returned by `map` and `unmap`. Dropping it without calling `flush` (or
/// explicitly deferring) panics, because a stale TLB entry is this
/// project's most expensive bug class: the fault is in `unmap`, the symptom
/// is somewhere else and later, and it is not deterministic.
///
/// Making this a type rather than a comment is deliberate. M1 had a rule
/// that lived only in a comment -- "do not read this register afterwards"
/// -- and it was violated anyway; the result was a triple fault. See
/// ADR 012.
#[must_use = "the TLB must be invalidated before this mapping change takes effect"]
pub struct TlbFlush {
    page: VirtAddr,
}

impl TlbFlush {
    fn new(page: VirtAddr) -> Self {
        TlbFlush { page }
    }

    /// Invalidate this page's translation and discharge the obligation.
    pub fn flush(self) {
        // The page-table write has already happened. A barrier between the
        // write and the invalidation makes sure the CPU cannot reorder the
        // two -- the SDM may not strictly require it (4.10.4.1 is open to
        // reading), but it could not be verified under QEMU, and where a
        // thing cannot be measured the worst case is assumed. The cost is
        // one instruction on a path that already touches page tables.
        //
        // SAFETY: `mfence` orders memory operations and has no other
        // effect. `invlpg` invalidates one TLB entry; it requires CPL 0,
        // which holds, and cannot fault for a canonical address.
        //
        // Reference: Intel SDM Vol. 2A, "INVLPG"; Vol. 3A, 4.10.4.1
        // "Operations that Invalidate TLBs".
        unsafe {
            asm!(
                "mfence",
                "invlpg [{}]",
                in(reg) self.page.0,
                options(nostack, preserves_flags),
            );
        }
        core::mem::forget(self);
    }

    /// Discharge the obligation without flushing, because the caller will
    /// perform a broader invalidation.
    ///
    /// The name is long because every use needs justifying: bulk mapping
    /// followed by one full flush, or a test that deliberately wants a
    /// stale entry.
    pub fn ignore_because_caller_will_flush_all(self) {
        core::mem::forget(self);
    }
}

impl Drop for TlbFlush {
    fn drop(&mut self) {
        panic!(
            "mapping at {} changed without invalidating the TLB; the CPU may \
             still be using the old translation",
            self.page
        );
    }
}

/// Invalidate every non-global TLB entry by reloading CR3.
///
/// Global entries (the kernel's own mappings, which are mapped GLOBAL so
/// they survive address-space switches) are *not* affected. Changing one of
/// those needs `invlpg` or a CR4.PGE toggle -- a trap worth remembering for
/// any code that edits kernel mappings after boot.
///
/// Reference: Intel SDM Vol. 3A, 4.10.4.1 and 4.10.2.4 "Global Pages".
pub fn flush_all() {
    // SAFETY: Reading CR3 and writing the same value back is architecturally
    // defined to invalidate non-global TLB entries, and leaves the active
    // table unchanged. Requires CPL 0, which holds.
    unsafe {
        asm!(
            "mov {tmp}, cr3",
            "mov cr3, {tmp}",
            tmp = out(reg) _,
            options(nostack, preserves_flags),
        );
    }
}

// ---- Errors ------------------------------------------------------------

/// Why a `map` was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapError {
    /// Something is already mapped there. Overwriting would silently drop
    /// the old frame, which nobody may still hold a reference to -- a leak.
    /// Change a mapping by unmapping first.
    AlreadyMapped { existing: PhysFrame },
    /// An intermediate table was needed and the `FrameSource` had nothing
    /// left. An ordinary condition, not a bug.
    OutOfFrames,
    /// The address is not canonical. Accessing it would raise #GP rather
    /// than #PF, because the canonical check precedes the page walk.
    NotCanonical { addr: VirtAddr },
    /// A user flag on a kernel address, or a kernel mapping requested in
    /// the user half.
    WrongHalf { addr: VirtAddr, user: bool },
    /// No access bits at all, or both WRITE and EXEC (W^X).
    InvalidFlags(PageFlags),
    /// A large page covers this address; a 4 KiB mapping cannot be placed
    /// inside it without splitting it first.
    BlockedByHugePage { level: u8 },
}

/// Why an `unmap` was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnmapError {
    /// Nothing is mapped there. The same class of mistake as a double
    /// free: accepting it quietly would let the caller free a frame twice.
    NotMapped,
    NotCanonical {
        addr: VirtAddr,
    },
    BlockedByHugePage {
        level: u8,
    },
}

/// Why a `translate` found nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TranslateError {
    NotMapped,
    NotCanonical { addr: VirtAddr },
}

// ---- Walking -----------------------------------------------------------

/// Borrow a page table through the physical window.
///
/// # Safety
///
/// `frame` must hold a live page table, and the caller must not create a
/// second mutable borrow of the same frame.
unsafe fn table_mut<'a>(frame: PhysFrame) -> &'a mut PageTable {
    let virt = phys_to_virt(frame.start());
    // SAFETY: The caller guarantees the frame holds a table. The physical
    // window maps all RAM writably, and a table frame is 4 KiB aligned by
    // construction, which satisfies PageTable's alignment.
    unsafe { &mut *(virt.0 as *mut PageTable) }
}

/// Borrow a page table immutably through the physical window.
///
/// # Safety
///
/// As [`table_mut`], but shared.
unsafe fn table_ref<'a>(frame: PhysFrame) -> &'a PageTable {
    let virt = phys_to_virt(frame.start());
    // SAFETY: See table_mut.
    unsafe { &*(virt.0 as *const PageTable) }
}

/// Walk from `table` down to the PT that would hold `addr`, creating
/// missing levels from `source`.
///
/// # Safety
///
/// `table` must be a live PML4 reachable through the physical window.
unsafe fn walk_create<'a>(
    table: &'a mut PageTable,
    addr: VirtAddr,
    user: bool,
    source: &mut impl FrameSource,
) -> Result<&'a mut PageTable, MapError> {
    let mut current: *mut PageTable = table;

    // Levels 4, 3, 2 are intermediate; level 1 is the leaf table we want.
    for level in (2..=4u8).rev() {
        let index = table_index(addr, level);

        // SAFETY: `current` points at a live table -- on the first pass the
        // caller's PML4, afterwards one we just validated or created.
        let entry = unsafe { (*current).entries[index] };

        let next_frame = if entry.is_present() {
            if entry.is_huge() {
                // A large page covers this address. Splitting it is a
                // policy decision (which pieces keep which permissions),
                // so refuse rather than guess.
                return Err(MapError::BlockedByHugePage { level });
            }
            entry.frame()
        } else {
            let frame = source.alloc_frame().ok_or(MapError::OutOfFrames)?;

            // A fresh frame holds whatever the last owner left. Zero it
            // before the CPU can walk it, or stale bits become bogus
            // mappings.
            //
            // SAFETY: The frame came from the allocator, so nothing else
            // owns it, and the physical window maps it writably.
            unsafe {
                let ptr = phys_to_virt(frame.start()).0 as *mut PageTable;
                ptr.write(PageTable::empty());
            }

            // SAFETY: `current` points at a live table, as above.
            unsafe {
                (*current).entries[index] = Entry::new(frame, intermediate_entry_flags(user));
            }

            frame
        };

        // SAFETY: `next_frame` holds a page table: either it was already
        // present and non-huge, or we just created and zeroed it. No other
        // mutable borrow of it is live -- we drop `current` on this line.
        current = unsafe { table_mut(next_frame) };
    }

    // SAFETY: The loop leaves `current` pointing at the level-1 table.
    Ok(unsafe { &mut *current })
}

/// Walk down to the leaf entry for `addr` without creating anything.
///
/// Returns the entry and the level it was found at (1 for a 4 KiB page,
/// 2 or 3 for a large page).
///
/// # Safety
///
/// `table` must be a live PML4 reachable through the physical window.
unsafe fn walk_lookup(table: &PageTable, addr: VirtAddr) -> Option<(Entry, u8)> {
    let mut current: *const PageTable = table;

    for level in (1..=4u8).rev() {
        let index = table_index(addr, level);
        // SAFETY: `current` points at a live table; see walk_create.
        let entry = unsafe { (*current).entries[index] };

        if !entry.is_present() {
            return None;
        }
        if level == 1 || entry.is_huge() {
            return Some((entry, level));
        }

        // SAFETY: Present, non-huge intermediate entry, so its frame holds
        // the next table.
        current = unsafe { table_ref(entry.frame()) };
    }

    None
}

// ---- Public API --------------------------------------------------------

/// Check an address before touching any table.
fn check_address(addr: VirtAddr, flags_user: bool) -> Result<(), MapError> {
    if !layout::is_canonical(addr.0) {
        return Err(MapError::NotCanonical { addr });
    }
    // A user mapping must be in the user half and a kernel mapping in the
    // kernel half. Getting this wrong would either make kernel memory
    // reachable from ring 3 or create a user mapping ring 3 cannot see.
    let in_user_half = layout::is_user(addr.0);
    if flags_user != in_user_half {
        return Err(MapError::WrongHalf {
            addr,
            user: flags_user,
        });
    }
    Ok(())
}

/// Map `page` to `frame`.
///
/// Returns a [`TlbFlush`] that must be discharged before the mapping is
/// considered live.
///
/// # Safety
///
/// `table` must be a live PML4. The caller is responsible for the frame
/// being theirs to map and for the mapping not aliasing something in a way
/// that breaks another holder's assumptions.
pub unsafe fn map(
    table: &mut PageTable,
    page: VirtAddr,
    frame: PhysFrame,
    flags: PageFlags,
    source: &mut impl FrameSource,
) -> Result<TlbFlush, MapError> {
    flags.validate()?;

    let user = flags.contains(PageFlags::USER);
    check_address(page, user)?;

    if !page.is_frame_aligned() {
        // A page is identified by its first byte; an unaligned address
        // means the caller is confused about what is being mapped.
        return Err(MapError::InvalidFlags(flags));
    }

    // SAFETY: The caller guarantees `table` is a live PML4.
    let leaf = unsafe { walk_create(table, page, user, source)? };
    let index = table_index(page, 1);

    if leaf.entries[index].is_present() {
        return Err(MapError::AlreadyMapped {
            existing: leaf.entries[index].frame(),
        });
    }

    leaf.entries[index] = Entry::new(frame, leaf_entry_flags(flags));

    Ok(TlbFlush::new(page))
}

/// Remove the mapping for `page` and return the frame it pointed at.
///
/// The frame is *returned*, not freed: it belongs to whoever mapped it.
///
/// # Safety
///
/// `table` must be a live PML4.
pub unsafe fn unmap(
    table: &mut PageTable,
    page: VirtAddr,
) -> Result<(PhysFrame, TlbFlush), UnmapError> {
    if !layout::is_canonical(page.0) {
        return Err(UnmapError::NotCanonical { addr: page });
    }

    // SAFETY: The caller guarantees `table` is a live PML4.
    let found = unsafe { walk_lookup(table, page) };

    match found {
        None => Err(UnmapError::NotMapped),
        Some((_, level)) if level != 1 => Err(UnmapError::BlockedByHugePage { level }),
        Some((entry, _)) => {
            let frame = entry.frame();

            // Walk again to clear the entry. A second walk rather than
            // threading a mutable pointer out of `walk_lookup`: the cost is
            // four memory reads, and the alternative is a borrow that
            // outlives the walk.
            //
            // SAFETY: The caller guarantees `table` is a live PML4, and the
            // lookup above proved every level down to the leaf is present
            // and non-huge.
            unsafe {
                clear_leaf(table, page);
            }

            Ok((frame, TlbFlush::new(page)))
        }
    }
}

/// Clear the leaf entry for `page`.
///
/// # Safety
///
/// Every level down to the leaf must be present and non-huge; the caller
/// must have just established this.
unsafe fn clear_leaf(table: &mut PageTable, page: VirtAddr) {
    let mut current: *mut PageTable = table;

    for level in (2..=4u8).rev() {
        let index = table_index(page, level);
        // SAFETY: `current` points at a live table, and the caller
        // guarantees the entry is present and non-huge.
        let entry = unsafe { (*current).entries[index] };
        debug_assert!(entry.is_present() && !entry.is_huge());
        // SAFETY: As above; the entry's frame holds the next table.
        current = unsafe { table_mut(entry.frame()) };
    }

    let index = table_index(page, 1);
    // SAFETY: `current` is the level-1 table.
    unsafe {
        (*current).entries[index] = Entry::empty();
    }

    // The intermediate tables are deliberately left in place even if they
    // are now empty. Freeing them would mean returning frames the caller
    // never explicitly handed over, and would need a reference count per
    // table. When VSpaces become capability objects in M11, tearing one
    // down will walk and free the whole tree at once.
}

/// Map a large page: 2 MiB at PD level or 1 GiB at PDPT level.
///
/// Separate from [`map`] on purpose. Large pages are only used to build the
/// physical window, where every page has the same permissions and the
/// addresses are known to be aligned; exposing a size parameter on the
/// general `map` would invite callers to get the alignment wrong.
///
/// # Safety
///
/// `table` must be a live PML4. `size` must be 2 MiB or 1 GiB, and both
/// addresses aligned to it. No `TlbFlush` is returned: this is only used
/// while building an inactive table, which is followed by a full flush.
pub unsafe fn map_huge(
    table: &mut PageTable,
    virt: VirtAddr,
    phys: PhysAddr,
    size: u64,
    flags: PageFlags,
    source: &mut impl FrameSource,
) -> Result<(), MapError> {
    flags.validate()?;

    let user = flags.contains(PageFlags::USER);
    check_address(virt, user)?;

    // 2 MiB lives at level 2 (PD), 1 GiB at level 3 (PDPT).
    let level: u8 = match size {
        x if x == FRAME_SIZE << 9 => 2,
        x if x == FRAME_SIZE << 18 => 3,
        _ => return Err(MapError::InvalidFlags(flags)),
    };

    assert!(
        virt.0.is_multiple_of(size) && phys.0.is_multiple_of(size),
        "a {} byte page needs both addresses aligned to it: {} -> {}",
        size,
        virt,
        phys
    );

    // Walk to the level *above* the one that will hold the large page,
    // creating tables as needed.
    let mut current: *mut PageTable = table;
    for walk_level in ((level + 1)..=4u8).rev() {
        let index = table_index(virt, walk_level);
        // SAFETY: `current` points at a live table -- the caller's PML4 on
        // the first pass, one we validated or created afterwards.
        let entry = unsafe { (*current).entries[index] };

        let next = if entry.is_present() {
            if entry.is_huge() {
                return Err(MapError::BlockedByHugePage { level: walk_level });
            }
            entry.frame()
        } else {
            let frame = source.alloc_frame().ok_or(MapError::OutOfFrames)?;
            // SAFETY: Fresh frame from the allocator, reachable through the
            // window. Zeroed so stale bits cannot become mappings.
            unsafe {
                let ptr = phys_to_virt(frame.start()).0 as *mut PageTable;
                ptr.write(PageTable::empty());
            }
            // SAFETY: `current` points at a live table.
            unsafe {
                (*current).entries[index] = Entry::new(frame, intermediate_entry_flags(user));
            }
            frame
        };

        // SAFETY: `next` holds a page table, present and non-huge.
        current = unsafe { table_mut(next) };
    }

    let index = table_index(virt, level);
    // SAFETY: `current` is the table at `level`.
    let existing = unsafe { (*current).entries[index] };
    if existing.is_present() {
        return Err(MapError::AlreadyMapped {
            existing: existing.frame(),
        });
    }

    // SAFETY: As above. HUGE marks this entry as a leaf at a level that
    // would otherwise point at another table.
    unsafe {
        (*current).entries[index] = Entry::new(
            PhysFrame::containing(phys),
            leaf_entry_flags(flags) | EntryFlags::HUGE,
        );
    }

    Ok(())
}

/// Look up the physical address and flags behind `addr`.
///
/// Works for large pages too, which is why the offset is computed from the
/// level the entry was found at.
///
/// # Safety
///
/// `table` must be a live PML4.
pub unsafe fn translate(
    table: &PageTable,
    addr: VirtAddr,
) -> Result<(PhysAddr, PageFlags), TranslateError> {
    if !layout::is_canonical(addr.0) {
        return Err(TranslateError::NotCanonical { addr });
    }

    // SAFETY: The caller guarantees `table` is a live PML4.
    let (entry, level) = unsafe { walk_lookup(table, addr) }.ok_or(TranslateError::NotMapped)?;

    // Page size at this level: 4 KiB at 1, 2 MiB at 2, 1 GiB at 3.
    let page_size = FRAME_SIZE << (9 * (level as u32 - 1));
    let offset = addr.0 & (page_size - 1);

    Ok((
        PhysAddr(entry.frame().start().0 + offset),
        page_flags_from_entry(entry.flags()),
    ))
}

// ---- CR3 ---------------------------------------------------------------

/// Physical frame of the active PML4.
pub fn active_table() -> PhysFrame {
    let cr3: u64;
    // SAFETY: Reading CR3 has no side effects. Requires CPL 0, which holds.
    unsafe {
        asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
    }
    // The low 12 bits are flags (PCD, PWT), not address.
    PhysFrame::containing(PhysAddr(cr3 & ADDRESS_MASK))
}

/// Make `table` the active page table.
///
/// # Safety
///
/// Catastrophically unsafe if the new table does not map the currently
/// executing code, the current stack, and the IDT: the next instruction
/// fetch would fault, and with no mapped handler that is a triple fault
/// and a silent reset. `vm::switch_to` verifies all three before calling.
pub unsafe fn load_table(table: PhysFrame) {
    // SAFETY: The caller guarantees the table maps everything needed to
    // keep executing. Writing CR3 also flushes non-global TLB entries.
    unsafe {
        asm!("mov cr3, {}", in(reg) table.start().0, options(nostack, preserves_flags));
    }
}
