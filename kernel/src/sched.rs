// SPDX-License-Identifier: BSD-2-Clause
//! Context switching and the round-robin scheduler.
//!
//! Two things live here and they are deliberately separable:
//!
//! * **Mechanism** -- [`switch_context`] saves one thread's registers
//!   and loads another's. This stays in the kernel forever.
//! * **Policy** -- the ready queue and the choice of what runs next.
//!   This is temporary. ADR 024 explains why it cannot leave yet (a
//!   user-space scheduler needs IPC, and IPC is M9) and names exactly
//!   which parts move out in M10.
//!
//! The module's public surface is three functions, which is the seam
//! the eventual move happens along: [`yield_now`], [`tick`] and
//! [`current`]. The queue itself is private.
//!
//! # The intrusive queue
//!
//! The ready queue is a singly linked list threaded through the TCBs,
//! not a `VecDeque`. The scheduler runs inside a timer interrupt, and
//! ADR 022 forbids allocation there -- a queue that could allocate
//! would trip the tripwire the moment the system got busy. The same
//! reasoning put the derivation list inside the capability slots
//! (ADR 019).

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::cap::tcb::{self, Context, ThreadState};
use crate::memory::PhysAddr;
use crate::println;

// ---- Interrupt-flag helpers (ADR 025) ----------------------------------

/// RFLAGS.IF, bit 9.
///
/// Reference: Intel SDM Vol. 1, Figure 3-8 "EFLAGS Register".
const RFLAGS_IF: u64 = 1 << 9;

/// Read RFLAGS.
pub fn read_rflags() -> u64 {
    let flags: u64;
    // SAFETY: `pushfq` pushes RFLAGS and `pop` takes it back off. Both
    // touch the stack, so `nostack` would be a lie; everything else is
    // side-effect free.
    unsafe {
        core::arch::asm!("pushfq; pop {}", out(reg) flags, options(preserves_flags));
    }
    flags
}

/// Whether interrupts are currently enabled.
pub fn interrupts_enabled() -> bool {
    read_rflags() & RFLAGS_IF != 0
}

/// A region that must not be interrupted, with the previous IF state
/// remembered.
///
/// Restoring the saved flag rather than unconditionally executing `sti`
/// is what makes nesting safe: an inner region that ended with `sti`
/// would silently re-open an outer one. ADR 025 names this as the rule.
pub struct InterruptGuard {
    was_enabled: bool,
}

impl InterruptGuard {
    /// Disable interrupts, remembering whether they were on.
    pub fn new() -> InterruptGuard {
        let was_enabled = interrupts_enabled();
        // SAFETY: `cli` only clears IF. It cannot fault at CPL 0, and
        // the guard's `Drop` restores the previous state on every path
        // out of the region, including a panic.
        unsafe {
            core::arch::asm!("cli", options(nomem, nostack));
        }
        InterruptGuard { was_enabled }
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        if self.was_enabled {
            // SAFETY: Restoring the flag this guard cleared. Nothing
            // between `new` and here may rely on IF staying clear past
            // the end of the region -- that is the region's contract.
            unsafe {
                core::arch::asm!("sti", options(nomem, nostack));
            }
        }
    }
}

/// Enable interrupts for the first time.
///
/// Called once, at the end of boot, after the IDT, the LAPIC timer and
/// the scheduler are all up. Every assumption written before this point
/// under "interrupts are disabled" stops holding here; ADR 025 lists
/// the regions that still need protection and how they get it.
///
/// # Safety
///
/// The IDT must be loaded with handlers for every vector that can now
/// fire, and the scheduler must be initialised: a timer interrupt
/// arriving before there is a current thread has nothing to switch
/// from.
pub unsafe fn enable_interrupts() {
    assert!(
        !interrupts_enabled(),
        "interrupts were already enabled; this is the one place that turns \
         them on and it has run twice"
    );
    assert!(
        CURRENT.load(Ordering::Acquire) != 0,
        "interrupts enabled before the scheduler has a current thread; the \
         first timer tick would have nothing to switch from"
    );

    // SAFETY: Caller's guarantee that the IDT and the scheduler are
    // ready.
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }
}

