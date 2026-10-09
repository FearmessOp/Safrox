// SPDX-License-Identifier: MPL-2.0
//! Resolving a capability pointer to a slot.
//!
//! This is the first thing every syscall does, so it is written to have a
//! worst case rather than an average: **no loop, two memory accesses,
//! seven comparisons**. ADR 018 explains why that mattered more than the
//! flexibility of seL4's guarded radix tree.
//!
//! ```text
//!  63      56 55      48 47            32 31            16 15          0
//! +----------+----------+----------------+---------------+-------------+
//! | L1 gen   | L0 gen   | reserved, zero |   L1 index    |  L0 index   |
//! |  8 bit   |  8 bit   |     16 bit     |    16 bit     |   16 bit    |
//! +----------+----------+----------------+---------------+-------------+
//! ```
//!
//! Two properties hold together, and the second was missing from the
//! first draft of ADR 018:
//!
//! * **Exactly two levels.** A root CNode slot may only hold a CNode, so
//!   the meaning of the two index fields never depends on the shape of
//!   the tree.
//! * **Generations.** Each slot counts how often it has been emptied, and
//!   the CPtr carries the generation it was issued against. A slot reused
//!   since then no longer answers to the old pointer.
//!
//! Together they give the invariant the acceptance test checks:
//!
//! > A CPtr resolves to the same slot every time, or it returns an error.
//! > It never resolves to a different slot.

use super::cnode::{slot_at, Slot};
use super::{CapError, CapType};
use crate::memory::PhysAddr;

/// A capability pointer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CPtr(pub u64);

const L0_INDEX_SHIFT: u32 = 0;
const L1_INDEX_SHIFT: u32 = 16;
const RESERVED_SHIFT: u32 = 32;
const L0_GEN_SHIFT: u32 = 48;
const L1_GEN_SHIFT: u32 = 56;
const INDEX_MASK: u64 = 0xFFFF;
const GEN_MASK: u64 = 0xFF;
const RESERVED_MASK: u64 = 0xFFFF;

impl CPtr {
    /// Build a CPtr from its parts.
    ///
    /// The generations come from the slots being addressed; a caller that
    /// invents them gets `StaleCPtr`, which is the point.
    pub const fn new(l1_index: u16, l1_generation: u8, l0_index: u16, l0_generation: u8) -> CPtr {
        CPtr(
            ((l1_generation as u64) << L1_GEN_SHIFT)
                | ((l0_generation as u64) << L0_GEN_SHIFT)
                | ((l1_index as u64) << L1_INDEX_SHIFT)
                | ((l0_index as u64) << L0_INDEX_SHIFT),
        )
    }

    pub const fn l1_index(self) -> usize {
        ((self.0 >> L1_INDEX_SHIFT) & INDEX_MASK) as usize
    }

    pub const fn l0_index(self) -> usize {
        ((self.0 >> L0_INDEX_SHIFT) & INDEX_MASK) as usize
    }

    pub const fn l1_generation(self) -> u8 {
        ((self.0 >> L1_GEN_SHIFT) & GEN_MASK) as u8
    }

    pub const fn l0_generation(self) -> u8 {
        ((self.0 >> L0_GEN_SHIFT) & GEN_MASK) as u8
    }

    const fn reserved(self) -> u64 {
        (self.0 >> RESERVED_SHIFT) & RESERVED_MASK
    }

    /// The same pointer with a different leaf generation.
    ///
    /// Used by the tests to build a deliberately stale pointer.
    pub const fn with_l0_generation(self, generation: u8) -> CPtr {
        CPtr((self.0 & !(GEN_MASK << L0_GEN_SHIFT)) | ((generation as u64) << L0_GEN_SHIFT))
    }
}

impl core::fmt::Display for CPtr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:#018x} (L1 {}/g{}, L0 {}/g{})",
            self.0,
            self.l1_index(),
            self.l1_generation(),
            self.l0_index(),
            self.l0_generation()
        )
    }
}

/// The root of a CSpace: a CNode whose slots hold only CNodes.
#[derive(Clone, Copy)]
pub struct CSpace {
    root: PhysAddr,
    root_slots: usize,
}

impl CSpace {
    /// # Safety
    ///
    /// `root` must point at `1 << radix` live slots that will outlive
    /// this value.
    pub const unsafe fn new(root: PhysAddr, radix: u8) -> CSpace {
        CSpace {
            root,
            root_slots: 1usize << radix,
        }
    }

    /// How many slots the root CNode has.
    ///
    /// The tests use this to derive an out-of-range index from the real
    /// size rather than inventing a number that happens to be too big
    /// today and might not be tomorrow.
    pub const fn root_slots(&self) -> usize {
        self.root_slots
    }

