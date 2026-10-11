// SPDX-License-Identifier: MPL-2.0
//! M15's acceptance checks: the keyboard and the console.
//!
//! **The kernel is not under test here, and that is the claim.**
//! M13 needed a new capability type to move the serial port out;
//! M14 needed a device Untyped that named a real device. This
//! milestone needed neither — the keyboard is M13's pattern pointed
//! at a second device and the console is M14's mapping pointed at
//! the same framebuffer. So what this file checks is mostly what
//! two user components reported about themselves, plus the handful
//! of things only the kernel can see.
//!
//! # What only the kernel can see
//!
//! * that IRQ 1 is **owned by a user component** and not by the
//!   kernel, read out of `cap::irq`'s tables rather than from
//!   anything a component said;
//! * that the keyboard's #PF in `kbd-isolation` landed at the
//!   framebuffer's address, compared against a prediction the
//!   kernel computed itself;
//! * that interrupts actually arrived, counted at the vector.
//!
//! # What the components report, and why that is enough
//!
//! The pixel checks are made by the console, because reading the
//! framebuffer requires a mapping and the kernel deliberately has
//! none (M14's criterion 2 — a kernel that could draw has lent the
//! device rather than handed it over).
//!
//! That is not "take the component's word for it". The component
//! reports **what it read**, pixel by pixel, and this file checks
//! the values. A component that drew nothing would report
//! background where ink was expected, and the check here would
//! fail on the number.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use safrox_abi as abi;

use crate::println;
use crate::qemu;
use crate::sched;
use crate::syscall;

// ---- Channels ----------------------------------------------------------

const KEYBOARD_CHANNEL: u64 = abi::channel::KEYBOARD;
const CONSOLE_CHANNEL: u64 = abi::channel::CONSOLE;

const STEP_COUNT: usize = 32;

// The keyboard's steps, matching `keyboard/src/main.rs`.
const KBD_SELFTEST: u64 = 1;
const KBD_PROBE: u64 = 2;
const KBD_READY: u64 = 3;
const KBD_IRQ: u64 = 4;
const KBD_BYTE: u64 = 5;
const KBD_KEY: u64 = 6;
const KBD_ERROR: u64 = 7;
const KBD_ISOLATION: u64 = 8;
const KBD_UNRECOGNISED: u64 = 9;
const KBD_DROPPED: u64 = 10;
const KBD_DONE: u64 = 11;
const KBD_OVERRUN: u64 = 12;

// The console's steps, matching `console/src/main.rs`.
const CON_GEOMETRY: u64 = 1;
const CON_MAPPED: u64 = 2;
const CON_GRID: u64 = 3;
const CON_READY: u64 = 4;
const CON_DREW: u64 = 5;
const CON_PIXEL_TSC: u64 = 6;
const CON_VERIFIED: u64 = 7;
const CON_VERIFY_FAIL: u64 = 8;
const CON_SCROLLED: u64 = 9;
const CON_SCROLL_CYCLES: u64 = 10;
const CON_SCROLL_CELLS: u64 = 11;
const CON_SCROLL_VERIFY: u64 = 12;
const CON_ERROR: u64 = 13;
const CON_ISOLATION: u64 = 14;
const CON_KEYS: u64 = 15;
const CON_DONE: u64 = 16;

const PANIC_STEP: u64 = abi::channel::PANIC_STEP;

const NOT_REPORTED: u64 = u64::MAX;

// ---- Recording ---------------------------------------------------------

static KBD_STEPS: [AtomicU64; STEP_COUNT] = [const { AtomicU64::new(NOT_REPORTED) }; STEP_COUNT];
static CON_STEPS: [AtomicU64; STEP_COUNT] = [const { AtomicU64::new(NOT_REPORTED) }; STEP_COUNT];

static KBD_PANICKED: AtomicBool = AtomicBool::new(false);
static CON_PANICKED: AtomicBool = AtomicBool::new(false);

/// How many characters the console has verified, and how many of
/// those verifications failed.
///
/// Counted separately from the steps because every key produces one
/// and a step holds a single value: without the counter, "the last
/// character verified" would pass a run where every earlier one had
/// failed.
static VERIFIED_COUNT: AtomicU64 = AtomicU64::new(0);
static VERIFY_FAILURES: AtomicU64 = AtomicU64::new(0);

/// The characters the console reported drawing, in order.
///
/// **The acceptance test's evidence for criterion 1.** A key
/// injected as `a` must come out of the decoder as `a`, and the only
/// way to know is to record what the console was asked to draw and
/// compare the sequence with what was injected.
static DRAWN: [AtomicU64; 32] = [const { AtomicU64::new(NOT_REPORTED) }; 32];
static DRAWN_COUNT: AtomicU64 = AtomicU64::new(0);

/// TSC readings taken by the console right after a character
/// reached the framebuffer.
static PIXEL_TSC: [AtomicU64; 32] = [const { AtomicU64::new(0) }; 32];
static PIXEL_TSC_COUNT: AtomicU64 = AtomicU64::new(0);

/// TSC readings taken by the **kernel** at the moment IRQ 1 was
/// delivered.
///
/// This is the other end of the latency measurement, and it is taken
/// here rather than in the keyboard component for a reason: the
/// component's first instruction runs after the kernel has already
/// delivered the interrupt, scheduled it and returned to CPL 3. A
/// reading taken there would exclude the part of the path the
/// microkernel is responsible for.
static IRQ_TSC: [AtomicU64; 32] = [const { AtomicU64::new(0) }; 32];
static IRQ_TSC_COUNT: AtomicU64 = AtomicU64::new(0);

/// Record an interrupt on line 1, from the IRQ dispatch path.
///
/// Called with interrupts disabled, from the vector handler.
pub fn record_irq(irq: u8) {
    if irq != 1 {
        return;
    }
    let index = IRQ_TSC_COUNT.fetch_add(1, Ordering::AcqRel) as usize;
    if index < IRQ_TSC.len() {
        IRQ_TSC[index].store(read_tsc(), Ordering::Release);
    }
}

/// Record a step, from the dispatcher's `Echo` path.
///
/// Errors are sticky, for the reason M11 established: several
/// invocations share a step and "last write wins" lets a failure be
/// hidden by what follows.
pub fn record_step(channel: u64, value: u64) -> bool {
    let (table, panicked) = if channel & KEYBOARD_CHANNEL != 0 {
        (&KBD_STEPS, &KBD_PANICKED)
    } else if channel & CONSOLE_CHANNEL != 0 {
        (&CON_STEPS, &CON_PANICKED)
    } else {
        return false;
    };

    let index = (channel & 0xFF) as usize;

    if index == PANIC_STEP as usize {
        panicked.store(true, Ordering::Release);
        return true;
    }

    // The per-character records, before the generic step store: each
    // of these arrives many times and a single slot would keep only
    // the last.
    if channel & CONSOLE_CHANNEL != 0 {
        match index as u64 {
            CON_DREW => {
                let at = DRAWN_COUNT.fetch_add(1, Ordering::AcqRel) as usize;
                if at < DRAWN.len() {
                    DRAWN[at].store(value, Ordering::Release);
                }
            }
            CON_VERIFIED => {
                VERIFIED_COUNT.fetch_add(1, Ordering::AcqRel);
            }
            CON_VERIFY_FAIL => {
                VERIFY_FAILURES.fetch_add(1, Ordering::AcqRel);
            }
            CON_PIXEL_TSC => {
                let at = PIXEL_TSC_COUNT.fetch_add(1, Ordering::AcqRel) as usize;
                if at < PIXEL_TSC.len() {
                    PIXEL_TSC[at].store(value, Ordering::Release);
                }
            }
            _ => {}
        }
    }

    if index < STEP_COUNT {
        let previous = table[index].load(Ordering::Acquire);
        if previous == NOT_REPORTED || !syscall::is_error(previous) {
            table[index].store(value, Ordering::Release);
        }
    }
    true
}

