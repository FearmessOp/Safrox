// SPDX-License-Identifier: MPL-2.0
//! I/O port access as a capability (ADR 045).
//!
//! **The gap ADR 017 left.** The device Untyped model was built for
//! MMIO: a device's physical range becomes an Untyped, a Frame is
//! retyped out of it, the Frame is mapped into a component's address
//! space, and the kernel steps out of the way entirely.
//!
//! x86's I/O ports do not fit that shape. A port is not in the
//! address space — `in` and `out` reach a separate 16-bit space, and
//! whether they are allowed is decided by CPL against IOPL and by a
//! bitmap in the TSS, not by a page table (Intel SDM Vol. 1, 19.3;
//! Vol. 3A, 20.4 and 20.5). There is no page to map.
//!
//! So the kernel executes the instruction on the caller's behalf,
//! after checking that the capability covers the port. The driver's
//! IOPL stays 0 and a direct `out` from ring 3 raises #GP.
//!
//! # Two types, not one
//!
//! [`CapType::IoPortControl`] hands out ranges; an `IoPort` uses one.
//! The split is ADR 041's, for ADR 041's reason: if a single type
//! could do both, then either memory would buy ports (retype) or the
//! owner of 0x3F8 could issue itself 0x70 (privilege escalation).
//! **The right to hand out authority and the right to use it are
//! different rights.**
//!
//! # A range, not a port
//!
//! One capability covers `[first, last]`. The UART's eight registers
//! are one object. Per-port capabilities would mean the driver
//! carrying eight slots and picking the right one for each register
//! offset — and applying a register offset to the wrong capability is
//! a mistake the range check could not catch, because each individual
//! access would be legitimate.
//!
//! # One owner
//!
//! Issuing a range that overlaps a live one is refused. Two
//! components driving one UART is a state with no answer to "who set
//! DLAB" — the same question ADR 041 asked about interrupt lines, and
//! the same answer: sharing is a multiplexer's job, and the
//! multiplexer owns the device.

use core::sync::atomic::{AtomicU64, Ordering};

use super::cnode::Slot;
use super::{CapError, CapType, ObjectRef, Rights};

/// How many issued ranges the kernel tracks.
///
/// A fixed table for the same reason [`crate::cap::irq`]'s is fixed:
/// the overlap check runs inside an invocation that must not
/// allocate. Sixteen is far more than the one range this milestone
/// issues, and growing it is a one-line change.
pub const MAX_RANGES: usize = 16;

/// COM1's register window: the base plus its seven registers.
///
/// 0x3F8 is an IBM PC platform convention rather than a property of
/// the 16550A, and QEMU's default `-serial` device is wired here.
///
/// Reference: OSDev wiki, "Serial Ports" — Port Addresses.
pub const COM1_FIRST: u16 = 0x3F8;
pub const COM1_LAST: u16 = 0x3FF;

// There is deliberately no `CMOS_PORT` constant here.
//
// Scenario 5 has the driver attempt 0x70 — the CMOS/RTC index
// register, chosen because writing it selects a byte of the
// machine's configuration and the pair 0x70/0x71 can rewrite it. A
// UART driver that could reach it would hold far more authority
// than driving a UART needs.
//
// The literal lives in the *driver*, which is the component making
// the attempt. A constant here would be the kernel naming a port it
// has no opinion about: `covers()` does not special-case anything,
// and a reader finding `CMOS_PORT` in the capability layer would
// reasonably assume it did.

/// Authority over a range of I/O ports. 32 bytes (ADR 017).
#[repr(C, align(32))]
pub struct IoPort {
    /// First port in the range, inclusive.
    first: u16,
    /// Last port in the range, inclusive.
    last: u16,
    _pad: [u8; 4],
    /// Accesses performed through this capability, for the report.
    ///
    /// Inside the object rather than a global, so that revoking the
    /// capability takes the counter with it — a global would survive
    /// and make a restarted driver's figures include its
    /// predecessor's.
    accesses: u64,
    _reserved: [u64; 2],
}

