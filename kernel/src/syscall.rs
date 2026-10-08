// SPDX-License-Identifier: BSD-2-Clause
//! The one system call.
//!
//! ADR 017 fixed the syscall count at one: everything is a capability
//! invocation. ADR 030 settles what travels in which register and how
//! the kernel stack is reached. This module is the implementation —
//! an assembly stub, a dispatcher, and the argument validation between
//! them.
//!
//! # What makes this different from the exception path
//!
//! M1's stubs handle an *exception*: the CPU has already pushed a
//! frame, switched stacks if the privilege level changed, and cleared
//! IF through the interrupt gate. `syscall` does none of that. It
//! writes RCX and R11, loads CS and SS from an MSR, jumps, and leaves
//! everything else exactly as user code arranged it — including RSP,
//! which still points into user memory.
//!
//! So the first two instructions here are load-bearing in a way no
//! exception stub's are: until the stack is switched, every push would
//! land in memory a user component controls.
//!
//! # The threat model
//!
//! Every value arriving here was chosen by user code, and from this
//! milestone that code is assumed hostile. Nothing is trusted: not the
//! CPtr, not the method number, not the arguments, and not the state
//! of RFLAGS (which is why `IA32_FMASK` clears four bits — ADR 030).
//!
//! The rule is ADR 023's K5 at its sharpest: **every `Err` path must
//! be reachable from `user-cap-forge`**. An untested error path is the
//! first thing to break under hostile input.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::cap::cptr::CPtr;
use crate::cap::{tcb, CapError, CapType};
use crate::println;

// ---- Where the kernel stack lives --------------------------------------

/// The stack `syscall` switches to, updated on every thread switch.
///
/// `syscall` does not consult the TSS — that is only for interrupts
/// that change privilege level — so the stub has to find the stack
/// itself. This holds the same value as `TSS.RSP0`, written at the
/// same moment by `sched::switch_to`, and a `debug_assert` ties them
/// together.
///
/// # Why not `swapgs`
///
/// The usual answer is a per-CPU structure reached through GS, which
/// costs no general register. ADR 030 rejected it: `swapgs` is a
/// *swap*, so every path into the kernel must execute exactly one, and
/// an exception arriving at the wrong moment leaves GS pointing the
/// wrong way. Getting that right means a "where did I come from" test
/// in all 34 exception stubs.
///
/// RIP-relative addressing costs no register either, and on one CPU
/// a single static is correct. It stops being correct with a second
/// CPU; that is in the known gaps, and the fix is `swapgs` done
/// properly rather than this made cleverer.
#[unsafe(no_mangle)]
static mut KERNEL_RSP: u64 = 0;

/// Where the stub parks the user's RSP while the kernel runs.
///
/// Also single-CPU-only, and for the same reason.
#[unsafe(no_mangle)]
static mut SAVED_USER_RSP: u64 = 0;

/// Point the syscall path at a thread's kernel stack.
///
/// # Safety
///
/// `top` must be one past the last byte of a mapped, writable,
/// 16-byte-aligned kernel stack belonging to the thread about to run.
/// Must be called with interrupts disabled, from the scheduler.
pub unsafe fn set_kernel_stack(top: u64) {
    debug_assert!(
        top.is_multiple_of(16),
        "the syscall stack must be 16-byte aligned"
    );
    // SAFETY: Single CPU, interrupts disabled by the caller, so no
    // syscall can be in flight reading this. A single aligned qword.
    unsafe {
        let slot = &raw mut KERNEL_RSP;
        slot.write(top);
    }
}

/// What the syscall path would use as its stack right now.
pub fn kernel_stack() -> u64 {
    // SAFETY: A single aligned qword read; see `set_kernel_stack`.
    unsafe {
        let slot = &raw const KERNEL_RSP;
        slot.read()
    }
}

// ---- The frame the stub builds -----------------------------------------

/// Machine state at the point `syscall` was executed.
///
/// Deliberately *similar* to [`crate::interrupts::ExceptionFrame`] and
/// deliberately not the same type. In an exception the last five
/// fields are pushed by the CPU; here the stub fills them from
/// different places — RIP comes out of RCX, RFLAGS out of R11, RSP out
/// of a static. Sharing a type would make "where did this field come
/// from" unanswerable at the point of use.
///
/// `#[repr(C)]` because the assembly below builds this layout by hand.
#[repr(C)]
#[derive(Debug)]
pub struct SyscallFrame {
    // --- pushed by the stub, in `push` order r15 .. rax ---
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    /// Saved RFLAGS. `syscall` put it here; `sysret` reads it back.
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    /// The return address. `syscall` put it here; `sysret` reads it
    /// back, which is why a non-canonical value is dangerous (ADR 029).
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    /// The user's RSP, rescued from the static before the stack switch.
    pub user_rsp: u64,
}

/// Offsets the assembly uses, kept beside the struct.
mod frame_offset {
    pub const RAX: usize = 14 * 8;
    pub const USER_RSP: usize = 15 * 8;
}

const _: () = {
    use core::mem::offset_of;
    assert!(offset_of!(SyscallFrame, rax) == frame_offset::RAX);
    assert!(offset_of!(SyscallFrame, user_rsp) == frame_offset::USER_RSP);
    assert!(
        core::mem::size_of::<SyscallFrame>() == 16 * 8,
        "the syscall frame is not 16 qwords; the assembly's pushes and \
         the struct disagree, and every field would be read from the \
         wrong place"
    );
};

// ---- Counters, for the acceptance test ---------------------------------

/// Syscalls that completed.
static SYSCALL_COUNT: AtomicU64 = AtomicU64::new(0);
/// Syscalls that returned an error rather than a result.
static SYSCALL_ERRORS: AtomicU64 = AtomicU64::new(0);
/// Returns that took the `sysret` path.
static SYSRET_RETURNS: AtomicU64 = AtomicU64::new(0);
/// Returns forced onto `iretq` by a non-canonical RCX (ADR 029).
static IRETQ_RETURNS: AtomicU64 = AtomicU64::new(0);

