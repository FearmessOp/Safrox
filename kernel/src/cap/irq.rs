// SPDX-License-Identifier: MPL-2.0
//! Interrupt handling as a capability (ADR 041, ADR 042).
//!
//! This is the milestone where a hardware interrupt reaches a thread
//! outside the kernel. Everything before it was the kernel talking to
//! itself; M13's driver needs the other direction.
//!
//! Two decisions shape the whole file:
//!
//! * **A line has exactly one owner** (ADR 041). Two components
//!   sharing an IRQ raises three questions with no good answers —
//!   which one gets the interrupt, which one acknowledges it, and why
//!   one component should be able to watch another's interrupt rate
//!   as a side channel. Sharing is solved in user space by a
//!   multiplexer that owns the line, which is the microkernel
//!   argument in miniature.
//! * **The interrupt path does four things and stops** (ADR 042):
//!   find the owner, mask the line, signal the Notification,
//!   acknowledge the LAPIC. That code runs in interrupt context, at a
//!   rate the *device* chooses, inside the trusted base. Every
//!   instruction there is multiplied by a number the kernel does not
//!   control.
//!
//! # Why `IRQHandler` is an object rather than a flag
//!
//! It has to be revokable. A component that dies must not keep its
//! interrupt line forever, and revocation is something the capability
//! system already does — but only for capabilities that name an
//! *object*. A field inside a Notification cannot be revoked; there
//! is nothing separate to revoke.
//!
//! # Why the handler remembers a *slot* rather than a Notification
//!
//! `SetNotification` could store the Notification's address and the
//! interrupt path could signal it directly. It stores the address of
//! the **capability slot** instead, with the slot's generation
//! alongside — the same idiom as ADR 018's CPtrs, ADR 028's VSpaces
//! and ADR 036's reply targets.
//!
//! Three things follow, and none of them is incidental:
//!
//! * The badge comes from the capability, so the driver chooses what
//!   its own interrupts look like, exactly as ADR 037 intends. A
//!   badge field here would be a second place to say the same thing.
//! * Deleting the Notification capability advances the slot's
//!   generation, so the interrupt path *detects* the revocation
//!   rather than having to be told about it. No bookkeeping is owed
//!   by `delete`, and none can be forgotten.
//! * The signal goes through [`crate::cap::ipc::signal`] unchanged —
//!   the same function a thread calls. A notification raised by a
//!   device and one raised by a thread are not merely alike; they are
//!   the same code, so a driver cannot tell them apart and the kernel
//!   has no second delivery path to keep correct.

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use super::cnode::{self, Slot};
use super::{CapError, CapType, ObjectRef, Rights};
use crate::memory::PhysAddr;

/// How many interrupt lines the kernel tracks.
///
/// Sixteen, which covers the legacy ISA range and the handful of
/// vectors M10 actually uses. The number is a **table size**, not a
/// hardware limit: the table is read on the interrupt path and
/// ADR 022 forbids allocating there, so it has to be fixed at compile
/// time. Growing it is a one-line change; making it dynamic is not.
pub const MAX_IRQ: usize = 16;

/// The IRQ this milestone's scenarios use.
///
/// Arbitrary, and deliberately not 0: an off-by-one that produced a
/// zero IRQ would otherwise look like a valid line.
pub const TEST_IRQ: u8 = 5;

/// A second line, so the scenarios can show that owning one does not
/// imply owning another.
pub const TEST_IRQ_ALT: u8 = 11;

/// An interrupt line's handler. 32 bytes (ADR 017).
#[repr(C, align(32))]
pub struct IrqHandler {
    /// The line this handler owns.
    irq: u8,
    /// Whether a Notification capability has been attached.
    bound: u8,
    /// Generation of `notification_slot` when it was attached
    /// (ADR 018). What makes a deleted capability detectable here.
    slot_generation: u8,
    _pad: [u8; 5],
    /// Physical address of the slot holding the Notification
    /// capability, or zero. **Not** the Notification itself — see the
    /// module docs.
    notification_slot: u64,
    /// Reserved, keeping the object at exactly 32 bytes so that a
    /// field added later is a visible decision rather than growth.
    _reserved: [u64; 2],
}

