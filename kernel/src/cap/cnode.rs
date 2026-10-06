// SPDX-License-Identifier: BSD-2-Clause
//! Slots, CNodes, and the derivation list.
//!
//! A slot is 32 bytes and carries everything about one capability: what
//! object it names, what may be done with it, how deep in the derivation
//! tree it sits, which generation of the slot it belongs to, and its two
//! links in the derivation list.
//!
//! The derivation list is the whole of ADR 019: all capabilities in the
//! system sit in one doubly linked list, in the preorder of the
//! derivation tree. That ordering is what makes `revoke` a forward scan
//! rather than a tree walk -- a capability's descendants are exactly the
//! run of entries that follow it with a greater depth.

use core::fmt;

use super::{CapError, CapType, ObjectRef, Rights, MAX_DERIVATION_DEPTH, SLOT_SIZE};
use crate::memory::PhysAddr;

/// A capability slot.
///
/// ```text
///   +---------------------------+---------------------------+
///   | object: PhysAddr     8 B  | type 5 | rights 4 |   8 B  |
///   |                           | depth 4 | gen 8 | pad      |
///   +---------------------------+---------------------------+
///   | mdb_prev: PhysAddr   8 B  | mdb_next: PhysAddr   8 B   |
///   +---------------------------+---------------------------+
/// ```
///
/// The links are physical addresses of other slots rather than Rust
/// references: a slot lives inside a CNode object, which lives in memory
/// carved out of an Untyped, and there is no lifetime that describes
/// that. Every traversal goes through the physical window.
#[derive(Clone, Copy)]
#[repr(C, align(32))]
pub struct Slot {
    /// The object this capability names, or `ObjectRef::NULL` if empty.
    object: ObjectRef,
    /// Packed: type (bits 0-4), rights (5-8), depth (9-12),
    /// generation (16-23). See the accessors.
    packed: u64,
    /// Previous slot in the derivation list, by physical address.
    mdb_prev: PhysAddr,
    /// Next slot in the derivation list, by physical address.
    mdb_next: PhysAddr,
}

const TYPE_SHIFT: u32 = 0;
const TYPE_MASK: u64 = 0x1F;
const RIGHTS_SHIFT: u32 = 5;
const RIGHTS_MASK: u64 = 0xF;
const DEPTH_SHIFT: u32 = 9;
const DEPTH_MASK: u64 = 0xF;
/// Device-memory flag, bit 13: next to the depth, where there was a
/// spare bit. Only Untyped and the Frames carved from it ever set it.
const DEVICE_BIT: u32 = 13;
const GENERATION_SHIFT: u32 = 16;
const GENERATION_MASK: u64 = 0xFF;
/// Badge/size lives in the top 32 bits: a badge for Endpoint and
/// Notification, `size_bits` for Untyped, `radix` for CNode. They never
/// coexist, so one field serves all three.
const EXTRA_SHIFT: u32 = 32;

const _: () = {
    assert!(
        core::mem::size_of::<Slot>() == SLOT_SIZE,
        "Slot must be exactly 32 bytes; ADR 019's budget depends on it"
    );
    assert!(
        core::mem::align_of::<Slot>() == 32,
        "Slot must be 32-byte aligned so a CNode is an array of them"
    );
};

impl Slot {
    pub const fn is_empty(&self) -> bool {
        self.object.is_null()
    }

    pub fn cap_type(&self) -> CapType {
        let bits = ((self.packed >> TYPE_SHIFT) & TYPE_MASK) as u8;
        // A slot is only ever written by this module, so an unknown type
        // means memory corruption rather than a bad argument. Stopping
        // here keeps the damage local.
        CapType::from_bits(bits).unwrap_or_else(|| {
            panic!(
                "capability slot holds type {}, which no object uses; the \
                 slot has been corrupted",
                bits
            )
        })
    }

    pub fn rights(&self) -> Rights {
        Rights::from_bits(((self.packed >> RIGHTS_SHIFT) & RIGHTS_MASK) as u8)
    }

