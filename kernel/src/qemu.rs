// SPDX-License-Identifier: BSD-2-Clause
//! QEMU `isa-debug-exit` integration: how the kernel terminates the VM with a
//! status the host test runner can decode.

use crate::io::{halt_forever, outl};

/// I/O base of the `isa-debug-exit` device, matching the QEMU command line:
///
/// ```text
/// -device isa-debug-exit,iobase=0xf4,iosize=0x04
/// ```
///
/// 0xf4 is not a fixed property of the device -- it is the value we pass on
/// the command line, chosen because it sits in the unassigned 0xf0-0xff
/// region of the legacy ISA I/O map and so collides with no real hardware.
/// The default in QEMU's source is 0x501.
///
/// Reference: QEMU `hw/misc/debugexit.c`, which defines the device's
/// `iobase`/`iosize` properties (defaults 0x501 / 2).
const DEBUG_EXIT_PORT: u16 = 0xF4;

/// Value the kernel writes on success.
pub const EXIT_SUCCESS: u32 = 0x10;

/// Value the kernel writes when it panics.
pub const EXIT_FAILURE: u32 = 0x11;

/// Host process exit status produced by writing `value` to the device.
///
/// QEMU does **not** let the guest choose its own host exit status. The
/// `isa-debug-exit` write handler calls `exit((value << 1) | 1)`, so the low
/// bit of the status is always 1 and **the guest can never produce status 0**
/// -- QEMU reserves 0 for "the VM powered off normally".
///
/// A test runner that assumes "non-zero means failure" would therefore report
/// every successful kernel run as a failure. `scripts/run.py` decodes the
/// status with this same formula instead.
///
/// Reference: QEMU `hw/misc/debugexit.c`, `debug_exit_write()`:
/// `exit((val << 1) | 1);`
pub const fn host_status(value: u32) -> u32 {
    (value << 1) | 1
}

/// Host status for a successful run: 33.
pub const HOST_STATUS_SUCCESS: u32 = host_status(EXIT_SUCCESS);

/// Host status for a panic: 35.
pub const HOST_STATUS_FAILURE: u32 = host_status(EXIT_FAILURE);

/// Terminate the virtual machine.
///
/// Never returns. If `isa-debug-exit` is absent -- for instance on real
/// hardware, or under a QEMU invocation that omits the device -- the write
/// goes nowhere and we fall through to halting the CPU forever rather than
/// continuing with a kernel that believed it had exited.
pub fn exit(value: u32) -> ! {
    // SAFETY: `DEBUG_EXIT_PORT` is the base of the isa-debug-exit device as
    // configured on our QEMU command line, declared with `iosize=0x04`, so a
    // dword write is the correct access width. On a machine where the device
    // is not present the write is absorbed by the unassigned-port behaviour
    // of the platform and has no effect; either outcome is safe, because the
    // only thing that follows is an unconditional halt.
    unsafe {
        outl(DEBUG_EXIT_PORT, value);
    }

    halt_forever()
}
