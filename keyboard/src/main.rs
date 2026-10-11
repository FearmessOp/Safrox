// SPDX-License-Identifier: MPL-2.0
#![no_std]
#![no_main]

//! The Safrox keyboard driver (M15).
//!
//! The second device outside the kernel, and the point of it is that
//! **nothing in the kernel changed to make it possible**. M13 built
//! the pattern for COM1 — an IOPort capability, an IRQHandler, a
//! Notification bound to this thread, an Endpoint — and this
//! component is that pattern pointed at a different device.
//!
//! What it holds:
//!
//! * **two IOPort capabilities**, `[0x60,0x60]` and `[0x64,0x64]`.
//!   Two rather than one range covering both, because the range
//!   would also carry 0x61 (the PC speaker and the NMI status
//!   register) and 0x62/0x63. Narrower authority for no cost
//!   (ADR 053).
//! * an **IRQHandler** for line 1 and the **Notification** it
//!   delivers to.
//! * an **Endpoint**, which it *sends* on. The console receives.
//!
//! It holds no Untyped, no framebuffer, and no capability naming the
//! console's objects. Criterion 6 is that the keyboard cannot reach
//! the screen, and the reason it cannot is that there is nothing in
//! its CSpace to name (ADR 018).
//!
//! # Scan code set 1
//!
//! Measured, not assumed. See [`scancode`] and ADR 053.
//!
//! # What it does not do
//!
//! No PS/2 controller initialisation: no reset, no LED updates, no
//! typematic rate. Out of scope by decision, and the dependency that
//! creates — on the firmware having left translation on — is
//! `hardware-todo.md` item 11.

#![allow(clippy::upper_case_acronyms)]

mod scancode;

use safrox_abi as abi;
use safrox_abi::method;

use scancode::{Decoder, Event, Key, Modifiers};

// ---- Entry -------------------------------------------------------------

/// Where the root task enters, at CPL 3.
///
/// # Safety
///
/// Called by the root task with a valid stack and nothing else.
#[no_mangle]
#[link_section = ".text.start"]
pub extern "C" fn _start() -> ! {
    main()
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    echo(CHANNEL_PANIC, 0);
    spin()
}

// ---- The syscall -------------------------------------------------------

/// Invoke a capability. ADR 030's registers, widened by ADR 043.
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

// ---- Report channel ----------------------------------------------------
//
// **Taken from the ABI, not written here**, which is what ADR 047
// asked for in M13 and no component had done.
//
// The first draft of this file wrote `1 << 30` as a bare constant.
// That bit is `channel::ROOT_PANIC`: every key event would have
// registered as the root task panicking, and the failure would have
// surfaced nowhere near the keyboard. The console's first draft took
// `1 << 29`, which is `ROOT_DONE`.
//
// Two collisions in one milestone, from the one habit of each
// component naming its own number. Reading the constant from the
// shared crate makes the next collision a compile-time duplicate
// rather than a runtime mystery.

use safrox_abi::channel;

/// The keyboard's channel.
const CHANNEL: u64 = channel::KEYBOARD;
/// Its panic channel.
const CHANNEL_PANIC: u64 = channel::KEYBOARD | channel::PANIC_STEP;

/// The decoder's self-test ran; the value is its failure mask.
const STEP_SELFTEST: u64 = 1;
/// The device was reachable; the value is the status register.
const STEP_PROBE: u64 = 2;
/// The driver has claimed its line and is waiting.
const STEP_READY: u64 = 3;
/// An interrupt arrived; the value is how many have.
const STEP_IRQ: u64 = 4;
/// A byte came off the device; the value is the byte.
const STEP_BYTE: u64 = 5;
/// A key event was sent; the value is the character.
const STEP_KEY: u64 = 6;
/// An invocation failed; the value is the error.
const STEP_ERROR: u64 = 7;
/// Attempting the screen's address faulted, as it must.
const STEP_ISOLATION: u64 = 8;
/// Bytes the decoder did not recognise.
const STEP_UNRECOGNISED: u64 = 9;
/// Events dropped because the batch was full.
const STEP_DROPPED: u64 = 10;
/// The driver is finished; the value is how many keys it sent.
const STEP_DONE: u64 = 11;
/// The device's own queue overran; the value is how many times.
const STEP_OVERRUN: u64 = 12;

