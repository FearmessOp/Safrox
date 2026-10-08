// SPDX-License-Identifier: BSD-2-Clause
#![no_std]
#![no_main]

//! The Safrox serial driver — the first device driver outside the
//! kernel (ADR 045, ADR 047).
//!
//! **This is the milestone.** Every log line from M0 to M12 was
//! written by the kernel's own 16550A driver. That code is gone; this
//! file is what replaced it, and it runs at CPL 3 with no authority
//! beyond three capabilities:
//!
//! * an **IOPort** covering 0x3F8..=0x3FF, so it can reach the UART's
//!   eight registers and **nothing else** — a write to 0x70 is
//!   refused by the kernel with `PortNotInRange`;
//! * an **IRQHandler** for line 4, so it can be told when a byte
//!   arrives;
//! * an **Endpoint**, so clients can ask it for something.
//!
//! It holds no Untyped, no capability naming another component, and
//! no way to reach the root task. If it crashes, the three
//! capabilities are revoked and the system continues — which is
//! `driver-dies`, and the actual point of the architecture.
//!
//! # What it does that the kernel no longer does
//!
//! Owns the configuration: baud divisor, line format, FIFO depth,
//! modem control, interrupt enable. Eight registers. The kernel's
//! emergency console touches two (ADR 046) and configures none while
//! this is running.
//!
//! # Waiting for two things at once
//!
//! A driver must hear a client request *and* its device. M9c built
//! bound notifications (ADR 038) for exactly this and nothing used
//! them until now: the Notification is bound to this thread's TCB, so
//! a `Recv` on the Endpoint also returns when IRQ 4 fires. Without
//! binding the driver would have to choose which to wait for, and
//! either choice makes it deaf to the other.

#![allow(clippy::upper_case_acronyms)]

use safrox_abi as abi;
use safrox_abi::method;

// ---- Entry -------------------------------------------------------------

/// Where the root task enters, at CPL 3.
///
/// In `.text.start` so the linker puts it first: the loader copies a
/// flat image and jumps to the load address, so the entry point has
/// to *be* the first byte.
///
/// # Safety
///
/// Called by the root task with a valid stack and nothing else. Never
/// returns.
#[no_mangle]
#[link_section = ".text.start"]
pub extern "C" fn _start() -> ! {
    main()
}

/// A driver cannot print its own panic.
///
/// The obvious irony, and worth stating: the component whose job is
/// output has no way to report its own failure, because reporting it
/// would mean using the device it has just failed to drive.
///
/// So it reports through `Echo` — which names no capability and is
/// therefore always available — and then spins. The kernel's
/// emergency console is what actually gets the message out, which is
/// ADR 046's case in miniature.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    echo(CHANNEL_PANIC, 0);
    spin()
}

// ---- The syscall -------------------------------------------------------

