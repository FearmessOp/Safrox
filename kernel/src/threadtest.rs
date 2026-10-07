// SPDX-License-Identifier: BSD-2-Clause
//! Thread and scheduler acceptance tests.
//!
//! M6's risk is not the context switch -- that is forty lines of
//! assembly with a clear contract. The risk is that interrupts are
//! enabled for the first time since M2, invalidating an assumption
//! that was never written down because it was universal. So several of
//! these checks are about IF rather than about threads.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::cap::cnode::{self, CNode};
use crate::cap::tcb::{self, ThreadState};
use crate::cap::untyped;
use crate::cap::{boot, CapError, CapType};
use crate::memory::{layout, VirtAddr};
use crate::sched;
use crate::{apic, println};

/// KSTACK slot of the first test thread.
///
/// Slots 0 and 1 belong to IST1 and the main stack (ADR 009), so the
/// test threads start at 2.
const THREAD_A_SLOT: u64 = 2;
const THREAD_B_SLOT: u64 = 3;

/// How many times the two cooperative threads hand off to each other.
const PING_PONG_ROUNDS: u64 = 100;

/// How many switches the measurement times.
const MEASURE_SWITCHES: u64 = 10_000;

// ---- Shared state between the test threads ----------------------------
//
// Statics rather than captured variables: a thread entry point is an
// `extern "C" fn() -> !` with no arguments, because there is nowhere
// to put an argument -- the context has a RIP and an RSP and nothing
// else.

/// Rounds completed by the ping-pong pair.
static ROUNDS: AtomicU64 = AtomicU64::new(0);

/// Each thread records its own RSP here so the main thread can check
/// that the two never share a stack.
static RSP_A: AtomicU64 = AtomicU64::new(0);
static RSP_B: AtomicU64 = AtomicU64::new(0);

/// Set when a thread notices its partner's RSP inside its own stack.
static STACK_OVERLAP: AtomicBool = AtomicBool::new(false);

/// The spinning thread sets this so the test can see it ran at all.
static SPIN_ITERATIONS: AtomicU64 = AtomicU64::new(0);

/// RFLAGS.IF as observed by a thread on its very first instruction.
///
/// 0 = not yet recorded, 1 = interrupts were off, 2 = they were on.
/// Three states rather than a bool because "never ran" and "ran with
/// interrupts disabled" are different failures and must not share a
/// value.
static SPIN_IF_AT_ENTRY: AtomicU64 = AtomicU64::new(0);

/// Tells the spinning thread to stop.
static SPIN_STOP: AtomicBool = AtomicBool::new(false);

