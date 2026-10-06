// SPDX-License-Identifier: BSD-2-Clause
//! Untyped memory and `retype`.
//!
//! `retype` is the only way memory becomes a kernel object, and the only
//! way memory reaches user space. Both facts shape the implementation:
//!
//! * It is **atomic**. Phase one validates everything and touches
//!   nothing; phase two writes and cannot fail. A caller never sees a
//!   half-finished result, so there is no partial state to reason about.
//! * It **always zeroes**. Not sometimes, not when the memory looks
//!   suspicious. Because this is the single door, one `memset` here makes
//!   information leakage structurally impossible, where zeroing on
//!   destruction would need remembering at every one of several exits.
//!
//! See ADR 020.

use super::cnode::{self, CNode, Slot};
use super::{
    CapError, CapType, ObjectRef, Rights, FRAME_SIZE, IRQ_HANDLER_SIZE, MAX_DERIVATION_DEPTH,
    NOTIFICATION_SIZE, PAGE_TABLE_SIZE, RETYPE_MAX_COUNT, SLOT_SIZE, TCB_SIZE, UNTYPED_MAX_BITS,
    UNTYPED_MIN_BITS,
};
use crate::memory::PhysAddr;

/// The mutable state of an Untyped: how far it has been handed out.
///
/// Lives in the object itself rather than in the capability, because
/// several capabilities may name the same Untyped and they must all see
/// the same watermark.
#[repr(C)]
pub struct UntypedHeader {
    /// First byte not yet handed out, as an offset from the base.
    watermark: u64,
}

/// Bytes of the Untyped reserved for its own header.
///
/// The header sits at the start of the region and the watermark begins
/// after it, so an Untyped can never hand out the memory describing
/// itself.
const HEADER_SIZE: u64 = 16;

const _: () = {
    assert!(
        core::mem::size_of::<UntypedHeader>() as u64 <= HEADER_SIZE,
        "the Untyped header does not fit in its reserved space"
    );
    assert!(
        HEADER_SIZE.is_power_of_two(),
        "HEADER_SIZE must be a power of two so the first object stays aligned"
    );
};

/// A live Untyped, reached through its capability.
pub struct Untyped {
    base: PhysAddr,
    size: u64,
    device: bool,
}

impl Untyped {
    /// # Safety
    ///
    /// `base` must name a region of `1 << size_bits` bytes that this
    /// Untyped owns.
    pub unsafe fn from_slot(slot: &Slot) -> Result<Untyped, CapError> {
        if slot.is_empty() {
            return Err(CapError::EmptySlot);
        }
        if slot.cap_type() != CapType::Untyped {
            return Err(CapError::WrongType {
                found: slot.cap_type(),
            });
        }
        let size_bits = slot.extra() as u8;
        if !(UNTYPED_MIN_BITS..=UNTYPED_MAX_BITS).contains(&size_bits) {
            return Err(CapError::InvalidArgument);
        }
        Ok(Untyped {
            base: slot.object().0,
            size: 1u64 << size_bits,
            device: slot.is_device(),
        })
    }

    fn header(&self) -> &'static mut UntypedHeader {
        // SAFETY: The first HEADER_SIZE bytes of the region are reserved
        // for this header and written by `initialise`. PMAP maps all RAM
        // writably.
        unsafe { &mut *crate::memory::phys_to_virt(self.base).as_ptr::<UntypedHeader>() }
    }

    /// Bytes handed out so far, including alignment padding.
    pub fn watermark(&self) -> u64 {
        self.header().watermark
    }

    /// Bytes still available, ignoring alignment.
    pub fn remaining(&self) -> u64 {
        self.size - self.watermark()
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn base(&self) -> PhysAddr {
        self.base
    }

    pub fn is_device(&self) -> bool {
        self.device
    }

    /// Write the header of a fresh Untyped.
    ///
    /// # Safety
    ///
    /// The region must be owned and not yet in use.
    pub unsafe fn initialise(base: PhysAddr) {
        // SAFETY: The caller owns the region; PMAP maps it writably.
        unsafe {
            let header = crate::memory::phys_to_virt(base).as_ptr::<UntypedHeader>();
            header.write(UntypedHeader {
                watermark: HEADER_SIZE,
            });
        }
    }

    /// Return every byte to the pool. Only `revoke` may do this.
    ///
    /// # Safety
    ///
    /// Every capability derived from this Untyped must already be gone,
    /// or the objects they name will be handed out again underneath
    /// them.
    unsafe fn reset(&self) {
        self.header().watermark = HEADER_SIZE;
    }
}