pub fn syscall_count() -> u64 {
    SYSCALL_COUNT.load(Ordering::Acquire)
}

pub fn error_count() -> u64 {
    SYSCALL_ERRORS.load(Ordering::Acquire)
}

pub fn sysret_count() -> u64 {
    SYSRET_RETURNS.load(Ordering::Acquire)
}

pub fn iretq_count() -> u64 {
    IRETQ_RETURNS.load(Ordering::Acquire)
}

/// Counted by the stub, from assembly, when it takes the `iretq` path.
#[unsafe(no_mangle)]
unsafe extern "C" fn syscall_count_iretq() {
    IRETQ_RETURNS.fetch_add(1, Ordering::AcqRel);
}

/// Counted by the stub when it takes the ordinary `sysret` path.
#[unsafe(no_mangle)]
unsafe extern "C" fn syscall_count_sysret() {
    SYSRET_RETURNS.fetch_add(1, Ordering::AcqRel);
}

// ---- The result encoding -----------------------------------------------

/// How an invocation's outcome is packed into RAX.
///
/// One register, two outcomes. The top bit distinguishes them, which
/// leaves 63 bits for a result — ample, since M8's invocations return
/// either nothing or a small integer.
///
/// A second register would be roomier and was rejected (ADR 030): M9
/// needs a genuinely multi-valued return for IPC, and a two-value
/// stopgap now would constrain that design.
const ERROR_FLAG: u64 = 1 << 63;

/// Pack a `CapError` into the error half of the return value.
///
/// The discriminant is written out by hand rather than taken from the
/// enum's layout. `CapError` carries data in several variants, so its
/// representation is not a small integer, and tying the user-visible
/// ABI to a compiler-chosen layout would make reordering the variants
/// a silent ABI break.
fn encode_error(error: CapError) -> u64 {
    let code: u64 = match error {
        CapError::InvalidCPtr => 1,
        CapError::CPtrOutOfRange => 2,
        CapError::EmptySlot => 3,
        CapError::StaleCPtr => 4,
        CapError::RootSlotMustBeCNode => 5,
        CapError::WrongType { .. } => 6,
        CapError::InsufficientRights { .. } => 7,
        CapError::RightsEscalation { .. } => 8,
        CapError::SlotNotEmpty => 9,
        CapError::DerivationTooDeep { .. } => 10,
        CapError::NotEnoughMemory { .. } => 11,
        CapError::TooManyObjects { .. } => 12,
        CapError::DeviceUntypedFrameOnly { .. } => 13,
        CapError::ThreadBusy { .. } => 14,
        CapError::NotUserAddress { .. } => 15,
        CapError::FlagsWrongHalf { .. } => 16,
        CapError::MissingPageTable { .. } => 17,
        CapError::AlreadyMapped { .. } => 18,
        CapError::NotMapped { .. } => 19,
        CapError::VSpaceInUse { .. } => 20,
        CapError::InvalidArgument => 21,
        // M9. Appended rather than inserted: these numbers are a
        // user-visible ABI, and renumbering an existing one would
        // silently change what a component thinks went wrong.
        CapError::MessageTooLong { .. } => 22,
        CapError::TooManyCaps { .. } => 23,
        CapError::EndpointDead => 24,
        CapError::NoReplyTarget => 25,
        CapError::ReplyTargetGone => 26,
        CapError::NotificationBusy => 27,
        CapError::AlreadyBound => 28,
        // M10. Appended, for the same reason as M9's block above: a
        // component that has learned what 21 means must not find it
        // meaning something else after a kernel upgrade.
        CapError::IrqAlreadyOwned { .. } => 29,
        // M11. Appended, as always. Distinct from `NotEnoughMemory`
        // (11) on purpose: one means the caller's own budget is
        // spent, the other that a kernel-wide pool is. See
        // `CapError::NoKernelStack`.
        CapError::NoKernelStack { .. } => 30,
    };
    ERROR_FLAG | code
}

/// Whether a returned value encodes an error, for the test.
pub fn is_error(value: u64) -> bool {
    value & ERROR_FLAG != 0
}

/// The error code out of a returned value, for the test.
pub fn error_code(value: u64) -> u64 {
    value & !ERROR_FLAG
}

// ---- Methods -----------------------------------------------------------