fn kbd(which: u64) -> u64 {
    KBD_STEPS[which as usize].load(Ordering::Acquire)
}

fn con(which: u64) -> u64 {
    CON_STEPS[which as usize].load(Ordering::Acquire)
}

struct StepDump;

impl core::fmt::Debug for StepDump {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "kbd[")?;
        for (index, slot) in KBD_STEPS.iter().enumerate() {
            let value = slot.load(Ordering::Acquire);
            if value != NOT_REPORTED {
                write!(f, " {}={:#x}", index, value)?;
            }
        }
        write!(f, " ] con[")?;
        for (index, slot) in CON_STEPS.iter().enumerate() {
            let value = slot.load(Ordering::Acquire);
            if value != NOT_REPORTED {
                write!(f, " {}={:#x}", index, value)?;
            }
        }
        write!(f, " ]")
    }
}

fn read_tsc() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: RDTSC is unprivileged, reads no memory, writes EDX:EAX.
    unsafe {
        core::arch::asm!("rdtsc", out("eax") low, out("edx") high,
                         options(nomem, nostack, preserves_flags));
    }
    ((high as u64) << 32) | (low as u64)
}

/// Long enough for the console to map a thousand pages and for the
/// harness to inject its keys.
///
/// M14's figure, which is twice M13's for the mapping, and keys
/// arrive on top of that.
const DEADLINE: u64 = 90_000_000_000;

/// Wait until a console step is reported, or the deadline passes.
///
/// Waiting for an **event** rather than a duration (ADR 023, K8).
fn wait_for_console(which: u64) -> bool {
    let deadline = read_tsc() + DEADLINE;
    while con(which) == NOT_REPORTED && read_tsc() < deadline {
        // **Checked inside the loop, not merely before it.** If the
        // root task could not build the console, the line being
        // waited for is never coming, and waiting the full deadline
        // out to then blame the console is both slow and wrong.
        check_root_task_setup();
        sched::yield_now();
    }
    con(which) != NOT_REPORTED
}

/// Wait until the console has drawn `count` characters.
///
/// The keys are injected by `run.py` after the console says it is
/// ready, so this is the only way to know they have all arrived.
fn wait_for_characters(count: u64) -> bool {
    let deadline = read_tsc() + DEADLINE;
    let mut announced = 0u64;
    loop {
        announce_keys(&mut announced);
        if DRAWN_COUNT.load(Ordering::Acquire) >= count || read_tsc() >= deadline {
            break;
        }
        sched::yield_now();
    }
    announce_keys(&mut announced);
    DRAWN_COUNT.load(Ordering::Acquire) >= count
}

/// Print one line per key the console has taken.
///
/// **The harness paces itself on these**, and without them it could
/// not. The i8042's output buffer is sixteen bytes and one key can
/// be four of them (a shifted letter is make, make, break, break),
/// so a runner that injected its keys as fast as QMP accepts them
/// would overrun the device and lose some — and the test would fail
/// with "the console drew 5 characters, expected 7", blaming the
/// decoder for the harness's haste.
///
/// The alternative was a fixed delay between keys, which ADR 023's
/// K8 rules out: a duration chosen to be long enough on this machine
/// is a duration that is wrong on another. These lines turn the
/// pacing into an event.
///
/// **Counted from the console, not the keyboard.** The keyboard's
/// count says what it sent; this says what was taken. On a
/// rendezvous they end up equal (ADR 033), and the difference in
/// between is exactly the window the harness must not run ahead of.
///
/// Printed *after* both TSC readings for the key are already
/// recorded — the console takes its reading when the pixels land and
/// the kernel took its at interrupt time, so a serial write here
/// cannot inflate the latency it is measuring. It does delay the
/// *next* key, which is the point.
fn announce_keys(seen: &mut u64) {
    let keys = con(CON_KEYS);
    if keys == NOT_REPORTED {
        return;
    }
    while *seen < keys {
        *seen += 1;
        println!("[kbd] the console has taken {} key(s)", *seen);
    }
}

// ---- The keys the harness injects --------------------------------------

/// What `run.py` sends, and therefore what must come back.
///
/// Named here as well as in the runner, which is two files agreeing
/// by hand — the hazard ADR 047 recorded. It is not avoidable
/// without a channel from the harness into the kernel, and inventing
/// one for the tests is the thing this project keeps refusing to do.
///
/// `shift-a` produces 'A' and `a` produces 'a': the pair is what
/// shows the modifier is **state** rather than part of the key, and
/// the second one also shows the release was processed — a decoder
/// that never saw `0xAA` would report 'A' both times.
const EXPECTED: &[u8] = b"saFrox";

// ---- Criterion 1: scan codes become characters -------------------------

/// Check that what was injected is what the console drew.
///
/// **The end-to-end claim, and the only one that covers the whole
/// path.** A scan code arrived at 0x60, a component at CPL 3 read
/// it, decoded set 1, applied the modifier state, sent it over an
/// Endpoint, and another component drew it. Any break anywhere in
/// that chain changes this sequence.
fn check_characters() {
    let count = DRAWN_COUNT.load(Ordering::Acquire);

    if count < EXPECTED.len() as u64 {
        panic!(
            "the console drew {} characters, expected {}. The keys \
             the harness injected did not all reach the glass: a \
             scan code was dropped, mis-decoded, or never arrived. \
             Steps: {:?}",
            count,
            EXPECTED.len(),
            StepDump
        );
    }

    for (index, expected) in EXPECTED.iter().enumerate() {
        let found = DRAWN[index].load(Ordering::Acquire);
        if found != *expected as u64 {
            panic!(
                "character {} was {:#04x} ({:?}), expected {:#04x} \
                 ({:?}). The decoder turned a scan code into the \
                 wrong character -- check the set 1 tables in \
                 keyboard/src/scancode.rs against what \
                 scripts/m15-spike/ measures.",
                index, found, found as u8 as char, expected, *expected as char
            );
        }
    }

    println!(
        "[kbd] {} injected keys arrived as the characters they name: {:?}",
        EXPECTED.len(),
        core::str::from_utf8(EXPECTED).unwrap_or("?")
    );
}

// ---- Criterion 2: make and break are distinguished ---------------------

