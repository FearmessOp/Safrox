// SPDX-License-Identifier: MPL-2.0
//! The I/O APIC: masking an interrupt line in hardware (M10's debt).
//!
//! M10 masked lines with [`crate::cap::irq`]'s software flag and said
//! so plainly, promising hardware in M12. This is that promise, and
//! the first thing to record is **what it does and does not buy**.
//!
//! # ADR 042's contract does not change
//!
//! The contract was, and remains: *no second interrupt is delivered
//! on a line between one arriving and the driver's `Ack`*. The
//! software flag already provided it — [`handle_interrupt`] checks
//! the flag before signalling, so a second interrupt is counted and
//! dropped. What the flag could not do is stop the *device* from
//! asserting the line, so the interrupt still reached the CPU, still
//! cost a dispatch, and still needed an EOI.
//!
//! Masking the redirection entry stops it at the source. The
//! observable contract is identical, which is the point: `irq-test`
//! is unchanged and still passes, because it tests the contract
//! rather than the mechanism.
//!
//! # What this cannot be tested by, and why that is said here
//!
//! **`irq-test` raises its interrupts with a self-IPI**
//! ([`crate::apic::raise_irq`]), and a self-IPI goes from the LAPIC
//! straight to the CPU. It does not pass through the I/O APIC, so no
//! redirection-table mask can stop it.
//!
//! That is not a flaw in the test — M10 chose a self-IPI
//! deliberately, because with a real device "no second interrupt
//! arrived" and "the device sent nothing" are indistinguishable
//! (ADR 023's K8). It does mean the hardware mask is **not** what
//! makes `irq-test` pass, and claiming otherwise would be exactly
//! the kind of unfalsifiable statement this project refuses.
//!
//! So what is checked here is what *can* be: the register writes
//! land. [`verify`] reads each redirection entry back from the chip
//! and compares it against what was written. A mask that never
//! reached the hardware reads back unmasked, and that is a real
//! failure a real test can see — whereas "the interrupt did not
//! arrive" would have been true anyway.
//!
//! The honest summary is: the flag is what enforces the contract
//! today; the hardware mask is what will enforce it when a device
//! interrupt exists to be stopped (M13), and it is wired up and
//! verified now so that M13 does not inherit an untested path.
//!
//! Reference: Intel 82093AA I/O Advanced Programmable Interrupt
//! Controller datasheet, sections 3.2 (register access) and 3.2.4
//! (the redirection table).

use core::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crate::memory::{PhysAddr, VirtAddr};
use crate::println;

/// Physical address of the I/O APIC on a PC-compatible machine.
///
/// Hard-coded rather than read from ACPI's MADT, and the trade is
/// worth stating: parsing ACPI means a table walker in the kernel,
/// which is a parser for firmware-supplied data inside the trusted
/// base. 0xFEC0_0000 is what every PC and every QEMU machine type
/// uses, and a wrong guess is caught immediately — [`init`] reads
/// the version register back and refuses to continue if it does not
/// look like an I/O APIC.
///
/// A real system would read the MADT. A real system would also have
/// a user-space ACPI component to read it, which is the M13 shape.
const IOAPIC_PHYS_BASE: u64 = 0xFEC0_0000;

/// The register-select window. Write the register number here.
const IOREGSEL: usize = 0x00;

/// The data window. Reads and writes go to the selected register.
const IOWIN: usize = 0x10;

/// Register 0x01: version, and in bits 16..=23 the number of
/// redirection entries minus one.
const REG_VERSION: u32 = 0x01;

/// Register 0x10 is the first redirection entry's low dword; each
/// entry is two dwords.
const REG_REDIR_BASE: u32 = 0x10;

/// Bit 16 of a redirection entry's low dword: **masked**.
///
/// Reference: 82093AA datasheet, 3.2.4, "Interrupt Mask".
const REDIR_MASKED: u32 = 1 << 16;