/// Invoke a capability. ADR 030's registers, widened by ADR 043.
///
/// ```text
///   RDI  the capability          RDX  arg0     R8  arg2
///   RSI  the method              R10  arg1     R9  arg3
/// ```
fn invoke(cptr: u64, method: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let result: u64;
    // SAFETY: `syscall` is the only way out of ring 3 and the kernel
    // validates every argument — a wrong capability pointer returns
    // an error rather than reaching an object (ADR 018). RCX and R11
    // are declared clobbered because the instruction destroys them.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rdi") cptr => _,
            inlateout("rsi") method => _,
            inlateout("rdx") arg0 => _,
            inlateout("r10") arg1 => _,
            inlateout("r8") arg2 => _,
            inlateout("r9") arg3 => _,
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn echo(channel: u64, value: u64) -> u64 {
    invoke(channel, method::ECHO, value, 0, 0, 0)
}

// ---- Report channels ---------------------------------------------------
//
// Bit 31: M8 uses 8..17, M10 uses 20 and 21, M11 uses 24 and 25, the
// root task uses bits 28..30.

/// The driver's own channel.
const CHANNEL: u64 = 1 << 31;
/// Its panic channel.
const CHANNEL_PANIC: u64 = (1 << 31) | 0xFF;

const STEP_READY: u64 = 1;
const STEP_WROTE: u64 = 2;
const STEP_IRQ: u64 = 3;
const STEP_BYTE: u64 = 4;
const STEP_DENIED: u64 = 5;
const STEP_ERROR: u64 = 6;
/// A ping was answered from inside `serve`.
///
/// Exists so another component can find out that this one has
/// finished starting. See [`CMD_PING`].
const STEP_PING: u64 = 7;

// ---- The CSpace the root task builds -----------------------------------
//
// Fixed indices, baked in at both ends. A real system would pass
// these in a BootInfo of its own; this one is small enough that two
// constants in agreement are cheaper than a protocol, and the root
// task's `driver.rs` names the same numbers.

/// Slot 0 of the driver's root CNode is the leaf, which is itself.
const LEAF: u64 = 0;

fn cptr(index: u64) -> u64 {
    (LEAF << 16) | index
}

/// The Endpoint clients send to.
const SLOT_ENDPOINT: u64 = 1;
/// The IOPort covering COM1.
const SLOT_IOPORT: u64 = 2;
/// The IRQHandler for line 4.
const SLOT_IRQ: u64 = 3;
/// The Notification IRQ 4 signals.
///
/// Already bound to this thread when the driver starts — see the
/// note in `main`. The driver holds the capability so it can name
/// the Notification as its line's destination, not so it can bind
/// it.
const SLOT_NOTIFICATION: u64 = 4;

// There is deliberately no capability for the driver's own TCB.
//
// An earlier version had one, for a `Notification.Bind` that could
// never have succeeded. Removing the call removed the only use, and
// a capability a component holds for no reason is authority it
// might one day be able to misuse — a driver that can name its own
// TCB can suspend itself, and nothing here needs to.

// ---- COM1 --------------------------------------------------------------
//
// The same constants the kernel used to own. They are here now, and
// that move is the milestone: a new device means a new component, not
// a new kernel module.
//
// Reference: PC16550D datasheet; OSDev wiki "Serial Ports".

const COM1: u64 = 0x3F8;

const REG_DATA: u64 = 0;
const REG_IER: u64 = 1;
const REG_DIVISOR_LO: u64 = 0;
const REG_DIVISOR_HI: u64 = 1;
const REG_FCR: u64 = 2;
const REG_LCR: u64 = 3;
const REG_MCR: u64 = 4;
const REG_LSR: u64 = 5;

/// LCR bit 7: Divisor Latch Access Bit.
const LCR_DLAB: u64 = 1 << 7;
/// 8 data bits, 1 stop bit, no parity.
const LCR_8N1: u64 = 0b0000_0011;

const FCR_ENABLE: u64 = 1 << 0;
const FCR_CLEAR_RX: u64 = 1 << 1;
const FCR_CLEAR_TX: u64 = 1 << 2;
const FCR_TRIGGER_1: u64 = 0b0000_0000;

const MCR_DTR: u64 = 1 << 0;
const MCR_RTS: u64 = 1 << 1;
/// On the IBM PC, OUT2 gates the UART's interrupt onto the bus. With
/// it clear, IRQ 4 never reaches the I/O APIC however correctly
/// everything else is configured.
const MCR_OUT2: u64 = 1 << 3;

/// IER bit 0: interrupt when received data is available.
const IER_RX_AVAILABLE: u64 = 1 << 0;

/// LSR bit 0: data ready in the receive buffer.
const LSR_DATA_READY: u64 = 1 << 0;
/// LSR bit 5: transmitter holding register empty.
const LSR_THR_EMPTY: u64 = 1 << 5;

/// 115200 baud. `divisor = 115_200 / baud`.
const BAUD_DIVISOR: u64 = 1;

/// Bound on the transmit poll. No unbounded loop, in a driver as much
/// as in the kernel: a wedged UART must degrade to dropped output
/// rather than a thread that never yields again.
const TX_SPIN_LIMIT: u32 = 100_000;

/// Bound on the main loop.
///
/// A driver would normally run forever. This one stops, because the
/// scenario has to end and a component that never returns cannot be
/// told the test is over. Reaching the limit is reported rather than
/// silent.
const MAX_REQUESTS: u32 = 64;

// ---- Port access -------------------------------------------------------

/// Write a byte to a UART register.
///
/// **Every one of these is a syscall**, and that is ADR 045's
/// accepted cost. At 115200 baud a character takes ~87 µs on the
/// wire and a syscall round trip is on the order of 1 µs, so the
/// kernel's involvement is two orders of magnitude below the device
/// itself. The same design would be wrong for a gigabit NIC — which
/// is why MMIO devices use ADR 017's device Untyped and never come
/// through here.
fn out8(register: u64, value: u64) -> u64 {
    invoke(
        cptr(SLOT_IOPORT),
        method::IOPORT_OUT8,
        COM1 + register,
        value,
        0,
        0,
    )
}

/// Read a byte from a UART register.
fn in8(register: u64) -> u64 {
    invoke(
        cptr(SLOT_IOPORT),
        method::IOPORT_IN8,
        COM1 + register,
        0,
        0,
        0,
    )
}

// ---- The driver --------------------------------------------------------

fn main() -> ! {
    // --- 1. Configure the device -------------------------------------
    //
    // **Eight registers, and this is what "owning a device" means.**
    // The kernel's emergency console touches two of them and
    // configures none of them while this component is alive
    // (ADR 046).
    let mut worst = 0u64;

    // Interrupts off while we set up. The order matters: programming
    // the divisor needs DLAB, and an interrupt arriving with DLAB set
    // would find a UART that cannot be read.
    worst = first_error(worst, out8(REG_IER, 0));

    worst = first_error(worst, out8(REG_LCR, LCR_DLAB));
    worst = first_error(worst, out8(REG_DIVISOR_LO, BAUD_DIVISOR & 0xFF));
    worst = first_error(worst, out8(REG_DIVISOR_HI, BAUD_DIVISOR >> 8));

    // DLAB clear and 8N1 in one write. Until this lands the device is
    // in divisor-latch mode, which is the window ADR 046's panic path
    // exists to clean up after.
    worst = first_error(worst, out8(REG_LCR, LCR_8N1));

    // Trigger level 1, not 14: this driver wants to be woken for a
    // single byte. The kernel's old driver used 14 because it never
    // enabled receive interrupts at all and the figure was arbitrary.
    worst = first_error(
        worst,
        out8(REG_FCR, FCR_ENABLE | FCR_CLEAR_RX | FCR_CLEAR_TX | FCR_TRIGGER_1),
    );

    // OUT2 is what connects the interrupt line to the bus.
    worst = first_error(worst, out8(REG_MCR, MCR_DTR | MCR_RTS | MCR_OUT2));

    if abi::is_error(worst) {
        echo(CHANNEL | STEP_ERROR, worst);
        spin();
    }

    // --- 2. Arrange to be interrupted --------------------------------
    //
    // Two steps here, and a third that is **not** here: the
    // Notification was bound to this thread by the root task, before
    // this component was ever resumed.
    //
    // That is not a division of labour chosen for tidiness. A thread
    // cannot bind its own Notification: `bind_notification` refuses
    // a thread the scheduler is holding (ADR 026), and a thread
    // making a syscall is running by definition. The first version
    // of this driver tried and got `ThreadBusy` — the check working
    // rather than failing.
    //
    // So **what a component may receive is part of how it was
    // built**, settled by whoever built it alongside its CSpace and
    // its address space. The driver inherits the arrangement and
    // uses it.
    //
    // What it does do is say where its *line* delivers, which is
    // its own business: the IRQHandler is its capability and the
    // Notification is its destination (ADR 042).
    worst = first_error(
        worst,
        invoke(
            cptr(SLOT_IRQ),
            method::IRQ_SET_NOTIFICATION,
            cptr(SLOT_NOTIFICATION),
            0,
            0,
            0,
        ),
    );

    // Now enable the device's own interrupt, and only now: an
    // interrupt arriving before the Notification was bound would be
    // counted as unbound and dropped (ADR 042).
    worst = first_error(worst, out8(REG_IER, IER_RX_AVAILABLE));

    // `Ack` opens the line. It starts masked — an owner that has not
    // attached a Notification has nowhere to deliver, so the kernel
    // keeps it shut until the driver says it is ready (ADR 041).
    worst = first_error(worst, invoke(cptr(SLOT_IRQ), method::IRQ_ACK, 0, 0, 0, 0));

    if abi::is_error(worst) {
        echo(CHANNEL | STEP_ERROR, worst);
        spin();
    }

    // --- 3. Prove the port restriction is real -----------------------
    //
    // **Scenario 5, reported by the component the restriction binds.**
    //
    // The driver tries to touch the CMOS index register at 0x70 — a
    // port that can rewrite the machine's configuration, and exactly
    // the sort of thing a UART driver has no business reaching. It
    // holds a live IOPort capability with full rights; what it does
    // not hold is a capability covering *that* port.
    //
    // The error must be `PORT_NOT_IN_RANGE` rather than
    // `INSUFFICIENT_RIGHTS` or `WRONG_TYPE`, and the distinction is
    // the test: a driver with no port access at all would be refused
    // too, and would prove nothing about the range check.
    let denied = invoke(cptr(SLOT_IOPORT), method::IOPORT_OUT8, 0x70, 0, 0, 0);
    echo(CHANNEL | STEP_DENIED, denied);

    // --- 4. Ready ----------------------------------------------------
    echo(CHANNEL | STEP_READY, 0);

    // --- 5. Serve ----------------------------------------------------
    serve();
}

/// The main loop: wait, dispatch, repeat.
///
/// Returns when the request limit is reached. A production driver
/// would not return at all; this one must, because the scenario ends.
fn serve() -> ! {
    let mut written: u64 = 0;
    let mut interrupts: u64 = 0;

    for _ in 0..MAX_REQUESTS {
        // **Blocks until a client sends or the device interrupts.**
        //
        // One call, two sources, because the Notification is bound to
        // this thread (ADR 038).
        let reply = invoke(cptr(SLOT_ENDPOINT), method::RECV, 0, 0, 0, 0);

        if abi::is_error(reply) {
            // The Endpoint was revoked — the root task is taking the
            // driver down, which is `driver-dies`. Report and stop;
            // retrying would spin on an endpoint that is not coming
            // back.
            echo(CHANNEL | STEP_ERROR, reply);
            spin();
        }

        // Badge below, label above (ADR 047). **The split is the
        // trust boundary**: the badge was attached by the root task
        // when it minted the client's capability, so the client
        // could not choose it; the label is the one field the client
        // *does* choose, so it carries the request and nothing else.
        //
        // The driver therefore knows who is asking without believing
        // anybody, and treats what they want as untrusted input.
        let badge = reply & 0xFFFF_FFFF;
        let label = reply >> 32;

        // A notification has no sender and so no label. Zero is not
        // a command, which is what tells the two apart.
        let command = (label >> 8) & 0xFF;
        let _ = badge;

        match command {
            CMD_WRITE => {
                let byte = label & 0xFF;
                if transmit(byte) {
                    written += 1;
                    echo(CHANNEL | STEP_WROTE, written);
                }
            }
            CMD_FINISH => {
                echo(CHANNEL | STEP_WROTE, written);
                spin();
            }
            CMD_PING => {
                // **The handshake.** Answering costs nothing and
                // means something precise: this component has
                // configured the device, claimed its line, and
                // reached its loop. A sender's `Send` does not
                // return until a receiver takes the message, so
                // whoever pinged now knows all of that happened.
                //
                // Nothing else in the system can tell them. The
                // root task cannot see this component's `Echo` --
                // that goes to the kernel's harness, not to a
                // capability it holds -- and there is no "has that
                // thread run yet" invocation, nor should there be:
                // it would be scheduler state exposed as an API,
                // and a component that needs to know should be
                // told by the component in question.
                echo(CHANNEL | STEP_PING, 1);
            }
            _ => {
                // Not a command: a notification. The device has a
                // byte, or the kernel signalled for some other
                // reason — either way the answer is to ask the
                // device.
                interrupts += 1;
                let status = in8(REG_LSR);
                if !abi::is_error(status) && status & LSR_DATA_READY != 0 {
                    let byte = in8(REG_DATA);
                    echo(CHANNEL | STEP_BYTE, byte);
                }
                echo(CHANNEL | STEP_IRQ, interrupts);

                // **`Ack` is the driver's job** (ADR 042). The kernel
                // masked the line before signalling and does not know
                // when the device has stopped asserting it — that
                // needs the device's status register, which is the
                // driver's to read. An `Ack` that never comes
                // silences this device and nothing else.
                invoke(cptr(SLOT_IRQ), method::IRQ_ACK, 0, 0, 0, 0);
            }
        }
    }

    echo(CHANNEL | STEP_WROTE, written);
    spin()
}

/// Commands, as the label encodes them (ADR 047).
const CMD_WRITE: u64 = 1;
const CMD_FINISH: u64 = 2;
/// "Are you serving yet?" — answered, and nothing more.
///
/// A command rather than a side effect of some other request,
/// because the question is real: starting is not instantaneous and
/// the thing that wants to know cannot observe it any other way.
const CMD_PING: u64 = 3;

/// Put one byte on the wire.
///
/// Returns whether it was accepted. Bounded poll, for the kernel's
/// reason: a wedged device must cost output rather than liveness.
fn transmit(byte: u64) -> bool {
    for _ in 0..TX_SPIN_LIMIT {
        let status = in8(REG_LSR);
        if abi::is_error(status) {
            return false;
        }
        if status & LSR_THR_EMPTY != 0 {
            break;
        }
    }
    !abi::is_error(out8(REG_DATA, byte))
}

/// Keep the first error, ignore later successes.
fn first_error(worst: u64, result: u64) -> u64 {
    if abi::is_error(worst) {
        worst
    } else if abi::is_error(result) {
        result
    } else {
        0
    }
}

fn spin() -> ! {
    loop {
        core::hint::spin_loop();
    }
}