// ---- Preemption control (ADR 025) --------------------------------------

/// Nesting depth of "no thread switch here" regions.
///
/// Distinct from `cli`, and the distinction is the point. A region that
/// disables interrupts loses device data; a region that only disables
/// *preemption* still takes interrupts, counts the tick, and simply
/// does not switch threads. `revoke` runs in the second kind, which is
/// what ADR 019 meant by "open to interrupts, closed to thread
/// switching".
static PREEMPT_DISABLE: AtomicUsize = AtomicUsize::new(0);

/// Ticks that arrived while preemption was disabled.
///
/// Counted rather than dropped: the acceptance test uses this to show
/// that a long preempt-disabled region does not lose time, it defers
/// it.
static DEFERRED_TICKS: AtomicU64 = AtomicU64::new(0);

/// A region in which the scheduler will not switch threads.
pub struct PreemptGuard {
    _private: (),
}

impl PreemptGuard {
    pub fn new() -> PreemptGuard {
        PREEMPT_DISABLE.fetch_add(1, Ordering::AcqRel);
        PreemptGuard { _private: () }
    }
}

impl Drop for PreemptGuard {
    fn drop(&mut self) {
        let previous = PREEMPT_DISABLE.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "preempt-disable underflow");
    }
}

/// Whether thread switching is currently forbidden.
pub fn preemption_disabled() -> bool {
    PREEMPT_DISABLE.load(Ordering::Acquire) != 0
}

/// How many ticks were deferred by preempt-disabled regions.
pub fn deferred_ticks() -> u64 {
    DEFERRED_TICKS.load(Ordering::Acquire)
}

// ---- The ready queue ---------------------------------------------------
//
// Physical addresses rather than references: a TCB lives in memory
// carved out of an Untyped, and there is no Rust lifetime that
// describes it. The same choice as the derivation list (ADR 019).

/// The running thread, by physical address. Zero before `init`.
static CURRENT: AtomicU64 = AtomicU64::new(0);

/// Head and tail of the ready queue.
static QUEUE_HEAD: AtomicU64 = AtomicU64::new(0);
static QUEUE_TAIL: AtomicU64 = AtomicU64::new(0);

/// Total context switches, for the measurement.
static SWITCH_COUNT: AtomicU64 = AtomicU64::new(0);

/// Switches that reloaded CR3 because the address space changed.
static CR3_SWITCHES: AtomicU64 = AtomicU64::new(0);

/// Switches that left CR3 alone because both threads share a VSpace.
///
/// Counted separately rather than derived, and the acceptance test
/// requires *both* to be non-zero. A single "CR3 switches" counter
/// would be satisfied by an implementation that reloads on every
/// switch -- which is correct but throws away the whole non-global TLB
/// for nothing (ADR 027).
static CR3_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Threads stopped because their address space had been revoked.
static VSPACE_STALE: AtomicU64 = AtomicU64::new(0);

/// Timer ticks taken.
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn switch_count() -> u64 {
    SWITCH_COUNT.load(Ordering::Acquire)
}

pub fn tick_count() -> u64 {
    TICK_COUNT.load(Ordering::Acquire)
}

pub fn cr3_switch_count() -> u64 {
    CR3_SWITCHES.load(Ordering::Acquire)
}

pub fn cr3_skipped_count() -> u64 {
    CR3_SKIPPED.load(Ordering::Acquire)
}

pub fn stale_vspace_count() -> u64 {
    VSPACE_STALE.load(Ordering::Acquire)
}

/// Root page table of the running thread, or zero for the kernel's.
///
/// ADR 028's case A turns on this: revoking the VSpace that is in CR3
/// right now would hand the live page tables back to their Untyped
/// while the CPU is still walking them.
pub fn current_vspace() -> u64 {
    let phys = CURRENT.load(Ordering::Acquire);
    if phys == 0 {
        return 0;
    }
    // SAFETY: CURRENT names a live TCB.
    let tcb = unsafe { tcb::tcb_at(crate::cap::ObjectRef(PhysAddr(phys))) };
    tcb.vspace_root()
}

