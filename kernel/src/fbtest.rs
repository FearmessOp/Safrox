// SPDX-License-Identifier: BSD-2-Clause
//! M14 acceptance: the framebuffer, driven from a component.
//!
//! **What this milestone did not need is the point of it.** M13
//! moved the serial port out of the kernel and had to invent a
//! capability to do it — ADR 045's IOPort, because x86 ports live in
//! a 16-bit space no page table describes. A framebuffer is memory,
//! so the existing machinery suffices: a device Untyped becomes
//! Frames (ADR 017, M5), the Frames are mapped by their owner
//! (ADR 043, M11), and the component writes.
//!
//! The one thing that was missing was a device Untyped that named a
//! real device. ADR 017 wrote that boot would create them in M5 and
//! nothing did until now (ADR 050).
//!
//! Eight criteria:
//!
//! 1. the geometry is reported and matches what Limine said;
//! 2. the component runs at CPL 3 and maps the framebuffer itself —
//!    and the **kernel's own CR3 does not map it**;
//! 3. it fills the screen and draws three rectangles;
//! 4. it reads the pixels back and the values are right;
//! 5. the pitch is used rather than `width * bpp / 8`;
//! 6. a full-screen fill is timed;
//! 7. the previous 63 scenarios still pass;
//! 8. exit 33.
//!
//! # Why criterion 4 is the whole test
//!
//! Nothing here can look at a screen. "The drawing happened" has to
//! mean something checkable, and the only checkable thing is that
//! the bytes are where they should be — which the component reads
//! back itself, because it is the only thing that has the
//! framebuffer mapped.
//!
//! That last clause is criterion 2 restated. If the kernel could
//! verify the pixels, the kernel would have the framebuffer mapped,
//! and the milestone's claim would be false.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use safrox_abi as abi;

use crate::memory::bootinfo;
use crate::println;
use crate::qemu;
use crate::sched;
use crate::syscall;

// ---- Channels ----------------------------------------------------------

const SCREEN_CHANNEL: u64 = abi::channel::SCREEN;

const STEP_COUNT: usize = 64;

/// Steps the **screen component** reports.
const SCR_GEOMETRY: u64 = 1;
const SCR_MAPPED: u64 = 2;
const SCR_FILLED: u64 = 3;
const SCR_RECTS: u64 = 4;
const SCR_VERIFIED: u64 = 5;
const SCR_ERROR: u64 = 6;
const SCR_FILL_CYCLES_WC: u64 = 7;
// There is deliberately no `SCR_OOB`.
//
// The component used to echo the address it was about to fault on,
// and this constant read it back. The report could never arrive:
// the echo and the store have no scheduling point between them, so
// no kernel thread runs again before the fault.
//
// The address is computed in `run_out_of_bounds` instead, from the
// geometry and `abi::FRAMEBUFFER_BASE`, and checked against CR2 in
// the #PF handler. That is the stronger arrangement besides -- the
// prediction and the observation now come from different places,
// where an echo would only have confirmed that the component can
// do arithmetic.
const SCR_MISUSE: u64 = 10;
const SCR_PITCH: u64 = 11;
/// A component with no framebuffer capability tried to map it.
const SCR_NO_CAP: u64 = 46;
/// The component's panic handler ran.
const SCR_PANIC: u64 = abi::channel::PANIC_STEP;

const NOT_REPORTED: u64 = u64::MAX;

/// Where `fb-out-of-bounds` expects the faulting store to land.
///
/// Zero when no scenario has armed a prediction, which is every
/// scenario but one.
static PREDICTED_FAULT: AtomicU64 = AtomicU64::new(0);

