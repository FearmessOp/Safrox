// SPDX-License-Identifier: BSD-2-Clause
//! User address spaces, built out of capabilities.
//!
//! A VSpace is a root `PageTable` capability (ADR 027). There is no
//! ninth object type: what makes a table a root is that a TCB is bound
//! to it, and that fact lives in the TCB.
//!
//! Three things distinguish this layer from [`crate::memory::paging`]
//! underneath it:
//!
//! * **Intermediate tables are supplied, never allocated.** `map_frame`
//!   fails with [`CapError::MissingPageTable`] rather than quietly
//!   spending frames the caller did not offer. ADR 022's rule -- the
//!   kernel does not allocate on its own behalf -- reaches all the way
//!   down here.
//! * **Rights become permissions.** A Frame capability without
//!   `Rights::WRITE` maps read-only; without `Rights::READ` it does not
//!   map at all.
//! * **The user half is the only half.** Invariant K1 of ADR 027: a
//!   mapping into a user VSpace must target a user address, whatever
//!   the flags say.
//!
//! The paging module itself is unchanged from M3. It takes a
//! `FrameSource`, and this module passes one that never yields a
//! frame -- which turns "a table was missing" into an ordinary error
//! return. ADR 011's claim that the API is mechanism rather than policy
//! was tested here and held.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::cnode::Slot;
use super::{CapError, CapType, Rights};
use crate::memory::paging::{self, FrameSource, PageFlags, PageTable};
use crate::memory::{layout, PhysAddr, PhysFrame, VirtAddr};
use crate::println;

/// Entries in a PML4; the upper half starts here.
///
/// Index 256 is the first address of the upper canonical half, which is
/// `layout::KERNEL_MIN`. Everything from here up is the kernel's and is
/// shared by every address space.
const KERNEL_PML4_FIRST: usize = 256;

/// A `FrameSource` that never produces a frame.
///
/// This is how the capability layer reuses M3's `paging::map` without
/// letting it allocate. A missing intermediate table surfaces as
/// `MapError::OutOfFrames`, which this module translates into
/// `MissingPageTable` -- an instruction to the caller rather than a
/// failure, since the caller is the one holding the PageTable
/// capabilities.
struct NoFrames;

impl FrameSource for NoFrames {
    fn alloc_frame(&mut self) -> Option<PhysFrame> {
        None
    }
}

// ---- Counters, for the acceptance test ---------------------------------

/// Mappings created through the capability path.
static MAPS: AtomicUsize = AtomicUsize::new(0);
/// Mappings removed.
static UNMAPS: AtomicUsize = AtomicUsize::new(0);
/// VSpaces created.
static VSPACES: AtomicUsize = AtomicUsize::new(0);

pub fn map_count() -> usize {
    MAPS.load(Ordering::Acquire)
}

pub fn unmap_count() -> usize {
    UNMAPS.load(Ordering::Acquire)
}

pub fn vspace_count() -> usize {
    VSPACES.load(Ordering::Acquire)
}

// ---- The kernel half ---------------------------------------------------

/// Bitmap of the upper-half PML4 entries present at boot.
///
/// Recorded by [`record_kernel_entries`] and checked on every VSpace
/// creation. ADR 027's invariant K2: if the kernel ever creates a *new*
/// top-level entry after boot, address spaces made before that point
/// would not have it, and a thread running in one would be unable to
/// see part of the kernel -- including, potentially, an interrupt
/// handler.
///
/// 256 bits for entries 256..512, as four `u64`s.
static KERNEL_ENTRY_MASK: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Whether [`record_kernel_entries`] has run.
static KERNEL_ENTRIES_RECORDED: AtomicU64 = AtomicU64::new(0);

/// Compute the present-entry bitmap of the kernel PML4's upper half.
///
/// # Safety
///
/// `table` must be the live kernel PML4.
unsafe fn upper_half_mask(table: &PageTable) -> [u64; 4] {
    let mut mask = [0u64; 4];
    for index in KERNEL_PML4_FIRST..paging::ENTRY_COUNT {
        // SAFETY: Index is in range by the loop bound, and the caller
        // guarantees the table is live.
        if unsafe { paging::entry_present(table, index) } {
            let bit = index - KERNEL_PML4_FIRST;
            mask[bit / 64] |= 1u64 << (bit % 64);
        }
    }
    mask
}

