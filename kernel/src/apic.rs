// SPDX-License-Identifier: BSD-2-Clause
//! Local APIC and its timer.
//!
//! M6 needs exactly one interrupt source: a periodic timer, so that a
//! thread which never yields can still be preempted. That is the whole
//! requirement, and it shapes what is here -- the LAPIC's timer and
//! nothing else. No IO APIC, no external interrupt routing; those
//! arrive with device drivers in M12.
//!
//! Two things have to happen before the timer can fire safely:
//!
//! 1. **The legacy PIC must be disabled.** The 8259 pair is wired up by
//!    the firmware and delivers to vectors 0x08-0x0F and 0x70-0x77 --
//!    which overlap the CPU's own exception vectors. A spurious IRQ0
//!    would arrive as #DF. Limine leaves the PIC masked, but "leaves"
//!    is not a guarantee we can build on, so this masks it explicitly.
//! 2. **The LAPIC must be enabled**, both in the `IA32_APIC_BASE` MSR
//!    and in its own spurious-interrupt register.
//!
//! Reference: Intel SDM Vol. 3A, Chapter 11 "Advanced Programmable
//! Interrupt Controller (APIC)".

use core::sync::atomic::{AtomicU64, Ordering};

use crate::io::outb;
use crate::memory::{PhysAddr, VirtAddr};
use crate::println;

/// `IA32_APIC_BASE`. Bit 11 enables the LAPIC; bits 12..=51 hold the
/// physical base address of its register page.
///
/// Reference: Intel SDM Vol. 3A, 11.4.4 "Local APIC Status and
/// Location", Figure 11-5.
const IA32_APIC_BASE: u32 = 0x1B;

/// Bit 11 of `IA32_APIC_BASE`: global enable.
const APIC_BASE_ENABLE: u64 = 1 << 11;

/// Register offsets within the LAPIC page. All registers are 32 bits
/// and must be accessed as aligned dwords.
///
/// Reference: Intel SDM Vol. 3A, Table 11-1 "Local APIC Register
/// Address Map".
mod reg {
    /// Local APIC ID.
    pub const ID: usize = 0x020;
    /// Local APIC version.
    pub const VERSION: usize = 0x030;
    /// End-of-interrupt. Written (with zero) to acknowledge.
    pub const EOI: usize = 0x0B0;
    /// Spurious interrupt vector. Bit 8 is the software enable.
    pub const SPURIOUS: usize = 0x0F0;
    /// Timer's entry in the local vector table.
    pub const LVT_TIMER: usize = 0x320;
    /// Initial count: writing a non-zero value starts the timer.
    pub const TIMER_INITIAL_COUNT: usize = 0x380;
    /// Current count, counting down.
    pub const TIMER_CURRENT_COUNT: usize = 0x390;
    /// Divide configuration.
    pub const TIMER_DIVIDE: usize = 0x3E0;
}

/// Bit 8 of the spurious-interrupt register: APIC software enable.
///
/// Reference: Intel SDM Vol. 3A, Figure 11-23.
const SPURIOUS_ENABLE: u32 = 1 << 8;

/// Vector for spurious interrupts.
///
/// The low four bits were required to be 1 on the original P5 APIC, and
/// 0xFF is the conventional choice that satisfies that on every CPU.
pub const SPURIOUS_VECTOR: u8 = 0xFF;

/// Vector the timer delivers on.
///
/// 32 is the first vector after the CPU's reserved range (ADR 006's
/// 0..=31), so it cannot be confused with an exception.
pub const TIMER_VECTOR: u8 = 32;

/// Bit 17 of the timer LVT: periodic mode.
///
/// Reference: Intel SDM Vol. 3A, Figure 11-10 "Local Vector Table",
/// timer mode field (bits 18:17): 00 one-shot, 01 periodic.
const LVT_TIMER_PERIODIC: u32 = 1 << 17;

/// Bit 16 of any LVT entry: masked.
const LVT_MASKED: u32 = 1 << 16;

/// Divide configuration for 16.
///
/// The encoding is scattered: bits [1:0] and bit 3 together form the
/// divisor, and the value 0b0011 means divide by 16.
///
/// Reference: Intel SDM Vol. 3A, Figure 11-10 and Table 11-2.
const TIMER_DIVIDE_16: u32 = 0b0011;

