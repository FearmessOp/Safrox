// SPDX-License-Identifier: BSD-2-Clause
//! The emergency console (ADR 046).
//!
//! **What is left of the kernel's serial driver, and the measure of
//! what left.** Until M13 this file was `serial.rs`: a 16550A driver
//! that programmed the baud divisor, the line format, the FIFOs and
//! the modem control register, and owned COM1 outright. The driver is
//! now a user component (ADR 045, ADR 047) and this is the remainder.
//!
//! # Driving a device and writing to a port are different things
//!
//! The distinction is the whole of ADR 046's argument, and it is
//! worth stating in one place because "did the driver really leave
//! the kernel" is otherwise unanswerable.
//!
//! **Driving** a UART means owning its configuration, fielding its
//! interrupts, managing its state and arbitrating between clients. A
//! driver sets the baud rate, flips DLAB, enables the FIFO, writes
//! MCR, takes IRQ 4 and buffers what arrives.
//!
//! **Writing a byte to a port** is none of that. It touches two
//! registers: read LSR to ask whether there is room, write THR. It
//! configures nothing, expects no interrupt, and keeps no state.
//!
//! The difference is countable — eight registers against two — and
//! the count is what `driver-test` reports.
//!
//! # Three states, and only two of them configure
//!
//! * **Boot.** Nothing exists yet; no component can be asked to
//!   print, because components are what the root task has not built
//!   yet. [`init`] programs the UART once: 115200 8N1, FIFOs on.
//!
//! * **Running.** The driver owns the device. Everything here reads
//!   LSR and writes THR and **touches nothing else** — not LCR, not
//!   IER, not FCR, not MCR, not the divisor. That is not politeness:
//!   if the kernel re-programmed the baud rate under a component that
//!   had configured it, the component's ownership would be a fiction.
//!
//! * **Panic.** The system is over. [`reconfigure_for_panic`] puts
//!   the UART back into a state where a byte can get out, because the
//!   most likely reason we are here is that the driver died — and if
//!   it died with DLAB set, every subsequent write to THR goes to the
//!   baud divisor and **not one character appears**. Losing the panic
//!   message at the exact moment it is needed is the failure this
//!   guards against, and it is why the "never configure" rule above
//!   has a written exception rather than none.
//!
//! Register and bit definitions come from the National Semiconductor
//! PC16550D datasheet and the OSDev wiki "Serial Ports" page
//! <https://wiki.osdev.org/Serial_Ports>.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::io::{inb, outb};

/// Base I/O port of COM1.
///
/// The same 0x3F8 the user-space driver asks for, and that is not a
/// duplication to be removed: the driver reaches it through an
/// `IOPort` capability with a range check (ADR 045), and the kernel
/// reaches it directly because at the two moments this code runs
/// there is nobody to ask. Two paths to one device, with different
/// authority and different lifetimes.
const COM1_BASE: u16 = 0x3F8;

// ---- Register offsets --------------------------------------------------
//
// Which register an offset selects depends on DLAB in the Line
// Control Register, for offsets 0 and 1. That dependency is the
// reason `reconfigure_for_panic` exists.

/// DLAB=0: Receiver Buffer (read) / Transmitter Holding Register.
const REG_DATA: u16 = 0;
/// DLAB=0: Interrupt Enable Register.
const REG_IER: u16 = 1;
/// DLAB=1: Divisor Latch, low byte.
const REG_DIVISOR_LO: u16 = 0;
/// DLAB=1: Divisor Latch, high byte.
const REG_DIVISOR_HI: u16 = 1;
/// Write: FIFO Control Register.
const REG_FCR: u16 = 2;
/// Line Control Register.
const REG_LCR: u16 = 3;
/// Modem Control Register.
const REG_MCR: u16 = 4;
/// Line Status Register.
const REG_LSR: u16 = 5;

/// LCR bit 7: Divisor Latch Access Bit.
///
/// Reference: PC16550D datasheet, "Line Control Register", bit 7.
const LCR_DLAB: u8 = 1 << 7;

/// LCR bits 1:0 = 11 (8 data bits), bit 2 = 0 (1 stop), bit 3 = 0
/// (no parity). Together: 8N1.
const LCR_8N1: u8 = 0b0000_0011;

