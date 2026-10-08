// SPDX-License-Identifier: BSD-2-Clause
//! The capability object model.
//!
//! Every kernel resource is an object, and every access to an object goes
//! through a capability held in a CNode slot. The design is fixed by
//! ADR 017–023 and summarised in docs/object-model.md; this module is the
//! implementation.
//!
//! * [`cptr`] — resolving a `CPtr` to a slot (ADR 018)
//! * [`cnode`] — slots, the derivation list, copy/delete/revoke (ADR 019, 021)
//! * [`untyped`] — retype (ADR 020)
//! * [`boot`] — building the root Untypeds and the init CSpace (ADR 022)
//!
//! Borrowed from seL4; the three places we depart from it are named in
//! ADR 017 (one PageTable type), ADR 018 (no guards, fixed two levels)
//! and ADR 019 (bounded derivation depth).

pub mod boot;
pub mod cnode;
pub mod cptr;
pub mod ipc;
pub mod irq;
pub mod tcb;
pub mod untyped;
pub mod vspace;

use core::fmt;

use crate::memory::PhysAddr;

// ---- Object types ------------------------------------------------------

/// The object types (ADR 017).
///
/// `Null` is not one of them: it is the absence of a capability, and it
/// is here so that a slot is always a valid `CapType` rather than needing
/// a separate "occupied" flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum CapType {
    Null = 0,
    Untyped = 1,
    CNode = 2,
    Tcb = 3,
    Endpoint = 4,
    Notification = 5,
    Frame = 6,
    PageTable = 7,
    IrqHandler = 8,
    /// Authority to claim interrupt lines (M10, ADR 041).
    ///
    /// The one capability in the system with **no object behind it**.
    /// Every other type names memory carved from an Untyped; this one
    /// names the right to hand out lines, and a line is not memory.
    /// There is exactly one, created at boot and given to init, and it
    /// cannot be retyped into existence — which is why `fixed_size`
    /// returns `None` for it and `object_layout` refuses it outright.
    IrqControl = 9,
}

impl CapType {
    /// Decode from the packed representation in a slot.
    ///
    /// Returns `None` for a value no type uses, which can only happen if
    /// a slot was corrupted -- the kernel is the only writer.
    const fn from_bits(bits: u8) -> Option<CapType> {
        match bits {
            0 => Some(CapType::Null),
            1 => Some(CapType::Untyped),
            2 => Some(CapType::CNode),
            3 => Some(CapType::Tcb),
            4 => Some(CapType::Endpoint),
            5 => Some(CapType::Notification),
            6 => Some(CapType::Frame),
            7 => Some(CapType::PageTable),
            8 => Some(CapType::IrqHandler),
            9 => Some(CapType::IrqControl),
            _ => None,
        }
    }

    /// Human-readable name, for the acceptance tests and panics.
    pub const fn name(self) -> &'static str {
        match self {
            CapType::Null => "Null",
            CapType::Untyped => "Untyped",
            CapType::CNode => "CNode",
            CapType::Tcb => "TCB",
            CapType::Endpoint => "Endpoint",
            CapType::Notification => "Notification",
            CapType::Frame => "Frame",
            CapType::PageTable => "PageTable",
            CapType::IrqHandler => "IRQHandler",
            CapType::IrqControl => "IRQControl",
        }
    }

    /// Size in bytes of one object of this type, or `None` if the size is
    /// chosen by the caller (`Untyped` and `CNode`).
    pub const fn fixed_size(self) -> Option<usize> {
        match self {
            CapType::Tcb => Some(TCB_SIZE),
            CapType::Endpoint => Some(ENDPOINT_SIZE),
            CapType::Notification => Some(NOTIFICATION_SIZE),
            CapType::Frame => Some(FRAME_SIZE),
            CapType::PageTable => Some(PAGE_TABLE_SIZE),
            CapType::IrqHandler => Some(IRQ_HANDLER_SIZE),
            // `Untyped` and `CNode` have a size the caller chooses;
            // `Null` and `IrqControl` have no object at all, which is
            // a third thing this `None` is quietly standing for. The
            // distinction is enforced in `object_layout`, which
            // refuses both rather than asking here.
            CapType::Null | CapType::Untyped | CapType::CNode | CapType::IrqControl => None,
        }
    }

    /// Alignment in bytes. ADR 017's rule: the next power of two at or
    /// above the object's size, so that carving one out of an Untyped is
    /// a single mask (ADR 020).
    pub const fn alignment(self) -> usize {
        match self.fixed_size() {
            Some(size) => size.next_power_of_two(),
            // Variable-sized types are aligned to their actual size,
            // which retype computes from `size_bits`.
            None => 1,
        }
    }
}