/// Check CR2 against a prediction, from inside the #PF handler.
///
/// # Why the check lives here and not in the scenario
///
/// `fb-out-of-bounds` claims the fault lands **exactly** one page
/// past the mapping. Verifying that needs both the predicted address
/// and CR2, and no scenario thread ever holds both: the component
/// reports and then stores with no syscall between, so the waiting
/// kernel thread is not scheduled again before the fault, and the
/// fault stops the machine.
///
/// Without this the scenario would accept **any** user write fault.
/// A component that faulted on its own stack would pass, and the
/// claim — that a device Untyped's size is a real boundary — would
/// be untested while looking tested (ADR 023, K5).
///
/// Does nothing unless armed, which is every other scenario.
pub fn check_predicted_fault(cr2: u64) {
    let expected = PREDICTED_FAULT.load(Ordering::Acquire);
    if expected == 0 {
        return;
    }

    if cr2 == expected {
        println!(
            "[fb] the fault landed at {:#018x}, exactly where the \
             mapping ends: the kernel predicted it from the device \
             Untyped's size before the component ran, and the CPU \
             agrees. The boundary is real.",
            cr2
        );
        return;
    }

    println!(
        "[fb] WRONG ADDRESS: the component faulted at {:#018x} but \
         the mapping ends at {:#018x}. The scenario's claim is about \
         the page after the framebuffer; a fault somewhere else means \
         the component broke for an unrelated reason and the boundary \
         was never tested.",
        cr2, expected
    );
}

static STEPS: [AtomicU64; STEP_COUNT] = [const { AtomicU64::new(NOT_REPORTED) }; STEP_COUNT];
static SCREEN_PANICKED: AtomicBool = AtomicBool::new(false);

/// Record a step, from the dispatcher's `Echo` path.
///
/// Errors are sticky, for the reason M11 established and every
/// milestone since has repeated: several invocations share a step
/// and "last write wins" lets a failure be hidden by what follows.
pub fn record_step(channel: u64, value: u64) -> bool {
    if channel & SCREEN_CHANNEL == 0 {
        return false;
    }

    let index = (channel & 0xFF) as usize;
    if index == SCR_PANIC as usize {
        SCREEN_PANICKED.store(true, Ordering::Release);
        return true;
    }
    if index < STEP_COUNT {
        let previous = STEPS[index].load(Ordering::Acquire);
        if previous == NOT_REPORTED || !syscall::is_error(previous) {
            STEPS[index].store(value, Ordering::Release);
        }
    }
    true
}

fn step(which: u64) -> u64 {
    STEPS[which as usize].load(Ordering::Acquire)
}

struct StepDump;