/// What a user component may ask of a capability.
///
/// A `match` rather than a table lookup. A table would need its own
/// bounds check as a separate step, and a forgotten bounds check on a
/// user-supplied index is a classic way in.
///
/// Deliberately small. M8 adds **no new operations** — these are M5's
/// and M6's invocations, reachable from ring 3 for the first time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum Method {
    /// Report what this thread is doing. Takes a TCB.
    ReadRegisters = 1,
    /// Store a priority. Takes a TCB. (ADR 024: stored, not used.)
    SetPriority = 2,
    /// Say hello, so a user thread can prove it reached the kernel.
    ///
    /// Not a capability operation and not a new kernel service: it
    /// returns a value derived from its argument and touches nothing.
    /// It exists because the *first* thing to prove is that the
    /// round trip works at all, and proving that with an invocation
    /// would entangle two failures.
    Echo = 3,
    /// Block until a Notification is signalled. Takes a Notification.
    ///
    /// Added in M10, and it is the first invocation that **blocks**.
    /// Everything before it answered and returned; this one parks the
    /// calling thread until something signals, which is what lets a
    /// ring-3 driver wait for its device rather than poll for it.
    ///
    /// That works without special handling because a syscall runs on
    /// the calling thread's own kernel stack (ADR 029): blocking saves
    /// the context there like any other switch, and the return to
    /// ring 3 happens whenever the thread is resumed.
    Wait = 4,

    // ---- M11: a component manages its own address space (ADR 043)
    //
    // Seven invocations, and **no new kernel logic behind any of
    // them**. M7 wrote `map_frame`, `map_table` and the rest to take
    // capabilities and check rights; opening them to user space is a
    // resolution layer and nothing more. That is the dividend of
    // ADR 011's mechanism/policy split, collected four milestones
    // later.
    /// `Frame.Map`: place this Frame in a VSpace.
    ///
    /// Target: the Frame. Arguments: the VSpace's CPtr, the address,
    /// the permissions.
    FrameMap = 5,

    /// `Frame.Unmap`: remove whatever is mapped at an address.
    ///
    /// Target: the **VSpace**, not the Frame — and that is not an
    /// oversight. One Frame can be mapped into several address spaces
    /// (ADR 027), so a Frame capability does not say which mapping is
    /// meant. The address space plus the address does.
    FrameUnmap = 6,

    /// `PageTable.Map`: supply an intermediate table.
    ///
    /// Target: the PageTable being inserted. Arguments: the VSpace's
    /// CPtr, the address it covers, the level (3 = PDPT, 2 = PD,
    /// 1 = PT).
    PageTableMap = 7,

    /// `PageTable.Unmap`: take an intermediate table back out.
    ///
    /// Target: the VSpace, for the reason [`Method::FrameUnmap`]
    /// gives.
    PageTableUnmap = 8,

    /// `Untyped.Retype`: turn memory into objects.
    ///
    /// The invocation that makes a component self-sufficient. Without
    /// it a component must ask init for every object it needs, which
    /// turns init into an allocator — exactly what ADR 020 avoided by
    /// making an Untyped a watermark rather than a heap.
    ///
    /// Four arguments, which is why M11 widened the ABI: the type,
    /// `size_bits` and count packed into one word, and the
    /// destination as a CNode CPtr plus an offset.
    Retype = 9,

    /// `TCB.SetSpace`: give a thread an address space and a CSpace.
    ///
    /// **What makes a component able to create another component.**
    /// Scenario 5 is one user thread building a second and starting
    /// it, which is M12's root task in miniature.
    SetSpace = 10,

    /// `TCB.Resume`: make a configured thread runnable.
    Resume = 11,

    /// `PageTable.CreateVSpace`: turn a fresh root into an address
    /// space.
    ///
    /// **Not in ADR 043's list of seven**, and its absence was found
    /// by writing scenario 5 rather than by reading the code. The
    /// claim "no new kernel logic" survives — `create_vspace` is M7's,
    /// unchanged — but the claim "seven invocations are enough to
    /// build a component" did not. A component can retype a
    /// PageTable and map it as an *intermediate* table; it had no way
    /// to make one a root, so the address space it was supposed to
    /// give its child could not exist.
    ///
    /// The kernel half is copied in by `create_vspace`, which is why
    /// this cannot be "the user writes a PML4 itself": the entries
    /// that make interrupts deliverable are not the caller's to
    /// write.
    CreateVSpace = 12,

    /// `TCB.SetUserRegisters`: entry point and stack for a ring-3
    /// thread.
    ///
    /// M6's `SetRegisters` takes a Rust `fn` pointer and is useless
    /// here — a kernel address is exactly what a user thread must not
    /// start at. This takes two user-half addresses, which
    /// `configure_user` already asserts.
    ///
    /// **The kernel stack is not an argument**, and that is the
    /// boundary this invocation makes visible. A thread entering the
    /// kernel needs a stack in the kernel half; kernel-half memory is
    /// named by no capability (ADR 027's K1 is the same rule seen
    /// from the other side), so a component cannot supply one and
    /// must not be able to. The kernel assigns it. See
    /// [`crate::cap::tcb::set_user_registers`] for what that costs.
    SetUserRegisters = 13,
}

impl Method {
    fn from_bits(bits: u64) -> Option<Method> {
        match bits {
            1 => Some(Method::ReadRegisters),
            2 => Some(Method::SetPriority),
            3 => Some(Method::Echo),
            4 => Some(Method::Wait),
            5 => Some(Method::FrameMap),
            6 => Some(Method::FrameUnmap),
            7 => Some(Method::PageTableMap),
            8 => Some(Method::PageTableUnmap),
            9 => Some(Method::Retype),
            10 => Some(Method::SetSpace),
            11 => Some(Method::Resume),
            12 => Some(Method::CreateVSpace),
            13 => Some(Method::SetUserRegisters),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Method::ReadRegisters => "ReadRegisters",
            Method::SetPriority => "SetPriority",
            Method::Echo => "Echo",
            Method::Wait => "Wait",
            Method::FrameMap => "Frame.Map",
            Method::FrameUnmap => "Frame.Unmap",
            Method::PageTableMap => "PageTable.Map",
            Method::PageTableUnmap => "PageTable.Unmap",
            Method::Retype => "Untyped.Retype",
            Method::SetSpace => "TCB.SetSpace",
            Method::Resume => "TCB.Resume",
            Method::CreateVSpace => "PageTable.CreateVSpace",
            Method::SetUserRegisters => "TCB.SetUserRegisters",
        }
    }