/// Check that key releases were decoded and not forwarded.
///
/// Two things at once, and both matter:
///
/// * the driver's self-test covers the decoding, with sequences
///   measured from the device;
/// * the character sequence above covers the *consequence* — `a`
///   after `shift-a` is lower case only if the shift release was
///   seen.
///
/// The second is why this is not merely the self-test repeated: a
/// decoder that reported `Break` correctly but a driver that forgot
/// to apply it would pass the self-test and fail here.
fn check_make_break() {
    let selftest = kbd(KBD_SELFTEST);
    if selftest == NOT_REPORTED {
        panic!(
            "the keyboard component never reported its decoder \
             self-test. It runs before any capability is touched, so \
             its absence means the component did not start at all. \
             Steps: {:?}",
            StepDump
        );
    }

    let failures = selftest & 0xFFFF_FFFF;
    let cases = selftest >> 32;

    if cases == 0 {
        panic!(
            "the decoder self-test ran zero cases, which cannot fail \
             and therefore proves nothing (ADR 023, K5)."
        );
    }

    if failures != 0 {
        panic!(
            "the scan code decoder failed self-test cases {:#b} of \
             {}. Each case is a byte sequence measured from the real \
             device by scripts/m15-spike/; a failure means the \
             decoder disagrees with the hardware, not with its \
             author.",
            failures, cases
        );
    }

    println!(
        "[kbd] the scan code decoder passed {} self-test cases, every \
         sequence measured from the device",
        cases
    );

    // The consequence check: 'a' lower case after 'A' upper case.
    //
    // EXPECTED is "saFrox": the 'F' is shifted and the 'r' after it
    // is not. A driver that never processed the shift release would
    // have drawn 'R'.
    let after_shift = DRAWN[3].load(Ordering::Acquire);
    if after_shift != b'r' as u64 {
        panic!(
            "the character after a shifted one was {:#04x}, expected \
             {:#04x} ('r'). The shift *release* was not processed: \
             make and break are being treated alike, so the modifier \
             never goes back down.",
            after_shift, b'r'
        );
    }

    println!("[kbd] a key release was decoded: the shift came back up");
}

// ---- Criterion 3: extended keys ----------------------------------------

/// Check that the 0xE0 prefix was decoded.
///
/// The harness injects a `left` arrow after the text, and the
/// console moves its cursor rather than drawing a character. So the
/// evidence is that the **next** character lands where the cursor
/// was moved to — overwriting the last letter rather than following
/// it.
fn check_extended() {
    // Case 3 of the decoder's self-test is the one that distinguishes
    // an extended key from the plain key of the same number, and a
    // decoder that dropped the prefix fails it. Already checked in
    // `check_make_break` via the failure mask; what is checked here
    // is the consequence.
    let count = DRAWN_COUNT.load(Ordering::Acquire);
    let expected = EXPECTED.len() as u64 + 1;

    if count < expected {
        panic!(
            "the console drew {} characters; {} were expected after \
             the arrow key and the letter following it. An extended \
             key either produced a character (the prefix was dropped) \
             or was swallowed entirely.",
            count, expected
        );
    }

    let overwritten = DRAWN[EXPECTED.len()].load(Ordering::Acquire);
    if overwritten != b'X' as u64 {
        panic!(
            "the character typed after the left arrow was {:#04x}, \
             expected {:#04x} ('X').",
            overwritten, b'X'
        );
    }

    println!(
        "[kbd] the 0xE0 prefix was decoded: an arrow key moved the \
         cursor instead of drawing"
    );
}

// ---- Criterion 4: the pixels match the font ----------------------------

/// Check the console's read-back of what it drew.
///
/// **This is the criterion the milestone turns on.** "I called the
/// draw function" proves nothing; the component reads pixels back
/// out of the real framebuffer and compares them against the font
/// bitmap it drew from.
///
/// Eight samples per character (ADR 054): four pixels the glyph
/// sets, two it leaves clear inside its body, and two just outside
/// the cell. Dropping any group leaves a real failure undetected —
/// only set pixels passes a cell painted solid, only inside-the-cell
/// passes a glyph one column to the left.
fn check_pixels() {
    let failures = VERIFY_FAILURES.load(Ordering::Acquire);
    let verified = VERIFIED_COUNT.load(Ordering::Acquire);

    if verified == 0 {
        panic!(
            "the console verified zero characters against the font. \
             Either nothing was drawn, or the verification was \
             skipped -- and a check that never runs is \
             indistinguishable from its absence (ADR 023, K5). \
             Steps: {:?}",
            StepDump
        );
    }

    if failures != 0 {
        let detail = con(CON_VERIFY_FAIL);
        let which = detail >> 32;
        let found = detail & 0xFFFF_FFFF;
        panic!(
            "{} of {} characters did not match the font when read \
             back from the framebuffer. Last failure: check {} found \
             {:#010x}. Check 1 is a pixel the glyph sets, 2 one it \
             leaves clear, 3 and 4 are outside the cell -- so 3 or 4 \
             means the glyph went into the wrong cell.",
            failures, verified, which, found
        );
    }

    let last = con(CON_VERIFIED);
    let sampled = last & 0xFFFF_FFFF;
    let character = (last >> 32) as u8;

    println!(
        "[kbd] {} characters read back from the framebuffer match the \
         font bitmap, {} pixels sampled each (last was {:?})",
        verified, sampled, character as char
    );
}

// ---- Criterion 5: scrolling --------------------------------------------

/// Check that the screen scrolled and that both ends moved.
///
/// Verified by **pixel reads at the top and the bottom**. Checking
/// only the top would pass a scroll that moved the text up and left
/// the old last row where it was.
fn check_scroll() {
    let scrolls = con(CON_SCROLLED);
    if scrolls == NOT_REPORTED || scrolls == 0 {
        panic!(
            "the console never scrolled. The scenario injects enough \
             keys to fill the screen; if it did not scroll, either \
             the keys did not arrive or the row count is wrong. \
             Steps: {:?}",
            StepDump
        );
    }

    let verify = con(CON_SCROLL_VERIFY);
    if verify == NOT_REPORTED {
        panic!("the console scrolled but never reported its read-back.");
    }

    let top = verify & 0xFFFF_FFFF;
    let bottom = verify >> 32;

    // After a scroll the top row carries what was on the second row,
    // which is text, so at least one of three samples should be ink.
    if top == 0 {
        panic!(
            "after scrolling, all three samples from the top row were \
             background. The content below did not move up -- the \
             scroll cleared the screen rather than shifting it."
        );
    }

    // And the last row must have been cleared.
    if bottom != 0 {
        panic!(
            "after scrolling, {} of three samples from the bottom row \
             were still ink. The last row was not cleared, so the \
             text that was there is still on the glass under whatever \
             is written next.",
            bottom
        );
    }

    println!(
        "[kbd] the screen scrolled {} time(s): the top row carries \
         what was below it and the last row was cleared, both read \
         back from the framebuffer",
        scrolls
    );
}

// ---- Criterion 6: isolation --------------------------------------------