/// Read RSP.
fn rsp() -> u64 {
    let value: u64;
    // SAFETY: Reading the stack pointer into a general register has no
    // side effects and touches no memory.
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Read the timestamp counter.
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

// ---- Building threads --------------------------------------------------

/// Everything one test thread needs.
struct TestThread {
    /// Index of the slot in the object CNode holding its TCB cap.
    slot_index: usize,
    stack_slot: u64,
    stack_top: VirtAddr,
    stack_bottom: VirtAddr,
}

/// The CNode the test retypes into.
fn object_cnode() -> CNode {
    // SAFETY: boot::init put a CNode in this root slot.
    unsafe { boot::leaf_cnode(boot::ROOT_SLOT_OBJECTS) }
}

/// Retype a TCB, map it a stack, and point it at an entry function.
///
/// The stack's frames come from the same Untyped as the TCB: ADR 026
/// says a thread's stack is charged to whoever created the thread, not
/// to the kernel. The intermediate page tables come from the same
/// place, which is the lesson ADR 022 learned in the heap.
fn create_thread(
    untyped_slot_index: usize,
    tcb_slot_index: usize,
    stack_slot: u64,
    entry: extern "C" fn() -> !,
) -> TestThread {
    let cnode = object_cnode();
    // SAFETY: Index below the count boot reported.
    let parent = unsafe { boot::root_untyped(untyped_slot_index) };

    // SAFETY: Live Untyped and CNode; the destination slot is empty.
    unsafe { untyped::retype(parent, CapType::Tcb, 0, 1, &cnode, tcb_slot_index) }
        .expect("could not retype a TCB");

    let (guard, bottom, top) = layout::kstack_slot(stack_slot);

    // The stack's frames, from the same Untyped. Four 4 KiB Frames for
    // a 16 KiB stack.
    let frames_needed = (layout::KSTACK_SIZE / crate::memory::FRAME_SIZE) as usize;
    let frame_base_slot = tcb_slot_index + 1;

    // SAFETY: Live Untyped and CNode; the destination slots are empty.
    unsafe {
        untyped::retype(
            parent,
            CapType::Frame,
            0,
            frames_needed,
            &cnode,
            frame_base_slot,
        )
    }
    .expect("could not retype the stack frames");

    // Map them into the thread's KSTACK slot, leaving the guard page
    // below unmapped.
    for index in 0..frames_needed {
        // SAFETY: Index in range.
        let frame_slot = unsafe { cnode.slot(frame_base_slot + index) };
        let frame = crate::memory::PhysFrame::containing(frame_slot.object().0);
        let page = VirtAddr::new(bottom.0 + (index as u64) * crate::memory::FRAME_SIZE);

        // SAFETY: The kernel's tables are live, the address is inside
        // this thread's unmapped KSTACK slot, and the frame came from
        // a retype so nothing else owns it. The intermediate tables
        // come from the heap's reserve, the only frame source left
        // after ADR 022's retirement.
        unsafe {
            let table = crate::memory::vm::kernel_table();
            let mut source = crate::memory::heap::reserve_frames();
            let flags = crate::memory::paging::PageFlags::READ
                | crate::memory::paging::PageFlags::WRITE
                | crate::memory::paging::PageFlags::GLOBAL;
            let tlb = crate::memory::paging::map(table, page, frame, flags, &mut source)
                .expect("could not map a thread stack page");
            tlb.flush();
        }
    }

    // SAFETY: Index in range.
    let tcb_slot = unsafe { cnode.slot(tcb_slot_index) };

    // SAFETY: The slot holds a live TCB; the stack is mapped, 16-byte
    // aligned by construction, and owned by nothing else. `entry`
    // diverges.
    unsafe { tcb::set_registers(tcb_slot, entry, top, bottom, stack_slot) }
        .expect("SetRegisters failed");

    // A thread that has never run must resume at the entry trampoline,
    // not at its entry point.
    //
    // Checked here, before the thread is ever queued, because the
    // consequence of getting it wrong cannot be observed afterwards.
    // Every switch happens inside a cli region whose `sti` is on the
    // line after `switch_context` returns -- a line a brand-new thread
    // does not pass through. Such a thread runs with IF clear, and if
    // it never yields it keeps the CPU permanently: the timer meant to
    // preempt it is masked in its own RFLAGS. There is then nothing
    // left to run an assertion on, and the failure appears only as a
    // thirty-second timeout naming nothing.
    //
    // So the invariant is checked on the stopped thread, while there
    // is still a CPU to report it with. Through ReadRegisters rather
    // than the TCB directly, so the capability path is what gets
    // tested (ADR 025).
    //
    // SAFETY: The slot holds a live TCB.
    let configured = unsafe { tcb::read_registers(tcb_slot) }.expect("ReadRegisters failed");
    assert_eq!(
        configured.rip,
        sched::entry_trampoline(),
        "a newly configured thread would resume at {:#018x} rather than \
         the entry trampoline at {:#018x}. Starting a thread directly at \
         its entry point skips the sti that ends the switch's cli \
         region, so it runs with interrupts disabled for its whole life \
         -- and if it never yields, nothing can take the CPU back from \
         it (ADR 025).",
        configured.rip,
        sched::entry_trampoline()
    );

    println!(
        "[thread] created: tcb slot {}, stack {} .. {} (slot {}), guard at {}",
        tcb_slot_index, bottom, top, stack_slot, guard
    );

    TestThread {
        slot_index: tcb_slot_index,
        stack_slot,
        stack_top: top,
        stack_bottom: bottom,
    }
}

/// Borrow a test thread's TCB.
///
/// # Safety
///
/// The caller must not hold another mutable borrow of the same TCB.
unsafe fn tcb_of(thread: &TestThread) -> &'static mut tcb::Tcb {
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let slot = unsafe { cnode.slot(thread.slot_index) };
    // SAFETY: The slot holds a live TCB; caller's aliasing guarantee.
    unsafe { tcb::from_slot(slot) }.expect("the slot no longer holds a TCB")
}

// ---- The cooperative pair ----------------------------------------------

/// Thread A: yields to B, counting rounds.
extern "C" fn thread_a() -> ! {
    loop {
        RSP_A.store(rsp(), Ordering::Release);

        // If B's recorded RSP falls inside A's stack, the two threads
        // are sharing memory and everything after this is meaningless.
        let b = RSP_B.load(Ordering::Acquire);
        if b != 0 {
            // SAFETY: We are thread A; `current` is our own TCB.
            let me = unsafe { sched::current() };
            if me.owns_stack_address(b) {
                STACK_OVERLAP.store(true, Ordering::Release);
            }
        }

        let round = ROUNDS.fetch_add(1, Ordering::AcqRel) + 1;
        if round >= PING_PONG_ROUNDS * 2 {
            // Done. Suspend so the main thread can take over.
            //
            // SAFETY: We are this thread; suspending the current
            // thread yields and does not return until resumed.
            unsafe {
                let me = sched::current();
                sched::suspend(me);
            }
        }

        sched::yield_now();
    }
}

/// Thread B: the same, from the other side.
extern "C" fn thread_b() -> ! {
    loop {
        RSP_B.store(rsp(), Ordering::Release);

        let a = RSP_A.load(Ordering::Acquire);
        if a != 0 {
            // SAFETY: We are thread B.
            let me = unsafe { sched::current() };
            if me.owns_stack_address(a) {
                STACK_OVERLAP.store(true, Ordering::Release);
            }
        }

        let round = ROUNDS.fetch_add(1, Ordering::AcqRel) + 1;
        if round >= PING_PONG_ROUNDS * 2 {
            // SAFETY: As in thread A.
            unsafe {
                let me = sched::current();
                sched::suspend(me);
            }
        }

        sched::yield_now();
    }
}

