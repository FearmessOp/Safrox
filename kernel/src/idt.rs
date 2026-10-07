// SPDX-License-Identifier: BSD-2-Clause
//! Interrupt Descriptor Table.
//!
//! The IDT maps a vector number to a gate descriptor telling the CPU where
//! to jump, which code segment to use, which stack to switch to, and whether
//! to leave interrupts enabled while the handler runs.
//!
//! M1 fills vectors 0..=31, the architecturally defined exceptions. Vectors
//! 32..=255 are left not-present: an unexpected interrupt there raises #GP
//! with the vector in its error code, which is far more informative than
//! jumping to whatever happened to be in uninitialised memory.
//!
//! Reference: Intel SDM Vol. 3A, 6.10 "Interrupt Descriptor Table (IDT)" and
//! Figure 6-7 "64-Bit IDT Gate Descriptors".

use core::arch::asm;
use core::mem::size_of;

use crate::apic::{irq_to_vector, IRQ_VECTOR_COUNT, SPURIOUS_VECTOR, TIMER_VECTOR};
use crate::gdt::{self, KERNEL_CODE_SELECTOR};
use crate::interrupts::{
    isr_table, EXCEPTION_COUNT, IRQ_STUB_BASE, SPURIOUS_STUB_INDEX, TIMER_STUB_INDEX,
};

/// Total number of IDT vectors the architecture defines.
const IDT_ENTRIES: usize = 256;

// ---- Gate descriptor type and attribute bits ---------------------------

/// Gate type 0b1110: 64-bit interrupt gate.
///
/// The difference from a *trap* gate (0b1111) is one bit of behaviour: an
/// interrupt gate clears IF on entry, a trap gate leaves it as it was. We
/// use interrupt gates throughout so that a handler is never itself
/// interrupted -- with one serial port and no locking discipline yet, nested
/// handlers would interleave their output and corrupt the log the tests
/// parse.
///
/// Reference: Intel SDM Vol. 3A, 6.14.1 "64-Bit Mode IDT" and Table 3-2.
const GATE_TYPE_INTERRUPT: u8 = 0b1110;

/// Bit 7 of the type/attributes byte, P: descriptor present.
const ATTR_PRESENT: u8 = 1 << 7;

/// DPL field (bits 6:5) set to 0: only ring 0 may invoke this gate with a
/// software `int n`.
///
/// This matters for #BP: `int3` from ring 3 with DPL=0 would raise #GP
/// instead of #BP. That is the behaviour we want -- user space should not be
/// able to drive kernel exception handlers directly. The kernel's own `int3`
/// runs at CPL 0, where the DPL check does not apply.
///
/// Reference: Intel SDM Vol. 3A, 6.12.1.2 "Software-Generated Exceptions".
const ATTR_DPL0: u8 = 0;

/// A 64-bit IDT gate descriptor: 16 bytes.
///
/// `#[repr(C, packed)]` because the CPU reads this layout literally. The
/// handler address is split into three pieces for backwards compatibility
/// with the 16- and 32-bit formats this descriptor grew out of.
///
/// Reference: Intel SDM Vol. 3A, Figure 6-7.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    /// Handler address, bits 15:0.
    offset_low: u16,
    /// Code segment selector the handler runs under.
    selector: u16,
    /// Bits 2:0 are the IST index (0 = no stack switch). Bits 7:3 are zero.
    ist: u8,
    /// Bits 3:0 gate type, bit 4 zero (system descriptor), bits 6:5 DPL,
    /// bit 7 present.
    type_attributes: u8,
    /// Handler address, bits 31:16.
    offset_mid: u16,
    /// Handler address, bits 63:32.
    offset_high: u32,
    /// Reserved, must be zero.
    reserved: u32,
}

impl IdtEntry {
    /// An absent entry. Taking this vector raises #GP rather than jumping
    /// into uninitialised memory.
    const fn missing() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attributes: 0, // P = 0
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    /// Build a present 64-bit interrupt gate.
    ///
    /// `ist` is 0 for "keep using the current stack", or 1..=7 to switch to
    /// `tss.ist[ist - 1]` unconditionally.
    fn interrupt_gate(handler: u64, ist: u8) -> Self {
        debug_assert!(ist <= 7, "IST index must fit in 3 bits");

        Self {
            offset_low: (handler & 0xFFFF) as u16,
            selector: KERNEL_CODE_SELECTOR,
            ist: ist & 0b111,
            type_attributes: ATTR_PRESENT | ATTR_DPL0 | GATE_TYPE_INTERRUPT,
            offset_mid: ((handler >> 16) & 0xFFFF) as u16,
            offset_high: ((handler >> 32) & 0xFFFF_FFFF) as u32,
            reserved: 0,
        }
    }
}

