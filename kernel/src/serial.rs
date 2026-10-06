// SPDX-License-Identifier: BSD-2-Clause
//! 16550A UART driver for COM1, used as the kernel's log sink.
//!
//! Polled I/O only: no interrupts, no buffering, no allocation. Every write
//! spins on the transmit-holding-register-empty bit with a bounded loop, so a
//! missing or wedged UART degrades to dropped output rather than a hang.
//!
//! Register and bit definitions come from the National Semiconductor PC16550D
//! datasheet (the de-facto 16550A reference) and the OSDev wiki "Serial Ports"
//! page <https://wiki.osdev.org/Serial_Ports>. Individual citations are on the
//! constants below.

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::io::{inb, outb};

/// Base I/O port of COM1.
///
/// 0x3F8 is the first of the four conventional PC serial port bases
/// (0x3F8/0x2F8/0x3E8/0x2E8). This assignment is an IBM PC platform
/// convention rather than a property of the UART itself; QEMU's default
/// `-serial` device is wired here.
///
/// Reference: OSDev wiki, "Serial Ports" — Port Addresses table.
/// <https://wiki.osdev.org/Serial_Ports#Port_Addresses>
const COM1_BASE: u16 = 0x3F8;

// ---- Register offsets from the UART base -------------------------------
//
// The 16550A exposes eight registers. Which register a given offset selects
// depends on the Divisor Latch Access Bit (DLAB) in the Line Control
// Register for offsets 0 and 1.
//
// Reference: PC16550D datasheet, Table 2 "UART Reset Configuration" and the
// register summary; OSDev wiki "Serial Ports" — Port Offsets table.

/// DLAB=0: Receiver Buffer (read) / Transmitter Holding Register (write).
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

// ---- Line Control Register bits ----------------------------------------

/// LCR bit 7: Divisor Latch Access Bit. When set, offsets 0 and 1 address the
/// baud rate divisor latches instead of the data and interrupt-enable
/// registers.
///
/// Reference: PC16550D datasheet, "Line Control Register", bit 7.
const LCR_DLAB: u8 = 1 << 7;

/// LCR bits 1:0 = 11 selects 8 data bits; bit 2 = 0 selects 1 stop bit;
/// bit 3 = 0 disables parity. Together: 8N1.
///
/// Reference: PC16550D datasheet, "Line Control Register", Table 4
/// (word length select) and bits 2-3.
const LCR_8N1: u8 = 0b0000_0011;

// ---- FIFO Control Register bits ----------------------------------------

/// FCR bit 0: enable both FIFOs.
///
/// Reference: PC16550D datasheet, "FIFO Control Register", bit 0.
const FCR_ENABLE: u8 = 1 << 0;
/// FCR bit 1: clear the receive FIFO. Self-clearing.
const FCR_CLEAR_RX: u8 = 1 << 1;
/// FCR bit 2: clear the transmit FIFO. Self-clearing.
const FCR_CLEAR_TX: u8 = 1 << 2;
/// FCR bits 7:6 = 11 sets the receiver trigger level to 14 bytes. Irrelevant
/// while we never enable receive interrupts, but it leaves the FIFO in a
/// defined state.
///
/// Reference: PC16550D datasheet, "FIFO Control Register", Table 3.
const FCR_TRIGGER_14: u8 = 0b1100_0000;

// ---- Modem Control Register bits ---------------------------------------

/// MCR bit 0: Data Terminal Ready.
///
/// Reference: PC16550D datasheet, "Modem Control Register", bit 0.
const MCR_DTR: u8 = 1 << 0;
/// MCR bit 1: Request To Send.
const MCR_RTS: u8 = 1 << 1;
/// MCR bit 2: OUT1. Unused on most PCs; driven during the loopback test only
/// so that the test exercises a known MCR value.
const MCR_OUT1: u8 = 1 << 2;
/// MCR bit 3: OUT2. On the IBM PC this gates the UART's interrupt line onto
/// the bus, so it must be set for IRQs to reach the PIC. We keep it set even
/// though M0 uses no interrupts, so that later milestones inherit a working
/// configuration.
///
/// Reference: OSDev wiki, "Serial Ports" — Modem Control Register, OUT2 note.
const MCR_OUT2: u8 = 1 << 3;
/// MCR bit 4: LOOP. Internally connects the transmitter to the receiver so
/// the UART can be tested without external wiring.
///
/// Reference: PC16550D datasheet, "Modem Control Register", bit 4 (Loop).
const MCR_LOOPBACK: u8 = 1 << 4;

// ---- Line Status Register bits -----------------------------------------

/// LSR bit 5: Transmitter Holding Register Empty. Set when the UART is ready
/// to accept another byte for transmission.
///
/// Reference: PC16550D datasheet, "Line Status Register", bit 5 (THRE).
const LSR_THR_EMPTY: u8 = 1 << 5;

// ---- Baud rate ---------------------------------------------------------

/// The UART is clocked at 1.8432 MHz on the PC platform and divides that by
/// 16 to produce the bit clock, so:
///
/// ```text
/// divisor = 1_843_200 / (16 * baud) = 115_200 / baud
/// ```
///
/// For 115200 baud the divisor is 1, the fastest rate the chip supports.
///
/// Reference: PC16550D datasheet, "Programmable Baud Generator" and Table 5
/// (baud rates with a 1.8432 MHz crystal); OSDev wiki "Serial Ports" —
/// Setting the Baud Rate.
const BAUD_DIVISOR: u16 = 1;