const _: () = {
    assert!(
        core::mem::size_of::<IoPort>() == super::IOPORT_SIZE,
        "IoPort is not IOPORT_SIZE bytes; ADR 017's table and retype's \
         arithmetic disagree"
    );
    assert!(
        core::mem::align_of::<IoPort>() == super::IOPORT_SIZE,
        "an IoPort's alignment must equal its size (ADR 017)"
    );
};

impl IoPort {
    pub fn first(&self) -> u16 {
        self.first
    }

    pub fn last(&self) -> u16 {
        self.last
    }

    pub fn accesses(&self) -> u64 {
        self.accesses
    }

    /// Whether this capability covers `port`.
    ///
    /// **The whole of scenario 5.** Delete this test's use in
    /// [`out8`] and [`in8`] and a driver holding COM1 can reach every
    /// port on the machine.
    pub fn covers(&self, port: u16) -> bool {
        port >= self.first && port <= self.last
    }
}

/// # Safety
///
/// `object` must name a live IoPort, unaliased.
pub unsafe fn port_at(object: ObjectRef) -> &'static mut IoPort {
    debug_assert!(!object.is_null(), "null IoPort address");
    debug_assert!(
        object.0 .0.is_multiple_of(super::IOPORT_SIZE as u64),
        "IoPort at {} is not {}-byte aligned",
        object,
        super::IOPORT_SIZE
    );
    // SAFETY: Caller's guarantee; PMAP maps all RAM writably.
    unsafe { &mut *crate::memory::phys_to_virt(object.0).as_ptr::<IoPort>() }
}

/// Borrow the IoPort a capability names.
///
/// # Safety
///
/// `slot` must hold a live IoPort capability, unaliased.
pub unsafe fn port_from_slot(slot: &Slot) -> Result<&'static mut IoPort, CapError> {
    if slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if slot.cap_type() != CapType::IoPort {
        return Err(CapError::WrongType {
            found: slot.cap_type(),
        });
    }
    // SAFETY: The capability names an IoPort-sized, aligned object.
    Ok(unsafe { port_at(slot.object()) })
}

// ---- The issued-range table --------------------------------------------
//
// Packed as `(first << 16) | last | LIVE`, one atomic per entry. A
// lock would be between an invocation and nothing else -- no
// interrupt path reads this -- but atomics cost nothing here and
// match the shape `irq::OWNERS` already uses.

/// Bit 32 marks an entry as live, so that the range `0x0000..=0x0000`
/// is representable. Without it, "port 0, length 1" and "empty" are
/// the same bits — and port 0 is a real port.
const LIVE: u64 = 1 << 32;

static RANGES: [AtomicU64; MAX_RANGES] = [const { AtomicU64::new(0) }; MAX_RANGES];

fn pack(first: u16, last: u16) -> u64 {
    LIVE | ((first as u64) << 16) | (last as u64)
}

fn unpack(bits: u64) -> Option<(u16, u16)> {
    if bits & LIVE == 0 {
        return None;
    }
    Some((((bits >> 16) & 0xFFFF) as u16, (bits & 0xFFFF) as u16))
}

/// Whether any live range overlaps `[first, last]`.
pub fn overlaps(first: u16, last: u16) -> bool {
    for entry in &RANGES {
        if let Some((have_first, have_last)) = unpack(entry.load(Ordering::Acquire)) {
            // Two closed intervals overlap unless one ends before the
            // other begins.
            if first <= have_last && have_first <= last {
                return true;
            }
        }
    }
    false
}

/// Record a range as issued. Returns false if the table is full.
fn record(first: u16, last: u16) -> bool {
    for entry in &RANGES {
        if entry.load(Ordering::Acquire) & LIVE == 0 {
            entry.store(pack(first, last), Ordering::Release);
            return true;
        }
    }
    false
}