/// The running thread.
///
/// # Safety
///
/// The caller must not hold another mutable borrow of the same TCB.
pub unsafe fn current() -> &'static mut tcb::Tcb {
    let phys = CURRENT.load(Ordering::Acquire);
    assert!(phys != 0, "sched::current() before sched::init()");
    // SAFETY: CURRENT only ever holds the address of a live TCB, and
    // the caller promises not to alias it.
    unsafe { tcb::tcb_at(crate::cap::ObjectRef(PhysAddr(phys))) }
}

/// Physical address of the running thread, without borrowing it.
pub fn current_phys() -> u64 {
    CURRENT.load(Ordering::Acquire)
}

/// Put a thread on the back of the ready queue.
///
/// # Safety
///
/// `thread` must be a live TCB that is not already queued. Queueing a
/// thread twice would make the list cyclic and the scheduler would
/// hand the same thread out repeatedly.
pub unsafe fn enqueue(thread: &mut tcb::Tcb) {
    let _guard = InterruptGuard::new();

    // SAFETY: Every TCB lives in the physical window.
    let phys = unsafe { tcb::tcb_phys(thread) }.0;

    debug_assert!(
        thread.next_ready() == 0,
        "the thread is already linked into a queue"
    );
    // SAFETY: Caller holds the scheduler invariants; we are inside the
    // interrupt-free region that protects them.
    unsafe {
        thread.set_state(ThreadState::Ready);
    }
    thread.set_next_ready(0);

    let tail = QUEUE_TAIL.load(Ordering::Acquire);
    if tail == 0 {
        QUEUE_HEAD.store(phys, Ordering::Release);
    } else {
        // SAFETY: A non-zero tail is the address of a live TCB.
        let tail_tcb = unsafe { tcb::tcb_at(crate::cap::ObjectRef(PhysAddr(tail))) };
        tail_tcb.set_next_ready(phys);
    }
    QUEUE_TAIL.store(phys, Ordering::Release);
}

/// Take the front of the ready queue, or `None` if it is empty.
///
/// # Safety
///
/// Must be called with interrupts disabled.
unsafe fn dequeue() -> Option<&'static mut tcb::Tcb> {
    let head = QUEUE_HEAD.load(Ordering::Acquire);
    if head == 0 {
        return None;
    }

    // SAFETY: A non-zero head is the address of a live TCB.
    let thread = unsafe { tcb::tcb_at(crate::cap::ObjectRef(PhysAddr(head))) };
    let next = thread.next_ready();

    QUEUE_HEAD.store(next, Ordering::Release);
    if next == 0 {
        QUEUE_TAIL.store(0, Ordering::Release);
    }
    thread.set_next_ready(0);

    Some(thread)
}

/// Take a specific thread out of the queue, if it is there.
///
/// O(queue length), and that is acceptable because it happens only on
/// `Suspend` -- not on any hot path. A doubly linked queue would make
/// it O(1) at the cost of another field in every TCB; the trade is
/// recorded here rather than silently made.
///
/// # Safety
///
/// Must be called with interrupts disabled.
unsafe fn remove_from_queue(target: u64) -> bool {
    let mut previous = 0u64;
    let mut cursor = QUEUE_HEAD.load(Ordering::Acquire);

    // Bounded by the queue length, which is bounded by the number of
    // TCBs in the system -- no unbounded loop on a kernel path.
    while cursor != 0 {
        // SAFETY: A non-zero link is the address of a live TCB.
        let node = unsafe { tcb::tcb_at(crate::cap::ObjectRef(PhysAddr(cursor))) };
        let next = node.next_ready();

        if cursor == target {
            if previous == 0 {
                QUEUE_HEAD.store(next, Ordering::Release);
            } else {
                // SAFETY: As above.
                let prev_tcb = unsafe { tcb::tcb_at(crate::cap::ObjectRef(PhysAddr(previous))) };
                prev_tcb.set_next_ready(next);
            }
            if next == 0 {
                QUEUE_TAIL.store(previous, Ordering::Release);
            }
            node.set_next_ready(0);
            return true;
        }

        previous = cursor;
        cursor = next;
    }

    false
}