/// FCR bit 0: enable both FIFOs.
const FCR_ENABLE: u8 = 1 << 0;
/// FCR bit 1: clear the receive FIFO. Self-clearing.
const FCR_CLEAR_RX: u8 = 1 << 1;
/// FCR bit 2: clear the transmit FIFO. Self-clearing.
const FCR_CLEAR_TX: u8 = 1 << 2;
/// FCR bits 7:6 = 11: receiver trigger level 14 bytes.
const FCR_TRIGGER_14: u8 = 0b1100_0000;

/// MCR bit 0: Data Terminal Ready.
const MCR_DTR: u8 = 1 << 0;
/// MCR bit 1: Request To Send.
const MCR_RTS: u8 = 1 << 1;
/// MCR bit 2: OUT1. Driven during the loopback test only.
const MCR_OUT1: u8 = 1 << 2;
/// MCR bit 3: OUT2. On the IBM PC this gates the UART's interrupt
/// line onto the bus.
const MCR_OUT2: u8 = 1 << 3;
/// MCR bit 4: LOOP. Ties the transmitter to the receiver internally.
const MCR_LOOPBACK: u8 = 1 << 4;

/// LSR bit 5: Transmitter Holding Register Empty.
const LSR_THR_EMPTY: u8 = 1 << 5;

/// 115200 baud: divisor 1, the fastest the chip supports.
///
/// `divisor = 1_843_200 / (16 * baud) = 115_200 / baud`.
const BAUD_DIVISOR: u16 = 1;

/// Upper bound on the THRE poll loop.
///
/// A character takes ~87 µs at 115200 baud, so a healthy UART sets
/// THRE within a few thousand iterations. The limit exists purely so
/// that an absent or wedged UART cannot hang the kernel: no kernel
/// path may contain an unbounded loop.
const TX_SPIN_LIMIT: u32 = 100_000;

/// Set once [`init`] has passed the loopback self-test.
static READY: AtomicBool = AtomicBool::new(false);

/// Bytes written here, for the report.
///
/// **What makes the "kernel drives nothing" claim checkable rather
/// than asserted.** It counts every byte the kernel put on the wire
/// itself — and the figure a reader should care about is not this one
/// but how it compares to the driver's, which is why `driver-test`
/// prints both.
static BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// Writes to a register other than LSR or THR.
///
/// **Zero while a driver owns the device, and the test asserts it.**
/// Incremented by [`init`] (six, at boot, before any component
/// exists) and by [`reconfigure_for_panic`]. A non-zero delta during
/// normal running would mean the kernel reached past the two
/// registers ADR 046 allows it — which is exactly the claim the
/// counter exists to falsify.
static CONFIG_WRITES: AtomicU64 = AtomicU64::new(0);

pub fn bytes_written() -> u64 {
    BYTES_WRITTEN.load(Ordering::Acquire)
}

pub fn config_writes() -> u64 {
    CONFIG_WRITES.load(Ordering::Acquire)
}

/// Initialise COM1 at 115200 8N1 and run a loopback self-test.
///
/// Returns `false` if the self-test fails, which means no UART is
/// present. Logging stays safe in that case: writes to a missing port
/// are absorbed by the chipset and the bounded spin keeps us from
/// hanging.
///
/// **Called once, at boot, before any component exists.** A system
/// that could ask a driver to print its first line would not need
/// this; there is no such moment.
pub fn init() -> bool {
    // SAFETY: Every port here is a 16550A register at the conventional
    // COM1 base, written in the datasheet's initialisation order: mask
    // interrupts, set DLAB and program the divisor, clear DLAB and set
    // the line format, reset the FIFOs. We are single-threaded, before
    // any other CPU is started and before any component exists, so
    // there is no interleaving that could leave DLAB set.
    unsafe {
        outb(COM1_BASE + REG_IER, 0x00);

        outb(COM1_BASE + REG_LCR, LCR_DLAB);
        outb(COM1_BASE + REG_DIVISOR_LO, (BAUD_DIVISOR & 0xFF) as u8);
        outb(COM1_BASE + REG_DIVISOR_HI, (BAUD_DIVISOR >> 8) as u8);

        outb(COM1_BASE + REG_LCR, LCR_8N1);

        outb(
            COM1_BASE + REG_FCR,
            FCR_ENABLE | FCR_CLEAR_RX | FCR_CLEAR_TX | FCR_TRIGGER_14,
        );

        // Loopback self-test: with LOOP set the transmitter is tied to
        // the receiver, so a byte written to THR must read back. 0xAE
        // has alternating bits, so a stuck-at-0 or stuck-at-1 bus
        // fails.
        outb(
            COM1_BASE + REG_MCR,
            MCR_LOOPBACK | MCR_RTS | MCR_OUT1 | MCR_OUT2,
        );
        outb(COM1_BASE + REG_DATA, 0xAE);
        let echoed = inb(COM1_BASE + REG_DATA);

        // Leave the UART in normal operation either way, so a failed
        // self-test does not strand the chip in a strange mode.
        outb(COM1_BASE + REG_MCR, MCR_DTR | MCR_RTS | MCR_OUT2);

        if echoed != 0xAE {
            return false;
        }
    }

    // Six configuration writes, counted. The figure is in the report
    // so that "the kernel configured the UART once, at boot" is a
    // number rather than a promise.
    CONFIG_WRITES.fetch_add(6, Ordering::AcqRel);
    READY.store(true, Ordering::Release);
    true
}