    /// What type of capability this method may be invoked on.
    ///
    /// A property of the method rather than a constant at the
    /// resolution site, which is the change M10 forced: until now
    /// every invocation took a TCB, so `resolve_typed(.., Tcb)` was
    /// both correct and indistinguishable from a hardcoded
    /// assumption. `Wait` takes a Notification, and the difference
    /// has to live somewhere a new method cannot miss.
    ///
    /// `Echo` has no answer -- it names no capability at all -- and
    /// returning `None` is what keeps it from being given a plausible
    /// one.
    fn capability_type(self) -> Option<CapType> {
        match self {
            Method::ReadRegisters | Method::SetPriority => Some(CapType::Tcb),
            Method::Wait => Some(CapType::Notification),
            Method::FrameMap => Some(CapType::Frame),
            // Both `Unmap`s target the address space rather than the
            // object being unmapped; see `Method::FrameUnmap`.
            Method::FrameUnmap | Method::PageTableMap | Method::PageTableUnmap => {
                Some(CapType::PageTable)
            }
            Method::Retype => Some(CapType::Untyped),
            Method::SetSpace | Method::Resume => Some(CapType::Tcb),
            Method::CreateVSpace => Some(CapType::PageTable),
            Method::SetUserRegisters => Some(CapType::Tcb),
            Method::Echo => None,
        }
    }
}
/// The last method a capability invocation named.
///
/// Recorded so the report can say *which* invocation ran, not merely
/// that syscalls happened. Without it, "the capability path works
/// from ring 3" would rest entirely on `Echo` — and `Echo` resolves
/// no capability at all.
static LAST_METHOD: AtomicU64 = AtomicU64::new(0);

/// Name of the last method invoked, for the report.
pub fn last_method_name() -> &'static str {
    match Method::from_bits(LAST_METHOD.load(Ordering::Acquire)) {
        Some(method) => method.name(),
        None => "<none yet>",
    }
}

// ---- The dispatcher ----------------------------------------------------

/// Handle one system call.
///
/// Called by the stub with a pointer to the frame it built. Returns the
/// value to place in RAX.
///
/// Every failure returns; none panics. From this milestone the caller
/// is a user component, and a kernel that stops because somebody passed
/// a bad pointer is a kernel any user component can halt.
///
/// # Safety
///
/// Called only from [`syscall_entry`], with `frame` pointing at the
/// register block it pushed on the current kernel stack.
#[unsafe(no_mangle)]
unsafe extern "C" fn syscall_dispatch(frame: *mut SyscallFrame) -> u64 {
    // SAFETY: The stub passes a pointer to the frame it just pushed.
    // It is aligned (the stack is 16-byte aligned and every push is 8
    // bytes), fully initialised, and valid for this call because the
    // stub does not touch the stack below it until we return.
    let frame = unsafe { &*frame };

    SYSCALL_COUNT.fetch_add(1, Ordering::AcqRel);

    // The kernel stack this call arrived on. Captured here because
    // this is the one place that runs on the calling thread's own
    // stack with its identity known -- the acceptance test uses it to
    // show two user threads enter on *different* stacks (ADR 029).
    let kernel_rsp = frame as *const SyscallFrame as u64;

    // Four argument registers from M11, not two (ADR 043). `syscall`
    // destroys RCX and R11, leaving RDX, R10, R8 and R9 -- ADR 034
    // counted them for IPC and the invocation path had been using
    // only the first two. `Frame.Map` needs three and
    // `Untyped.Retype` four, so the registers that were already there
    // are now used.
    //
    // The alternative was packing permissions into an address's low
    // bits, which are free because the address is page-aligned. It
    // works and it is wrong: `0x400000 | 2` is not an address, and
    // one day somebody treats it as one.
    let args = Args {
        arg0: frame.rdx,
        arg1: frame.r10,
        arg2: frame.r8,
        arg3: frame.r9,
    };

    let result = dispatch(frame.rdi, frame.rsi, args, kernel_rsp);

    match result {
        Ok(value) => {
            debug_assert!(
                value & ERROR_FLAG == 0,
                "an invocation returned a value with the error flag set; \
                 user code would read a success as a failure"
            );
            value
        }
        Err(error) => {
            SYSCALL_ERRORS.fetch_add(1, Ordering::AcqRel);
            encode_error(error)
        }
    }
}

/// The four argument registers, grouped.
///
/// A struct rather than four parameters, because `dispatch` already
/// had five and M11 would have made it seven. Grouping them also puts
/// the ABI in one place: a reader asking "what is in R8" finds the
/// answer here rather than counting positional arguments.
#[derive(Clone, Copy)]
struct Args {
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
}

/// The CSpace the calling thread resolves its CPtrs through
/// (ADR 043).
///
/// **This function is M11's actual subject.** Until now the
/// dispatcher used `boot::init_cspace()` unconditionally, which was
/// correct while every user thread was the test's own and becomes an
/// isolation hole the moment two components exist: one CSpace means
/// every thread can name every capability, so "component B cannot
/// reach component A's PageTable" would be a claim no test could
/// falsify.
///
/// A zero `cspace_root` means the init CSpace. That is a definition
/// rather than a fallback — the boot thread and the kernel's own test
/// threads *are* init.
fn current_cspace() -> Result<crate::cap::cptr::CSpace, CapError> {
    // SAFETY: We are servicing a syscall, so there is a current
    // thread, and nothing else holds a borrow of it on this path.
    let tcb = unsafe { crate::sched::current() };

    let root = tcb.cspace_root();
    if root == 0 {
        return Ok(crate::cap::boot::init_cspace());
    }

    // The capability that was handed over may have been revoked
    // since. Address plus generation, the ADR 018 idiom: without the
    // generation a revoked-and-reused CNode would resolve this
    // thread's CPtrs through a stranger's capabilities, which is
    // worse than failing.
    if !tcb.cspace_is_live() {
        return Err(CapError::StaleCPtr);
    }

    // SAFETY: `cspace_is_live` just confirmed the slot still holds
    // the CNode this thread was bound to, with `1 << radix` slots.
    Ok(unsafe { crate::cap::cptr::CSpace::new(crate::memory::PhysAddr(root), tcb.cspace_radix()) })
}