// ---- The rest of a redirection entry -----------------------------------
//
// A redirection entry is 64 bits across two consecutive registers,
// and until M15 this module wrote exactly one of those bits. The
// others carry where the interrupt goes and how the line behaves,
// and leaving them at whatever the firmware left behind means
// unmasking an entry that delivers vector 0 to nobody.
//
// Reference throughout: 82093AA I/O APIC datasheet, 3.2.4,
// "I/O Redirection Table Registers"; Intel SDM Vol. 3A, 11.5.1 for
// the delivery modes the local APIC accepts.

/// Delivery mode, bits 10:8. **000 = Fixed**: deliver the vector in
/// bits 7:0 to the processors named by the destination field.
///
/// The alternatives are for things this kernel does not do — SMI,
/// NMI, INIT and ExtINT all ignore the vector field, and Lowest
/// Priority picks a CPU by TPR, which needs more than one CPU to
/// mean anything (`hardware-todo.md` item 7).
const REDIR_DELIVERY_FIXED: u32 = 0b000 << 8;

/// Destination mode, bit 11. **0 = Physical**: the destination field
/// is a LAPIC ID rather than a logical bitmask.
///
/// Physical because the kernel addresses the boot CPU by its ID and
/// has no logical destination scheme to address it by.
const REDIR_DEST_PHYSICAL: u32 = 0 << 11;

/// Polarity, bit 13. **0 = active high.**
///
/// The ISA default, and an assumption: the authoritative answer is
/// in ACPI's MADT, in the Interrupt Source Override entries, which
/// this kernel does not read. `hardware-todo.md` item 13.
const REDIR_ACTIVE_HIGH: u32 = 0 << 13;

/// Trigger mode, bit 15. **0 = edge triggered.**
///
/// Also the ISA default and also an assumption; the same MADT
/// entries carry the real answer. Edge matters here beyond
/// correctness: a level-triggered line that is never de-asserted
/// re-fires forever, so getting this wrong on real hardware is an
/// interrupt storm rather than a missing interrupt.
const REDIR_EDGE: u32 = 0 << 15;

/// The CPU interrupts are delivered to.
///
/// Zero: the boot processor, which is the only one this kernel
/// starts. The single-CPU assumption is already recorded as debt
/// (`hardware-todo.md` item 7) and this is one more place it is
/// built in.
///
/// In physical destination mode this is a LAPIC ID, and it goes in
/// bits 59:56 of the entry — the **high** dword's bits 27:24.
const REDIR_DESTINATION: u32 = 0;

/// Virtual address of the register window, once mapped.
static IOAPIC_BASE: AtomicU64 = AtomicU64::new(0);

/// How many redirection entries the chip reports.
static REDIRECTION_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Hardware mask operations performed.
///
/// Counted so the acceptance test can show the path ran at all. A
/// mask that is never applied reads back identically to one that is
/// applied correctly *if nothing checks* — this is half of what
/// stops that, and [`verify`] is the other half.
static MASK_WRITES: AtomicU64 = AtomicU64::new(0);

/// How many lines this module tracks the routing of.
///
/// A bound on the table below, not on the hardware: a chip
/// reporting more entries than this simply has the extra ones
/// unverified, rather than this writing past an array.
const MAX_LINES: usize = 32;

/// Redirection entries pointed at a vector (M15).
static ROUTE_WRITES: AtomicU64 = AtomicU64::new(0);

/// What vector each line was routed to, as the kernel intended.
///
/// **Deliberately not a shadow of the hardware.** A shadow would
/// agree with what the kernel meant to write, which is exactly the
/// thing [`verify`] exists to doubt. This records the intent so
/// that `verify` has something to hold the chip's own answer
/// against; the two disagreeing is the finding.
///
/// Zero means "not routed". Vector 0 is #DE and no line is ever
/// pointed at it, so the sentinel cannot collide with a real value.
static ROUTED_VECTOR: [AtomicU8; MAX_LINES] = [const { AtomicU8::new(0) }; MAX_LINES];