/// Whether [`init`] completed its self-test.
pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Put the UART back into a state where a byte can get out.
///
/// **Panic path only** (ADR 046, state 3), and the one place the
/// "never configure while a driver owns it" rule is deliberately
/// broken.
///
/// The hazard is specific. The user-space driver sets DLAB to program
/// the baud divisor; if it dies in that window — or if its death is
/// what we are panicking about — the UART is left in divisor-latch
/// mode, and every byte written to offset 0 goes to the divisor
/// instead of the transmitter. Not one character of the panic report
/// would appear.
///
/// So this clears DLAB, restores 8N1, and resets the FIFOs. It does
/// **not** touch the divisor: whatever rate the driver chose is the
/// rate the host's terminal is listening at, and "fixing" it would
/// turn readable output into line noise.
///
/// Safe to run when no driver ever started, because every write is
/// idempotent and the state it imposes is the state [`init`] left.
fn reconfigure_for_panic() {
    // SAFETY: The system has panicked and is about to stop. Interrupts
    // are already masked by the panic handler, no component can be
    // scheduled again, and these are the same 16550A registers `init`
    // writes. The device may be in any state, which is the reason for
    // writing them.
    unsafe {
        // DLAB clear and 8N1 in one write. This is the load-bearing
        // one: without it a driver that died mid-configuration takes
        // the panic message with it.
        outb(COM1_BASE + REG_LCR, LCR_8N1);

        // No interrupts: the driver that would have handled them is
        // not going to run again.
        outb(COM1_BASE + REG_IER, 0x00);

        // Drop whatever the driver had queued. Those bytes belong to a
        // conversation that is over, and interleaving them with the
        // panic report would corrupt the one message that matters.
        outb(
            COM1_BASE + REG_FCR,
            FCR_ENABLE | FCR_CLEAR_RX | FCR_CLEAR_TX | FCR_TRIGGER_14,
        );

        // DTR/RTS asserted, OUT2 on, loopback off. A driver that died
        // during its own self-test could have left LOOP set, which
        // would send the panic report straight back into the receive
        // FIFO and nowhere else.
        outb(COM1_BASE + REG_MCR, MCR_DTR | MCR_RTS | MCR_OUT2);
    }

    CONFIG_WRITES.fetch_add(4, Ordering::AcqRel);
}

/// Prepare the console for a panic report.
///
/// Called by the panic handler before anything is printed.
pub fn enter_panic() {
    reconfigure_for_panic();
}

/// Write one byte, spinning (boundedly) for room.
///
/// **Two registers, and that is the whole of what the kernel does to
/// this device while the system is running.** LSR to ask, THR to
/// write.
fn put_byte(byte: u8) {
    for _ in 0..TX_SPIN_LIMIT {
        // SAFETY: Reading the Line Status Register has no side effect
        // that matters here. LSR is at a fixed offset from the COM1
        // base and is always readable, whether or not a UART is
        // present — an absent device reads as 0xFF, which has THRE
        // set, so we fall through to the write rather than spinning.
        let status = unsafe { inb(COM1_BASE + REG_LSR) };
        if status & LSR_THR_EMPTY != 0 {
            break;
        }
    }

    // SAFETY: Writing the Transmitter Holding Register queues one byte.
    // If the loop above timed out we write anyway and accept that the
    // byte may be dropped — losing log output is strictly better than
    // hanging the kernel, which is the only alternative.
    unsafe {
        outb(COM1_BASE + REG_DATA, byte);
    }
    BYTES_WRITTEN.fetch_add(1, Ordering::AcqRel);
}