/// Snapshot which upper-half PML4 entries the kernel uses.
///
/// Called once, at the end of boot, after every kernel mapping exists.
///
/// # Safety
///
/// Call once, after `vm::init` and after the heap and MMIO regions have
/// been mapped.
pub unsafe fn record_kernel_entries() {
    // SAFETY: Caller's guarantee that boot mapping is complete.
    let mask = unsafe {
        let table = crate::memory::vm::kernel_table();
        upper_half_mask(table)
    };

    let mut count = 0u32;
    for (word, slot) in mask.iter().zip(KERNEL_ENTRY_MASK.iter()) {
        slot.store(*word, Ordering::Release);
        count += word.count_ones();
    }
    KERNEL_ENTRIES_RECORDED.store(1, Ordering::Release);

    println!(
        "[vspace] kernel half: {} of {} top-level entries in use; every \
         VSpace shares them (ADR 027)",
        count,
        paging::ENTRY_COUNT - KERNEL_PML4_FIRST
    );
}

/// Number of upper-half entries the kernel occupies.
pub fn kernel_entry_count() -> u32 {
    KERNEL_ENTRY_MASK
        .iter()
        .map(|word| word.load(Ordering::Acquire).count_ones())
        .sum()
}

/// Check the kernel's top-level entries have not changed since boot.
///
/// ADR 027's K2, enforced rather than assumed. A new entry here means
/// every VSpace created before now is missing part of the kernel, and
/// the symptom would be a fault inside an interrupt handler in one
/// address space but not another -- the sort of bug that takes days.
///
/// # Safety
///
/// `table` must be the live kernel PML4.
unsafe fn assert_kernel_entries_unchanged(table: &PageTable) {
    assert!(
        KERNEL_ENTRIES_RECORDED.load(Ordering::Acquire) != 0,
        "a VSpace was created before the kernel's top-level entries were \
         recorded; there is nothing to copy from"
    );

    // SAFETY: Caller's guarantee.
    let now = unsafe { upper_half_mask(table) };

    for (index, word) in now.iter().enumerate() {
        let expected = KERNEL_ENTRY_MASK[index].load(Ordering::Acquire);
        assert_eq!(
            *word, expected,
            "the kernel's top-level page-table entries changed after boot \
             (word {} is {:#018x}, was {:#018x}). Every VSpace created \
             before now is missing the new entry, so a thread in one of \
             them cannot see part of the kernel -- including possibly an \
             interrupt handler. See ADR 027, invariant K2.",
            index, word, expected
        );
    }
}

// ---- Creating a VSpace -------------------------------------------------

/// Turn a fresh PageTable capability into a user address space.
///
/// The table must have come from a `retype`, so it is already zeroed:
/// the user half starts empty, which is the point. The kernel's
/// top-level entries are then copied in, so that an interrupt taken
/// while this space is active can reach its handler.
///
/// Copying **entries**, not tables: everything below the PML4 is shared,
/// one physical copy for all address spaces. That is what makes a change
/// to a kernel mapping visible everywhere, and it is also why a user
/// mapping into the kernel half would corrupt every address space at
/// once -- see [`map_frame`]'s K1 check.
///
/// # Safety
///
/// `slot` must hold a live PageTable capability that nothing has mapped
/// anywhere.
pub unsafe fn create_vspace(slot: &Slot) -> Result<(), CapError> {
    // No thread switch while the kernel entries are being copied: a
    // half-populated PML4 that something switched into would be missing
    // most of the kernel (ADR 025).
    let _interrupts = crate::sched::InterruptGuard::new();

    // SAFETY: Caller's guarantee.
    let root = unsafe { root_table_from_slot(slot) }?;

    // SAFETY: Both tables are live and distinct -- the new one came from
    // a retype, the kernel's from vm::init.
    unsafe {
        let kernel = crate::memory::vm::kernel_table();
        assert_kernel_entries_unchanged(kernel);

        for index in KERNEL_PML4_FIRST..paging::ENTRY_COUNT {
            paging::copy_entry(kernel, root, index);
        }
    }

    VSPACES.fetch_add(1, Ordering::AcqRel);
    Ok(())
}

