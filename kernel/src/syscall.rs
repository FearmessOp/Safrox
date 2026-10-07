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
}

impl Method {
    fn from_bits(bits: u64) -> Option<Method> {
        match bits {
            1 => Some(Method::ReadRegisters),
            2 => Some(Method::SetPriority),
            3 => Some(Method::Echo),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Method::ReadRegisters => "ReadRegisters",
            Method::SetPriority => "SetPriority",
            Method::Echo => "Echo",
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

    let result = dispatch(frame.rdi, frame.rsi, frame.rdx, frame.r10, kernel_rsp);

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

/// Resolve the capability, check the method, and perform it.
///
/// Split out of [`syscall_dispatch`] so the error paths are ordinary
/// `?` returns rather than a cascade of matches.
fn dispatch(
    cptr_bits: u64,
    method_bits: u64,
    arg0: u64,
    _arg1: u64,
    kernel_rsp: u64,
) -> Result<u64, CapError> {
    let method = Method::from_bits(method_bits).ok_or(CapError::InvalidArgument)?;

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
        if !crate::usertest::record_forge_result(cptr_bits, arg0) {
            crate::usertest::record_report(cptr_bits, arg0, kernel_rsp);
        }

        // Something derived from the argument rather than the argument
        // itself. Returning the input unchanged would pass even if the
        // kernel never looked at it -- a mirror does not prove anyone
        // is home.
        return Ok(arg0.wrapping_add(1).rotate_left(1));
    }

    let cptr = CPtr(cptr_bits);
    let space = crate::cap::boot::init_cspace();

    // SAFETY: The CSpace is live from boot. `resolve_typed` validates
    // every field of the CPtr -- reserved bits, both indices, both
    // generations, and that the root slot holds a CNode (ADR 018) --
    // so a forged pointer returns an error rather than reaching an
    // object. No other mutable borrow of the slot is held here.
    let slot = unsafe { crate::cap::cptr::resolve_typed(&space, cptr, CapType::Tcb) }?;

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
        // Handled above, before the capability was resolved.
        Method::Echo => unreachable!("Echo is handled before resolution"),
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
