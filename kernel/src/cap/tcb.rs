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

/// Where a thread is in its lifecycle (ADR 026, extended by ADR 033).
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
    /// Waiting for a rendezvous on an endpoint (ADR 033).
    ///
    /// **In that endpoint's queue**, which is what distinguishes it
    /// from the state below and what `cancel_ipc` needs to know.
    BlockedOnEndpoint = 4,
    /// Made a `Call` and is waiting for the reply (ADR 036).
    ///
    /// **In no queue at all.** The right to reply lives in the
    /// *server's* TCB, not in a structure the caller is parked on --
    /// which is why waking this thread needs no queue walk.
    BlockedOnReply = 5,
}

impl ThreadState {
    const fn from_bits(bits: u8) -> Option<ThreadState> {
        match bits {
            0 => Some(ThreadState::Inactive),
            1 => Some(ThreadState::Ready),
            2 => Some(ThreadState::Running),
            3 => Some(ThreadState::Suspended),
            4 => Some(ThreadState::BlockedOnEndpoint),
            5 => Some(ThreadState::BlockedOnReply),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            ThreadState::Inactive => "Inactive",
            ThreadState::Ready => "Ready",
            ThreadState::Running => "Running",
            ThreadState::Suspended => "Suspended",
            ThreadState::BlockedOnEndpoint => "BlockedOnEndpoint",
            ThreadState::BlockedOnReply => "BlockedOnReply",
        }
    }

    /// Whether the scheduler is holding a reference to this thread.
    ///
    /// ADR 026: a thread in one of these states cannot have its TCB
    /// revoked, because the queue or the CPU is still pointing at it.
    ///
    /// The two IPC states are deliberately **not** busy, and ADR 039
    /// settled the question ADR 026 left open. A blocked thread is not
    /// held by the scheduler; it is held by an endpoint, and an
    /// endpoint can be told to let go. Refusing to revoke it would make
    /// a thread immortal for as long as nobody sends it a message --
    /// and whether anybody does is up to another component.
    pub const fn is_busy(self) -> bool {
        matches!(self, ThreadState::Ready | ThreadState::Running)
    }

    /// Whether this thread is waiting on an IPC operation.
    pub const fn is_blocked_on_ipc(self) -> bool {
        matches!(
            self,
            ThreadState::BlockedOnEndpoint | ThreadState::BlockedOnReply
        )
    }
}

/// Bytes of the TCB the IPC buffer will occupy from M9.
///
/// Reserved now rather than later: ADR 017 fixed the TCB at 2048 bytes
/// and the acceptance test asserts it, so the space has to exist for
/// the figure to mean anything.
pub const IPC_BUFFER_OFFSET: usize = 1024;

/// Where the FPU/SSE state lives inside the TCB (ADR 031).
///
/// `fxsave` writes 512 bytes and **requires 16-byte alignment** — a
/// misaligned one raises #GP, and that fault would not appear until
/// the first thread that touched the FPU.
///
/// A TCB is 2048-byte aligned (ADR 017), so any offset inside it that
/// is a multiple of 16 is 16-aligned in absolute terms too. 256 rather
/// than the first available multiple (144) so the scheduler's fields
/// have room to grow without moving this.
pub const FXSAVE_OFFSET: usize = 256;

/// Where M9's IPC fields begin inside the TCB.
///
/// M8 left a gap between the scheduler's state (which ended at 136) and
/// the fxsave area at 256, specifically so that fields added later would
/// not move anything the assembly reads. M9 is what that gap was for.
pub const IPC_FIELDS_START: usize = 136;

/// Where they end. Ten fields, 80 bytes.
///
/// Named rather than written as a literal because the reserved array
/// below is sized from it: get this wrong and the TCB changes size,
/// which the `const` assertion turns into a build failure instead of a
/// misaligned fxsave area at runtime.
///
/// M9b moved this from 208 to 216 for `replying_server`. The gap M8
/// left runs to [`FXSAVE_OFFSET`], so the room was already there and
/// nothing else moved — which is what the gap was for.
pub const IPC_FIELDS_END: usize = 216;

/// Where M11's CSpace fields end. Three fields, 24 bytes.
///
/// The same gap, used again. M8 left 256 - 136 = 120 bytes between
/// the scheduler's state and the fxsave area; M9 took 80 of them and
/// M11 takes 24 more, leaving 16.
///
/// That the room was there is not luck — ADR 029 reserved it
/// deliberately so that a field added later would not move the
/// fxsave area, which the assembly's alignment requirement depends
/// on. Three milestones have now added fields without anything
/// moving.
pub const CSPACE_FIELDS_END: usize = 240;

/// Size of an `fxsave` area: fixed by the architecture.
///
/// Reference: Intel SDM Vol. 2A, "FXSAVE", Table 3-43.
pub const FXSAVE_SIZE: usize = 512;

/// Default MXCSR: every SSE exception masked.
///
/// The value matters because the alternative is zero, and a zeroed
/// MXCSR means every exception *unmasked* — the first floating-point
/// operation in a new thread would raise #XM. Writing zeroes into an
/// `fxrstor` area is not "a clean FPU"; it is a differently broken
/// one.
///
/// Reference: Intel SDM Vol. 1, 11.6.4 ("the default MXCSR value at
/// reset is 1F80H").
const MXCSR_DEFAULT: u32 = 0x1F80;

/// Default x87 control word: all exceptions masked, extended precision.
///
/// Reference: Intel SDM Vol. 1, 8.1.5.
const X87_CW_DEFAULT: u16 = 0x037F;

/// Offsets within the `fxsave` area that `configure` has to fill in.
///
/// Reference: Intel SDM Vol. 2A, Table 3-43 "Layout of the FXSAVE and
/// FXRSTOR Memory Region".
const FXSAVE_FCW_OFFSET: usize = 0;
const FXSAVE_MXCSR_OFFSET: usize = 24;

/// Where a reply must be sent, and the proof it is still the same
/// thread (ADR 036).
///
/// The two fields travel together everywhere, and keeping them apart
/// was how `set_pending_send` grew to eight parameters. They are not
/// two pieces of data but one: **an address plus the generation that
/// makes it checkable** — the idiom ADR 018 established for CPtrs and
/// ADR 028 reused for VSpaces. An address on its own is exactly the
/// thing that goes stale silently.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReplySlot {
    /// Physical address of the slot holding the caller's TCB
    /// capability.
    pub slot: PhysAddr,
    /// The generation that slot had when the reply right was recorded.
    pub generation: u8,
}

impl ReplySlot {
    /// No reply expected.
    pub const NONE: ReplySlot = ReplySlot {
        slot: PhysAddr(0),
        generation: 0,
    };
}

/// How a blocked thread's IPC wait ended (ADR 039).
///
/// Four outcomes and no fifth. "Undefined" is not among them — the
/// master prompt's rule applied to a state machine rather than to a
/// value.
///
/// # Why `Cancelled` and `ReplyGone` are separate
///
/// M9b added the fourth, and the reason is ADR 039's error table
/// rather than tidiness:
///
/// | Cancelled while | The caller is told |
/// |---|---|
/// | `BlockedOnEndpoint` (Send or Recv) | `EndpointDead` |
/// | `BlockedOnReply` | `ReplyTargetGone` |
///
/// A `Call` passes through *both* states, so by the time it wakes its
/// own state no longer says which one it was cancelled in. One value
/// for both would have made a dead server and a revoked endpoint
/// indistinguishable to the client — two situations with different
/// remedies. The distinction is recorded where it is still known, in
/// `cancel_ipc`, which can see the state it is cancelling.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum SendOutcome {
    /// Still queued; nothing has happened.
    Pending = 0,
    /// A receiver took the message, or a reply arrived.
    Delivered = 1,
    /// The endpoint was revoked, or this thread was taken out of its
    /// queue by `Suspend` while waiting for a rendezvous (ADR 039).
    Cancelled = 2,
    /// This thread was waiting for a **reply** that will never come:
    /// the server died, or the wait was cancelled (ADR 036).
    ReplyGone = 3,
}

