// SPDX-License-Identifier: BSD-2-Clause
//! The kernel panic handler.
//!
//! A panic is unrecoverable: we report it on the serial console with its
//! source location and then terminate the VM with a status the host runner
//! decodes as a failure. There is no unwinding (`panic = "abort"` in
//! Cargo.toml), so this function never returns.

use core::arch::asm;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::serial;
use crate::{print, println, qemu};

/// Guards against panicking while handling a panic.
///
/// If formatting the message itself faults, re-entering the handler would
/// recurse until the stack is exhausted and the machine triple-faults,
/// destroying the evidence. On re-entry we skip all formatting and exit
/// immediately, so the VM still terminates with a deterministic status.
static PANICKING: AtomicBool = AtomicBool::new(false);

#[panic_handler]
fn on_panic(info: &PanicInfo<'_>) -> ! {
    // Nothing may preempt us from here on: an interrupt handler that touched
    // the serial port would interleave its output with the panic report.
    //
    // SAFETY: `cli` merely clears the interrupt flag. It requires CPL 0,
    // which holds -- the kernel runs exclusively in ring 0 in this
    // milestone. Masking interrupts can only delay work, never corrupt
    // state, and we never re-enable them because this function diverges.
    //
    // Reference: Intel SDM Vol. 2A, "CLI - Clear Interrupt Flag".
    unsafe {
        asm!("cli", options(nomem, nostack, preserves_flags));
    }

    // `swap` makes the check-and-set atomic, so a panic on another CPU
    // cannot slip past the guard between the load and the store.
    if PANICKING.swap(true, Ordering::AcqRel) {
        serial::write_str("\n[PANIC] recursive panic; aborting\n");
        qemu::exit(qemu::EXIT_FAILURE);
    }

    println!();
    println!("*** KERNEL PANIC ***");

    match info.location() {
        Some(location) => {
            println!(
                "[PANIC] at {}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            );
        }
        None => {
            // `location()` only returns None for panics raised outside any
            // tracked source location, which should not happen for us.
            println!("[PANIC] at <unknown location>");
        }
    }

    print!("[PANIC] ");
    println!("{}", info.message());

    println!(
        "[PANIC] halting; signalling QEMU (write {:#04x} -> exit status {})",
        qemu::EXIT_FAILURE,
        qemu::HOST_STATUS_FAILURE
    );

    qemu::exit(qemu::EXIT_FAILURE)
}
