// SPDX-License-Identifier: BSD-2-Clause
//! Thread control blocks.
//!
//! A TCB is 2048 bytes (ADR 017): a saved context, a kernel stack
//! pointer, a scheduling state, and 1 KiB reserved for the IPC buffer
//! that M9 will use. It is the first object in the system with
//! *mutable* state that outlives a syscall, which makes its lifecycle
//! the subject of its own ADR (026).
//!
//! What is deliberately absent in M6:
//!
//! * **FPU/SSE state.** Space is reserved but nothing is saved. Kernel
//!   threads do not use SSE -- the build passes `-mno-sse` -- so there
//!   is nothing to lose yet. M8's user threads make this mandatory and
//!   it is in the known gaps.
//! * **An address space.** Every thread here runs in the kernel's. M11
//!   adds a VSpace field.

use core::sync::atomic::{AtomicU64, Ordering};

use super::cnode::Slot;
use super::{CapError, CapType, ObjectRef};
use crate::memory::{PhysAddr, VirtAddr};

/// The registers a context switch must preserve.
///
/// Only the callee-saved set, plus RSP and RIP. The caller-saved
/// registers do not need saving because a context switch happens inside
/// a function call: the SysV ABI already says the caller must treat them
/// as clobbered.
///
/// That argument holds for a *voluntary* switch. A preempted thread is
/// interrupted at an arbitrary instruction, and all fifteen registers
/// matter -- but those are saved by the interrupt stub into an
/// `ExceptionFrame` on the thread's own kernel stack, and the switch
/// then happens with that frame already pushed. So the two paths meet:
/// by the time `switch_context` runs, the only live state is the
/// callee-saved set.
///
/// `#[repr(C)]` because the assembly in `sched` indexes these fields by
/// offset, and the compiler must not reorder them.
///
/// Reference: System V AMD64 ABI, section 3.2.1, Figure 3.4 ("Register
/// Usage"): RBX, RBP, R12-R15 are callee-saved.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Context {
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    /// Stack pointer. The switch writes this last and reads it first.
    pub rsp: u64,
    /// Where the thread resumes. For a thread that has never run, the
    /// entry point; otherwise the return address `switch_context`
    /// pushed.
    pub rip: u64,
}

/// Offsets the assembly uses. Kept beside the struct so that a field
/// added in the wrong place is a build failure rather than a crash.
pub mod context_offset {
    pub const RBX: usize = 0;
    pub const RBP: usize = 8;
    pub const R12: usize = 16;
    pub const R13: usize = 24;
    pub const R14: usize = 32;
    pub const R15: usize = 40;
    pub const RSP: usize = 48;
    pub const RIP: usize = 56;
}

const _: () = {
    use core::mem::offset_of;
    assert!(offset_of!(Context, rbx) == context_offset::RBX);
    assert!(offset_of!(Context, rbp) == context_offset::RBP);
    assert!(offset_of!(Context, r12) == context_offset::R12);
    assert!(offset_of!(Context, r13) == context_offset::R13);
    assert!(offset_of!(Context, r14) == context_offset::R14);
    assert!(offset_of!(Context, r15) == context_offset::R15);
    assert!(offset_of!(Context, rsp) == context_offset::RSP);
    assert!(offset_of!(Context, rip) == context_offset::RIP);
    assert!(
        core::mem::size_of::<Context>() == 64,
        "the context is not 64 bytes; the assembly's offsets are wrong"
    );
};

/// Where a thread is in its lifecycle (ADR 026).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ThreadState {
    /// Retyped but never started. No meaningful context.
    Inactive = 0,
    /// On the ready queue, waiting for the CPU.
    Ready = 1,
    /// Currently executing.
    Running = 2,
    /// Taken off the queue by `Suspend`. Context is meaningful and the
    /// thread resumes exactly where it stopped.
    Suspended = 3,
}

impl ThreadState {
    const fn from_bits(bits: u8) -> Option<ThreadState> {
        match bits {
            0 => Some(ThreadState::Inactive),
            1 => Some(ThreadState::Ready),
            2 => Some(ThreadState::Running),
            3 => Some(ThreadState::Suspended),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            ThreadState::Inactive => "Inactive",
            ThreadState::Ready => "Ready",
            ThreadState::Running => "Running",
            ThreadState::Suspended => "Suspended",
        }
    }

    /// Whether the scheduler is holding a reference to this thread.
    ///
    /// ADR 026: a thread in one of these states cannot have its TCB
    /// revoked, because the queue or the CPU is still pointing at it.
    pub const fn is_busy(self) -> bool {
        matches!(self, ThreadState::Ready | ThreadState::Running)
    }
}