/// A thread that never yields.
///
/// The whole point: if this runs and the main thread ever regains the
/// CPU, the only thing that can have taken it away is the timer. That
/// is the difference between preemption and cooperation, and it cannot
/// be demonstrated by a thread that yields.
extern "C" fn thread_spin() -> ! {
    // The first thing this thread does, before anything can change it:
    // record whether interrupts are enabled.
    //
    // A thread that starts with IF=0 can never be preempted, and if it
    // also never yields it owns the CPU permanently -- the timer that
    // was supposed to take it away is masked in its own RFLAGS. That
    // is exactly what happened before the entry trampoline existed
    // (ADR 025), and it showed up as a thirty-second hang rather than
    // a failure. Recorded here so that the same mistake produces an
    // assertion naming its cause instead of a timeout naming nothing.
    SPIN_IF_AT_ENTRY.store(
        if sched::interrupts_enabled() { 2 } else { 1 },
        Ordering::Release,
    );

    loop {
        SPIN_ITERATIONS.fetch_add(1, Ordering::AcqRel);

        if SPIN_STOP.load(Ordering::Acquire) {
            // SAFETY: We are this thread.
            unsafe {
                let me = sched::current();
                sched::suspend(me);
            }
        }

        // Deliberately no yield, no hint, no pause. A `spin_loop`
        // hint would be polite and would also make the loop cheap
        // enough that the compiler might reason about it; this loop
        // exists to be interrupted.
        core::hint::black_box(SPIN_ITERATIONS.load(Ordering::Acquire));
    }
}

// ---- The checks --------------------------------------------------------

/// Run the thread test suite.
pub fn run() {
    println!("[thread] starting thread and scheduler tests");

    check_tcb_object();
    check_interrupt_flag_discipline();
    let (a, b) = check_cooperative_switch();
    check_timer_ticks();
    check_preemption();
    check_suspend_resume(&a);
    check_read_registers(&a);
    check_kill_running(&a, &b);
    measure_switch_cost();

    println!("[thread] all thread tests passed");
}

/// 1. A TCB is what ADR 017 says it is.
fn check_tcb_object() {
    use core::mem::{align_of, size_of};

    assert_eq!(
        size_of::<tcb::Tcb>(),
        crate::cap::TCB_SIZE,
        "the TCB is {} bytes, not the {} ADR 017 fixed",
        size_of::<tcb::Tcb>(),
        crate::cap::TCB_SIZE
    );
    assert_eq!(
        align_of::<tcb::Tcb>(),
        crate::cap::TCB_SIZE,
        "a TCB's alignment must equal its size, or retype's placement \
         arithmetic is wrong"
    );
    assert_eq!(
        size_of::<tcb::Context>(),
        64,
        "the context is {} bytes; the assembly's offsets assume 64",
        size_of::<tcb::Context>()
    );

    println!(
        "[thread] tcb: {} B, {}-byte aligned, {} B context, {} B reserved \
         for the IPC buffer",
        size_of::<tcb::Tcb>(),
        align_of::<tcb::Tcb>(),
        size_of::<tcb::Context>(),
        crate::cap::TCB_SIZE - tcb::IPC_BUFFER_OFFSET
    );
}

/// 2. The interrupt flag is where ADR 025 says it should be.
///
/// Three regions, three different answers, and the third is the one
/// that would be easy to get wrong: `revoke` runs with interrupts
/// *enabled* and preemption disabled, which is ADR 019's decision and
/// not an oversight.
fn check_interrupt_flag_discipline() {
    // The ambient state: interrupts are on.
    assert!(
        sched::interrupts_enabled(),
        "interrupts are disabled in ordinary kernel code; M6 enables them \
         at the end of boot and this check runs after that"
    );
    println!("[thread] IF discipline: ordinary kernel code runs with IF=1");

    // An InterruptGuard closes them, and restores on drop.
    {
        let _guard = sched::InterruptGuard::new();
        assert!(
            !sched::interrupts_enabled(),
            "an InterruptGuard did not disable interrupts"
        );
        println!("[thread] IF discipline: inside an InterruptGuard, IF=0");
    }
    assert!(
        sched::interrupts_enabled(),
        "an InterruptGuard did not restore IF on drop; every region after \
         this one would run unprotected"
    );

    // Nested guards: the inner one must not re-enable on the way out.
    {
        let _outer = sched::InterruptGuard::new();
        {
            let _inner = sched::InterruptGuard::new();
        }
        assert!(
            !sched::interrupts_enabled(),
            "a nested InterruptGuard re-enabled interrupts when the inner \
             one dropped; the outer region is now unprotected"
        );
    }
    println!("[thread] IF discipline: nested guards do not re-open the outer region");

    // A preempt guard leaves interrupts alone.
    {
        let _preempt = sched::PreemptGuard::new();
        assert!(
            sched::interrupts_enabled(),
            "a PreemptGuard disabled interrupts; it is supposed to stop \
             thread switching, not interrupt delivery (ADR 019)"
        );
        assert!(
            sched::preemption_disabled(),
            "a PreemptGuard did not disable preemption"
        );
        println!(
            "[thread] IF discipline: inside a PreemptGuard, IF=1 and \
             preemption is off -- this is what revoke uses (ADR 019)"
        );
    }
    assert!(
        !sched::preemption_disabled(),
        "a PreemptGuard did not restore preemption on drop"
    );

    // And the real thing: run a revoke and watch IF stay set.
    //
    // Observed rather than asserted in the abstract. A `cli` added to
    // revoke by mistake would make this line change.
    let cnode = object_cnode();
    // SAFETY: Index below the count boot reported.
    let parent = unsafe { boot::root_untyped(1) };
    // SAFETY: Live Untyped and CNode; slot 200 is unused by the thread
    // test's fixtures.
    unsafe { untyped::retype(parent, CapType::Endpoint, 0, 1, &cnode, 200) }
        .expect("could not retype the revoke subject");

    let if_during_revoke = {
        // SAFETY: Index in range.
        let slot = unsafe { cnode.slot(200) };
        // SAFETY: A live capability in the derivation list.
        let _report = unsafe { cnode::revoke(slot) };
        // Sampled after the call rather than inside it: `revoke` has no
        // hook, and what matters is that it did not leave IF clear.
        sched::interrupts_enabled()
    };
    assert!(
        if_during_revoke,
        "revoke left interrupts disabled; ADR 019 requires it to stay open \
         to them, because it can run for seconds and dropping device \
         interrupts for that long is not acceptable"
    );

    println!(
        "[thread] IF discipline: revoke ran and left IF=1 (ADR 019: open to \
         interrupts, closed to thread switching)"
    );
}