const _: () = {
    assert!(
        core::mem::size_of::<IrqHandler>() == super::IRQ_HANDLER_SIZE,
        "IrqHandler is not IRQ_HANDLER_SIZE bytes; ADR 017's table and \
         retype's arithmetic disagree"
    );
    assert!(
        core::mem::align_of::<IrqHandler>() == super::IRQ_HANDLER_SIZE,
        "an IrqHandler's alignment must equal its size (ADR 017)"
    );
};

impl IrqHandler {
    pub fn irq(&self) -> u8 {
        self.irq
    }

    pub fn is_bound(&self) -> bool {
        self.bound != 0
    }
}

/// # Safety
///
/// `object` must name a live IrqHandler, unaliased.
pub unsafe fn handler_at(object: ObjectRef) -> &'static mut IrqHandler {
    debug_assert!(!object.is_null(), "null IrqHandler address");
    debug_assert!(
        object.0 .0.is_multiple_of(super::IRQ_HANDLER_SIZE as u64),
        "IrqHandler at {} is not {}-byte aligned",
        object,
        super::IRQ_HANDLER_SIZE
    );
    // SAFETY: Caller's guarantee; PMAP maps all RAM writably.
    unsafe { &mut *crate::memory::phys_to_virt(object.0).as_ptr::<IrqHandler>() }
}

/// Borrow the IrqHandler a capability names.
///
/// # Safety
///
/// `slot` must hold a live IrqHandler capability, unaliased.
pub unsafe fn handler_from_slot(slot: &Slot) -> Result<&'static mut IrqHandler, CapError> {
    if slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if slot.cap_type() != CapType::IrqHandler {
        return Err(CapError::WrongType {
            found: slot.cap_type(),
        });
    }
    // SAFETY: The capability names an IrqHandler-sized, aligned object.
    Ok(unsafe { handler_at(slot.object()) })
}

// ---- The ownership table (ADR 041) -------------------------------------
//
// Read on the interrupt path, so: fixed size, no allocation, no lock.
// Atomics rather than a spinlock because the reader is an interrupt
// handler and the writer is a syscall -- a lock between those two is a
// deadlock waiting for the right interleaving.

/// Physical address of the IrqHandler owning each line, or zero.
///
/// One `AtomicU64` per entry rather than one lock over the array: the
/// interrupt path reads exactly one entry and must not be delayed by
/// a syscall writing a different one.
static OWNERS: [AtomicU64; MAX_IRQ] = [const { AtomicU64::new(0) }; MAX_IRQ];

/// Whether each line is masked. 1 = masked, no delivery.
///
/// Separate from `OWNERS` because the two change at different rates:
/// a line is masked and unmasked on every single interrupt, and its
/// owner changes once.
///
/// # The flag and the hardware (M10's debt, paid in M12)
///
/// M10 said this was a software flag and that M12 would move it to a
/// real I/O APIC redirection-table mask. It has, and the honest
/// account of what changed is narrower than "the mask is now real".
///
/// **The flag stays, and it is still what enforces the contract.**
/// [`handle_interrupt`] reads it with one atomic load, on the
/// interrupt path, where a device-register read through the
/// select/data window would be two uncached accesses that cannot be
/// made atomic against a concurrent write. ADR 042's contract — no
/// second delivery before `Ack` — is the flag's.
///
/// What the hardware adds is stopping the line *at the source*, so
/// a masked line costs no dispatch and no EOI. That matters for a
/// real device and is invisible to `irq-test`, which raises its
/// interrupts with a self-IPI: those go LAPIC-to-CPU and never pass
/// through the I/O APIC at all.
///
/// So the two are kept in step and [`crate::ioapic::verify`] checks
/// that they are, because "the register holds what we wrote" is a
/// falsifiable claim where "the interrupt did not arrive" would not
/// have been — it was never going to.
static MASKED: [AtomicU8; MAX_IRQ] = [const { AtomicU8::new(1) }; MAX_IRQ];

/// Set both the flag and the hardware, in that order.
///
/// **The flag first when masking**, the hardware first when
/// unmasking, and the asymmetry is deliberate: whichever is set
/// first, the window between them must fail *closed*. Masking means
/// the flag shuts the door before the hardware does; unmasking means
/// the hardware opens before the flag does. An interrupt arriving in
/// either window is dropped by the flag, which is the safe answer.
///
/// # Safety
///
/// Interrupts must be disabled: `ioapic::set_masked` is a
/// read-modify-write of a device register.
unsafe fn set_masked(irq: usize, masked: bool) {
    debug_assert!(irq < MAX_IRQ);
    if masked {
        MASKED[irq].store(1, Ordering::Release);
        // SAFETY: Caller's guarantee that interrupts are disabled.
        unsafe {
            crate::ioapic::set_masked(irq as u8, true);
        }
    } else {
        // SAFETY: As above.
        unsafe {
            crate::ioapic::set_masked(irq as u8, false);
        }
        MASKED[irq].store(0, Ordering::Release);
    }
}