/// Let go of a range whose IoPort is being destroyed.
///
/// Called from `release_ipc_references`, so from both `delete` and
/// `revoke` — the same hook [`crate::cap::irq::release_line`] uses,
/// and for the same reason: an authority that outlived the
/// capability naming it could never be reclaimed.
///
/// **This is what makes `driver-restart` possible.** Without it the
/// first driver's range would stay in the table forever and the
/// replacement would be refused as an overlap.
///
/// # Safety
///
/// `slot` must hold a live IoPort capability.
pub unsafe fn release_range(slot: &Slot) -> bool {
    // SAFETY: The caller guarantees the slot holds a live IoPort.
    let port = unsafe { port_at(slot.object()) };
    let wanted = pack(port.first, port.last);

    for entry in &RANGES {
        if entry.load(Ordering::Acquire) == wanted {
            entry.store(0, Ordering::Release);
            return true;
        }
    }
    false
}

// ---- IOPortControl (ADR 045) -------------------------------------------

/// `IOPortControl.Issue`: carve a range out and put it in a slot.
///
/// Like [`crate::cap::irq::irq_control_get`] and for the same reason,
/// this is **not** reachable through `retype`: what an IoPort names
/// is not memory but a range of ports, and there are exactly 65536 of
/// those whether or not anybody has memory to spare. Retype is
/// reached from inside here, so the one-owner rule is enforced on the
/// only path that can create one.
///
/// The object still has to live somewhere, so the caller supplies an
/// Untyped — and the IoPort is linked below *that* Untyped in the
/// derivation list, which is what makes revoking the Untyped release
/// the range.
///
/// # Safety
///
/// `control_slot` must hold a live IOPortControl capability,
/// `untyped_slot` a live Untyped, and `dest_offset` name a slot in
/// the live CNode `dest_cnode`.
pub unsafe fn issue(
    control_slot: &Slot,
    first: u16,
    last: u16,
    untyped_slot: &mut Slot,
    dest_cnode: &super::cnode::CNode,
    dest_offset: usize,
) -> Result<(), CapError> {
    let _guard = crate::sched::InterruptGuard::new();

    // The authority. Without an IOPortControl capability there is no
    // way to reach this at all — which is what `driver-hostile`
    // demonstrates.
    if control_slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if control_slot.cap_type() != CapType::IoPortControl {
        return Err(CapError::WrongType {
            found: control_slot.cap_type(),
        });
    }
    if !control_slot.rights().contains(Rights::WRITE) {
        // Handing out a range is producing authority, not reading it.
        return Err(CapError::InsufficientRights {
            needed: Rights::WRITE,
            held: control_slot.rights(),
        });
    }

    // An inverted range would pass every later check vacuously:
    // `covers` is false for every port, so the capability would be
    // useless rather than dangerous — but it would also occupy a
    // table entry and overlap nothing, which is a quiet way to
    // exhaust the table.
    if first > last {
        return Err(CapError::InvalidArgument);
    }

    // **One owner per port** (ADR 045, following ADR 041). Checked
    // before anything is written, so a refused Issue leaves no trace
    // — ADR 020's atomicity rule, and the reason this comes before
    // the retype rather than after it.
    if overlaps(first, last) {
        return Err(CapError::IoPortsAlreadyOwned { first, last });
    }

    // SAFETY: Caller's guarantee about the Untyped and destination.
    // `retype` validates both and writes nothing on failure, so an
    // error here also leaves no trace.
    unsafe {
        super::untyped::retype(untyped_slot, CapType::IoPort, 0, 1, dest_cnode, dest_offset)
    }?;

    // SAFETY: The retype above just put a live IoPort here.
    let slot = unsafe { dest_cnode.slot(dest_offset) };
    // SAFETY: As above; retype zeroed the object.
    let port = unsafe { port_at(slot.object()) };
    port.first = first;
    port.last = last;
    port.accesses = 0;

    if !record(first, last) {
        // The table is full. The object exists and the capability is
        // in the slot, so this has to undo rather than merely report
        // — a capability for an unrecorded range would be one the
        // overlap check cannot see, which is worse than no
        // capability at all.
        //
        // SAFETY: The slot holds the live IoPort just created, and
        // nothing else has had the chance to name it.
        unsafe {
            super::cnode::delete(slot)?;
        }
        return Err(CapError::TooManyObjects {
            count: MAX_RANGES + 1,
        });
    }

    Ok(())
}