impl fmt::Display for CapType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// ---- Object sizes ------------------------------------------------------
//
// ADR 017 gave ceilings estimated during the architect pass. These are the
// real figures, and the `const` assertions below make exceeding a ceiling
// a build failure rather than something noticed later.

/// A TCB: execution context plus the IPC buffer.
///
/// 2048 bytes = 1024 context + 1024 buffer. ADR 017 decided the buffer
/// lives inside the TCB rather than in a separate Frame, because the
/// alternative makes the kernel resolve a physical address on every IPC.
/// The consequence is that **IPC messages are at most 1 KiB**.
pub const TCB_SIZE: usize = 2048;

/// Bytes of the TCB given over to the IPC buffer.
pub const IPC_BUFFER_SIZE: usize = 1024;

/// A synchronous IPC rendezvous point.
pub const ENDPOINT_SIZE: usize = 32;

/// An asynchronous signal.
pub const NOTIFICATION_SIZE: usize = 64;

/// A 4 KiB page that can be mapped into a user address space.
pub const FRAME_SIZE: usize = 4096;

/// One level of a user page table.
pub const PAGE_TABLE_SIZE: usize = 4096;

/// Authority over one interrupt line.
pub const IRQ_HANDLER_SIZE: usize = 32;

/// One capability slot. ADR 019's derivation list lives inside it.
pub const SLOT_SIZE: usize = 32;

/// Smallest `size_bits` an Untyped may have: 2^4 = 16 bytes.
pub const UNTYPED_MIN_BITS: u8 = 4;

/// Largest `size_bits` an Untyped may have: 2^47, the user half.
pub const UNTYPED_MAX_BITS: u8 = 47;

/// Most objects one `retype` may produce (ADR 020).
pub const RETYPE_MAX_COUNT: usize = 256;

/// Deepest a capability may be derived (ADR 019).
pub const MAX_DERIVATION_DEPTH: u8 = 8;

// ---- Ceilings from ADR 017 --------------------------------------------
//
// Exceeding one of these is a build failure. The ADR's table is the
// contract; this is the enforcement.

const _: () = {
    assert!(
        TCB_SIZE <= 2048,
        "TCB exceeds the 2048-byte ceiling in ADR 017"
    );
    assert!(
        ENDPOINT_SIZE <= 32,
        "Endpoint exceeds the 32-byte ceiling in ADR 017"
    );
    assert!(
        NOTIFICATION_SIZE <= 64,
        "Notification exceeds the 64-byte ceiling in ADR 017"
    );
    assert!(
        IRQ_HANDLER_SIZE <= 32,
        "IRQHandler exceeds the 32-byte ceiling in ADR 017"
    );
    assert!(
        SLOT_SIZE == 32,
        "a slot must be exactly 32 bytes: ADR 019 budgets 8 for the object, \
         8 for type/rights/depth/generation and 16 for the derivation links"
    );
    assert!(
        IPC_BUFFER_SIZE < TCB_SIZE,
        "the IPC buffer must fit inside the TCB alongside the context"
    );
    assert!(
        MAX_DERIVATION_DEPTH <= 15,
        "the depth field is four bits wide"
    );
};

// ---- Rights ------------------------------------------------------------

/// The four rights bits (ADR 021).
///
/// Not `bitflags!` because the set is tiny and fixed, and because the
/// monotonic-narrowing check below reads better as a plain mask test.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rights(u8);

impl Rights {
    pub const NONE: Rights = Rights(0);
    /// Take data out of the object: receive, map readable, read state.
    pub const READ: Rights = Rights(1 << 0);
    /// Put data into the object: send, map writable, retype, start/stop.
    pub const WRITE: Rights = Rights(1 << 1);
    /// Carry *other capabilities* over this object.
    pub const GRANT: Rights = Rights(1 << 2);
    /// Carry only a reply capability.
    pub const GRANTREPLY: Rights = Rights(1 << 3);

    pub const ALL: Rights = Rights(0b1111);

    pub const fn from_bits(bits: u8) -> Rights {
        Rights(bits & 0b1111)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }

    pub const fn intersection(self, other: Rights) -> Rights {
        Rights(self.0 & other.0)
    }