    pub fn depth(&self) -> u8 {
        ((self.packed >> DEPTH_SHIFT) & DEPTH_MASK) as u8
    }

    /// Which generation of this slot the current occupant belongs to.
    ///
    /// Incremented every time the slot is emptied, so a CPtr handed out
    /// before then no longer resolves (ADR 018).
    pub fn generation(&self) -> u8 {
        ((self.packed >> GENERATION_SHIFT) & GENERATION_MASK) as u8
    }

    /// Badge (Endpoint, Notification), `size_bits` (Untyped) or `radix`
    /// (CNode), depending on the type.
    pub fn extra(&self) -> u32 {
        (self.packed >> EXTRA_SHIFT) as u32
    }

    pub fn object(&self) -> ObjectRef {
        self.object
    }

    /// Whether this is a device Untyped (ADR 017).
    ///
    /// Stored in bit 13, next to the depth: only Untyped uses it and
    /// there was a spare bit.
    pub fn is_device(&self) -> bool {
        self.packed & (1 << DEVICE_BIT) != 0
    }

    /// Fill an empty slot.
    ///
    /// The generation is *preserved*, not reset: it counts how many times
    /// this slot has been reused, and resetting it would defeat the whole
    /// mechanism.
    fn fill(
        &mut self,
        object: ObjectRef,
        cap_type: CapType,
        rights: Rights,
        depth: u8,
        extra: u32,
        device: bool,
    ) {
        debug_assert!(self.is_empty(), "filling an occupied slot");
        debug_assert!(depth <= MAX_DERIVATION_DEPTH);

        let generation = self.generation();
        self.object = object;
        self.packed = ((cap_type as u64) << TYPE_SHIFT)
            | ((rights.bits() as u64) << RIGHTS_SHIFT)
            | ((depth as u64) << DEPTH_SHIFT)
            | ((device as u64) << DEVICE_BIT)
            | ((generation as u64) << GENERATION_SHIFT)
            | ((extra as u64) << EXTRA_SHIFT);
    }

    /// Empty the slot and advance its generation.
    ///
    /// The derivation links are *not* touched here: unlinking is the
    /// caller's job, because it needs to see the neighbours.
    fn clear(&mut self) {
        // Wrapping is deliberate and bounded: eight bits means a stale
        // CPtr can be mistaken for a live one only after 256 reuses of
        // the same slot. ADR 018 states this as a limited guarantee
        // rather than an absolute one.
        let next_generation = self.generation().wrapping_add(1);
        self.object = ObjectRef::NULL;
        self.packed = (next_generation as u64) << GENERATION_SHIFT;
    }

    /// Physical address of this slot, for the derivation links.
    ///
    /// # Safety
    ///
    /// `self` must live inside a CNode reached through the physical
    /// window, which is true of every slot the kernel touches.
    unsafe fn phys_addr(&self) -> PhysAddr {
        let virt = core::ptr::from_ref(self) as u64;
        PhysAddr(virt - crate::memory::pmap_base())
    }
}

impl fmt::Debug for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            write!(f, "Slot(empty, gen {})", self.generation())
        } else {
            write!(
                f,
                "Slot({} {} at {}, depth {}, gen {})",
                self.cap_type(),
                // `{:?}` on Rights prints R|W|G form.
                DebugRights(self.rights()),
                self.object,
                self.depth(),
                self.generation()
            )
        }
    }
}

/// Wrapper so `Slot`'s Debug can print rights inline.
struct DebugRights(Rights);

impl fmt::Display for DebugRights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

// ---- Reaching slots through the window ---------------------------------

/// Borrow the slot at a physical address.
///
/// # Safety
///
/// `addr` must be the address of a live slot inside a CNode, and the
/// caller must not create a second mutable borrow of it.
pub unsafe fn slot_at<'a>(addr: PhysAddr) -> &'a mut Slot {
    debug_assert!(addr.0 != 0, "null slot address");
    debug_assert!(
        (addr.0 as usize).is_multiple_of(SLOT_SIZE),
        "slot address is not 32-byte aligned"
    );
    // SAFETY: The caller guarantees the address names a live slot. PMAP
    // maps all RAM writably, and slots are 32-byte aligned by
    // construction, which satisfies `Slot`'s alignment.
    unsafe { &mut *crate::memory::phys_to_virt(addr).as_ptr::<Slot>() }
}