/// Whether the chip was found and initialised.
pub fn present() -> bool {
    IOAPIC_BASE.load(Ordering::Acquire) != 0
}

/// How many redirection entries have been pointed at a vector.
pub fn route_writes() -> u64 {
    ROUTE_WRITES.load(Ordering::Acquire)
}

/// How many redirection entries the chip has.
pub fn redirection_count() -> usize {
    REDIRECTION_COUNT.load(Ordering::Acquire)
}

/// How many mask/unmask writes have reached the hardware.
pub fn mask_writes() -> u64 {
    MASK_WRITES.load(Ordering::Acquire)
}

/// Select a register and read it.
///
/// # Safety
///
/// The chip must be mapped and `register` must exist.
unsafe fn read(register: u32) -> u32 {
    let base = IOAPIC_BASE.load(Ordering::Acquire);
    debug_assert!(base != 0, "I/O APIC read before ioapic::init()");
    // SAFETY: The window is mapped uncached by `init`. The two
    // accesses must not be reordered or merged, which is why both
    // are volatile: the select write is what gives the data read its
    // meaning.
    unsafe {
        ((base + IOREGSEL as u64) as *mut u32).write_volatile(register);
        ((base + IOWIN as u64) as *const u32).read_volatile()
    }
}

/// Select a register and write it.
///
/// # Safety
///
/// As [`read`], and `value` must be legal for the register.
unsafe fn write(register: u32, value: u32) {
    let base = IOAPIC_BASE.load(Ordering::Acquire);
    debug_assert!(base != 0, "I/O APIC write before ioapic::init()");
    // SAFETY: Caller's guarantee; see `read` on the volatile pair.
    unsafe {
        ((base + IOREGSEL as u64) as *mut u32).write_volatile(register);
        ((base + IOWIN as u64) as *mut u32).write_volatile(value);
    }
}

/// The register number of a line's low dword.
const fn redirection_register(irq: u8) -> u32 {
    REG_REDIR_BASE + (irq as u32) * 2
}

/// Point a line at its vector, leaving it masked.
///
/// **This was missing until M15, and the hole was invisible for
/// five milestones.** `set_masked` writes bit 16 and nothing else,
/// so an entry's vector, delivery mode and destination stayed at
/// whatever the firmware left. Unmasking such an entry opens a line
/// that delivers an undefined vector to an undefined CPU — in QEMU,
/// nothing arrives at all.
///
/// Nothing caught it because nothing had used the chip. M10's
/// interrupt tests raise their interrupts with a self-IPI, which
/// goes LAPIC-to-CPU and never passes through here (the note
/// `init` prints says so). M13's serial driver owns IRQ 4 but its
/// scenarios exercise the write path rather than the line. IRQ 1,
/// in M15, is the first real device line this kernel has had to
/// deliver.
///
/// # Called from `IRQControl.Get`, not from `init`
///
/// A line is routed when it is **claimed**, not when the chip is
/// found. That follows the capability model: an unowned line has
/// nowhere to deliver, and writing a vector for it would be
/// configuring hardware on behalf of a component that does not
/// exist. ADR 041 starts a claimed line masked for the same reason
/// and this is the other half of that step.
///
/// The entry stays masked here. `Ack` opens it, once the driver has
/// bound a Notification and is ready to receive (ADR 042).
///
/// # What is assumed
///
/// Edge triggered and active high: the ISA defaults. The
/// authoritative source is ACPI's MADT, whose Interrupt Source
/// Override entries say when a line differs, and this kernel does
/// not read it. For IRQ 1 on QEMU's q35 there is typically no
/// override, so the assumption is probably not merely convenient
/// but correct — which shrinks the debt rather than removing it.
/// `hardware-todo.md` item 13.
///
/// Reference: 82093AA datasheet, 3.2.4; Intel SDM Vol. 3A, 11.5.1.
///
/// # Safety
///
/// As [`set_masked`]: interrupts must be disabled, because this
/// writes the select/data pair the interrupt path also uses.
pub unsafe fn route(irq: u8, vector: u8) {
    if !present() || (irq as usize) >= redirection_count() {
        return;
    }
    debug_assert!(!crate::sched::interrupts_enabled());

    let low = (vector as u32)
        | REDIR_DELIVERY_FIXED
        | REDIR_DEST_PHYSICAL
        | REDIR_ACTIVE_HIGH
        | REDIR_EDGE
        | REDIR_MASKED;

    // The destination is bits 59:56 of the entry, which is bits
    // 27:24 of the high dword.
    let high = REDIR_DESTINATION << 24;

    let register = redirection_register(irq);
    // SAFETY: The chip is mapped and `irq` is below the reported
    // count, so both dwords of this entry exist.
    unsafe {
        // **The high dword first.** Writing the low dword is what
        // arms the entry — it carries the mask bit and the vector —
        // so setting the destination afterwards would leave a
        // window in which a live entry points at a destination that
        // has not been written yet. The entry is masked here, so
        // the window is theoretical; the order costs nothing and
        // the habit is worth more than the instruction.
        write(register + 1, high);
        write(register, low);
    }

    if (irq as usize) < MAX_LINES {
        ROUTED_VECTOR[irq as usize].store(vector, Ordering::Release);
    }
    ROUTE_WRITES.fetch_add(1, Ordering::AcqRel);
}