/// Write one character, translating `\n` to CRLF.
pub fn put_char(c: u8) {
    if c == b'\n' {
        put_byte(b'\r');
    }
    put_byte(c);
}

/// Write a string.
pub fn write_str(s: &str) {
    for byte in s.bytes() {
        put_char(byte);
    }
}

/// Adapter so that [`core::write!`] can target the console.
///
/// Carries no state: the UART is addressed by fixed port number, so
/// every instance refers to the same device.
pub struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str(s);
        Ok(())
    }
}

/// Print to the emergency console. No newline is appended.
///
/// The macro keeps its name. Every one of the kernel's log lines and
/// every acceptance test's expectation goes through it, and renaming
/// it would have been a change to 59 scenarios in exchange for
/// nothing — the point of M13 is what the kernel stopped *doing*, not
/// what its printing is called.
///
/// # Why a preempt guard
///
/// **A line is an output unit, and it has to leave the kernel
/// whole.** A long message is thousands of `outb`s; from M6 a timer
/// interrupt can land in the middle of them and switch threads. The
/// other thread then prints its own line *into* the first.
///
/// Observed in M14's suite run, in the log itself:
///
/// ```text
/// ...when they stopped existing[ipc] notification at 0x...4418000
///  so did it.
/// ```
///
/// Two complete messages, interleaved, and **neither one is a line
/// any more**. Every acceptance expectation is anchored with `^` and
/// `$`, so both vanish from the runner's point of view — which is
/// how one suite run reported `vsuserro` missing a line that was
/// plainly in its log, and another reported the same of
/// `driverdies`. Different scenarios each time, because the victim
/// is whoever happens to be printing when the tick arrives.
///
/// This was a latent defect from M6, not from M14. It needed a long
/// line and a busy moment, and M14 supplied both: a third component
/// to schedule and several multi-line reports.
///
/// **`PreemptGuard`, not `InterruptGuard`**, and the distinction is
/// ADR 025's. What corrupts a line is a *thread switch*, not an
/// interrupt; an interrupt that merely counts a tick and returns
/// does no harm. Disabling interrupts for the duration of a
/// thousand-character message would lose device interrupts for
/// milliseconds, which ADR 019 refused for revoke and which is no
/// more acceptable here.
///
/// The guard costs one atomic increment and one decrement per
/// `print!`.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use ::core::fmt::Write as _;
        // Held for the whole write, so a tick in the middle defers
        // its switch until the line is out.
        let _preempt = $crate::sched::PreemptGuard::new();
        // Writing is infallible by construction (see
        // `Console::write_str`), so the Result cannot be an error.
        let _ = ::core::write!(&mut $crate::emergency::Console, $($arg)*);
    }};
}

/// Print a line to the emergency console.
///
/// **One guard, not two.** The body is a single `print!` with the
/// newline formatted in rather than two calls, because two calls
/// would release the guard between them and leave the gap this
/// exists to close — a switch after the message and before the `\n`
/// produces exactly the same interleaving.
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        $crate::print!("{}\n", ::core::format_args!($($arg)*));
    }};
}

/// What the kernel still touches, and what it has stopped touching.
///
/// Printed by `driver-test`. The figures are the evidence for
/// criterion 4: a configuration count that stays at its boot value
/// while a driver is running is the difference between "the kernel
/// does not configure the UART" and a sentence in an ADR.
pub fn report() {
    println!(
        "[emergency] COM1 console: {} byte(s) written, {} configuration \
         write(s) (6 at boot, +4 only on the panic path -- ADR 046)",
        bytes_written(),
        config_writes()
    );
    println!(
        "[emergency] while a driver owns the device the kernel touches \
         LSR and THR and nothing else: no baud rate, no DLAB, no FIFO, \
         no MCR, no IRQ"
    );
}