/// Where `kbd-isolation` expects the keyboard's faulting read.
///
/// Computed by the kernel from the ABI constant, **not** announced
/// by the component. M14 learned why: an `Echo` before the faulting
/// access cannot be heard, because `Echo` returns straight to ring 3
/// (ADR 048) and the next instruction faults with no scheduling
/// point in between.
static PREDICTED_FAULT: AtomicU64 = AtomicU64::new(0);

/// Called from the #PF handler with the faulting address.
pub fn check_predicted_fault(cr2: u64) {
    let predicted = PREDICTED_FAULT.load(Ordering::Acquire);
    if predicted == 0 {
        return;
    }

    if cr2 != predicted {
        panic!(
            "the keyboard component faulted at {:#018x}, but \
             kbd-isolation predicted {:#018x}. The fault is not the \
             one the scenario is about.",
            cr2, predicted
        );
    }

    println!(
        "[kbd] the keyboard component took a #PF at {:#018x}, the \
         address the console maps the framebuffer at: it cannot see \
         the screen",
        cr2
    );

    println!("[kbd] M15 kbd-isolation checks passed");

    // **Exiting from the fault handler is correct, and it is only
    // correct because the two halves are ordered elsewhere.**
    //
    // There is no choice about ending the run here: the kernel has
    // no fault recovery, and returning would re-execute the
    // faulting instruction forever.
    //
    // That makes the keyboard's fault the end of the scenario, so
    // the console's half has to have happened *first*. It does:
    // the root task waits to receive the console's report before it
    // resumes the keyboard (`roottask/src/keyboard.rs`), which is a
    // rendezvous and not a delay.
    //
    // Without that ordering this scenario failed about one run in
    // three, and the failure looked like the serial interleaving
    // M14 chased — the expected line appeared to run into the next
    // one. It was not interleaving: the line was never printed at
    // all, because the component that prints it had not run yet.
    qemu::exit(qemu::EXIT_FAILURE);
}

// ---- What the root task said went wrong --------------------------------

/// The root task's M15 step numbers, from `roottask/src/keyboard.rs`.
///
/// Named again here, which is two files agreeing by hand — the
/// hazard ADR 047 recorded and has not yet fixed. The alternative
/// is the ABI crate, where they belong; moving them is a change to
/// the contract every component reads and not something to do in
/// passing.
mod root_step {
    pub const KBD_RETYPE: u64 = 60;
    pub const KBD_SETUP: u64 = 61;
    pub const KBD_WIRED: u64 = 65;
    pub const KBD_RESUME: u64 = 66;
    pub const CON_RETYPE: u64 = 67;
    pub const CON_SETUP: u64 = 68;
    pub const CON_WIRED: u64 = 69;
    pub const CON_RESUME: u64 = 70;
}

/// Check that the root task managed to build both components.
///
/// **A component that was never built cannot report anything**, and
/// until this existed that failure was indistinguishable from a
/// component that started and went quiet. The first run of
/// `kbd-test` died on "the console never reported itself ready",
/// which named the console — and the console was innocent: the root
/// task's own reports were being dropped, because `roottest`'s step
/// table was twenty entries long and M15's steps are numbered sixty
/// and up.
///
/// So the evidence existed, was recorded nowhere, and the test
/// pointed at the wrong component. Both halves are fixed: the table
/// is long enough, and this reads it.
///
/// Called first by every scenario.
fn check_root_task_setup() {
    for (step, what) in [
        (root_step::CON_RETYPE, "create the console's objects"),
        (root_step::CON_SETUP, "build the console's address space"),
        (root_step::CON_WIRED, "hand the console its capabilities"),
        (root_step::CON_RESUME, "start the console"),
        (root_step::KBD_RETYPE, "create the keyboard's objects"),
        (root_step::KBD_SETUP, "build the keyboard's address space"),
        (root_step::KBD_WIRED, "hand the keyboard its capabilities"),
        (root_step::KBD_RESUME, "start the keyboard"),
    ] {
        let result = crate::roottest::m15_step(step);
        if result == NOT_REPORTED || !syscall::is_error(result) {
            continue;
        }
        panic!(
            "the root task failed to {}: error {} ({:#x}). No \
             component reported anything because there was no \
             component to report. Steps: {:?}",
            what,
            abi::error_code(result),
            result,
            StepDump
        );
    }
}

// ---- Criterion 7: the line belongs to a component ----------------------

/// Check that IRQ 1 is owned by a user component and masked by the
/// kernel's rules rather than by the kernel's choice.
///
/// **Read from the kernel's own tables**, not from anything a
/// component said. A component claiming to own a line and a kernel
/// agreeing are two different facts, and only the second is
/// checkable here.
fn check_irq_ownership() {
    if !crate::cap::irq::is_owned(1) {
        panic!(
            "IRQ 1 has no owner. The keyboard component is supposed \
             to hold an IRQHandler for it; without one, every \
             interrupt the device raises is counted as orphaned and \
             dropped (ADR 042)."
        );
    }

    let delivered = crate::cap::irq::delivered();
    if delivered == 0 {
        panic!(
            "no interrupt was ever delivered to a Notification. The \
             keys the harness injected did not reach the i8042, or \
             the line was never unmasked. A run that passed its \
             other checks without this would be reading keys by \
             polling, which is not what was built."
        );
    }

    let unbound = crate::cap::irq::unbound();
    if unbound != 0 {
        panic!(
            "{} interrupt(s) arrived for a line whose owner had not \
             called SetNotification. The driver unmasked before it \
             was ready to receive.",
            unbound
        );
    }

    println!(
        "[kbd] IRQ 1 is owned by a user component; {} interrupt(s) \
         delivered, {} acked, 0 orphaned",
        delivered,
        crate::cap::irq::acks()
    );

    check_line_routing();
}

