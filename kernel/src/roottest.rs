// SPDX-License-Identifier: BSD-2-Clause
//! M12 acceptance: the root task, and the kernel that stops.
//!
//! Four criteria, and they are not equally hard:
//!
//! 1. the root task runs at CPL 3, reads its BootInfo and reports it;
//! 2. it builds two components out of its own Untypeds, connects
//!    them with an Endpoint, and they exchange a message;
//! 3. the kernel's accounting adds up — every frame is in an
//!    Untyped, kept by the kernel, or **declared lost**;
//! 4. the kernel has no component-building code left after boot.
//!
//! Criterion 2 is the milestone. Criteria 3 and 4 are the ones that
//! could most easily be written so as to prove nothing, and most of
//! the commentary below is about how they avoid that.
//!
//! # Reporting
//!
//! Through `Echo`, whose CPtr argument names no capability and is
//! therefore free to carry a channel (ADR 044). No debug syscall was
//! added: a syscall added for a test is a syscall the kernel has
//! forever.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use pythos_abi as abi;

use crate::println;
use crate::roottask;
use crate::sched;
use crate::syscall;

// ---- Channels ----------------------------------------------------------
//
// Bit 28 upward, matching `roottask/src/main.rs`. M8 uses 8..17,
// M10 uses 20 and 21, M11 uses 24 and 25.

const CHANNEL: u64 = 1 << 28;
const CHANNEL_DONE: u64 = 1 << 29;
const CHANNEL_PANIC: u64 = 1 << 30;

/// How many steps the root task reports.
const STEP_COUNT: usize = 20;

const STEP_MAGIC: u64 = 0;
const STEP_VERSION: u64 = 1;
const STEP_UNTYPED_COUNT: u64 = 2;
const STEP_UNTYPED_BYTES_LOW: u64 = 3;
const STEP_UNTYPED_BYTES_HIGH: u64 = 4;
const STEP_USABLE_LOW: u64 = 5;
const STEP_USABLE_HIGH: u64 = 6;
const STEP_KERNEL_BYTES_LOW: u64 = 7;
const STEP_KERNEL_BYTES_HIGH: u64 = 8;
const STEP_ACCOUNTING: u64 = 9;
const STEP_CHILD_RETYPE: u64 = 10;
const STEP_CHILD_SETUP: u64 = 11;
const STEP_CHILD_RESUME: u64 = 12;
const STEP_SENT: u64 = 13;
const STEP_RECEIVED: u64 = 14;
const STEP_LOST_LOW: u64 = 15;
const STEP_LOST_HIGH: u64 = 16;

/// A value no report carries, so "never reported" and "reported
/// zero" stay distinguishable. Zero is the success of most
/// invocations, which makes the distinction load-bearing.
const NOT_REPORTED: u64 = u64::MAX;

static STEPS: [AtomicU64; STEP_COUNT] = [const { AtomicU64::new(NOT_REPORTED) }; STEP_COUNT];
static DONE: AtomicBool = AtomicBool::new(false);
static PANICKED: AtomicBool = AtomicBool::new(false);

/// Record a step, called from the syscall dispatcher's `Echo` path.
///
/// **Errors are sticky**, for the reason M11 recorded: several
/// invocations share a step, and "last write wins" lets a failure be
/// overwritten by whatever follows it. The symptom there was not a
/// failed assertion but a #PF in ring 3.
pub fn record_step(channel: u64, value: u64) -> bool {
    if channel & CHANNEL_PANIC != 0 {
        PANICKED.store(true, Ordering::Release);
        return true;
    }
    if channel & CHANNEL_DONE != 0 {
        DONE.store(true, Ordering::Release);
        return true;
    }
    if channel & CHANNEL != 0 {
        let index = (channel & 0xFF) as usize;
        if index < STEP_COUNT {
            let previous = STEPS[index].load(Ordering::Acquire);
            if previous == NOT_REPORTED || !syscall::is_error(previous) {
                STEPS[index].store(value, Ordering::Release);
            }
        }
        return true;
    }
    false
}

fn step(which: u64) -> u64 {
    STEPS[which as usize].load(Ordering::Acquire)
}

