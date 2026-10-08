// SPDX-License-Identifier: BSD-2-Clause
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

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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

/// Whether the chip was found and initialised.
pub fn present() -> bool {
    IOAPIC_BASE.load(Ordering::Acquire) != 0
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
    }

    disagreements
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