/// Resolve the capability, check the method, and perform it.
///
/// Split out of [`syscall_dispatch`] so the error paths are ordinary
/// `?` returns rather than a cascade of matches.
fn dispatch(
    cptr_bits: u64,
    method_bits: u64,
    args: Args,
    kernel_rsp: u64,
) -> Result<u64, CapError> {
    let method = Method::from_bits(method_bits).ok_or(CapError::InvalidArgument)?;
    let arg0 = args.arg0;

    // Echo first: it names no capability, so resolving one would be
    // asking a question with no answer.
    if method == Method::Echo {
        // The acceptance test uses Echo as its channel back from ring
        // 3, because a user thread has no other way to say anything.
        // The *kernel* learns nothing from it -- that is what makes
        // Echo safe to expose -- so the test's bookkeeping lives in
        // the test module rather than here.
        //
        // `cptr_bits` carries the channel. Echo ignores it as a
        // capability pointer, which is precisely why it is free to
        // mean something else.
        //
        // Each test module gets first refusal on the channel, in
        // turn, and says whether it was theirs. The channels are
        // distinct bits (M8 uses 8, 9, 10, 16 and 17; M10 uses 20 and
        // 21), so the order does not matter -- but asking rather than
        // computing means a collision shows up as a report going to
        // the wrong place rather than as a silent drop.
        if !crate::usertest::record_forge_result(cptr_bits, arg0)
            && !crate::irqtest::record_user_report(cptr_bits, arg0)
            && !crate::vspaceuser::record_step(cptr_bits, arg0)
        {
            crate::usertest::record_report(cptr_bits, arg0, kernel_rsp);
        }

        // Something derived from the argument rather than the argument
        // itself. Returning the input unchanged would pass even if the
        // kernel never looked at it -- a mirror does not prove anyone
        // is home.
        return Ok(arg0.wrapping_add(1).rotate_left(1));
    }

    let cptr = CPtr(cptr_bits);

    // **The calling thread's own CSpace**, not init's (ADR 043).
    //
    // This one line is what the M11 negatives rest on. While every
    // user thread was the kernel test's own, `init_cspace()` was
    // correct and indistinguishable from a hardcoded assumption; with
    // two components it is an isolation hole, because one CSpace
    // means every thread can name every capability.
    let space = current_cspace()?;

    let required = method
        .capability_type()
        .expect("Echo is handled before resolution and names no capability");

    // SAFETY: `current_cspace` returned a live CSpace -- either init's,
    // or one whose generation it just rechecked. `resolve_typed`
    // validates every field of the CPtr -- reserved bits, both
    // indices, both generations, and that the root slot holds a CNode
    // (ADR 018) -- so a forged pointer returns an error rather than
    // reaching an object. No other mutable borrow of the slot is held
    // here.
    let slot = unsafe { crate::cap::cptr::resolve_typed(&space, cptr, required) }?;

    // Which method ran, recorded for the report. A single counter
    // would say "syscalls happened"; this says which ones, and the
    // test asserts that `ReadRegisters` was among them -- otherwise
    // "the capability path works from ring 3" would rest on `Echo`
    // alone, and `Echo` touches no capability at all.
    LAST_METHOD.store(method as u64, Ordering::Release);

    match method {
        Method::ReadRegisters => {
            // SAFETY: `resolve_typed` established the slot holds a live
            // TCB, and nothing else borrows it on this path.
            let registers = unsafe { tcb::read_registers(slot) }?;
            // The switch count: small, non-secret, and enough for a
            // user thread to observe that it is being scheduled.
            //
            // Not RIP or RSP, deliberately. Those are kernel-half
            // addresses for a thread that has entered the kernel, and
            // handing them to ring 3 would leak the kernel's layout --
            // exactly what KASLR exists to prevent, on the day this
            // kernel has KASLR.
            Ok(registers.switch_count)
        }
        Method::SetPriority => {
            let priority = u8::try_from(arg0).map_err(|_| CapError::InvalidArgument)?;
            // SAFETY: As above.
            unsafe { tcb::set_priority(slot, priority) }?;
            Ok(0)
        }
        Method::Wait => {
            // **This does not return until something signals.**
            //
            // The calling thread is parked and another runs; control
            // comes back here when a `Signal` -- from a thread or, in
            // M10, from an interrupt handler -- wakes it. The driver
            // loop a ring-3 component writes is built on exactly
            // this.
            //
            // The mask goes back in RAX. It cannot collide with the
            // error encoding: a badge is 32 bits (ADR 037) and
            // ERROR_FLAG is bit 63.
            //
            // SAFETY: `resolve_typed` established the slot holds a
            // live Notification, and nothing else borrows the current
            // TCB on this path.
            let mask = unsafe { crate::cap::ipc::wait(slot) }?;
            Ok(mask as u64)
        }

        // ---- M11: address spaces, from ring 3 -----------------------
        //
        // Every arm below calls an M7 function unchanged. That is the
        // claim ADR 043 makes and the reward ADR 011 predicted: the
        // functions were written to take capabilities and check
        // rights, so opening them to user space is a resolution
        // layer and nothing else.
        Method::FrameMap => {
            // RDI the Frame, RDX the VSpace root, R10 the address,
            // R8 the permissions.
            //
            // SAFETY: The target slot holds a Frame and this one must
            // hold a PageTable, so they cannot be the same slot -- a
            // slot has one type. No aliasing `&mut Slot` arises.
            let root = unsafe { resolve_aux(&space, args.arg0, CapType::PageTable) }?;
            let permissions = decode_permissions(args.arg2)?;

            // SAFETY: `resolve_typed` established the target slot
            // holds a live Frame and `resolve_aux` that the root slot
            // holds a live PageTable. They are distinct slots -- a
            // slot has one type -- so no aliasing arises, and nothing
            // else borrows either here.
            unsafe {
                crate::cap::vspace::map_frame(
                    root,
                    slot,
                    crate::memory::VirtAddr(args.arg1),
                    permissions,
                )
            }?;
            Ok(0)
        }
        Method::FrameUnmap => {
            // The target is the **address space**, not the Frame: one
            // Frame may be mapped in several VSpaces (ADR 027), and a
            // Frame capability does not say which one is meant.
            //
            // SAFETY: `resolve_typed` established the slot holds a
            // live PageTable, and nothing else borrows it here.
            let frame = unsafe {
                crate::cap::vspace::unmap_frame(slot, crate::memory::VirtAddr(args.arg0))
            }?;

            // The physical address is **not** returned, for
            // `ReadRegisters`' reason: it is a fact about the
            // machine's layout, and ring 3 has no use for it that
            // ring 3 could not get by asking its own allocator. The
            // caller already holds the Frame capability; that is the
            // Frame.
            let _ = frame;
            Ok(0)
        }
        Method::PageTableMap => {
            // RDI the table being inserted, RDX the root, R10 the
            // address, R8 the level (3 = PDPT, 2 = PD, 1 = PT).
            //
            // **The one arm where both capabilities have the same
            // type**, and therefore the one where the caller can name
            // the same slot twice. Every other pair differs -- Frame
            // and PageTable, Untyped and CNode, TCB and PageTable and
            // CNode -- so a slot's single type keeps them apart. Here
            // it does not, and resolving one slot twice would hand
            // out two `&mut` to it.
            //
            // The CPtrs are compared *before* resolution, because
            // forming the second reference is already the error. The
            // comparison is exact rather than conservative: two CPtrs
            // that both resolve successfully reach the same slot only
            // if their index pairs are equal, and then their
            // generations must both equal the slot's and so equal
            // each other (ADR 018). Equal slot therefore means equal
            // bits.
            if args.arg0 == cptr_bits {
                return Err(CapError::InvalidArgument);
            }

            // SAFETY: Not the target slot, by the check above, and
            // the only other reference in scope is to that slot.
            let root = unsafe { resolve_aux(&space, args.arg0, CapType::PageTable) }?;
            let level = u8::try_from(args.arg2).map_err(|_| CapError::InvalidArgument)?;

            // The kernel does **not** produce the table (ADR 027): a
            // missing intermediate is `MissingPageTable`, and the
            // caller offers one. That is why this invocation exists at
            // all, and why a component's page tables come out of its
            // own budget.
            //
            // SAFETY: Both slots hold live PageTables by resolution
            // and are distinct, as argued above.
            unsafe {
                crate::cap::vspace::map_table(root, slot, crate::memory::VirtAddr(args.arg1), level)
            }?;
            Ok(0)
        }
        Method::PageTableUnmap => {
            // Target the address space, as `FrameUnmap`.
            let level = u8::try_from(args.arg1).map_err(|_| CapError::InvalidArgument)?;

            // SAFETY: As `FrameUnmap`.
            let table = unsafe {
                crate::cap::vspace::unmap_table(slot, crate::memory::VirtAddr(args.arg0), level)
            }?;
            let _ = table;
            Ok(0)
        }
        Method::Retype => {
            // RDI the Untyped, RDX what to make, R10 how many, R8 the
            // destination CNode, R9 the offset within it.
            //
            // `arg0` carries two fields: the type in the low byte and
            // `size_bits` in the next. ADR 043 put the compound on the
            // destination instead; it moved here during
            // implementation, for a reason better than preference. An
            // offset packed into the destination CPtr would land in
            // the CPtr's reserved field, and resolution *requires*
            // that field to be zero (ADR 018) -- so that packing is
            // not merely ugly, it is unrepresentable.
            //
            // Type and size_bits are not addresses and are not
            // independent: together they are one answer to "what
            // object". Packing them is not the mistake ADR 043
            // rejected, which was making a value that is not an
            // address look like one.
            let cap_type = decode_retype_target(args.arg0 & 0xFF)?;
            let size_bits = ((args.arg0 >> 8) & 0xFF) as u8;
            let count = usize::try_from(args.arg1).map_err(|_| CapError::InvalidArgument)?;
            let offset = usize::try_from(args.arg3).map_err(|_| CapError::InvalidArgument)?;

            // SAFETY: The target slot holds an Untyped and this one
            // must hold a CNode, so they are distinct slots.
            let dest_slot = unsafe { resolve_aux(&space, args.arg2, CapType::CNode) }?;
            // SAFETY: The capability says how many slots the object
            // has, and `resolve_aux` established it is a live CNode.
            let dest = unsafe {
                crate::cap::cnode::CNode::new(dest_slot.object().0, dest_slot.as_radix())
            };

            // **This is where ADR 022's budget claim becomes
            // testable.** Until now every retype ran inside the
            // kernel, so "a component can only spend its own memory"
            // was true but unobserved. A component exhausting its
            // Untyped from ring 3 observes it.
            //
            // SAFETY: The target slot holds a live Untyped and
            // `dest` a live CNode; they are distinct slots because
            // their types differ, so the mutable borrow of the
            // Untyped does not alias the CNode.
            unsafe {
                crate::cap::untyped::retype(slot, cap_type, size_bits, count, &dest, offset)
            }?;
            Ok(0)
        }
        Method::SetSpace => {
            // Both spaces, always. The kernel-side function takes
            // `Option` so boot can set one at a time; ring 3 does not
            // get that, because a thread with one and not the other
            // is a state with no use and a window with a hazard
            // (ADR 043). There is also no CPtr that means "none" --
            // zero is a perfectly good pointer to slot 0 of slot 0 --
            // so an optional argument would need a flag word to
            // disambiguate, for a case nobody wants.
            // SAFETY: Three required types -- TCB for the target,
            // PageTable here, CNode below -- and a slot has one type,
            // so the three references cannot name the same slot.
            let vspace = unsafe { resolve_aux(&space, args.arg0, CapType::PageTable) }?;
            // SAFETY: As above.
            let cspace = unsafe { resolve_aux(&space, args.arg1, CapType::CNode) }?;

            // SAFETY: All three slots hold live capabilities of the
            // required types, and nothing else borrows the TCB here.
            unsafe { tcb::set_space(slot, Some(vspace), Some(cspace)) }?;
            Ok(0)
        }
        Method::Resume => {
            // SAFETY: `resolve_typed` established the slot holds a
            // live TCB, and nothing else borrows it on this path.
            let thread = unsafe { tcb::from_slot(slot) }?;
            // SAFETY: `thread` is that live TCB.
            unsafe { crate::sched::resume(thread) }?;
            Ok(0)
        }
        Method::CreateVSpace => {
            // SAFETY: `resolve_typed` established the slot holds a
            // live PageTable. `create_vspace` refuses a table that is
            // already mapped somewhere, so the "nothing has mapped
            // it" half of its contract is checked rather than
            // assumed.
            unsafe { crate::cap::vspace::create_vspace(slot) }?;
            Ok(0)
        }
        Method::SetUserRegisters => {
            // RDI the TCB, RDX the entry point, R10 the user stack,
            // R8 the CNode of kernel-stack frames, R9 the offset.
            //
            // The frames are the caller's and the virtual range is
            // the kernel's; see `set_user_registers` for why that
            // split is forced rather than chosen.
            let offset = usize::try_from(args.arg3).map_err(|_| CapError::InvalidArgument)?;

            // SAFETY: The target slot holds a TCB and this one must
            // hold a CNode, so they are distinct slots.
            let frames_slot = unsafe { resolve_aux(&space, args.arg2, CapType::CNode) }?;
            // SAFETY: The capability says how many slots the object
            // has, and resolution established it is a live CNode.
            let frames = unsafe {
                crate::cap::cnode::CNode::new(frames_slot.object().0, frames_slot.as_radix())
            };

            // SAFETY: The target slot holds a live TCB and `frames` a
            // live CNode; they are distinct slots because their types
            // differ.
            unsafe { tcb::set_user_registers(slot, args.arg0, args.arg1, &frames, offset) }?;
            Ok(0)
        }

        // Handled above, before the capability was resolved.
        Method::Echo => unreachable!("Echo is handled before resolution"),
    }
}