/// The IDT.
///
/// `static mut` because `lidt` points the CPU straight at this memory, and
/// because the gates can only be built at run time (they embed the stubs'
/// addresses).
static mut IDT: [IdtEntry; IDT_ENTRIES] = [IdtEntry::missing(); IDT_ENTRIES];

/// Operand for `lidt`: a 16-bit limit followed by a 64-bit base. Same shape
/// as the GDT pointer, and packed for the same reason.
///
/// Reference: Intel SDM Vol. 2A, "LGDT/LIDT".
#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Build the IDT and load it.
///
/// # Safety
///
/// [`gdt::init`] must have run first: every gate names
/// [`KERNEL_CODE_SELECTOR`], and the double-fault gate relies on the TSS
/// being loaded for its IST entry to mean anything. Call exactly once, with
/// interrupts disabled.
pub unsafe fn init() {
    // SAFETY: Single-threaded boot path with interrupts disabled, called
    // once. No exception can be delivered through the IDT until `lidt`
    // executes at the end, so these writes cannot race the CPU.
    unsafe {
        for vector in 0..EXCEPTION_COUNT {
            // Route the double fault to its own stack. Every other
            // exception uses whatever stack is current.
            let ist = if vector == 8 {
                gdt::DOUBLE_FAULT_IST_INDEX
            } else {
                0
            };

            let handler = isr_table[vector] as usize as u64;
            IDT[vector] = IdtEntry::interrupt_gate(handler, ist);
        }

        // M6's two external vectors. Both use the current stack: an
        // interrupt arriving on a thread's kernel stack is handled
        // there, which is what makes the handler's view of "which
        // thread was interrupted" simply "the one whose stack I am on".
        //
        // No IST for the timer, deliberately. An IST entry switches
        // stacks *unconditionally*, so a timer interrupt arriving
        // inside another timer interrupt would re-use the same stack
        // and overwrite the outer frame. Interrupt gates already
        // prevent that by clearing IF, but relying on two mechanisms
        // where one suffices invites the day they disagree.
        IDT[TIMER_VECTOR as usize] =
            IdtEntry::interrupt_gate(isr_table[TIMER_STUB_INDEX] as usize as u64, 0);
        IDT[SPURIOUS_VECTOR as usize] =
            IdtEntry::interrupt_gate(isr_table[SPURIOUS_STUB_INDEX] as usize as u64, 0);

        // M10's interrupt lines, one gate each.
        //
        // Every line gets a gate whether or not anybody owns it, and
        // that is a decision rather than laziness. An unowned line
        // with no gate would raise #GP if its interrupt ever arrived,
        // and #GP during interrupt delivery is a double fault -- the
        // kernel would die because of something outside it, which
        // ADR 030 refuses. With a gate, the same interrupt reaches
        // `handle_interrupt`, finds no owner, and is counted and
        // dropped: ADR 042's *defined* behaviour.
        //
        // DPL 0, like every other gate: a user thread must not be
        // able to forge a device interrupt with `int 38`. The way to
        // signal a driver from user space is `Signal` on its
        // Notification, which requires holding a capability for it.
        for irq in 0..IRQ_VECTOR_COUNT {
            let vector = irq_to_vector(irq as u8) as usize;
            IDT[vector] =
                IdtEntry::interrupt_gate(isr_table[IRQ_STUB_BASE + irq] as usize as u64, 0);
        }

        let pointer = DescriptorTablePointer {
            limit: (size_of::<[IdtEntry; IDT_ENTRIES]>() - 1) as u16,
            base: &raw const IDT as u64,
        };

        load_idt(&pointer);
    }
}

/// Execute `lidt`.
///
/// # Safety
///
/// `pointer` must describe a well-formed IDT that stays valid for as long as
/// the CPU might consult it.
unsafe fn load_idt(pointer: &DescriptorTablePointer) {
    // SAFETY: The caller guarantees the table is well-formed and lives in a
    // static, so it outlives any possible use. `lidt` only loads IDTR; it
    // validates nothing, so the instruction itself cannot fault.
    unsafe {
        asm!("lidt [{}]", in(reg) pointer, options(readonly, nostack, preserves_flags));
    }
}