/// Number of threads waiting to run.
pub fn ready_count() -> usize {
    let _guard = InterruptGuard::new();
    let mut count = 0usize;
    let mut cursor = QUEUE_HEAD.load(Ordering::Acquire);
    while cursor != 0 {
        count += 1;
        // SAFETY: Non-zero links name live TCBs.
        let node = unsafe { tcb::tcb_at(crate::cap::ObjectRef(PhysAddr(cursor))) };
        cursor = node.next_ready();
        assert!(
            count <= 1024,
            "the ready queue is longer than any plausible thread count; it \
             has a cycle, which means a thread was enqueued twice"
        );
    }
    count
}

// ---- The switch itself -------------------------------------------------

unsafe extern "C" {
    /// Save `from`'s callee-saved registers and load `to`'s.
    ///
    /// Defined in the assembly below. Returns to the caller of
    /// whichever thread it switched *to* -- so from the caller's point
    /// of view it returns much later, in a different thread's time,
    /// with the same stack it had.
    fn switch_context(from: *mut Context, to: *const Context);
}

core::arch::global_asm!(
    ".section .text",
    ".code64",
    ".align 16",
    ".global switch_context",
    "switch_context:",
    // RDI = &mut from, RSI = &from (SysV AMD64 argument registers).
    //
    // Only the callee-saved set. The caller-saved registers are already
    // the caller's problem under the ABI -- this is an ordinary
    // function call as far as the compiler is concerned, and it has
    // spilled anything it still needs.
    "    mov [rdi + {rbx}], rbx",
    "    mov [rdi + {rbp}], rbp",
    "    mov [rdi + {r12}], r12",
    "    mov [rdi + {r13}], r13",
    "    mov [rdi + {r14}], r14",
    "    mov [rdi + {r15}], r15",
    // The return address is the resume point. Taking it off the stack
    // and storing it as RIP is what lets a thread be resumed by a
    // `jmp` later: the switch into this thread will push it back.
    "    pop rax",
    "    mov [rdi + {rip}], rax",
    "    mov [rdi + {rsp}], rsp",
    // Load the new thread.
    "    mov rsp, [rsi + {rsp}]",
    "    mov rbx, [rsi + {rbx}]",
    "    mov rbp, [rsi + {rbp}]",
    "    mov r12, [rsi + {r12}]",
    "    mov r13, [rsi + {r13}]",
    "    mov r14, [rsi + {r14}]",
    "    mov r15, [rsi + {r15}]",
    // Push the new thread's resume address and return to it. `ret`
    // rather than `jmp [rsi + rip]`: the indirect jump would leave the
    // return-address predictor mismatched, and `ret` is what the CPU's
    // return-stack buffer expects after the `pop` above.
    "    mov rax, [rsi + {rip}]",
    "    push rax",
    "    ret",
    rbx = const tcb::context_offset::RBX,
    rbp = const tcb::context_offset::RBP,
    r12 = const tcb::context_offset::R12,
    r13 = const tcb::context_offset::R13,
    r14 = const tcb::context_offset::R14,
    r15 = const tcb::context_offset::R15,
    rsp = const tcb::context_offset::RSP,
    rip = const tcb::context_offset::RIP,
);

// ---- Where a thread that has never run begins --------------------------

unsafe extern "C" {
    /// The first instruction executed by every brand-new thread.
    fn thread_entry_trampoline();
}