/// Resolve a second capability named by an argument register.
///
/// The target CPtr arrives in RDI and is resolved by `dispatch`; the
/// invocations M11 adds name a second object -- the address space to
/// map into, the CNode to retype into -- and it resolves **through the
/// same CSpace**. That is the whole of negative (b): a component that
/// does not hold a capability for another component's page table
/// cannot produce a CPtr that reaches it, because the indices walk
/// from its own root.
///
/// # Safety
///
/// As [`crate::cap::cptr::resolve_typed`]: the caller must not hold
/// another mutable borrow of the resolved slot. Every caller here
/// resolves slots of differing types, and a slot has one type.
unsafe fn resolve_aux(
    space: &crate::cap::cptr::CSpace,
    bits: u64,
    required: CapType,
) -> Result<&'static mut crate::cap::cnode::Slot, CapError> {
    // SAFETY: Caller's guarantee; `resolve_typed` validates every
    // field of the pointer before it reaches an object.
    unsafe { crate::cap::cptr::resolve_typed(space, CPtr(bits), required) }
}

/// Decode the permissions argument of `Frame.Map` (ADR 032).
///
/// Three values, and the fourth combination -- writable *and*
/// executable -- has no encoding. W^X is not enforced here; it is
/// unrepresentable, which is a stronger statement and the reason
/// [`crate::cap::vspace::MapPermissions`] is an enum rather than a
/// flags word.
fn decode_permissions(bits: u64) -> Result<crate::cap::vspace::MapPermissions, CapError> {
    match bits {
        0 => Ok(crate::cap::vspace::MapPermissions::ReadOnly),
        1 => Ok(crate::cap::vspace::MapPermissions::ReadWrite),
        2 => Ok(crate::cap::vspace::MapPermissions::ReadExecute),
        _ => Err(CapError::InvalidArgument),
    }
}