impl core::fmt::Debug for StepDump {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (index, slot) in STEPS.iter().enumerate() {
            let value = slot.load(Ordering::Acquire);
            if value != NOT_REPORTED {
                write!(f, " {}={:#x}", index, value)?;
            }
        }
        Ok(())
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

const DEADLINE: u64 = 60_000_000_000;

/// Wait until a step is reported, or the deadline passes.
///
/// Waiting for an **event** rather than a duration (ADR 023, K8).
/// The deadline is twice M13's because this component maps a
/// thousand pages one invocation at a time before it reports
/// anything.
fn wait_for(which: u64) -> bool {
    let deadline = read_tsc() + DEADLINE;
    while step(which) == NOT_REPORTED && read_tsc() < deadline {
        sched::yield_now();
    }
    step(which) != NOT_REPORTED
}

// ---- Criterion 2: the kernel does not map the framebuffer --------------

/// Check that the framebuffer is **absent from the kernel's own
/// address space**.
///
/// # Why this is the criterion and not a comment
///
/// "The component drives the screen" is satisfied by a system where
/// the kernel also has it mapped and merely chooses not to write.
/// That system has not handed the device over; it has lent it.
///
/// The check is a translation in the kernel's own page tables, which
/// is the same question the CPU asks on every access — so a mapping
/// that existed could not hide from it.
///
/// Limine *does* map the framebuffer at boot, inside the HHDM, which
/// is how the kernel could print to it if it wanted. That mapping is
/// in the bootloader's tables; `vm::init` builds the kernel's own
/// and `memory/vm.rs` already excludes framebuffer regions from the
/// physical window (see its note on which memory-map kinds are
/// included). This asserts that exclusion rather than trusting it.
fn check_kernel_does_not_map_it(address: u64, size: u64) {
    let mut found = 0u64;
    let mut first = 0u64;

    // SAFETY: The kernel's own PML4, live for the whole of the
    // kernel's life. Borrowed once for the loop rather than per
    // page; nothing else holds a mutable borrow on this path and
    // the loop only reads.
    let table = unsafe { crate::memory::vm::kernel_table() };

    // **Every page, not a sample.** A kernel that mapped one page of
    // the framebuffer -- a panic console in the top-left corner,
    // say -- would pass a spot check of the middle, and that kernel
    // is precisely the one this criterion exists to rule out.
    //
    // Bounded by the framebuffer's size: no unbounded loop on a
    // kernel path.
    let mut offset = 0u64;
    while offset < size {
        let virt = crate::memory::VirtAddr(crate::memory::hhdm_offset() + address + offset);
        // SAFETY: `table` is the live kernel PML4 and `translate`
        // only walks it.
        //
        // `is_ok` rather than `is_some`: an `Err` here means the
        // address is not mapped, which is the answer this check
        // wants for every page.
        if unsafe { crate::memory::paging::translate(table, virt) }.is_ok() {
            if found == 0 {
                first = virt.0;
            }
            found += 1;
        }
        offset += 4096;
    }

    let _ = first;

    assert_eq!(
        found, 0,
        "the kernel's address space maps {} page(s) of the framebuffer \
         at {:#x}. A device the kernel can still reach has not been \
         handed over -- it has been lent, and 'the component drives the \
         screen' would be a statement about what the kernel chooses to \
         do rather than about what it can do.",
        found, address
    );

    println!(
        "[fb] the kernel's own address space does not map any of the \
         {} framebuffer page(s): the component holds the only mapping",
        size.div_ceil(4096)
    );
}

// ---- The positive scenario ---------------------------------------------

/// `fb-test`: the eight criteria.
pub fn run() -> ! {
    println!("[fb] the kernel has started the root task; the screen is a component");

    // ---- 1. The geometry ---------------------------------------------
    let Some(fb) = bootinfo::get().framebuffer() else {
        panic!(
            "this machine reported no framebuffer, so there is nothing \
             to hand to a component. The scenario cannot pass vacuously: \
             a run with no screen is a run that did not test anything, \
             and saying so is the honest outcome."
        );
    };

    println!(
        "[fb] framebuffer: {}x{} at {:#x}, pitch {} bytes, {} bpp, \
         {} KiB",
        fb.width,
        fb.height,
        fb.address,
        fb.pitch,
        fb.bpp,
        fb.size() / 1024
    );

    assert!(
        fb.width > 0 && fb.height > 0 && fb.pitch > 0 && fb.bpp > 0,
        "the framebuffer's geometry has a zero in it: {}x{}, pitch {}, \
         {} bpp. Every loop in the component is bounded by these, so a \
         zero makes 'it drew nothing' and 'it drew everything' the same \
         run.",
        fb.width,
        fb.height,
        fb.pitch,
        fb.bpp
    );

    assert!(
        fb.pitch >= fb.width * (fb.bpp as u64 / 8),
        "the pitch ({}) is smaller than one row of pixels ({} x {} / 8 \
         = {}). Rows would overlap.",
        fb.pitch,
        fb.width,
        fb.bpp,
        fb.width * (fb.bpp as u64 / 8)
    );

    // **And the component agrees.**
    //
    // The geometry reaches the component by a different route than
    // it reaches this test: the root task patches it into the
    // component's image (ADR 049's note on why), while this reads
    // the kernel's own copy. Two paths from one source, and a
    // mismatch would mean the patch landed at the wrong offset or
    // on the wrong fields.
    //
    // Without this the component could be drawing on a screen of
    // some other shape and every check below would still pass --
    // they would all be consistent with each other and wrong
    // together.
    assert!(
        wait_for(SCR_GEOMETRY),
        "the component never reported the geometry it was given. \
         Steps: {:?}",
        StepDump
    );

    let reported = step(SCR_GEOMETRY);
    let packed = fb.width | (fb.height << 16) | (fb.pitch << 32) | ((fb.bpp as u64) << 56);
    assert_eq!(
        reported, packed,
        "the component believes the screen is {:#018x} where the kernel \
         was told {:#018x} (width | height<<16 | pitch<<32 | bpp<<56). \
         The two come from one source by different routes -- the kernel \
         reads its own copy of Limine's answer, the component reads what \
         the root task patched into its image -- so a mismatch means the \
         patch went to the wrong place.",
        reported, packed
    );

    // ---- 2a. The component started and mapped it ----------------------
    assert!(
        wait_for(SCR_MAPPED),
        "the screen component never reported mapping the framebuffer. \
         It maps a thousand pages one invocation at a time, so a failure \
         partway reports an error rather than nothing -- check step {}. \
         Steps: {:?}",
        SCR_ERROR,
        StepDump
    );
    assert!(
        !SCREEN_PANICKED.load(Ordering::Acquire),
        "the screen component's panic handler ran. It cannot print -- it \
         holds no capability for the serial driver -- so this is all the \
         detail there is. Steps: {:?}",
        StepDump
    );

    let error = step(SCR_ERROR);
    assert_eq!(
        error,
        NOT_REPORTED,
        "the screen component reported error {:#018x} ({})",
        error,
        syscall::error_code(error)
    );

    let mapped = step(SCR_MAPPED);
    let expected = fb.size().div_ceil(4096);
    assert_eq!(
        mapped,
        expected,
        "the component mapped {} page(s); the framebuffer is {} bytes, \
         which is {} page(s) at the reported pitch. A short mapping \
         means part of the screen is unreachable and the drawing below \
         would be checking the part that worked.",
        mapped,
        fb.size(),
        expected
    );

    println!(
        "[fb] the screen component mapped {} page(s) of device memory \
         into its own address space, write-combining (ADR 051), through \
         {} invocation(s) the kernel validated one at a time",
        mapped, mapped
    );

    // ---- 2b. And the kernel does not -----------------------------------
    check_kernel_does_not_map_it(fb.address, fb.size());

    // ---- 3. It drew ----------------------------------------------------
    assert!(
        wait_for(SCR_FILLED),
        "the component mapped the framebuffer but never reported \
         filling it. Steps: {:?}",
        StepDump
    );
    assert!(
        wait_for(SCR_RECTS),
        "the screen was filled but no rectangles were drawn. Steps: {:?}",
        StepDump
    );

    let rects = step(SCR_RECTS);
    assert_eq!(
        rects, 3,
        "the component drew {} rectangle(s); the scenario specifies \
         three, in different colours and different places.",
        rects
    );

    // ---- 4. And read it back -------------------------------------------
    //
    // **The only evidence there can be.** No test here can look at a
    // screen, so "it drew" means "the bytes are where they should
    // be" -- checked by the component, because it holds the only
    // mapping, which is criterion 2 showing through.
    assert!(
        wait_for(SCR_VERIFIED),
        "the component drew but never reported verifying. The read-back \
         is the only evidence this test can have: nothing here can look \
         at a screen. Steps: {:?}",
        StepDump
    );

    let checked = step(SCR_VERIFIED);
    assert!(
        !syscall::is_error(checked),
        "the component's read-back failed: check {}, value {:#x}. The \
         pixels it wrote are not the pixels it found.",
        (checked >> 32) & 0xFF,
        checked & 0xFFFF_FFFF
    );

    // Four corners, plus five inside and up to four outside each of
    // three rectangles. The exact figure depends on how many
    // outside-pixels fall on screen, so the test asserts a floor
    // rather than an equality -- but a floor well above what a
    // vacuous run would produce.
    assert!(
        checked >= 20,
        "the component checked {} pixel(s), which is fewer than the \
         four corners and three rectangles it was asked to verify. A \
         verification that checked almost nothing would pass for the \
         same reason an empty loop does.",
        checked
    );

    println!(
        "[fb] the component read back {} pixel(s) and every one held \
         what it wrote: the four corners, five inside each rectangle, \
         and the pixels just outside each edge. The last group is what \
         distinguishes three rectangles from a screen painted one \
         colour.",
        checked
    );

    // ---- 5. The pitch ---------------------------------------------------
    report_pitch(&fb);

    // ---- 6. The measurement ---------------------------------------------
    report_fill_cost(&fb);

    println!("[fb] all framebuffer tests passed");

    syscall::report();

    println!("[boot] M14 checks passed");
    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        qemu::EXIT_SUCCESS,
        qemu::HOST_STATUS_SUCCESS
    );