// Every switch happens inside an interrupt-free region: `yield_now`,
// `suspend` and `tick` all hold IF clear across `switch_context`,
// because a half-written RSP is not a thing an interrupt can be taken
// on (ADR 025). The region is reopened by dropping the guard *after*
// the switch returns.
//
// That works for a thread that has run before, because its saved RIP
// points back into the function that holds the guard. A thread that
// has never run has no such frame: its saved RIP is its entry point,
// and `switch_context`'s `ret` lands there directly, skipping every
// `sti` on the way. It would run with interrupts disabled forever --
// and a thread that does not yield would then own the CPU for good,
// because the timer that was supposed to take it away is masked in
// its own RFLAGS.
//
// So a new thread does not start at its entry point; it starts here.
// `switch_context` has just loaded the callee-saved set, so R12 holds
// whatever `Tcb::configure` put there -- the real entry point. Open
// the interrupt window, then jump.
//
// `jmp`, not `call`: the entry point is `-> !` and has no return
// address beneath it. The stack `configure` built is already aligned
// as if a `call` had happened, so the ABI is satisfied on arrival.
core::arch::global_asm!(
    ".section .text",
    ".code64",
    ".align 16",
    ".global thread_entry_trampoline",
    "thread_entry_trampoline:",
    "    sti",
    "    jmp r12",
);

/// The address every freshly configured thread resumes at.
///
/// [`tcb::Tcb::configure`] stores this as the thread's RIP and the real
/// entry point in R12.
pub fn entry_trampoline() -> u64 {
    // Through a pointer rather than straight to an integer: a direct
    // numeric cast of a function *item* is denied, and rightly -- it
    // is the kind of expression that silently becomes a cast of a
    // closure or a zero-sized type if the name ever changes meaning.
    (thread_entry_trampoline as *const ()) as u64
}

/// Install `next`'s address space, if it differs from the current one.
///
/// Returns `false` if the thread's VSpace has been revoked, in which
/// case the caller must not switch into it (ADR 028, case B).
///
/// # Safety
///
/// Interrupts must be disabled, and `next` must be a live TCB.
unsafe fn switch_address_space(next: &tcb::Tcb) -> bool {
    if !next.vspace_is_live() {
        // The address space was revoked while this thread was not
        // running. Switching in would put the CPU on page tables that
        // belong to somebody else now.
        VSPACE_STALE.fetch_add(1, Ordering::AcqRel);
        return false;
    }

    let root = next.vspace_root();

    // Zero means the kernel's own address space. The kernel PML4 is
    // where every thread starts and where the boot thread stays.
    let target = if root == 0 {
        crate::memory::vm::kernel_pml4()
    } else {
        PhysAddr(root)
    };

    let active = crate::memory::paging::active_table().start();

    if active.0 == target.0 {
        // Same address space: leave CR3 alone.
        //
        // Not an optimisation to be measured and kept if it pays --
        // writing CR3 discards every non-global TLB entry, and doing
        // that to arrive at the mappings already in use is pure loss.
        // The two counters exist so the test can tell this branch
        // from the other one (ADR 027).
        CR3_SKIPPED.fetch_add(1, Ordering::AcqRel);
        return true;
    }

    // SAFETY: Every VSpace carries the kernel's top-level entries,
    // copied by `vspace::create_vspace` and checked unchanged since
    // boot (ADR 027, K2). So the code executing right now, its stack,
    // the IDT and PMAP are all mapped identically in the new table --
    // which is the precondition `load_table` demands.
    unsafe {
        crate::memory::paging::load_table(crate::memory::PhysFrame::containing(target));
    }
    CR3_SWITCHES.fetch_add(1, Ordering::AcqRel);
    true
}