/// Mask or unmask a line in the hardware.
///
/// Does nothing if the chip was not found, which is what lets this
/// be called unconditionally from [`crate::cap::irq`]: a machine
/// without an I/O APIC still has the software flag, and the contract
/// is the flag's to keep.
///
/// # Safety
///
/// Call with interrupts disabled — this is a read-modify-write of a
/// register the interrupt path also reads, and a tick landing
/// between the two halves would make the write operate on stale
/// bits.
pub unsafe fn set_masked(irq: u8, masked: bool) {
    if !present() || (irq as usize) >= redirection_count() {
        return;
    }
    debug_assert!(!crate::sched::interrupts_enabled());

    let register = redirection_register(irq);
    // SAFETY: The chip is mapped and the register is a redirection
    // entry's low dword, which the count check above bounded.
    unsafe {
        let current = read(register);
        let updated = if masked {
            current | REDIR_MASKED
        } else {
            current & !REDIR_MASKED
        };
        write(register, updated);
    }
    MASK_WRITES.fetch_add(1, Ordering::AcqRel);
}

/// Whether the hardware says a line is masked.
///
/// Reads the chip rather than a shadow copy, which is the whole
/// value of this function: a shadow would agree with what the kernel
/// *meant* to write.
///
/// Returns `None` if there is no I/O APIC or no such line.
pub fn hardware_masked(irq: u8) -> Option<bool> {
    if !present() || (irq as usize) >= redirection_count() {
        return None;
    }
    // SAFETY: The chip is mapped and the register exists. A read of
    // the select/data pair is not atomic against a concurrent write,
    // so this is for diagnostics and tests rather than the interrupt
    // path -- which uses the software flag, one atomic load.
    let value = unsafe { read(redirection_register(irq)) };
    Some(value & REDIR_MASKED != 0)
}

