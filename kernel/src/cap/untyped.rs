// SPDX-License-Identifier: MPL-2.0
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
//!
//! # Where the watermark lives
//!
//! For ordinary memory, in the Untyped's own first sixteen bytes. For
//! device memory, in a kernel-side table at the bottom of this file,
//! keyed by base address. The two places hold the same quantity and
//! the split is forced by physics rather than chosen -- see
//! [`Untyped::watermark`] and ADR 050.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::cnode::{self, CNode, Slot};
use super::{
    CapError, CapType, ObjectRef, Rights, FRAME_SIZE, IOPORT_SIZE, IRQ_HANDLER_SIZE,
    MAX_DERIVATION_DEPTH, NOTIFICATION_SIZE, PAGE_TABLE_SIZE, RETYPE_MAX_COUNT, SLOT_SIZE,
    TCB_SIZE, UNTYPED_MAX_BITS, UNTYPED_MIN_BITS,
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
        let size_bits = slot.as_size_bits();
        if !(UNTYPED_MIN_BITS..=UNTYPED_MAX_BITS).contains(&size_bits) {
            return Err(CapError::InvalidArgument);
        }
        Ok(Untyped {
            base: slot.object().0,
            size: 1u64 << size_bits,
            device: slot.is_device(),
        })
    }

    /// The header, which lives in the Untyped's **own memory**.
    ///
    /// # Panics
    ///
    /// On a device Untyped, where there is no such header. See
    /// [`watermark`](Self::watermark) for why, and `device_watermark`
    /// for where the figure lives instead.
    fn header(&self) -> &'static mut UntypedHeader {
        debug_assert!(
            !self.device,
            "header() on a device Untyped at {:#x}. Device memory holds \
             no header: it is outside the physical window, so this would \
             fault, and even mapped it would mean writing a watermark \
             onto the top-left corner of a screen or over a control \
             register. The figure lives in `device_watermark` instead \
             (ADR 050).",
            self.base.0
        );
        // SAFETY: The first HEADER_SIZE bytes of the region are reserved
        // for this header and written by `initialise`. PMAP maps all RAM
        // writably.
        unsafe { &mut *crate::memory::phys_to_virt(self.base).as_ptr::<UntypedHeader>() }
    }

    /// Bytes handed out so far, including alignment padding.
    ///
    /// # Two places, one meaning
    ///
    /// For ordinary memory the figure is in the Untyped's own first
    /// sixteen bytes. That is not an implementation detail but the
    /// answer to a question: **several capabilities may name one
    /// Untyped** (`Copy` and `Mint` both make one), and they must
    /// all see the same watermark. A counter in the capability
    /// would give each copy its own, and two copies would hand out
    /// the same bytes twice.
    ///
    /// Device memory cannot hold it. It is outside the physical
    /// window by construction -- `vm::init` excludes framebuffer
    /// regions -- so the write would fault; and if it were mapped,
    /// the bytes being written on are the top-left of a screen, or
    /// a device's control registers.
    ///
    /// So device Untypeds keep their watermark in a kernel-side
    /// table, **keyed by base address** so that the "all copies
    /// agree" property is preserved exactly as it is for RAM
    /// (ADR 050).
    pub fn watermark(&self) -> u64 {
        if self.device {
            device_watermark(self.base)
        } else {
            self.header().watermark
        }
    }

    /// Move the watermark to `value`.
    ///
    /// The single writer, so that "where does this number live" is
    /// answered once rather than at each of the three call sites
    /// that used to reach into `header()` directly.
    fn set_watermark(&self, value: u64) {
        if self.device {
            set_device_watermark(self.base, value);
        } else {
            self.header().watermark = value;
        }
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
        // Device memory has no header to skip past, so its watermark
        // starts at zero rather than at HEADER_SIZE -- the whole
        // region is available.
        self.set_watermark(if self.device { 0 } else { HEADER_SIZE });
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
        CapType::IoPort => IOPORT_SIZE as u64,

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

        // None of these names memory, and they fail to for different
        // reasons: `Null` is the absence of a capability, while
        // `IrqControl` and `IoPortControl` are authority over
        // interrupt lines and port ranges — properties of the machine
        // rather than of RAM (ADR 041, ADR 045). There is exactly one
        // of each, created at boot; no amount of Untyped produces a
        // second.
        CapType::Null | CapType::IrqControl | CapType::IoPortControl => {
            return Err(CapError::InvalidArgument)
        }
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
    // Interrupts off for the whole call, not just phase 2 (ADR 025).
    //
    // The phase split is an *atomicity* argument: nothing is written
    // until everything has been validated, so a failure leaves no
    // partial state. It says nothing about concurrency. From M6 a
    // timer interrupt can switch threads, and a thread that ran
    // between phase 1 and phase 2 could move the watermark or fill a
    // destination slot -- making phase 1's conclusions false exactly
    // when phase 2 is relying on them.
    //
    // This was a real correction to ADR 025's first draft, which said
    // "phase 2 only" and was wrong for this reason.
    let _interrupts = crate::sched::InterruptGuard::new();

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
            | CapType::IoPort
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

    // **Zeroing is scoped to RAM** (ADR 020, as corrected in M14).
    //
    // ADR 020 said retype zeroes "not sometimes, not when the memory
    // looks suspicious", and that is still the rule for ordinary
    // memory: one `memset` at the single door makes information
    // leakage structurally impossible.
    //
    // Device memory is exempt, for two separate reasons and either
    // alone would be enough:
    //
    //  1. **It is impossible.** The physical window covers RAM only
    //     (ADR 027), and `vm::init` excludes framebuffer regions by
    //     construction. The write faults -- `#PF` at
    //     `pmap_base + 0xfd000000`, which is how this was found.
    //  2. **It is wrong.** Zeroing a framebuffer blanks the screen;
    //     zeroing a device's register window issues commands to the
    //     hardware. A retype would have *hardware effects* as a side
    //     effect of bookkeeping, and on some other device that means
    //     a reset.
    //
    // The leakage argument does not transfer. It is about one
    // object's bytes reaching the **next owner of that memory**. A
    // framebuffer's contents are not a previous owner's residue;
    // they are the device's present state, and the device is a shared
    // thing by its nature. Nothing is being leaked that the next
    // holder could not see by looking at the screen.
    //
    // This is the third rule of ADR 020's that assumed "an Untyped's
    // memory is writable RAM", after the header and the watermark.
    // See ADR 023's K11.
    if !untyped.is_device() {
        // SAFETY: The region is inside this Untyped, which owns it,
        // the range was checked above, and the Untyped is RAM, so
        // PMAP maps it writably.
        unsafe {
            let start = crate::memory::phys_to_virt(PhysAddr(untyped.base.0 + plan.first));
            core::ptr::write_bytes(
                start.as_ptr::<u8>(),
                0,
                (plan.object_size * count as u64) as usize,
            );
        }
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
    untyped.set_watermark(plan.first + plan.object_size * count as u64);

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

// ---- The device watermark table (ADR 050) ------------------------------
//
// **This table is the mechanism ADR 050 described before it existed.**
//
// The ADR said, of a device Untyped, that "its bookkeeping lives in the
// capability, not in the memory it names -- which is what `size_bits`
// in the slot is for". The first half of that sentence was a
// requirement stated as though it were a fact, and the second half was
// wrong: `size_bits` is the region's *size*, which never changes. The
// watermark changes on every retype and had nowhere to go.
//
// Nothing noticed until the first real device Untyped was used, at
// which point `header()` dereferenced an address outside the physical
// window and the kernel took a page fault at 0xffff8000fd000000. See
// ADR 023's K10, which this produced.
//
// # Why not a field in the slot
//
// Because **an Untyped capability can be copied.** Two capabilities
// naming one Untyped must see one watermark, or each hands out the
// same bytes to a different holder. That is the same argument that
// puts the figure in the object's own memory for RAM: the watermark
// belongs to the *object*, and a capability is not the object.
//
// So the key is the **base address**, which is what identifies the
// object, and copies agree by construction.
//
// # Shape
//
// `ioport::RANGES`' shape, for `ioport::RANGES`' reason: a fixed table
// read from inside an invocation, which may not allocate (ADR 022).
// Atomics rather than a lock because every caller already holds an
// InterruptGuard and a lock between a syscall and nothing else would
// protect nothing.

/// How many device Untypeds the kernel can track at once.
///
/// Sixteen, and the figure is arithmetic rather than taste. A device
/// region is blocked into `2^k` pieces at `2^k` boundaries like any
/// other Untyped (ADR 020), and a framebuffer's size is almost never
/// a power of two. QEMU's default 1280x800 at 32 bpp is 4 096 000
/// bytes, which decomposes into **six** blocks: 2 MiB, 1 MiB,
/// 512 KiB, 256 KiB, 128 KiB, 32 KiB. One screen therefore costs six
/// entries, so eight would leave room for almost nothing; sixteen
/// leaves room for a second device and matches
/// [`crate::cap::ioport::MAX_RANGES`], which is fixed for the same
/// reason.
pub const MAX_DEVICE_UNTYPED: usize = 16;

/// One tracked device Untyped.
///
/// `size` is the liveness marker, and that is a decision rather than
/// a convenience: a live Untyped is at least `1 << UNTYPED_MIN_BITS`
/// bytes, so zero is a value no real entry can hold. Using `base`
/// would have needed a separate flag -- physical address zero is a
/// perfectly good address, and the same trap `ioport::LIVE` exists to
/// avoid.
struct DeviceUntyped {
    /// Physical base address of the region. The key.
    base: AtomicU64,
    /// Bytes in the region. Zero means the entry is free.
    size: AtomicU64,
    /// First byte not yet handed out, as an offset from `base`.
    ///
    /// Starts at **zero**, not at `HEADER_SIZE`: there is no header
    /// here to step over, so the whole region is available.
    watermark: AtomicU64,
    /// How many capabilities name this Untyped.
    ///
    /// Not a nicety. The entry must outlive every capability that
    /// could still retype from it, and no longer -- an entry that
    /// stayed behind would make the base unreusable and would hand a
    /// later Untyped at the same address a stale watermark.
    refs: AtomicU32,
}

impl DeviceUntyped {
    /// A free entry.
    ///
    /// `#[allow]` because clippy is right in general and wrong here.
    /// A `const` holding atomics is usually a trap: every *use* of
    /// the name produces a fresh value, so `COUNTER.fetch_add(..)`
    /// would increment a temporary and discard it.
    ///
    /// This one is used in exactly one place, inside the
    /// `[const { .. }; N]` initialiser below, where producing a
    /// fresh value per element is precisely what is wanted — the
    /// alternative, `[DeviceUntyped { .. }; N]`, needs `Copy`, and
    /// an atomic must not be `Copy`.
    #[allow(clippy::declare_interior_mutable_const)]
    const EMPTY: DeviceUntyped = DeviceUntyped {
        base: AtomicU64::new(0),
        size: AtomicU64::new(0),
        watermark: AtomicU64::new(0),
        refs: AtomicU32::new(0),
    };
}

static DEVICE_UNTYPEDS: [DeviceUntyped; MAX_DEVICE_UNTYPED] =
    [const { DeviceUntyped::EMPTY }; MAX_DEVICE_UNTYPED];

/// The entry for `base`, if there is one.
fn device_entry(base: PhysAddr) -> Option<&'static DeviceUntyped> {
    DEVICE_UNTYPEDS.iter().find(|entry| {
        entry.size.load(Ordering::Acquire) != 0 && entry.base.load(Ordering::Acquire) == base.0
    })
}

/// Whether `base` is a tracked device Untyped.
///
/// Used by `cnode::seed_root` as a tripwire: a device Untyped
/// capability whose entry does not exist is the exact mistake ADR 050
/// made in prose, and boot is where it would be made again.
pub fn has_device_entry(base: PhysAddr) -> bool {
    device_entry(base).is_some()
}

/// The entry for `base`, or stop.
///
/// A missing entry is not a caller's error but a kernel one: the
/// device flag and the entry are set together and released together,
/// so a capability marked device with no entry means one of the two
/// paths forgot. Carrying on would read a watermark of zero and hand
/// out memory that is already in use.
fn entry_or_panic(base: PhysAddr) -> &'static DeviceUntyped {
    device_entry(base).unwrap_or_else(|| {
        panic!(
            "no device-Untyped entry for base {}. The capability says \
             this is device memory, so its watermark lives in the \
             kernel table (ADR 050) -- but no entry was reserved for \
             it. Every path that sets the device flag must call \
             `reserve_device` first: boot's framebuffer seeding and \
             `cnode::mark_device` are the only two.",
            base
        )
    })
}