/// Interrupts delivered to a Notification.
static DELIVERED: AtomicU64 = AtomicU64::new(0);

/// Interrupts that arrived while the line was masked.
///
/// **The counter scenario 4 is built around.** It should be zero in a
/// well-behaved run: the line is masked precisely so that nothing
/// arrives. A non-zero value means a device raised an interrupt
/// between the mask and the `Ack`, which is information rather than
/// an error — and with the mask removed it is the figure that moves.
static WHILE_MASKED: AtomicU64 = AtomicU64::new(0);

/// Interrupts that arrived for a line with no owner (ADR 042).
///
/// Normally unreachable, because revoking an IrqHandler masks the
/// line before clearing its owner. Counted because the race exists —
/// an interrupt can be raised just before that happens — and ADR 042
/// requires the case to be *defined* rather than merely unlikely.
static ORPHANED: AtomicU64 = AtomicU64::new(0);

/// Interrupts whose owner had a Notification capability that has
/// since been deleted.
///
/// Distinct from `ORPHANED`, and the distinction is the point: there
/// the *line* has no owner, here the owner's destination is gone.
/// They have different causes and different remedies, and a single
/// counter would make `irq-revoked` unable to say which it had
/// produced.
static STALE_NOTIFICATION: AtomicU64 = AtomicU64::new(0);

/// Interrupts for a line whose owner never called `SetNotification`.
static UNBOUND: AtomicU64 = AtomicU64::new(0);

/// Acks performed.
static ACKS: AtomicU64 = AtomicU64::new(0);

pub fn delivered() -> u64 {
    DELIVERED.load(Ordering::Acquire)
}

pub fn while_masked() -> u64 {
    WHILE_MASKED.load(Ordering::Acquire)
}

pub fn orphaned() -> u64 {
    ORPHANED.load(Ordering::Acquire)
}

pub fn stale_notification() -> u64 {
    STALE_NOTIFICATION.load(Ordering::Acquire)
}

pub fn unbound() -> u64 {
    UNBOUND.load(Ordering::Acquire)
}

pub fn acks() -> u64 {
    ACKS.load(Ordering::Acquire)
}

/// Whether a line is currently masked.
///
/// An out-of-range line reports masked: there is no such line, so
/// nothing may be delivered on it, and saying "not masked" would be
/// the more dangerous of the two lies.
pub fn is_masked(irq: u8) -> bool {
    if irq as usize >= MAX_IRQ {
        return true;
    }
    MASKED[irq as usize].load(Ordering::Acquire) != 0
}

/// Whether a line has an owner.
pub fn is_owned(irq: u8) -> bool {
    if irq as usize >= MAX_IRQ {
        return false;
    }
    OWNERS[irq as usize].load(Ordering::Acquire) != 0
}

pub fn report() {
    use crate::println;

    println!(
        "[irq] {} delivered, {} acks; {} arrived while masked",
        delivered(),
        acks(),
        while_masked()
    );
    println!(
        "[irq] refused: {} orphaned (no owner), {} stale notification \
         (capability deleted), {} unbound (no SetNotification)",
        orphaned(),
        stale_notification(),
        unbound()
    );
}

// ---- IRQControl (ADR 041) ----------------------------------------------