/// 3. Two threads hand off to each other, on separate stacks.
fn check_cooperative_switch() -> (TestThread, TestThread) {
    ROUNDS.store(0, Ordering::Release);
    RSP_A.store(0, Ordering::Release);
    RSP_B.store(0, Ordering::Release);
    STACK_OVERLAP.store(false, Ordering::Release);

    let a = create_thread(2, 0, THREAD_A_SLOT, thread_a);
    let b = create_thread(2, 8, THREAD_B_SLOT, thread_b);

    // The two stacks must not overlap. Checked here from the outside
    // as well as from inside the threads: the inside check proves the
    // running RSPs differ, this one proves the *ranges* do.
    assert!(
        a.stack_top.0 <= b.stack_bottom.0 || b.stack_top.0 <= a.stack_bottom.0,
        "thread A's stack {}..{} overlaps thread B's {}..{}",
        a.stack_bottom,
        a.stack_top,
        b.stack_bottom,
        b.stack_top
    );

    let switches_before = sched::switch_count();

    // SAFETY: Both TCBs are live and neither is queued.
    unsafe {
        sched::enqueue(tcb_of(&a));
        sched::enqueue(tcb_of(&b));
    }

    // Hand over. Each yield runs one of them until it yields back.
    //
    // Bounded: the threads suspend themselves after PING_PONG_ROUNDS*2
    // rounds, and this loop allows a generous margin beyond that
    // before giving up -- an unbounded wait would hang the test rather
    // than failing it.
    let mut handovers = 0u64;
    while ROUNDS.load(Ordering::Acquire) < PING_PONG_ROUNDS * 2 && handovers < PING_PONG_ROUNDS * 8
    {
        sched::yield_now();
        handovers += 1;
    }

    let rounds = ROUNDS.load(Ordering::Acquire);
    assert!(
        rounds >= PING_PONG_ROUNDS * 2,
        "only {} rounds completed out of {}; the threads are not running",
        rounds,
        PING_PONG_ROUNDS * 2
    );

    // The count may overshoot the target by exactly one, and the
    // bound is checked rather than waved away.
    //
    // Each thread increments the counter and only then looks at it, so
    // the thread that takes the count to 200 suspends itself -- but
    // its partner may already be on the ready queue, and a timer tick
    // will run it once more before it too notices and stops. With two
    // threads that is one extra round and no more.
    //
    // Two is a different matter: it would mean a thread kept being
    // scheduled after calling `suspend` on itself, which is the one
    // thing `suspend` promises not to allow. This is the check that
    // would catch it, so the bound is tight on purpose.
    //
    // Before M6 this was deterministic, because nothing could
    // interrupt a thread between the increment and the test. Enabling
    // interrupts made the exact figure a timing accident; the *bound*
    // is still an invariant.
    assert!(
        rounds <= PING_PONG_ROUNDS * 2 + 1,
        "{} rounds completed where at most {} were possible. A thread \
         ran after suspending itself: each thread checks the counter \
         immediately after incrementing it, so with two threads the \
         count can exceed the target by one and no more.",
        rounds,
        PING_PONG_ROUNDS * 2 + 1
    );

    assert!(
        !STACK_OVERLAP.load(Ordering::Acquire),
        "a thread found its partner's RSP inside its own stack; the two \
         are sharing memory"
    );

    let rsp_a = RSP_A.load(Ordering::Acquire);
    let rsp_b = RSP_B.load(Ordering::Acquire);
    assert!(rsp_a != 0 && rsp_b != 0, "a thread never recorded its RSP");

    // SAFETY: Both TCBs live; no other borrow is held here.
    let (count_a, count_b) = unsafe { (tcb_of(&a).switch_count(), tcb_of(&b).switch_count()) };
    assert!(
        count_a > 0 && count_b > 0,
        "one of the threads was never scheduled: A ran {} times, B ran {}",
        count_a,
        count_b
    );

    let switches = sched::switch_count() - switches_before;

    // Both test threads have suspended themselves, so the scheduler's
    // current thread must be this one. A mismatch would mean a switch
    // left `current` pointing at a thread that is not running -- which
    // the next timer tick would then try to save registers into.
    // SAFETY: Both TCBs live; no other borrow is held here.
    let (phys_a, phys_b) = unsafe { (tcb::tcb_phys(tcb_of(&a)).0, tcb::tcb_phys(tcb_of(&b)).0) };
    let current = sched::current_phys();
    assert!(
        current != phys_a && current != phys_b,
        "the scheduler still thinks a suspended test thread ({:#018x}) is \
         current",
        current
    );

    // The two threads occupy the KSTACK slots they were given, and
    // different ones. Checked through the TCB's own record rather than
    // the local, so a `configure` that stored the wrong slot shows up.
    // SAFETY: Both TCBs live.
    let (slot_a, slot_b) = unsafe { (tcb_of(&a).stack_slot(), tcb_of(&b).stack_slot()) };
    assert_eq!(slot_a, THREAD_A_SLOT);
    assert_eq!(slot_b, THREAD_B_SLOT);
    assert_ne!(
        slot_a, slot_b,
        "both threads recorded KSTACK slot {}; they would share a stack",
        slot_a
    );

    println!(
        "[thread] cooperative: {} rounds over {} switches; A at {:#018x} \
         (ran {}x, kstack slot {}), B at {:#018x} (ran {}x, kstack slot \
         {}); neither RSP is in the other's stack",
        rounds, switches, rsp_a, count_a, slot_a, rsp_b, count_b, slot_b
    );

    (a, b)
}