/// Bytes handed out so far from the device Untyped at `base`.
fn device_watermark(base: PhysAddr) -> u64 {
    entry_or_panic(base).watermark.load(Ordering::Acquire)
}

/// Move the device Untyped's watermark.
fn set_device_watermark(base: PhysAddr, value: u64) {
    let entry = entry_or_panic(base);
    let size = entry.size.load(Ordering::Acquire);
    debug_assert!(
        value <= size,
        "device Untyped at {} asked to move its watermark to {}, past \
         the end of its {} bytes",
        base,
        value,
        size
    );
    entry.watermark.store(value, Ordering::Release);
}

/// Start tracking a device Untyped. Returns false if the table is full.
///
/// Called at the moment the device flag goes on, which is the only
/// moment at which the two can be kept in step. Two callers and no
/// more: boot's framebuffer seeding (ADR 050) and
/// [`cnode::mark_device`].
///
/// # Panics
///
/// If `base` is already tracked. Two live Untypeds cannot share a base
/// -- they would be the same object -- so this means a region was
/// seeded twice, and the second seeding would give the first one's
/// holders a watermark that moves underneath them.
#[must_use]
pub fn reserve_device(base: PhysAddr, size: u64) -> bool {
    debug_assert!(size > 0, "a zero-sized device Untyped at {}", base);

    assert!(
        device_entry(base).is_none(),
        "two device Untypeds claim base {}; a region has been seeded \
         twice and both holders would share one watermark",
        base
    );

    for entry in &DEVICE_UNTYPEDS {
        if entry.size.load(Ordering::Acquire) == 0 {
            entry.base.store(base.0, Ordering::Relaxed);
            entry.watermark.store(0, Ordering::Relaxed);
            entry.refs.store(1, Ordering::Relaxed);
            // `size` last. It is the liveness marker, so the entry
            // becomes findable only once the rest of it is written.
            entry.size.store(size, Ordering::Release);
            return true;
        }
    }
    false
}