/// Bring up the I/O APIC, with every line masked.
///
/// # Safety
///
/// Call once, after `apic::init` has mapped the LAPIC and with
/// interrupts disabled.
pub unsafe fn init() {
    let phys = PhysAddr(IOAPIC_PHYS_BASE);
    // One page past the LAPIC's, in the same KMMIO region.
    let virt = VirtAddr::new(crate::memory::layout::KMMIO_BASE + 0x1000);

    // SAFETY: The kernel's tables are active, this KMMIO page is
    // free, and the address is the conventional I/O APIC window.
    unsafe {
        map_window(phys, virt);
    }
    IOAPIC_BASE.store(virt.0, Ordering::Release);

    // Is there actually a chip here? The version register's low byte
    // is the version and bits 16..=23 are "entries minus one". A
    // window that is not an I/O APIC reads as all-ones or all-zeroes
    // under QEMU, and both produce a nonsense count.
    //
    // SAFETY: The window is mapped.
    let version_reg = unsafe { read(REG_VERSION) };
    let version = version_reg & 0xFF;
    let count = ((version_reg >> 16) & 0xFF) as usize + 1;

    if version == 0 || version == 0xFF || count <= 1 || count > 240 {
        // No chip, or not one we understand. **Not a panic**: the
        // software flag keeps ADR 042's contract on its own, and a
        // kernel that refused to boot without an I/O APIC would be
        // refusing over a capability it does not yet need.
        IOAPIC_BASE.store(0, Ordering::Release);
        println!(
            "[ioapic] no I/O APIC at {} (version register reads {:#010x}); \
             line masking stays in software, which is what keeps ADR 042's \
             contract either way",
            phys, version_reg
        );
        return;
    }

    REDIRECTION_COUNT.store(count, Ordering::Release);

    // **Every line masked**, before anything can be routed to it.
    // The same reasoning as `IRQControl.Get` starting a line masked:
    // a line with no owner has nowhere to deliver, so an open one
    // can only produce refusals.
    for irq in 0..count {
        let register = REG_REDIR_BASE + (irq as u32) * 2;
        // SAFETY: The chip is mapped and `irq` is below the count it
        // reported.
        unsafe {
            let current = read(register);
            write(register, current | REDIR_MASKED);
        }
    }

    println!(
        "[ioapic] version {:#04x} at {}, {} redirection entries, all \
         masked (M10's debt: the mask is now in hardware)",
        version, phys, count
    );
    println!(
        "[ioapic] note: irq-test raises interrupts with a self-IPI, which \
         goes LAPIC-to-CPU and does not pass through this chip -- so the \
         hardware mask is not what makes that scenario pass, and saying \
         otherwise would be unfalsifiable. What is checked is that the \
         writes land: see ioapic::verify"
    );
}