    /// Whether `self` is a subset of `parent`.
    ///
    /// This is the monotonic-narrowing check of ADR 021, and it is the
    /// *whole* check: because every `copy` enforces it, the invariant
    /// holds along any derivation chain by induction. No ancestor walk is
    /// needed at runtime.
    pub const fn is_subset_of(self, parent: Rights) -> bool {
        self.0 & !parent.0 == 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Debug for Rights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("none");
        }
        let mut first = true;
        for (bit, name) in [
            (Rights::READ, "R"),
            (Rights::WRITE, "W"),
            (Rights::GRANT, "G"),
            (Rights::GRANTREPLY, "Gr"),
        ] {
            if self.contains(bit) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

// ---- Errors ------------------------------------------------------------

/// Why an operation on a capability failed.
///
/// Every variant is something a *caller* did wrong, not a kernel bug, so
/// all of them return rather than panicking: from M11 the caller is a user
/// component and a bad request must not stop the kernel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CapError {
    /// The reserved bits of a CPtr were not zero.
    InvalidCPtr,
    /// An index was past the end of its CNode.
    CPtrOutOfRange,
    /// The slot is empty.
    EmptySlot,
    /// The generation in the CPtr does not match the slot's. The slot was
    /// reused since this CPtr was handed out (ADR 018).
    StaleCPtr,
    /// A root CNode slot held something other than a CNode. ADR 018
    /// requires exactly two levels.
    RootSlotMustBeCNode,
    /// The capability does not support this operation.
    WrongType { found: CapType },
    /// A required rights bit is missing.
    InsufficientRights { needed: Rights, held: Rights },
    /// `copy` asked for rights the source does not have.
    RightsEscalation { asked: Rights, held: Rights },
    /// The destination slot is occupied.
    SlotNotEmpty,
    /// The result would exceed `MAX_DERIVATION_DEPTH`.
    DerivationTooDeep { depth: u8 },
    /// The Untyped does not have room. Carries the remaining bytes so the
    /// caller does not have to ask separately.
    NotEnoughMemory { remaining: u64, needed: u64 },
    /// `count` exceeded `RETYPE_MAX_COUNT`.
    TooManyObjects { count: usize },
    /// A device Untyped was asked for something other than a Frame
    /// (ADR 017).
    DeviceUntypedFrameOnly { asked: CapType },
    /// The operation would disturb a thread the scheduler is holding
    /// (ADR 026). Suspend it first.
    ///
    /// Carries the state by name rather than as a `ThreadState`: this
    /// enum is defined before `tcb` in the module graph, and the
    /// caller wants to print it rather than match on it.
    ThreadBusy { state: &'static str },

    // ---- M7: address spaces (ADR 027, 028) ----
    /// A mapping was requested at an address outside the user half.
    ///
    /// Distinct from the paging layer's `WrongHalf`, which is about the
    /// USER *flag* agreeing with the address. This one is about
    /// ownership: the kernel's top-level entries are shared by every
    /// address space, so a user mapping in the kernel half would edit
    /// tables that every other VSpace is walking (ADR 027, K1).
    NotUserAddress { address: u64 },
    /// The paging layer refused the address because the USER flag and
    /// the half disagree.
    ///
    /// Distinct from [`CapError::NotUserAddress`], and the distinction
    /// is not pedantry -- it was invisible until a mutation test made
    /// it visible. Deleting the capability layer's half check left
    /// `vspace-kernel-half` passing, because the paging layer refused
    /// the same addresses and nothing in the log said which guard had
    /// fired.
    ///
    /// They are checking different properties. `NotUserAddress` is
    /// about ownership: a user VSpace may only edit the user half,
    /// because the kernel half's tables are shared by every address
    /// space. This one is about flag consistency within a single
    /// request. Separate values are what make it possible to show both
    /// are present.
    FlagsWrongHalf { address: u64 },
    /// An intermediate page table is missing at this address.
    ///
    /// Not a failure so much as an instruction: map a PageTable at the
    /// level below first. The kernel will not spend a frame the caller
    /// did not offer (ADR 027).
    MissingPageTable { address: u64 },
    /// Something is already mapped there. Unmap it first -- overwriting
    /// would silently drop a frame somebody may still hold.
    AlreadyMapped { address: u64 },
    /// Nothing is mapped there.
    NotMapped { address: u64 },
    /// The VSpace is the one currently in CR3 (ADR 028, case A).
    ///
    /// Revoking it would hand the live page tables back to the Untyped
    /// while the CPU is still walking them. Move the running thread to
    /// another VSpace first; `SetSpace` always works, so the sequence
    /// always terminates.
    VSpaceInUse { root: u64 },

    // ---- M9: IPC (ADR 033-039) ----
    /// A message claimed more than 128 words (ADR 034).
    MessageTooLong { length: usize },
    /// A message claimed more than four capabilities (ADR 035).
    TooManyCaps { caps: usize },
    /// The endpoint was revoked while this thread waited on it, or the
    /// wait was cancelled some other way (ADR 039).
    ///
    /// Waiters are woken rather than the revoke being refused, and the
    /// reasoning differs from ADR 026's: the waiters may belong to
    /// other components, so refusing would let any of them make an
    /// endpoint un-revokable.
    EndpointDead,
    /// `Reply` found nobody waiting for one.
    NoReplyTarget,
    /// The thread that called is gone, or is no longer waiting
    /// (ADR 036).
    ///
    /// Distinct from [`CapError::NoReplyTarget`], and the distinction
    /// is the point: that one means "nobody ever called", this one
    /// means "somebody did and is no longer there". Collapsing them
    /// would make the generation check -- the thing that stops a reply
    /// reaching a *different* thread at the same address -- impossible
    /// to test for on its own.
    ReplyTargetGone,
    /// No kernel stack slot is free for a new user thread (M11).
    ///
    /// **Not [`CapError::NotEnoughMemory`]**, and the distinction is
    /// the one `vspace-user-exhaust` turns on. That error means "your
    /// Untyped is empty" — a component's own budget, which ADR 022
    /// says is the only thing a component can exhaust. This one means
    /// the kernel's KSTACK region is full, which is a *shared*
    /// resource and therefore a weaker guarantee.
    ///
    /// Collapsing them would make the budget test ambiguous: a run
    /// that exhausted the shared pool would report the same code as
    /// one that exhausted its own memory, and the claim "a component
    /// can only starve itself" could not be checked.
    NoKernelStack { limit: u64 },
    /// A Notification already has a waiter, or is already bound
    /// (ADR 038).
    NotificationBusy,
    /// The thread or the notification is already bound to something.
    AlreadyBound,

    // ---- M10: interrupts (ADR 041, 042) ----
    /// Another component already holds the handler for this line
    /// (ADR 041).
    ///
    /// Not a resource shortage but the **one-owner rule**: a second
    /// holder could not be given the interrupt without answering who
    /// acknowledges it, and could observe the first's device activity
    /// as a side channel. Sharing a line is a user-space problem,
    /// solved by a multiplexer that owns it.
    IrqAlreadyOwned { irq: u8 },

    /// Size, alignment or type combination is not valid.
    InvalidArgument,
}

impl fmt::Display for CapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapError::InvalidCPtr => write!(f, "InvalidCPtr (reserved bits not zero)"),
            CapError::CPtrOutOfRange => write!(f, "CPtrOutOfRange"),
            CapError::EmptySlot => write!(f, "EmptySlot"),
            CapError::StaleCPtr => write!(f, "StaleCPtr (the slot was reused)"),
            CapError::RootSlotMustBeCNode => {
                write!(f, "RootSlotMustBeCNode (ADR 018: exactly two levels)")
            }
            CapError::WrongType { found } => write!(f, "WrongType (found {})", found),
            CapError::InsufficientRights { needed, held } => {
                write!(f, "InsufficientRights (need {:?}, hold {:?})", needed, held)
            }
            CapError::RightsEscalation { asked, held } => {
                write!(f, "RightsEscalation (asked {:?}, source {:?})", asked, held)
            }
            CapError::SlotNotEmpty => write!(f, "SlotNotEmpty"),
            CapError::DerivationTooDeep { depth } => {
                write!(f, "DerivationTooDeep (would be {})", depth)
            }
            CapError::NoKernelStack { limit } => write!(
                f,
                "NoKernelStack (all {} user kernel-stack slots are in use; \
                 this is the kernel's region, not the caller's budget)",
                limit
            ),
            CapError::NotEnoughMemory { remaining, needed } => write!(
                f,
                "NotEnoughMemory (need {} bytes, {} remain)",
                needed, remaining
            ),
            CapError::TooManyObjects { count } => {
                write!(f, "TooManyObjects ({} > {})", count, RETYPE_MAX_COUNT)
            }
            CapError::DeviceUntypedFrameOnly { asked } => write!(
                f,
                "DeviceUntypedFrameOnly (asked for {}, device memory may only \
                 become a Frame)",
                asked
            ),
            CapError::ThreadBusy { state } => write!(
                f,
                "ThreadBusy (the thread is {}; suspend it first -- ADR 026)",
                state
            ),
            CapError::NotUserAddress { address } => write!(
                f,
                "NotUserAddress ({:#018x} is not in the user half; the \
                 kernel's tables are shared by every VSpace -- ADR 027)",
                address
            ),
            CapError::FlagsWrongHalf { address } => write!(
                f,
                "FlagsWrongHalf ({:#018x}: the USER flag and the address \
                 half disagree)",
                address
            ),
            CapError::MissingPageTable { address } => write!(
                f,
                "MissingPageTable (no intermediate table covers {:#018x}; \
                 map one first -- the kernel does not allocate)",
                address
            ),
            CapError::AlreadyMapped { address } => {
                write!(f, "AlreadyMapped ({:#018x} is occupied)", address)
            }
            CapError::NotMapped { address } => {
                write!(f, "NotMapped ({:#018x})", address)
            }
            CapError::VSpaceInUse { root } => write!(
                f,
                "VSpaceInUse (root {:#018x} is in CR3 right now; move the \
                 running thread elsewhere first -- ADR 028)",
                root
            ),
            CapError::MessageTooLong { length } => write!(
                f,
                "MessageTooLong ({} words; the IPC buffer holds {} -- \
                 ADR 034)",
                length,
                ipc::MAX_MESSAGE_WORDS
            ),
            CapError::TooManyCaps { caps } => write!(
                f,
                "TooManyCaps ({}; at most {} travel in one message -- \
                 ADR 035)",
                caps,
                ipc::MAX_TRANSFER_CAPS
            ),
            CapError::EndpointDead => write!(
                f,
                "EndpointDead (the endpoint was revoked while this thread \
                 waited on it -- ADR 039)"
            ),
            CapError::NoReplyTarget => {
                write!(f, "NoReplyTarget (nobody is waiting for a reply)")
            }
            CapError::ReplyTargetGone => write!(
                f,
                "ReplyTargetGone (the caller is gone or no longer waiting; \
                 the generation check refused a reply to whatever occupies \
                 its memory now -- ADR 036)"
            ),
            CapError::NotificationBusy => write!(
                f,
                "NotificationBusy (a Notification takes one waiter -- \
                 ADR 038)"
            ),
            CapError::AlreadyBound => {
                write!(f, "AlreadyBound (ADR 038)")
            }
            CapError::IrqAlreadyOwned { irq } => write!(
                f,
                "IrqAlreadyOwned (IRQ {} already has a handler; a line \
                 has exactly one owner, and sharing is a user-space \
                 multiplexer's job -- ADR 041)",
                irq
            ),
            CapError::InvalidArgument => write!(f, "InvalidArgument"),
        }
    }
}