/// A second capability now names this device Untyped.
///
/// Called from [`cnode::copy`], which is the only operation that
/// produces one: `mint` refuses an Untyped by type, and `retype`
/// cannot produce an Untyped from device memory at all
/// (`DeviceUntypedFrameOnly`).
pub fn retain_device(base: PhysAddr) {
    entry_or_panic(base).refs.fetch_add(1, Ordering::AcqRel);
}

/// A capability naming this device Untyped has gone.
///
/// Returns `Some(watermark)` if that was the last one and the entry
/// has been released, `None` if others remain.
///
/// The counterpart of [`crate::cap::irq::release_line`] and
/// [`crate::cap::ioport::release_range`], reached from the same hook
/// in `release_ipc_references` -- and with one difference worth
/// naming. Those two release on the *first* delete, because an
/// IrqHandler or an IoPort is a single object that one capability
/// names. An Untyped can be copied, so releasing on the first delete
/// would retire an entry that surviving capabilities still retype
/// from, and the next `reserve_device` for the same base would reset
/// their watermark to zero.
pub fn release_device(base: PhysAddr) -> Option<u64> {
    let entry = entry_or_panic(base);

    let before = entry.refs.fetch_sub(1, Ordering::AcqRel);
    assert!(
        before > 0,
        "the device Untyped at {} was released once more than it was \
         retained; the reference count has gone negative",
        base
    );
    if before > 1 {
        return None;
    }

    let watermark = entry.watermark.load(Ordering::Acquire);
    // `size` first, for the mirror of the reason it was written last:
    // clearing the liveness marker retires the entry before any of its
    // fields are reused.
    entry.size.store(0, Ordering::Release);
    entry.base.store(0, Ordering::Relaxed);
    entry.watermark.store(0, Ordering::Relaxed);
    Some(watermark)
}

/// Report the device Untypeds, for the acceptance test.
pub fn device_report() {
    use crate::println;

    let mut live = 0usize;
    let mut bytes = 0u64;
    let mut handed_out = 0u64;

    for entry in &DEVICE_UNTYPEDS {
        let size = entry.size.load(Ordering::Acquire);
        if size == 0 {
            continue;
        }
        let base = entry.base.load(Ordering::Acquire);
        let watermark = entry.watermark.load(Ordering::Acquire);
        println!(
            "[cap] device untyped {:#x}..{:#x} ({} KiB), {} B handed \
             out, {} capability/ies",
            base,
            base + size,
            size / 1024,
            watermark,
            entry.refs.load(Ordering::Acquire)
        );
        live += 1;
        bytes += size;
        handed_out += watermark;
    }

    println!(
        "[cap] {} device untyped(s) of {} table slots, {} KiB total, \
         {} KiB handed out; watermarks live in this table and not in \
         the memory they describe (ADR 050)",
        live,
        MAX_DEVICE_UNTYPED,
        bytes / 1024,
        handed_out / 1024
    );
}