/// A CNode: an array of slots.
///
/// Not a Rust type with a length -- the length is `1 << radix` and lives
/// in the capability that names the CNode, not in the object. That keeps
/// the object itself a plain array, which is what retype produces.
pub struct CNode {
    base: PhysAddr,
    slots: usize,
}

impl CNode {
    /// # Safety
    ///
    /// `base` must point at `1 << radix` live slots.
    pub unsafe fn new(base: PhysAddr, radix: u8) -> CNode {
        CNode {
            base,
            slots: 1usize << radix,
        }
    }

    pub fn len(&self) -> usize {
        self.slots
    }

    /// Borrow one slot.
    ///
    /// # Safety
    ///
    /// As [`slot_at`]; additionally `index` must be in range, which the
    /// caller is expected to have checked so that the error can be
    /// reported rather than panicked.
    pub unsafe fn slot(&self, index: usize) -> &'static mut Slot {
        debug_assert!(index < self.slots);
        // SAFETY: Index in range by the caller's guarantee, and the CNode
        // is a contiguous array of slots.
        unsafe { slot_at(PhysAddr(self.base.0 + (index * SLOT_SIZE) as u64)) }
    }
}

// ---- The derivation list -----------------------------------------------

/// Insert `child` into the derivation list immediately after `parent`.
///
/// Preorder is maintained by this one rule: a new derivative always goes
/// *directly* after the capability it derives from. Siblings therefore
/// appear in reverse creation order, which does not matter -- what
/// matters is that every descendant of a node appears in the contiguous
/// run following it, which this preserves.
///
/// # Safety
///
/// Both slots must be live, and `child` must not already be in a list.
unsafe fn link_after(parent: &mut Slot, child: &mut Slot) {
    // SAFETY: Both slots live inside CNodes reached through the window.
    let parent_addr = unsafe { parent.phys_addr() };
    // SAFETY: As above.
    let child_addr = unsafe { child.phys_addr() };

    let after = parent.mdb_next;

    child.mdb_prev = parent_addr;
    child.mdb_next = after;
    parent.mdb_next = child_addr;

    if after.0 != 0 {
        // SAFETY: A non-zero link always names a live slot -- unlink
        // clears the links of whatever it removes.
        let after_slot = unsafe { slot_at(after) };
        after_slot.mdb_prev = child_addr;
    }
}

/// Remove a slot from the derivation list.
///
/// # Safety
///
/// `slot` must be live and in a list.
unsafe fn unlink(slot: &mut Slot) {
    let prev = slot.mdb_prev;
    let next = slot.mdb_next;

    if prev.0 != 0 {
        // SAFETY: Non-zero links name live slots.
        unsafe { slot_at(prev) }.mdb_next = next;
    }
    if next.0 != 0 {
        // SAFETY: As above.
        unsafe { slot_at(next) }.mdb_prev = prev;
    }

    slot.mdb_prev = PhysAddr(0);
    slot.mdb_next = PhysAddr(0);
}

// ---- Operations --------------------------------------------------------