/// Bytes of the TCB the IPC buffer will occupy from M9.
///
/// Reserved now rather than later: ADR 017 fixed the TCB at 2048 bytes
/// and the acceptance test asserts it, so the space has to exist for
/// the figure to mean anything.
pub const IPC_BUFFER_OFFSET: usize = 1024;

/// The thread control block itself.
///
/// `#[repr(C, align(2048))]`: the alignment is what ADR 017's retype
/// arithmetic expects, and `const` assertions below hold it to the
/// declared size.
#[repr(C, align(2048))]
pub struct Tcb {
    /// Saved registers. First, because the assembly takes a pointer to
    /// the TCB and uses it directly as a pointer to this field.
    pub context: Context,

    /// Scheduling state, as a `ThreadState`.
    state: u8,
    /// Priority. **Stored but not used in M6** -- ADR 024: this is
    /// data, not policy, until M10's scheduler interprets it.
    priority: u8,
    /// Padding to keep the following fields naturally aligned.
    _pad: [u8; 6],

    /// Top of this thread's kernel stack: where RSP starts.
    stack_top: u64,
    /// Bottom of the stack, for the overflow report. Knowing which
    /// thread's stack overflowed is the whole point of
    /// `thread-stack-overflow`.
    stack_bottom: u64,

    /// Index into the KSTACK area, so the stack can be unmapped when
    /// the thread dies.
    stack_slot: u64,

    /// How many times this thread has been scheduled. Diagnostic, and
    /// the acceptance test's evidence that round-robin is round.
    switch_count: u64,

    /// Where the next TCB in the ready queue lives, or zero.
    ///
    /// The queue is intrusive -- a `VecDeque` would allocate, and the
    /// scheduler runs inside a timer interrupt where ADR 022 forbids
    /// allocation. This is the same reasoning that put the derivation
    /// list inside the capability slots (ADR 019).
    next_ready: u64,

    /// Reserved. Brings the fixed part to IPC_BUFFER_OFFSET.
    _reserved: [u8; IPC_BUFFER_OFFSET - 112],

    /// The IPC buffer. Untouched in M6.
    _ipc_buffer: [u8; super::TCB_SIZE - IPC_BUFFER_OFFSET],
}

const _: () = {
    assert!(
        core::mem::size_of::<Tcb>() == super::TCB_SIZE,
        "the TCB is not TCB_SIZE bytes; ADR 017's table and retype's \
         arithmetic disagree"
    );
    assert!(
        core::mem::align_of::<Tcb>() == super::TCB_SIZE,
        "the TCB's alignment must equal its size (ADR 017)"
    );
    assert!(
        core::mem::offset_of!(Tcb, context) == 0,
        "the context must be first: the assembly treats a TCB pointer as \
         a context pointer"
    );
};

impl Tcb {
    pub fn state(&self) -> ThreadState {
        ThreadState::from_bits(self.state).unwrap_or_else(|| {
            panic!(
                "TCB holds thread state {}, which is not a valid state; the \
                 object has been corrupted",
                self.state
            )
        })
    }

    /// Change the state.
    ///
    /// # Safety
    ///
    /// The caller must hold the scheduler's invariants: a thread in
    /// `Ready` must be on the queue exactly once, and at most one
    /// thread may be `Running`.
    pub unsafe fn set_state(&mut self, state: ThreadState) {
        self.state = state as u8;
    }

    pub fn priority(&self) -> u8 {
        self.priority
    }

    /// Set the priority.
    ///
    /// Stored and returned by `ReadRegisters`; **not consulted by M6's
    /// scheduler** (ADR 024).
    pub fn set_priority(&mut self, priority: u8) {
        self.priority = priority;
    }

    pub fn stack_top(&self) -> u64 {
        self.stack_top
    }

    pub fn stack_bottom(&self) -> u64 {
        self.stack_bottom
    }

    pub fn stack_slot(&self) -> u64 {
        self.stack_slot
    }

    /// Whether an address is inside this thread's kernel stack.
    ///
    /// The acceptance test uses this to prove two threads never share a
    /// stack, and the overflow report uses it to name the thread.
    pub fn owns_stack_address(&self, addr: u64) -> bool {
        addr > self.stack_bottom && addr <= self.stack_top
    }

    pub fn switch_count(&self) -> u64 {
        self.switch_count
    }

    pub(crate) fn record_switch(&mut self) {
        self.switch_count = self.switch_count.wrapping_add(1);
    }