/// Reassemble a 64-bit value the root task reported in halves.
///
/// `Echo` carries one argument, and the test wants the exact figure
/// rather than a truncation — the whole of criterion 3 is an
/// equality between large numbers.
fn step_u64(low: u64, high: u64, name: &str) -> u64 {
    let lo = step(low);
    let hi = step(high);
    assert_ne!(
        lo, NOT_REPORTED,
        "the root task never reported {} (low)",
        name
    );
    assert_ne!(
        hi, NOT_REPORTED,
        "the root task never reported {} (high)",
        name
    );
    (hi << 32) | (lo & 0xFFFF_FFFF)
}

fn assert_ok(which: u64, name: &str) {
    let value = step(which);
    assert_ne!(
        value, NOT_REPORTED,
        "the root task never reported {}; it stopped before that point. \
         Steps so far: {:?}",
        name, StepDump
    );
    assert!(
        !syscall::is_error(value),
        "{} failed with error {} ({:#018x})",
        name,
        syscall::error_code(value),
        value
    );
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

/// How long to wait for the root task, in TSC cycles.
const DEADLINE: u64 = 30_000_000_000;

/// Wait until the root task says it has finished.
fn wait_for_done() {
    let deadline = read_tsc() + DEADLINE;
    while !DONE.load(Ordering::Acquire) && read_tsc() < deadline {
        sched::yield_now();
    }

    assert!(
        !PANICKED.load(Ordering::Acquire),
        "the root task's panic handler ran. It cannot print -- printing \
         needs a driver and a driver is a component -- so this is all the \
         detail there is. Steps: {:?}",
        StepDump
    );
    assert!(
        DONE.load(Ordering::Acquire),
        "the root task never finished. Steps: {:?}",
        StepDump
    );
}

// ---- The positive scenario ---------------------------------------------

/// `root-test`: the four criteria.
pub fn run() -> ! {
    println!("[roottest] the kernel has started the root task and sealed itself");

    // Criterion 4's first half, measured **before** waiting. The
    // seal is already closed: `roottask::start` closed it, and
    // `kmain` reached here afterwards.
    assert!(
        roottask::is_sealed(),
        "the kernel is not sealed, so criterion 4 has nothing to say"
    );
    let ops_at_seal = roottask::ops_at_seal();
    assert!(
        ops_at_seal > 0,
        "the seal closed after zero privileged operations. The counter is \
         not being incremented, so 'it stops moving' would be true of a \
         counter that never moved -- a check indistinguishable from its \
         own absence (ADR 023, K5)."
    );

    wait_for_done();

    // ---- 1. It read its BootInfo -------------------------------------
    //
    // Magic and version first, in that order, because they are the
    // only two fields a root task of the wrong version may rely on.
    let magic = step(STEP_MAGIC);
    assert_eq!(
        magic,
        abi::BOOTINFO_MAGIC,
        "the root task read magic {:#018x} rather than {:#018x}. It is \
         reading something, but not the BootInfo the kernel wrote -- \
         check the address and the mapping.",
        magic,
        abi::BOOTINFO_MAGIC
    );
    let version = step(STEP_VERSION);
    assert_eq!(
        version,
        abi::BOOTINFO_VERSION as u64,
        "BootInfo version {} rather than {}",
        version,
        abi::BOOTINFO_VERSION
    );

    // The count the root task saw, against what the kernel handed
    // over. **This is the pair K5's mutation breaks**: report one
    // fewer and the sum in criterion 3 falls short by that
    // Untyped's size.
    let reported_count = step(STEP_UNTYPED_COUNT);
    let actual_count = crate::cap::boot::root_untyped_count() as u64;
    assert_eq!(
        reported_count, actual_count,
        "BootInfo says {} Untypeds, the kernel created {}. A root task \
         told about fewer than it holds cannot account for the \
         difference, and the memory is unreachable in practice even \
         though a capability for it exists.",
        reported_count, actual_count
    );

    println!(
        "[roottest] the root task read its BootInfo at {:#018x}: magic \
         ok, version {}, {} untypeds",
        abi::BOOTINFO_ADDRESS,
        version,
        reported_count
    );

    // ---- 3. The accounting -------------------------------------------
    //
    // Checked before criterion 2 because it is about what the root
    // task was *given*, and the children are spent out of it.
    let untyped_bytes = step_u64(
        STEP_UNTYPED_BYTES_LOW,
        STEP_UNTYPED_BYTES_HIGH,
        "untyped bytes",
    );
    let usable_bytes = step_u64(STEP_USABLE_LOW, STEP_USABLE_HIGH, "usable bytes");
    let kernel_bytes = step_u64(
        STEP_KERNEL_BYTES_LOW,
        STEP_KERNEL_BYTES_HIGH,
        "kernel bytes",
    );
    let lost_bytes = step_u64(STEP_LOST_LOW, STEP_LOST_HIGH, "lost bytes");

    // The root task's own verdict, computed from the descriptors it
    // holds rather than from a figure it was handed.
    let verdict = step(STEP_ACCOUNTING);
    assert_ne!(
        verdict, NOT_REPORTED,
        "the root task never reported its verdict"
    );
    assert_eq!(
        verdict,
        1,
        "the root task summed its Untypeds at {} bytes, was told {} were \
         usable, {} kept by the kernel and {} unreachable, and the four do \
         not balance: {} + {} + {} != {}.",
        untyped_bytes,
        usable_bytes,
        kernel_bytes,
        lost_bytes,
        untyped_bytes,
        kernel_bytes,
        lost_bytes,
        usable_bytes
    );

    // And the kernel's own figures agree with what it told the root
    // task. Two separate claims: the arithmetic balances (above),
    // and the kernel did not lie about the inputs (here).
    let accounting = crate::roottask::Accounting::measure();
    assert_eq!(
        usable_bytes,
        accounting.usable * crate::memory::FRAME_SIZE,
        "BootInfo reported {} usable bytes but the allocator's bitmap \
         says {}. The root task's accounting is being checked against a \
         number the kernel invented.",
        usable_bytes,
        accounting.usable * crate::memory::FRAME_SIZE
    );

    // **The sum alone is not the claim.** `lost_bytes` is in the
    // identity, so a kernel that lost half the machine would still
    // balance. What makes criterion 3 mean "no frame disappears" is
    // this bound on the remainder.
    //
    // It is not asserted to be zero, because it is not: `cap::boot`
    // builds Untypeds of 2^k bytes at 2^k boundaries (ADR 020), and
    // the frames that fit no such block are genuinely unreachable
    // after `retire()`. Pretending otherwise would be the
    // comfortable lie. What *is* asserted is that the remainder is
    // small -- a few fragments at region edges, not a region.
    let lost_fraction_limit = usable_bytes / 100;
    assert!(
        lost_bytes <= lost_fraction_limit,
        "{} bytes ({} KiB) are in no Untyped and were not kept by the \
         kernel -- more than 1% of the {} MiB the machine has. That is \
         not edge fragmentation; a whole region is being dropped, most \
         likely because the Untyped CNode filled up or a region failed \
         to align.",
        lost_bytes,
        lost_bytes / 1024,
        usable_bytes / (1024 * 1024)
    );

    println!(
        "[roottest] accounting: {} MiB in untypeds + {} MiB kept by the \
         kernel + {} KiB unreachable = {} MiB usable. The three figures \
         come from the capabilities, from call counting, and from the \
         bitmap -- three mechanisms, so they can disagree.",
        untyped_bytes / (1024 * 1024),
        kernel_bytes / (1024 * 1024),
        lost_bytes / 1024,
        usable_bytes / (1024 * 1024)
    );

    // ---- 2. Two components, connected ---------------------------------
    //
    // **The milestone.** Everything here was done at CPL 3 by a
    // program the kernel loaded and did not otherwise touch.
    assert_ok(
        STEP_CHILD_RETYPE,
        "the children's objects, retyped by the root task",
    );
    assert_ok(
        STEP_CHILD_SETUP,
        "the children's address spaces, CSpaces and endpoints",
    );
    assert_ok(STEP_CHILD_RESUME, "TCB.Resume on both children");

    // The two children talked. Waited for separately, because
    // `Resume` schedules rather than runs (ADR 023, K8): asserting
    // immediately would blame the kernel for a race the test
    // created, which is the mistake M11 made and recorded.
    let deadline = read_tsc() + DEADLINE;
    while (step(STEP_SENT) == NOT_REPORTED || step(STEP_RECEIVED) == NOT_REPORTED)
        && read_tsc() < deadline
    {
        sched::yield_now();
    }

    assert_ok(STEP_SENT, "Endpoint.Send from the first child");

    let received = step(STEP_RECEIVED);
    assert_ne!(
        received, NOT_REPORTED,
        "the sending child's Send returned successfully but the receiving \
         child never woke. Steps: {:?}",
        StepDump
    );
    assert!(
        !syscall::is_error(received),
        "the receiving child's Recv returned error {}",
        syscall::error_code(received)
    );

    // **The badge is the claim.** It identifies the capability the
    // sender used, and the sender did not choose it -- the root task
    // attached it when it minted the copy (ADR 037). A message that
    // arrived from anywhere else would carry something different.
    const SENDER_BADGE: u64 = 0x5EED;
    assert_eq!(
        received, SENDER_BADGE,
        "the receiving child woke with badge {:#x} rather than {:#x}. A \
         message arrived, but not through the capability the root task \
         minted -- so 'the two are connected' is not what was shown.",
        received, SENDER_BADGE
    );

    println!(
        "[roottest] two components built by a ring 3 program exchanged a \
         message; the receiver woke with badge {:#x}, which its sender \
         could not have chosen",
        received
    );

    // ---- 4. The kernel stopped ----------------------------------------
    //
    // The counter has not moved since the seal. That is the whole
    // claim, and it is falsifiable in the way grepping for
    // `build_component` is not: `note_privileged_op` panics after
    // the seal, so a kernel that created an object here would have
    // died rather than reached this line.
    //
    // The two halves matter separately. The panic is the
    // enforcement; this equality is the evidence that the
    // enforcement had something to enforce -- it rules out a
    // counter that is simply never called.
    let ops_now = roottask::privileged_ops();
    assert_eq!(
        ops_now,
        ops_at_seal,
        "the kernel performed {} privileged operations after sealing \
         ({} at the seal, {} now). After boot the kernel answers \
         syscalls and delivers interrupts; it does not create objects, \
         map pages or start threads of its own accord (ADR 044).",
        ops_now - ops_at_seal,
        ops_at_seal,
        ops_now
    );

    // How many syscalls were served, **reported and not asserted
    // on**, and the reason is a mistake this test made first.
    //
    // The assertion here was `syscalls > ops_at_seal`, on the
    // reasoning that the root task should do more than the kernel
    // did and that an inversion would mean the kernel had grown
    // back the code M12 removed. It failed: 103 operations against
    // 61 syscalls.
    //
    // The kernel had not grown anything. 97 of those 103 are a
    // single `Copy` per root Untyped, and the machine has 97 of
    // them -- so the count scales with **how much RAM the machine
    // has**, not with how much work the kernel does. On a 64 MiB
    // machine it would have passed; on a 1 GiB one it would fail
    // harder. The quantity being compared was never the quantity
    // the claim was about.
    //
    // There is no honest threshold to tune here, because the two
    // numbers measure unrelated things. Criterion 4's real content
    // is the equality above -- the counter does not move after the
    // seal -- and that one is enforced by `note_privileged_op`
    // panicking rather than by arithmetic. A second, weaker check
    // dressed up as corroboration would be worse than none: it
    // would fail for reasons unconnected to the claim and be
    // "fixed" by raising a constant.
    let syscalls = syscall::syscall_count();

    println!(
        "[roottest] the kernel performed {} privileged operations during \
         boot and none since the seal; it has served {} syscalls",
        ops_at_seal, syscalls
    );
    println!(
        "[roottest] criterion 4 is enforced rather than asserted: \
         `note_privileged_op` panics after the seal, so a kernel that \
         built a component here would have stopped instead of reaching \
         this line"
    );

    println!("[roottest] all root task tests passed");

    syscall::report();

    println!("[boot] M12 checks passed");
    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        crate::qemu::EXIT_SUCCESS,
        crate::qemu::HOST_STATUS_SUCCESS
    );

    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

// ---- Negative (a): a root task with no memory --------------------------

/// `root-no-untyped`: the root task is given no Untyped at all.
///
/// Expected: it reports the shortage and stops; **the kernel stays
/// up** and exits 33.
///
/// # Why this is a test and not a hypothetical
///
/// The root task's first real action is to find an Untyped big
/// enough to build its children from. With none, every path after
/// that point is reached with a capability that names nothing — and
/// the question is whether the *kernel* notices. A root task that
/// dies is one thing; a kernel that dies with it is a kernel where
/// any component's bad day is everyone's.
///
/// The starvation is arranged in the root task rather than in the
/// kernel: the kernel hands over everything it has, which is the
/// behaviour under test. See [`starve`].
pub fn run_no_untyped() -> ! {
    println!("[roottest] the root task will find no usable Untyped");

    // The root task has already been started and the seal closed —
    // `kmain` reaches here afterwards. What this scenario changes is
    // what the root task *finds*, which is arranged by the flag
    // below being read on its first invocation.
    assert!(
        roottask::is_sealed(),
        "the kernel is not sealed; this scenario tests what a sealed \
         kernel does when its one component starves"
    );

    wait_for_done();

    // The root task reported the shortage through the step it uses
    // for its children's objects: with no Untyped it never gets as
    // far as retyping, and it says so with the error flag.
    let retype = step(STEP_CHILD_RETYPE);
    assert_ne!(
        retype, NOT_REPORTED,
        "the root task never reported anything about its children. It \
         was expected to notice it had no memory and say so."
    );
    assert!(
        syscall::is_error(retype),
        "the root task reported {:#018x} for its children's objects, \
         which is not an error -- but it was given no Untyped to build \
         them from.",
        retype
    );

    // **Which** error, and the first version of this did not ask.
    //
    // It checked only that the flag was set, and the root task
    // reported a bare `ERROR_FLAG` — error code *zero*. The log read
    // "reported error 0" and the test passed, which is a check
    // indistinguishable from its own absence (ADR 023, K5): a root
    // task that set the flag for any reason, or for none, would
    // satisfy it identically.
    //
    // `NotEnoughMemory` (11) is the claim. It names the caller's own
    // budget, which is exactly what ran out, and it is the same code
    // `vspace-user-exhaust` asserts for a component that spends
    // itself dry — the two scenarios differ in how the budget came
    // to be empty, not in whose budget it is.
    let code = syscall::error_code(retype);
    assert_eq!(
        code,
        11, // NotEnoughMemory
        "the root task reported error {} where NotEnoughMemory (11) was \
         expected. Zero in particular is not an error code: a bare \
         ERROR_FLAG says only that something went wrong, which a root \
         task could report for any reason at all.",
        code
    );

    // **The kernel is still running**, which is the claim. Reaching
    // this line at all is most of it; the counter check makes it
    // specific.
    let ops_now = roottask::privileged_ops();
    assert_eq!(
        ops_now,
        roottask::ops_at_seal(),
        "the kernel performed privileged operations while its component \
         starved. A kernel that steps in when a component runs out has \
         not actually handed the memory over."
    );

    println!(
        "[roottest] the root task found no usable Untyped, reported \
         error {}, and stopped; the kernel is still running and did \
         nothing on its behalf",
        syscall::error_code(retype)
    );

    syscall::report();

    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

// ---- Negative (b): the root task dies ----------------------------------

/// `root-dies`: the root task's TCB is revoked.
///
/// Expected: **the kernel panics**, and this exits 35.
///
/// # Why panic, and why that is not a cop-out
///
/// ADR 044 settled this, and the reasoning is worth repeating
/// because "the kernel carries on" sounds like the robust answer and
/// is not.
///
/// Every capability in the system descends from one the root task
/// holds. When it dies, nothing else can name an Untyped, an
/// interrupt line, or a frame of memory — not because the kernel
/// refuses, but because a CPtr is meaningful only relative to a root
/// (ADR 018) and the only root that reached anything is gone. A
/// kernel that "continued" would continue doing nothing, forever,
/// with no way to be told otherwise.
///
/// That is a *defined* outcome, which is what ADR 023 demands.
/// "Undefined" was not available: the choice was between a panic
/// that says what happened and an idle loop that does not.
///
/// The alternative considered and rejected was rebooting. It is the
/// same decision wearing a recovery story, and it hides the failure
/// from exactly the person who needs to see it.
pub fn run_root_dies() -> ! {
    println!("[roottest] the root task's TCB will be revoked while it runs");

    assert!(
        roottask::is_sealed(),
        "the kernel is not sealed; this scenario is about what a sealed \
         kernel does when its one component dies"
    );

    // Let it get far enough to be worth killing. Waiting for a
    // specific step rather than a fixed delay: a timing-based wait
    // would make the scenario depend on how fast the machine is,
    // which is K8's mistake.
    let deadline = read_tsc() + DEADLINE;
    while step(STEP_MAGIC) == NOT_REPORTED && read_tsc() < deadline {
        sched::yield_now();
    }
    assert_ne!(
        step(STEP_MAGIC),
        NOT_REPORTED,
        "the root task never reported its BootInfo, so it never really \
         started and killing it would prove nothing"
    );

    println!(
        "[roottest] the root task is running (it read its BootInfo); \
         revoking its TCB now"
    );

    // The kernel's own capability for the root task's TCB. Revoking
    // it is the sharpest available version of "the root task died":
    // not a crash it might have survived, but the thread ceasing to
    // exist.
    // SAFETY: `cap::boot::init_from_regions` ran during boot and
    // seeded this root slot with the object CNode.
    let cnode = unsafe { crate::cap::boot::leaf_cnode(crate::cap::boot::ROOT_SLOT_OBJECTS) };
    // SAFETY: Index in range; the slot holds the root task's TCB.
    let slot = unsafe { cnode.slot(roottask::ROOT_TCB_SLOT) };

    // SAFETY: The slot holds a live TCB and nothing else borrows it.
    // `delete` releases what the IPC layer holds on its behalf
    // (ADR 039) before clearing the slot.
    unsafe {
        crate::cap::cnode::delete(slot).expect("could not delete the root task's TCB");
    }

    // **The kernel notices, not this function.**
    //
    // The first version of this scenario waited here and then
    // declared failure with `unreachable!`, and that is exactly
    // what happened: the scheduler stopped scheduling the thread
    // and carried on with the boot thread, perfectly content. The
    // panic came from the *test*.
    //
    // Which would have been worthless. A negative scenario that
    // detects the fault itself proves the scenario works, not the
    // kernel -- the system would have sat there forever on any
    // machine nobody was watching, and ADR 044's "this must be a
    // panic" would have been a sentence in a document.
    //
    // `roottask::check_alive` on the timer tick is the fix, and the
    // wait below is now a wait for *the kernel's* panic. Reaching
    // the end of it means the kernel did not notice.
    //
    // The deadline is in ticks rather than cycles: the check runs
    // on the timer, so what has to elapse is ticks, and a
    // cycle-based wait would be measuring the wrong clock (ADR 023,
    // K8).
    let ticks_before = sched::tick_count();
    let deadline = read_tsc() + DEADLINE;
    while read_tsc() < deadline {
        sched::yield_now();
    }
    let ticks = sched::tick_count() - ticks_before;

    unreachable!(
        "the root task's TCB was deleted, {} timer ticks have passed, \
         and the kernel is still running. `roottask::check_alive` runs \
         on every tick and is supposed to notice -- so either it is not \
         being called, or it is looking at the wrong slot.\n\
         \n\
         The kernel noticing is the whole point: a scenario that \
         detected this itself would prove the scenario worked. Every \
         capability descends from one the root task held, so the \
         machine is now running with no way to be given work and no \
         way to be told.",
        ticks
    );
}

/// Whether this boot should starve the root task (negative a).
///
/// Read by [`crate::roottask`] when it decides how many Untypeds to
/// hand over. A flag rather than a separate code path, so that the
/// scenario exercises **the same handover code** the positive one
/// does — a second path would test itself rather than the kernel.
static STARVE: AtomicBool = AtomicBool::new(false);

/// Arrange for the root task to be given no Untyped.
///
/// Called from `kmain` before `roottask::start`, when the command
/// line asks for it.
pub fn starve() {
    STARVE.store(true, Ordering::Release);
}

/// Whether the root task should be starved.
pub fn starving() -> bool {
    STARVE.load(Ordering::Acquire)
}