/// 4. The timer fires and keeps firing.
fn check_timer_ticks() {
    let start = apic::timer_interrupts();

    // Wait for 100 ticks. Bounded by a TSC deadline so that a dead
    // timer fails the test instead of hanging it -- which, with
    // interrupts enabled and nothing else to run, it otherwise would.
    const TARGET: u64 = 100;
    let deadline = read_tsc() + 20_000_000_000;

    while apic::timer_interrupts() - start < TARGET && read_tsc() < deadline {
        core::hint::spin_loop();
    }

    let taken = apic::timer_interrupts() - start;
    assert!(
        taken >= TARGET,
        "only {} timer interrupts arrived out of {}; the LAPIC timer is \
         not running, or EOI is not being written (after which the LAPIC \
         delivers nothing further)",
        taken,
        TARGET
    );

    // The scheduler must have seen them too. A gap between the two
    // counters means ticks were taken but not processed.
    let scheduler_ticks = sched::tick_count();
    assert!(
        scheduler_ticks > 0,
        "the LAPIC counted {} interrupts but the scheduler saw no ticks",
        taken
    );

    assert_eq!(
        apic::spurious_interrupts(),
        0,
        "{} spurious interrupts arrived; something is masking an interrupt \
         between raising and delivery",
        apic::spurious_interrupts()
    );

    println!(
        "[thread] timer: {} interrupts taken, {} ticks processed, {} \
         deferred by preempt-disabled regions, 0 spurious",
        taken,
        scheduler_ticks,
        sched::deferred_ticks()
    );
    sched::report();
}

/// 5. A thread that never yields is still taken off the CPU.
fn check_preemption() {
    SPIN_ITERATIONS.store(0, Ordering::Release);
    SPIN_STOP.store(false, Ordering::Release);
    SPIN_IF_AT_ENTRY.store(0, Ordering::Release);

    let spinner = create_thread(3, 16, 4, thread_spin);

    let switches_before = sched::switch_count();
    let ticks_before = sched::tick_count();

    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(tcb_of(&spinner));
    }

    // Hand the CPU to the spinner. It will never give it back
    // voluntarily -- so if this call ever returns, something took the
    // CPU away from it, and the only thing that can is the timer.
    sched::yield_now();

    let iterations = SPIN_ITERATIONS.load(Ordering::Acquire);
    let switches = sched::switch_count() - switches_before;
    let ticks = sched::tick_count() - ticks_before;

    assert!(
        iterations > 0,
        "the spinning thread never ran; there is nothing to preempt"
    );

    // Checked before the tick count, because it explains it. A thread
    // that started with IF clear cannot be preempted, and every
    // assertion below would fail as a consequence of this one.
    assert_eq!(
        SPIN_IF_AT_ENTRY.load(Ordering::Acquire),
        2,
        "the thread began executing with interrupts disabled. Every \
         switch happens inside a cli region whose sti is on the line \
         after switch_context returns -- a line a thread that has never \
         run does not pass through. It needs the entry trampoline \
         (ADR 025); without it a thread that does not yield keeps the \
         CPU forever, because the timer is masked in its own RFLAGS."
    );
    assert!(
        ticks > 0,
        "control returned from a thread that never yields, but no timer \
         tick was recorded; something other than preemption switched back \
         and the test proves nothing"
    );

    println!(
        "[thread] preemption: the spinner started with IF=1, ran {} \
         iterations without yielding; {} ticks and {} switches brought \
         control back here",
        iterations, ticks, switches
    );

    // Stop it and take it out of the queue.
    SPIN_STOP.store(true, Ordering::Release);

    // Bounded wait for it to notice.
    let deadline = read_tsc() + 20_000_000_000;
    loop {
        // SAFETY: Live TCB; no other borrow held.
        let state = unsafe { tcb_of(&spinner) }.state();
        if state == ThreadState::Suspended || read_tsc() > deadline {
            break;
        }
        sched::yield_now();
    }

    // SAFETY: Live TCB.
    let final_state = unsafe { tcb_of(&spinner) }.state();
    assert_eq!(
        final_state,
        ThreadState::Suspended,
        "the spinner did not suspend itself; it is still {}",
        final_state.name()
    );
}