/// Check that what was written to the chip reads back.
///
/// **The falsifiable part.** Masking a line that no device drives
/// produces no observable difference in behaviour, so "the mask
/// worked" cannot be shown by the absence of an interrupt — the
/// interrupt was never going to arrive. What can be shown is that
/// the register holds what the kernel put there.
///
/// Returns the number of lines whose hardware state disagreed with
/// the software flag. Zero is the expected answer.
///
/// # Safety
///
/// Interrupts must be disabled: this reads the select/data pair,
/// which `set_masked` also writes.
pub unsafe fn verify() -> usize {
    if !present() {
        return 0;
    }
    debug_assert!(!crate::sched::interrupts_enabled());

    let mut disagreements = 0usize;
    let lines = redirection_count().min(crate::cap::irq::MAX_IRQ);

    // `irq` indexes the hardware's registers, the software mask
    // table and `ROUTED_VECTOR` — three things, only one of which
    // is an array here. `.enumerate()` over one of them would name
    // the loop after whichever was chosen and leave the others
    // indexed anyway.
    #[allow(clippy::needless_range_loop)]
    for irq in 0..lines {
        let software = crate::cap::irq::is_masked(irq as u8);
        let hardware = hardware_masked(irq as u8);
        if hardware != Some(software) {
            println!(
                "[ioapic] line {}: software says {}, hardware says {:?}",
                irq,
                if software { "masked" } else { "open" },
                hardware
            );
            disagreements += 1;
        }

        // **The rest of the entry**, for a routed line.
        //
        // Reading only the mask bit is what let M15's bug live for
        // five milestones: this function reported every line in
        // agreement, and it was telling the truth — the bit it
        // wrote did read back. The vector, which nothing wrote, was
        // not being looked at (ADR 023, K12).
        let intended = if irq < MAX_LINES {
            ROUTED_VECTOR[irq].load(Ordering::Acquire)
        } else {
            0
        };
        if intended == 0 {
            // Not routed, so there is nothing to agree with. An
            // unowned line's entry holds whatever the firmware left
            // and the kernel makes no claim about it.
            continue;
        }

        let register = redirection_register(irq as u8);
        // SAFETY: `irq` is below the reported count, so both dwords
        // of this entry exist; interrupts are disabled per the
        // function's contract.
        let (low, high) = unsafe { (read(register), read(register + 1)) };

        let vector = (low & 0xFF) as u8;
        if vector != intended {
            println!(
                "[ioapic] line {}: routed to vector {} but the chip \
                 holds {}. An entry that is unmasked now delivers \
                 somewhere nobody is listening",
                irq, intended, vector
            );
            disagreements += 1;
        }

        // Delivery mode, destination mode, polarity and trigger
        // mode, all of which `route` writes as zero. Checked as one
        // mask rather than four comparisons: the question is
        // whether any of them came back different, and naming which
        // in the message is what the hex is for.
        const CHECKED: u32 = (0b111 << 8) | (1 << 11) | (1 << 13) | (1 << 15);
        if low & CHECKED != 0 {
            println!(
                "[ioapic] line {}: delivery/polarity/trigger bits are \
                 {:#010x}, expected zero (fixed, physical, active \
                 high, edge)",
                irq,
                low & CHECKED
            );
            disagreements += 1;
        }

        let destination = (high >> 24) & 0xFF;
        if destination != REDIR_DESTINATION {
            println!(
                "[ioapic] line {}: destination is LAPIC {} and not {}",
                irq, destination, REDIR_DESTINATION
            );
            disagreements += 1;
        }
    }

    disagreements
}

/// Report what the chip holds for a line, for the acceptance tests.
///
/// Returns `(vector, masked)` read **from the hardware**, or `None`
/// if there is no chip or no such line.
///
/// Exists so that a test can say "IRQ 1 is pointed at vector 34 and
/// open" with the chip's own answer rather than the kernel's
/// intention — the distinction [`verify`] is built on.
///
/// # Safety
///
/// Interrupts must be disabled: this reads the select/data pair.
pub unsafe fn routing_of(irq: u8) -> Option<(u8, bool)> {
    if !present() || (irq as usize) >= redirection_count() {
        return None;
    }
    debug_assert!(!crate::sched::interrupts_enabled());

    let register = redirection_register(irq);
    // SAFETY: Bounded above; interrupts disabled per the contract.
    let low = unsafe { read(register) };
    Some(((low & 0xFF) as u8, low & REDIR_MASKED != 0))
}

/// Map the I/O APIC's register window.
///
/// # Safety
///
/// The kernel's tables must be active and `virt` unmapped.
unsafe fn map_window(phys: PhysAddr, virt: VirtAddr) {
    use crate::memory::paging::{self, PageFlags};

    // Uncached, as for the LAPIC: these are device registers, and
    // the select/data protocol is destroyed by any reordering or
    // caching of the two accesses.
    let flags = PageFlags::READ | PageFlags::WRITE | PageFlags::GLOBAL | PageFlags::NO_CACHE;

    // SAFETY: Caller's guarantee. The frame source is the heap's
    // reserve, which is the only one left after ADR 022's
    // retirement; the window itself is device memory and not a frame
    // from anywhere.
    unsafe {
        let table = crate::memory::vm::kernel_table();
        let mut source = crate::memory::heap::reserve_frames();
        let guard = paging::map(
            table,
            virt,
            crate::memory::PhysFrame::containing(phys),
            flags,
            &mut source,
        )
        .expect("could not map the I/O APIC register window");
        guard.flush();
    }
}