    // ---- 8. Exit 33 -----------------------------------------------------
    qemu::exit(qemu::EXIT_SUCCESS)
}

/// Criterion 5: was the pitch used, and can this machine tell?
///
/// # The mutation that survives, reported rather than buried
///
/// The check that matters is whether row `y` starts at `y * pitch`
/// rather than at `y * width * bpp / 8`. **On QEMU those are the
/// same number** — the pitch here is 5120 and so is `1280 * 32 / 8`
/// — so a component that multiplied instead of using the pitch would
/// pass every scenario in this file.
///
/// That was verified by doing it: replacing `pitch` with
/// `width * bpp / 8` in `Geometry::offset` changes nothing on this
/// machine and `fb-test` still exits 33. **The mutation survives.**
///
/// ADR 023 says a surviving mutation is reported rather than
/// quietly left, so it is reported here, in the log, on every run.
/// The alternative — asserting that the two differ — would make the
/// scenario fail on the only hardware available, which is a test of
/// the emulator rather than of the kernel.
///
/// What *is* checked is that the component uses the field: it writes
/// a marker through `offset(0, 1)` and reads it back through the
/// same path, which fails if the arithmetic is inconsistent with
/// itself. That is weaker than the claim, and saying how much weaker
/// is the point of this function.
fn report_pitch(fb: &bootinfo::Framebuffer) {
    let packed = step(SCR_PITCH);
    assert_ne!(
        packed, NOT_REPORTED,
        "the component never reported its pitch check"
    );

    let marker = packed & 0xFFFF_FFFF;
    let padded = (packed >> 32) & 1 == 1;

    assert_eq!(
        marker, 0x00AB_CDEF,
        "the component wrote a marker at the start of row 1 and read \
         back {:#010x}. Its own row arithmetic is inconsistent: the \
         address it wrote to and the address it read from are the same \
         expression.",
        marker
    );

    let row_bytes = fb.width * (fb.bpp as u64 / 8);
    if padded {
        println!(
            "[fb] pitch {} bytes against {} bytes of visible pixels: this \
             machine pads its rows by {}, so using the pitch is load-bearing \
             and the read-back above would have failed without it",
            fb.pitch,
            row_bytes,
            fb.pitch - row_bytes
        );
    } else {
        println!(
            "[fb] pitch {} bytes equals width x bpp / 8 on this machine, \
             so criterion 5 cannot be falsified here. MUTATION SURVIVES: \
             replacing the pitch with width*bpp/8 in Geometry::offset \
             leaves every scenario passing. Reported rather than buried \
             (ADR 023): the claim 'the pitch is used' is checked only \
             for self-consistency, and real hardware that pads its rows \
             is what would test it. See docs/hardware-todo.md.",
            fb.pitch
        );
    }
}