/// Decode the object type of `Untyped.Retype`.
///
/// Written out rather than reusing `CapType::from_bits`, and the
/// difference matters: these numbers are a **user-visible ABI**, and
/// inheriting them from an internal enum's discriminants would mean a
/// renumbering inside the kernel silently changed what ring 3 asks
/// for. The match is exhaustive, so a type added later cannot be
/// forgotten here -- it has to be refused or allowed on purpose.
fn decode_retype_target(bits: u64) -> Result<CapType, CapError> {
    let requested = match bits {
        1 => CapType::Untyped,
        2 => CapType::CNode,
        3 => CapType::Tcb,
        4 => CapType::Endpoint,
        5 => CapType::Notification,
        6 => CapType::Frame,
        7 => CapType::PageTable,
        8 => CapType::IrqHandler,
        9 => CapType::IrqControl,
        _ => return Err(CapError::InvalidArgument),
    };

    match requested {
        CapType::Untyped
        | CapType::CNode
        | CapType::Tcb
        | CapType::Endpoint
        | CapType::Notification
        | CapType::Frame
        | CapType::PageTable => Ok(requested),

        // `Null` is the absence of a capability, not a thing to make.
        CapType::Null => Err(CapError::InvalidArgument),

        // An IRQHandler comes from `IRQControl.Get` and nowhere else
        // (ADR 041): the kernel hands out a line, and a handler
        // object conjured from memory would name no line. Retype
        // *can* produce the object -- it has a size -- which is
        // exactly why the refusal is written here rather than left to
        // chance.
        CapType::IrqHandler => Err(CapError::InvalidArgument),

        // The one capability naming no object at all. There is
        // exactly one, created at boot; `object_layout` would refuse
        // it anyway, and saying so here makes the reason visible.
        CapType::IrqControl => Err(CapError::InvalidArgument),
    }
}