/// Size and alignment of one object of a given type.
///
/// `size_bits` is only consulted for the variable-sized types; for the
/// rest it must be zero, which `validate` checks.
fn object_layout(cap_type: CapType, size_bits: u8) -> Result<(u64, u64), CapError> {
    let size: u64 = match cap_type {
        CapType::Tcb => TCB_SIZE as u64,
        CapType::Endpoint => super::ENDPOINT_SIZE as u64,
        CapType::Notification => NOTIFICATION_SIZE as u64,
        CapType::Frame => FRAME_SIZE as u64,
        CapType::PageTable => PAGE_TABLE_SIZE as u64,
        CapType::IrqHandler => IRQ_HANDLER_SIZE as u64,

        CapType::CNode => {
            // A CNode is 2^radix slots. The radix arrives as size_bits.
            if size_bits == 0 || size_bits > 16 {
                return Err(CapError::InvalidArgument);
            }
            (SLOT_SIZE as u64) << size_bits
        }
        CapType::Untyped => {
            if !(UNTYPED_MIN_BITS..=UNTYPED_MAX_BITS).contains(&size_bits) {
                return Err(CapError::InvalidArgument);
            }
            1u64 << size_bits
        }

        CapType::Null => return Err(CapError::InvalidArgument),
    };

    // ADR 017's alignment rule: the next power of two at or above the
    // size. For the variable types the size is already a power of two.
    let align = size.next_power_of_two();
    Ok((size, align))
}

/// What a validated retype is about to do.
///
/// Produced by phase one, consumed by phase two. Its existence is the
/// atomicity argument: everything that can fail has already happened by
/// the time one of these exists.
struct RetypePlan {
    first: u64,
    object_size: u64,
    rights: Rights,
    extra: u32,
}