impl SendOutcome {
    const fn from_bits(bits: u8) -> SendOutcome {
        match bits {
            1 => SendOutcome::Delivered,
            2 => SendOutcome::Cancelled,
            3 => SendOutcome::ReplyGone,
            // Anything else reads as "nothing has happened", which is
            // the only safe interpretation: it leaves the thread
            // waiting rather than inventing a completion it never had.
            _ => SendOutcome::Pending,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            SendOutcome::Pending => "Pending",
            SendOutcome::Delivered => "Delivered",
            SendOutcome::Cancelled => "Cancelled",
            SendOutcome::ReplyGone => "ReplyGone",
        }
    }
}

/// The message a thread is blocked trying to send.
///
/// A blocked sender is not executing, so its message cannot stay in
/// registers; and an endpoint holds no storage (ADR 033), so it cannot
/// go there either. It waits here, in the sender's own TCB — which is
/// precisely what ADR 033 means by "the message lives in the sender's
/// TCB".
///
/// `#[repr(C)]` so the layout is written out rather than inferred. No
/// assembly reads it, but the TCB's padding is computed from its size
/// and an inferred layout would make that arithmetic a guess.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PendingSend {
    /// The message tag.
    tag: u64,
    /// The first three message words. The rest are already in the IPC
    /// buffer, where the sender wrote them.
    registers: [u64; 3],
    /// The badge on the capability the sender used (ADR 037).
    badge: u32,
    /// A [`SendOutcome`].
    outcome: u8,
    /// Whether this is a `Call`, so a reply target must be recorded.
    wants_reply: u8,
    /// Generation of the sender's own TCB slot, for that reply target.
    reply_generation: u8,
    _pad: u8,
    /// Physical address of the slot holding the sender's own TCB
    /// capability, so a reply can be checked for staleness (ADR 036).
    reply_slot: u64,
    /// The capability pointers the message carries — at most four
    /// (ADR 035). How many are meaningful is in the tag.
    caps: [u64; 4],
}

const _: () = {
    // 8 (tag) + 24 (registers) + 4 (badge) + 4 (four bytes of flags)
    // + 8 (reply_slot) + 32 (caps) = 80.
    assert!(
        core::mem::size_of::<PendingSend>() == 80,
        "PendingSend changed size; the TCB's reserved padding is \
         computed from it and would no longer add up"
    );
};

impl PendingSend {
    const EMPTY: PendingSend = PendingSend {
        tag: 0,
        registers: [0; 3],
        badge: 0,
        outcome: SendOutcome::Pending as u8,
        wants_reply: 0,
        reply_generation: 0,
        _pad: 0,
        reply_slot: 0,
        caps: [0; 4],
    };
}

/// A message waiting for a thread that is not running yet.
///
/// # Why this is per-thread, and what it replaced
///
/// M9a parked deliveries in **one global slot**, on the stated
/// invariant that "at most one delivery can be in flight, because the
/// transfer runs with interrupts disabled and the woken thread reads
/// its delivery before anything else can run".
///
/// The second half of that sentence is false, and M9b's first run
/// proved it: waking a thread calls `sched::enqueue`, which puts it on
/// the *ready queue*. It does not run. The thread that did the waking
/// carries on, and a server that replies and then receives again —
/// which is the entire shape of `ReplyRecv` — parks a second delivery
/// while the first is still sitting there.
///
/// The global slot's assertion caught it on the first attempt rather
/// than letting two clients receive each other's answers, which is
/// what that assertion was for. But the invariant it guarded was
/// never true; it was merely unreachable in M9a, where no two
/// deliveries ever overlapped.
///
/// A message addressed to a thread belongs in that thread. The send
/// direction already worked this way ([`PendingSend`]) for the same
/// reason, and the asymmetry was the oversight.
///
/// `#[repr(C)]` so the layout is written rather than inferred: the
/// reserved array below is sized from this type.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Incoming {
    /// The message tag.
    tag: u64,
    /// The first three message words.
    registers: [u64; 3],
    /// The badge on the capability the sender used (ADR 037), or the
    /// accumulated signal mask for a notification.
    badge: u32,
    /// Whether anything is here at all.
    ///
    /// A flag rather than "the tag is zero", because a zero-length,
    /// zero-label message is perfectly legal (ADR 034) and a receiver
    /// must be able to tell one from an empty mailbox.
    present: u8,
    _pad: [u8; 3],
}

const _: () = {
    assert!(
        core::mem::size_of::<Incoming>() == 40,
        "Incoming changed size; the TCB's reserved padding is computed \
         from it and would no longer add up"
    );
};