/// Upper bound on the THRE poll loop.
///
/// At 115200 baud a character takes ~87 µs to shift out, so a healthy UART
/// sets THRE within a few thousand iterations of this loop. The limit exists
/// purely so that an absent or wedged UART cannot hang the kernel: no kernel
/// path may contain an unbounded loop.
const TX_SPIN_LIMIT: u32 = 100_000;

/// Set once [`init`] has passed the loopback self-test.
///
/// `AtomicBool` rather than `static mut`: this is observable from the panic
/// handler, which can run at any point, so the access must be sound without
/// relying on there being a single thread.
static READY: AtomicBool = AtomicBool::new(false);

/// Initialise COM1 at 115200 8N1 and run a loopback self-test.
///
/// Returns `false` if the self-test fails, which means no UART is present at
/// [`COM1_BASE`]. Logging remains safe in that case: writes to a missing port
/// are absorbed by the chipset and the bounded spin keeps us from hanging.
pub fn init() -> bool {
    // SAFETY: Every port in this block is a 16550A register at the
    // conventional COM1 base, and the write sequence is the standard
    // initialisation order from the PC16550D datasheet: mask interrupts,
    // set DLAB and program the divisor, clear DLAB and set the line format,
    // then reset the FIFOs. No other code touches these ports during boot
    // (we are single-threaded here, before any other CPU is started), so
    // there is no interleaving that could leave DLAB set.
    unsafe {
        // Mask all UART interrupt sources; this driver is strictly polled.
        // Reference: PC16550D datasheet, "Interrupt Enable Register" —
        // writing 0 disables all four interrupt types.
        outb(COM1_BASE + REG_IER, 0x00);

        // Program the baud rate divisor behind DLAB.
        outb(COM1_BASE + REG_LCR, LCR_DLAB);
        outb(COM1_BASE + REG_DIVISOR_LO, (BAUD_DIVISOR & 0xFF) as u8);
        outb(COM1_BASE + REG_DIVISOR_HI, (BAUD_DIVISOR >> 8) as u8);

        // Clear DLAB and select 8N1 in the same write.
        outb(COM1_BASE + REG_LCR, LCR_8N1);

        outb(
            COM1_BASE + REG_FCR,
            FCR_ENABLE | FCR_CLEAR_RX | FCR_CLEAR_TX | FCR_TRIGGER_14,
        );

        // Loopback self-test: with LOOP set, the transmitter is tied to the
        // receiver internally, so a byte written to THR must read back from
        // RBR. 0xAE is an arbitrary pattern with alternating bits, chosen so
        // that a stuck-at-0 or stuck-at-1 data bus fails the test.
        outb(
            COM1_BASE + REG_MCR,
            MCR_LOOPBACK | MCR_RTS | MCR_OUT1 | MCR_OUT2,
        );
        outb(COM1_BASE + REG_DATA, 0xAE);
        let echoed = inb(COM1_BASE + REG_DATA);

        // Leave the UART in normal (non-loopback) operation either way, so a
        // failed self-test does not strand the chip in a strange mode.
        outb(COM1_BASE + REG_MCR, MCR_DTR | MCR_RTS | MCR_OUT2);

        if echoed != 0xAE {
            return false;
        }
    }

    READY.store(true, Ordering::Release);
    true
}

/// Whether [`init`] completed its self-test successfully.
pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Write one byte to the UART, spinning (boundedly) for room in the FIFO.
fn put_byte(byte: u8) {
    for _ in 0..TX_SPIN_LIMIT {
        // SAFETY: Reading the Line Status Register has no side effects that
        // matter here. LSR is at a fixed offset from the COM1 base and is
        // always readable, whether or not a UART is actually present (an
        // absent device reads as 0xFF, which has THRE set, so we fall
        // through to the write immediately instead of spinning).
        let status = unsafe { inb(COM1_BASE + REG_LSR) };
        if status & LSR_THR_EMPTY != 0 {
            break;
        }
    }

    // SAFETY: Writing the Transmitter Holding Register queues one byte for
    // transmission. If the loop above timed out we write anyway and accept
    // that the byte may be dropped -- losing log output is strictly better
    // than hanging the kernel, which is the only alternative.
    unsafe {
        outb(COM1_BASE + REG_DATA, byte);
    }
}

/// Write one character, translating `\n` to CRLF for terminal consumers.
pub fn put_char(c: u8) {
    if c == b'\n' {
        put_byte(b'\r');
    }
    put_byte(c);
}

/// Write a string to the UART.
pub fn write_str(s: &str) {
    for byte in s.bytes() {
        put_char(byte);
    }
}

/// Adapter so that [`core::write!`] can target the serial port.
///
/// Carries no state: the UART is addressed by fixed port number, so every
/// instance refers to the same device.
pub struct Serial;

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str(s);
        Ok(())
    }
}

/// Print to COM1. No newline is appended.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use ::core::fmt::Write as _;
        // Writing to the serial port is infallible by construction (see
        // `Serial::write_str`), so the Result cannot be an error.
        let _ = ::core::write!(&mut $crate::serial::Serial, $($arg)*);
    }};
}

/// Print a line to COM1.
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        $crate::print!($($arg)*);
        $crate::print!("\n");
    }};
}