/// Check that the chip itself points IRQ 1 at the right vector.
///
/// **Read back from the I/O APIC**, not from the kernel's record of
/// what it meant to write. The distinction is the whole of this
/// check: M15 found a kernel that had never written a vector at
/// all, and `ioapic::verify` reported every line in agreement
/// because the only field it read was the one field that *was*
/// being written (ADR 023, K12).
///
/// Delivered interrupts above already prove the line works. This
/// proves *why*, which is what makes the next failure diagnosable:
/// a run where nothing arrives can now distinguish "the line is
/// masked", "the line points at the wrong vector" and "the device
/// said nothing".
fn check_line_routing() {
    let guard = sched::InterruptGuard::new();
    // SAFETY: Interrupts are disabled by the guard, which is what
    // reading the chip's select/data pair requires.
    let routing = unsafe { crate::ioapic::routing_of(1) };
    // SAFETY: As above — the guard holds interrupts off for both
    // calls, which is what reading the chip's select/data pair
    // requires.
    let disagreements = unsafe { crate::ioapic::verify() };
    drop(guard);

    let Some((vector, masked)) = routing else {
        // No I/O APIC. The interrupts counted above still arrived,
        // so something delivered them; saying so is better than
        // asserting about a chip that is not there.
        println!(
            "[kbd] no I/O APIC on this machine, so the line's routing \
             cannot be read back; the interrupts above arrived by some \
             other path"
        );
        return;
    };

    let expected = crate::apic::IRQ_VECTOR_BASE + 1;
    if vector != expected {
        panic!(
            "the I/O APIC has IRQ 1 pointed at vector {}, expected {}. \
             Interrupts arrived anyway, which means they came from \
             somewhere other than this line -- and the scenario is \
             about this line.",
            vector, expected
        );
    }

    if masked {
        panic!(
            "the I/O APIC still has IRQ 1 masked, yet interrupts were \
             counted. Something is delivering on a line the chip says \
             is shut."
        );
    }

    if disagreements != 0 {
        panic!(
            "{} redirection entry/entries disagree with what the \
             kernel wrote. ioapic::verify names each on the console \
             above.",
            disagreements
        );
    }

    println!(
        "[kbd] the I/O APIC has IRQ 1 on vector {}, unmasked, fixed \
         delivery to LAPIC 0, edge triggered and active high -- read \
         back from the chip, not from what the kernel meant to write",
        vector
    );
    println!(
        "[kbd] {} routing write(s) and {} mask write(s) reached the \
         chip this boot",
        crate::ioapic::route_writes(),
        crate::ioapic::mask_writes()
    );
}

// ---- What the components said went wrong -------------------------------

/// Check that neither component reported an error of its own.
///
/// **Both components have an error step and until this existed
/// nothing read either one.** A keyboard that failed to bind its
/// Notification, or a console whose `Recv` returned a bad badge,
/// reported it faithfully into a slot no test looked at — and the
/// run would then fail somewhere downstream, on a symptom, with the
/// cause sitting in a register the panic message does not print.
///
/// That is ADR 023's K5 from the other side: not a check that cannot
/// fail, but a *report* that cannot be heard. Both are a claim
/// indistinguishable from its own absence.
///
/// Called by every scenario that expects the components to work.
/// The two negatives that expect an error — `kbd-no-cap`, whose
/// keyboard is refused its ports, and `kbd-isolation`, where both
/// components provoke one deliberately — read their own steps
/// directly and do not call this.
fn check_no_component_errors() {
    let keyboard = kbd(KBD_ERROR);
    if keyboard != NOT_REPORTED {
        panic!(
            "the keyboard component reported error {:#x} \
             ({}). It says so on its own error step, which means it \
             reached a failure it could name rather than dying. \
             Steps: {:?}",
            keyboard,
            abi::error_code(keyboard),
            StepDump
        );
    }

    let console = con(CON_ERROR);
    if console != NOT_REPORTED {
        panic!(
            "the console component reported error {:#x} ({}). \
             Steps: {:?}",
            console,
            abi::error_code(console),
            StepDump
        );
    }
}

/// Report what the two components counted, and cross-check the
/// counts that have to agree.
///
/// **The keyboard's wakeup count and the kernel's delivery count are
/// two different facts**, and comparing them is the only way to see
/// a Notification that was signalled but never waited on. The kernel
/// knows how many interrupts it delivered to a Notification; the
/// component knows how many times `Wait` returned. A driver that
/// missed wakeups — because it was still draining when the next
/// interrupt arrived, which is the i8042 behaviour ADR 053's
/// drain loop exists for — shows up here as a gap and nowhere else.
///
/// A gap is **reported, not asserted**: fewer wakeups than
/// interrupts is the correct outcome when one wakeup drained several
/// bytes, and that is the design rather than a fault.
fn report_component_counts() {
    let wakeups = kbd(KBD_IRQ);
    let delivered = crate::cap::irq::delivered();

    if wakeups != NOT_REPORTED {
        if wakeups > delivered {
            panic!(
                "the keyboard woke {} time(s) but the kernel \
                 delivered only {} interrupt(s) to its Notification. \
                 A component cannot wake more often than it was \
                 signalled; either the count is wrong or something \
                 else is signalling that Notification.",
                wakeups, delivered
            );
        }

        println!(
            "[kbd] the keyboard woke {} time(s) for {} interrupt(s) \
             delivered -- one wakeup drains until the output buffer \
             is empty, so these need not be equal (ADR 053)",
            wakeups, delivered
        );
    }

    let sent = kbd(KBD_DONE);
    let received = con(CON_KEYS);
    if sent != NOT_REPORTED && received != NOT_REPORTED {
        // Over an Endpoint, which is a rendezvous: every `Send` that
        // returned had a receiver take it. They can differ by one if
        // the run was cut between the two reports, and not by more.
        if sent > received + 1 {
            panic!(
                "the keyboard sent {} key(s) and the console received \
                 {}. A rendezvous cannot lose messages (ADR 033), so \
                 the gap is either a report that did not arrive or a \
                 send that blocked forever.",
                sent, received
            );
        }
        println!(
            "[kbd] the keyboard sent {} key(s); the console received {}",
            sent, received
        );
    }

    let last_byte = kbd(KBD_BYTE);
    let last_key = kbd(KBD_KEY);
    if last_byte != NOT_REPORTED && last_key != NOT_REPORTED {
        println!(
            "[kbd] the last byte off the wire was {:#04x} and became \
             {:?}",
            last_byte, last_key as u8 as char
        );
    }
}

// ---- Criterion 8: the measurement --------------------------------------