// ---- The CSpace the root task builds -----------------------------------
//
// Fixed indices, agreed with `roottask/src/keyboard.rs`.

/// Slot 0 of this component's root CNode is the leaf, which is itself.
const LEAF: u64 = 0;

fn cptr(index: u64) -> u64 {
    (LEAF << 16) | index
}

/// The Endpoint the console receives on.
const SLOT_ENDPOINT: u64 = 1;
/// The IOPort covering 0x60, the data register.
const SLOT_IOPORT_DATA: u64 = 2;
/// The IOPort covering 0x64, the status and command register.
const SLOT_IOPORT_STATUS: u64 = 3;
/// The IRQHandler for line 1.
const SLOT_IRQ: u64 = 4;
/// The Notification line 1 signals, already bound to this thread.
const SLOT_NOTIFICATION: u64 = 5;

// ---- The i8042 ---------------------------------------------------------
//
// Reference: IBM PS/2 Technical Reference, "Keyboard and Auxiliary
// Device Controller"; Intel 8042 datasheet.

/// The data register. Scan codes arrive here.
const PORT_DATA: u64 = 0x60;
/// The status register when read, the command register when written.
///
/// **Only ever read here.** Writing to 0x64 is how the controller is
/// configured and how the machine is reset (0xFE); this driver does
/// neither. The capability permits it, and the restraint is the
/// driver's rather than the kernel's — which is the honest division:
/// the kernel cannot know that a keyboard driver has no business
/// resetting the box.
const PORT_STATUS: u64 = 0x64;

/// Status bit 0: the output buffer is full, so there is a byte to
/// read at 0x60.
const STATUS_OUTPUT_FULL: u64 = 1 << 0;
/// Status bit 5: the byte came from the auxiliary device (a mouse)
/// rather than the keyboard.
///
/// Read and ignored, deliberately: no mouse is configured, so this
/// should never be set, and a byte that has it set is counted as a
/// surprise rather than fed to a keyboard decoder.
const STATUS_AUX: u64 = 1 << 5;

/// How many bytes to take from the device in one wake-up.
///
/// **This is the bound that actually binds** — see the note on
/// [`serve`]. A single interrupt can mean several bytes, because the
/// controller raises the line once and holds the rest in its queue;
/// draining one byte per interrupt would leave the queue full and
/// the line silent.
///
/// 16 is above the longest sequence a keyboard produces (Pause, six
/// bytes) and equal to the i8042's own queue depth, so a full queue
/// is emptied in one visit. Exceeding it means the device is
/// producing faster than this loop consumes, which is reported
/// rather than looped on.
const DRAIN_LIMIT: u32 = 16;

/// How many events one drain can produce.
///
/// Sized to [`DRAIN_LIMIT`] because that is the most bytes that can
/// be read before the batch is flushed, and the fewest events a byte
/// can produce is zero. It cannot be exceeded; see [`serve`].
const BATCH: usize = DRAIN_LIMIT as usize;

/// Bound on the main loop, so the scenario can end.
const MAX_WAKEUPS: u32 = 256;

// ---- Port access -------------------------------------------------------

/// Read the status register at 0x64.
fn read_status() -> u64 {
    invoke(
        cptr(SLOT_IOPORT_STATUS),
        method::IOPORT_IN8,
        PORT_STATUS,
        0,
        0,
        0,
    )
}

/// Read a scan code from 0x60.
fn read_data() -> u64 {
    invoke(cptr(SLOT_IOPORT_DATA), method::IOPORT_IN8, PORT_DATA, 0, 0, 0)
}

// ---- The driver --------------------------------------------------------