/// Switch to `next`, making the current thread ready again.
///
/// # Safety
///
/// Must be called with interrupts disabled and `next` off the queue.
unsafe fn switch_to(next: &'static mut tcb::Tcb) {
    debug_assert!(
        !interrupts_enabled(),
        "a context switch with interrupts enabled: an interrupt arriving \
         with RSP half-written has nowhere sensible to push (ADR 025)"
    );

    // The address space goes first, and that ordering is the whole
    // reason the kernel half is shared (ADR 027). CR3 changes while
    // this code is still running; the instructions after it, the
    // stack under it and the IDT above it must all still be reachable,
    // and they are, because every VSpace carries the same upper half.
    //
    // Doing it after `switch_context` would not work at all: by then
    // this function is not executing any more.
    //
    // SAFETY: Interrupts are disabled and `next` is a live TCB.
    if !unsafe { switch_address_space(next) } {
        // Revoked VSpace. Stop the thread rather than running it
        // somewhere it does not belong, and say so -- ADR 028 rejected
        // falling back to the kernel's address space silently.
        //
        // SAFETY: `next` is off the queue and not running.
        unsafe {
            next.set_state(ThreadState::Suspended);
            next.clear_vspace();
        }
        println!(
            "[sched] thread at {:#018x} has a revoked address space; \
             suspended rather than switched into (ADR 028)",
            // SAFETY: Every TCB is in the physical window.
            unsafe { tcb::tcb_phys(next) }.0
        );
        return;
    }

    let current_phys = CURRENT.load(Ordering::Acquire);
    // SAFETY: CURRENT names a live TCB.
    let previous = unsafe { tcb::tcb_at(crate::cap::ObjectRef(PhysAddr(current_phys))) };

    // SAFETY: Both TCBs live; we hold the scheduler's invariants.
    unsafe {
        if previous.state() == ThreadState::Running {
            previous.set_state(ThreadState::Ready);
        }
        next.set_state(ThreadState::Running);
    }

    next.record_switch();
    // SAFETY: Every TCB is in the physical window.
    let next_phys = unsafe { tcb::tcb_phys(next) }.0;
    CURRENT.store(next_phys, Ordering::Release);
    SWITCH_COUNT.fetch_add(1, Ordering::AcqRel);

    // Put the outgoing thread back on the queue *before* switching:
    // after `switch_context` this code is not running any more, and
    // the enqueue would happen at some arbitrary later time.
    if previous.state() == ThreadState::Ready {
        let phys = current_phys;
        debug_assert!(previous.next_ready() == 0);
        let tail = QUEUE_TAIL.load(Ordering::Acquire);
        if tail == 0 {
            QUEUE_HEAD.store(phys, Ordering::Release);
        } else {
            // SAFETY: Non-zero tail names a live TCB.
            let tail_tcb = unsafe { tcb::tcb_at(crate::cap::ObjectRef(PhysAddr(tail))) };
            tail_tcb.set_next_ready(phys);
        }
        QUEUE_TAIL.store(phys, Ordering::Release);
    }

    let from = &raw mut previous.context;
    let to = &raw const next.context;

    // SAFETY: Both contexts belong to live TCBs with mapped stacks, and
    // interrupts are disabled. Control returns here only when
    // something switches back to `previous`.
    unsafe {
        switch_context(from, to);
    }
}

/// Give up the CPU to the next ready thread.
///
/// Returns when something switches back. If no other thread is ready,
/// returns immediately -- yielding to nobody is a no-op rather than a
/// deadlock.
pub fn yield_now() {
    let guard = InterruptGuard::new();

    // SAFETY: Interrupts are disabled.
    let Some(next) = (unsafe { dequeue() }) else {
        return;
    };

    // SAFETY: Interrupts disabled, `next` is off the queue.
    unsafe {
        switch_to(next);
    }

    drop(guard);
}

/// One timer tick.
///
/// Called from the interrupt dispatcher. Counts the tick and, unless
/// preemption is disabled, switches to the next ready thread.
///
/// # Safety
///
/// Call only from the timer interrupt handler, with interrupts
/// disabled (the gate is an interrupt gate, so the CPU has already
/// cleared IF).
pub unsafe fn tick() {
    TICK_COUNT.fetch_add(1, Ordering::AcqRel);

    if preemption_disabled() {
        // Deferred, not lost. ADR 025: a preempt-disabled region still
        // takes interrupts; it just does not switch threads.
        DEFERRED_TICKS.fetch_add(1, Ordering::AcqRel);
        return;
    }

    if CURRENT.load(Ordering::Acquire) == 0 {
        return;
    }

    // SAFETY: We are in an interrupt handler entered through an
    // interrupt gate, so IF is clear.
    let Some(next) = (unsafe { dequeue() }) else {
        return;
    };

    // SAFETY: As above; `next` is off the queue.
    unsafe {
        switch_to(next);
    }
}