/// Criterion 6: what a full-screen fill costs.
///
/// Two figures were asked for, write-combining and not. **Only one
/// is measured**, and the reason is in ADR 051: QEMU's TCG does not
/// model a write-combining buffer, so the second figure would differ
/// from the first by noise and inviting a comparison would be
/// inviting a wrong conclusion.
///
/// The measurement is kept because it shows the write-combining path
/// *works* — the mapping is made, the pixels are right — and because
/// it is a baseline for the day this runs on hardware.
fn report_fill_cost(fb: &bootinfo::Framebuffer) {
    let cycles = step(SCR_FILL_CYCLES_WC);
    assert_ne!(
        cycles, NOT_REPORTED,
        "the component never reported its fill time"
    );

    let pixels = fb.width * fb.height;
    assert!(
        cycles > pixels,
        "a {}-pixel fill took {} cycles, which is less than one cycle \
         per pixel. The loop did not run, or the clock did not move.",
        pixels,
        cycles
    );

    println!(
        "[fb] full-screen fill: {} cycles for {} pixels ({} cycles each), \
         write-combining",
        cycles,
        pixels,
        cycles / pixels
    );
    println!(
        "[fb] measured under emulation, and the figure is a floor rather \
         than a cost. QEMU's TCG does not model a write-combining buffer \
         -- WC's whole benefit is gathering stores into 64-byte bursts, \
         and here every store is a function call. A second figure \
         without WC would differ by noise, so it is not reported: an \
         invited comparison that cannot mean anything is worse than no \
         comparison (ADR 051)."
    );
}