    /// Borrow a root slot directly, bypassing CPtr resolution.
    ///
    /// For boot code, which populates the root before any CPtr exists.
    ///
    /// # Safety
    ///
    /// `index` must be in range, and no other mutable borrow of the slot
    /// may be live.
    pub unsafe fn root_slot(&self, index: usize) -> &'static mut Slot {
        debug_assert!(index < self.root_slots);
        // SAFETY: Index in range by the caller's guarantee; the root is a
        // contiguous array of slots.
        unsafe { slot_at(PhysAddr(self.root.0 + (index * super::SLOT_SIZE) as u64)) }
    }
}

/// Resolve a CPtr to the slot it names.
///
/// No loop. Returns a distinct error for each way it can fail, so that a
/// caller -- and from M11, a user component -- can tell a typo from a
/// stale pointer from a structural mistake.
///
/// # Safety
///
/// `space` must describe a live CSpace, and the caller must not hold
/// another mutable borrow of the resolved slot.
pub unsafe fn resolve(space: &CSpace, cptr: CPtr) -> Result<&'static mut Slot, CapError> {
    // 1. The reserved field. Checked first because it costs nothing and
    //    catches a pointer that was never valid, as opposed to one that
    //    went stale. Keeping the field zero today is what lets it mean
    //    something tomorrow (ADR 018).
    if cptr.reserved() != 0 {
        return Err(CapError::InvalidCPtr);
    }

    let l1_index = cptr.l1_index();
    if l1_index >= space.root_slots {
        return Err(CapError::CPtrOutOfRange);
    }

    // SAFETY: Index checked; the root is an array of live slots.
    let inner = unsafe { space.root_slot(l1_index) };

    // 2. The intermediate slot's generation. Checked before anything is
    //    read out of it: if this CNode was replaced since the pointer was
    //    issued, nothing below is what the holder meant.
    if inner.generation() != cptr.l1_generation() {
        return Err(CapError::StaleCPtr);
    }
    if inner.is_empty() {
        return Err(CapError::EmptySlot);
    }

    // 3. Exactly two levels. This is the check that was missing from the
    //    first draft of ADR 018, and its absence meant a root slot whose
    //    type changed would silently redirect an old CPtr into a
    //    different object's slot zero.
    if inner.cap_type() != CapType::CNode {
        return Err(CapError::RootSlotMustBeCNode);
    }

    let radix = inner.as_radix();
    let leaf_slots = 1usize << radix;
    let l0_index = cptr.l0_index();
    if l0_index >= leaf_slots {
        return Err(CapError::CPtrOutOfRange);
    }

    let leaf_addr = PhysAddr(inner.object().0 .0 + (l0_index * super::SLOT_SIZE) as u64);
    // SAFETY: The CNode capability names `1 << radix` live slots and the
    // index is in range.
    let leaf = unsafe { slot_at(leaf_addr) };

    // 4. The leaf's generation. Catches the ordinary case: this slot held
    //    something else when the pointer was handed out.
    if leaf.generation() != cptr.l0_generation() {
        return Err(CapError::StaleCPtr);
    }

    Ok(leaf)
}

/// Resolve and require a particular type.
///
/// Most invocations want this: "give me the Endpoint at this CPtr". The
/// type check lives here so that every call site reports it the same way.
///
/// # Safety
///
/// As [`resolve`].
pub unsafe fn resolve_typed(
    space: &CSpace,
    cptr: CPtr,
    expected: CapType,
) -> Result<&'static mut Slot, CapError> {
    // SAFETY: Caller's guarantee.
    let slot = unsafe { resolve(space, cptr) }?;
    if slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if slot.cap_type() != expected {
        return Err(CapError::WrongType {
            found: slot.cap_type(),
        });
    }
    Ok(slot)
}

/// Build the CPtr that currently addresses a given leaf slot.
///
/// The generations are read from the slots as they are *now*, so the
/// pointer is valid until one of them is emptied. Boot code and tests use
/// this; a user component receives CPtrs rather than constructing them.
///
/// # Safety
///
/// `space` must be live and both indices in range.
pub unsafe fn address_of(space: &CSpace, l1_index: usize, l0_index: usize) -> Option<CPtr> {
    if l1_index >= space.root_slots {
        return None;
    }
    // SAFETY: Index checked.
    let inner = unsafe { space.root_slot(l1_index) };
    if inner.is_empty() || inner.cap_type() != CapType::CNode {
        return None;
    }

    let radix = inner.as_radix();
    if l0_index >= (1usize << radix) {
        return None;
    }

    let leaf_addr = PhysAddr(inner.object().0 .0 + (l0_index * super::SLOT_SIZE) as u64);
    // SAFETY: In range, as checked.
    let leaf = unsafe { slot_at(leaf_addr) };

    Some(CPtr::new(
        l1_index as u16,
        inner.generation(),
        l0_index as u16,
        leaf.generation(),
    ))
}