fn main() -> ! {
    // --- 1. Test the decoder, before touching anything ----------------
    //
    // **Runs with no capability at all**, which is what makes it
    // useful in `kbd-no-cap`: that scenario has to distinguish "the
    // driver was refused the hardware" from "the driver is broken",
    // and a self-test that needed the hardware could not tell them
    // apart.
    //
    // Every byte sequence it checks was measured from the device
    // (ADR 053), not invented.
    let selftest = scancode::self_test();
    echo(
        CHANNEL | STEP_SELFTEST,
        (selftest.failures as u64) | ((selftest.cases as u64) << 32),
    );
    if selftest.failures != 0 {
        // Nothing below would mean anything: a broken decoder turns
        // correct scan codes into wrong keys, and the test would be
        // measuring the wrong thing while passing.
        spin();
    }

    // --- 2. Prove isolation from the screen ---------------------------
    //
    // **Criterion 6, the half this component can demonstrate.**
    //
    // The console maps the framebuffer at `FRAMEBUFFER_BASE` in its
    // own address space. This component has no mapping there and no
    // capability that could create one, so the read below faults.
    //
    // Only attempted in the scenario built for it: a #PF ends the
    // component, so a driver that did this every boot would never
    // serve a key.
    if scenario_is_isolation() {
        echo(CHANNEL | STEP_ISOLATION, abi::FRAMEBUFFER_BASE);
        // SAFETY: There is no safe way to make this claim. The point
        // of the read is that the address is **not** mapped in this
        // address space, so it must fault; the kernel's #PF handler
        // reports the faulting address and the scenario checks it.
        // Nothing after this line runs.
        unsafe {
            let value = core::ptr::read_volatile(abi::FRAMEBUFFER_BASE as *const u32);
            // Unreachable if the claim holds. Reported rather than
            // ignored, because a read that *succeeded* would mean
            // the isolation this milestone is about does not exist,
            // and silence would let the scenario pass.
            echo(CHANNEL | STEP_ERROR, value as u64);
        }
        spin();
    }

    // --- 3. Is the device there? --------------------------------------
    //
    // A read of the status register, before claiming anything. On a
    // machine with no i8042 the bus floats and this returns 0xFF —
    // which is reported, so that a run producing no keys says why.
    let status = read_status();
    echo(CHANNEL | STEP_PROBE, status);
    if abi::is_error(status) {
        echo(CHANNEL | STEP_ERROR, status);
        spin();
    }

    // Drain whatever the firmware left behind. The BIOS used the
    // keyboard during POST and the queue may hold a byte; left
    // there, it would be delivered as the user's first key press.
    let mut stale = 0u32;
    while stale < DRAIN_LIMIT {
        let status = read_status();
        if abi::is_error(status) || status & STATUS_OUTPUT_FULL == 0 {
            break;
        }
        read_data();
        stale += 1;
    }

    // --- 4. Arrange to be interrupted ---------------------------------
    //
    // The Notification was bound to this thread by the root task,
    // before this component ran. It could not have bound its own:
    // `bind_notification` refuses a thread the scheduler is holding
    // (ADR 026), and a thread making a syscall is running by
    // definition. M13 learned this and the reasoning is in
    // `driver/src/main.rs`.
    let mut worst = invoke(
        cptr(SLOT_IRQ),
        method::IRQ_SET_NOTIFICATION,
        cptr(SLOT_NOTIFICATION),
        0,
        0,
        0,
    );

    // The line starts masked: an owner with nowhere to deliver has
    // the door kept shut for it (ADR 041). `Ack` opens it, and only
    // now, with the destination set.
    worst = first_error(worst, invoke(cptr(SLOT_IRQ), method::IRQ_ACK, 0, 0, 0, 0));

    if abi::is_error(worst) {
        echo(CHANNEL | STEP_ERROR, worst);
        spin();
    }

    echo(CHANNEL | STEP_READY, 0);

    serve()
}