// ---- Negative (a): a component with no capability ----------------------

/// `fb-no-cap`: a component tries to map a framebuffer it was not
/// given.
///
/// Expected: refused at CPtr resolution. Exit 33.
///
/// # Why this is worth a scenario when `driver-hostile` exists
///
/// M13 showed a component cannot reach a device it holds no
/// capability for, and the device was an I/O port — which is not in
/// any address space, so the result is unsurprising.
///
/// A framebuffer **is** memory, at a physical address anybody can
/// compute. The interesting question is whether knowing the address
/// is enough, and the answer is no: a Frame comes only from a
/// retype, a retype needs an Untyped, and the component holds none.
/// An address is a number; a capability is a reference (ADR 050).
pub fn run_no_cap() -> ! {
    println!("[fb] a component with no framebuffer capability will try to map it");

    assert!(
        wait_for(SCR_NO_CAP),
        "the component never reported its attempt. Steps: {:?}",
        StepDump
    );

    let result = step(SCR_NO_CAP);
    assert!(
        syscall::is_error(result),
        "a component holding no device Untyped retyped a Frame and \
         mapped the framebuffer. It returned {:#018x}.",
        result
    );

    let code = syscall::error_code(result);
    assert_eq!(
        code,
        abi::error::EMPTY_SLOT,
        "the attempt was refused with error {} where EmptySlot ({}) was \
         expected.\n\
         \n\
         The distinction is the scenario. A refusal from inside `retype` \
         would mean the kernel considered the request and declined it; \
         EmptySlot means there was nothing to consider. The component \
         could not *name* a capability it was never given (ADR 018), \
         which is isolation by construction rather than by enforcement \
         -- and the stronger property, because a later method cannot \
         forget to check it.",
        code,
        abi::error::EMPTY_SLOT
    );

    println!(
        "[fb] refused with EmptySlot: a framebuffer is memory at an \
         address anybody can compute, and knowing the address is not \
         enough. A Frame comes from a retype, a retype needs an Untyped, \
         and this component holds none."
    );

    // The framebuffer is untouched: nothing was mapped anywhere.
    if let Some(fb) = bootinfo::get().framebuffer() {
        check_kernel_does_not_map_it(fb.address, fb.size());
    }

    syscall::report();

    println!("[boot] M14 fb-no-cap checks passed");
    qemu::exit(qemu::EXIT_SUCCESS)
}

// ---- Negative (b): past the end ----------------------------------------