impl Incoming {
    const EMPTY: Incoming = Incoming {
        tag: 0,
        registers: [0; 3],
        badge: 0,
        present: 0,
        _pad: [0; 3],
    };
}

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

    /// Physical address of this thread's root page table, or zero to
    /// mean "the kernel's own address space" (ADR 027).
    ///
    /// Zero rather than an `Option` because the scheduler compares it
    /// against CR3 on every switch, and a null check costs the same
    /// instruction as the comparison it would otherwise do anyway.
    vspace_root: u64,

    /// Physical address of the slot the VSpace capability lives in.
    ///
    /// Needed because the staleness check has to *look at* that slot,
    /// and a TCB has no CSpace to resolve a CPtr through. The slot's
    /// address is stable: it sits inside a CNode object, which does
    /// not move.
    ///
    /// Zero when `vspace_root` is zero.
    vspace_slot: u64,

    /// Generation that slot had when `SetSpace` was called.
    ///
    /// Checked before every switch into this thread, alongside the
    /// slot still holding a PageTable. The root address alone would
    /// not be enough: revoking returns the memory to its Untyped and
    /// resets the watermark, so the *next* retype is likely to place
    /// a new object at exactly the old address. The thread would then
    /// resume in somebody else's address space, with no fault to
    /// mark the moment.
    ///
    /// Precisely the problem ADR 018 solved for CPtrs, wearing a
    /// different hat. See ADR 028.
    vspace_generation: u8,

    /// Whether this thread runs at CPL 3 (ADR 029).
    ///
    /// A byte rather than a `bool` so the struct's layout is written
    /// out rather than inferred; the assembly does not read it, but
    /// the field's position is part of a layout the `const` assertions
    /// below pin down.
    user_mode: u8,

    // ---- M9: IPC (ADR 033, 035, 036, 038) --------------------------
    //
    // Nine fields, 72 bytes, placed in the gap M8 left between the
    // scheduler's state and the fxsave area. ADR 017's 2048-byte
    // ceiling is unchanged and the assertion below still holds it.
    /// Whether the last bound-notification decision delivered the
    /// notification. Flipped on every delivery (ADR 038).
    ///
    /// **This single bit is the starvation guarantee.** With it, a
    /// thread waiting on both an endpoint and a bound notification
    /// alternates between them; without it, whichever source is chosen
    /// unconditionally can starve the other forever. A bit and a
    /// branch.
    ///
    /// ADR 038 originally said "notification first" and left the
    /// starvation risk as an open question to be measured in M12. That
    /// was a category error: a progress guarantee is not an
    /// optimisation, and it is not measured -- it either holds or it
    /// does not.
    last_delivery_was_notification: u8,
    _pad2: [u8; 5],

    /// Next thread in an endpoint's wait queue, or zero.
    ipc_next: u64,

    /// Previous thread in that queue, or zero.
    ///
    /// The field that makes cancellation O(1). M6's ready queue has no
    /// equivalent and removal there walks the list -- defensible,
    /// because that length is the *system's* thread count. An
    /// endpoint's queue length is an *adversary's* thread count, so the
    /// same cost would be a denial of service with interrupts disabled
    /// (ADR 033, corrected before implementation).
    ipc_prev: u64,

    /// The endpoint this thread is queued on, or zero.
    ///
    /// Recorded so that cancelling a wait can reach the queue directly.
    /// Without it, "take this thread off whichever endpoint holds it"
    /// would mean examining every endpoint in the system -- unbounded
    /// work on a path ADR 039 requires to be bounded, and the O(1)
    /// doubly linked list would have bought nothing.
    blocked_on_endpoint: u64,

    /// Physical address of the thread that is waiting for this thread
    /// to reply, or zero (ADR 036).
    ///
    /// Lives in the *server's* TCB rather than the caller's, and the
    /// moment it is written is load-bearing: at rendezvous, not when
    /// `Recv` returns. Written later, a second receiver arriving on the
    /// same endpoint could steal the caller.
    reply_to: u64,

    /// The generation `reply_to`'s TCB slot had at rendezvous.
    ///
    /// Address plus generation, the idiom ADR 018 established for
    /// CPtrs and ADR 028 reused for VSpaces. Without it, a caller whose
    /// TCB was revoked and whose memory was retyped into a *new* TCB at
    /// the same address would receive a reply meant for a thread that
    /// no longer exists. `ipc-stale-reply` is that scenario.
    reply_generation: u8,
    _pad3: [u8; 7],

    /// Physical address of the slot holding the caller's TCB
    /// capability, so the generation can be re-read later.
    reply_slot: u64,

    /// Where incoming capabilities may land: the CNode's base address,
    /// or zero if `SetReceiveSlots` was never called (ADR 035).
    ///
    /// Zero is not "a safe default" but the *absence of a
    /// destination*. A sender cannot name a slot in another thread's
    /// CSpace -- if it could, the full/empty distinction would let it
    /// scan -- so with no receive slots open there is nowhere for a
    /// capability to go. Taking authority is an action.
    receive_cnode: u64,

    /// First slot of the receive window, and how many.
    receive_index: u16,
    receive_count: u16,
    /// Radix of the receive CNode, so the window can be bounds-checked
    /// without resolving a capability at transfer time.
    receive_radix: u8,
    _pad4: [u8; 3],

    /// The Notification bound to this thread, or zero (ADR 038).
    ///
    /// While set, a `Recv` on any endpoint can also be woken by this
    /// notification. M13's driver needs exactly that: one thread, two
    /// sources -- a client request and an interrupt.
    bound_notification: u64,

    /// The server that owes *this* thread a reply, or zero (ADR 039).
    ///
    /// The other half of `reply_to`, and the two are always set and
    /// cleared together:
    ///
    /// ```text
    /// server.reply_to        -> caller
    /// caller.replying_server -> server
    /// ```
    ///
    /// Without it, cancelling a caller's wait would mean *finding* the
    /// server that holds the reply right, and the only way to do that
    /// is to examine every TCB in the system. ADR 039 rejected the
    /// scan as unbounded work on a path the master prompt requires to
    /// be bounded, and bought the back-pointer for eight bytes.
    ///
    /// The eight bytes carry an obligation: the two fields must agree,
    /// so a `debug_assert` guards the places that write either one.
    replying_server: u64,

    // ---- M11: the thread's own CSpace (ADR 043) --------------------
    //
    // Until M11 every CPtr was resolved against the *init* CSpace,
    // which was correct while every user thread was the test's own.
    // It stops being correct the moment two components exist: one
    // CSpace means every thread can name every capability, so
    // "component B cannot reach component A's PageTable" would be a
    // claim no test could falsify (ADR 023, K5).
    //
    // Three fields, and they mirror `vspace_root`/`vspace_slot`/
    // `vspace_generation` exactly. Not a coincidence: both record
    // "this thread is bound to that object, and that object is still
    // the one it was" -- the address-plus-generation idiom of
    // ADR 018, reused by ADR 028, ADR 036 and M10's IRQ handler.
    /// Physical address of this thread's root CNode, or zero for the
    /// init CSpace.
    ///
    /// Zero is a definition rather than a fallback: the boot thread
    /// and the kernel's test threads *are* init, so init's CSpace is
    /// theirs.
    cspace_root: u64,

    /// Physical address of the slot holding the root CNode
    /// capability, so its generation can be re-read.
    cspace_slot: u64,

    /// Generation that slot had when `SetSpace` was called.
    cspace_generation: u8,

    /// Radix of the root CNode: it has `1 << radix` slots.
    ///
    /// Cached here so that resolving a CPtr does not have to read the
    /// capability first. The root's size is fixed when the CSpace is
    /// bound and cannot change without a new `SetSpace`.
    cspace_radix: u8,

    _pad5: [u8; 6],

    /// Reserved. Brings the fixed part to FXSAVE_OFFSET.
    _reserved: [u8; FXSAVE_OFFSET - CSPACE_FIELDS_END],

    /// FPU/SSE state, saved and restored on **every** switch (ADR 031).
    ///
    /// Not lazily: the kernel never sets CR0.TS and never takes #NM.
    /// Lazy switching would save a `fxsave` on threads that do not use
    /// the FPU — a population that barely exists in user code — and it
    /// is the mechanism behind CVE-2018-3665.
    ///
    /// `align(16)` is not written here because the containing struct's
    /// 2048-byte alignment plus a 16-multiple offset already gives it;
    /// the `const` assertion below checks that rather than trusting it.
    fpu_state: [u8; FXSAVE_SIZE],

    /// What this thread is in the middle of sending, while it is
    /// blocked on an endpoint (ADR 033).
    ///
    /// A blocked sender is not executing, so its message cannot stay
    /// in registers; and the endpoint holds no storage, so it cannot
    /// go there either. It stays in the sender's own TCB, which is
    /// exactly what ADR 033 said the message does.
    ///
    /// Placed after the fxsave area rather than in the 136..256 gap
    /// because it is **not** on the hot path: a rendezvous that
    /// happens immediately never touches it.
    pending: PendingSend,

    /// The message this thread was woken with, if any (M9b).
    ///
    /// Beside `pending` and for the symmetrical reason: a thread that
    /// is not running cannot be handed anything in registers, so both
    /// directions need somewhere to wait. See [`Incoming`] for what
    /// this replaced and why the global slot it replaced could not
    /// work.
    incoming: Incoming,

    /// Reserved. Brings the fixed part to IPC_BUFFER_OFFSET.
    _reserved2: [u8; IPC_BUFFER_OFFSET
        - FXSAVE_OFFSET
        - FXSAVE_SIZE
        - core::mem::size_of::<PendingSend>()
        - core::mem::size_of::<Incoming>()],

    /// The IPC buffer: 128 message words (ADR 017, ADR 034).
    ///
    /// Words `0..3` are **never used**: a message's first three words
    /// travel in registers, and the buffer is indexed `[i]` rather than
    /// `[i - 3]` so that the index needs no adjustment.
    ///
    /// That wastes 24 bytes and it was measured rather than assumed.
    /// `[i - 3]` costs four instructions on a single access, and --
    /// the finding that actually decided it -- prevents LLVM from
    /// recognising the copy loop as a `memcpy`, turning 14 instructions
    /// into 40. The copy loop is IPC's hot path. See ADR 034's
    /// measurement section and scripts/m9-index-probe.
    ///
    /// `[u64; 128]` rather than bytes because every access is
    /// word-sized and a byte array would need a cast at each one.
    ipc_buffer: [u64; super::IPC_BUFFER_SIZE / 8],
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

    // M8: the FPU area must be where ADR 031 says, and 16-byte
    // aligned. A misaligned `fxsave` raises #GP, and nothing would
    // reach that fault until the first thread used the FPU -- long
    // after whatever edit moved the field.
    assert!(
        core::mem::offset_of!(Tcb, fpu_state) == FXSAVE_OFFSET,
        "the fxsave area is not at FXSAVE_OFFSET"
    );
    assert!(
        FXSAVE_OFFSET.is_multiple_of(16),
        "the fxsave area must be 16-byte aligned; the TCB's own 2048-byte \
         alignment only helps if the offset is a multiple of 16"
    );
    assert!(
        FXSAVE_OFFSET + FXSAVE_SIZE <= IPC_BUFFER_OFFSET,
        "the fxsave area runs into the IPC buffer"
    );

    // M9: the IPC fields sit in the gap M8 left between the
    // scheduler's state and the fxsave area.
    //
    // Pinned as *relationships* rather than as absolute offsets. The
    // absolute numbers depend on padding the compiler inserts for
    // alignment, so writing them out would make a build failure out of
    // a harmless change -- but the ordering and adjacency are what the
    // reserved array's size depends on, and those are checked.
    assert!(
        core::mem::offset_of!(Tcb, ipc_next) >= IPC_FIELDS_START,
        "the IPC fields start before the gap M8 left for them"
    );
    assert!(
        core::mem::offset_of!(Tcb, ipc_prev) == core::mem::offset_of!(Tcb, ipc_next) + 8,
        "ipc_prev must follow ipc_next: the queue code reads them as a \
         pair and a gap between them wastes a cache line on the one \
         structure IPC touches most"
    );
    assert!(
        core::mem::offset_of!(Tcb, blocked_on_endpoint) == core::mem::offset_of!(Tcb, ipc_prev) + 8,
        "blocked_on_endpoint must follow the queue links"
    );
    assert!(
        core::mem::offset_of!(Tcb, reply_to) > core::mem::offset_of!(Tcb, blocked_on_endpoint),
        "reply_to and blocked_on_endpoint swapped places"
    );
    assert!(
        core::mem::offset_of!(Tcb, receive_cnode) > core::mem::offset_of!(Tcb, reply_to),
        "receive_cnode and reply_to swapped places"
    );
    assert!(
        core::mem::offset_of!(Tcb, replying_server)
            == core::mem::offset_of!(Tcb, bound_notification) + 8,
        "replying_server must follow bound_notification"
    );
    assert!(
        core::mem::offset_of!(Tcb, replying_server) + 8 == IPC_FIELDS_END,
        "replying_server is not the last IPC field, or IPC_FIELDS_END \
         is wrong; the reserved array below is sized from it"
    );
    assert!(
        IPC_FIELDS_END <= FXSAVE_OFFSET,
        "the IPC fields run into the fxsave area"
    );

    // M11: the CSpace fields follow the IPC ones, in the same gap.
    assert!(
        core::mem::offset_of!(Tcb, cspace_root) == IPC_FIELDS_END,
        "the CSpace fields do not start where the IPC fields ended"
    );
    assert!(
        core::mem::offset_of!(Tcb, cspace_slot) == core::mem::offset_of!(Tcb, cspace_root) + 8,
        "cspace_slot must follow cspace_root: they are read together \
         on every staleness check"
    );
    assert!(
        core::mem::offset_of!(Tcb, cspace_slot) + 8 + 8 == CSPACE_FIELDS_END,
        "CSPACE_FIELDS_END disagrees with the fields; the reserved \
         array is sized from it"
    );
    assert!(
        CSPACE_FIELDS_END <= FXSAVE_OFFSET,
        "the CSpace fields run into the fxsave area"
    );

    // The buffer really is at the offset ADR 017 fixed, and really is
    // 128 words. ADR 034's length check is written against this.
    assert!(
        core::mem::offset_of!(Tcb, ipc_buffer) == IPC_BUFFER_OFFSET,
        "the IPC buffer is not at IPC_BUFFER_OFFSET"
    );
    assert!(
        super::IPC_BUFFER_SIZE / 8 == 128,
        "the IPC buffer is not 128 words; ADR 034's MAX_MESSAGE_WORDS \
         disagrees with ADR 017's buffer size"
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

    /// Physical address of this thread's root page table, or zero for
    /// the kernel's address space.
    pub fn vspace_root(&self) -> u64 {
        self.vspace_root
    }

    /// Generation the VSpace slot had when it was bound.
    pub fn vspace_generation(&self) -> u8 {
        self.vspace_generation
    }

    /// Whether this thread's address space is still the one it was
    /// given.
    ///
    /// Three comparisons, no loop (ADR 028). Each rules out a
    /// different way the capability could have gone away:
    ///
    /// * the slot is empty -- deleted or revoked;
    /// * it holds something else -- the memory was retyped;
    /// * the generation moved -- it was emptied and refilled, possibly
    ///   with another PageTable at the very same address.
    ///
    /// The third is the one that matters most, and the one an
    /// address-only check would miss.
    /// This thread's root CNode, or zero for the init CSpace
    /// (ADR 043).
    pub fn cspace_root(&self) -> u64 {
        self.cspace_root
    }

    /// Radix of that root: it has `1 << radix` slots.
    pub fn cspace_radix(&self) -> u8 {
        self.cspace_radix
    }

    /// Whether the CSpace capability this thread was given is still
    /// the one it was given.
    ///
    /// The same three comparisons [`Tcb::vspace_is_live`] makes, for
    /// the same reason: revoking returns memory to its Untyped and
    /// resets the watermark (ADR 020), so the next retype is *likely*
    /// to land a new object at the old address. A thread whose CSpace
    /// was revoked would otherwise resolve its CPtrs through whatever
    /// occupies that memory now — which is worse than failing, because
    /// it is a capability system silently pointing at a stranger.
    pub fn cspace_is_live(&self) -> bool {
        if self.cspace_root == 0 {
            // The init CSpace. Built by boot rather than retyped, so
            // no capability names it and nothing can revoke it.
            return true;
        }
        if self.cspace_slot == 0 {
            return false;
        }

        // SAFETY: A non-zero `cspace_slot` is the physical address of
        // a slot inside a live CNode, recorded by `set_cspace`. CNodes
        // do not move, and PMAP maps all RAM.
        let slot = unsafe {
            &*crate::memory::phys_to_virt(PhysAddr(self.cspace_slot)).as_ptr::<super::cnode::Slot>()
        };

        !slot.is_empty()
            && slot.cap_type() == CapType::CNode
            && slot.generation() == self.cspace_generation
            && slot.object().0 .0 == self.cspace_root
    }

    /// Bind this thread to a CSpace.
    ///
    /// # Safety
    ///
    /// `root` must name a live CNode of `1 << radix` slots, `slot` the
    /// capability naming it, and the thread must not be busy.
    unsafe fn set_cspace(&mut self, root: PhysAddr, slot: PhysAddr, generation: u8, radix: u8) {
        self.cspace_root = root.0;
        self.cspace_slot = slot.0;
        self.cspace_generation = generation;
        self.cspace_radix = radix;
    }

    /// Return this thread to the init CSpace.
    ///
    /// # Safety
    ///
    /// The thread must not be busy.
    pub unsafe fn clear_cspace(&mut self) {
        self.cspace_root = 0;
        self.cspace_slot = 0;
        self.cspace_generation = 0;
        self.cspace_radix = 0;
    }

    pub fn vspace_is_live(&self) -> bool {
        if self.vspace_root == 0 {
            // The kernel's address space. It cannot be revoked --
            // nothing holds a capability to it, because `vm::init`
            // built it rather than retyping it.
            return true;
        }
        if self.vspace_slot == 0 {
            return false;
        }

        // SAFETY: A non-zero `vspace_slot` is the physical address of
        // a slot inside a live CNode, recorded by `set_vspace`. CNodes
        // are never freed while a capability names them, and PMAP maps
        // all RAM.
        let slot = unsafe {
            &*crate::memory::phys_to_virt(PhysAddr(self.vspace_slot)).as_ptr::<super::cnode::Slot>()
        };

        !slot.is_empty()
            && slot.cap_type() == CapType::PageTable
            && slot.generation() == self.vspace_generation
            && slot.object().0 .0 == self.vspace_root
    }

    /// Forget this thread's address space, putting it back in the
    /// kernel's.
    ///
    /// Called by `SetSpace(None)`, and by the scheduler when it finds
    /// the VSpace has been revoked (ADR 028, case B). In the second
    /// case the thread is suspended at the same moment: this is not a
    /// silent fallback into the kernel's address space, which ADR 028
    /// rejected as worse than stopping.
    ///
    /// # Safety
    ///
    /// The caller must hold the scheduler's invariants and must not be
    /// executing in the VSpace being dropped.
    pub unsafe fn clear_vspace(&mut self) {
        self.vspace_root = 0;
        self.vspace_slot = 0;
        self.vspace_generation = 0;
    }

    /// Whether an address is inside this thread's kernel stack.
    ///
    /// The acceptance test uses this to prove two threads never share a
    /// stack, and the overflow report uses it to name the thread.
    pub fn owns_stack_address(&self, addr: u64) -> bool {
        addr > self.stack_bottom && addr <= self.stack_top
    }

    /// Whether this thread runs at CPL 3 (ADR 029).
    pub fn is_user_mode(&self) -> bool {
        self.user_mode != 0
    }

    /// Pointer to this thread's `fxsave` area.
    ///
    /// Raw rather than a slice, because `fxsave` and `fxrstor` take a
    /// memory operand the compiler must make no assumptions about.
    ///
    /// # Safety
    ///
    /// The caller must use it only as the operand of `fxsave` or
    /// `fxrstor`, and must hold no Rust reference to the same bytes
    /// across the instruction.
    pub unsafe fn fpu_state_ptr(&mut self) -> *mut u8 {
        let ptr = self.fpu_state.as_mut_ptr();
        debug_assert!(
            (ptr as usize).is_multiple_of(16),
            "the fxsave area is not 16-byte aligned, so fxsave would #GP"
        );
        ptr
    }

    /// Fill the `fxsave` area with a *clean* FPU state.
    ///
    /// Not zeroes. A zeroed area restores MXCSR = 0, which unmasks
    /// every SSE exception -- so the thread's first floating-point
    /// operation raises #XM -- and an x87 control word of 0, which is
    /// wrong in the same way. `fxrstor` can also #GP on reserved MXCSR
    /// bits.
    ///
    /// Only two fields need writing; the rest of a clean state really
    /// is zeroes (empty registers, clear tag word) and retype already
    /// zeroed the object.
    ///
    /// Same reasoning as zeroing RBP in [`Tcb::configure`]: a thread's
    /// initial state has to be chosen rather than inherited.
    fn initialise_fpu_state(&mut self) {
        self.fpu_state = [0; FXSAVE_SIZE];

        let fcw = X87_CW_DEFAULT.to_le_bytes();
        self.fpu_state[FXSAVE_FCW_OFFSET..FXSAVE_FCW_OFFSET + 2].copy_from_slice(&fcw);

        let mxcsr = MXCSR_DEFAULT.to_le_bytes();
        self.fpu_state[FXSAVE_MXCSR_OFFSET..FXSAVE_MXCSR_OFFSET + 4].copy_from_slice(&mxcsr);
    }

    /// What MXCSR this thread's saved state holds, for the test.
    ///
    /// Reported because "the FPU state was initialised" is otherwise
    /// indistinguishable from "the FPU state is zeroes", and the two
    /// behave identically until the first floating-point operation.
    pub fn saved_mxcsr(&self) -> u32 {
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(&self.fpu_state[FXSAVE_MXCSR_OFFSET..FXSAVE_MXCSR_OFFSET + 4]);
        u32::from_le_bytes(bytes)
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

    // ---- M9: IPC ---------------------------------------------------

    /// The message buffer: 128 words, of which `0..3` are never used.
    ///
    /// See the field's own documentation for why those three are
    /// wasted — it is a measured decision, not an oversight.
    pub fn ipc_buffer(&self) -> &[u64; super::IPC_BUFFER_SIZE / 8] {
        &self.ipc_buffer
    }

    pub fn ipc_buffer_mut(&mut self) -> &mut [u64; super::IPC_BUFFER_SIZE / 8] {
        &mut self.ipc_buffer
    }

    pub(crate) fn ipc_next(&self) -> u64 {
        self.ipc_next
    }

    pub(crate) fn set_ipc_next(&mut self, next: u64) {
        self.ipc_next = next;
    }

    pub(crate) fn ipc_prev(&self) -> u64 {
        self.ipc_prev
    }

    /// The queue links, for `ipc-cancel-cost` to touch before it
    /// starts the clock.
    ///
    /// The measurement times `Endpoint::remove`, which reads both
    /// links and writes to both neighbours. Reaching them for the
    /// first time costs a page walk and three cache misses — real
    /// costs, but not the one being measured, and large enough to
    /// swamp it: the first attempt reported a 50-thread queue as
    /// three hundred times *slower* than a 500-thread one, which is
    /// the warm-up and nothing else.
    pub fn ipc_prev_for_measurement(&self) -> u64 {
        self.ipc_prev
    }

    /// As [`Tcb::ipc_prev_for_measurement`].
    pub fn ipc_next_for_measurement(&self) -> u64 {
        self.ipc_next
    }

    pub(crate) fn set_ipc_prev(&mut self, prev: u64) {
        self.ipc_prev = prev;
    }

    /// Which endpoint this thread is queued on, or zero.
    pub fn blocked_on(&self) -> u64 {
        self.blocked_on_endpoint
    }

    pub(crate) fn set_blocked_on(&mut self, endpoint: u64) {
        self.blocked_on_endpoint = endpoint;
    }

    /// Who is waiting for this thread to reply (ADR 036).
    ///
    /// The caller's TCB address, the slot its capability lives in, and
    /// the generation that slot had at rendezvous.
    pub fn reply_target(&self) -> Option<(u64, PhysAddr, u8)> {
        if self.reply_to == 0 {
            return None;
        }
        Some((
            self.reply_to,
            PhysAddr(self.reply_slot),
            self.reply_generation,
        ))
    }

    /// Record that `caller` is waiting for this thread's reply.
    ///
    /// Called **at rendezvous**, not when `Recv` returns. ADR 036 is
    /// specific about the moment: written later, a second thread
    /// calling `Recv` on the same endpoint could claim a caller the
    /// first had already been matched with.
    ///
    /// # Safety
    ///
    /// `caller` must be the physical address of a live TCB entering
    /// `BlockedOnReply`, and `slot` the address of the slot holding
    /// its capability.
    pub(crate) unsafe fn set_reply_target(&mut self, caller: u64, slot: PhysAddr, generation: u8) {
        debug_assert!(
            self.reply_to == 0,
            "this thread already owes a reply to {:#018x}; recording a \
             second caller would strand the first for good",
            self.reply_to
        );
        self.reply_to = caller;
        self.reply_slot = slot.0;
        self.reply_generation = generation;

        // The back half of the link, written here so the two can
        // never be set separately. ADR 039 requires them to be either
        // both full or both empty, and the cheapest way to hold a
        // rule like that is to give it one place to live.
        //
        // SAFETY: The caller guarantees `caller` names a live TCB
        // entering BlockedOnReply.
        let caller_tcb = unsafe { tcb_at(ObjectRef(PhysAddr(caller))) };
        // SAFETY: Every TCB lives in the physical window.
        let own_phys = unsafe { tcb_phys(self) };
        caller_tcb.replying_server = own_phys.0;
    }

    /// Forget that this thread owes a reply.
    ///
    /// Clears only this end of the link. The caller's
    /// `replying_server` is cleared by whichever path knows the caller
    /// is still the thread it was — this function is also reached when
    /// the caller has *gone*, and following a stale pointer to tidy it
    /// up is exactly the bug the generation check exists to prevent.
    pub(crate) fn clear_reply_target(&mut self) {
        self.reply_to = 0;
        self.reply_slot = 0;
        self.reply_generation = 0;
    }

    /// Whether the recorded reply target is still the thread it was.
    ///
    /// Three comparisons, no loop — the shape ADR 018 established for
    /// CPtrs and ADR 028 reused for VSpaces:
    ///
    /// * the slot still holds a TCB capability,
    /// * it still names the same object,
    /// * its generation has not moved.
    ///
    /// The third is the one that matters. Revoking a TCB returns its
    /// memory to an Untyped and resets the watermark, so the next
    /// retype is likely to place a **new** TCB at exactly the old
    /// address. An address-only check would deliver the reply to a
    /// stranger, silently. `ipc-stale-reply` arranges precisely that
    /// and requires `ReplyTargetGone`.
    pub fn reply_target_is_live(&self) -> bool {
        let Some((caller, slot_addr, generation)) = self.reply_target() else {
            return false;
        };
        if slot_addr.0 == 0 {
            return false;
        }

        // SAFETY: A non-zero reply slot is the physical address of a
        // slot inside a live CNode, recorded at rendezvous. CNodes are
        // not freed while a capability names them, and PMAP maps all
        // RAM.
        let slot =
            unsafe { &*crate::memory::phys_to_virt(slot_addr).as_ptr::<super::cnode::Slot>() };

        !slot.is_empty()
            && slot.cap_type() == CapType::Tcb
            && slot.generation() == generation
            && slot.object().0 .0 == caller
    }

    /// Where incoming capabilities may land (ADR 035).
    ///
    /// `None` when `SetReceiveSlots` was never called — and that is
    /// not a refusal but an **absence**. A sender cannot name a slot
    /// in another thread's CSpace, so with no window open there is
    /// nowhere for a capability to go. Taking authority is an action.
    pub fn receive_slots(&self) -> Option<(super::cnode::CNode, usize, usize)> {
        if self.receive_cnode == 0 || self.receive_count == 0 {
            return None;
        }
        // SAFETY: `set_receive_slots` established that the CNode is
        // live with this radix and that the window lies inside it.
        let cnode =
            unsafe { super::cnode::CNode::new(PhysAddr(self.receive_cnode), self.receive_radix) };
        Some((
            cnode,
            self.receive_index as usize,
            self.receive_count as usize,
        ))
    }

    /// # Safety
    ///
    /// `base` must name a live CNode of `1 << radix` slots, with
    /// `index + count` inside it.
    pub(crate) unsafe fn set_receive_slots(
        &mut self,
        base: PhysAddr,
        radix: u8,
        index: u16,
        count: u16,
    ) {
        self.receive_cnode = base.0;
        self.receive_radix = radix;
        self.receive_index = index;
        self.receive_count = count;
    }

    pub(crate) fn clear_receive_slots(&mut self) {
        self.receive_cnode = 0;
        self.receive_radix = 0;
        self.receive_index = 0;
        self.receive_count = 0;
    }

    /// The Notification bound to this thread, or zero (ADR 038).
    pub fn bound_notification(&self) -> u64 {
        self.bound_notification
    }

    pub(crate) fn set_bound_notification(&mut self, notification: u64) {
        self.bound_notification = notification;
    }

    /// The server that owes this thread a reply, or zero (ADR 039).
    pub fn replying_server(&self) -> u64 {
        self.replying_server
    }

    /// Record which server owes this thread a reply.
    ///
    /// The back half of the two-way link; `set_reply_target` writes
    /// the front half on the server. Both are written at rendezvous
    /// and both are cleared when the reply arrives or the wait is
    /// cancelled.
    pub(crate) fn set_replying_server(&mut self, server: u64) {
        self.replying_server = server;
    }

    /// Whether the previous bound delivery was the notification.
    ///
    /// The alternation bit — ADR 038's starvation guarantee in one
    /// byte. See the field's documentation.
    #[allow(dead_code)] // M9c
    pub fn last_delivery_was_notification(&self) -> bool {
        self.last_delivery_was_notification != 0
    }

    pub(crate) fn set_last_delivery_was_notification(&mut self, value: bool) {
        self.last_delivery_was_notification = u8::from(value);
    }

    // ---- The message a blocked sender is holding -------------------

    /// Record what this thread is about to block trying to send.
    ///
    /// The register words go here because they are about to stop
    /// existing: a blocked thread's registers live in a saved context,
    /// and the three message words are not part of the callee-saved
    /// set a context switch preserves.
    pub(crate) fn set_pending_send(
        &mut self,
        tag: u64,
        registers: [u64; 3],
        badge: u32,
        wants_reply: bool,
        caps: &[u64],
        reply: ReplySlot,
    ) {
        let mut stored = [0u64; 4];
        for (dst, src) in stored.iter_mut().zip(caps.iter()) {
            *dst = *src;
        }
        self.pending = PendingSend {
            tag,
            registers,
            badge,
            outcome: SendOutcome::Pending as u8,
            wants_reply: u8::from(wants_reply),
            reply_generation: reply.generation,
            _pad: 0,
            reply_slot: reply.slot.0,
            caps: stored,
        };
    }

    /// What this thread is blocked sending.
    ///
    /// Returns the tag, the register words, the badge, whether a reply
    /// is wanted, and the capability pointers.
    pub(crate) fn pending_send(&self) -> (u64, [u64; 3], u32, bool, [u64; 4]) {
        (
            self.pending.tag,
            self.pending.registers,
            self.pending.badge,
            self.pending.wants_reply != 0,
            self.pending.caps,
        )
    }

    /// Where this thread's own TCB capability lives, for the reply
    /// target a receiver is about to record (ADR 036).
    pub(crate) fn pending_reply_slot(&self) -> ReplySlot {
        ReplySlot {
            slot: PhysAddr(self.pending.reply_slot),
            generation: self.pending.reply_generation,
        }
    }

    /// Say how a blocked sender's wait ended.
    pub(crate) fn record_send_outcome(&mut self, outcome: SendOutcome) {
        self.pending.outcome = outcome as u8;
    }

    /// Read and clear the outcome.
    ///
    /// Clearing matters: a thread that sent once and blocked again
    /// must not read the previous send's `Delivered` and conclude its
    /// new message was taken.
    pub(crate) fn take_send_outcome(&mut self) -> SendOutcome {
        let outcome = SendOutcome::from_bits(self.pending.outcome);
        self.pending.outcome = SendOutcome::Pending as u8;
        outcome
    }

    // ---- The message this thread was woken with (M9b) --------------

    /// Park a delivery for a thread that is not running yet.
    ///
    /// # Panics
    ///
    /// If one is already parked. A second delivery would overwrite a
    /// message the thread has not collected, and both situations that
    /// could produce one are kernel defects: a wake path that does not
    /// clear, or two senders matched with a single receiver.
    ///
    /// This assertion replaces the one on M9a's *global* delivery slot
    /// and keeps its value. What changes is that the check is now
    /// per-thread, so the **legal** case — two deliveries to two
    /// different threads — stops tripping it. That case is not
    /// hypothetical: it is every `ReplyRecv`.
    pub(crate) fn park_incoming(&mut self, tag: u64, badge: u32, registers: [u64; 3]) {
        assert!(
            self.incoming.present == 0,
            "a second message was parked for a thread that has not \
             collected the first; either a wake path is not clearing \
             the slot, or two senders were matched with one receiver"
        );
        self.incoming = Incoming {
            tag,
            registers,
            badge,
            present: 1,
            _pad: [0; 3],
        };
    }

    /// Collect the parked message, if there is one.
    ///
    /// `None` means nothing was parked, which is how a cancelled wait
    /// is told from a legitimately empty message — ADR 034 permits a
    /// zero-length, zero-label one, so the contents cannot carry that
    /// distinction.
    pub(crate) fn take_incoming(&mut self) -> Option<(u64, u32, [u64; 3])> {
        if self.incoming.present == 0 {
            return None;
        }
        let taken = self.incoming;
        self.incoming = Incoming::EMPTY;
        Some((taken.tag, taken.badge, taken.registers))
    }

    /// Throw away a parked message its target will never collect.
    pub(crate) fn discard_incoming(&mut self) {
        self.incoming = Incoming::EMPTY;
    }

    /// Reset every IPC field. Part of `configure`.
    pub(crate) fn clear_ipc_state(&mut self) {
        self.pending = PendingSend::EMPTY;
        self.incoming = Incoming::EMPTY;
        self.ipc_next = 0;
        self.ipc_prev = 0;
        self.blocked_on_endpoint = 0;
        self.last_delivery_was_notification = 0;
        self.clear_reply_target();
        self.clear_receive_slots();
        self.bound_notification = 0;
        self.replying_server = 0;
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
        self.user_mode = 0;

        // M9. A thread being given a fresh entry point must not
        // inherit a half-finished conversation: a stale `reply_to`
        // would make the *next* `Reply` wake a thread that is no
        // longer waiting, and stale queue links would corrupt the
        // first endpoint this thread touches.
        self.clear_ipc_state();

        // A clean FPU, not a zeroed one (ADR 031). The difference is
        // MXCSR: zero unmasks every SSE exception, so the thread's
        // first floating-point operation would raise #XM.
        self.initialise_fpu_state();

        // The address space is *not* reset here. `SetRegisters` gives a
        // thread somewhere to start; `SetSpace` gives it somewhere to
        // live, and the two are independent invocations. Clearing the
        // VSpace here would make the order they are called in matter,
        // which is the sort of hidden coupling that shows up much
        // later as "it works if you do it the other way round".
    }

    /// Give this thread a user entry point, to be reached at **CPL 3**.
    ///
    /// Same shape as [`Tcb::configure`] and a different destination.
    /// The thread still starts on its kernel stack, in the kernel, with
    /// a trampoline as its RIP -- but that trampoline ends in `iretq`
    /// into ring 3 rather than a `jmp` into kernel code.
    ///
    /// # Why the first entry is `iretq` and not `sysret`
    ///
    /// Both are "return" instructions and both can lower the privilege
    /// level; there is no instruction for *entering* ring 3. ADR 029
    /// chose `iretq` for the first entry because all five values -- RIP,
    /// CS, RFLAGS, RSP, SS -- are written out on the stack where they
    /// can be read if the transition faults. `sysret` derives CS and SS
    /// from an MSR and takes RFLAGS from R11, so a mistake there is a
    /// `#GP` with nothing to look at.
    ///
    /// Later returns, from syscalls, do use `sysret`: that path is hot
    /// and it runs thousands of times rather than once.
    ///
    /// # The register handoff
    ///
    /// Three values have to reach the trampoline, and the only channel
    /// is the callee-saved set the context switch restores:
    ///
    /// * **R12** -- the user entry point,
    /// * **R13** -- the user stack pointer,
    /// * **R14** -- a value handed to the user code in RDI.
    ///
    /// Same mechanism M6 used to get an entry point past the switch,
    /// for the same reason: `switch_context` loads these as part of an
    /// ordinary context, so nothing special is needed on the way in.
    ///
    /// # Safety
    ///
    /// `kernel_stack_top` must be this thread's own kernel stack, as
    /// [`Tcb::configure`] requires. `user_entry` must be a canonical
    /// user address that is mapped executable in this thread's VSpace,
    /// and `user_stack` a mapped, writable, 16-byte-aligned user
    /// address. Neither is checked here: the mapping lives in a VSpace
    /// this function cannot see, and a wrong value produces a #PF in
    /// ring 3 -- which is a user fault, not a kernel one.
    pub unsafe fn configure_user(
        &mut self,
        user_entry: u64,
        user_stack: u64,
        argument: u64,
        kernel_stack_top: VirtAddr,
        kernel_stack_bottom: VirtAddr,
        stack_slot: u64,
    ) {
        assert!(
            crate::memory::layout::is_user(user_entry),
            "the user entry point {:#018x} is not in the user half; a \
             thread at CPL 3 cannot fetch from the kernel half at all",
            user_entry
        );
        assert!(
            crate::memory::layout::is_user(user_stack),
            "the user stack {:#018x} is not in the user half",
            user_stack
        );
        assert!(
            user_stack.is_multiple_of(16),
            "the user stack {:#018x} is not 16-byte aligned",
            user_stack
        );

        // SAFETY: Caller's guarantee about the kernel stack. The entry
        // function passed here is the trampoline, which diverges.
        unsafe {
            self.configure(
                crate::sched::user_entry_trampoline_fn(),
                kernel_stack_top,
                kernel_stack_bottom,
                stack_slot,
            );
        }

        // `configure` built a *kernel* thread's context: RIP is M6's
        // trampoline and R12 is whatever entry point it was handed.
        // Both have to be replaced, and forgetting the first one is
        // not a subtle failure -- it is this milestone's first real
        // bug, and SMEP caught it.
        //
        // The symptom was a #PF at the user entry point with
        // `cs=0x0008` and the instruction-fetch bit set: M6's
        // trampoline had run `sti; jmp r12` with R12 already pointing
        // at user code, so the kernel jumped into a USER page at
        // CPL 0. SMEP refused the fetch, which is exactly what ADR 032
        // says it is for -- it simply found a kernel bug before it
        // found a hostile one.
        //
        // Had SMEP been off, the user program would have run happily
        // at ring 0 and every check in `user-test` would have passed
        // except the one that reads CS.
        self.context.rip = crate::sched::user_entry_trampoline();
        self.context.r12 = user_entry;
        self.context.r13 = user_stack;
        self.context.r14 = argument;

        self.user_mode = 1;
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

        // M9: the boot thread starts with no IPC state, like any
        // other. Retype zeroed the object, but the boot TCB is also
        // reachable through this path after a `configure`, so the
        // reset is written rather than assumed.
        self.clear_ipc_state();

        // The boot thread is kernel code and stays kernel code.
        self.user_mode = 0;

        // Its FPU state is live in the CPU right now, and the first
        // switch *away* from it is what writes this area -- the same
        // argument that keeps `adopt_running` from touching `context`.
        // But the area must still hold a legal state in case something
        // restores it before that first save, so it is initialised
        // rather than left as retype's zeroes.
        self.initialise_fpu_state();

        // The boot thread runs in the kernel's address space, which is
        // what zero means (ADR 027). It never gets a VSpace capability
        // because the kernel PML4 was built by `vm::init` rather than
        // retyped, so there is no slot that could name it.
        self.vspace_root = 0;
        self.vspace_slot = 0;
        self.vspace_generation = 0;

        // And the init CSpace, which is what zero means there
        // (ADR 043). The symmetry is exact: the init CSpace is built
        // by `cap::boot` rather than retyped, so no capability names
        // it either.
        //
        // Written out rather than left to retype's zeroing for the
        // reason the IPC reset above gives: the boot TCB reaches this
        // path after a `configure` too, where the object is not
        // freshly zeroed.
        self.cspace_root = 0;
        self.cspace_slot = 0;
        self.cspace_generation = 0;
        self.cspace_radix = 0;
    }

    /// Bind this thread to an address space.
    ///
    /// # Safety
    ///
    /// `root` must be the physical address of a live PML4 whose kernel
    /// half is populated, and `slot` the address of the slot holding
    /// its capability. The thread must not be running.
    unsafe fn set_vspace(&mut self, root: PhysAddr, slot: PhysAddr, generation: u8) {
        self.vspace_root = root.0;
        self.vspace_slot = slot.0;
        self.vspace_generation = generation;
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
    /// Root page table, or zero for the kernel's address space.
    pub vspace_root: u64,
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

/// Kernel stack slots handed out to threads created from ring 3.
///
/// Slots 0 and 1 are IST1 and the main stack; the kernel's own tests
/// use up to 11. User-created threads start well clear of those.
const USER_KSTACK_FIRST: u64 = 32;

/// How many such threads may exist. **A bound, deliberately.**
///
/// Without one, a component could create threads until the KSTACK
/// region ran out and take the machine with it — so the limit is not
/// tidiness, it is the same budget argument as ADR 022 applied to a
/// resource that is *address space* rather than memory.
///
/// This is the one allocation in the kernel that a component triggers
/// and does not pay for in frames, because the frames are its own
/// (see [`set_user_registers`]) and only the virtual range is the
/// kernel's. A proper answer gives each component a quota; M11 gives
/// a global ceiling and says so.
const USER_KSTACK_COUNT: u64 = 16;

/// Next free slot.
static NEXT_USER_KSTACK: AtomicU64 = AtomicU64::new(USER_KSTACK_FIRST);

/// `TCB.SetUserRegisters`: give a thread a ring-3 entry point.
///
/// M6's [`set_registers`] takes a Rust `fn` pointer, which is a kernel
/// address — exactly what a user thread must not start at. This takes
/// two user-half addresses, which `configure_user` asserts.
///
/// # The kernel stack
///
/// A thread entering the kernel needs a stack in the kernel half, and
/// **no capability names kernel-half memory** — that is ADR 027's K1
/// seen from the other side. So the caller cannot supply the address.
///
/// It does supply the *frames*: `stack_frames` is a CNode holding
/// [`KSTACK_FRAMES`] Frame capabilities from the caller's own Untyped,
/// starting at `offset`. The kernel chooses the virtual range and maps
/// them there. Authority over the memory stays with the component;
/// authority over the address space stays with the kernel; and ADR
/// 022's claim that the kernel allocates nothing for itself survives
/// a component creating a thread.
///
/// # Safety
///
/// `tcb_slot` must hold a live TCB and `frames` a live CNode, and the
/// caller must not hold another mutable borrow of either.
pub unsafe fn set_user_registers(
    tcb_slot: &Slot,
    entry: u64,
    user_stack: u64,
    frames: &super::cnode::CNode,
    offset: usize,
) -> Result<(), CapError> {
    count_invocation();

    // Both addresses are the caller's, so both are checked here
    // rather than asserted in `configure_user` — an assert is for a
    // kernel mistake, and these are a user's.
    if !crate::memory::layout::is_user(entry) || !crate::memory::layout::is_user(user_stack) {
        return Err(CapError::NotUserAddress { address: entry });
    }
    if !user_stack.is_multiple_of(16) {
        return Err(CapError::InvalidArgument);
    }

    let needed = KSTACK_FRAMES;
    if offset
        .checked_add(needed)
        .is_none_or(|end| end > frames.len())
    {
        return Err(CapError::CPtrOutOfRange);
    }

    // Every frame validated before any is mapped: the two-phase shape
    // ADR 020 uses for retype, for the same reason. A half-mapped
    // kernel stack is worse than none.
    for index in 0..needed {
        // SAFETY: Index checked against `frames.len()` above.
        let slot = unsafe { frames.slot(offset + index) };
        if slot.is_empty() {
            return Err(CapError::EmptySlot);
        }
        if slot.cap_type() != CapType::Frame {
            return Err(CapError::WrongType {
                found: slot.cap_type(),
            });
        }
        // A device frame as a kernel stack would put the kernel's
        // saved registers in MMIO space.
        if slot.is_device() {
            return Err(CapError::InvalidArgument);
        }
    }

    // SAFETY: Caller's guarantee.
    let tcb = unsafe { from_slot(tcb_slot) }?;
    if tcb.state().is_busy() {
        return Err(CapError::ThreadBusy {
            state: tcb.state().name(),
        });
    }

    // Claim a stack slot. `fetch_add` and then check, so two threads
    // racing here cannot be handed the same range.
    let stack_slot = NEXT_USER_KSTACK.fetch_add(1, Ordering::AcqRel);
    if stack_slot >= USER_KSTACK_FIRST + USER_KSTACK_COUNT {
        return Err(CapError::NoKernelStack {
            limit: USER_KSTACK_COUNT,
        });
    }

    let (_, bottom, top) = crate::memory::layout::kstack_slot(stack_slot);

    for index in 0..needed {
        // SAFETY: Index checked, and the loop above established the
        // slot holds a live, non-device Frame.
        let slot = unsafe { frames.slot(offset + index) };
        let frame = crate::memory::PhysFrame::containing(slot.object().0);
        let page =
            crate::memory::VirtAddr::new(bottom.0 + (index as u64) * crate::memory::FRAME_SIZE);

        // SAFETY: The kernel's tables are live; the address is inside
        // a KSTACK slot this call has just claimed and nothing else
        // has mapped; the frame is a live Frame capability.
        unsafe {
            let table = crate::memory::vm::kernel_table();
            let mut source = crate::memory::heap::reserve_frames();
            let flags = crate::memory::paging::PageFlags::READ
                | crate::memory::paging::PageFlags::WRITE
                | crate::memory::paging::PageFlags::GLOBAL;
            match crate::memory::paging::map(table, page, frame, flags, &mut source) {
                Ok(tlb) => tlb.flush(),
                // The slot was never handed out before, so this can
                // only mean the intermediate tables could not be
                // built -- out of the *heap's* reserve, not the
                // caller's Untyped, which is why it is not
                // `NotEnoughMemory` either.
                Err(_) => {
                    return Err(CapError::NoKernelStack {
                        limit: USER_KSTACK_COUNT,
                    })
                }
            }
        }
    }

    // SAFETY: The kernel stack was just mapped and is this thread's
    // alone; the user addresses were checked above.
    unsafe {
        tcb.configure_user(entry, user_stack, 0, top, bottom, stack_slot);
    }

    Ok(())
}

/// Frames one kernel stack needs.
const KSTACK_FRAMES: usize =
    (crate::memory::layout::KSTACK_SIZE / crate::memory::FRAME_SIZE) as usize;

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
        vspace_root: tcb.vspace_root(),
    })
}