/// Report the key-to-pixel latency.
///
/// From the kernel's TSC reading at interrupt delivery to the
/// console's reading immediately after the character reached the
/// framebuffer. The whole path: IRQ 1 → the driver waking → two
/// IPCs → the console drawing → a pixel.
///
/// **Under emulation**, and the caveat is the same one ADR 051
/// attached to the framebuffer fill: QEMU's TCG does not model the
/// bus, the write-combining buffer or the interrupt controller's
/// real latency. The figure is a baseline to compare against on
/// hardware, not a cost.
fn report_latency() {
    let pixels = PIXEL_TSC_COUNT
        .load(Ordering::Acquire)
        .min(PIXEL_TSC.len() as u64);
    let interrupts = IRQ_TSC_COUNT
        .load(Ordering::Acquire)
        .min(IRQ_TSC.len() as u64);

    if pixels == 0 || interrupts == 0 {
        println!(
            "[kbd] no latency samples: no interrupt and pixel pair \
             was recorded"
        );
        return;
    }

    let mut total = 0u64;
    let mut worst = 0u64;
    let mut best = u64::MAX;
    let mut counted = 0u64;

    // **Paired by time, not by index**, and the first version of
    // this paired by index and reported a figure two orders of
    // magnitude too large: 214 million cycles, which is about
    // thirteen seconds of wall clock for a keystroke.
    //
    // There is no one-to-one correspondence to index into. A
    // shifted letter is four scan codes — two makes and two breaks
    // — and the i8042 raises an interrupt for each, while the
    // console draws one character. The run that exposed this saw
    // **18 interrupts for 7 characters**. Pairing IRQ[3] with
    // PIXEL[3] measured from an interrupt belonging to a different
    // key, so most of what it reported was the gap between
    // keystrokes — which the test harness controls, not the system.
    //
    // The pairing that means something: for each pixel, the **last
    // interrupt before it**. That is the byte whose arrival
    // produced the character, because the driver drains the output
    // buffer and sends what it decoded before going back to `Wait`.
    //
    // A measurement whose definition nobody checks is ADR 023's K5
    // in its quietest form: it cannot fail, so it proves nothing.
    for slot in PIXEL_TSC.iter().take(pixels as usize) {
        let pixel = slot.load(Ordering::Acquire);
        if pixel == 0 {
            continue;
        }

        let mut irq = 0u64;
        for slot in IRQ_TSC.iter().take(interrupts as usize) {
            let candidate = slot.load(Ordering::Acquire);
            if candidate != 0 && candidate < pixel && candidate > irq {
                irq = candidate;
            }
        }

        if irq == 0 {
            // A character with no interrupt before it, which the
            // real path cannot produce. Skipped rather than counted
            // as zero, which would drag the mean down and hide it.
            continue;
        }

        let delta = pixel - irq;
        total += delta;
        worst = worst.max(delta);
        best = best.min(delta);
        counted += 1;
    }

    if counted == 0 {
        println!("[kbd] no usable latency samples: no pixel followed its interrupt");
        return;
    }

    println!(
        "[kbd] key-to-pixel latency over {} samples: {} cycles mean, \
         {} best, {} worst (IRQ 1 -> driver -> IPC -> console -> \
         pixel)",
        counted,
        total / counted,
        best,
        worst
    );
    println!(
        "[kbd] the figure is an emulated baseline, not a cost: QEMU's \
         TCG does not model interrupt delivery latency or the \
         framebuffer's bus (ADR 051's caveat, same reason)"
    );
}

/// Report the scroll cost.
fn report_scroll_cost() {
    let cycles = con(CON_SCROLL_CYCLES);
    let cells = con(CON_SCROLL_CELLS);

    if cycles == NOT_REPORTED {
        return;
    }

    println!(
        "[kbd] one scroll: {} cycles, {} cells repainted (a character \
         grid in RAM, so only changed cells reach the glass -- \
         ADR 054)",
        cycles, cells
    );
    println!(
        "[kbd] the saving against a full-screen blit is the cell \
         count, not the cycle count: under emulation the bus is not \
         modelled, so hardware is where the two diverge \
         (hardware-todo.md item 12)"
    );
}

// ---- The scenarios -----------------------------------------------------

/// `kbd-test`: the positive scenario.
pub fn run() {
    println!("[kbd] waiting for the console to map the framebuffer");

    if !wait_for_console(CON_READY) {
        panic!(
            "the console never reported itself ready. It maps a \
             thousand framebuffer pages one invocation at a time \
             before saying so; if it did not get there, the geometry \
             or the device Untypeds are wrong. Steps: {:?}",
            StepDump
        );
    }

    let geometry = con(CON_GEOMETRY);
    let grid = con(CON_GRID);
    println!(
        "[kbd] console: {}x{} pixels, {}x{} characters",
        geometry & 0xFFFF,
        (geometry >> 16) & 0xFFFF,
        grid & 0xFFFF_FFFF,
        grid >> 32
    );

    // **How many pages it actually mapped**, which is the one number
    // that distinguishes "the console mapped the screen" from "the
    // console mapped the first page and reported ready". A console
    // that mapped one page would draw the first few rows and fault
    // on the rest; checking the count catches it here instead.
    let mapped = con(CON_MAPPED);
    if mapped == NOT_REPORTED || mapped == 0 {
        panic!(
            "the console reported itself ready without mapping any \
             framebuffer pages. Steps: {:?}",
            StepDump
        );
    }

    // The console packs its geometry into one word: width in the low
    // 16, height in the next 16, pitch in the 24 above that and bpp
    // in the top 8. The pitch mask is 24 bits and not 32 — the top
    // byte is the bpp, and masking it in would make the page count
    // enormous and the check below fire on a healthy run.
    let width = geometry & 0xFFFF;
    let height = (geometry >> 16) & 0xFFFF;
    let pitch = (geometry >> 32) & 0xFF_FFFF;
    let bpp = geometry >> 56;
    let needed = if pitch != 0 {
        (pitch * height).div_ceil(4096)
    } else {
        0
    };

    if needed != 0 && mapped < needed {
        panic!(
            "the console mapped {} page(s) for a {}x{} framebuffer \
             with pitch {}, which needs {}. The rows past the mapping \
             are not on the screen and writing to them would fault.",
            mapped, width, height, pitch, needed
        );
    }

    println!(
        "[kbd] the console mapped {} framebuffer page(s) for \
         {}x{}x{} pitch {}, one invocation each, holding no ordinary \
         Untyped",
        mapped, width, height, bpp, pitch
    );

    if kbd(KBD_READY) == NOT_REPORTED {
        let deadline = read_tsc() + DEADLINE;
        while kbd(KBD_READY) == NOT_REPORTED && read_tsc() < deadline {
            sched::yield_now();
        }
    }

    if kbd(KBD_READY) == NOT_REPORTED {
        panic!(
            "the keyboard component never claimed its line. Steps: {:?}",
            StepDump
        );
    }

    let probe = kbd(KBD_PROBE);
    if probe == 0xFF {
        panic!(
            "the i8042's status register read back 0xFF, which is a \
             floating bus: there is no keyboard controller on this \
             machine. The scenario cannot test what is not there \
             (hardware-todo.md item 11)."
        );
    }

    println!("[kbd] the keyboard is ready; the harness may inject keys now");

    // The harness is watching for that line. Everything below waits
    // for the consequences.
    if !wait_for_characters(EXPECTED.len() as u64 + 1) {
        panic!(
            "only {} characters reached the glass within the \
             deadline, {} expected. Steps: {:?}",
            DRAWN_COUNT.load(Ordering::Acquire),
            EXPECTED.len() + 1,
            StepDump
        );
    }

    check_no_component_errors();
    check_characters();
    check_make_break();
    check_extended();
    check_pixels();
    check_irq_ownership();
    report_component_counts();

    let unrecognised = kbd(KBD_UNRECOGNISED);
    if unrecognised != NOT_REPORTED && unrecognised > 0 {
        println!(
            "[kbd] {} byte(s) the decoder did not recognise -- the \
             firmware may have left the controller in a different \
             scan code set (hardware-todo.md item 11)",
            unrecognised
        );
    }

    report_latency();

    if KBD_PANICKED.load(Ordering::Acquire) || CON_PANICKED.load(Ordering::Acquire) {
        panic!("a component's panic handler ran. Steps: {:?}", StepDump);
    }

    println!("[boot] M15 kbd-test checks passed");
    qemu::exit(qemu::EXIT_SUCCESS);
}