/// 6. Suspend takes a thread out of the rotation; resume puts it back.
fn check_suspend_resume(thread: &TestThread) {
    // SAFETY: Live TCB; no other borrow held in this scope.
    let tcb = unsafe { tcb_of(thread) };

    // The ping-pong threads suspended themselves at the end of check 3.
    assert_eq!(
        tcb.state(),
        ThreadState::Suspended,
        "the thread should have suspended itself; it is {}",
        tcb.state().name()
    );

    let count_before = tcb.switch_count();
    let ready_before = sched::ready_count();

    // A suspended thread is not in the queue, so yielding must not
    // reach it.
    for _ in 0..8 {
        sched::yield_now();
    }
    assert_eq!(
        tcb.switch_count(),
        count_before,
        "a suspended thread was scheduled {} more times; the scheduler is \
         not skipping it",
        tcb.switch_count() - count_before
    );

    // Resume: back in the queue, and it runs again.
    // SAFETY: Live TCB in a resumable state.
    unsafe { sched::resume(tcb) }.expect("resume failed");
    assert_eq!(
        tcb.state(),
        ThreadState::Ready,
        "a resumed thread is {} rather than Ready",
        tcb.state().name()
    );
    assert_eq!(
        sched::ready_count(),
        ready_before + 1,
        "resume did not add the thread to the ready queue"
    );

    // Let it run. It will suspend itself again immediately -- its
    // round counter is already past the limit.
    sched::yield_now();

    assert!(
        tcb.switch_count() > count_before,
        "a resumed thread was never scheduled"
    );

    println!(
        "[thread] suspend/resume: a suspended thread was skipped by {} \
         yields, then resumed and ran again (switch count {} -> {})",
        8,
        count_before,
        tcb.switch_count()
    );
}

/// 7. ReadRegisters returns something true about a stopped thread.
fn check_read_registers(thread: &TestThread) {
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let slot = unsafe { cnode.slot(thread.slot_index) };

    // Make sure it is stopped: reading a running thread's registers
    // gives an answer that is stale before it is returned.
    // SAFETY: Live TCB.
    let tcb = unsafe { tcb_of(thread) };
    if tcb.state() != ThreadState::Suspended {
        // SAFETY: Live TCB.
        unsafe {
            sched::suspend(tcb);
        }
    }

    // SAFETY: The slot holds a live TCB.
    let regs = unsafe { tcb::read_registers(slot) }.expect("ReadRegisters failed");

    assert_eq!(regs.state, ThreadState::Suspended);

    // RIP must be inside the kernel image: the thread is stopped
    // somewhere in `thread_a`, or in the scheduler on its behalf.
    assert!(
        regs.rip >= layout::KIMAGE_BASE,
        "the suspended thread's RIP is {:#018x}, outside the kernel image \
         -- the saved context is not a real one",
        regs.rip
    );

    // RSP must be inside *this thread's* stack. This is the check that
    // would catch a context switch which saved the wrong RSP.
    assert!(
        regs.rsp > thread.stack_bottom.0 && regs.rsp <= thread.stack_top.0,
        "the suspended thread's RSP is {:#018x}, outside its own stack \
         {}..{}",
        regs.rsp,
        thread.stack_bottom,
        thread.stack_top
    );

    // SetPriority stores, ReadRegisters returns. M6's scheduler does
    // not consult it (ADR 024) -- it is data, not policy, until M10.
    // SAFETY: Live TCB capability.
    unsafe { tcb::set_priority(slot, 42) }.expect("SetPriority failed");
    // SAFETY: As above.
    let after = unsafe { tcb::read_registers(slot) }.expect("ReadRegisters failed");
    assert_eq!(after.priority, 42, "SetPriority did not stick");

    println!(
        "[thread] ReadRegisters: suspended thread at rip={:#018x}, \
         rsp={:#018x} (inside its own stack), {} switches, priority {} \
         (stored, not used -- ADR 024)",
        regs.rip, regs.rsp, regs.switch_count, after.priority
    );

    println!(
        "[thread] invocations: {} went through the capability path",
        tcb::invocation_count()
    );
}

/// 8. Revoking a busy thread's TCB is refused, and the refusal is not
///    a dead end (ADR 026).
fn check_kill_running(suspended: &TestThread, busy: &TestThread) {
    let cnode = object_cnode();

    // Make the second thread ready, so it is genuinely busy.
    // SAFETY: Live TCB.
    let busy_tcb = unsafe { tcb_of(busy) };
    if busy_tcb.state() == ThreadState::Suspended {
        // SAFETY: Live TCB in a resumable state.
        unsafe { sched::resume(busy_tcb) }.expect("resume failed");
    }
    assert!(
        busy_tcb.state().is_busy(),
        "the thread meant to be busy is {}",
        busy_tcb.state().name()
    );

    // SAFETY: Index in range.
    let busy_slot = unsafe { cnode.slot(busy.slot_index) };

    // SetRegisters on a busy thread: refused. Changing RIP under a
    // thread the scheduler is holding would make it resume somewhere
    // it has never been.
    // SAFETY: Live TCB capability; expected to fail.
    match unsafe {
        tcb::set_registers(
            busy_slot,
            thread_spin,
            VirtAddr::new(busy.stack_top.0),
            VirtAddr::new(busy.stack_bottom.0),
            busy.stack_slot,
        )
    } {
        Err(CapError::ThreadBusy { state }) => {
            assert!(
                state == "Ready" || state == "Running",
                "ThreadBusy reported state {}, which is not a busy state",
                state
            );
        }
        other => panic!(
            "SetRegisters on a busy thread returned {:?} instead of \
             ThreadBusy",
            other
        ),
    }

    // Resume on a busy thread: also refused, and for the same reason
    // -- it is already in the queue, and queueing it twice would make
    // the list cyclic.
    // SAFETY: Live TCB; expected to fail.
    match unsafe { sched::resume(busy_tcb) } {
        Err(CapError::ThreadBusy { .. }) => {}
        other => panic!(
            "resuming an already-ready thread returned {:?} instead of \
             ThreadBusy",
            other
        ),
    }

    // The two-step kill (ADR 026): suspend always works, and after it
    // the thread is no longer busy. This is what makes refusing the
    // first step something other than a denial of service.
    // SAFETY: Live TCB.
    unsafe {
        sched::suspend(busy_tcb);
    }
    assert!(
        !busy_tcb.state().is_busy(),
        "suspend did not make the thread non-busy; it is {}",
        busy_tcb.state().name()
    );

    // And now the operation that was refused succeeds.
    // SAFETY: Live TCB capability, now suspended.
    unsafe {
        tcb::set_registers(
            busy_slot,
            thread_spin,
            VirtAddr::new(busy.stack_top.0),
            VirtAddr::new(busy.stack_bottom.0),
            busy.stack_slot,
        )
    }
    .expect("SetRegisters on a suspended thread should succeed");

    // The other thread was suspended all along, so operations on it
    // are allowed throughout -- the refusal is about state, not about
    // TCBs in general.
    // SAFETY: Index in range.
    let quiet_slot = unsafe { cnode.slot(suspended.slot_index) };
    // SAFETY: Live TCB capability.
    unsafe { tcb::set_priority(quiet_slot, 7) }.expect("SetPriority on a suspended thread");

    println!(
        "[thread] kill a running thread: SetRegisters and Resume both give \
         ThreadBusy; Suspend always works, and afterwards the same calls \
         succeed -- the two-step sequence of ADR 026"
    );
}

