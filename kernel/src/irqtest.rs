// SPDX-License-Identifier: BSD-2-Clause
//! Interrupt handling acceptance tests (M10, ADR 041 and 042).
//!
//! M10's claim is narrow and checkable: **a hardware interrupt reaches
//! a thread outside the kernel, and the kernel's part in it is four
//! steps long.** Everything here is built to make that claim falsifiable
//! rather than merely plausible.
//!
//! Three things shape how the scenarios are written.
//!
//! # The interrupt source is a self-IPI, and that is a test decision
//!
//! A real device raises interrupts when *it* decides. Scenario 4 has
//! to show that no second interrupt is delivered before `Ack`, and
//! with a device there is no way to distinguish "the mask worked"
//! from "nothing was sent" — which is ADR 023's K8 exactly: a test
//! must not depend on the timing of the thing it measures.
//!
//! A LAPIC self-IPI is a genuine CPU-delivered interrupt — same IDT
//! entry, same stub, same dispatcher, same EOI — that arrives when
//! the kernel asks for it. The only thing it does not exercise is the
//! IOAPIC, and ADR 042 says plainly that the IOAPIC arrives in M12.
//!
//! # Waiting is for a counter to move, never for a duration
//!
//! Every wait here spins until a specific counter changes, bounded by
//! a TSC deadline so a broken path fails instead of hanging. In
//! particular, the "no second interrupt" check does **not** sleep and
//! then assert nothing happened — it waits for `while_masked` to
//! move, which is the positive evidence that the interrupt arrived
//! *and was refused*. "Nothing happened" and "nothing was sent" would
//! otherwise look identical.
//!
//! # Two counters, not one
//!
//! `delivered` and `while_masked` are separate for the same reason
//! M6's `cr3_switches` and `cr3_skipped` are: a single "interrupts
//! seen" counter would be satisfied by an implementation that never
//! masks anything. Requiring one to move while the other does not is
//! what makes the mask testable.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::cap::cnode::CNode;
use crate::cap::irq;
use crate::cap::tcb::{self, ThreadState};
use crate::cap::{boot, ipc, untyped, CapError, CapType, Rights};
use crate::memory::{layout, VirtAddr};
use crate::{apic, println, sched, syscall, usertest};

// ---- Fixture slots -----------------------------------------------------
//
// Each scenario runs in its own QEMU boot (they are dispatched by
// cmdline and every one of them exits), so these need only avoid
// colliding with each other and with the two slots boot reserves --
// `BOOT_TCB_SLOT` (255) and `IRQ_CONTROL_SLOT` (254).

/// The IrqHandler for [`irq::TEST_IRQ`].
const SLOT_HANDLER: usize = 0;
/// A second handler, for a different line.
const SLOT_HANDLER_ALT: usize = 1;
/// The Notification the handler signals. Badge 0, full rights.
const SLOT_NTFN: usize = 2;
/// A badged copy of it — what the handler actually points at.
const SLOT_NTFN_BADGED: usize = 3;
/// Spare, for the refusal cases.
const SLOT_SCRATCH: usize = 4;
/// A second Notification, for the revoked-destination negative.
const SLOT_NTFN_DOOMED: usize = 5;

/// The worker thread's TCB, with its four stack frames just above.
const SLOT_WORKER_TCB: usize = 16;
const SLOT_WORKER_FRAMES: usize = 17;

/// KSTACK slot for the worker. 0 and 1 belong to IST1 and the main
/// stack (ADR 009).
const WORKER_STACK_SLOT: u64 = 2;

/// The badge the handler's Notification capability carries.
///
/// Not 1, and not a power of two chosen by accident: a mask of
/// `0b1010` would survive a bug that ORs in a constant or shifts by
/// the wrong amount, where `1` would not.
const IRQ_BADGE: u32 = 0x00A5_0000;

/// Bytes one worker thread costs: a TCB plus its stack frames,
/// rounded up to a power of two. Stated as a *need* rather than an
/// Untyped index, which is ADR 023's K6.
const THREAD_BYTES: u64 = 32 * 1024;

/// How many latency samples to take.
const LATENCY_SAMPLES: u32 = 32;

/// How long any bounded wait may take, in TSC cycles.
///
/// Generous: under emulation the TSC is virtual and this is a
/// backstop against hanging, not a performance assertion.
const WAIT_DEADLINE: u64 = 20_000_000_000;

// ---- Worker state ------------------------------------------------------
//
// Statics rather than captured variables: a thread entry point is an
// `extern "C" fn() -> !` with nowhere to put an argument.

/// How many times the worker has woken from `Wait`.
static WORKER_WAKES: AtomicU64 = AtomicU64::new(0);
/// The badge it woke with, most recently.
static WORKER_BADGE: AtomicU64 = AtomicU64::new(0);
/// TSC at the moment the worker resumed.
static WAKE_TSC: AtomicU64 = AtomicU64::new(0);
/// TSC just before the interrupt was raised.
static RAISE_TSC: AtomicU64 = AtomicU64::new(0);
/// Set if the worker's `Wait` ever returned an error.
static WORKER_ERROR: AtomicBool = AtomicBool::new(false);

fn read_tsc() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: RDTSC is unprivileged, reads no memory, writes EDX:EAX.
    //
    // Reference: Intel SDM Vol. 2B, "RDTSC".
    unsafe {
        core::arch::asm!(
            "rdtsc",
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((high as u64) << 32) | (low as u64)
}

/// The CNode the fixtures retype into.
fn object_cnode() -> CNode {
    // SAFETY: `cap::boot::init` put a CNode in this root slot.
    unsafe { boot::leaf_cnode(boot::ROOT_SLOT_OBJECTS) }
}

/// The one IRQControl capability, seeded at boot (ADR 041).
fn irq_control() -> &'static mut crate::cap::cnode::Slot {
    let cnode = object_cnode();
    // SAFETY: `IRQ_CONTROL_SLOT` is within the object CNode (checked
    // at build time in `cap::boot`) and holds the IRQControl boot
    // seeded there.
    unsafe { cnode.slot(boot::IRQ_CONTROL_SLOT) }
}