/// `kbd-scroll`: fill the screen and push it past the last row.
pub fn run_scroll() {
    if !wait_for_console(CON_READY) {
        panic!(
            "the console never reported itself ready. Steps: {:?}",
            StepDump
        );
    }

    println!("[kbd] the console is ready; the harness may fill the screen now");

    // **The acknowledgements have to be printed while waiting**, not
    // after. The harness sends key N+1 only once it has seen the
    // line for key N, so a loop that waits for the scroll without
    // announcing anything deadlocks: the console cannot scroll
    // without keys, and no more keys arrive without the lines.
    //
    // The first version of this waited silently and the run died
    // with exactly one key delivered.
    let deadline = read_tsc() + DEADLINE;
    let mut announced = 0u64;
    while con(CON_SCROLLED) == NOT_REPORTED && read_tsc() < deadline {
        announce_keys(&mut announced);
        sched::yield_now();
    }
    announce_keys(&mut announced);

    check_no_component_errors();
    check_scroll();
    report_scroll_cost();

    println!("[boot] M15 kbd-scroll checks passed");
    qemu::exit(qemu::EXIT_SUCCESS);
}

/// `kbd-no-cap`: a keyboard component with no port authority.
pub fn run_no_cap() {
    let deadline = read_tsc() + DEADLINE;
    while kbd(KBD_SELFTEST) == NOT_REPORTED && read_tsc() < deadline {
        check_root_task_setup();
        sched::yield_now();
    }

    // **The decoder still works.** This is what separates "the
    // component was refused the hardware" from "the component is
    // broken", and without it the scenario would pass for either
    // reason.
    let selftest = kbd(KBD_SELFTEST);
    if selftest == NOT_REPORTED {
        panic!(
            "the keyboard component never ran its self-test. It needs \
             no capability at all, so its absence means the component \
             did not start -- which is not what this scenario is \
             about. Steps: {:?}",
            StepDump
        );
    }
    if selftest & 0xFFFF_FFFF != 0 {
        panic!(
            "the decoder failed its self-test in the no-capability \
             scenario. The component is broken, which would make the \
             refusal below unattributable."
        );
    }

    // The probe: an `In8` on a slot holding nothing.
    let deadline = read_tsc() + DEADLINE;
    while kbd(KBD_PROBE) == NOT_REPORTED && read_tsc() < deadline {
        sched::yield_now();
    }

    let probe = kbd(KBD_PROBE);
    if probe == NOT_REPORTED {
        panic!(
            "the component never reported its probe of the status \
             register. Steps: {:?}",
            StepDump
        );
    }

    if !syscall::is_error(probe) {
        panic!(
            "the component read {:#x} from the i8042's status \
             register while holding no IOPort capability. A \
             component reached a device it was never given.",
            probe
        );
    }

    let code = abi::error_code(probe);
    if code != abi::error::EMPTY_SLOT {
        panic!(
            "the refusal was error {} where EmptySlot ({}) was \
             expected. The distinction matters: EmptySlot means the \
             component could not *name* the capability (ADR 018), \
             which is isolation by construction. Any other code means \
             something was there and declined.",
            code,
            abi::error::EMPTY_SLOT
        );
    }

    // And the kernel's side: nobody owns line 1.
    if crate::cap::irq::is_owned(1) {
        panic!(
            "IRQ 1 has an owner in a scenario where no IRQHandler was \
             issued."
        );
    }

    println!(
        "[kbd] a keyboard component with no IOPort was refused with \
         EmptySlot: it could not name the capability, rather than \
         being told no"
    );
    println!("[kbd] its decoder passed its self-test, so the refusal is the hardware's absence and not the component's");
    println!("[boot] M15 kbd-no-cap checks passed");
    qemu::exit(qemu::EXIT_SUCCESS);
}

/// `kbd-driver-dies`: the keyboard is killed; the console lives.
pub fn run_driver_dies() {
    if !wait_for_console(CON_READY) {
        panic!(
            "the console never reported itself ready. Steps: {:?}",
            StepDump
        );
    }

    // The root task kills the keyboard right after starting it. What
    // this checks is the consequence.
    let deadline = read_tsc() + DEADLINE;
    while crate::cap::irq::is_owned(1) && read_tsc() < deadline {
        sched::yield_now();
    }

    if crate::cap::irq::is_owned(1) {
        panic!(
            "IRQ 1 still has an owner after the keyboard's \
             capabilities were revoked. A dead component is still \
             holding a line, which means nothing else can ever claim \
             it."
        );
    }

    // **The line must be masked, not merely unowned.** An unowned
    // unmasked line delivers interrupts to nobody, and every one of
    // them is work the kernel does for a component that is gone.
    if !crate::cap::irq::is_masked(1) {
        panic!(
            "IRQ 1 is unowned and unmasked after the driver died. \
             Revoking an IRQHandler masks the line before clearing \
             its owner (ADR 042); if it did not, a dead driver's \
             device could interrupt forever."
        );
    }

    if CON_PANICKED.load(Ordering::Acquire) {
        panic!(
            "the console panicked when the keyboard died. The \
             Endpoint they shared belongs to the root task precisely \
             so that this does not happen."
        );
    }

    // The console is still waiting, which is the point: it holds a
    // live Endpoint with no sender, which is an ordinary state.
    if con(CON_ERROR) != NOT_REPORTED {
        panic!(
            "the console reported error {:#x} after the keyboard \
             died. It should simply still be waiting.",
            con(CON_ERROR)
        );
    }

    // **And it has not finished.** The console echoes its done step
    // when it leaves its loop; still waiting means that step never
    // arrived.
    //
    // This is the difference between "the console survived" and "the
    // console survived and is still a console". A `Recv` that
    // returned an error would fall out of the loop, report done, and
    // satisfy every check above — the component would be alive,
    // unpanicked, errorless, and no longer serving anybody.
    if con(CON_DONE) != NOT_REPORTED {
        panic!(
            "the console left its serve loop after {} key(s) when the \
             keyboard died. It is supposed to still be blocked in \
             Recv: an Endpoint whose only sender has gone is an \
             ordinary Endpoint with nobody sending, not a broken one \
             (ADR 039).",
            con(CON_DONE)
        );
    }

    println!(
        "[kbd] the keyboard's capabilities were revoked: IRQ 1 is \
         unowned and masked, and the console is still waiting on an \
         Endpoint that outlived its sender"
    );
    println!("[boot] M15 kbd-driver-dies checks passed");
    qemu::exit(qemu::EXIT_SUCCESS);
}