/// Copy a capability into an empty slot, narrowing its rights.
///
/// `copy` is the only way rights are ever reduced; there is no in-place
/// narrowing, because that would have to affect the copy's descendants
/// too and so would need an unbounded tree walk (ADR 021).
///
/// # Safety
///
/// Both slots must be live and distinct.
pub unsafe fn copy(src: &mut Slot, dest: &mut Slot, rights: Rights) -> Result<(), CapError> {
    if src.is_empty() {
        return Err(CapError::EmptySlot);
    }
    // Overwriting an occupied slot would destroy a capability without
    // unlinking it, leaving the derivation list pointing at a slot that
    // no longer belongs to it. `mint` inherits this check by calling
    // `copy`; `captest::check_slot_not_empty` exercises both.
    if !dest.is_empty() {
        return Err(CapError::SlotNotEmpty);
    }

    // The monotonic-narrowing rule. Enforced here and only here; the
    // invariant then holds along any chain by induction (ADR 021).
    if !rights.is_subset_of(src.rights()) {
        return Err(CapError::RightsEscalation {
            asked: rights,
            held: src.rights(),
        });
    }

    let depth = src.depth() + 1;
    if depth > MAX_DERIVATION_DEPTH {
        return Err(CapError::DerivationTooDeep { depth });
    }

    dest.fill(
        src.object(),
        src.cap_type(),
        rights,
        depth,
        src.extra(),
        src.is_device(),
    );

    // SAFETY: Both slots are live; `dest` was empty so it was not in any
    // list.
    unsafe {
        link_after(src, dest);
    }

    Ok(())
}

/// Copy a capability and attach a badge.
///
/// Attaching a badge makes the capability behave as if it named a
/// distinct object -- a server can tell its clients apart by badge
/// without trusting them -- so it counts as producing authority and needs
/// `GRANT` (ADR 021).
///
/// # Safety
///
/// As [`copy`].
pub unsafe fn mint(
    src: &mut Slot,
    dest: &mut Slot,
    rights: Rights,
    badge: u32,
) -> Result<(), CapError> {
    if src.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if !src.rights().contains(Rights::GRANT) {
        return Err(CapError::InsufficientRights {
            needed: Rights::GRANT,
            held: src.rights(),
        });
    }
    // A badge is written once. Re-badging would let a holder forge a
    // different client's identity.
    if src.extra() != 0 && badge != src.extra() {
        return Err(CapError::InvalidArgument);
    }

    // SAFETY: Caller's guarantee.
    unsafe {
        copy(src, dest, rights)?;
    }

    let packed = dest.packed & !(0xFFFF_FFFFu64 << EXTRA_SHIFT);
    dest.packed = packed | ((badge as u64) << EXTRA_SHIFT);

    Ok(())
}

/// Delete one capability.
///
/// The object itself is *not* destroyed here. Working out whether this
/// was the last capability naming it needs a scan of the derivation list,
/// which M5 does not do: object destruction arrives with the types that
/// need tearing down (TCB queues, Endpoint waiters) in M7 and M9. Until
/// then a deleted capability simply stops existing, and the memory
/// returns to its Untyped only when that Untyped is revoked -- which is
/// the path M5 does implement and test.
///
/// # Safety
///
/// `slot` must be live.
pub unsafe fn delete(slot: &mut Slot) -> Result<(), CapError> {
    if slot.is_empty() {
        return Err(CapError::EmptySlot);
    }

    // SAFETY: Caller's guarantee; the slot is in a list because every
    // non-empty slot is.
    unsafe {
        unlink(slot);
    }
    slot.clear();

    Ok(())
}

/// How much work a revoke did.
#[derive(Clone, Copy, Debug)]
pub struct RevokeReport {
    /// Capabilities deleted.
    pub deleted: usize,
    /// Slots examined, including the ones that ended the scan.
    pub visited: usize,
}

/// Delete every capability derived from `slot`, leaving `slot` itself.
///
/// This is the forward scan ADR 019 is built around: because the
/// derivation list is in preorder, a capability's descendants are exactly
/// the entries that follow it until one appears whose depth is not
/// greater.
///
/// Bounded by the number of slots in the system, not by the shape of the
/// tree -- but that bound is large, and ADR 019 gives the figure. The
/// `cap-revoke-depth` scenario measures it.
///
/// # Safety
///
/// `slot` must be live, and nothing may modify the derivation list
/// concurrently. The second condition holds because a syscall does not
/// yield (ADR 019).
pub unsafe fn revoke(slot: &mut Slot) -> Result<RevokeReport, CapError> {
    if slot.is_empty() {
        return Err(CapError::EmptySlot);
    }

    let root_depth = slot.depth();
    let mut cursor = slot.mdb_next;
    let mut deleted = 0usize;
    let mut visited = 0usize;

    while cursor.0 != 0 {
        // SAFETY: A non-zero link names a live slot.
        let current = unsafe { slot_at(cursor) };
        visited += 1;

        if current.is_empty() || current.depth() <= root_depth {
            // Not a descendant: the run of descendants has ended.
            break;
        }

        let next = current.mdb_next;

        // SAFETY: `current` is live and in the list.
        unsafe {
            unlink(current);
        }
        current.clear();
        deleted += 1;

        cursor = next;
    }

    Ok(RevokeReport { deleted, visited })
}