/// Spin until `read()` changes from `before`, or the deadline passes.
///
/// Returns whether it moved. Every wait in this file goes through
/// here: waiting for a *counter* rather than for a *duration* is what
/// keeps these scenarios from depending on how fast the machine is.
fn wait_for_change(before: u64, read: impl Fn() -> u64) -> bool {
    let deadline = read_tsc() + WAIT_DEADLINE;
    while read_tsc() < deadline {
        if read() != before {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

// ---- Fixtures ----------------------------------------------------------

/// Claim [`irq::TEST_IRQ`] and put its handler in [`SLOT_HANDLER`].
fn claim_test_irq() {
    let cnode = object_cnode();
    // SAFETY: The CSpace is built; an IrqHandler is 32 bytes and the
    // request names a size rather than an Untyped index (K6).
    let parent = unsafe { boot::root_untyped_with_room(4096) };

    // SAFETY: Live IRQControl, Untyped and CNode; the destination
    // slot is empty.
    unsafe { irq::irq_control_get(irq_control(), irq::TEST_IRQ, parent, &cnode, SLOT_HANDLER) }
        .expect("IRQControl.Get refused the test line");
}

/// Create a Notification and a badged copy of it.
fn create_notification(plain: usize, badged: usize, badge: u32) {
    let cnode = object_cnode();
    // SAFETY: The CSpace is built.
    let parent = unsafe { boot::root_untyped_with_room(4096) };

    // SAFETY: Live Untyped and CNode; the destination slot is empty.
    unsafe { untyped::retype(parent, CapType::Notification, 0, 1, &cnode, plain) }
        .expect("could not retype a Notification");

    // The badge is attached by `mint`, not by `retype`: a freshly
    // retyped capability has badge 0, and ADR 037 makes badging a
    // separate act because it is the act that creates an identity.
    //
    // SAFETY: Both indices are in range and the destination is empty.
    unsafe {
        let src = cnode.slot(plain);
        let dest = cnode.slot(badged);
        crate::cap::cnode::mint(src, dest, Rights::ALL, badge)
    }
    .expect("could not mint a badged Notification capability");
}

/// Build the worker thread: a TCB, a stack, and an entry point.
///
/// A trimmed copy of `threadtest`'s fixture. Duplicated rather than
/// shared because the two differ in what they need — this one has no
/// use for the entry-trampoline assertion, which `threadtest` makes
/// because *that* milestone is where it mattered.
fn create_worker(entry: extern "C" fn() -> !) {
    let cnode = object_cnode();
    // SAFETY: The CSpace is built; the request names a size (K6).
    let parent = unsafe { boot::root_untyped_with_room(THREAD_BYTES) };

    // SAFETY: Live Untyped and CNode; the slot is empty.
    unsafe { untyped::retype(parent, CapType::Tcb, 0, 1, &cnode, SLOT_WORKER_TCB) }
        .expect("could not retype the worker's TCB");

    let (_guard, bottom, top) = layout::kstack_slot(WORKER_STACK_SLOT);
    let frames = (layout::KSTACK_SIZE / crate::memory::FRAME_SIZE) as usize;

    // SAFETY: As above.
    unsafe {
        untyped::retype(
            parent,
            CapType::Frame,
            0,
            frames,
            &cnode,
            SLOT_WORKER_FRAMES,
        )
    }
    .expect("could not retype the worker's stack frames");

    for index in 0..frames {
        // SAFETY: Index in range.
        let frame_slot = unsafe { cnode.slot(SLOT_WORKER_FRAMES + index) };
        let frame = crate::memory::PhysFrame::containing(frame_slot.object().0);
        let page = VirtAddr::new(bottom.0 + (index as u64) * crate::memory::FRAME_SIZE);

        // SAFETY: The kernel's tables are live, the address is inside
        // this thread's own unmapped KSTACK slot, and the frame came
        // from a retype so nothing else owns it. The intermediate
        // tables come from the heap's reserve, the only frame source
        // left after ADR 022's retirement.
        unsafe {
            let table = crate::memory::vm::kernel_table();
            let mut source = crate::memory::heap::reserve_frames();
            let flags = crate::memory::paging::PageFlags::READ
                | crate::memory::paging::PageFlags::WRITE
                | crate::memory::paging::PageFlags::GLOBAL;
            let tlb = crate::memory::paging::map(table, page, frame, flags, &mut source)
                .expect("could not map a worker stack page");
            tlb.flush();
        }
    }

    // SAFETY: Index in range.
    let tcb_slot = unsafe { cnode.slot(SLOT_WORKER_TCB) };
    // SAFETY: The slot holds a live TCB; the stack is mapped and
    // owned by nothing else, and `entry` diverges.
    unsafe { tcb::set_registers(tcb_slot, entry, top, bottom, WORKER_STACK_SLOT) }
        .expect("SetRegisters failed for the worker");
}

/// Borrow the worker's TCB.
///
/// # Safety
///
/// The caller must not hold another mutable borrow of it.
unsafe fn worker_tcb() -> &'static mut tcb::Tcb {
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let slot = unsafe { cnode.slot(SLOT_WORKER_TCB) };
    // SAFETY: The slot holds a live TCB; caller's aliasing guarantee.
    unsafe { tcb::from_slot(slot) }.expect("the worker slot no longer holds a TCB")
}

/// Put the worker on the ready queue and let it reach its `Wait`.
fn start_worker() {
    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(worker_tcb());
    }

    // Hand it the CPU until it is actually blocked. Waiting for the
    // *state* rather than yielding a fixed number of times: how many
    // yields it takes is a scheduling detail, that it ends up blocked
    // is the precondition the scenario needs.
    let deadline = read_tsc() + WAIT_DEADLINE;
    loop {
        // SAFETY: Live TCB; no other borrow held here.
        let state = unsafe { worker_tcb() }.state();
        if state == ThreadState::BlockedOnEndpoint {
            return;
        }
        assert!(
            read_tsc() < deadline,
            "the worker never reached its Wait; it is {} after a full \
             deadline. Either it was never scheduled, or Wait returned \
             immediately -- which would mean the Notification already \
             held a signal.",
            state.name()
        );
        sched::yield_now();
    }
}

/// The worker: block on the Notification, record what arrives.
///
/// Deliberately does **not** `Ack`. Acknowledging is the driver's job
/// and the scenarios need to choose the moment — scenario 4's whole
/// claim is about what happens *before* an Ack, which a worker that
/// acknowledged automatically would make unobservable.
extern "C" fn irq_worker() -> ! {
    let cnode = object_cnode();

    loop {
        // SAFETY: Index in range; the slot holds a live Notification.
        let slot = unsafe { cnode.slot(SLOT_NTFN) };

        // SAFETY: We are this thread and hold no other borrow of it.
        match unsafe { ipc::wait(slot) } {
            Ok(badge) => {
                // First, before anything else: the clock. Every line
                // after this one is measurement overhead.
                WAKE_TSC.store(read_tsc(), Ordering::Release);
                WORKER_BADGE.store(badge as u64, Ordering::Release);
                WORKER_WAKES.fetch_add(1, Ordering::AcqRel);
            }
            Err(_) => {
                // The Notification was revoked out from under us
                // (ADR 039). Recorded rather than panicked: one of the
                // negative scenarios arranges exactly this, and a
                // panic here would report it as a kernel fault.
                WORKER_ERROR.store(true, Ordering::Release);
                // SAFETY: We are this thread; suspending it yields.
                unsafe {
                    let me = sched::current();
                    sched::suspend(me);
                }
            }
        }
    }
}

// ---- The suite ---------------------------------------------------------

/// Run the interrupt tests.
pub fn run() -> ! {
    println!("[irq] starting interrupt handling tests");

    check_derivation();
    check_binding();
    check_delivery();
    check_mask_until_ack();
    measure_wake_latency();

    irq::report();
    println!("[irq] all interrupt tests passed");

    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

/// 1. A line is claimed once, and only once (ADR 041).
fn check_derivation() {
    let cnode = object_cnode();

    // Nobody owns the line yet.
    assert!(
        !irq::is_owned(irq::TEST_IRQ),
        "IRQ {} already has an owner before anything claimed it",
        irq::TEST_IRQ
    );

    claim_test_irq();

    // SAFETY: Index in range; the slot now holds an IrqHandler.
    let slot = unsafe { cnode.slot(SLOT_HANDLER) };
    assert_eq!(
        slot.cap_type(),
        CapType::IrqHandler,
        "IRQControl.Get produced a {} rather than an IRQHandler",
        slot.cap_type()
    );

    // SAFETY: The slot holds a live IrqHandler.
    let handler = unsafe { irq::handler_from_slot(slot) }.expect("the handler is not readable");
    assert_eq!(
        handler.irq(),
        irq::TEST_IRQ,
        "the handler owns line {} rather than the {} that was asked for",
        handler.irq(),
        irq::TEST_IRQ
    );
    assert!(
        !handler.is_bound(),
        "a freshly derived handler already has a Notification"
    );

    assert!(
        irq::is_owned(irq::TEST_IRQ),
        "the line has no owner after a successful Get"
    );

    // **The line starts masked.** An owner with no Notification has
    // nowhere to receive an interrupt, so an open line would produce
    // a refusal on every device assertion.
    assert!(
        irq::is_masked(irq::TEST_IRQ),
        "a freshly claimed line is open; an interrupt arriving before \
         SetNotification would have nowhere to go, and the line would \
         stay open to be refused again (ADR 041)"
    );

    // A second claim on the same line: refused. This is the one-owner
    // rule, and it is the reason `IrqAlreadyOwned` exists as its own
    // error rather than as `InvalidArgument`.
    //
    // SAFETY: Live IRQControl and CNode; expected to fail.
    let parent = unsafe { boot::root_untyped_with_room(4096) };
    // SAFETY: As above.
    match unsafe {
        irq::irq_control_get(irq_control(), irq::TEST_IRQ, parent, &cnode, SLOT_SCRATCH)
    } {
        Err(CapError::IrqAlreadyOwned { irq }) => {
            assert_eq!(irq, irq::TEST_IRQ, "the refusal names the wrong line");
        }
        other => panic!(
            "claiming an owned line returned {:?} instead of \
             IrqAlreadyOwned. Two owners of one line cannot both be \
             told about an interrupt, cannot both acknowledge it, and \
             the second can watch the first's device activity \
             (ADR 041).",
            other.map(|()| "Ok")
        ),
    }

    // The refusal left no trace: the scratch slot is still empty, so
    // a refused Get is atomic in the sense ADR 020 requires.
    // SAFETY: Index in range.
    let scratch = unsafe { cnode.slot(SLOT_SCRATCH) };
    assert!(
        scratch.is_empty(),
        "a refused Get left a capability in the destination slot"
    );

    // Owning one line does not imply owning another.
    //
    // SAFETY: Live IRQControl, Untyped and CNode.
    let parent = unsafe { boot::root_untyped_with_room(4096) };
    // SAFETY: As above; the slot is empty.
    unsafe {
        irq::irq_control_get(
            irq_control(),
            irq::TEST_IRQ_ALT,
            parent,
            &cnode,
            SLOT_HANDLER_ALT,
        )
    }
    .expect("a second, different line was refused");

    assert!(
        irq::is_owned(irq::TEST_IRQ_ALT),
        "the second line has no owner after a successful Get"
    );

    println!(
        "[irq] derivation: IRQ {} claimed and masked; a second claim on it \
         gives IrqAlreadyOwned and leaves no trace; IRQ {} claimed \
         independently (ADR 041)",
        irq::TEST_IRQ,
        irq::TEST_IRQ_ALT
    );
}

/// 2. A handler is told where to deliver, and refuses nonsense.
fn check_binding() {
    let cnode = object_cnode();

    create_notification(SLOT_NTFN, SLOT_NTFN_BADGED, IRQ_BADGE);

    // SAFETY: Indices in range.
    let handler_slot = unsafe { cnode.slot(SLOT_HANDLER) };

    // Binding something that is not a Notification: refused. The
    // interrupt path would otherwise read an Endpoint's queue head as
    // a signal mask.
    //
    // SAFETY: Both slots are live; expected to fail.
    match unsafe { irq::set_notification(handler_slot, irq_control()) } {
        Err(CapError::WrongType { found }) => {
            assert_eq!(found, CapType::IrqControl);
        }
        other => panic!(
            "SetNotification on a non-Notification returned {:?} instead \
             of WrongType",
            other.map(|()| "Ok")
        ),
    }

    // An empty slot: refused, and distinctly from the above.
    // SAFETY: Index in range.
    let empty = unsafe { cnode.slot(SLOT_SCRATCH) };
    // SAFETY: Both slots live; expected to fail.
    match unsafe { irq::set_notification(handler_slot, empty) } {
        Err(CapError::EmptySlot) => {}
        other => panic!(
            "SetNotification on an empty slot returned {:?} instead of \
             EmptySlot",
            other.map(|()| "Ok")
        ),
    }

    // The real thing, with the badged capability.
    // SAFETY: Indices in range.
    let badged = unsafe { cnode.slot(SLOT_NTFN_BADGED) };
    // SAFETY: Both slots hold live capabilities of the right types.
    unsafe { irq::set_notification(handler_slot, badged) }.expect("SetNotification was refused");

    // SAFETY: The slot holds a live IrqHandler.
    let handler = unsafe { irq::handler_from_slot(handler_slot) }.expect("handler unreadable");
    assert!(
        handler.is_bound(),
        "SetNotification returned Ok but the handler is not bound"
    );

    // Binding does not open the line. `Ack` is what does that, and
    // the distinction matters: a driver binds during setup and
    // acknowledges when it is ready to be interrupted.
    assert!(
        irq::is_masked(irq::TEST_IRQ),
        "SetNotification opened the line; only Ack may do that (ADR 042)"
    );

    println!(
        "[irq] binding: handler points at a Notification badged {:#010x}; \
         a non-Notification gives WrongType and an empty slot gives \
         EmptySlot; the line is still masked (ADR 037, ADR 042)",
        IRQ_BADGE
    );
}

/// 3. An interrupt arrives and wakes a thread blocked in `Wait`.
///
/// The whole of M10 in one check: a hardware interrupt, delivered by
/// the CPU, reaching a thread that is not the kernel.
fn check_delivery() {
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let handler_slot = unsafe { cnode.slot(SLOT_HANDLER) };

    // Open the line. The driver is ready.
    //
    // SAFETY: The slot holds a live IrqHandler.
    unsafe { irq::ack(handler_slot) }.expect("Ack was refused");
    assert!(!irq::is_masked(irq::TEST_IRQ), "Ack did not open the line");

    create_worker(irq_worker);
    start_worker();

    let wakes_before = WORKER_WAKES.load(Ordering::Acquire);
    let delivered_before = irq::delivered();

    // Raise it. This is a real IPI: the LAPIC delivers vector
    // `IRQ_VECTOR_BASE + TEST_IRQ` through the IDT, the stub pushes a
    // frame, `exception_dispatch` matches the vector, and
    // `handle_interrupt` runs in interrupt context.
    //
    // SAFETY: Every IRQ vector has an IDT gate (`idt::init`).
    unsafe {
        apic::raise_irq(irq::TEST_IRQ);
    }

    // The signal has made the worker runnable, not running. Hand it
    // the CPU.
    let deadline = read_tsc() + WAIT_DEADLINE;
    while WORKER_WAKES.load(Ordering::Acquire) == wakes_before && read_tsc() < deadline {
        sched::yield_now();
    }

    assert!(
        !WORKER_ERROR.load(Ordering::Acquire),
        "the worker's Wait returned an error"
    );
    assert_eq!(
        WORKER_WAKES.load(Ordering::Acquire),
        wakes_before + 1,
        "the worker did not wake. delivered={}, while_masked={}, \
         orphaned={}, stale={}, unbound={} -- if `delivered` moved the \
         signal was sent and the scheduler did not run the thread; if \
         one of the others moved, the interrupt arrived and was \
         refused for the reason that counter names",
        irq::delivered(),
        irq::while_masked(),
        irq::orphaned(),
        irq::stale_notification(),
        irq::unbound()
    );

    assert_eq!(
        irq::delivered(),
        delivered_before + 1,
        "the worker woke but the delivery was not counted"
    );

    // The badge is the one the *capability* carried, which is how a
    // driver owning several lines tells them apart from one mask
    // (ADR 037).
    assert_eq!(
        WORKER_BADGE.load(Ordering::Acquire),
        IRQ_BADGE as u64,
        "the worker woke with badge {:#010x} rather than the {:#010x} \
         minted onto the handler's capability; the interrupt path is \
         not reading the badge from the capability",
        WORKER_BADGE.load(Ordering::Acquire),
        IRQ_BADGE
    );

    // And the line was shut on the way past.
    assert!(
        irq::is_masked(irq::TEST_IRQ),
        "the line is still open after an interrupt was delivered; the \
         device could raise a second one before the driver has started \
         on the first (ADR 042)"
    );

    println!(
        "[irq] delivery: a self-IPI on vector {} reached a thread blocked \
         in Wait, with badge {:#010x}; the line was masked before the \
         signal (ADR 042)",
        apic::irq_to_vector(irq::TEST_IRQ),
        IRQ_BADGE
    );
}

/// 4. **No second interrupt is delivered before `Ack`.**
///
/// The acceptance criterion this milestone turns on.
///
/// Two counters. `delivered` must not move while the line is masked;
/// `while_masked` must. Requiring both is what distinguishes "the
/// mask worked" from "no interrupt was sent" — a single counter would
/// be satisfied by either.
///
/// # What the K5 mutation showed, including the part that is a
/// criticism of this test
///
/// The prescribed mutation was to make `Ack` a no-op. It was killed,
/// but running it taught three things and only the first was the
/// expected one.
///
/// **It is killed.** With `Ack` never clearing the mask, the run
/// fails rather than passing. So the contract is tested.
///
/// **The first failure came from the wrong place.** Scenario 3's
/// `assert!(!is_masked(..))` fired first and stopped the run before
/// this check was reached. That assertion reads *the same flag `Ack`
/// writes* — it is close to asking the mutation whether it mutated
/// itself. A test suite whose only evidence is of that shape would
/// satisfy K5's letter and not its intent.
///
/// **The behavioural check stands on its own.** To find out, the two
/// `is_masked` assertions were temporarily removed and the mutation
/// re-run. The failure then came from here:
///
/// ```text
/// [PANIC] after Ack the same interrupt was still not delivered;
///         the line is masked permanently, which silences the device
///         rather than isolating it
/// ```
///
/// An interrupt was raised, no delivery followed, and the counter
/// said so — no flag was consulted. That is the evidence worth
/// having, and it is why the second half of this check exists at all.
///
/// A variant mutation was also needed to reach it: "never open the
/// line" is caught too early to exercise the second half, so `Ack`
/// was made to work exactly once. Under that, the first half passed
/// and the second failed — which confirms the two halves test
/// different properties rather than one twice.
///
/// The `is_masked` assertions are kept, because a fast failure with
/// a precise message is worth having. But they are not what makes
/// the check sound, and this note is here so that a later reader
/// does not mistake them for it.
fn check_mask_until_ack() {
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let handler_slot = unsafe { cnode.slot(SLOT_HANDLER) };

    // Scenario 3 left the line masked, which is the state a driver is
    // in when it has been told about an interrupt and has not yet
    // finished with it.
    assert!(
        irq::is_masked(irq::TEST_IRQ),
        "this check needs the line masked to begin with"
    );

    let delivered_before = irq::delivered();
    let masked_before = irq::while_masked();
    let wakes_before = WORKER_WAKES.load(Ordering::Acquire);

    // Raise it again. The device is impatient; the driver has not
    // acknowledged.
    //
    // SAFETY: The vector has an IDT gate.
    unsafe {
        apic::raise_irq(irq::TEST_IRQ);
    }

    // Wait for the **refusal**, not for a duration. This is the
    // positive evidence that the interrupt arrived and was dropped —
    // sleeping and then asserting that nothing happened could not
    // tell that apart from an interrupt that was never raised.
    let refused = wait_for_change(masked_before, irq::while_masked);
    assert!(
        refused,
        "the interrupt was raised on a masked line but `while_masked` \
         never moved, so it never arrived at all. The check cannot \
         conclude anything: the mask has not been tested."
    );

    // And the thing that must *not* have happened.
    assert_eq!(
        irq::delivered(),
        delivered_before,
        "a second interrupt was delivered while the line was masked. \
         Because a signal is a mask rather than a count (ADR 038), the \
         driver has no way to tell one interrupt from two."
    );
    assert_eq!(
        WORKER_WAKES.load(Ordering::Acquire),
        wakes_before,
        "the worker woke a second time without an intervening Ack"
    );

    println!(
        "[irq] masked: an interrupt on the shut line was refused \
         (while_masked {} -> {}) and not delivered (delivered stayed at \
         {})",
        masked_before,
        irq::while_masked(),
        delivered_before
    );

    // Now acknowledge, and the *same* stimulus gets through. Without
    // this half the check would pass on a kernel that masks a line
    // permanently, which is not isolation but a dead device.
    //
    // SAFETY: The slot holds a live IrqHandler.
    unsafe { irq::ack(handler_slot) }.expect("Ack was refused");
    assert!(
        !irq::is_masked(irq::TEST_IRQ),
        "Ack did not reopen the line"
    );

    // SAFETY: The vector has an IDT gate.
    unsafe {
        apic::raise_irq(irq::TEST_IRQ);
    }

    let deadline = read_tsc() + WAIT_DEADLINE;
    while WORKER_WAKES.load(Ordering::Acquire) == wakes_before && read_tsc() < deadline {
        sched::yield_now();
    }

    assert_eq!(
        irq::delivered(),
        delivered_before + 1,
        "after Ack the same interrupt was still not delivered; the line \
         is masked permanently, which silences the device rather than \
         isolating it"
    );
    assert_eq!(
        WORKER_WAKES.load(Ordering::Acquire),
        wakes_before + 1,
        "after Ack the worker did not wake"
    );

    println!(
        "[irq] ack: the identical stimulus was delivered once the line was \
         reopened ({} -> {} delivered). One interrupt per Ack, which is \
         the contract a driver can count on (ADR 042).",
        delivered_before,
        irq::delivered()
    );
}

/// 5. How long from raising an interrupt to the handler thread
///    running?
///
/// Not pass/fail. This is the figure M13's driver will be judged
/// against, so M10 records a baseline.
///
/// **What is being measured.** From the instruction before the IPI to
/// the first instruction of the woken thread — so it includes
/// interrupt delivery, the stub, the dispatcher, the signal, and a
/// context switch. The switch is in there because a driver
/// experiences it: a Notification makes a thread *runnable*, and
/// becoming runnable is not the same as running.
///
/// The minimum over the samples is reported rather than the mean. A
/// mean here measures how often the timer interrupted the
/// measurement; the minimum is the closest thing to the cost of the
/// path itself.
fn measure_wake_latency() {
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let handler_slot = unsafe { cnode.slot(SLOT_HANDLER) };

    let mut minimum = u64::MAX;
    let mut maximum = 0u64;
    let mut total = 0u64;
    let mut taken = 0u32;

    for _ in 0..LATENCY_SAMPLES {
        // Get the worker back into its Wait. It may still be running
        // the tail of its previous loop iteration.
        let deadline = read_tsc() + WAIT_DEADLINE;
        loop {
            // SAFETY: Live TCB; no other borrow held.
            let state = unsafe { worker_tcb() }.state();
            if state == ThreadState::BlockedOnEndpoint {
                break;
            }
            assert!(
                read_tsc() < deadline,
                "the worker never returned to its Wait between samples; \
                 it is {}",
                state.name()
            );
            sched::yield_now();
        }

        // Open the line for this round.
        //
        // SAFETY: The slot holds a live IrqHandler.
        unsafe { irq::ack(handler_slot) }.expect("Ack was refused mid-measurement");

        let wakes_before = WORKER_WAKES.load(Ordering::Acquire);

        RAISE_TSC.store(read_tsc(), Ordering::Release);
        // SAFETY: The vector has an IDT gate.
        unsafe {
            apic::raise_irq(irq::TEST_IRQ);
        }

        // Hand over immediately: the sample ends when the worker
        // executes, and it cannot execute while this thread holds the
        // CPU.
        let deadline = read_tsc() + WAIT_DEADLINE;
        while WORKER_WAKES.load(Ordering::Acquire) == wakes_before && read_tsc() < deadline {
            sched::yield_now();
        }
        assert_eq!(
            WORKER_WAKES.load(Ordering::Acquire),
            wakes_before + 1,
            "a measurement round produced no wake"
        );

        let raised = RAISE_TSC.load(Ordering::Acquire);
        let woke = WAKE_TSC.load(Ordering::Acquire);
        assert!(
            woke > raised,
            "the worker's wake timestamp precedes the raise; the TSC is \
             not monotonic across this switch and the figure would be \
             meaningless"
        );

        let elapsed = woke - raised;
        minimum = minimum.min(elapsed);
        maximum = maximum.max(elapsed);
        total += elapsed;
        taken += 1;
    }

    assert!(taken == LATENCY_SAMPLES, "not every sample was taken");

    println!(
        "[irq] wake latency: {} samples, minimum {} cycles, mean {}, \
         maximum {}",
        taken,
        minimum,
        total / taken as u64,
        maximum
    );
    println!(
        "[irq] wake latency: raise -> the handler thread's first \
         instruction, so it includes interrupt delivery, the stub, the \
         dispatcher, the signal and one context switch. Measured under \
         emulation, where the TSC is virtual and the IPI is simulated: \
         an order of magnitude, not a measurement. The minimum is the \
         headline because a mean here would mostly record how often \
         the timer interrupted the measurement."
    );
}

// ---- Negative scenarios ------------------------------------------------

/// **Negative a.** Claiming a line without IRQControl.
///
/// The authority check, and the reason it needs its own scenario:
/// every other test in this file holds the capability, so none of
/// them can show what happens without it. A check nothing reaches is
/// indistinguishable from its own absence (ADR 023, K5).
pub fn run_no_cap() -> ! {
    println!("[irq] no-capability test: claiming a line without IRQControl");

    let cnode = object_cnode();

    // Something that is emphatically not an IRQControl: an Endpoint.
    // SAFETY: The CSpace is built.
    let parent = unsafe { boot::root_untyped_with_room(4096) };
    // SAFETY: Live Untyped and CNode; the slot is empty.
    unsafe { untyped::retype(parent, CapType::Endpoint, 0, 1, &cnode, SLOT_SCRATCH) }
        .expect("could not retype the impostor");

    // SAFETY: Index in range.
    let impostor = unsafe { cnode.slot(SLOT_SCRATCH) };
    // SAFETY: The CSpace is built.
    let parent = unsafe { boot::root_untyped_with_room(4096) };

    // SAFETY: Live capabilities; expected to fail.
    match unsafe { irq::irq_control_get(impostor, irq::TEST_IRQ, parent, &cnode, SLOT_HANDLER) } {
        Err(CapError::WrongType { found }) => {
            println!(
                "[irq] refused with WrongType({}), as ADR 041 requires",
                found
            );
        }
        other => panic!(
            "claiming a line with an Endpoint in place of IRQControl \
             returned {:?} instead of WrongType. Interrupt lines would \
             be claimable by anyone holding any capability at all.",
            other.map(|()| "Ok")
        ),
    }

    // And nothing happened: no owner, no capability, no trace.
    assert!(
        !irq::is_owned(irq::TEST_IRQ),
        "the refused claim still took ownership of the line"
    );
    // SAFETY: Index in range.
    let dest = unsafe { cnode.slot(SLOT_HANDLER) };
    assert!(
        dest.is_empty(),
        "the refused claim still wrote a capability"
    );

    println!(
        "[irq] the line is unowned and the destination slot is empty; a \
         refused Get leaves no trace (ADR 020's atomicity rule)"
    );

    // Exits 35 by design, like `thread-kill-running`: the evidence is
    // in the log above and the runner expects a panic.
    panic!(
        "irq-no-cap completed its checks; exiting 35 by design so that \
         the WrongType refusal is asserted on a panic-expecting run"
    );
}

/// **Negative b.** The handler's destination is revoked out from
/// under it, and then an interrupt arrives.
///
/// This is the race ADR 042 names. The kernel must not signal through
/// a slot whose capability has been deleted — the memory may hold
/// anything now — and it must not die either. It detects, counts, and
/// carries on.
pub fn run_revoked() -> ! {
    println!("[irq] revoked-destination test: deleting a bound Notification");

    let cnode = object_cnode();

    claim_test_irq();
    create_notification(SLOT_NTFN_DOOMED, SLOT_NTFN_BADGED, IRQ_BADGE);

    // SAFETY: Indices in range.
    let handler_slot = unsafe { cnode.slot(SLOT_HANDLER) };
    // SAFETY: As above.
    let badged = unsafe { cnode.slot(SLOT_NTFN_BADGED) };
    // SAFETY: Both slots hold live capabilities of the right types.
    unsafe { irq::set_notification(handler_slot, badged) }.expect("SetNotification was refused");
    // SAFETY: The slot holds a live IrqHandler.
    unsafe { irq::ack(handler_slot) }.expect("Ack was refused");

    println!(
        "[irq] bound and open; now deleting the capability the handler \
         points at"
    );

    // Delete the capability the handler names. The *object* is still
    // there — `SLOT_NTFN_DOOMED` holds another capability for it — so
    // this is specifically the case where the handler's reference is
    // stale while the memory is still a valid Notification. That is
    // the harder half: a freed object might be caught by chance, a
    // live one at the same address never would be.
    //
    // SAFETY: The slot holds a live capability.
    unsafe { crate::cap::cnode::delete(badged) }.expect("delete failed");

    let stale_before = irq::stale_notification();
    let delivered_before = irq::delivered();

    // SAFETY: The vector has an IDT gate.
    unsafe {
        apic::raise_irq(irq::TEST_IRQ);
    }

    let detected = wait_for_change(stale_before, irq::stale_notification);
    assert!(
        detected,
        "an interrupt arrived for a handler whose Notification \
         capability had been deleted, and `stale_notification` did not \
         move. Either the interrupt never arrived -- in which case \
         nothing was tested -- or the kernel signalled through the \
         deleted slot, which delivers into whatever occupies it now."
    );

    assert_eq!(
        irq::delivered(),
        delivered_before,
        "the interrupt was delivered through a deleted capability"
    );

    // The kernel is still up. Stated by doing something, not by
    // asserting it: the lines below only run if it is.
    println!(
        "[irq] detected: stale_notification {} -> {}, delivered unchanged \
         at {}",
        stale_before,
        irq::stale_notification(),
        delivered_before
    );

    // The line stays shut. A driver that has lost its destination
    // must not have its device raising interrupts nobody can receive.
    assert!(
        irq::is_masked(irq::TEST_IRQ),
        "the line was reopened after its destination vanished; the \
         device would raise interrupts that are refused forever"
    );

    irq::report();
    println!(
        "[irq] the kernel is still running and the line is masked; a lost \
         destination isolates one driver rather than stopping the \
         machine (ADR 030, ADR 042)"
    );

    panic!(
        "irq-revoked completed its checks; exiting 35 by design so that \
         the stale-destination path is asserted on a panic-expecting run"
    );
}

/// **Negative c.** Allocating a frame from inside a real device
/// interrupt.
///
/// ADR 022's tripwire has been live since M6, but only the *timer*
/// has ever exercised it — and a timer interrupt is the kernel's own.
/// This is the first time it is reached from a path a user component
/// triggers, which is the case the rule was written for.
pub fn run_alloc() -> ! {
    println!("[irq] allocating from inside a real device interrupt");

    let cnode = object_cnode();

    claim_test_irq();
    create_notification(SLOT_NTFN, SLOT_NTFN_BADGED, IRQ_BADGE);

    // SAFETY: Indices in range.
    let handler_slot = unsafe { cnode.slot(SLOT_HANDLER) };
    // SAFETY: As above.
    let badged = unsafe { cnode.slot(SLOT_NTFN_BADGED) };
    // SAFETY: Both slots hold live capabilities of the right types.
    unsafe { irq::set_notification(handler_slot, badged) }.expect("SetNotification was refused");
    // SAFETY: The slot holds a live IrqHandler.
    unsafe { irq::ack(handler_slot) }.expect("Ack was refused");

    ALLOCATE_IN_HANDLER.store(true, Ordering::Release);
    println!(
        "[irq] armed; raising IRQ {} (must panic in the handler)",
        irq::TEST_IRQ
    );

    // SAFETY: The vector has an IDT gate.
    unsafe {
        apic::raise_irq(irq::TEST_IRQ);
    }

    let deadline = read_tsc() + WAIT_DEADLINE;
    while read_tsc() < deadline {
        core::hint::spin_loop();
    }

    unreachable!(
        "the interrupt was raised but the tripwire never fired; either \
         no interrupt arrived, or allocation inside interrupt context is \
         permitted on this path"
    );
}

/// Set by [`run_alloc`]; read by the IRQ dispatcher.
pub static ALLOCATE_IN_HANDLER: AtomicBool = AtomicBool::new(false);

/// Called from the IRQ handler when the scenario has armed it.
///
/// Lives here rather than in `interrupts` so that the test's intent
/// is beside the test — the same arrangement `threadtest` uses for
/// the timer.
pub fn allocate_from_handler_if_armed() {
    if !ALLOCATE_IN_HANDLER.load(Ordering::Acquire) {
        return;
    }
    ALLOCATE_IN_HANDLER.store(false, Ordering::Release);

    println!(
        "[irq] inside the device interrupt handler, in_interrupt={}; \
         calling allocate() (must panic)",
        crate::memory::frame_allocator::in_interrupt()
    );

    let frame = crate::memory::frame_allocator::allocate();

    unreachable!(
        "allocation inside a device interrupt handler returned {:?} \
         instead of panicking. ADR 022's tripwire does not work on the \
         path that matters: a device chooses how often this runs, so an \
         allocation here is one an attacker controls the rate of.",
        frame.map(|f| f.0)
    );
}

// ---- Scenario 6: a ring-3 thread receives an interrupt ----------------
//
// The five scenarios above all run the handler thread in the kernel,
// which proves the interrupt path but leaves the milestone's purpose
// half-shown: M13's driver is a *user component*, and the claim worth
// making is that a thread at CPL 3 can wait for a device.
//
// Built on M8's machinery rather than a copy of it -- `usertest`
// already knows how to assemble a program, build an address space and
// start a thread at ring 3, and duplicating that would mean two
// copies to keep right.

/// Object-CNode slots for this scenario's user space and thread.
///
/// Away from the others: M8's fixtures use 64..80, 96..112, 128 and
/// 160, and this scenario reuses `build_user_space` with its own
/// base.
const SLOT_USER_SPACE: usize = 64;
const SLOT_USER_TCB: usize = 128;

/// KSTACK slot for the user thread's kernel stack. M8 uses 10 and 11.
const USER_KSTACK_SLOT: u64 = 10;

/// Echo channels this scenario's user program reports on.
///
/// High bits, clear of `usertest`'s own channels (which use bits 8,
/// 9, 10, 16 and 17). A collision would route this scenario's reports
/// into M8's statics, where they would be silently ignored.
const CHANNEL_BEFORE_WAIT: u64 = 1 << 20;
const CHANNEL_AFTER_WAIT: u64 = 1 << 21;

/// What the ring-3 thread reported before calling `Wait`.
static USER_REACHED_WAIT: AtomicBool = AtomicBool::new(false);
/// What it reported after `Wait` returned: the badge, or an error.
static USER_WAIT_RESULT: AtomicU64 = AtomicU64::new(0);
static USER_WOKE: AtomicBool = AtomicBool::new(false);

/// Record a report from this scenario's user thread.
///
/// Called from the syscall dispatcher's `Echo` path, the same way
/// `usertest::record_report` is. Returns whether the channel was
/// ours.
pub fn record_user_report(channel: u64, value: u64) -> bool {
    if channel & CHANNEL_AFTER_WAIT != 0 {
        USER_WAIT_RESULT.store(value, Ordering::Release);
        USER_WOKE.store(true, Ordering::Release);
        return true;
    }
    if channel & CHANNEL_BEFORE_WAIT != 0 {
        USER_REACHED_WAIT.store(true, Ordering::Release);
        return true;
    }
    false
}

/// **Scenario 6.** A thread at CPL 3 blocks on a Notification and is
/// woken by a hardware interrupt.
///
/// This is M10's purpose stated as a test: an interrupt arrives at
/// the CPU, the kernel does its four steps, and a thread *outside the
/// kernel, at the lowest privilege level* resumes. Everything between
/// the device and the driver is mechanism; this is the end of it.
///
/// # How the claim is made falsifiable
///
/// The user thread reports twice: once before `Wait` and once after.
/// Both are required, and the pair is what distinguishes the three
/// ways this could go wrong:
///
/// * neither report -- the thread never ran;
/// * the first only -- `Wait` blocked and nothing woke it, which is
///   the failure the whole interrupt path exists to avoid;
/// * both, but with the error flag set -- `Wait` returned without
///   having delivered anything.
///
/// And the *order* is checked against the interrupt: the scenario
/// confirms the thread is blocked before raising it, so a `Wait` that
/// returned immediately because the mask was already set could not
/// pass.
pub fn run_user() -> ! {
    println!("[irq] ring 3 test: a user thread will wait for an interrupt");

    let cnode = object_cnode();

    // The kernel-side arrangement: a line, a Notification, a binding.
    claim_test_irq();
    create_notification(SLOT_NTFN, SLOT_NTFN_BADGED, IRQ_BADGE);

    // SAFETY: Indices in range.
    let handler_slot = unsafe { cnode.slot(SLOT_HANDLER) };
    // SAFETY: As above.
    let badged = unsafe { cnode.slot(SLOT_NTFN_BADGED) };
    // SAFETY: Both slots hold live capabilities of the right types.
    unsafe { irq::set_notification(handler_slot, badged) }.expect("SetNotification was refused");
    // SAFETY: The slot holds a live IrqHandler.
    unsafe { irq::ack(handler_slot) }.expect("Ack was refused");

    // The user program, in two halves around a blocking syscall.
    //
    // A CPtr to the Notification is built here and baked into the
    // program as an immediate: user code has no way to discover the
    // CSpace's shape, and should not -- the capability is *given* to
    // it, which is the whole model.
    let cspace = boot::init_cspace();
    // SAFETY: Valid indices; the slot holds the plain Notification.
    let ntfn_cptr =
        unsafe { crate::cap::cptr::address_of(&cspace, boot::ROOT_SLOT_OBJECTS, SLOT_NTFN) }
            .expect("could not address the Notification slot");

    let mut program = usertest::Program::new();

    // 1. "I am about to wait."
    program.emit(&usertest::mov_rdi_imm(CHANNEL_BEFORE_WAIT));
    program.emit(&usertest::mov_rsi_imm(syscall::Method::Echo as u64));
    program.emit(&usertest::mov_rdx_imm(0));
    program.emit(&usertest::program::SYSCALL);

    // 2. Wait. **This syscall does not return until an interrupt
    //    arrives**, which is the point of the scenario.
    program.emit(&usertest::mov_rdi_imm(ntfn_cptr.0));
    program.emit(&usertest::mov_rsi_imm(syscall::Method::Wait as u64));
    program.emit(&usertest::program::SYSCALL);

    // 3. "I woke, and here is what I got." RAX holds the mask (or an
    //    error); it moves to RDX, which is Echo's argument.
    program.emit(&[0x48, 0x89, 0xc2]); // mov rdx, rax
    program.emit(&usertest::mov_rdi_imm(CHANNEL_AFTER_WAIT));
    program.emit(&usertest::mov_rsi_imm(syscall::Method::Echo as u64));
    program.emit(&usertest::program::SYSCALL);

    program.emit(&usertest::program::SPIN);

    let space = usertest::build_user_space(SLOT_USER_SPACE, program.as_slice(), 0);
    let tcb = usertest::build_user_thread(SLOT_USER_TCB, USER_KSTACK_SLOT, &space, 0);

    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(usertest::tcb_at(tcb));
    }

    // Wait for it to reach its `Wait` -- and then for it to actually
    // be *blocked*. Both, and the second is what makes the interrupt
    // meaningful: raising one while the thread was still running
    // would leave "it woke" and "it never slept" indistinguishable.
    let deadline = read_tsc() + WAIT_DEADLINE;
    while !USER_REACHED_WAIT.load(Ordering::Acquire) && read_tsc() < deadline {
        sched::yield_now();
    }
    assert!(
        USER_REACHED_WAIT.load(Ordering::Acquire),
        "the user thread never reported; it did not reach ring 3, or its \
         syscall never arrived"
    );

    let deadline = read_tsc() + WAIT_DEADLINE;
    loop {
        // SAFETY: Live TCB; no other borrow held.
        let state = unsafe { usertest::tcb_at(tcb) }.state();
        if state == ThreadState::BlockedOnEndpoint {
            break;
        }
        assert!(
            read_tsc() < deadline,
            "the user thread reported but never blocked; it is {}. If \
             Wait returned immediately the interrupt below would prove \
             nothing.",
            state.name()
        );
        sched::yield_now();
    }

    println!(
        "[irq] the user thread is blocked at cpl 3 inside Wait; raising \
         IRQ {}",
        irq::TEST_IRQ
    );

    let delivered_before = irq::delivered();

    // SAFETY: The vector has an IDT gate.
    unsafe {
        apic::raise_irq(irq::TEST_IRQ);
    }

    let deadline = read_tsc() + WAIT_DEADLINE;
    while !USER_WOKE.load(Ordering::Acquire) && read_tsc() < deadline {
        sched::yield_now();
    }

    assert!(
        USER_WOKE.load(Ordering::Acquire),
        "the interrupt was raised but the ring-3 thread never woke. \
         delivered={}, while_masked={}, orphaned={}, stale={} -- if \
         `delivered` moved, the signal was sent and the thread was not \
         resumed into ring 3.",
        irq::delivered(),
        irq::while_masked(),
        irq::orphaned(),
        irq::stale_notification()
    );

    let result = USER_WAIT_RESULT.load(Ordering::Acquire);
    assert!(
        !syscall::is_error(result),
        "the ring-3 Wait returned error {} rather than a badge",
        syscall::error_code(result)
    );
    assert_eq!(
        result, IRQ_BADGE as u64,
        "the ring-3 thread woke with mask {:#010x} rather than the \
         {:#010x} the handler's capability carries",
        result, IRQ_BADGE
    );
    assert_eq!(
        irq::delivered(),
        delivered_before + 1,
        "the thread woke but the delivery was not counted"
    );

    // It really was at CPL 3 -- checked through the TCB's own record,
    // because every other line here would read the same on a kernel
    // thread.
    // SAFETY: Live TCB; no other borrow held.
    let user_mode = unsafe { usertest::tcb_at(tcb) }.is_user_mode();
    assert!(
        user_mode,
        "the thread that woke is not recorded as running at CPL 3, so \
         this scenario has shown nothing M10's other tests had not"
    );

    irq::report();
    println!(
        "[irq] ring 3: a thread at cpl 3 blocked in Wait, a hardware \
         interrupt on vector {} arrived, and the thread resumed with \
         badge {:#010x}. The device reached a driver outside the \
         kernel, which is what M10 is for.",
        apic::irq_to_vector(irq::TEST_IRQ),
        result
    );
    println!("[irq] all ring 3 interrupt checks passed");

    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}