/// Wait, drain, decode, send. Repeat.
///
/// # The buffer that is not here, and why
///
/// ADR 053 specified a 32-entry ring buffer with a drop-oldest
/// policy, against the hazard of a device producing faster than a
/// consumer consumes. Writing this loop showed the ring **cannot
/// overflow**, and the reason is worth recording rather than
/// quietly deleting the buffer.
///
/// `Send` blocks (ADR 033): it does not return until the console
/// receives. So this thread is in exactly one of two states —
/// draining the device, or blocked handing an event over. It can
/// never be accumulating events faster than it emits them, because
/// it does not run while it is blocked.
///
/// A 32-entry ring in that shape holds at most [`DRAIN_LIMIT`]
/// entries, is emptied before the next wait, and its overflow branch
/// is unreachable. **An overflow policy that cannot be reached is
/// indistinguishable from its absence** (ADR 023, K5), and writing
/// one would have produced a test asserting a behaviour the code
/// could not perform.
///
/// So the bound that binds is `DRAIN_LIMIT`, and the queue that
/// really overflows is the **device's**: while this thread blocks in
/// `Send`, the line is masked and the i8042 accumulates. When its
/// 16-byte queue fills it reports an overrun (0x00 or 0xFF), which
/// [`Decoder`] counts. `kbd-flood` drives exactly this and reads the
/// counter — a real bound, overflowed for real, observed where it
/// actually happens.
///
/// The batch below is not a queue: it is the result of one drain,
/// flushed immediately. It exists so the line can be `Ack`ed before
/// the sends begin, rather than held masked for the duration.
fn serve() -> ! {
    let mut decoder = Decoder::new();
    let mut modifiers = Modifiers::default();

    let mut wakeups = 0u32;
    let mut keys_sent = 0u64;
    let mut dropped = 0u64;
    let mut overruns = 0u64;

    // On the stack, not in a static. The image is mapped
    // read-execute and `link.ld` asserts `.bss` and `.data` are
    // empty, so a `static mut` would not merely be poor style — it
    // would fail to link, and if it linked it would fault on first
    // write.
    let mut batch = [(0u8, 0u8); BATCH];

    for _ in 0..MAX_WAKEUPS {
        // Blocks until line 1 fires. `Wait`, not `Recv`: nothing
        // sends to this component. The Endpoint it holds is for
        // sending, and the console is the one that receives.
        let signalled = invoke(cptr(SLOT_NOTIFICATION), method::WAIT, 0, 0, 0, 0);
        if abi::is_error(signalled) {
            // The Notification was revoked — `kbd-driver-dies` is
            // taking this component down. Stop; retrying would spin
            // on an object that is not coming back.
            echo(CHANNEL | STEP_ERROR, signalled);
            spin();
        }

        wakeups += 1;
        echo(CHANNEL | STEP_IRQ, wakeups as u64);

        // --- Drain ----------------------------------------------------
        //
        // Until the output buffer is empty, not once: the controller
        // raises the line a single time for a sequence, and a driver
        // that read one byte would leave the rest in the queue with
        // no further interrupt to collect them. The symptom is the
        // keyboard going silent after the first key.
        let mut count = 0usize;
        let mut reads = 0u32;

        while reads < DRAIN_LIMIT {
            let status = read_status();
            if abi::is_error(status) {
                echo(CHANNEL | STEP_ERROR, status);
                break;
            }
            if status & STATUS_OUTPUT_FULL == 0 {
                break;
            }

            let byte = read_data();
            if abi::is_error(byte) {
                echo(CHANNEL | STEP_ERROR, byte);
                break;
            }
            reads += 1;

            if status & STATUS_AUX != 0 {
                // A mouse byte. None is configured, so this is a
                // surprise; counted rather than fed to a decoder
                // that would read it as a keyboard code.
                overruns += 1;
                continue;
            }

            let byte = (byte & 0xFF) as u8;
            echo(CHANNEL | STEP_BYTE, byte as u64);

            let before = decoder.unrecognised();
            let Some(event) = decoder.feed(byte) else {
                if decoder.unrecognised() != before {
                    // An overrun or error code from the controller.
                    // **This is `kbd-flood`'s observable**: the
                    // device's queue filled while this thread was
                    // blocked in `Send`.
                    overruns += 1;
                }
                continue;
            };

            if modifiers.apply(event) {
                // A modifier changes state and produces no
                // character. Not sent: the console has no use for
                // "shift went down" and sending it would make the
                // key count disagree with what was typed.
                continue;
            }

            let Event::Make(key) = event else {
                // Releases are decoded — criterion 2 is that
                // make and break are told apart — and not
                // forwarded. The console draws characters.
                continue;
            };

            if count == batch.len() {
                // Unreachable: `reads` is bounded by `DRAIN_LIMIT`
                // and `batch` is that long, so there is a slot for
                // every byte read even if every byte produced an
                // event. Counted anyway, because "unreachable"
                // claims that are never checked are how K10
                // happens, and this one costs a comparison.
                dropped += 1;
                continue;
            }

            let (code, extended) = match key {
                Key::Code(c) => (c, 0u8),
                Key::Extended(c) => (c, 1u8),
            };
            let _ = code;

            batch[count] = (
                if extended == 1 {
                    // Extended keys have no character; the console
                    // reads the scan code and moves the cursor.
                    0
                } else {
                    scancode::to_char(key, modifiers)
                },
                match key {
                    Key::Code(c) | Key::Extended(c) => c,
                },
            );
            // Remember whether it was extended, in the high bit of
            // the scan code slot. The console needs the distinction:
            // left arrow and keypad-4 are the same number.
            if extended == 1 {
                batch[count].1 |= 0x80;
            }
            count += 1;
        }

        if reads == DRAIN_LIMIT {
            // The queue was still not empty. Reported: it means the
            // device is producing faster than this loop consumes,
            // which is `kbd-flood`'s condition.
            overruns += 1;
        }

        // --- Ack, then send -------------------------------------------
        //
        // **In this order, and the order is the point.** `Ack`
        // reopens the line (ADR 042); the sends below block until
        // the console takes each event. Sending first would hold
        // the line masked for the whole of the console's drawing,
        // and every key pressed during it would be lost at the
        // device rather than queued.
        invoke(cptr(SLOT_IRQ), method::IRQ_ACK, 0, 0, 0, 0);

        for &(character, code) in batch.iter().take(count) {
            // Label carries the payload, badge carries identity
            // (ADR 047). This component chose neither: the badge was
            // fixed when the root task minted the capability.
            let label = ((code as u64) << 8) | character as u64;
            let sent = invoke(cptr(SLOT_ENDPOINT), method::SEND, label, 0, 0, 0);
            if abi::is_error(sent) {
                echo(CHANNEL | STEP_ERROR, sent);
                spin();
            }
            keys_sent += 1;
            echo(CHANNEL | STEP_KEY, character as u64);
        }

        if decoder.unrecognised() > 0 {
            echo(CHANNEL | STEP_UNRECOGNISED, decoder.unrecognised() as u64);
        }
        if overruns > 0 {
            echo(CHANNEL | STEP_OVERRUN, overruns);
        }
        if dropped > 0 {
            echo(CHANNEL | STEP_DROPPED, dropped);
        }
    }

    echo(CHANNEL | STEP_DONE, keys_sent);
    spin()
}

/// Whether this build is the isolation scenario.
///
/// Patched into the image by the root task, the same mechanism the
/// screen component's geometry uses (ADR 049): a component cannot be
/// told which scenario it is in over IPC, because the question is
/// answered before it has anybody to ask.
///
/// `read_volatile` because the value the compiler saw is zero and
/// the value that is there now is not.
fn scenario_is_isolation() -> bool {
    // SAFETY: `KEYBOARD_SCENARIO` is in this image's .rodata, which
    // the root task patched before mapping the page read-execute.
    // A volatile read because the compiler's view of it is stale.
    unsafe { core::ptr::read_volatile(&raw const KEYBOARD_SCENARIO) == SCENARIO_ISOLATION }
}

/// The scenario number, patched by the root task before the image is
/// mapped. Zero means the ordinary one.
#[no_mangle]
#[link_section = ".rodata.scenario"]
pub static KEYBOARD_SCENARIO: u64 = 0;

/// The value that selects the isolation attempt.
///
/// A recognisable constant rather than 1: this is read out of a
/// patched image, and a patch landing at the wrong offset would far
/// more likely produce a small number than this one.
const SCENARIO_ISOLATION: u64 = 0x1501_A710;

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