/// `IRQControl.Get`: claim a line and put its handler in a slot.
///
/// The one operation that creates an `IrqHandler`, and it is
/// deliberately **not** a `retype` invocation: what an IrqHandler
/// names is not memory but *a line*, and there are exactly `MAX_IRQ`
/// of those whether or not anybody has memory to spare. Retype is
/// reached from here rather than from the caller, so the line's
/// exclusivity is enforced on the only path that can create one.
///
/// The object still needs somewhere to live, so the caller supplies
/// an Untyped — and the handler is linked below *that* Untyped in the
/// derivation list, which is what makes revoking the Untyped release
/// the line.
///
/// # Safety
///
/// `control_slot` must hold a live IRQControl capability,
/// `untyped_slot` a live Untyped, and `dest_offset` name a slot in
/// the live CNode `dest_cnode`.
pub unsafe fn irq_control_get(
    control_slot: &Slot,
    irq: u8,
    untyped_slot: &mut Slot,
    dest_cnode: &super::cnode::CNode,
    dest_offset: usize,
) -> Result<(), CapError> {
    let _guard = crate::sched::InterruptGuard::new();

    // The authority. Without an IRQControl capability there is no way
    // to reach this at all -- which is the point, and what the
    // `irq-no-cap` negative demonstrates.
    if control_slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if control_slot.cap_type() != CapType::IrqControl {
        return Err(CapError::WrongType {
            found: control_slot.cap_type(),
        });
    }
    if !control_slot.rights().contains(Rights::WRITE) {
        // Handing out a line is producing authority, not reading it.
        return Err(CapError::InsufficientRights {
            needed: Rights::WRITE,
            held: control_slot.rights(),
        });
    }

    if irq as usize >= MAX_IRQ {
        return Err(CapError::InvalidArgument);
    }

    // **One owner per line** (ADR 041). Checked before anything is
    // written, so a refused Get leaves no trace -- the same atomicity
    // rule ADR 020 applies to retype, and the reason this check comes
    // before the retype rather than after it.
    if OWNERS[irq as usize].load(Ordering::Acquire) != 0 {
        return Err(CapError::IrqAlreadyOwned { irq });
    }

    // SAFETY: Caller's guarantee about the Untyped and the
    // destination. `retype` validates both and writes nothing on
    // failure, so an error here also leaves no trace.
    unsafe {
        super::untyped::retype(
            untyped_slot,
            CapType::IrqHandler,
            0,
            1,
            dest_cnode,
            dest_offset,
        )
    }?;

    // SAFETY: The retype above just put a live IrqHandler here.
    let slot = unsafe { dest_cnode.slot(dest_offset) };
    // SAFETY: As above; retype zeroed the object.
    let handler = unsafe { handler_at(slot.object()) };
    handler.irq = irq;
    handler.bound = 0;
    handler.slot_generation = 0;
    handler.notification_slot = 0;

    // The line starts **masked**, and that is not caution but
    // arithmetic: an owner that has not yet attached a Notification
    // has nowhere to receive an interrupt, so delivering one would
    // produce a refusal by construction. `Ack` is what opens the
    // line, and a driver calls it when it is ready to be interrupted.
    // SAFETY: An InterruptGuard is held for the whole function.
    unsafe {
        set_masked(irq as usize, true);
    }
    OWNERS[irq as usize].store(slot.object().0 .0, Ordering::Release);

    Ok(())
}

/// `IRQHandler.SetNotification`: where this line's interrupts go.
///
/// The badge is **not** copied here. The interrupt path reads it from
/// the capability each time, so a driver that mints a differently
/// badged capability gets differently badged interrupts without
/// telling the kernel twice (ADR 037).
///
/// # Safety
///
/// Both slots must hold live capabilities of the right types, and
/// `ntfn_slot` must live inside a CNode reached through PMAP — which
/// every slot the kernel touches does.
pub unsafe fn set_notification(handler_slot: &Slot, ntfn_slot: &Slot) -> Result<(), CapError> {
    let _guard = crate::sched::InterruptGuard::new();

    // SAFETY: Caller's guarantee.
    let handler = unsafe { handler_from_slot(handler_slot) }?;

    if ntfn_slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if ntfn_slot.cap_type() != CapType::Notification {
        return Err(CapError::WrongType {
            found: ntfn_slot.cap_type(),
        });
    }
    if !ntfn_slot.rights().contains(Rights::WRITE) {
        // Signalling a Notification is writing to it (ADR 021), and
        // this is an arrangement to signal it repeatedly. Checked now
        // rather than on the interrupt path: a rights test there
        // would be the same answer computed thousands of times.
        return Err(CapError::InsufficientRights {
            needed: Rights::WRITE,
            held: ntfn_slot.rights(),
        });
    }

    handler.notification_slot = core::ptr::from_ref(ntfn_slot) as u64 - crate::memory::pmap_base();
    handler.slot_generation = ntfn_slot.generation();
    handler.bound = 1;
    Ok(())
}