/// `SetSpace`: give a thread an address space and a CSpace.
///
/// Refused for a busy thread, for the same reason `SetRegisters` is:
/// the scheduler holds a reference to it, and changing CR3 under a
/// thread it is about to switch into would put that thread in an
/// address space it never agreed to (ADR 026, ADR 028).
///
/// Passing `None` for either returns the thread to the kernel's
/// address space, or to the init CSpace. For the VSpace that is what
/// keeps ADR 028's refusal to revoke a live one from being a trap:
/// the running thread can always move out of the way first, so the
/// two-step sequence always terminates.
///
/// # Why both in one call (M11, ADR 043)
///
/// M7's version took only a VSpace, because there was only one
/// CSpace. From M11 a component gives a thread *both* — and they are
/// not independent: a thread with its own address space and somebody
/// else's capabilities is not a component, it is a confusion.
///
/// One call also means there is no window in which a thread has one
/// and not the other. That matters because the gap would be visible:
/// a thread is startable the moment both are set, and a scheduler
/// that caught it halfway would run it with a CSpace naming
/// capabilities for an address space it is not in.
///
/// # Safety
///
/// All slots must hold live capabilities, and the caller must not
/// hold another mutable borrow of the TCB.
pub unsafe fn set_space(
    tcb_slot: &Slot,
    vspace_slot: Option<&Slot>,
    cspace_slot: Option<&Slot>,
) -> Result<(), CapError> {
    count_invocation();

    // SAFETY: Caller's guarantee.
    let tcb = unsafe { from_slot(tcb_slot) }?;

    if tcb.state().is_busy() {
        return Err(CapError::ThreadBusy {
            state: tcb.state().name(),
        });
    }

    // Both arguments are validated **before** either is written, so a
    // refused call leaves the thread exactly as it was. The same
    // two-phase shape as `retype` (ADR 020): a caller that asked for
    // something impossible should not find its thread half-changed.
    let vspace = match vspace_slot {
        None => None,
        Some(slot) => {
            let root = super::vspace::root_address(slot)?;
            // The slot's own physical address, so the staleness check
            // can look at it later. A TCB has no CSpace to resolve a
            // CPtr through, and the slot's address is stable: slots
            // live inside CNode objects, and a CNode does not move.
            let slot_phys = PhysAddr(core::ptr::from_ref(slot) as u64 - crate::memory::pmap_base());
            Some((root, slot_phys, slot.generation()))
        }
    };

    let cspace = match cspace_slot {
        None => None,
        Some(slot) => {
            if slot.is_empty() {
                return Err(CapError::EmptySlot);
            }
            if slot.cap_type() != CapType::CNode {
                return Err(CapError::WrongType {
                    found: slot.cap_type(),
                });
            }
            let root = slot.object().0;
            let slot_phys = PhysAddr(core::ptr::from_ref(slot) as u64 - crate::memory::pmap_base());
            Some((root, slot_phys, slot.generation(), slot.as_radix()))
        }
    };

    // Everything checked; nothing below can fail.
    match vspace {
        // SAFETY: The thread is not busy, so the scheduler is not
        // about to switch into it.
        None => unsafe { tcb.clear_vspace() },
        // SAFETY: `root_address` established a live PageTable.
        Some((root, slot, generation)) => unsafe { tcb.set_vspace(root, slot, generation) },
    }

    match cspace {
        // SAFETY: As above.
        None => unsafe { tcb.clear_cspace() },
        // SAFETY: The checks above established a live CNode.
        Some((root, slot, generation, radix)) => unsafe {
            tcb.set_cspace(root, slot, generation, radix)
        },
    }

    Ok(())
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