/// Create `count` objects of `cap_type` out of `untyped`.
///
/// Atomic: either every object is created and every capability written,
/// or nothing happens at all and an error is returned. See ADR 020.
///
/// # Safety
///
/// `untyped_slot` must be a live Untyped capability, `dest` a live CNode,
/// and no other mutable borrow of either may exist.
pub unsafe fn retype(
    untyped_slot: &mut Slot,
    cap_type: CapType,
    size_bits: u8,
    count: usize,
    dest: &CNode,
    dest_offset: usize,
) -> Result<(), CapError> {
    // ================= PHASE 1: validate, touch nothing ==============

    if count == 0 {
        return Err(CapError::InvalidArgument);
    }
    if count > RETYPE_MAX_COUNT {
        return Err(CapError::TooManyObjects { count });
    }

    // SAFETY: The caller guarantees the slot is live.
    let untyped = unsafe { Untyped::from_slot(untyped_slot) }?;

    // ADR 017: device memory is not RAM, and a kernel object placed on a
    // device register window would both corrupt the object and issue
    // arbitrary commands to the hardware. A Frame is the only thing a
    // device Untyped may become.
    if untyped.is_device() && cap_type != CapType::Frame {
        return Err(CapError::DeviceUntypedFrameOnly { asked: cap_type });
    }

    // Fixed-size types must not be given a size.
    let fixed = matches!(
        cap_type,
        CapType::Tcb
            | CapType::Endpoint
            | CapType::Notification
            | CapType::Frame
            | CapType::PageTable
            | CapType::IrqHandler
    );
    if fixed && size_bits != 0 {
        return Err(CapError::InvalidArgument);
    }

    let (object_size, align) = object_layout(cap_type, size_bits)?;

    // Where the first object would start, and how much the whole run
    // costs including the alignment gap.
    let watermark = untyped.watermark();
    let first = watermark.next_multiple_of(align);
    let total = (first - watermark) + object_size * count as u64;

    if first + object_size * count as u64 > untyped.size {
        return Err(CapError::NotEnoughMemory {
            remaining: untyped.remaining(),
            needed: total,
        });
    }

    // Derivation depth, before anything is written.
    let depth = untyped_slot.depth() + 1;
    if depth > MAX_DERIVATION_DEPTH {
        return Err(CapError::DerivationTooDeep { depth });
    }

    // Destination slots: in range, and every one of them empty.
    if dest_offset.saturating_add(count) > dest.len() {
        return Err(CapError::CPtrOutOfRange);
    }
    // Every one of them, not just the first. An implementation that
    // checked each slot immediately before writing it would corrupt a
    // range whose occupied slot comes later, so the whole range is
    // validated here, in phase one, before anything is written.
    // `check_partial_overlap` in captest puts the obstacle in the
    // middle for exactly this reason.
    for index in 0..count {
        // SAFETY: Range checked immediately above.
        let slot = unsafe { dest.slot(dest_offset + index) };
        if !slot.is_empty() {
            return Err(CapError::SlotNotEmpty);
        }
    }

    // New objects start with full rights; narrowing happens at `copy`.
    // A CNode carries its radix and an Untyped its size_bits in `extra`.
    let extra = match cap_type {
        CapType::CNode | CapType::Untyped => size_bits as u32,
        _ => 0,
    };

    let plan = RetypePlan {
        first,
        object_size,
        rights: Rights::ALL,
        extra,
    };

    // ================= PHASE 2: write, cannot fail ===================
    //
    // Everything below has been checked. Nothing here returns an error,
    // and nothing can yield: a syscall does not switch threads (ADR 019),
    // so no other code can observe the half-written state.

    // SAFETY: The region is inside this Untyped, which owns it, and the
    // range was checked above. Zeroing before the objects are initialised
    // is what makes information leakage impossible -- ADR 020 -- and it
    // is unconditional on purpose.
    unsafe {
        let start = crate::memory::phys_to_virt(PhysAddr(untyped.base.0 + plan.first));
        core::ptr::write_bytes(
            start.as_ptr::<u8>(),
            0,
            (plan.object_size * count as u64) as usize,
        );
    }

    for index in 0..count {
        let object = ObjectRef(PhysAddr(
            untyped.base.0 + plan.first + plan.object_size * index as u64,
        ));

        // An Untyped needs its header before anyone can retype from it.
        if cap_type == CapType::Untyped {
            // SAFETY: The region was just zeroed and belongs to us.
            unsafe {
                Untyped::initialise(object.0);
            }
        }

        // SAFETY: Range and emptiness checked in phase 1.
        let slot = unsafe { dest.slot(dest_offset + index) };

        // SAFETY: Both slots are live, the destination is empty, and the
        // depth was validated.
        unsafe {
            cnode::attach_child(
                untyped_slot,
                slot,
                object,
                cap_type,
                plan.rights,
                plan.extra,
                // A Frame carved from device memory stays device memory:
                // it must never be recycled into a kernel object.
                untyped.is_device(),
            );
        }
    }

    // The watermark moves last, so that a reader seeing it advanced also
    // sees finished objects. Single-threaded today; the ordering costs
    // nothing and will still be right when it stops being.
    untyped.header().watermark = plan.first + plan.object_size * count as u64;

    Ok(())
}

/// Revoke an Untyped: delete every capability derived from it and return
/// its memory.
///
/// The watermark reset is the only way memory ever comes back. ADR 020
/// chose this over per-object freeing because the alternative turns an
/// Untyped into an allocator, and an allocator needs a search.
///
/// # Safety
///
/// `slot` must be a live Untyped capability.
pub unsafe fn revoke_untyped(slot: &mut Slot) -> Result<cnode::RevokeReport, CapError> {
    // SAFETY: Caller's guarantee.
    let untyped = unsafe { Untyped::from_slot(slot) }?;

    // SAFETY: As above; revoke walks the derivation list, which every
    // live slot is part of.
    let report = unsafe { cnode::revoke(slot) }?;

    // Every derived capability is gone, so nothing names the objects in
    // this region any more.
    //
    // SAFETY: Established by the revoke above.
    unsafe {
        untyped.reset();
    }

    Ok(report)
}