/// `IRQHandler.Ack`: the driver is finished; reopen the line.
///
/// One line of effect, and a decision behind it: **the kernel does
/// not know when this should be called** (ADR 042). The driver reads
/// its device's status register and acknowledges once the device has
/// stopped asserting the line. The kernel does not know where that
/// register is — and if it did, device logic would be in the kernel.
///
/// If `Ack` never comes, the line stays shut. That is isolation
/// rather than deadlock: a crashed driver silences its own device and
/// nothing else. The recovery path is revoking the handler, not a
/// timeout — a timeout needs a scheduling budget ADR 017 left out and
/// a kernel policy about how long a driver may take, which ADR 011
/// forbids.
///
/// # Safety
///
/// `slot` must hold a live IrqHandler capability.
pub unsafe fn ack(slot: &Slot) -> Result<(), CapError> {
    let _guard = crate::sched::InterruptGuard::new();

    // SAFETY: Caller's guarantee.
    let handler = unsafe { handler_from_slot(slot) }?;
    let irq = handler.irq as usize;
    debug_assert!(irq < MAX_IRQ, "an IrqHandler holds an out-of-range line");

    // SAFETY: An InterruptGuard is held for the whole function.
    unsafe {
        set_masked(irq, false);
    }
    ACKS.fetch_add(1, Ordering::AcqRel);
    Ok(())
}

/// Let go of a line whose handler is being destroyed (ADR 041).
///
/// Called from `release_ipc_references`, so from both `delete` and
/// `revoke`. Two things, and the second is not optional: the line
/// loses its owner **and is masked**. An ownerless open line is a
/// device raising interrupts that every handler then declines — a
/// storm that accomplishes nothing.
///
/// # Safety
///
/// `slot` must hold a live IrqHandler capability and interrupts must
/// be disabled.
pub unsafe fn release_line(slot: &Slot) -> bool {
    debug_assert!(!crate::sched::interrupts_enabled());

    // SAFETY: The caller guarantees the slot holds a live IrqHandler.
    let handler = unsafe { handler_at(slot.object()) };
    let irq = handler.irq as usize;
    if irq >= MAX_IRQ {
        return false;
    }

    // Masked **first**. Between clearing the owner and masking there
    // would be a window in which an interrupt finds no owner; closing
    // the line first makes that window empty rather than merely
    // short. The orphan counter still exists because the interrupt
    // may already have been raised before this line ran.
    // SAFETY: The caller guarantees interrupts are disabled.
    unsafe {
        set_masked(irq, true);
    }
    OWNERS[irq].store(0, Ordering::Release);

    handler.bound = 0;
    handler.notification_slot = 0;
    true
}

// ---- The interrupt path (ADR 042) --------------------------------------

