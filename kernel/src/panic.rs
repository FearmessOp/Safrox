// SPDX-License-Identifier: MPL-2.0
//! The kernel panic handler.
//!
//! A panic is unrecoverable: we report it on the emergency console
//! with its source location and then terminate the VM with a status
//! the host runner decodes as a failure. There is no unwinding
//! (`panic = "abort"` in Cargo.toml), so this function never returns.
//!
//! # Why the kernel still owns a way to print (ADR 046)
//!
//! From M13 the serial *driver* is a user component. This handler is
//! the reason the kernel did not give up the port entirely, and the
//! reasoning is worth having at the point of use rather than only in
//! the ADR.
//!
//! Three options were weighed. Asking the driver is useless — the
//! driver may be dead, and its death is among the likeliest reasons
//! to be here; IPC needs a scheduler whose invariants may be exactly
//! what broke. Writing a struct to a fixed address and reading it
//! from the host is clean, blind on real hardware, and would discard
//! the serial-log checking every one of the project's scenarios is
//! built on. So the kernel keeps a writer that needs nothing from
//! anybody.
//!
//! It is **not** a driver: it owns no configuration while the system
//! runs, takes no interrupt, buffers nothing. See
//! [`crate::emergency`] for the distinction and for what it costs.

use core::arch::asm;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::emergency;
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
        emergency::write_str("\n[PANIC] recursive panic; aborting\n");
        qemu::exit(qemu::EXIT_FAILURE);
    }

    // **Put the UART back into a usable state before saying anything**
    // (ADR 046, state 3). This is the one moment the kernel overrides
    // a component's ownership of a device, and the hazard is concrete:
    // the user-space driver sets DLAB to program the baud divisor, so
    // a driver that died in that window leaves the chip in
    // divisor-latch mode — where every byte written to the data port
    // goes to the divisor and **not one character of this report
    // appears**.
    //
    // After the first `println!` it would be too late: the lines that
    // name the panic's location would be the ones lost.
    //
    // Only on this path. While the system is healthy the kernel does
    // not touch a configuration register, because a device whose
    // settings the kernel may change at any time does not really
    // belong to the component that configured it.
    emergency::enter_panic();

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