/// The measurement: what does a context switch cost?
///
/// Not pass/fail. M9's IPC will sit on top of this number, so M6
/// records a baseline.
fn measure_switch_cost() {
    // A second thread to switch with. The pair must both be ready, so
    // that each `yield_now` is a real switch rather than a no-op.
    ROUNDS.store(0, Ordering::Release);
    let partner = create_thread(4, 32, 5, thread_yield_forever);

    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(tcb_of(&partner));
    }

    let switches_before = sched::switch_count();
    let start = read_tsc();

    // Each iteration is two switches: out to the partner and back.
    for _ in 0..(MEASURE_SWITCHES / 2) {
        sched::yield_now();
    }

    let elapsed = read_tsc() - start;
    let switches = sched::switch_count() - switches_before;

    assert!(
        switches > 0,
        "no switches happened during the measurement; the partner thread \
         is not running"
    );

    let per_switch = elapsed / switches;

    println!(
        "[thread] switch cost: {} switches in {} TSC cycles ({} cycles \
         each)",
        switches, elapsed, per_switch
    );
    println!(
        "[thread] switch cost: ~{} ns at 3 GHz -- measured under emulation, \
         where the TSC is virtual and the figure is an order of magnitude, \
         not a measurement. M9's IPC will be compared against it.",
        per_switch * 1000 / 3000
    );

    // Stop the partner so later code is not competing with it.
    // SAFETY: Live TCB.
    unsafe {
        let tcb = tcb_of(&partner);
        sched::suspend(tcb);
    }
}

/// A partner for the measurement: yields and nothing else.
extern "C" fn thread_yield_forever() -> ! {
    loop {
        sched::yield_now();
    }
}

// ---- Negative scenarios ------------------------------------------------

/// A thread that overflows its own stack.
///
/// M3 already tested a stack overflow, but that was the kernel's main
/// stack, mapped by `vm::init`. This one is a stack retyped from an
/// Untyped and mapped for a TCB -- a different path to the same guard
/// page, and the report must name *which thread* ran off its stack.
pub fn run_stack_overflow() -> ! {
    println!("[thread] stack overflow test: a thread will run off its own stack");

    let victim = create_thread(2, 0, THREAD_A_SLOT, thread_overflow);

    println!(
        "[thread] victim: tcb slot {}, stack {} .. {}, guard page below {}",
        victim.slot_index, victim.stack_bottom, victim.stack_top, victim.stack_bottom
    );

    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(tcb_of(&victim));
    }

    // Keep handing the CPU back to the victim until it runs off its
    // stack. One `yield_now` is not enough and the reason is worth
    // recording: the timer preempts the victim mid-recursion and
    // control returns here with the guard page still untouched. That
    // is preemption working, not the test failing -- so this loop
    // gives the CPU back rather than concluding anything from the
    // first return.
    //
    // Bounded by a TSC deadline: if the guard page is mapped, or the
    // recursion has been optimised into a loop, this must fail rather
    // than spin forever.
    let deadline = read_tsc() + 60_000_000_000;
    while read_tsc() < deadline {
        sched::yield_now();
    }

    unreachable!(
        "the thread recursed without ever reaching its guard page; either \
         the page below the stack is mapped -- in which case the overflow \
         silently corrupted whatever is there -- or the recursion was \
         optimised away and nothing was tested"
    );
}

/// Recurse until the guard page is hit.
extern "C" fn thread_overflow() -> ! {
    // Report which thread this is *before* overflowing, so the log
    // pairs the fault with the thread even though the fault handler
    // cannot name it: by the time #DF arrives on IST1, the thread's
    // own stack is unusable and `sched::current()` would have to be
    // trusted on a machine that has just proved it cannot be.
    //
    // SAFETY: We are this thread.
    let me = unsafe { sched::current() };
    println!(
        "[thread] overflowing: stack {:#018x}..{:#018x}, rsp now {:#018x}",
        me.stack_bottom(),
        me.stack_top(),
        rsp()
    );

    recurse(0);
}