/// Borrow the PML4 a PageTable capability names.
///
/// # Safety
///
/// `slot` must hold a live PageTable capability, and the caller must not
/// create a second mutable borrow of the same table.
unsafe fn root_table_from_slot(slot: &Slot) -> Result<&'static mut PageTable, CapError> {
    if slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if slot.cap_type() != CapType::PageTable {
        return Err(CapError::WrongType {
            found: slot.cap_type(),
        });
    }

    let phys = slot.object().0;
    debug_assert!(
        phys.0.is_multiple_of(crate::memory::FRAME_SIZE),
        "a PageTable object must be frame-aligned"
    );

    // SAFETY: The capability names a 4 KiB, frame-aligned object that
    // retype zeroed, reachable through PMAP. The caller promises not to
    // alias it.
    Ok(unsafe { &mut *crate::memory::phys_to_virt(phys).as_ptr::<PageTable>() })
}

/// Physical address of the root table a capability names.
pub fn root_address(slot: &Slot) -> Result<PhysAddr, CapError> {
    if slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if slot.cap_type() != CapType::PageTable {
        return Err(CapError::WrongType {
            found: slot.cap_type(),
        });
    }
    Ok(slot.object().0)
}

// ---- Rights and permissions --------------------------------------------

/// What a mapping is allowed to do (ADR 032).
///
/// Three variants, and the absence of a fourth is the point. "Readable,
/// writable **and** executable" is not refused at run time; it cannot
/// be written down. W^X is a property of the type rather than of a
/// check somebody has to remember — the same choice ADR 012 made for
/// `TlbFlush`.
///
/// A `PageFlags` argument with an `if write && exec { return Err }`
/// would do the same job and could be deleted in a refactor. A missing
/// enum variant cannot be deleted; removing it breaks the build.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
// Every variant starts with `Read`, and clippy suggests dropping the
// prefix. Keeping it: the shared prefix *is* the invariant. Every
// mapping is readable, and what varies is the second permission --
// which is the whole content of ADR 032. `Only`, `Write` and
// `Execute` would read as three unrelated things.
#[allow(clippy::enum_variant_names)]
pub enum MapPermissions {
    /// Readable. Not writable, not executable.
    ReadOnly,
    /// Readable and writable. Not executable.
    ReadWrite,
    /// Readable and executable. **Not writable.**
    ///
    /// Needs only `Rights::READ`: making a page executable does not
    /// change what is in it, and putting something there needs
    /// `WRITE`. The two steps are what W^X separates.
    ReadExecute,
    /// Readable and writable, **write-combining** (M14, ADR 051).
    ///
    /// The same authority as [`ReadWrite`](Self::ReadWrite) — it
    /// needs `Rights::WRITE` and nothing more — and a different cache
    /// type. That split is deliberate: a cache type is not a
    /// permission, and asking for write-combining grants no access
    /// the holder did not already have.
    ///
    /// It is a separate variant rather than a flag on `ReadWrite`
    /// because the enum is what the ABI's `perm` numbers map onto,
    /// and a component says which of these it wants with one number.
    ReadWriteCombining,
}

impl MapPermissions {
    pub const fn name(self) -> &'static str {
        match self {
            MapPermissions::ReadOnly => "r--",
            MapPermissions::ReadWrite => "rw-",
            MapPermissions::ReadExecute => "r-x",
            MapPermissions::ReadWriteCombining => "rw- wc",
        }
    }
}

/// Translate capability rights and a requested permission into page
/// flags (ADR 027 for the rights, ADR 032 for the permissions).
///
/// Two gates, and both must open. The capability says what *may* be
/// granted; the argument says what is *being asked for*.
///
/// `GRANT` and `GRANTREPLY` have no effect, which is worth stating
/// because `GRANT` reads like "may hand this on". It is not: ADR 021
/// defines it as the right to carry *capabilities* over an Endpoint,
/// and it says nothing about mapping.
///
/// # The M7 behaviour that changed
///
/// M7 mapped read-only when `WRITE` was missing, silently. That is now
/// an error. A caller that asked for a writable mapping and got a
/// read-only one finds out at its first store — a #PF whose cause is
/// in a different function, possibly a different milestone. Refusing
/// at the point of the request puts the diagnosis where the mistake
/// is.
fn permissions_to_flags(rights: Rights, requested: MapPermissions) -> Result<PageFlags, CapError> {
    if !rights.contains(Rights::READ) {
        // A mapping nothing may touch is not a weaker mapping, it is a
        // meaningless one -- and `PageFlags` rejects it a layer down
        // anyway.
        return Err(CapError::InsufficientRights {
            needed: Rights::READ,
            held: rights,
        });
    }

    let mut flags = PageFlags::READ | PageFlags::USER;

    match requested {
        MapPermissions::ReadOnly => {}
        MapPermissions::ReadWrite => {
            if !rights.contains(Rights::WRITE) {
                return Err(CapError::InsufficientRights {
                    needed: Rights::WRITE,
                    held: rights,
                });
            }
            flags |= PageFlags::WRITE;
        }
        MapPermissions::ReadExecute => {
            flags |= PageFlags::EXEC;
        }
        MapPermissions::ReadWriteCombining => {
            // The same rights test as ReadWrite. A cache type is not
            // an authority: write-combining lets the holder write no
            // byte it could not already write, it only changes when
            // those writes reach the device.
            if !rights.contains(Rights::WRITE) {
                return Err(CapError::InsufficientRights {
                    needed: Rights::WRITE,
                    held: rights,
                });
            }
            flags |= PageFlags::WRITE | PageFlags::WRITE_COMBINING;
        }
    }

    // The invariant this function exists to keep. It cannot fail --
    // no arm above sets both -- which is exactly why it is a
    // `debug_assert` rather than a returned error: if it ever fires,
    // the enum has grown a variant that should not exist.
    debug_assert!(
        !(flags.contains(PageFlags::WRITE) && flags.contains(PageFlags::EXEC)),
        "a mapping is both writable and executable; W^X is supposed to \
         be unrepresentable (ADR 032)"
    );

    Ok(flags)
}

