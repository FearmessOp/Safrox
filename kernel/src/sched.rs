// SPDX-License-Identifier: MPL-2.0
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

/// FPU states saved with `fxsave` (M8, ADR 031).
///
/// Counted so the acceptance test can show the save happens on *every*
/// switch. "The XMM register was preserved" would pass on a lazy
/// implementation too -- a correct lazy switcher also preserves it.
/// The count matching the switch count is what rules lazy out.
static FXSAVE_COUNT: AtomicU64 = AtomicU64::new(0);

/// FPU states restored with `fxrstor`.
static FXRSTOR_COUNT: AtomicU64 = AtomicU64::new(0);

/// Switches that updated TSS.RSP0 and the syscall stack.
static RSP0_UPDATES: AtomicU64 = AtomicU64::new(0);

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

pub fn fxsave_count() -> u64 {
    FXSAVE_COUNT.load(Ordering::Acquire)
}

pub fn fxrstor_count() -> u64 {
    FXRSTOR_COUNT.load(Ordering::Acquire)
}

pub fn rsp0_update_count() -> u64 {
    RSP0_UPDATES.load(Ordering::Acquire)
}

/// Save the CPU's FPU/SSE state into a thread's TCB.
///
/// Unconditional (ADR 031). The kernel never sets CR0.TS, never takes
/// #NM, and never asks whether the outgoing thread used the FPU --
/// that question is what lazy switching exists to answer, and lazy
/// switching is the mechanism behind CVE-2018-3665.
///
/// # Safety
///
/// Interrupts must be disabled, and `tcb` must be a live TCB whose
/// `fxsave` area nothing else is using.
unsafe fn save_fpu(tcb: &mut tcb::Tcb) {
    // SAFETY: `fpu_state_ptr` returns a 512-byte, 16-byte-aligned area
    // inside the TCB -- the alignment is checked at build time and
    // again by a debug assertion -- and the caller guarantees nothing
    // else is touching it. CR4.OSFXSR is set by `cpu::init`, so the
    // instruction is not #UD.
    //
    // Reference: Intel SDM Vol. 2A, "FXSAVE".
    unsafe {
        let area = tcb.fpu_state_ptr();
        core::arch::asm!("fxsave [{}]", in(reg) area, options(nostack, preserves_flags));
    }
    FXSAVE_COUNT.fetch_add(1, Ordering::AcqRel);
}

/// Load a thread's FPU/SSE state into the CPU.
///
/// # Safety
///
/// As [`save_fpu`], and the area must hold a state `fxrstor` accepts:
/// either one `fxsave` wrote, or one `initialise_fpu_state` built.
/// Arbitrary bytes can raise #GP through MXCSR's reserved bits.
unsafe fn restore_fpu(tcb: &mut tcb::Tcb) {
    // SAFETY: As `save_fpu`, plus the caller's guarantee that the area
    // holds a legal state -- which holds because every TCB's area is
    // written by `initialise_fpu_state` at configure time and by
    // `fxsave` thereafter.
    //
    // Reference: Intel SDM Vol. 2A, "FXRSTOR".
    unsafe {
        let area = tcb.fpu_state_ptr();
        core::arch::asm!("fxrstor [{}]", in(reg) area, options(nostack, preserves_flags));
    }
    FXRSTOR_COUNT.fetch_add(1, Ordering::AcqRel);
}