/// `kbd-flood`: keys faster than the console consumes them.
pub fn run_flood() {
    if !wait_for_console(CON_READY) {
        panic!(
            "the console never reported itself ready. Steps: {:?}",
            StepDump
        );
    }

    println!("[kbd] the console is ready; the harness may flood now");

    // Wait for the flood to produce something.
    let deadline = read_tsc() + DEADLINE;
    while DRAWN_COUNT.load(Ordering::Acquire) == 0 && read_tsc() < deadline {
        sched::yield_now();
    }

    // Let it run.
    //
    // **No per-key announcements here, and that is the scenario.**
    // Everywhere else the harness waits for the console to take a
    // key before sending the next; this one deliberately does not,
    // because what it tests is what happens when keys arrive faster
    // than they are consumed. Pacing the flood would remove the
    // flood.
    let settle = read_tsc() + DEADLINE / 3;
    while read_tsc() < settle {
        sched::yield_now();
    }

    let drawn = DRAWN_COUNT.load(Ordering::Acquire);
    let overruns = kbd(KBD_OVERRUN);
    let dropped = kbd(KBD_DROPPED);

    if drawn == 0 {
        panic!(
            "the flood produced no characters at all. Steps: {:?}",
            StepDump
        );
    }

    // **The driver's own batch must never have overflowed.**
    //
    // ADR 053 specified a 32-entry ring with a drop-oldest policy
    // and the code found it unreachable: `Send` blocks, so the
    // driver cannot accumulate faster than it emits. The batch is
    // bounded by DRAIN_LIMIT and sized to it, so this counter is
    // structurally zero.
    //
    // Asserted rather than assumed, because "unreachable" claims
    // that nothing checks are how ADR 023's K10 happens.
    if dropped != NOT_REPORTED && dropped > 0 {
        panic!(
            "the keyboard dropped {} event(s) from its batch. That \
             should be impossible: the batch is as long as the drain \
             limit, so every byte read has a slot. Either DRAIN_LIMIT \
             and BATCH have diverged, or the loop reads more than it \
             is bounded to.",
            dropped
        );
    }

    // What *should* overflow is the device's queue, and under
    // emulation it may not: QEMU delivers key events as fast as the
    // guest drains them, so whether the i8042's 16-byte buffer ever
    // fills depends on scheduling.
    //
    // So this is reported rather than asserted. Asserting it would
    // make the scenario fail on a fast machine, which would be
    // testing the host.
    if overruns != NOT_REPORTED && overruns > 0 {
        println!(
            "[kbd] the device's queue overran {} time(s) under the \
             flood: the bound that binds is the i8042's 16-byte \
             buffer, and it was reached",
            overruns
        );
    } else {
        println!(
            "[kbd] the device's queue did not overrun: QEMU delivered \
             keys no faster than the driver drained them. The bound \
             exists and was not reached, which this run cannot \
             distinguish from a bound that does not work -- it is the \
             emulator's pacing, not the driver's"
        );
    }

    println!(
        "[kbd] {} characters drawn under the flood; the driver's \
         batch never overflowed, which is structural: Send blocks, so \
         it cannot accumulate (ADR 053)",
        drawn
    );

    if KBD_PANICKED.load(Ordering::Acquire) || CON_PANICKED.load(Ordering::Acquire) {
        panic!(
            "a component panicked under the flood. Steps: {:?}",
            StepDump
        );
    }

    println!("[boot] M15 kbd-flood checks passed");
    qemu::exit(qemu::EXIT_SUCCESS);
}

/// `kbd-isolation`: neither component can reach the other's device.
pub fn run_isolation() {
    // **The prediction first, before anything can fault.**
    //
    // It used to be stored after the console's half had been
    // checked, and that was a race this scenario lost as soon as it
    // ran alongside the others: the keyboard reaches its faulting
    // read at its own speed, and if it gets there before this line
    // the #PF handler finds no prediction, declines the fault, and
    // the kernel panics with "unhandled exception" — at exactly the
    // address the scenario was waiting to see.
    //
    // The isolated run passed and `make test` failed, which is the
    // worst shape a race can take: it looked like the full run had
    // broken something.
    //
    // There is nothing to wait for anyway. The address is an ABI
    // constant, not a result — storing it early costs nothing and
    // removes the window entirely (ADR 023, K8: wait for events,
    // and do not create orderings that are not needed).
    PREDICTED_FAULT.store(abi::FRAMEBUFFER_BASE, Ordering::Release);

    println!(
        "[kbd] expecting the keyboard component to fault at {:#018x}",
        abi::FRAMEBUFFER_BASE
    );

    // The console's half: it names an IOPort it does not hold.
    let deadline = read_tsc() + DEADLINE;
    while con(CON_ISOLATION) == NOT_REPORTED && read_tsc() < deadline {
        check_root_task_setup();
        sched::yield_now();
    }

    let attempt = con(CON_ISOLATION);
    if attempt == NOT_REPORTED {
        panic!(
            "the console never reported its attempt on the keyboard's \
             port. Steps: {:?}",
            StepDump
        );
    }

    if !syscall::is_error(attempt) {
        panic!(
            "the console read {:#x} from port 0x60. It holds no \
             IOPort capability; a component reached a device it was \
             never given.",
            attempt
        );
    }

    let code = abi::error_code(attempt);
    if code != abi::error::EMPTY_SLOT {
        panic!(
            "the console's attempt returned error {} where EmptySlot \
             ({}) was expected.",
            code,
            abi::error::EMPTY_SLOT
        );
    }

    println!(
        "[kbd] the console cannot drive the keyboard: naming an \
         IOPort it does not hold returns EmptySlot"
    );

    // The keyboard's half: it reads the framebuffer's address and
    // faults. Predicted at the top of this function, checked in the
    // #PF handler.

    // **The address the keyboard says it is about to read.** It
    // echoes this immediately before the faulting instruction, and
    // the two have to agree: the prediction above is what the
    // *kernel* computed from the ABI constant; this is what the
    // *component* is going to dereference.
    //
    // Without it, a keyboard aiming somewhere else entirely would
    // still fault, the handler would compare the address against the
    // prediction, and the run would fail with "the fault is not the
    // one the scenario is about" -- true, and silent about the
    // component having been pointed elsewhere from the start.
    //
    // Waited for with a short deadline rather than read once: `Echo`
    // returns straight to ring 3 (ADR 048), so this report and the
    // fault are adjacent instructions and which the kernel sees
    // first is a scheduling detail. Absence is not an error here --
    // the fault may simply have won the race, and the handler checks
    // the address that matters.
    let deadline = read_tsc() + DEADLINE / 10;
    while kbd(KBD_ISOLATION) == NOT_REPORTED && read_tsc() < deadline {
        sched::yield_now();
    }

    let aiming_at = kbd(KBD_ISOLATION);
    if aiming_at != NOT_REPORTED && aiming_at != abi::FRAMEBUFFER_BASE {
        panic!(
            "the keyboard component announced it would read \
             {:#018x}, but the scenario is about {:#018x}. The two \
             halves of this test disagree about which address the \
             console's framebuffer is at, so whatever faults next \
             proves nothing.",
            aiming_at,
            abi::FRAMEBUFFER_BASE
        );
    }

    // Waiting rather than exiting: if the read *succeeded*, no fault
    // arrives, and the deadline turns that into a failure with a
    // message. An immediate exit would make the scenario pass
    // whether or not the isolation held.
    let deadline = read_tsc() + DEADLINE;
    while read_tsc() < deadline {
        sched::yield_now();
    }

    panic!(
        "the keyboard component read the framebuffer's address at \
         {:#018x} and no fault arrived. Either something mapped that \
         page into its address space, or the two components are \
         sharing one -- in which case the isolation this milestone is \
         about does not exist.",
        abi::FRAMEBUFFER_BASE
    );
}