/// `fb-out-of-bounds`: the component writes past its mapping.
///
/// Expected: **#PF**, and this is the one M14 scenario that exits
/// 35.
///
/// # Why a fault rather than a refusal
///
/// The other negatives are refusals: a component asked for something
/// and was told no. This one is a component writing to an address
/// nothing is mapped at, which no kernel check can intercept —
/// the CPU faults, and that is the mechanism being demonstrated.
///
/// What it proves is that the device Untyped's **size** is real. The
/// component mapped exactly the pages the framebuffer covers; one
/// page further is nothing, and the hardware says so. A scenario
/// that exited 33 would be one where the write had quietly worked,
/// which would mean the mapping extended past the device.
pub fn run_out_of_bounds() -> ! {
    println!("[fb] the screen component will write past the end of the framebuffer");

    // **The address is computed here, not waited for.**
    //
    // The first version waited for the component to report it
    // through `Echo`, and that expectation could never be met. The
    // component's last two actions are the report and the faulting
    // store, with no scheduling point between them: `Echo` returns
    // to ring 3 directly (ADR 048), so this thread is not runnable
    // until the fault, and the fault kills the machine. The line was
    // "missing" because it was unreachable, not because anything
    // went wrong -- and a scenario whose required output cannot be
    // produced is a test that can only fail.
    //
    // So the kernel derives the address from the same two facts the
    // component used: the base it maps at, which is in the ABI, and
    // the geometry, which came from Limine. Printing the *expected*
    // CR2 before the fault is strictly better than echoing the
    // component's own number, because the two are computed
    // independently and the fault below confirms or refutes the
    // kernel's.
    let Some(fb) = bootinfo::get().framebuffer() else {
        panic!(
            "this machine has no framebuffer, so there is no mapping \
             to write past. The scenario cannot pass vacuously."
        );
    };

    let pages = fb.size().div_ceil(4096);
    let past = abi::FRAMEBUFFER_BASE + pages * 4096;

    // Armed before the component is given a chance to fault. The #PF
    // handler compares CR2 against this and says whether they agree;
    // see `check_predicted_fault` for why the comparison cannot
    // happen anywhere else.
    PREDICTED_FAULT.store(past, Ordering::Release);

    println!(
        "[fb] it maps {} page(s) at {:#018x} and will write to \
         {:#018x}, one page past the last mapped byte",
        pages,
        abi::FRAMEBUFFER_BASE,
        past
    );
    println!(
        "[fb] expecting #PF with cr2={:#018x}. This scenario exits 35 \
         by design: a fault is not a refusal, and the boundary being \
         real is what makes a device Untyped's size mean something.",
        past
    );

    // From here the component faults and the kernel's #PF handler
    // reports it. Nothing below runs.
    //
    // Waiting rather than exiting: if the write *succeeded*, no
    // fault arrives, and the deadline below turns that into a
    // failure with a message. An immediate `exit(SUCCESS)` would
    // make the scenario pass whether or not the boundary held.
    let deadline = read_tsc() + DEADLINE;
    while read_tsc() < deadline {
        sched::yield_now();
    }

    panic!(
        "the component wrote one page past the end of its framebuffer \
         mapping at {:#018x} and no fault arrived. Either something \
         mapped that page, or the mapping is larger than the device \
         Untyped it came from -- in which case the size in a device \
         Untyped is decoration.",
        past
    );
}

// ---- Negative (c): a TCB out of device memory --------------------------

/// `fb-device-untyped-misuse`: retype a TCB from the framebuffer.
///
/// Expected: `DeviceUntypedFrameOnly`. Exit 33.
///
/// # Why this is not a duplicate of `cap-test`'s check 12
///
/// M5 tested this rule and still does, against an Untyped the test
/// marked as device by hand. That shows the rule is implemented.
///
/// This shows the rule is **reached**: that boot's seeding actually
/// set the flag on a real device's memory, and that a component
/// holding that memory meets the restriction. A kernel where
/// `mark_device` worked perfectly and boot forgot to call it would
/// pass M5's check and fail this one — and that kernel is exactly
/// what existed from M5 until M14 (ADR 050).
pub fn run_device_misuse() -> ! {
    println!("[fb] the screen component will ask its device Untyped for a TCB");

    assert!(
        wait_for(SCR_MISUSE),
        "the component never reported its attempt. Steps: {:?}",
        StepDump
    );

    let result = step(SCR_MISUSE);
    assert!(
        syscall::is_error(result),
        "a component retyped a TCB out of the framebuffer's memory. \
         It returned {:#018x}.\n\
         \n\
         A TCB there would put the scheduler's saved registers on the \
         screen, and a write to the screen would corrupt a thread's \
         context. ADR 017 forbade this in M5 and the invariant has \
         been tested since; what this scenario adds is that boot's \
         seeding sets the flag on real device memory (ADR 050).",
        result
    );

    let code = syscall::error_code(result);
    assert_eq!(
        code,
        abi::error::DEVICE_UNTYPED_FRAME_ONLY,
        "the attempt was refused with error {} where \
         DeviceUntypedFrameOnly ({}) was expected. Some other refusal \
         would mean the request failed for a reason unrelated to the \
         memory being a device -- NotEnoughMemory, say, which would \
         leave the actual rule untested.",
        code,
        abi::error::DEVICE_UNTYPED_FRAME_ONLY
    );

    println!(
        "[fb] refused with DeviceUntypedFrameOnly: boot's seeding set \
         the device flag on the framebuffer's Untypeds, and the flag is \
         what a retype consults. M5 wrote this rule against an Untyped a \
         test had marked by hand; this is the first time it has been met \
         by a component holding a real device."
    );

    syscall::report();

    println!("[boot] M14 fb-device-untyped-misuse checks passed");
    qemu::exit(qemu::EXIT_SUCCESS)
}