// ---- Object addresses --------------------------------------------------

/// Where an object lives, and how the kernel reaches it.
///
/// Objects live in physical memory carved out of an Untyped. The kernel
/// touches them through PMAP, so a `PhysAddr` plus the window is all that
/// is needed -- no per-object mapping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ObjectRef(pub PhysAddr);

impl ObjectRef {
    pub const NULL: ObjectRef = ObjectRef(PhysAddr(0));

    pub const fn is_null(self) -> bool {
        self.0 .0 == 0
    }
}

impl fmt::Display for ObjectRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_null() {
            f.write_str("<null>")
        } else {
            write!(f, "{}", self.0)
        }
    }
}

/// Report the measured object sizes.
///
/// ADR 017's table gives ceilings estimated during the architect pass;
/// these are the figures the build actually produced. Printing them means
/// the table can be checked against reality rather than trusted.
pub fn report_sizes() {
    use crate::println;

    println!(
        "[cap] slot {} B; Untyped 2^{}..2^{}; CNode {} B/slot",
        SLOT_SIZE, UNTYPED_MIN_BITS, UNTYPED_MAX_BITS, SLOT_SIZE
    );
    println!(
        "[cap] TCB {} B ({} context + {} ipc), Endpoint {} B, \
         Notification {} B",
        TCB_SIZE,
        TCB_SIZE - IPC_BUFFER_SIZE,
        IPC_BUFFER_SIZE,
        ENDPOINT_SIZE,
        NOTIFICATION_SIZE
    );
    println!(
        "[cap] Frame {} B, PageTable {} B, IRQHandler {} B",
        FRAME_SIZE, PAGE_TABLE_SIZE, IRQ_HANDLER_SIZE
    );
    println!(
        "[cap] limits: retype count <= {}, derivation depth <= {}",
        RETYPE_MAX_COUNT, MAX_DERIVATION_DEPTH
    );
}