/// Mark an Untyped capability as naming device memory (ADR 017).
///
/// Device memory is a hole in the physical address space where a
/// peripheral's registers appear. A kernel object placed there would be
/// corrupted by the device and would issue arbitrary commands to it, so
/// a device Untyped may only ever become a Frame.
///
/// There is deliberately no invocation that reaches this. A component
/// that could *set* the flag could only make its own memory less useful,
/// which is harmless; but the same operation inverted would let it
/// *clear* the flag on a real device region and place a TCB on a disk
/// controller. Keeping the flag kernel-only removes the question.
///
/// Boot will call this once the memory map's device regions are
/// enumerated (M8); until then the acceptance test is the only caller,
/// and it flags ordinary RAM, because what is under test is the
/// restriction the flag imposes rather than the address it names.
///
/// # Safety
///
/// `slot` must be a live Untyped capability that the caller owns, and
/// nothing may have been retyped out of it yet -- an object already
/// carved from it would not inherit the flag.
pub unsafe fn mark_device(slot: &mut Slot) -> Result<(), CapError> {
    if slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if slot.cap_type() != CapType::Untyped {
        return Err(CapError::WrongType {
            found: slot.cap_type(),
        });
    }
    slot.packed |= 1 << DEVICE_BIT;
    Ok(())
}

/// Seed a slot with a capability that has no parent.
///
/// Only boot code may do this: every other capability is derived from one
/// of these roots. Depth zero, and the slot starts its own list.
///
/// # Safety
///
/// `slot` must be live and empty, and this must be called only while
/// building the initial CSpace (ADR 022).
pub unsafe fn seed_root(
    slot: &mut Slot,
    object: ObjectRef,
    cap_type: CapType,
    rights: Rights,
    extra: u32,
    device: bool,
) -> Result<(), CapError> {
    if !slot.is_empty() {
        return Err(CapError::SlotNotEmpty);
    }
    slot.fill(object, cap_type, rights, 0, extra, device);
    slot.mdb_prev = PhysAddr(0);
    slot.mdb_next = PhysAddr(0);
    Ok(())
}

/// Attach a freshly retyped capability below its Untyped.
///
/// Used by `retype`, which has already validated everything.
///
/// # Safety
///
/// Both slots live; `child` empty.
pub(super) unsafe fn attach_child(
    parent: &mut Slot,
    child: &mut Slot,
    object: ObjectRef,
    cap_type: CapType,
    rights: Rights,
    extra: u32,
    device: bool,
) {
    let depth = parent.depth() + 1;
    debug_assert!(depth <= MAX_DERIVATION_DEPTH);

    child.fill(object, cap_type, rights, depth, extra, device);
    // SAFETY: Caller's guarantee.
    unsafe {
        link_after(parent, child);
    }
}

/// Walk the derivation list from `slot` and count its descendants.
///
/// Diagnostic only -- O(descendants), which is why it is not part of any
/// operation. The acceptance test uses it to show what a revoke is about
/// to do.
///
/// # Safety
///
/// `slot` must be live.
pub unsafe fn count_descendants(slot: &Slot) -> usize {
    let root_depth = slot.depth();
    let mut cursor = slot.mdb_next;
    let mut count = 0usize;

    while cursor.0 != 0 {
        // SAFETY: Non-zero links name live slots.
        let current = unsafe { slot_at(cursor) };
        if current.is_empty() || current.depth() <= root_depth {
            break;
        }
        count += 1;
        cursor = current.mdb_next;
    }

    count
}