// ---- Suspend and resume (ADR 026) --------------------------------------

/// Take a thread off the ready queue.
///
/// Works in any state, including `Running` -- which is what makes
/// ADR 026's two-step kill sequence (`Suspend` then revoke) always
/// possible, and therefore makes refusing to revoke a busy thread
/// something other than a denial-of-service vector.
///
/// Suspending the *current* thread yields immediately; it will not run
/// again until resumed.
///
/// # Safety
///
/// `thread` must be a live TCB.
pub unsafe fn suspend(thread: &mut tcb::Tcb) {
    let guard = InterruptGuard::new();

    // SAFETY: Every TCB is in the physical window.
    let phys = unsafe { tcb::tcb_phys(thread) }.0;
    let is_current = phys == CURRENT.load(Ordering::Acquire);

    if thread.state() == ThreadState::Ready {
        // SAFETY: Interrupts are disabled.
        unsafe {
            remove_from_queue(phys);
        }
    }

    // SAFETY: We hold the scheduler's invariants under the guard.
    unsafe {
        thread.set_state(ThreadState::Suspended);
    }

    if is_current {
        // The current thread just suspended itself. Hand the CPU over;
        // this returns only after a `resume`.
        //
        // SAFETY: Interrupts disabled.
        if let Some(next) = unsafe { dequeue() } {
            // SAFETY: `next` is off the queue.
            unsafe {
                switch_to(next);
            }
        } else {
            panic!(
                "the last runnable thread suspended itself; there is nothing \
                 left to switch to and the machine would hang"
            );
        }
    }

    drop(guard);
}

/// Put a suspended thread back on the ready queue.
///
/// # Safety
///
/// `thread` must be a live TCB.
pub unsafe fn resume(thread: &mut tcb::Tcb) -> Result<(), crate::cap::CapError> {
    let _guard = InterruptGuard::new();

    match thread.state() {
        ThreadState::Suspended | ThreadState::Inactive => {
            // SAFETY: Interrupts disabled; the thread is not queued.
            unsafe {
                enqueue(thread);
            }
            Ok(())
        }
        state => Err(crate::cap::CapError::ThreadBusy {
            state: state.name(),
        }),
    }
}

// ---- Initialisation ----------------------------------------------------

/// Adopt the currently executing code as the first thread.
///
/// The kernel is already running on a stack with a context; this
/// records that fact so there is something to switch *from*. Without
/// it the first switch would have nowhere to save registers.
///
/// # Safety
///
/// `boot_tcb` must be a live TCB that nothing else owns, and the
/// current stack must be the one described by its `stack_top` and
/// `stack_bottom`.
pub unsafe fn init(boot_tcb: &mut tcb::Tcb) {
    assert!(
        CURRENT.load(Ordering::Acquire) == 0,
        "sched::init called twice"
    );
    assert!(
        !interrupts_enabled(),
        "sched::init with interrupts already enabled"
    );

    // SAFETY: Caller's guarantee; nothing else is scheduled yet.
    unsafe {
        boot_tcb.set_state(ThreadState::Running);
    }
    // SAFETY: Every TCB is in the physical window.
    let phys = unsafe { tcb::tcb_phys(boot_tcb) }.0;
    CURRENT.store(phys, Ordering::Release);

    println!(
        "[sched] boot thread adopted: tcb at {:#018x}, stack {:#018x}..{:#018x}",
        phys,
        boot_tcb.stack_bottom(),
        boot_tcb.stack_top()
    );
}

/// Report the scheduler's state, for the acceptance test.
pub fn report() {
    println!(
        "[sched] {} switches, {} ticks ({} deferred), {} ready",
        switch_count(),
        tick_count(),
        deferred_ticks(),
        ready_count()
    );
    println!(
        "[sched] cr3: {} reloads, {} skipped (same address space), {} \
         threads stopped for a revoked vspace",
        cr3_switch_count(),
        cr3_skipped_count(),
        stale_vspace_count()
    );
}