// ---- The entry stub ----------------------------------------------------

unsafe extern "C" {
    /// Where `IA32_LSTAR` points. Defined in the assembly below.
    pub fn syscall_entry();
}

/// Address of the entry stub, for `cpu::enable_syscall`.
pub fn entry_address() -> u64 {
    // Through a pointer rather than a direct numeric cast of a function
    // item, for the reason `sched::entry_trampoline` gives.
    (syscall_entry as *const ()) as u64
}

// The stub. Entered from ring 3 with:
//
//   RCX = the address to return to
//   R11 = the caller's RFLAGS
//   RSP = the *user's* stack -- nothing has switched it
//   RFLAGS = the caller's, minus IF, DF, TF and AC (IA32_FMASK)
//
// The first two instructions are the ones that matter. Until RSP points
// at kernel memory, a push would write to an address user code chose.
// That is also why IF is masked: an interrupt arriving in this window
// would push its frame there.
//
// RIP-relative addressing for both statics, so no general register is
// spent before they have been saved. See ADR 030 on why this is not
// `swapgs`.
core::arch::global_asm!(
    ".section .text",
    ".code64",
    ".align 16",
    ".global syscall_entry",
    "syscall_entry:",
    // 1. Park the user's RSP and take the kernel's.
    "    mov [rip + SAVED_USER_RSP], rsp",
    "    mov rsp, [rip + KERNEL_RSP]",
    // 2. Build the frame. The user RSP goes on first, so it lands at
    //    the highest address -- matching `SyscallFrame`'s last field.
    "    push qword ptr [rip + SAVED_USER_RSP]",
    // 3. All fifteen general registers.
    //
    //    Every one, not just the callee-saved set, and the reason runs
    //    both ways: a register not saved leaks the user's value into
    //    the kernel, and a register not restored leaks the kernel's
    //    value back out. The second is the one that is easy to forget
    //    and the one that matters -- a leftover kernel pointer in a
    //    scratch register is a gift to an attacker.
    "    push rax",
    "    push rbx",
    "    push rcx",
    "    push rdx",
    "    push rsi",
    "    push rdi",
    "    push rbp",
    "    push r8",
    "    push r9",
    "    push r10",
    "    push r11",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    // 4. SysV: first argument in RDI, which now points at the frame.
    //
    //    No `cld` here -- IA32_FMASK already cleared DF on entry, which
    //    is one of the four reasons it exists (ADR 030). The exception
    //    stubs still need theirs, because an exception does not consult
    //    FMASK.
    "    mov rdi, rsp",
    "    call syscall_dispatch",
    // 5. The result goes to the frame's RAX slot, so the pops below
    //    deliver it rather than restoring what the user had.
    "    mov [rsp + {rax}], rax",
    // 6. Restore everything.
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop r11",
    "    pop r10",
    "    pop r9",
    "    pop r8",
    "    pop rbp",
    "    pop rdi",
    "    pop rsi",
    "    pop rdx",
    "    pop rcx",
    "    pop rbx",
    "    pop rax",
    // 7. Is RCX canonical?
    //
    //    `sysret` loads RIP from RCX. On Intel, a non-canonical RCX
    //    raises #GP *after* SS has taken its user value but while the
    //    stack is still the kernel's -- so the handler runs at CPL 0
    //    on a stack user code can influence. That is CVE-2012-0217,
    //    and it took out Xen, FreeBSD, NetBSD and Windows at once.
    //
    //    The test: sign-extend bit 47 upwards and see whether the
    //    value is unchanged.
    //
    //    R11 is free here: it holds the RFLAGS `sysret` wants, but
    //    that value is already on its way back in R11 itself -- the
    //    pop above restored it. Using it as scratch would destroy the
    //    flags, so RBX is borrowed and restored instead.
    "    push rbx",
    "    mov rbx, rcx",
    "    shl rbx, 16",
    "    sar rbx, 16",
    "    cmp rbx, rcx",
    "    pop rbx",
    "    jne 3f",
    // 8a. Canonical: the fast path.
    "    push rcx",
    "    call syscall_count_sysret",
    "    pop rcx",
    "    mov rsp, [rip + SAVED_USER_RSP]",
    "    sysretq",
    // 8b. Not canonical: build an iretq frame instead.
    //
    //     `iretq` makes the privilege transition atomically and
    //     reports a bad RIP as a #GP in the *user's* context, which is
    //     where the fault belongs. Slower, and only reached when user
    //     code is already returning somewhere impossible.
    "3:",
    "    push rcx",
    "    call syscall_count_iretq",
    "    pop rcx",
    // The frame iretq expects, pushed high to low: SS, RSP, RFLAGS,
    // CS, RIP.
    "    mov rax, {user_ss}",
    "    push rax",
    "    push qword ptr [rip + SAVED_USER_RSP]",
    "    push r11",
    "    mov rax, {user_cs}",
    "    push rax",
    "    push rcx",
    "    iretq",
    rax = const frame_offset::RAX,
    user_ss = const crate::gdt::USER_DATA_SELECTOR as u64,
    user_cs = const crate::gdt::USER_CODE_SELECTOR as u64,
);

/// Report what the syscall path has done, for the acceptance test.
pub fn report() {
    println!(
        "[syscall] {} calls ({} returned an error), {} via sysret, {} \
         forced onto iretq by a non-canonical rcx",
        syscall_count(),
        error_count(),
        sysret_count(),
        iretq_count()
    );
    println!(
        "[syscall] last capability invocation: {} (one syscall, ADR 017 \
         -- the method is an argument, not a syscall number)",
        last_method_name()
    );
}
