// SPDX-License-Identifier: BSD-2-Clause
//! Raw x86 port I/O and CPU halt primitives.
//!
//! These are the only places in the kernel that emit `in`/`out` instructions.
//! Everything above this module works in terms of named devices.

use core::arch::asm;

/// Write a byte to an I/O port.
///
/// # Safety
///
/// Port I/O is arbitrary hardware access: the caller must know that writing
/// `value` to `port` is meaningful and cannot corrupt device or system state.
/// There is no way for the type system to check this.
///
/// Reference: Intel SDM Vol. 2A, "OUT - Output to Port" (the `OUT imm8, AL`
/// and `OUT DX, AL` forms). The `"Nd"` constraint lets LLVM pick the
/// immediate encoding for ports < 0x100 and `DX` otherwise.
#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    // SAFETY: The caller guarantees that `port` is a valid byte-wide output
    // port and that writing `value` is intended. The asm block itself only
    // reads the two operands and performs the write; it clobbers no
    // registers and preserves flags. `nomem` is NOT used because a port
    // write can be observed by a device, and we must not let the compiler
    // reorder it across other volatile accesses.
    unsafe {
        asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nostack, preserves_flags),
        );
    }
}

/// Read a byte from an I/O port.
///
/// # Safety
///
/// Reading a port can have side effects on real hardware (for example, the
/// 16550A receive buffer pops a byte off the FIFO). The caller must know that
/// reading `port` is intended at this point.
///
/// Reference: Intel SDM Vol. 2A, "IN - Input from Port".
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: The caller guarantees `port` is a valid byte-wide input port
    // and that the read's side effects are intended. The asm writes only to
    // the `al` output operand.
    unsafe {
        asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nostack, preserves_flags),
        );
    }
    value
}

/// Write a doubleword to an I/O port.
///
/// # Safety
///
/// Same contract as [`outb`], for a 4-byte-wide port. Writing a dword to a
/// port that is not 4 bytes wide has implementation-defined behaviour.
///
/// Reference: Intel SDM Vol. 2A, "OUT - Output to Port" (`OUT DX, EAX`).
#[inline]
pub unsafe fn outl(port: u16, value: u32) {
    // SAFETY: The caller guarantees that `port` is a valid dword-wide output
    // port. The asm block performs exactly one write and touches nothing else.
    unsafe {
        asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") value,
            options(nostack, preserves_flags),
        );
    }
}

/// Mask interrupts and halt the CPU forever.
///
/// Used as the final fallback when there is nothing left to do: after a panic
/// has been reported, or when the QEMU debug-exit device is absent so the
/// kernel cannot terminate the machine.
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: `cli` clears IF so no interrupt can wake us into an
        // undefined state; `hlt` then parks the CPU until the next external
        // event, which cannot be delivered because IF is clear. Executing
        // both requires CPL 0, which holds: the kernel never leaves ring 0 in
        // this milestone. Neither instruction touches memory or registers.
        //
        // Reference: Intel SDM Vol. 2A, "CLI - Clear Interrupt Flag" and
        // "HLT - Halt". `hlt` is re-executed in a loop because an NMI or SMI
        // can resume execution even with IF clear (SDM Vol. 3A, 6.3.1).
        unsafe {
            asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