/// Point the kernel-entry paths at a thread's own kernel stack.
///
/// Two places hold the same address and the CPU reaches them
/// differently:
///
/// * `TSS.RSP0` -- read by the CPU when an *interrupt* raises the
///   privilege level from ring 3.
/// * `syscall::KERNEL_RSP` -- read by the syscall stub, because
///   `syscall` does not consult the TSS at all (ADR 030).
///
/// Both are written here so they cannot drift apart. If two user
/// threads shared one of them, the second to enter the kernel would
/// overwrite the first's frame -- random corruption, far from its
/// cause (ADR 029).
///
/// # Safety
///
/// Interrupts must be disabled, and `tcb` must be the thread about to
/// run, with a mapped kernel stack.
unsafe fn set_kernel_entry_stack(tcb: &tcb::Tcb) {
    let top = tcb.stack_top();

    // SAFETY: `top` is this thread's own kernel stack, mapped by the
    // code that created the thread, and interrupts are disabled so no
    // entry can be in flight reading a half-written value.
    unsafe {
        crate::gdt::set_rsp0(top);
        crate::syscall::set_kernel_stack(top);
    }

    debug_assert_eq!(
        crate::gdt::rsp0(),
        crate::syscall::kernel_stack(),
        "TSS.RSP0 and the syscall stack disagree; an interrupt and a \
         syscall from the same thread would land on different stacks"
    );

    RSP0_UPDATES.fetch_add(1, Ordering::AcqRel);
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

// ---- Entering ring 3 for the first time (M8, ADR 029) ------------------

unsafe extern "C" {
    /// Where a thread configured by `configure_user` begins.
    ///
    /// Named `_asm` so that [`user_entry_trampoline`] can be the
    /// function returning its address — matching the pair M6
    /// established for the kernel trampoline.
    #[link_name = "user_entry_trampoline"]
    fn user_entry_trampoline_asm() -> !;
}

// A user thread's first instruction at CPL 3.
//
// Reached the same way the kernel trampoline is -- `switch_context`
// restores the callee-saved set and returns into it -- so the three
// values it needs arrive in R12, R13 and R14 (ADR 029).
//
// `iretq` rather than `sysret` for this first entry. Both are the only
// ways down to ring 3; `iretq` takes all five values from the stack,
// where a mistake can be read out of a fault's frame. `sysret` derives
// CS and SS from STAR and takes RFLAGS from R11, so a mistake is a #GP
// with nothing to inspect. Syscall *returns* do use `sysret`: that
// path runs thousands of times rather than once.
//
// The frame, pushed highest first:
//
//     SS      user data selector, RPL 3
//     RSP     the user stack
//     RFLAGS  IF set -- and set *here*, not by `sti`
//     CS      user code selector, RPL 3
//     RIP     the user entry point
//
// RFLAGS carries IF for the same reason M6's kernel trampoline runs
// `sti`: a thread that never opens the interrupt window can never be
// preempted, and the timer meant to take the CPU back is masked in its
// own flags. But a plain `sti` here would be wrong -- it would enable
// interrupts *before* `iretq`, inside the scheduler's critical region,
// and the next instruction is the privilege transition. Putting IF in
// the pushed RFLAGS makes it take effect exactly at the transition,
// atomically.
//
// Bit 1 of RFLAGS is reserved and must be 1; the CPU rejects a frame
// without it.
//
// The user's own registers: RDI carries the argument from R14, and
// every other general register is zeroed. Not tidiness -- whatever the
// kernel last left in a register would otherwise be readable from ring
// 3, and the kernel's registers routinely hold kernel addresses.
core::arch::global_asm!(
    ".section .text",
    ".code64",
    ".align 16",
    ".global user_entry_trampoline",
    "user_entry_trampoline:",
    // Build the iretq frame from R12 (entry), R13 (stack).
    "    mov rax, {user_ss}",
    "    push rax",
    "    push r13",
    "    mov rax, {rflags}",
    "    push rax",
    "    mov rax, {user_cs}",
    "    push rax",
    "    push r12",
    // The argument, then scrub everything else. RAX is last because
    // the pushes above used it.
    "    mov rdi, r14",
    "    xor rsi, rsi",
    "    xor rdx, rdx",
    "    xor rcx, rcx",
    "    xor r8, r8",
    "    xor r9, r9",
    "    xor r10, r10",
    "    xor r11, r11",
    "    xor r12, r12",
    "    xor r13, r13",
    "    xor r14, r14",
    "    xor r15, r15",
    "    xor rbx, rbx",
    "    xor rbp, rbp",
    "    xor rax, rax",
    "    iretq",
    user_ss = const crate::gdt::USER_DATA_SELECTOR as u64,
    user_cs = const crate::gdt::USER_CODE_SELECTOR as u64,
    // IF (bit 9) plus the reserved bit 1, which must be set.
    rflags = const (1u64 << 9) | (1u64 << 1),
);

/// Address of the trampoline a user thread starts at.
///
/// [`tcb::Tcb::configure_user`] stores this as the thread's RIP,
/// overwriting the kernel trampoline `configure` left there.
///
/// Omitting that overwrite was this milestone's first bug. The kernel
/// trampoline ran `sti; jmp r12` with R12 already holding the user
/// entry point, so the kernel jumped into a USER page at CPL 0 — and
/// SMEP refused the fetch. The #PF named the right address with the
/// wrong CS, which is as clear a diagnosis as that mistake can
/// produce.
pub fn user_entry_trampoline() -> u64 {
    // Through a pointer rather than a direct numeric cast, as
    // `entry_trampoline` does and for the same reason.
    (user_entry_trampoline_asm as *const ()) as u64
}

/// The trampoline a thread configured by `configure_user` starts at.
///
/// Typed as a diverging `extern "C" fn` so it can be handed to
/// `Tcb::configure`, which takes the same shape for kernel threads.
pub fn user_entry_trampoline_fn() -> extern "C" fn() -> ! {
    // `user_entry_trampoline` is declared `-> !` in the extern block
    // and never returns: it ends in `iretq`.
    //
    // SAFETY: Reinterpreting one `extern "C"` function pointer as
    // another with the same (empty) argument list and the same
    // divergence. The assembly takes no arguments in the ABI sense --
    // it reads R12, R13 and R14, which the context switch loaded --
    // and does not return.
    unsafe {
        core::mem::transmute::<unsafe extern "C" fn() -> !, extern "C" fn() -> !>(
            user_entry_trampoline_asm,
        )
    }
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

    // The FPU, and the stacks the kernel will next be entered on (M8).
    //
    // Both before `switch_context`, for the same reason the CR3 reload
    // is: after it, this function is not executing any more.
    //
    // Save then restore, with nothing between them that could touch an
    // XMM register. The kernel builds with `-mno-sse` precisely so
    // that "nothing between them" is a property of the compiler rather
    // than a hope -- and that is also why the kernel itself never
    // needs its FPU state saved on the interrupt path.
    //
    // SAFETY: Interrupts are disabled, both TCBs are live, and each
    // fxsave area belongs to exactly one thread.
    unsafe {
        save_fpu(previous);
        restore_fpu(next);
        set_kernel_entry_stack(next);
    }

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

    // Is the root task still there? (ADR 044)
    //
    // **Before the preempt check**, deliberately: the question is
    // not "may we switch threads" but "is there still a system".
    // A preempt-disabled region is exactly where a kernel would sit
    // spinning if the root task had died, so skipping the check
    // there would skip it when it matters most.
    //
    // Cheap: two loads and a comparison, against an address
    // recorded once at boot. Before the root task starts, the
    // recorded address is zero and this returns immediately.
    crate::roottask::check_alive();

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

    // A thread may not suspend *itself* out of an IPC wait, and this
    // is the one misuse worth paying a runtime check for.
    //
    // M9a's first `recv` did exactly that: queue on the endpoint, set
    // `BlockedOnEndpoint`, call `suspend`. Suspend then saw the state
    // it had just been given, concluded the thread no longer wanted
    // to wait, and called `cancel_ipc` -- unqueueing the receiver it
    // had been asked to park. The endpoint went back to empty, the
    // sender blocked too, and the scenario timed out after thirty
    // seconds.
    //
    // **Nothing was corrupted, which is what made it hard to find.**
    // There was no bad pointer and no broken invariant; the two
    // halves simply cancelled out. A thirty-second timeout naming
    // nothing is the worst diagnosis in this codebase, so the misuse
    // gets a name here instead.
    //
    // Use [`block_current`] to park the running thread on a queue it
    // has already joined.
    assert!(
        !(is_current && thread.state().is_blocked_on_ipc()),
        "a thread cannot suspend itself out of an IPC wait (state {}): \
         suspend means \"this thread is no longer waiting\" and cancels \
         the wait, which is the opposite of what a blocking Send or \
         Recv needs. Use sched::block_current()",
        thread.state().name()
    );

    if thread.state() == ThreadState::Ready {
        // SAFETY: Interrupts are disabled.
        unsafe {
            remove_from_queue(phys);
        }
    }

    // M9: a thread waiting on an endpoint is not on the ready queue
    // but it *is* in a queue, and leaving it there would be the worst
    // kind of dangling reference -- the endpoint would hand a message
    // to a thread that is no longer willing to receive one, and the
    // delivery would be parked for a thread that never collects it.
    //
    // This is the link that makes ADR 039 real rather than written
    // down: without it, `Suspend` on a blocked thread would appear to
    // work and corrupt the queue. The `ipc-endpoint-revoked` scenario
    // is what proves the call is here.
    if thread.state().is_blocked_on_ipc() {
        // SAFETY: Interrupts are disabled and the thread is live.
        unsafe {
            crate::cap::ipc::cancel_ipc(thread);
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

/// Park the running thread and hand the CPU to the next ready one.
///
/// # How this differs from [`suspend`], and why they are separate
///
/// Both stop a thread, and that similarity is exactly the trap. Their
/// contracts are opposites:
///
/// | | [`suspend`] | `block_current` |
/// |---|---|---|
/// | Who asks | somebody else, through a TCB capability | the thread itself |
/// | Meaning | "stop waiting for whatever you are waiting for" | "I am waiting; wake me when it arrives" |
/// | IPC queues | **leaves them** (`cancel_ipc`) | **stays in them** |
/// | Thread state | sets `Suspended` | leaves it as the caller set it |
///
/// Calling the wrong one does not corrupt anything. It simply undoes
/// the work the caller just did, which is why M9a's first rendezvous
/// hung for thirty seconds with nothing in the log. The pair is split
/// here for the same reason M3 split `TlbFlush` out of `map` and M7
/// split `UserAccess` out of a plain read: two operations that look
/// alike and mean different things should not share a name.
///
/// # Contract
///
/// The caller must **already** have put the thread on the queue it is
/// waiting on and set its state to one of the blocked-on-IPC states.
/// This function changes neither; it only gives up the CPU.
///
/// # Safety
///
/// Interrupts must be disabled, the current thread must be queued
/// wherever it expects to be woken from, and its state must say so.
pub unsafe fn block_current() {
    debug_assert!(
        !interrupts_enabled(),
        "block_current with interrupts enabled: the caller is partway \
         through a rendezvous and a timer tick could run a third thread \
         into the same endpoint (ADR 025)"
    );

    let phys = CURRENT.load(Ordering::Acquire);
    assert!(phys != 0, "block_current() before sched::init()");

    // SAFETY: CURRENT names a live TCB.
    let current = unsafe { tcb::tcb_at(crate::cap::ObjectRef(PhysAddr(phys))) };

    // The caller's half of the contract. If this fires, the thread is
    // about to sleep with nothing holding a reference to it -- nobody
    // could ever wake it, and the failure would surface as a hang
    // rather than as this line.
    debug_assert!(
        current.state().is_blocked_on_ipc(),
        "block_current on a thread whose state is {}; the caller must \
         queue the thread and set BlockedOnEndpoint or BlockedOnReply \
         *before* giving up the CPU, or nothing will ever wake it",
        current.state().name()
    );

    // SAFETY: Interrupts are disabled by the caller's guard.
    let Some(next) = (unsafe { dequeue() }) else {
        // Every thread in the system is waiting for a message, and
        // messages come from threads. Named rather than left as a
        // hang: a deadlock is a diagnosis, a timeout is not.
        panic!(
            "every thread is blocked on IPC and there is nothing left to \
             run. This is a deadlock -- some component is waiting for a \
             message that only a blocked thread could send"
        );
    };

    // SAFETY: Interrupts disabled, `next` is off the ready queue, and
    // the current thread is parked on an IPC queue rather than the
    // ready one -- so `switch_to` will not put it back.
    unsafe {
        switch_to(next);
    }
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

    // Arm the kernel-entry stacks for the thread that is running right
    // now (M8). Without this they stay zero until the *first* switch,
    // and a ring-3 thread entering the kernel before then would have
    // the CPU load RSP0 = 0 -- a stack at address zero, which faults
    // while already delivering a fault.
    //
    // SAFETY: Interrupts are disabled (asserted above) and the boot
    // thread's stack is the main stack, mapped by `vm::init`.
    unsafe {
        set_kernel_entry_stack(boot_tcb);
    }

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
    // M8. The equality between `fxsave` and the switch count is what
    // makes "no lazy switching" testable at all: a correct lazy
    // implementation also preserves every register, so preservation
    // alone proves nothing about which strategy is in use (ADR 031).
    println!(
        "[sched] fpu: {} fxsave, {} fxrstor over {} switches; {} rsp0 \
         updates",
        fxsave_count(),
        fxrstor_count(),
        switch_count(),
        rsp0_update_count()
    );
}