// ---- The two accesses --------------------------------------------------

/// `IOPort.Out8`: write a byte to a port in this capability's range.
///
/// # Safety
///
/// `slot` must hold a live IoPort capability.
pub unsafe fn out8(slot: &Slot, port: u16, value: u8) -> Result<(), CapError> {
    // SAFETY: Caller's guarantee.
    let cap = unsafe { port_from_slot(slot) }?;

    if !slot.rights().contains(Rights::WRITE) {
        return Err(CapError::InsufficientRights {
            needed: Rights::WRITE,
            held: slot.rights(),
        });
    }

    // **Scenario 5.** Remove this and a driver holding COM1 reaches
    // every port on the machine — including 0x70, which rewrites the
    // CMOS, and 0x64, which can reset the box.
    //
    // Verified by removing it: `driver-test` dies at
    // `drivertest.rs:279` with *"the driver wrote to the CMOS index
    // register at 0x70 and the kernel allowed it"*, and 12 further
    // expectations go missing because the scenario never reaches
    // them. The check is not indistinguishable from its own absence
    // (ADR 023, K5).
    if !cap.covers(port) {
        return Err(CapError::PortNotInRange {
            port,
            first: cap.first,
            last: cap.last,
        });
    }

    cap.accesses = cap.accesses.wrapping_add(1);

    // SAFETY: The capability covers this port, which is the whole of
    // the authority check. Writing a byte to a port the caller is
    // entitled to drive is precisely what this invocation is for;
    // the kernel does not know or care what the device does with it.
    unsafe {
        crate::io::outb(port, value);
    }
    Ok(())
}

/// `IOPort.In8`: read a byte from a port in this capability's range.
///
/// # Safety
///
/// `slot` must hold a live IoPort capability.
pub unsafe fn in8(slot: &Slot, port: u16) -> Result<u8, CapError> {
    // SAFETY: Caller's guarantee.
    let cap = unsafe { port_from_slot(slot) }?;

    // **READ, not WRITE** — and the asymmetry is real rather than
    // tidy. Reading a UART's receive buffer pops a byte off the
    // FIFO, so an `in` has side effects; but the side effect is on
    // the device the caller already owns, and ADR 021 scopes rights
    // to the object rather than to the physics.
    if !slot.rights().contains(Rights::READ) {
        return Err(CapError::InsufficientRights {
            needed: Rights::READ,
            held: slot.rights(),
        });
    }

    if !cap.covers(port) {
        return Err(CapError::PortNotInRange {
            port,
            first: cap.first,
            last: cap.last,
        });
    }

    cap.accesses = cap.accesses.wrapping_add(1);

    // SAFETY: As `out8`. The caller holds a capability covering this
    // port, and the read's side effects are the device's own.
    Ok(unsafe { crate::io::inb(port) })
}

/// Report what the port layer has done, for the acceptance test.
pub fn report() {
    use crate::println;

    let mut ranges = 0;
    for entry in &RANGES {
        if let Some((first, last)) = unpack(entry.load(Ordering::Acquire)) {
            println!("[ioport] issued {:#06x}..={:#06x}", first, last);
            ranges += 1;
        }
    }
    println!(
        "[ioport] {} range(s) issued, {} table slots",
        ranges, MAX_RANGES
    );
}