/// Recurse with a frame big enough to reach the guard quickly.
///
/// `#[inline(never)]` and the volatile writes are both load-bearing:
/// an inlined or elided recursion does not grow the stack, and M3's
/// version of this test passed for exactly that wrong reason until
/// the volatile write was added.
///
/// `#[allow(unconditional_recursion)]` because that is the entire
/// point. The lint is right that this never returns by itself -- the
/// guard page is what ends it, and the lint cannot know that.
#[inline(never)]
#[allow(
    unconditional_recursion,
    reason = "the recursion is the test; the guard page terminates it"
)]
fn recurse(depth: u64) -> ! {
    let mut padding = [0u64; 64];
    // SAFETY: `padding` is a live local. The volatile writes stop the
    // optimiser from removing an array nothing reads -- which would
    // shrink the frame and make the overflow take far longer, or not
    // happen at all if the recursion became a loop.
    unsafe {
        core::ptr::write_volatile(&mut padding[0], depth);
        core::ptr::write_volatile(&mut padding[63], depth);
    }

    if depth.is_multiple_of(8) {
        println!("[thread] depth {}, rsp {:#018x}", depth, rsp());
    }

    recurse(depth + 1)
}

/// Allocate a frame from inside a real timer interrupt.
///
/// M5 tested this by driving the interrupt-depth counter by hand,
/// because there were no interrupts. This is the real thing: the
/// request happens inside the handler the LAPIC dispatched to.
pub fn run_interrupt_alloc() -> ! {
    println!("[thread] allocating from inside a real timer interrupt");

    ALLOCATE_IN_HANDLER.store(true, Ordering::Release);

    println!(
        "[thread] armed; waiting for the next timer tick (must panic in \
         the handler)"
    );

    // Bounded: if the timer is dead this fails rather than hanging.
    let deadline = read_tsc() + 20_000_000_000;
    while read_tsc() < deadline {
        core::hint::spin_loop();
    }

    unreachable!(
        "no timer interrupt arrived within the deadline, so the tripwire \
         was never reached"
    );
}

/// Set by `run_interrupt_alloc`; read by the timer handler.
pub static ALLOCATE_IN_HANDLER: AtomicBool = AtomicBool::new(false);

/// Called from the timer handler when the scenario has armed it.
///
/// Lives here rather than in `interrupts` so that the test's intent is
/// beside the test.
pub fn allocate_from_handler_if_armed() {
    if !ALLOCATE_IN_HANDLER.load(Ordering::Acquire) {
        return;
    }
    ALLOCATE_IN_HANDLER.store(false, Ordering::Release);

    println!(
        "[thread] inside the timer handler, in_interrupt={}; calling \
         allocate() (must panic)",
        crate::memory::frame_allocator::in_interrupt()
    );

    let frame = crate::memory::frame_allocator::allocate();

    unreachable!(
        "allocation inside a real interrupt handler returned {:?} instead \
         of panicking; ADR 022's tripwire does not work on the path that \
         matters",
        frame.map(|f| f.0)
    );
}

/// Revoke the Untyped a running thread's TCB came from.
///
/// ADR 026: refused with `ThreadBusy` rather than left undefined. The
/// scenario exits 35 because it panics on the *absence* of the
/// refusal -- the refusal itself is the success path and is checked in
/// `check_kill_running`.
pub fn run_kill_running() -> ! {
    println!("[thread] revoking a running thread's TCB");

    ROUNDS.store(0, Ordering::Release);
    let victim = create_thread(2, 0, THREAD_A_SLOT, thread_yield_forever);

    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(tcb_of(&victim));
    }

    // Let it run once so it is genuinely in the rotation.
    sched::yield_now();

    // SAFETY: Live TCB.
    let state = unsafe { tcb_of(&victim) }.state();
    assert!(
        state.is_busy(),
        "the victim is {} rather than busy; the scenario would not be \
         testing anything",
        state.name()
    );

    println!(
        "[thread] the victim is {}; attempting SetRegisters",
        state.name()
    );

    let cnode = object_cnode();
    // SAFETY: Index in range.
    let slot = unsafe { cnode.slot(victim.slot_index) };

    // SAFETY: Live TCB capability; expected to be refused.
    let result = unsafe {
        tcb::set_registers(
            slot,
            thread_yield_forever,
            victim.stack_top,
            victim.stack_bottom,
            victim.stack_slot,
        )
    };

    match result {
        Err(CapError::ThreadBusy { state }) => {
            println!(
                "[thread] refused with ThreadBusy({}), as ADR 026 requires",
                state
            );
        }
        other => panic!(
            "operating on a busy thread returned {:?} instead of \
             ThreadBusy; ADR 026 forbids leaving this undefined",
            other
        ),
    }

    // Now do it properly and show the sequence terminates.
    // SAFETY: Live TCB.
    unsafe {
        let tcb = tcb_of(&victim);
        sched::suspend(tcb);
    }

    // SAFETY: Live TCB capability, now suspended.
    unsafe {
        tcb::set_registers(
            slot,
            thread_yield_forever,
            victim.stack_top,
            victim.stack_bottom,
            victim.stack_slot,
        )
    }
    .expect("the same call must succeed once the thread is suspended");

    println!(
        "[thread] after Suspend the same call succeeds: the refusal is a \
         sequencing requirement, not a dead end"
    );

    // The scenario's own failure mode: panic deliberately, so the
    // runner sees 35 and the log carries the evidence above.
    panic!(
        "thread-kill-running completed its checks; exiting 35 by design so \
         that the ThreadBusy path is asserted on a panic-expecting run"
    );
}