/// Target tick rate, in hertz.
///
/// 100 Hz, i.e. a 10 ms quantum. The number is arbitrary and ADR 024
/// says so: it is a compromise between "the acceptance test collects
/// 100 ticks quickly enough" and "preemption is visible".
pub const TIMER_HZ: u32 = 100;

/// Virtual address of the LAPIC register page, once mapped.
static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);

/// Ticks the timer has delivered, counted here as well as in `sched`.
///
/// Two counters for the same event looks redundant and is not: this one
/// counts *interrupts taken*, the scheduler's counts *ticks processed*.
/// They differ if a tick is ever dropped before reaching the scheduler,
/// which is exactly the kind of thing worth being able to see.
static TIMER_INTERRUPTS: AtomicU64 = AtomicU64::new(0);

/// Spurious interrupts seen.
///
/// Expected to be zero. Counted anyway, because a non-zero value says
/// something real -- usually that an interrupt was masked between
/// being raised and being dispatched -- and a count of zero that was
/// never incremented is indistinguishable from one that was never
/// checked.
static SPURIOUS_INTERRUPTS: AtomicU64 = AtomicU64::new(0);

pub fn timer_interrupts() -> u64 {
    TIMER_INTERRUPTS.load(Ordering::Acquire)
}

pub fn spurious_interrupts() -> u64 {
    SPURIOUS_INTERRUPTS.load(Ordering::Acquire)
}

/// Count a timer interrupt and acknowledge it.
///
/// One function rather than two because the two must not be separated:
/// counting without acknowledging stops the timer, and acknowledging
/// without counting loses the only evidence that it ran.
///
/// # Safety
///
/// Call exactly once per timer interrupt, from its handler.
pub unsafe fn count_and_acknowledge() {
    TIMER_INTERRUPTS.fetch_add(1, Ordering::AcqRel);
    // SAFETY: Caller's guarantee.
    unsafe {
        end_of_interrupt();
    }
}

/// Count a spurious interrupt.
///
/// Deliberately does **not** acknowledge: the spurious vector is
/// specified not to need an EOI, and sending one would acknowledge
/// whatever interrupt is genuinely in service instead.
///
/// Reference: Intel SDM Vol. 3A, 11.9 "Spurious Interrupt".
pub fn count_spurious() {
    SPURIOUS_INTERRUPTS.fetch_add(1, Ordering::AcqRel);
}

/// Read a LAPIC register.
///
/// # Safety
///
/// The LAPIC must be mapped and `offset` must name a readable register.
unsafe fn read(offset: usize) -> u32 {
    let base = LAPIC_BASE.load(Ordering::Acquire);
    debug_assert!(base != 0, "LAPIC register read before apic::init()");
    // SAFETY: The caller guarantees the register exists; the page is
    // mapped uncached-writable by `init`. LAPIC registers are 32 bits
    // and 16-byte aligned, so an aligned dword read is the required
    // access width.
    unsafe { ((base + offset as u64) as *const u32).read_volatile() }
}

/// Write a LAPIC register.
///
/// # Safety
///
/// As [`read`], and `value` must be legal for the register: writing a
/// reserved bit is undefined.
unsafe fn write(offset: usize, value: u32) {
    let base = LAPIC_BASE.load(Ordering::Acquire);
    debug_assert!(base != 0, "LAPIC register write before apic::init()");
    // SAFETY: Caller's guarantee.
    unsafe {
        ((base + offset as u64) as *mut u32).write_volatile(value);
    }
}

/// Acknowledge the interrupt currently being serviced.
///
/// Must be the last thing a handler does. Until EOI is written the
/// LAPIC will not deliver another interrupt at the same or lower
/// priority -- so a handler that forgets it stops the timer dead, and
/// the symptom is a system that runs exactly one tick.
///
/// # Safety
///
/// Call once per interrupt, from the handler for that interrupt.
pub unsafe fn end_of_interrupt() {
    // SAFETY: EOI accepts any value; zero is conventional.
    unsafe {
        write(reg::EOI, 0);
    }
}

/// Mask every line on both legacy PICs.
///
/// The 8259s are chained and the firmware leaves them mapped to vectors
/// that collide with the CPU's exceptions. Masking is enough: with
/// every line masked the chips deliver nothing, and remapping them
/// would be work in service of a device M6 does not use.
///
/// Reference: Intel 8259A datasheet, OCW1 (the interrupt mask register
/// at the data port).
fn disable_legacy_pic() {
    /// Data port of the master 8259.
    const PIC1_DATA: u16 = 0x21;
    /// Data port of the slave 8259.
    const PIC2_DATA: u16 = 0xA1;

    // SAFETY: These two ports are the 8259 mask registers on every
    // PC-compatible machine. Writing 0xFF masks all eight lines of
    // each chip, which can only stop interrupts, never start them.
    unsafe {
        outb(PIC1_DATA, 0xFF);
        outb(PIC2_DATA, 0xFF);
    }

    println!("[apic] legacy 8259 PICs masked (all 16 lines)");
}