// ---- Mapping a frame ---------------------------------------------------

/// `Frame.Map`: place a Frame at an address in a VSpace.
///
/// The intermediate tables must already be there. If one is missing the
/// call fails with [`CapError::MissingPageTable`] and the caller maps a
/// PageTable at that level first -- the kernel will not spend a frame
/// the caller did not offer (ADR 027).
///
/// `permissions` says what the mapping may do, within what the Frame
/// capability's rights allow (ADR 032). Asking for more than the
/// capability holds is an error rather than a silent downgrade.
///
/// # Safety
///
/// Both slots must hold live capabilities, and the caller must not hold
/// another mutable borrow of the root table.
pub unsafe fn map_frame(
    root_slot: &Slot,
    frame_slot: &Slot,
    address: VirtAddr,
    permissions: MapPermissions,
) -> Result<(), CapError> {
    let _interrupts = crate::sched::InterruptGuard::new();

    if frame_slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if frame_slot.cap_type() != CapType::Frame {
        return Err(CapError::WrongType {
            found: frame_slot.cap_type(),
        });
    }

    let flags = permissions_to_flags(frame_slot.rights(), permissions)?;

    // K1 (ADR 027): a user VSpace takes user addresses only.
    //
    // The paging layer checks something related but not identical --
    // that the USER flag agrees with the half. This check is about
    // *ownership*: the kernel's top-level entries are shared by every
    // address space, so a mapping placed in the kernel half through a
    // user VSpace would edit a table that every other VSpace is also
    // using. The result is not a broken isolation boundary but a broken
    // kernel.
    //
    // Both checks are kept, and both are tested. Neither makes the
    // other redundant.
    //
    // # M11's mandatory mutation
    //
    // Deleting this check does **not** let a user mapping into the
    // kernel half: the paging layer still refuses it, with
    // `FlagsWrongHalf` (error 16) where this gives `NotUserAddress`
    // (15). `vspace-user-kernel-half` fails on exactly that
    // difference.
    //
    // Which is the interesting part. The mutation is caught, but by
    // the *other* check -- so if the two shared an error code, it
    // would have survived, and a reader would have concluded that
    // one of them was redundant. ADR 027 argued for keeping both and
    // M7 found by mutation that they must be distinguishable; this
    // is the same finding arriving from the user path, and it is
    // what the test asserts `15` rather than "an error".
    //
    // The checks differ in what they protect. Remove this one and
    // the kernel half is still safe *today*, because every user
    // mapping happens to carry the USER flag. It would stop being
    // safe the moment something mapped into a user VSpace without
    // it -- and nothing would then notice.
    if !layout::is_user(address.0) {
        return Err(CapError::NotUserAddress { address: address.0 });
    }
    if !address.is_frame_aligned() {
        return Err(CapError::InvalidArgument);
    }

    let frame = PhysFrame::containing(frame_slot.object().0);

    // SAFETY: Caller's guarantee.
    let root = unsafe { root_table_from_slot(root_slot) }?;

    // SAFETY: `root` is a live PML4. `NoFrames` means a missing
    // intermediate table is reported rather than created.
    let result = unsafe { paging::map(root, address, frame, flags, &mut NoFrames) };

    match result {
        Ok(tlb) => {
            tlb.flush();
            MAPS.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
        Err(paging::MapError::OutOfFrames) => {
            Err(CapError::MissingPageTable { address: address.0 })
        }
        Err(paging::MapError::AlreadyMapped { .. }) => {
            Err(CapError::AlreadyMapped { address: address.0 })
        }
        // The paging layer's own half check, reported under its own
        // name rather than folded into `NotUserAddress`.
        //
        // The two are checking different things and the distinction
        // only became visible under mutation. Deleting K1 above left
        // this scenario passing, because `paging::map` refused the
        // same four addresses and the test could not tell which guard
        // had fired. A check indistinguishable from its own absence is
        // not a check (ADR 023, K5).
        //
        // K1 is about *ownership*: the kernel's top-level entries are
        // shared by every address space, so a user mapping there would
        // edit tables every other VSpace walks. The paging layer's
        // check is about *flag consistency*: USER set on a kernel
        // address. Both are worth having; only separate error values
        // make it possible to prove both are present.
        Err(paging::MapError::WrongHalf { .. }) => {
            Err(CapError::FlagsWrongHalf { address: address.0 })
        }
        Err(_) => Err(CapError::InvalidArgument),
    }
}

/// `Frame.Unmap`: remove a mapping.
///
/// The Frame capability is untouched: it belongs to whoever holds it,
/// and unmapping is not destruction.
///
/// # Safety
///
/// As [`map_frame`].
pub unsafe fn unmap_frame(root_slot: &Slot, address: VirtAddr) -> Result<PhysAddr, CapError> {
    let _interrupts = crate::sched::InterruptGuard::new();

    if !layout::is_user(address.0) {
        return Err(CapError::NotUserAddress { address: address.0 });
    }

    // SAFETY: Caller's guarantee.
    let root = unsafe { root_table_from_slot(root_slot) }?;

    // SAFETY: `root` is a live PML4.
    match unsafe { paging::unmap(root, address) } {
        Ok((frame, tlb)) => {
            tlb.flush();
            UNMAPS.fetch_add(1, Ordering::AcqRel);
            Ok(frame.start())
        }
        Err(paging::UnmapError::NotMapped) => Err(CapError::NotMapped { address: address.0 }),
        Err(_) => Err(CapError::InvalidArgument),
    }
}

/// `PageTable.Map`: place an intermediate table into a VSpace.
///
/// `level` is 3 for a PDPT, 2 for a PD, 1 for a PT -- the level of the
/// table being inserted, not of the entry that will point at it.
///
/// # Safety
///
/// Both slots must hold live capabilities.
pub unsafe fn map_table(
    root_slot: &Slot,
    table_slot: &Slot,
    address: VirtAddr,
    level: u8,
) -> Result<(), CapError> {
    let _interrupts = crate::sched::InterruptGuard::new();

    if table_slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if table_slot.cap_type() != CapType::PageTable {
        return Err(CapError::WrongType {
            found: table_slot.cap_type(),
        });
    }
    if !(1..=3).contains(&level) {
        return Err(CapError::InvalidArgument);
    }
    if !layout::is_user(address.0) {
        return Err(CapError::NotUserAddress { address: address.0 });
    }

    let table_frame = PhysFrame::containing(table_slot.object().0);

    // SAFETY: Caller's guarantee.
    let root = unsafe { root_table_from_slot(root_slot) }?;

    // SAFETY: `root` is a live PML4 and `table_frame` holds a zeroed
    // table from a retype.
    match unsafe { paging::insert_table(root, address, table_frame, level, true) } {
        Ok(()) => Ok(()),
        Err(paging::MapError::AlreadyMapped { .. }) => {
            Err(CapError::AlreadyMapped { address: address.0 })
        }
        Err(paging::MapError::OutOfFrames) => {
            Err(CapError::MissingPageTable { address: address.0 })
        }
        Err(_) => Err(CapError::InvalidArgument),
    }
}

/// `PageTable.Unmap`: take an intermediate table back out.
///
/// The table's capability is unaffected; the caller may map it
/// elsewhere, or revoke it to return the memory. This is where ADR 015's
/// long-standing question went away: the kernel never frees a table,
/// because the kernel never owned one.
///
/// # Safety
///
/// As [`map_table`].
pub unsafe fn unmap_table(
    root_slot: &Slot,
    address: VirtAddr,
    level: u8,
) -> Result<PhysAddr, CapError> {
    let _interrupts = crate::sched::InterruptGuard::new();

    if !(1..=3).contains(&level) {
        return Err(CapError::InvalidArgument);
    }
    if !layout::is_user(address.0) {
        return Err(CapError::NotUserAddress { address: address.0 });
    }

    // SAFETY: Caller's guarantee.
    let root = unsafe { root_table_from_slot(root_slot) }?;

    // SAFETY: `root` is a live PML4.
    match unsafe { paging::remove_table(root, address, level) } {
        Ok(frame) => {
            // A whole subtree just left the address space, so one page
            // is not enough: flush everything non-global. The kernel's
            // own mappings are GLOBAL and survive, which is the point
            // of marking them so (ADR 027).
            paging::flush_all();
            Ok(frame.start())
        }
        Err(paging::UnmapError::NotMapped) => Err(CapError::NotMapped { address: address.0 }),
        Err(_) => Err(CapError::InvalidArgument),
    }
}

/// Look up an address in a VSpace, through the capability path.
///
/// Exists so the acceptance test can check that a mapping made through
/// capabilities agrees with what M3's `translate` reports -- the two
/// paths must describe the same page table, because they *are* the same
/// page table.
///
/// # Safety
///
/// `root_slot` must hold a live PageTable capability.
pub unsafe fn translate(
    root_slot: &Slot,
    address: VirtAddr,
) -> Result<(PhysAddr, PageFlags), CapError> {
    // SAFETY: Caller's guarantee.
    let root = unsafe { root_table_from_slot(root_slot) }?;

    // SAFETY: `root` is a live PML4.
    match unsafe { paging::translate(root, address) } {
        Ok(pair) => Ok(pair),
        Err(paging::TranslateError::NotMapped) => Err(CapError::NotMapped { address: address.0 }),
        Err(_) => Err(CapError::InvalidArgument),
    }
}

/// Tear down an address space by revoking the Untyped its objects came
/// from.
///
/// # Why the Untyped and not the root
///
/// The first version of this took the root PageTable capability and
/// revoked *that*, on the reading of ADR 027 that a VSpace is its root.
/// It deleted nothing and returned no memory, and the reason is worth
/// recording because the mistake is easy to repeat.
///
/// Revoke removes a capability's **descendants** (ADR 019). The root,
/// the three intermediate tables and the Frame are all children of the
/// same *Untyped* -- they are siblings, not a chain. The root is their
/// peer, so revoking it reaches none of them.
///
/// "A VSpace" is a shape built out of capabilities, not a single
/// object, and nothing in the derivation tree records that shape. The
/// thing that owns all of it is the Untyped, and that is what has to be
/// revoked. This matches ADR 020's decision that memory comes back by
/// resetting a watermark rather than object by object.
///
/// # The refusal
///
/// Refused if the root is the table in CR3 right now (ADR 028, case A).
/// Revoke resets the watermark, so the live page tables would be handed
/// out again while the CPU is still walking them -- not a fault but
/// undefined behaviour, and one no interrupt could report, because
/// reaching the IDT goes through those same tables.
///
/// Threads bound to this VSpace that are *not* running are left alone.
/// They are stopped when something next tries to switch into them,
/// which costs one comparison there instead of a sweep of every TCB
/// here. ADR 019 already measured revoke in seconds; adding a scan is
/// the wrong direction.
///
/// # Safety
///
/// `untyped_slot` must hold a live Untyped capability, and `root_slot`
/// a PageTable derived from it.
pub unsafe fn revoke_vspace(
    untyped_slot: &mut Slot,
    root_slot: &Slot,
) -> Result<super::cnode::RevokeReport, CapError> {
    let root = root_address(root_slot)?;

    if crate::sched::current_vspace() == root.0 {
        return Err(CapError::VSpaceInUse { root: root.0 });
    }

    // SAFETY: Caller's guarantee, and the root is not the active table
    // -- so nothing the CPU is walking is about to be freed.
    unsafe { super::untyped::revoke_untyped(untyped_slot) }
}

/// Report what the VSpace layer has done, for the acceptance test.
pub fn report() {
    println!(
        "[vspace] {} created, {} mappings, {} unmappings, {} kernel \
         top-level entries shared",
        vspace_count(),
        map_count(),
        unmap_count(),
        kernel_entry_count()
    );
}