    pub(crate) fn next_ready(&self) -> u64 {
        self.next_ready
    }

    pub(crate) fn set_next_ready(&mut self, next: u64) {
        self.next_ready = next;
    }

    /// Give this thread a stack and an entry point, ready to run.
    ///
    /// The context is built so that the first `switch_context` into
    /// this thread lands on `entry` with a correctly aligned stack and
    /// **interrupts enabled**.
    ///
    /// The second part is not free. Every switch happens inside an
    /// interrupt-free region, and the `sti` that ends that region is
    /// on the instruction after `switch_context` returns -- a place a
    /// brand-new thread never passes through, because its saved RIP is
    /// its entry point and the switch's `ret` goes straight there. Such
    /// a thread would run with IF clear for its whole life, and if it
    /// never yielded, nothing could take the CPU back: the timer that
    /// was meant to preempt it is masked in its own RFLAGS.
    ///
    /// So the entry point is not stored as RIP. It goes in R12 -- a
    /// callee-saved register, so the switch loads it as part of the
    /// ordinary context -- and RIP points at a trampoline that executes
    /// `sti` and jumps to it.
    ///
    /// # Safety
    ///
    /// `stack_top` must be one past the last byte of a mapped, writable
    /// stack that no other thread owns, with a guard page below
    /// `stack_bottom`. `entry` must not return: there is no return
    /// address beneath it.
    pub unsafe fn configure(
        &mut self,
        entry: extern "C" fn() -> !,
        stack_top: VirtAddr,
        stack_bottom: VirtAddr,
        stack_slot: u64,
    ) {
        assert!(
            stack_top.0 > stack_bottom.0,
            "stack top {} is not above bottom {}",
            stack_top,
            stack_bottom
        );
        assert!(
            stack_top.0.is_multiple_of(16),
            "stack top {} is not 16-byte aligned; every SSE spill in the \
             thread would be misaligned",
            stack_top
        );

        // The ABI wants RSP ≡ 0 (mod 16) *after* the return address is
        // pushed. `switch_context` jumps rather than calls, so imitate
        // the layout a `call` would have left: align, then subtract 8.
        //
        // The same arithmetic as `vm::switch_to_main_stack`, and wrong
        // in the same way if it is omitted.
        //
        // Reference: System V AMD64 ABI, section 3.2.2.
        let entry_rsp = (stack_top.0 & !0xF) - 8;

        self.context = Context {
            rsp: entry_rsp,
            // Not `entry`: the trampoline, which opens the interrupt
            // window this thread would otherwise never get.
            rip: crate::sched::entry_trampoline(),
            // The trampoline reads the real entry point from here.
            r12: entry as usize as u64,
            // Zeroed rather than left as they are: a thread starting
            // with another thread's RBP would make a backtrace walk
            // into a stack it does not own.
            ..Context::default()
        };

        self.stack_top = stack_top.0;
        self.stack_bottom = stack_bottom.0;
        self.stack_slot = stack_slot;
        self.switch_count = 0;
        self.next_ready = 0;
        self.state = ThreadState::Inactive as u8;
    }

    /// Record the stack of a thread that is **already running**.
    ///
    /// For the boot thread, which has a context the CPU is holding
    /// right now. Deliberately does not touch `context`: writing an
    /// entry point there would be wrong twice over -- the thread has
    /// no entry point to return to, and the first switch *away* from
    /// it is what fills the field in.
    ///
    /// # Safety
    ///
    /// The current stack must be the one described, and the caller
    /// must be the thread being adopted.
    pub unsafe fn adopt_running(
        &mut self,
        stack_top: VirtAddr,
        stack_bottom: VirtAddr,
        stack_slot: u64,
    ) {
        self.stack_top = stack_top.0;
        self.stack_bottom = stack_bottom.0;
        self.stack_slot = stack_slot;
        self.switch_count = 0;
        self.next_ready = 0;
        // Left as Inactive; `sched::init` sets Running.
        self.state = ThreadState::Inactive as u8;
    }
}

/// Borrow the TCB a capability names.
///
/// # Safety
///
/// `slot` must hold a live TCB capability, and the caller must not
/// create a second mutable borrow of the same object.
pub unsafe fn from_slot(slot: &Slot) -> Result<&'static mut Tcb, CapError> {
    if slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if slot.cap_type() != CapType::Tcb {
        return Err(CapError::WrongType {
            found: slot.cap_type(),
        });
    }
    // SAFETY: The capability names a TCB-sized, TCB-aligned object that
    // retype zeroed, reachable through the physical window. The caller
    // guarantees no aliasing borrow.
    Ok(unsafe { tcb_at(slot.object()) })
}