/// Read `IA32_APIC_BASE`.
///
/// # Safety
///
/// The MSR exists on every CPU with a local APIC, which is every
/// x86-64 CPU.
unsafe fn read_apic_base() -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: Caller's guarantee; RDMSR touches no memory.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") IA32_APIC_BASE,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((high as u64) << 32) | (low as u64)
}

/// Write `IA32_APIC_BASE`.
///
/// # Safety
///
/// `value` must be a legal APIC base: an aligned physical address with
/// only the defined control bits set.
unsafe fn write_apic_base(value: u64) {
    // SAFETY: Caller's guarantee.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") IA32_APIC_BASE,
            in("eax") (value & 0xFFFF_FFFF) as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Bring up the local APIC and start its timer.
///
/// Returns the measured ticks-per-second the timer was calibrated to.
///
/// # Safety
///
/// Call once, after the IDT is loaded (a timer interrupt must have
/// somewhere to go) and with interrupts still disabled. The LAPIC page
/// is mapped here, which requires the kernel's page tables to be
/// active and a frame source to be available for the intermediate
/// tables.
pub unsafe fn init() -> u32 {
    disable_legacy_pic();

    // SAFETY: IA32_APIC_BASE exists on every x86-64 CPU.
    let base_msr = unsafe { read_apic_base() };
    let phys_base = PhysAddr(base_msr & 0x000F_FFFF_FFFF_F000);

    // The LAPIC's registers are memory-mapped, and the mapping must be
    // uncacheable: these are device registers, and a cached write to
    // EOI that sits in a write-buffer would not acknowledge anything.
    //
    // KMMIO exists for exactly this (ADR 009 reserved it for M6).
    let virt_base = VirtAddr::new(crate::memory::layout::KMMIO_BASE);

    // SAFETY: The kernel's tables are active, KMMIO is unmapped, and
    // the LAPIC page is a device window the firmware reported.
    unsafe {
        map_lapic(phys_base, virt_base);
    }

    LAPIC_BASE.store(virt_base.0, Ordering::Release);

    // Enable the APIC globally, then in software.
    // SAFETY: Only setting the enable bit; the base address is the one
    // the CPU already reported.
    unsafe {
        write_apic_base(base_msr | APIC_BASE_ENABLE);
    }

    // SAFETY: The page is mapped and the APIC is globally enabled.
    unsafe {
        write(reg::SPURIOUS, SPURIOUS_ENABLE | SPURIOUS_VECTOR as u32);
    }

    // SAFETY: As above.
    let (id, version) = unsafe { (read(reg::ID) >> 24, read(reg::VERSION) & 0xFF) };
    println!(
        "[apic] lapic id {}, version {:#04x}, registers at {} (phys {})",
        id, version, virt_base, phys_base
    );

    // --- Calibrate ------------------------------------------------------
    //
    // The LAPIC timer counts at the bus frequency divided by the
    // divisor, and nothing reports that frequency portably. CPUID leaf
    // 0x15 gives it on recent Intel parts; QEMU does not implement it
    // usefully. So: count for a known interval against a clock we do
    // have.
    //
    // The PIT is the conventional reference. This kernel does not want
    // another device, so it uses the TSC instead -- which is also not
    // calibrated, but *is* monotonic, and M6 needs a tick rate that is
    // approximately right rather than exact. The figure is reported so
    // that a wildly wrong calibration is visible rather than silent.

    // SAFETY: The APIC is enabled and these registers exist.
    let ticks_per_second = unsafe { calibrate() };

    let initial_count = ticks_per_second / TIMER_HZ;
    assert!(
        initial_count > 0,
        "the calibrated LAPIC frequency ({} Hz) is below the target tick \
         rate ({} Hz); the timer would never fire",
        ticks_per_second,
        TIMER_HZ
    );

    // SAFETY: Periodic mode with our vector, unmasked, and a non-zero
    // initial count -- which is what starts the timer.
    unsafe {
        write(reg::TIMER_DIVIDE, TIMER_DIVIDE_16);
        write(reg::LVT_TIMER, LVT_TIMER_PERIODIC | TIMER_VECTOR as u32);
        write(reg::TIMER_INITIAL_COUNT, initial_count);
    }

    println!(
        "[apic] timer: {} Hz on vector {}, divisor 16, initial count {} \
         (calibrated to {} ticks/s)",
        TIMER_HZ, TIMER_VECTOR, initial_count, ticks_per_second
    );

    ticks_per_second
}

/// Measure how fast the LAPIC timer counts, in ticks per second.
///
/// # Safety
///
/// The APIC must be enabled and its registers mapped.
unsafe fn calibrate() -> u32 {
    /// How long to count for, in TSC cycles.
    ///
    /// The TSC's own frequency is unknown, so this is calibrated
    /// against an assumed 1 GHz and the result scaled. Under emulation
    /// none of these numbers are real; what matters is that the
    /// resulting tick rate is in the right order of magnitude, and the
    /// boot log prints it so a wrong one is visible.
    const SAMPLE_TSC_CYCLES: u64 = 10_000_000;
    const ASSUMED_TSC_HZ: u64 = 1_000_000_000;

    // SAFETY: Setting the divisor and starting a one-shot count.
    unsafe {
        write(reg::TIMER_DIVIDE, TIMER_DIVIDE_16);
        write(reg::LVT_TIMER, LVT_MASKED);
        write(reg::TIMER_INITIAL_COUNT, u32::MAX);
    }

    let start = read_tsc();
    // Bounded: the loop ends when the TSC has advanced by a fixed
    // amount, and the TSC is monotonic and never stops.
    while read_tsc() - start < SAMPLE_TSC_CYCLES {
        core::hint::spin_loop();
    }

    // SAFETY: Reading the current count and stopping the timer.
    let remaining = unsafe {
        let current = read(reg::TIMER_CURRENT_COUNT);
        write(reg::TIMER_INITIAL_COUNT, 0);
        current
    };

    let elapsed_ticks = (u32::MAX - remaining) as u64;
    let sample_seconds_numerator = SAMPLE_TSC_CYCLES;

    // ticks/s = elapsed_ticks / (SAMPLE_TSC_CYCLES / ASSUMED_TSC_HZ)
    let ticks_per_second = elapsed_ticks * ASSUMED_TSC_HZ / sample_seconds_numerator;

    // Clamp into something sane. An emulator can return figures that
    // make the initial count zero or overflow; neither should take the
    // kernel down, and both should be visible.
    let clamped = ticks_per_second.clamp(TIMER_HZ as u64 * 10, u32::MAX as u64);
    if clamped != ticks_per_second {
        println!(
            "[apic] WARNING: calibration gave {} ticks/s, clamped to {}; the \
             tick rate will not be {} Hz",
            ticks_per_second, clamped, TIMER_HZ
        );
    }

    clamped as u32
}

/// Read the timestamp counter.
fn read_tsc() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: RDTSC is unprivileged (CR4.TSD is clear), reads no
    // memory, and writes only EDX:EAX.
    //
    // Reference: Intel SDM Vol. 2B, "RDTSC".
    unsafe {
        core::arch::asm!(
            "rdtsc",
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((high as u64) << 32) | (low as u64)
}

/// Map the LAPIC's register page into KMMIO.
///
/// # Safety
///
/// The kernel's page tables must be active, `virt` must be unmapped,
/// and `phys` must be the LAPIC's register page.
unsafe fn map_lapic(phys: PhysAddr, virt: VirtAddr) {
    use crate::memory::paging::{self, PageFlags};

    // Device registers: readable, writable, never executable, and
    // **not cached**. A cached EOI write could sit in a write buffer
    // and acknowledge nothing.
    //
    // PCD (bit 4) is the page-level cache disable. The paging module's
    // PageFlags does not expose it -- no previous milestone needed an
    // uncached mapping -- so this is the one place that reaches for it.
    let flags = PageFlags::READ | PageFlags::WRITE | PageFlags::GLOBAL | PageFlags::NO_CACHE;

    // SAFETY: The caller guarantees the tables are live and the
    // address is free. The frame source is the heap's reserve, which
    // is the only source left after ADR 022's retirement -- and the
    // LAPIC page itself is device memory, not a frame from anywhere.
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
        .expect("could not map the LAPIC register page");
        guard.flush();
    }
}