/// A hardware interrupt arrived on `irq`.
///
/// **Four things, and there is no fifth** — see ADR 042. This runs in
/// interrupt context, at a rate the device chooses, inside the
/// trusted computing base. The fourth, the LAPIC EOI, belongs to the
/// dispatcher in `interrupts.rs`, which does it for every vector;
/// doing it here as well would be an extra write and a second place
/// to get it wrong.
///
/// Returns whether a Notification was signalled, which is what the
/// scenarios read.
///
/// # Safety
///
/// Called only from the interrupt dispatcher, with interrupts
/// disabled.
pub unsafe fn handle_interrupt(irq: u8) -> bool {
    debug_assert!(!crate::sched::interrupts_enabled());

    let index = irq as usize;
    if index >= MAX_IRQ {
        ORPHANED.fetch_add(1, Ordering::AcqRel);
        return false;
    }

    // 1. Who owns it?
    let owner = OWNERS[index].load(Ordering::Acquire);
    if owner == 0 {
        // ADR 042's defined behaviour for an ownerless line: swallow
        // the interrupt, shut the line, count it. Not a panic
        // (ADR 030: the kernel does not stop because of something
        // outside it), and not a silent open line (a storm).
        ORPHANED.fetch_add(1, Ordering::AcqRel);
        // SAFETY: This runs in the interrupt handler, so interrupts
        // are disabled.
        unsafe {
            set_masked(index, true);
        }
        return false;
    }

    // 2. Is the line open? A masked line is one whose driver has not
    //    yet acknowledged the previous interrupt, so this one is
    //    dropped and counted.
    //
    //    **This is the whole of scenario 4.** Remove the mask store
    //    in step 3 and this counter stops moving while a second
    //    delivery appears instead.
    if MASKED[index].load(Ordering::Acquire) != 0 {
        WHILE_MASKED.fetch_add(1, Ordering::AcqRel);
        return false;
    }

    // SAFETY: A non-zero owner is the address of a live IrqHandler;
    // `release_line` is the only path that clears it and it runs with
    // interrupts disabled.
    let handler = unsafe { handler_at(ObjectRef(PhysAddr(owner))) };

    // 3. **Mask before signalling**, and the order is a decision
    //    rather than an accident of how the lines fell.
    //
    //    Signalling makes the waiting thread runnable, and it could
    //    be scheduled before this handler returns. If the line were
    //    still open, the device could raise a second interrupt before
    //    the driver had started on the first -- and because a signal
    //    is a mask rather than a count (ADR 038), the driver would
    //    have no way to tell one interrupt from two.
    //
    //    The hardware mask goes with it. On the interrupt path that
    //    is two uncached device accesses, which is real cost -- but
    //    it is the cost of a line that is genuinely shut rather than
    //    one whose interrupts the kernel keeps taking and dropping.
    //
    // SAFETY: Interrupts are disabled in an interrupt handler.
    unsafe {
        set_masked(index, true);
    }

    if handler.bound == 0 {
        // Owned, but the driver never said where to deliver. The line
        // is now shut, so this costs one interrupt per `Ack` rather
        // than one per device assertion.
        UNBOUND.fetch_add(1, Ordering::AcqRel);
        return false;
    }

    // The destination, checked for staleness the same way every other
    // cross-object reference in this kernel is (ADR 018).
    //
    // SAFETY: `set_notification` recorded the address of a slot
    // inside a live CNode; CNodes are not freed while a capability
    // derived from their Untyped exists, and PMAP maps all RAM.
    let slot = unsafe { cnode::slot_at(PhysAddr(handler.notification_slot)) };

    if slot.is_empty()
        || slot.cap_type() != CapType::Notification
        || slot.generation() != handler.slot_generation
    {
        // The Notification capability was deleted, and possibly
        // replaced by something else entirely. Signalling through
        // this slot would deliver into whatever now occupies it.
        //
        // Counted and dropped, and the handler is unbound so the next
        // interrupt does not repeat the three comparisons. The line
        // stays masked: the driver has lost its destination, and
        // reopening a line nobody is listening to is the storm case
        // again.
        STALE_NOTIFICATION.fetch_add(1, Ordering::AcqRel);
        handler.bound = 0;
        handler.notification_slot = 0;
        return false;
    }

    // 4. Signal, through exactly the path ADR 038 built -- the same
    //    function a thread calls. Not a copy of it: a notification
    //    raised by a device and one raised by a thread must be
    //    indistinguishable, and the cheapest way to guarantee that is
    //    for there to be one of them.
    //
    //    `signal` never blocks, which is the property Notification
    //    exists to provide and the reason an interrupt handler may
    //    call it at all.
    //
    // SAFETY: The slot was just checked to hold a live Notification
    // capability with WRITE, and interrupts are disabled.
    match unsafe { crate::cap::ipc::signal(slot) } {
        Ok(()) => {
            DELIVERED.fetch_add(1, Ordering::AcqRel);
            true
        }
        Err(_) => {
            // `signal`'s only failure is insufficient rights, which
            // `set_notification` already refused. Reaching here means
            // the slot's rights were narrowed after binding -- not
            // impossible, and not worth a panic.
            STALE_NOTIFICATION.fetch_add(1, Ordering::AcqRel);
            false
        }
    }
}

// There is deliberately no `reset_for_test`.
//
// One was written and removed before it ever ran, because the
// compiler pointed out that nothing called it -- and the reason
// nothing called it is the interesting part. Each scenario is
// dispatched by cmdline and ends by exiting QEMU, so every one of
// them starts from a fresh boot with an empty ownership table. A
// reset would be a way to run two scenarios in one boot, which is
// something this project deliberately does not do: the value of the
// boot-to-exit shape is that no scenario can be contaminated by
// whatever ran before it.
//
// Keeping it "in case it is useful later" would mean a path that
// clears live ownership while capabilities still name it -- exactly
// the state `release_line` exists to prevent.