/// Borrow the TCB at an object address.
///
/// # Safety
///
/// `object` must name a live TCB, and the caller must not alias it.
pub unsafe fn tcb_at(object: ObjectRef) -> &'static mut Tcb {
    debug_assert!(!object.is_null(), "null TCB address");
    debug_assert!(
        object.0 .0.is_multiple_of(super::TCB_SIZE as u64),
        "TCB at {} is not {}-byte aligned",
        object,
        super::TCB_SIZE
    );
    // SAFETY: Caller's guarantee. PMAP maps all RAM writably and the
    // alignment matches `Tcb`'s.
    unsafe { &mut *crate::memory::phys_to_virt(object.0).as_ptr::<Tcb>() }
}

/// Physical address of a TCB, for the scheduler's intrusive queue.
///
/// # Safety
///
/// `tcb` must live inside the physical window, which every TCB does.
pub unsafe fn tcb_phys(tcb: &Tcb) -> PhysAddr {
    let virt = core::ptr::from_ref(tcb) as u64;
    PhysAddr(virt - crate::memory::pmap_base())
}

// ---- Invocations -------------------------------------------------------
//
// These are what `invoke` dispatches to (ADR 017's single syscall). In
// M6 the caller is kernel test code rather than a user component, but
// the signatures are the ones M8 will expose: everything goes through a
// capability, and every failure is an error rather than a panic.

/// The subset of a thread's state `ReadRegisters` returns.
///
/// Not the whole context: a debugger wants RIP and RSP, and the rest is
/// an implementation detail of the switch. Returning only what is
/// meaningful keeps M8 from having to support a wider contract than it
/// means to.
#[derive(Clone, Copy, Debug)]
pub struct ThreadRegisters {
    pub rip: u64,
    pub rsp: u64,
    pub state: ThreadState,
    pub priority: u8,
    pub switch_count: u64,
}

/// Count of every invocation, so the acceptance test can show the
/// capability path was actually used rather than the kernel reaching
/// around it.
static INVOCATION_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn invocation_count() -> u64 {
    INVOCATION_COUNT.load(Ordering::Acquire)
}

fn count_invocation() {
    INVOCATION_COUNT.fetch_add(1, Ordering::AcqRel);
}

/// `SetRegisters`: give an inactive thread its entry point and stack.
///
/// Refused for a thread that is running or ready: changing RIP under a
/// thread the scheduler is holding would make it resume somewhere it
/// never was.
///
/// # Safety
///
/// As [`from_slot`], plus the stack requirements of [`Tcb::configure`].
pub unsafe fn set_registers(
    slot: &Slot,
    entry: extern "C" fn() -> !,
    stack_top: VirtAddr,
    stack_bottom: VirtAddr,
    stack_slot: u64,
) -> Result<(), CapError> {
    count_invocation();
    // SAFETY: Caller's guarantee.
    let tcb = unsafe { from_slot(slot) }?;

    if tcb.state().is_busy() {
        return Err(CapError::ThreadBusy {
            state: tcb.state().name(),
        });
    }

    // SAFETY: Caller's guarantee about the stack.
    unsafe {
        tcb.configure(entry, stack_top, stack_bottom, stack_slot);
    }
    Ok(())
}

/// `ReadRegisters`: what is this thread doing?
///
/// Allowed in any state. For a `Running` thread the answer is stale the
/// moment it is returned -- the thread is still executing -- which is
/// why the acceptance test reads a *suspended* thread.
///
/// # Safety
///
/// As [`from_slot`].
pub unsafe fn read_registers(slot: &Slot) -> Result<ThreadRegisters, CapError> {
    count_invocation();
    // SAFETY: Caller's guarantee.
    let tcb = unsafe { from_slot(slot) }?;

    Ok(ThreadRegisters {
        rip: tcb.context.rip,
        rsp: tcb.context.rsp,
        state: tcb.state(),
        priority: tcb.priority(),
        switch_count: tcb.switch_count(),
    })
}

/// `SetPriority`: store a priority.
///
/// M6's scheduler does not read it (ADR 024). The invocation exists so
/// that M10 inherits a working path rather than having to add one.
///
/// # Safety
///
/// As [`from_slot`].
pub unsafe fn set_priority(slot: &Slot, priority: u8) -> Result<(), CapError> {
    count_invocation();
    // SAFETY: Caller's guarantee.
    let tcb = unsafe { from_slot(slot) }?;
    tcb.set_priority(priority);
    Ok(())
}
