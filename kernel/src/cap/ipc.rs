// SPDX-License-Identifier: MPL-2.0
//! Synchronous IPC: endpoints, notifications, and the rendezvous.
//!
//! This is M9, and it is the milestone the object model was built for.
//! Everything before it produced things a component can *hold*; this is
//! the first thing two components can *do together*.
//!
//! The design is fixed by ADR 033–040 and described end to end in
//! docs/ipc.md. Four decisions shape every line here:
//!
//! * **Rendezvous, no buffering** (ADR 033). A message is never "in
//!   flight": either two threads meet and the transfer happens, or one
//!   of them blocks. A buffer would be a resource, a resource needs an
//!   owner, and ADR 022 forbids the kernel allocating for itself.
//! * **Three words in registers, the rest in the TCB** (ADR 034). Three
//!   is not a choice but a count: M8's ABI leaves four registers and
//!   `Send` spends one on the endpoint's CPtr.
//! * **Capabilities land in slots the *receiver* opened** (ADR 035). Not
//!   a safe default — a structural impossibility. A sender has no way to
//!   name a destination, so it cannot force authority on anyone.
//! * **No fastpath** (ADR 040). One path, measured rather than guessed.
//!
//! # Why the queue is doubly linked
//!
//! M6's ready queue is singly linked and removal is O(queue). The same
//! choice here would be wrong, and the difference is not performance but
//! *who controls the length*: a component with enough Untyped can park
//! hundreds of threads on one endpoint, and then every cancellation
//! walks that list with interrupts disabled. ADR 033 was corrected
//! before implementation for this reason.
//!
//! # Scope: what is live, and what is written but not yet reachable
//!
//! M9 is being landed in three parts, because one attempt at all
//! eleven acceptance criteria produced code that did not compile and
//! was never run — which is worth saying plainly, since the whole
//! discipline here is that an untested claim is not a claim.
//!
//! | Part | Contents | State |
//! |---|---|---|
//! | **M9a** | Endpoint, `Send`, `Recv`, the wait queue, cancellation | **live** |
//! | **M9b** | `Call`, `Reply`, `ReplyRecv`, badges, reply validation | **live** |
//! | **M9c** | capability transfer, Notification, bound waiting | **live** |
//!
//! Nothing is left behind `#[allow(dead_code)]`. Across the three
//! parts that attribute did one job: keep written-but-unrun code
//! under the compiler's eye while making sure **nothing claimed to
//! be tested that was not.**
//!
//! Splitting the milestone paid for itself three times over. Turning
//! M9b on found **four** defects that M9a's tests could not reach,
//! every one of them on the first run of the scenario that reached
//! it:
//!
//! * the global delivery slot was unsound — see [`Incoming`] in
//!   `tcb.rs`; the invariant it asserted was never true, only
//!   unreachable;
//! * `Call`'s immediate-rendezvous path never read its outcome, so a
//!   client whose server died got an empty **success**;
//! * `send_inner`'s rendezvous branch never recorded an outcome for
//!   the receiver it woke — invisible while `recv` inferred the
//!   reason from the message's contents rather than reading it;
//! * a queued `Call` wakes **once**, and the outcome was read twice.
//!
//! Three of those are in code M9a shipped and tested. Written all at
//! once, they would have arrived together, in a build that had never
//! run.
//!
//! M9c then found three more — and found them by **reading the ADRs
//! against the code before running anything**, which is cheaper
//! still:
//!
//! * `release_ipc_references` had no `Notification` arm, hidden by a
//!   `_ => {}`. A thread in `Wait` when its Notification was revoked
//!   would have waited forever, and the scenario for it would not
//!   have failed — it would have **hung**. Making that match
//!   exhaustive also surfaced `IrqHandler`, which M12 will owe an
//!   arm;
//! * `wait`'s outcome match had a catch-all that swallowed
//!   `Pending`, which is the one value M9b's panic exists to catch;
//! * `recv`'s `pending_signal` was a hardcoded `false`, so bound
//!   waiting — and with it the whole alternation branch — could
//!   never run.
//!
//! One M9c mutation also **survived**, which is recorded rather than
//! quietly dropped: see `ipctest::check_no_starvation` and ADR 038.
//!
//! [`Incoming`]: super::tcb::Incoming

use core::sync::atomic::{AtomicU64, Ordering};

use super::cnode::{self, Slot};
use super::tcb::{self, Tcb, ThreadState};
use super::{CapError, CapType, ObjectRef, Rights};
use crate::memory::PhysAddr;

// ---- The message tag (ADR 034) -----------------------------------------

/// How many message words travel in registers.
///
/// Three, and the number is a count rather than a judgement. `syscall`
/// destroys RCX and R11 (ADR 030), which leaves RDX, R10, R8 and R9;
/// `Send` spends RDX on the endpoint's CPtr. ADR 034 then refused to let
/// the receive direction carry four while the send direction carries
/// three: "how many words are in registers" must have one answer.
pub const REGISTER_WORDS: usize = 3;

/// Longest message, in 64-bit words.
///
/// 128 words = 1024 bytes = the IPC buffer ADR 017 put inside the TCB.
pub const MAX_MESSAGE_WORDS: usize = 128;

/// Most capabilities one message may carry (ADR 035).
///
/// Four, taken from M13's worst case: starting a component hands it a
/// CSpace, a VSpace, an Endpoint and a Notification.
pub const MAX_TRANSFER_CAPS: usize = 4;

const _: () = {
    assert!(
        MAX_MESSAGE_WORDS * 8 == super::IPC_BUFFER_SIZE,
        "the message length and the TCB's IPC buffer disagree; one of \
         ADR 017 and ADR 034 has moved without the other"
    );
    assert!(
        REGISTER_WORDS < MAX_MESSAGE_WORDS,
        "every message word would be in a register"
    );
};

/// The message tag: what travels in RDI.
///
/// ```text
///  63                    32 31    21 20 19   16 15            4 3    0
/// +------------------------+--------+--+-------+----------------+------+
/// |        label           |reserved|N | caps  |     length     | rsvd |
/// |        32 bit          | 11 bit |1 | 4 bit |     12 bit     | 4 bit|
/// +------------------------+--------+--+-------+----------------+------+
/// ```
///
/// `label` is the application's own opcode and **the kernel never reads
/// it** — that is ADR 011's mechanism/policy split applied to the one
/// field a protocol designer most wants the kernel to understand.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MessageTag(u64);

/// Bit 20: the kernel sets this when a `Recv` was woken by a bound
/// Notification rather than by a rendezvous (ADR 038).
///
/// A sender writing it would make its message look like a signal, and
/// the receiver would read a badge as a mask. So the send path checks it
/// is clear, which is the third of ADR 034's five tag validations.
const TAG_IS_NOTIFICATION: u64 = 1 << 20;

const TAG_RSVD_LOW: u64 = 0xF;
const TAG_LENGTH_SHIFT: u32 = 4;
const TAG_LENGTH_MASK: u64 = 0xFFF;
const TAG_CAPS_SHIFT: u32 = 16;
const TAG_CAPS_MASK: u64 = 0xF;
const TAG_RSVD_HIGH_SHIFT: u32 = 21;
const TAG_RSVD_HIGH_MASK: u64 = 0x7FF;
const TAG_LABEL_SHIFT: u32 = 32;

impl MessageTag {
    pub const EMPTY: MessageTag = MessageTag(0);

    pub const fn from_bits(bits: u64) -> MessageTag {
        MessageTag(bits)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Build a tag. Used by the kernel-side tests, which play the part
    /// of user code until M13.
    pub const fn new(label: u32, length: usize, caps: usize) -> MessageTag {
        MessageTag(
            ((label as u64) << TAG_LABEL_SHIFT)
                | (((caps as u64) & TAG_CAPS_MASK) << TAG_CAPS_SHIFT)
                | (((length as u64) & TAG_LENGTH_MASK) << TAG_LENGTH_SHIFT),
        )
    }

    /// The application's opcode. **Never interpreted here.**
    pub const fn label(self) -> u32 {
        (self.0 >> TAG_LABEL_SHIFT) as u32
    }

    pub const fn length(self) -> usize {
        ((self.0 >> TAG_LENGTH_SHIFT) & TAG_LENGTH_MASK) as usize
    }

    pub const fn caps(self) -> usize {
        ((self.0 >> TAG_CAPS_SHIFT) & TAG_CAPS_MASK) as usize
    }

    pub const fn is_notification(self) -> bool {
        self.0 & TAG_IS_NOTIFICATION != 0
    }

    /// Replace the `caps` count with what was actually transferred.
    ///
    /// The receiver must not be told four capabilities arrived when one
    /// was dropped: it would go looking in slots that are still empty.
    /// ADR 034 makes this the kernel's responsibility rather than the
    /// sender's, because the sender cannot know (ADR 035 keeps the
    /// outcome from it deliberately).
    pub const fn with_caps(self, caps: usize) -> MessageTag {
        MessageTag(
            (self.0 & !(TAG_CAPS_MASK << TAG_CAPS_SHIFT))
                | (((caps as u64) & TAG_CAPS_MASK) << TAG_CAPS_SHIFT),
        )
    }

    /// Mark this delivery as coming from a bound Notification.
    pub const fn as_notification(self) -> MessageTag {
        MessageTag(self.0 | TAG_IS_NOTIFICATION)
    }

    /// Check every field a sender controls.
    ///
    /// Five checks, and ADR 023's K5 requires a test for each: a check
    /// that nothing reaches is indistinguishable from its own absence.
    /// `ipc-test` scenario 3 and the tag cases in `ipctest::run` drive
    /// all five.
    pub fn validate(self) -> Result<(), CapError> {
        if self.0 & TAG_RSVD_LOW != 0 {
            return Err(CapError::InvalidArgument);
        }
        if (self.0 >> TAG_RSVD_HIGH_SHIFT) & TAG_RSVD_HIGH_MASK != 0 {
            return Err(CapError::InvalidArgument);
        }
        // Only the kernel may say "this came from a Notification". A
        // sender that could set it would make a badge look like a
        // signal mask.
        if self.is_notification() {
            return Err(CapError::InvalidArgument);
        }
        if self.length() > MAX_MESSAGE_WORDS {
            return Err(CapError::MessageTooLong {
                length: self.length(),
            });
        }
        if self.caps() > MAX_TRANSFER_CAPS {
            return Err(CapError::TooManyCaps { caps: self.caps() });
        }
        Ok(())
    }
}

impl core::fmt::Debug for MessageTag {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "tag(label={:#x}, len={}, caps={}{})",
            self.label(),
            self.length(),
            self.caps(),
            if self.is_notification() {
                ", notification"
            } else {
                ""
            }
        )
    }
}

// ---- Endpoint (ADR 033) ------------------------------------------------

/// What an endpoint's queue currently holds.
///
/// One queue, not two, and that is what makes 32 bytes enough: senders
/// and receivers can never wait at the same time, because the arrival of
/// one releases the other immediately (ADR 033).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum EndpointState {
    /// Nobody waiting.
    Idle = 0,
    /// The queue holds senders.
    SendersWaiting = 1,
    /// The queue holds receivers.
    ReceiversWaiting = 2,
}

impl EndpointState {
    const fn from_bits(bits: u8) -> Option<EndpointState> {
        match bits {
            0 => Some(EndpointState::Idle),
            1 => Some(EndpointState::SendersWaiting),
            2 => Some(EndpointState::ReceiversWaiting),
            _ => None,
        }
    }

    #[allow(dead_code)] // used by the M9b/M9c reports
    pub const fn name(self) -> &'static str {
        match self {
            EndpointState::Idle => "Idle",
            EndpointState::SendersWaiting => "SendersWaiting",
            EndpointState::ReceiversWaiting => "ReceiversWaiting",
        }
    }
}

/// A synchronous rendezvous point. 32 bytes (ADR 017).
///
/// Holds no message. That is the whole of ADR 033 in one sentence: the
/// message lives in the sending thread's TCB, so an endpoint's size does
/// not grow with what passes through it.
#[repr(C, align(32))]
pub struct Endpoint {
    /// An [`EndpointState`].
    state: u8,
    _pad: [u8; 7],
    /// First waiting TCB, by physical address, or zero.
    queue_head: u64,
    /// Last waiting TCB, so appending is O(1).
    queue_tail: u64,
    /// Reserved. Keeps the object at exactly 32 bytes so that a field
    /// added later is a visible decision rather than a silent growth.
    _reserved: u64,
}

const _: () = {
    assert!(
        core::mem::size_of::<Endpoint>() == super::ENDPOINT_SIZE,
        "Endpoint is not ENDPOINT_SIZE bytes; ADR 017's ceiling and \
         retype's arithmetic disagree"
    );
    assert!(
        core::mem::align_of::<Endpoint>() == super::ENDPOINT_SIZE,
        "an Endpoint's alignment must equal its size (ADR 017)"
    );
};

impl Endpoint {
    pub fn state(&self) -> EndpointState {
        EndpointState::from_bits(self.state).unwrap_or_else(|| {
            panic!(
                "endpoint holds state {}, which is not a valid state; the \
                 object has been corrupted",
                self.state
            )
        })
    }

    fn set_state(&mut self, state: EndpointState) {
        self.state = state as u8;
    }

    pub fn is_idle(&self) -> bool {
        self.queue_head == 0
    }

    #[allow(dead_code)] // M9b
    pub fn queue_head(&self) -> u64 {
        self.queue_head
    }

    /// How many threads are waiting. Diagnostic only — O(queue).
    ///
    /// Not used by any operation: every one of them is O(1), which is
    /// ADR 033's corrected decision and what `ipc-cancel-cost` measures.
    pub fn queue_length(&self) -> usize {
        let mut count = 0usize;
        let mut cursor = self.queue_head;
        while cursor != 0 {
            count += 1;
            // SAFETY: A non-zero link is the address of a live TCB; the
            // queue operations below maintain that.
            let tcb = unsafe { tcb::tcb_at(ObjectRef(PhysAddr(cursor))) };
            cursor = tcb.ipc_next();
            assert!(
                count <= MAX_QUEUE_LENGTH,
                "an endpoint's wait queue is longer than any plausible \
                 thread count; it has a cycle, which means a thread was \
                 queued twice"
            );
        }
        count
    }

    /// Append a thread. O(1).
    ///
    /// # Safety
    ///
    /// `thread` must be live and not already in any IPC queue, and
    /// interrupts must be disabled.
    unsafe fn enqueue(&mut self, thread: &mut Tcb, state: EndpointState) {
        debug_assert!(!crate::sched::interrupts_enabled());
        debug_assert!(
            thread.ipc_next() == 0 && thread.ipc_prev() == 0,
            "the thread is already linked into an IPC queue"
        );
        debug_assert!(
            self.is_idle() || self.state() == state,
            "an endpoint cannot hold senders and receivers at once \
             (ADR 033)"
        );

        // SAFETY: Every TCB lives in the physical window.
        let phys = unsafe { tcb::tcb_phys(thread) }.0;

        thread.set_ipc_next(0);
        thread.set_ipc_prev(self.queue_tail);

        if self.queue_tail == 0 {
            self.queue_head = phys;
        } else {
            // SAFETY: A non-zero tail names a live TCB.
            let tail = unsafe { tcb::tcb_at(ObjectRef(PhysAddr(self.queue_tail))) };
            tail.set_ipc_next(phys);
        }
        self.queue_tail = phys;
        self.set_state(state);
    }

    /// Take the thread at the front. O(1).
    ///
    /// # Safety
    ///
    /// Interrupts must be disabled.
    unsafe fn dequeue(&mut self) -> Option<&'static mut Tcb> {
        debug_assert!(!crate::sched::interrupts_enabled());

        if self.queue_head == 0 {
            return None;
        }
        // SAFETY: A non-zero head names a live TCB.
        let thread = unsafe { tcb::tcb_at(ObjectRef(PhysAddr(self.queue_head))) };
        let next = thread.ipc_next();

        self.queue_head = next;
        if next == 0 {
            self.queue_tail = 0;
            self.set_state(EndpointState::Idle);
        } else {
            // SAFETY: A non-zero link names a live TCB.
            let next_tcb = unsafe { tcb::tcb_at(ObjectRef(PhysAddr(next))) };
            next_tcb.set_ipc_prev(0);
        }

        thread.set_ipc_next(0);
        thread.set_ipc_prev(0);
        Some(thread)
    }

    /// Remove one specific thread. **O(1)** — ADR 033's corrected
    /// decision, and the reason the queue is doubly linked.
    ///
    /// M6's `remove_from_queue` is O(length) and that was defensible:
    /// the ready queue's length is the system's thread count. Here the
    /// length is *an adversary's* thread count, so the same cost would
    /// be a denial of service with interrupts disabled.
    ///
    /// # Safety
    ///
    /// `thread` must be live and interrupts disabled.
    unsafe fn remove(&mut self, thread: &mut Tcb) -> bool {
        debug_assert!(!crate::sched::interrupts_enabled());

        // SAFETY: Every TCB lives in the physical window.
        let phys = unsafe { tcb::tcb_phys(thread) }.0;
        // Both neighbours come from the thread's own links. Replacing
        // this pair with a walk from `queue_head` -- the singly linked
        // queue ADR 033 first specified -- was M9a's K5 mutation: the
        // measurement went from 42/42 to 294 against 2478, a factor of
        // eight, and `ipc-cancel-cost` named it a linear walk.
        let prev = thread.ipc_prev();
        let next = thread.ipc_next();

        // Not queued here: both links zero *and* not the only element.
        if prev == 0 && next == 0 && self.queue_head != phys {
            return false;
        }

        if prev == 0 {
            self.queue_head = next;
        } else {
            // SAFETY: A non-zero link names a live TCB.
            let prev_tcb = unsafe { tcb::tcb_at(ObjectRef(PhysAddr(prev))) };
            prev_tcb.set_ipc_next(next);
        }

        if next == 0 {
            self.queue_tail = prev;
        } else {
            // SAFETY: As above.
            let next_tcb = unsafe { tcb::tcb_at(ObjectRef(PhysAddr(next))) };
            next_tcb.set_ipc_prev(prev);
        }

        thread.set_ipc_next(0);
        thread.set_ipc_prev(0);

        if self.queue_head == 0 {
            self.set_state(EndpointState::Idle);
        }
        true
    }
}

/// Longest an IPC wait queue may get before the kernel calls it a bug.
///
/// Not a performance bound — every queue operation is O(1). This is a
/// consistency check: a list this long means it has a cycle, i.e. a
/// thread was queued twice. Same reasoning and same figure as M6's
/// `ready_count`.
pub const MAX_QUEUE_LENGTH: usize = 1024;

// ---- Hooks for the cancellation measurement ----------------------------
//
// `ipc-cancel-cost` needs to put threads on an endpoint's queue and
// take one off again, in order to time the second operation at two
// queue lengths. It cannot do that through `send` and `recv`: those
// block the *calling* thread, and there is one of those.
//
// So the queue methods are exposed to the test through these three
// functions rather than being made public outright. The distinction
// matters: `enqueue` and `remove` have preconditions that only the IPC
// paths can state (the thread is blocked, the state field agrees with
// what the queue holds), and a public method would invite a caller
// who does not know them.
//
// What is measured is exactly the code the real paths run --
// `Endpoint::remove` is the same function `cancel_ipc` calls.

/// Put a thread on an endpoint's queue, for the measurement only.
///
/// # Safety
///
/// Interrupts must be disabled, and `thread` must be live and on no
/// queue.
pub unsafe fn queue_for_measurement(endpoint: &mut Endpoint, thread: &mut Tcb) {
    // SAFETY: Caller's guarantee.
    unsafe {
        thread.set_state(ThreadState::BlockedOnEndpoint);
        endpoint.enqueue(thread, EndpointState::ReceiversWaiting);
    }
}

/// Take a specific thread off an endpoint's queue, for the
/// measurement only.
///
/// **This is the operation `ipc-cancel-cost` times.** It is
/// `Endpoint::remove` and nothing else — the same code `cancel_ipc`
/// runs, so the figure means what the scenario says it means.
///
/// # Safety
///
/// Interrupts must be disabled and `thread` must be queued on this
/// endpoint.
pub unsafe fn unqueue_for_measurement(endpoint: &mut Endpoint, thread: &mut Tcb) -> bool {
    // SAFETY: Caller's guarantee.
    unsafe { endpoint.remove(thread) }
}

/// Empty an endpoint's queue without waking anything.
///
/// For the measurement's second run, which needs the endpoint back in
/// its initial state. Not a path any real operation takes: the
/// threads here never ran and have nothing to be told.
///
/// # Safety
///
/// Interrupts must be disabled and every queued thread must be one
/// the caller put there.
pub unsafe fn drain_for_measurement(endpoint: &mut Endpoint) -> usize {
    let mut drained = 0usize;
    // SAFETY: Caller's guarantee; the queue shrinks each iteration.
    while let Some(thread) = unsafe { endpoint.dequeue() } {
        // SAFETY: The thread is off the queue now.
        unsafe {
            thread.set_state(ThreadState::Inactive);
        }
        thread.set_blocked_on(0);
        drained += 1;
        assert!(
            drained <= MAX_QUEUE_LENGTH,
            "draining an endpoint did not terminate; the queue has a cycle"
        );
    }
    drained
}

/// Borrow the Endpoint a capability names.
///
/// # Safety
///
/// `slot` must hold a live Endpoint capability and the caller must not
/// alias it.
pub unsafe fn endpoint_from_slot(slot: &Slot) -> Result<&'static mut Endpoint, CapError> {
    if slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if slot.cap_type() != CapType::Endpoint {
        return Err(CapError::WrongType {
            found: slot.cap_type(),
        });
    }
    // SAFETY: The capability names an Endpoint-sized, Endpoint-aligned
    // object that retype zeroed; PMAP maps all RAM.
    Ok(unsafe { endpoint_at(slot.object()) })
}

/// # Safety
///
/// `object` must name a live Endpoint, unaliased.
pub unsafe fn endpoint_at(object: ObjectRef) -> &'static mut Endpoint {
    debug_assert!(!object.is_null(), "null Endpoint address");
    debug_assert!(
        object.0 .0.is_multiple_of(super::ENDPOINT_SIZE as u64),
        "Endpoint at {} is not {}-byte aligned",
        object,
        super::ENDPOINT_SIZE
    );
    // SAFETY: Caller's guarantee; PMAP maps all RAM writably.
    unsafe { &mut *crate::memory::phys_to_virt(object.0).as_ptr::<Endpoint>() }
}

// ---- Notification (ADR 038) --------------------------------------------

/// An asynchronous signal: a 32-bit mask and at most one waiter.
///
/// A mask rather than a counter, and that is a decision with a
/// consequence: "three signals arrived" is not recoverable, "they came
/// from these three sources" is. A counter would overflow, and what to
/// do then is a question with no good answer (ADR 038).
///
/// Retyping one has worked since M5 — the object model has had the
/// type all along — but nothing signalled or waited on one until
/// M9c.
#[repr(C, align(64))]
pub struct Notification {
    /// Accumulated signal bits. Badges are OR'd in (ADR 037).
    signalled: u32,
    _pad: [u8; 4],
    /// The waiting TCB's physical address, or zero. At most one.
    waiter: u64,
    /// The TCB bound to this notification (ADR 038), or zero.
    bound_tcb: u64,
    /// Reserved; keeps the object at 64 bytes.
    _reserved: [u64; 5],
}

const _: () = {
    assert!(
        core::mem::size_of::<Notification>() == super::NOTIFICATION_SIZE,
        "Notification is not NOTIFICATION_SIZE bytes"
    );
    assert!(
        core::mem::align_of::<Notification>() == super::NOTIFICATION_SIZE,
        "a Notification's alignment must equal its size (ADR 017)"
    );
};

impl Notification {
    pub fn signalled(&self) -> u32 {
        self.signalled
    }

    pub fn waiter(&self) -> u64 {
        self.waiter
    }

    pub fn bound_tcb(&self) -> u64 {
        self.bound_tcb
    }

    pub fn has_pending(&self) -> bool {
        self.signalled != 0
    }

    /// Take the accumulated mask and clear it.
    fn take(&mut self) -> u32 {
        let mask = self.signalled;
        self.signalled = 0;
        mask
    }

    fn set_waiter(&mut self, tcb: u64) {
        self.waiter = tcb;
    }

    fn clear_waiter(&mut self) {
        self.waiter = 0;
    }

    fn set_bound_tcb(&mut self, tcb: u64) {
        self.bound_tcb = tcb;
    }
}

/// # Safety
///
/// `slot` must hold a live Notification capability, unaliased.
pub unsafe fn notification_from_slot(slot: &Slot) -> Result<&'static mut Notification, CapError> {
    if slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if slot.cap_type() != CapType::Notification {
        return Err(CapError::WrongType {
            found: slot.cap_type(),
        });
    }
    // SAFETY: The capability names a Notification-sized, aligned object.
    Ok(unsafe { notification_at(slot.object()) })
}

/// # Safety
///
/// `object` must name a live Notification, unaliased.
pub unsafe fn notification_at(object: ObjectRef) -> &'static mut Notification {
    debug_assert!(!object.is_null(), "null Notification address");
    debug_assert!(
        object.0 .0.is_multiple_of(super::NOTIFICATION_SIZE as u64),
        "Notification at {} is not {}-byte aligned",
        object,
        super::NOTIFICATION_SIZE
    );
    // SAFETY: Caller's guarantee; PMAP maps all RAM writably.
    unsafe { &mut *crate::memory::phys_to_virt(object.0).as_ptr::<Notification>() }
}

// ---- Counters, for the acceptance test ---------------------------------

/// Completed rendezvous.
static RENDEZVOUS_COUNT: AtomicU64 = AtomicU64::new(0);
/// Times a sender arrived first and had to block.
static SENDER_BLOCKED: AtomicU64 = AtomicU64::new(0);
/// Times a receiver arrived first and had to block.
static RECEIVER_BLOCKED: AtomicU64 = AtomicU64::new(0);
/// Capabilities actually transferred.
static CAPS_TRANSFERRED: AtomicU64 = AtomicU64::new(0);
/// Capabilities silently dropped by one of ADR 035's five checks.
///
/// Counted separately from the transfers because "four asked for, two
/// arrived" is the only way a dropped capability is visible at all —
/// ADR 035 deliberately returns no error to either side.
static CAPS_DROPPED: AtomicU64 = AtomicU64::new(0);
/// Signals delivered through a Notification.
static SIGNALS_DELIVERED: AtomicU64 = AtomicU64::new(0);
/// Deliveries that went to a *bound* notification during a `Recv`.
static BOUND_DELIVERIES: AtomicU64 = AtomicU64::new(0);
/// Times the alternation bit chose the endpoint over a ready signal.
///
/// This is the evidence for ADR 038's starvation guarantee: a
/// "notification first" implementation would leave it at zero while
/// everything else still passed.
static ALTERNATION_CHOSE_ENDPOINT: AtomicU64 = AtomicU64::new(0);
/// Times the alternation bit chose the notification over a waiting
/// sender.
///
/// The other half of the evidence. `ALTERNATION_CHOSE_ENDPOINT` alone
/// would be satisfied by an "endpoint first" implementation, which
/// starves the notification instead — the same defect in the other
/// direction (ADR 038 rejected both). Requiring **both** counters to
/// move is what shows the choice alternates rather than being fixed.
static ALTERNATION_CHOSE_NOTIFICATION: AtomicU64 = AtomicU64::new(0);

/// Whether `recv` should narrate each alternation decision.
///
/// Off by default and turned on by `ipc-starvation` alone. The
/// scenario needs to see the *state at the decision*, not only which
/// way it went — and three attempts at it were spent inferring that
/// state from outcomes, which produced two wrong diagnoses.
static ALTERNATION_TRACE: AtomicU64 = AtomicU64::new(0);

/// Turn the alternation trace on or off.
pub fn set_alternation_trace(on: bool) {
    ALTERNATION_TRACE.store(u64::from(on), Ordering::Release);
}
/// Cancellations: a queued thread removed by suspend or revoke.
static CANCELLATIONS: AtomicU64 = AtomicU64::new(0);
/// Replies that found their target gone (ADR 036).
static STALE_REPLIES: AtomicU64 = AtomicU64::new(0);
/// Calls made: a Send that also waits for an answer.
static CALLS: AtomicU64 = AtomicU64::new(0);
/// Replies delivered to a caller that was still waiting.
static REPLIES_SENT: AtomicU64 = AtomicU64::new(0);
/// `ReplyRecv` invocations — one syscall doing two operations.
///
/// Counted so the acceptance test can show the saving is real rather
/// than claimed: a server loop using `ReplyRecv` makes exactly one
/// kernel entry per request, and the figure here against the syscall
/// count is what demonstrates it (ADR 036).
static REPLY_RECVS: AtomicU64 = AtomicU64::new(0);

pub fn rendezvous_count() -> u64 {
    RENDEZVOUS_COUNT.load(Ordering::Acquire)
}

pub fn sender_blocked_count() -> u64 {
    SENDER_BLOCKED.load(Ordering::Acquire)
}

pub fn receiver_blocked_count() -> u64 {
    RECEIVER_BLOCKED.load(Ordering::Acquire)
}

pub fn caps_transferred() -> u64 {
    CAPS_TRANSFERRED.load(Ordering::Acquire)
}

pub fn caps_dropped() -> u64 {
    CAPS_DROPPED.load(Ordering::Acquire)
}

pub fn signals_delivered() -> u64 {
    SIGNALS_DELIVERED.load(Ordering::Acquire)
}

pub fn bound_deliveries() -> u64 {
    BOUND_DELIVERIES.load(Ordering::Acquire)
}

pub fn alternation_chose_endpoint() -> u64 {
    ALTERNATION_CHOSE_ENDPOINT.load(Ordering::Acquire)
}

pub fn alternation_chose_notification() -> u64 {
    ALTERNATION_CHOSE_NOTIFICATION.load(Ordering::Acquire)
}

pub fn cancellations() -> u64 {
    CANCELLATIONS.load(Ordering::Acquire)
}

pub fn stale_replies() -> u64 {
    STALE_REPLIES.load(Ordering::Acquire)
}

pub fn calls() -> u64 {
    CALLS.load(Ordering::Acquire)
}

pub fn replies_sent() -> u64 {
    REPLIES_SENT.load(Ordering::Acquire)
}

pub fn reply_recvs() -> u64 {
    REPLY_RECVS.load(Ordering::Acquire)
}

/// Report what the IPC path has done.
pub fn report() {
    use crate::println;

    println!(
        "[ipc] {} rendezvous ({} senders blocked, {} receivers blocked)",
        rendezvous_count(),
        sender_blocked_count(),
        receiver_blocked_count()
    );
    println!(
        "[ipc] caps: {} transferred, {} dropped by the five checks (ADR 035)",
        caps_transferred(),
        caps_dropped()
    );
    println!(
        "[ipc] notifications: {} signals delivered, {} to a bound waiter, \
         {} times alternation chose the endpoint and {} the notification",
        signals_delivered(),
        bound_deliveries(),
        alternation_chose_endpoint(),
        alternation_chose_notification()
    );
    println!(
        "[ipc] {} cancellations, {} replies found their target gone",
        cancellations(),
        stale_replies()
    );
    println!(
        "[ipc] call/reply: {} calls, {} replies delivered, {} of them \
         through ReplyRecv (one syscall rather than two, ADR 036)",
        calls(),
        replies_sent(),
        reply_recvs()
    );
}

// ---- The message transfer (ADR 034) ------------------------------------

/// Copy a message from one thread's registers and buffer to another's.
///
/// The register words are passed separately because they are not in the
/// TCB at all: they live in the caller's syscall frame, or — for the
/// kernel-side tests that stand in for user code until M13 — in an
/// array. Everything from `mr[3]` up is copied buffer to buffer.
///
/// The buffer is indexed `buffer[i]`, so its first three slots are never
/// used. That wastes 24 bytes and it was **measured** rather than
/// assumed: `buffer[i - 3]` costs four instructions on a single access,
/// and — the finding that decided it — stops LLVM recognising the copy
/// loop as a `memcpy`, turning 14 instructions into 40. See ADR 034's
/// measurement section; the probe is in scripts/m9-index-probe.
///
/// # Safety
///
/// Both TCBs must be live and distinct, and interrupts disabled.
unsafe fn copy_message(
    from: &Tcb,
    to: &mut Tcb,
    registers: &[u64; REGISTER_WORDS],
    length: usize,
) -> [u64; REGISTER_WORDS] {
    debug_assert!(!crate::sched::interrupts_enabled());
    debug_assert!(length <= MAX_MESSAGE_WORDS);

    // The register words pass straight through; the caller writes them
    // into the receiver's frame.
    let delivered = *registers;

    if length > REGISTER_WORDS {
        let src = from.ipc_buffer();
        let dst = to.ipc_buffer_mut();
        // `[i]`, not `[i - 3]`. See the note above.
        dst[REGISTER_WORDS..length].copy_from_slice(&src[REGISTER_WORDS..length]);
    }

    delivered
}

// ---- Capability transfer (ADR 035) -------------------------------------

/// Move up to four capabilities into the slots the receiver opened.
///
/// Returns how many actually arrived, which is what the receiver's tag
/// will say. The gap between asked and arrived is the only signal that
/// anything was dropped, and ADR 035 makes that deliberate: telling the
/// *sender* would let it probe the receiver's CSpace, and telling the
/// *receiver* is unnecessary because the slots are its own.
///
/// Five checks, each dropping one capability and continuing with the
/// next:
///
/// 1. the sender's CPtr resolves,
/// 2. the sender holds `GRANT` on it (ADR 021),
/// 3. the receiver opened a slot for it,
/// 4. that slot is empty,
/// 5. the derivation depth is within bounds (ADR 019).
///
/// The third is not a policy but the structure: if the receiver never
/// called `SetReceiveSlots`, there is no destination, and the loop below
/// does not run. A sender cannot force authority on anyone.
///
/// # Safety
///
/// Both TCBs live, interrupts disabled, and `sender_space` the sender's
/// own CSpace.
unsafe fn transfer_caps(
    sender: &Tcb,
    receiver: &Tcb,
    cptrs: &[super::cptr::CPtr],
    sender_space: &super::cptr::CSpace,
) -> usize {
    debug_assert!(!crate::sched::interrupts_enabled());

    let Some((dest_cnode, dest_start, dest_count)) = receiver.receive_slots() else {
        // Check 3, in its most important form: the receiver never
        // opened a slot, so there is nowhere for a capability to land.
        // Not a rejected transfer -- an absent one.
        if !cptrs.is_empty() {
            CAPS_DROPPED.fetch_add(cptrs.len() as u64, Ordering::AcqRel);
        }
        return 0;
    };

    let _ = sender;
    let mut transferred = 0usize;

    for (index, cptr) in cptrs.iter().enumerate().take(MAX_TRANSFER_CAPS) {
        if index >= dest_count {
            CAPS_DROPPED.fetch_add(1, Ordering::AcqRel);
            continue;
        }

        // 1. The sender's pointer resolves.
        //
        // SAFETY: The CSpace is the sender's, live for as long as the
        // thread is. `resolve` validates every field of the CPtr.
        let Ok(src) = (unsafe { super::cptr::resolve(sender_space, *cptr) }) else {
            CAPS_DROPPED.fetch_add(1, Ordering::AcqRel);
            continue;
        };

        // 2. GRANT: carrying a capability is producing authority.
        if !src.rights().contains(Rights::GRANT) {
            CAPS_DROPPED.fetch_add(1, Ordering::AcqRel);
            continue;
        }

        // SAFETY: `receive_slots` recorded a live CNode and a range
        // inside it; `dest_start + index` is within `dest_count`.
        let dest = unsafe { dest_cnode.slot(dest_start + index) };

        // 4. The receiver's slot is free.
        if !dest.is_empty() {
            CAPS_DROPPED.fetch_add(1, Ordering::AcqRel);
            continue;
        }

        // 5. Depth, checked by `copy` itself. Rights are carried
        //    unchanged: narrowing is `copy`'s job and the sender does it
        //    beforehand if it wants to (ADR 035).
        //
        // SAFETY: Both slots are live and distinct -- the destination is
        // empty, so it cannot be the source.
        match unsafe { cnode::copy(src, dest, src.rights()) } {
            Ok(()) => {
                transferred += 1;
                CAPS_TRANSFERRED.fetch_add(1, Ordering::AcqRel);
            }
            Err(_) => {
                CAPS_DROPPED.fetch_add(1, Ordering::AcqRel);
            }
        }
    }

    // Anything past the fourth was already refused by the tag check
    // (`TooManyCaps`), so there is nothing to count here.
    transferred
}

// ---- Thread state helpers ----------------------------------------------

/// Put a blocked thread back on the ready queue.
///
/// # Safety
///
/// `thread` must be live, off every IPC queue, and interrupts disabled.
unsafe fn make_runnable(thread: &mut Tcb) {
    debug_assert!(!crate::sched::interrupts_enabled());
    debug_assert!(
        thread.ipc_next() == 0 && thread.ipc_prev() == 0,
        "waking a thread that is still linked into an IPC queue"
    );
    thread.set_blocked_on(0);
    // SAFETY: The caller guarantees the thread is off every IPC queue,
    // and a blocked thread is by definition not on the ready queue
    // either -- which is `enqueue`'s precondition.
    unsafe {
        thread.set_state(ThreadState::Suspended);
        crate::sched::enqueue(thread);
    }
}

// ---- What a transfer produces ------------------------------------------

/// What a completed IPC delivered to the receiving thread.
///
/// Returned as a value rather than written into a syscall frame because
/// until M13 the "user" is kernel test code, which has no frame. The
/// dispatcher unpacks this into RDI, RSI and the three message
/// registers.
#[derive(Clone, Copy, Debug)]
pub struct Delivery {
    /// The tag, with `caps` corrected to what actually arrived.
    pub tag: MessageTag,
    /// The badge on the capability the sender used (ADR 037) — or the
    /// accumulated signal mask, when `tag.is_notification()`.
    pub badge: u32,
    /// The first three message words.
    pub registers: [u64; REGISTER_WORDS],
}

impl Delivery {
    pub const EMPTY: Delivery = Delivery {
        tag: MessageTag::EMPTY,
        badge: 0,
        registers: [0; REGISTER_WORDS],
    };
}

/// What a sender hands to the kernel.
///
/// Grouped so that `send`, `call` and `reply` do not each take five
/// positional arguments — and so that adding a field later is a change
/// to one type rather than to every signature.
#[derive(Clone, Copy)]
pub struct Message<'a> {
    pub tag: MessageTag,
    pub registers: [u64; REGISTER_WORDS],
    /// Capabilities to carry. At most [`MAX_TRANSFER_CAPS`], and the
    /// tag's `caps` field must agree.
    pub caps: &'a [super::cptr::CPtr],
}

impl Message<'_> {
    #[allow(dead_code)] // M9b
    pub const fn empty() -> Message<'static> {
        Message {
            tag: MessageTag::EMPTY,
            registers: [0; REGISTER_WORDS],
            caps: &[],
        }
    }

    /// A message with a label and three words, carrying no capability.
    #[allow(dead_code)] // M9b
    pub const fn words(label: u32, registers: [u64; REGISTER_WORDS]) -> Message<'static> {
        Message {
            tag: MessageTag::new(label, REGISTER_WORDS, 0),
            registers,
            caps: &[],
        }
    }
}

// ---- Parking a delivery ------------------------------------------------
//
// A blocked receiver cannot be handed its message directly: it is not
// executing, and its registers are in a saved context. So the delivery
// waits in the receiver's own TCB until it runs again.
//
// # What this replaced, and why the first version could not work
//
// M9a parked deliveries in a single global slot, with an assertion
// that only one could ever be in flight. The stated reason was that
// "the transfer runs with interrupts disabled and the woken thread
// reads its delivery before anything else can run".
//
// The second half is false. Waking a thread calls `sched::enqueue`,
// which makes it *ready* -- it does not run. The waker carries on, and
// a server that replies and then receives again (which is the whole
// shape of `ReplyRecv`) parks a second delivery while the first is
// still sitting in the slot.
//
// M9b's first run tripped the assertion on its second scenario. That
// is the assertion working: it reported a broken invariant instead of
// letting two clients receive each other's answers. But the invariant
// was never true -- only unreachable, because nothing in M9a produced
// two overlapping deliveries.
//
// A message addressed to a thread belongs in that thread. The send
// direction already worked this way (`PendingSend`), and the asymmetry
// was the oversight rather than a decision.

/// Collect the message this thread was woken with.
///
/// # Safety
///
/// Interrupts disabled, and `thread` is the running thread.
unsafe fn take_delivery(thread: &mut Tcb) -> Delivery {
    debug_assert!(!crate::sched::interrupts_enabled());
    match thread.take_incoming() {
        Some((tag, badge, registers)) => Delivery {
            tag: MessageTag::from_bits(tag),
            badge,
            registers,
        },
        None => Delivery::EMPTY,
    }
}

/// Hand a delivery to a thread that is about to be woken.
///
/// # Safety
///
/// Interrupts disabled; `thread` live and about to become runnable.
unsafe fn park_delivery(thread: &mut Tcb, delivery: Delivery) {
    debug_assert!(!crate::sched::interrupts_enabled());
    thread.park_incoming(delivery.tag.bits(), delivery.badge, delivery.registers);
}

// ---- The rendezvous (ADR 033) ------------------------------------------

/// Carry out a transfer between two threads that have met.
///
/// The steps are ADR 033's, in its order, and the order is not
/// cosmetic: the tag can only be corrected after the capability
/// transfer, because only then is the real count known.
///
/// # Safety
///
/// Both TCBs live and distinct; interrupts disabled; the receiver is
/// already off the endpoint's queue.
unsafe fn complete_rendezvous(
    sender: &mut Tcb,
    receiver: &mut Tcb,
    message: &Message<'_>,
    badge: u32,
    sender_space: &super::cptr::CSpace,
) -> Delivery {
    debug_assert!(!crate::sched::interrupts_enabled());

    // 1. The message.
    //
    // SAFETY: Both TCBs are live and distinct; interrupts disabled.
    let registers =
        unsafe { copy_message(sender, receiver, &message.registers, message.tag.length()) };

    // 2. Capabilities, into slots the receiver opened (ADR 035).
    //
    // Five checks, each dropping one capability and carrying on with
    // the next. The count that comes back is what actually arrived,
    // which is the only thing the receiver is told — see
    // `transfer_caps` for why neither side gets an error.
    //
    // SAFETY: Both TCBs are live and distinct; interrupts disabled;
    // `sender_space` is the sending thread's own CSpace.
    let transferred = unsafe { transfer_caps(sender, receiver, message.caps, sender_space) };

    // 3. The tag, carrying the count that is true rather than the one
    //    that was asked for. A tag claiming four when one arrived
    //    would send the receiver looking in empty slots (ADR 034).
    let tag = message.tag.with_caps(transferred);

    RENDEZVOUS_COUNT.fetch_add(1, Ordering::AcqRel);

    Delivery {
        tag,
        badge,
        registers,
    }
}

// ---- Send, Recv, Call, Reply (ADR 033, ADR 036) ------------------------

/// Whether a sender expects a reply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SendKind {
    /// `Send`: the sender carries on once the message is taken.
    Oneway,
    /// `Call`: the sender blocks until the receiver replies.
    Call,
}

/// `Send` / `Call`: deliver a message to an endpoint.
///
/// Blocks if no receiver is waiting — and for `Call`, blocks again
/// afterwards, in `BlockedOnReply`. Returns the reply for a `Call` and
/// nothing for a `Send`.
///
/// # Safety
///
/// `slot` must hold a live Endpoint capability; `sender_space` must be
/// the calling thread's own CSpace; no other mutable borrow of the
/// current TCB may be held.
unsafe fn send_inner(
    slot: &Slot,
    message: &Message<'_>,
    sender_space: &super::cptr::CSpace,
    kind: SendKind,
    caller_slot: Option<&Slot>,
) -> Result<Delivery, CapError> {
    // A note on `sender_space`, and a limitation written down rather
    // than discovered later.
    //
    // When the rendezvous happens immediately, the sender is running
    // and its CSpace is right here. When the sender blocks, the
    // *receiver* performs the transfer later and needs the sender's
    // CSpace then -- but a CSpace is reached through a root CNode
    // capability the sender holds, and resolving it from another
    // thread's context is M13's problem.
    //
    // Until M13 there is exactly one CSpace: the init CSpace boot
    // built (ADR 022). The deferred path therefore uses it, which is
    // correct today and will stop being correct the moment a second
    // CSpace exists. It is listed in the known gaps and the assertion
    // in `pending_sender_space` is what will fail first.
    // The whole transfer is indivisible (ADR 025). Not because the two
    // threads could race -- both are stopped -- but because a timer
    // tick could run a *third* thread, and that thread could send on
    // the same endpoint.
    let _guard = crate::sched::InterruptGuard::new();

    message.tag.validate()?;

    if !slot.rights().contains(Rights::WRITE) {
        return Err(CapError::InsufficientRights {
            needed: Rights::WRITE,
            held: slot.rights(),
        });
    }
    if kind == SendKind::Call && !slot.rights().contains(Rights::GRANTREPLY) {
        // Without GRANTREPLY the server has no way to answer, so a
        // `Call` would block forever by construction. Refusing is the
        // only outcome that is not a trap (ADR 021).
        return Err(CapError::InsufficientRights {
            needed: Rights::GRANTREPLY,
            held: slot.rights(),
        });
    }

    // The badge rides along from M9a, because it costs one `mov` and
    // because leaving it zero would make the M9b tests pass for the
    // wrong reason — a server that cannot tell its clients apart looks
    // identical to one whose clients all carry badge 0.
    let badge = slot.as_badge();

    // SAFETY: The caller guarantees the slot holds a live Endpoint.
    let endpoint = unsafe { endpoint_from_slot(slot) }?;
    // SAFETY: We are the running thread and hold no other borrow.
    let sender = unsafe { crate::sched::current() };

    // A receiver is already waiting: rendezvous now.
    if endpoint.state() == EndpointState::ReceiversWaiting {
        // SAFETY: Interrupts disabled.
        let receiver = unsafe { endpoint.dequeue() }.expect(
            "ReceiversWaiting with an empty queue: the state and \
                     the queue have drifted apart",
        );

        // SAFETY: Both TCBs live; the receiver is off the queue.
        let delivery =
            unsafe { complete_rendezvous(sender, receiver, message, badge, sender_space) };

        if kind == SendKind::Call {
            // The reply right goes into the *server's* TCB, and it goes
            // in **now** -- at rendezvous, not when the server's `Recv`
            // returns. ADR 036 is specific: written later, a second
            // receiver arriving on this endpoint could claim this
            // caller.
            let caller_slot = caller_slot.ok_or(CapError::InvalidArgument)?;
            // SAFETY: Every TCB lives in the physical window.
            let caller_phys = unsafe { tcb::tcb_phys(sender) }.0;
            let slot_phys =
                PhysAddr(core::ptr::from_ref(caller_slot) as u64 - crate::memory::pmap_base());
            // SAFETY: The caller is about to enter BlockedOnReply, and
            // `caller_slot` holds its capability.
            unsafe {
                receiver.set_reply_target(caller_phys, slot_phys, caller_slot.generation());
            }
        }

        // The receiver's outcome, recorded here because this is a
        // wake path and every wake path owes one.
        //
        // M9a left it out and nothing noticed: `recv` returned the
        // parked delivery without asking why it had woken, so the
        // field was never read. M9b's `collect_reply` reads the
        // outcome on both `Call` paths, and the measurement scenario
        // -- which blocks a receiver and then sends to it -- reached
        // this line first and woke with `Pending`.
        //
        // The panic that caught it is deliberately worded as an
        // instruction rather than a complaint: *some wake path is not
        // recording one*. This was that path.
        receiver.record_send_outcome(tcb::SendOutcome::Delivered);

        // SAFETY: The receiver is off every queue and about to run.
        unsafe {
            park_delivery(receiver, delivery);
            make_runnable(receiver);
        }

        if kind == SendKind::Oneway {
            return Ok(Delivery::EMPTY);
        }

        // `Call`: wait for the reply. Not in any queue -- the right to
        // wake us is in the server's TCB (ADR 033's second new state).
        //
        // SAFETY: We are the running thread; `block_current` yields
        // and returns when something resumes us.
        unsafe {
            sender.set_state(ThreadState::BlockedOnReply);
            crate::sched::block_current();
        }

        // And then the outcome, which this path used to skip.
        //
        // The first version returned `Ok(take_delivery(sender))` here
        // without asking *why* the thread had woken -- so a client
        // whose server died got an empty message and a success. The
        // kernel had done everything right: `release_reply_obligation`
        // woke the caller and recorded `ReplyGone`, and the log said
        // so. The caller simply did not look.
        //
        // The deferred path below always checked. The asymmetry is
        // the whole defect, and `ipc-server-dies` is what found it:
        // the scenario arranges an immediate rendezvous, so it is the
        // only one that reaches this branch with a dead server.
        let outcome = sender.take_send_outcome();
        // SAFETY: Running again, interrupts still disabled by the
        // guard above.
        return unsafe { collect_reply(sender, outcome, "Call, after an immediate rendezvous") };
    }

    // Nobody is receiving. Queue up and block.
    //
    // The message stays in this thread's own TCB (ADR 033): the
    // endpoint holds no storage, so there is nothing to allocate here
    // and nothing that could fail.
    SENDER_BLOCKED.fetch_add(1, Ordering::AcqRel);

    // SAFETY: Every Endpoint lives in the physical window.
    let endpoint_phys = unsafe { endpoint_phys(endpoint) };

    // Park what we are sending. The register words are about to stop
    // existing -- they are not in the callee-saved set a context
    // switch preserves -- so they go into the TCB alongside the tag.
    let reply = match caller_slot {
        Some(slot) => tcb::ReplySlot {
            slot: PhysAddr(core::ptr::from_ref(slot) as u64 - crate::memory::pmap_base()),
            generation: slot.generation(),
        },
        None => tcb::ReplySlot::NONE,
    };
    let cap_bits: [u64; MAX_TRANSFER_CAPS] = {
        let mut bits = [0u64; MAX_TRANSFER_CAPS];
        for (dst, src) in bits.iter_mut().zip(message.caps.iter()) {
            *dst = src.0;
        }
        bits
    };
    sender.set_pending_send(
        message.tag.bits(),
        message.registers,
        badge,
        kind == SendKind::Call,
        &cap_bits,
        reply,
    );
    sender.set_blocked_on(endpoint_phys);

    // SAFETY: Interrupts disabled; the sender is on no other queue.
    unsafe {
        endpoint.enqueue(sender, EndpointState::SendersWaiting);
        sender.set_state(ThreadState::BlockedOnEndpoint);
        crate::sched::block_current();
    }

    // Woken -- **once**, whichever kind this is, and that word is the
    // whole of a defect this path had.
    //
    // A queued `Call` does not wake when its message is taken.
    // `recv`'s `wants_reply` branch moves the sender from
    // BlockedOnEndpoint straight to BlockedOnReply *without* making
    // it runnable, so from this thread's side the two waits ADR 036
    // describes are one sleep. There is one wake, and therefore one
    // outcome to read.
    //
    // The first version read it here to choose between `Send` and
    // `Call`, then handed the `Call` to `collect_reply`, which read
    // it again. `take_send_outcome` clears the field, so the second
    // read found `Pending` and the panic fired -- correctly. It was
    // reporting a missing recording that was in fact a surplus read.
    let outcome = sender.take_send_outcome();

    if kind == SendKind::Call {
        // SAFETY: Running, interrupts disabled.
        return unsafe { collect_reply(sender, outcome, "Call, queued on an endpoint") };
    }

    match outcome {
        tcb::SendOutcome::Delivered => Ok(Delivery::EMPTY),
        tcb::SendOutcome::Cancelled => Err(CapError::EndpointDead),
        // A one-way `Send` never enters BlockedOnReply, and
        // `ReplyGone` is only ever recorded for a thread that is in
        // it: `cancel_ipc` chooses the outcome by state, and
        // `release_reply_obligation` only ever touches the thread a
        // reply target names. So this is not a case to handle but a
        // claim that the state machine is intact.
        tcb::SendOutcome::ReplyGone => panic!(
            "a one-way Send woke with outcome ReplyGone, which is only \
             recorded for a thread in BlockedOnReply -- a Send never \
             enters that state, so either cancel_ipc() stopped \
             choosing by state or a SendKind was lost"
        ),
        tcb::SendOutcome::Pending => unexpected_wake("Send, queued on an endpoint", outcome),
    }
}

/// Turn the outcome a `Call` woke with into a result.
///
/// Both halves of `Call` end here — the one that rendezvoused
/// immediately and the one that queued first — because both wake out
/// of `BlockedOnReply` and owe the caller the same answer. Having two
/// copies is how the immediate path came to skip the check entirely:
/// it returned the parked message without asking whether one had been
/// parked at all, so a dead server produced an empty success.
///
/// The outcome is **passed in rather than read here**, and that is
/// not a matter of taste. `take_send_outcome` clears the field, so a
/// second call consumes an answer nobody recorded; see the note in
/// `send_inner` above. One wake, one read, and the reading happens
/// where the caller already had to branch.
///
/// `waiting_in` names which of the two halves this was, so that a
/// `Pending` says more than "a Call woke without an answer".
///
/// # Safety
///
/// `thread` is the running thread and interrupts are disabled.
unsafe fn collect_reply(
    thread: &mut Tcb,
    outcome: tcb::SendOutcome,
    waiting_in: &str,
) -> Result<Delivery, CapError> {
    match outcome {
        tcb::SendOutcome::Delivered => {
            // SAFETY: Caller's guarantee.
            Ok(unsafe { take_delivery(thread) })
        }
        tcb::SendOutcome::ReplyGone => Err(CapError::ReplyTargetGone),
        // Woken out of the *first* wait by a cancellation: the
        // endpoint went away before anyone took the message. ADR 039's
        // table keeps this distinct from ReplyGone because "the
        // endpoint went away" and "the server went away" have
        // different remedies, and a client that could not tell them
        // apart would have to guess.
        tcb::SendOutcome::Cancelled => Err(CapError::EndpointDead),
        tcb::SendOutcome::Pending => unexpected_wake(waiting_in, outcome),
    }
}

/// A thread woke from an IPC wait with no outcome recorded.
///
/// Not a state the kernel can make sense of, and the master prompt
/// does not allow "undefined" as an answer: a thread resumed from an
/// IPC wait was resumed by *something*, and that something owes it an
/// answer.
///
/// `waiting_in` names the operation that was blocked. Without it the
/// message says a wake path is missing but not *which* wait it
/// failed to satisfy, and there are four — the two halves of `Call`,
/// `Recv`, and `Wait`. The first attempt at this panic omitted it and
/// cost a second run to find out.
fn unexpected_wake(waiting_in: &str, outcome: tcb::SendOutcome) -> ! {
    panic!(
        "a thread blocked in {} was resumed with outcome {}; some wake \
         path is not recording one. Every path that makes a blocked \
         thread runnable owes it an answer -- see reply(), \
         send_inner()'s rendezvous branch, cancel_ipc() and \
         release_reply_obligation()",
        waiting_in,
        outcome.name()
    )
}

/// Physical address of an Endpoint.
///
/// # Safety
///
/// `endpoint` must live inside the physical window, which every
/// retyped object does.
unsafe fn endpoint_phys(endpoint: &Endpoint) -> u64 {
    let virt = core::ptr::from_ref(endpoint) as u64;
    virt - crate::memory::pmap_base()
}

/// Physical address of a Notification.
///
/// # Safety
///
/// As [`endpoint_phys`].
unsafe fn notification_phys(notification: &Notification) -> u64 {
    let virt = core::ptr::from_ref(notification) as u64;
    virt - crate::memory::pmap_base()
}

/// `Send`: one-way message. Blocks until a receiver takes it.
///
/// # Safety
///
/// As [`send_inner`].
pub unsafe fn send(
    slot: &Slot,
    message: &Message<'_>,
    sender_space: &super::cptr::CSpace,
) -> Result<(), CapError> {
    // SAFETY: Caller's guarantee.
    unsafe { send_inner(slot, message, sender_space, SendKind::Oneway, None) }?;
    Ok(())
}

/// `Call`: send and wait for the reply.
///
/// `caller_slot` is the slot holding this thread's own TCB capability.
/// The kernel needs it to record a reply target that can be checked
/// for staleness later (ADR 036) — an address alone would not survive
/// the TCB being revoked and its memory reused.
///
/// # Safety
///
/// As [`send_inner`].
pub unsafe fn call(
    slot: &Slot,
    message: &Message<'_>,
    sender_space: &super::cptr::CSpace,
    caller_slot: &Slot,
) -> Result<Delivery, CapError> {
    CALLS.fetch_add(1, Ordering::AcqRel);
    // SAFETY: Caller's guarantee.
    unsafe {
        send_inner(
            slot,
            message,
            sender_space,
            SendKind::Call,
            Some(caller_slot),
        )
    }
}

/// `Recv`: wait for a message on an endpoint.
///
/// A thread with a bound Notification can also be woken by a signal,
/// with the alternation bit deciding which source wins when both are
/// ready — that is ADR 038 and it arrives in **M9c**. Nothing before
/// then can bind one, so the branch below never fires; it is left in
/// place, behind a condition that is currently always false, rather
/// than removed and rewritten later.
///
/// # Safety
///
/// `slot` must hold a live Endpoint capability, and no other mutable
/// borrow of the current TCB may be held.
pub unsafe fn recv(slot: &Slot) -> Result<Delivery, CapError> {
    let _guard = crate::sched::InterruptGuard::new();

    if !slot.rights().contains(Rights::READ) {
        return Err(CapError::InsufficientRights {
            needed: Rights::READ,
            held: slot.rights(),
        });
    }

    // SAFETY: Caller's guarantee.
    let endpoint = unsafe { endpoint_from_slot(slot) }?;
    // SAFETY: We are the running thread.
    let receiver = unsafe { crate::sched::current() };

    // A bound notification holding a signal, and the alternation bit
    // that keeps either source from starving the other (ADR 038).
    //
    // SAFETY: We are the running thread; `bound_signal_pending` only
    // reads, and a non-zero bound notification names a live object.
    let pending_signal = unsafe { bound_signal_pending(receiver) };
    let sender_waiting = endpoint.state() == EndpointState::SendersWaiting;

    // **The alternation, and it is one branch.**
    //
    // When both sources are ready, the one that did *not* win last
    // time wins now. ADR 038 calls this a correctness guarantee
    // rather than an optimisation, and the distinction decides how it
    // is verified: an optimisation is measured, a progress guarantee
    // is not — it either holds or it does not. `ipc-starvation` is
    // what shows this one holds, and it is built so that replacing
    // this branch with "notification first" makes the scenario
    // *hang*. An infinite wait is the definition of starvation, so a
    // test that hangs is stronger evidence than one that prints a
    // number.
    //
    // When only one source is ready there is nothing to alternate
    // between: the bit is not consulted, only updated.
    let take_endpoint = if sender_waiting && pending_signal {
        receiver.last_delivery_was_notification()
    } else {
        sender_waiting
    };

    // Diagnostic trace for the starvation scenario, off by default.
    //
    // Three attempts at that scenario were spent reasoning about an
    // interleaving none of the logs could show: the counters said
    // which branch *won*, never what the state was when the choice
    // was made. Guessing from an outcome is how the last two
    // "fixes" went in — one of them a real kernel change made for a
    // reason that turned out to be wrong.
    //
    // Turned on by the test, so an ordinary run pays nothing.
    if ALTERNATION_TRACE.load(Ordering::Acquire) != 0 {
        crate::println!(
            "[ipc] trace: sender_waiting={} pending_signal={} \
             last_was_ntfn={} -> take_endpoint={}",
            sender_waiting,
            pending_signal,
            receiver.last_delivery_was_notification(),
            take_endpoint
        );
    }

    if take_endpoint {
        if pending_signal {
            // Both were ready and alternation chose the endpoint. This
            // counter is the evidence for ADR 038's guarantee: a
            // "notification first" implementation leaves it at zero
            // while every other check still passes.
            ALTERNATION_CHOSE_ENDPOINT.fetch_add(1, Ordering::AcqRel);
        }

        // SAFETY: Interrupts disabled.
        let sender = unsafe { endpoint.dequeue() }.expect(
            "SendersWaiting with an empty queue: the state and the \
                     queue have drifted apart",
        );

        // The sender is blocked, so its parked message is complete and
        // nothing is writing it.
        let (tag_bits, registers, badge, wants_reply, cap_bits) = sender.pending_send();
        let message_tag = MessageTag::from_bits(tag_bits);
        let cptrs: [super::cptr::CPtr; MAX_TRANSFER_CAPS] =
            core::array::from_fn(|i| super::cptr::CPtr(cap_bits[i]));

        let message = Message {
            tag: message_tag,
            registers,
            caps: &cptrs[..message_tag.caps().min(MAX_TRANSFER_CAPS)],
        };

        // The sender's CSpace. Until M13 there is one (see the note in
        // `send_inner`), so this is the init CSpace -- correct today,
        // and the first thing that must change when components get
        // their own.
        let sender_space = pending_sender_space();

        // SAFETY: Both TCBs live and distinct; interrupts disabled.
        let delivery =
            unsafe { complete_rendezvous(sender, receiver, &message, badge, &sender_space) };

        if wants_reply {
            // SAFETY: Every TCB lives in the physical window.
            let caller_phys = unsafe { tcb::tcb_phys(sender) }.0;
            let reply = sender.pending_reply_slot();
            // SAFETY: The caller is entering BlockedOnReply and the
            // slot address was recorded when it called.
            unsafe {
                receiver.set_reply_target(caller_phys, reply.slot, reply.generation);
            }
            // SAFETY: Running thread; the sender stays blocked, now in
            // BlockedOnReply rather than BlockedOnEndpoint.
            unsafe {
                sender.set_state(ThreadState::BlockedOnReply);
            }
            sender.set_blocked_on(0);

            // No outcome is recorded here, and the omission is the
            // point: the sender is not being woken. It goes on
            // sleeping in a second state, and the answer it will read
            // belongs to whoever ends *that* wait -- `reply`,
            // `cancel_ipc`, or `release_reply_obligation`.
            //
            // Recording `Delivered` here would be worse than useless.
            // It would sit in the field as a plausible-looking stale
            // value, so a wake path that forgot its own recording
            // would hand the caller an empty message and a success
            // instead of tripping the `Pending` panic. The whole
            // value of that panic is that the field is empty unless
            // someone filled it deliberately.
        } else {
            sender.record_send_outcome(tcb::SendOutcome::Delivered);
            // SAFETY: The sender is off every IPC queue.
            unsafe {
                make_runnable(sender);
            }
        }

        // The bit is updated whether or not there was a choice to
        // make. Only updating it when both sources were ready would
        // leave it stale: a thread that took ten endpoint messages
        // with no signals pending would still have it reading
        // "notification last", and the first time both arrived
        // together the endpoint would win again.
        receiver.set_last_delivery_was_notification(false);
        return Ok(delivery);
    }

    // The bound notification won — either because it was the only
    // source ready, or because alternation chose it over a waiting
    // sender.
    if pending_signal {
        if sender_waiting {
            ALTERNATION_CHOSE_NOTIFICATION.fetch_add(1, Ordering::AcqRel);
        }
        // SAFETY: Running thread, and `bound_signal_pending` just
        // found a live bound notification holding a signal.
        let delivery = unsafe { deliver_bound_signal(receiver) };
        return Ok(delivery);
    }

    // Nothing to collect. Queue up and block.
    RECEIVER_BLOCKED.fetch_add(1, Ordering::AcqRel);

    // SAFETY: Every Endpoint lives in the physical window.
    let endpoint_addr = unsafe { endpoint_phys(endpoint) };
    receiver.set_blocked_on(endpoint_addr);

    // SAFETY: Interrupts disabled; the receiver is on no other queue.
    unsafe {
        endpoint.enqueue(receiver, EndpointState::ReceiversWaiting);
        receiver.set_state(ThreadState::BlockedOnEndpoint);
        crate::sched::block_current();
    }

    // Woken: a sender arrived, a bound signal came, or the wait was
    // cancelled. The outcome says which, and it is read rather than
    // inferred from the delivery's contents -- a zero-length,
    // zero-label message is perfectly legal (ADR 034), so "the
    // delivery looks empty" cannot distinguish a cancelled wait from
    // a satisfied one.
    //
    // M9a inferred it, with a three-part condition on the tag, the
    // badge and `blocked_on`. That worked only because nothing then
    // sent an empty message to a blocked receiver.
    //
    // SAFETY: Running again, interrupts disabled.
    match receiver.take_send_outcome() {
        tcb::SendOutcome::Delivered => {
            // SAFETY: As above.
            Ok(unsafe { take_delivery(receiver) })
        }
        tcb::SendOutcome::Cancelled => Err(CapError::EndpointDead),
        tcb::SendOutcome::ReplyGone => Err(CapError::ReplyTargetGone),
        outcome @ tcb::SendOutcome::Pending => {
            unexpected_wake("Recv, queued on an endpoint", outcome)
        }
    }
}

/// `Reply`: answer the thread that called us.
///
/// # Safety
///
/// `replier_space` must be the running thread's CSpace; no other
/// mutable borrow of the current TCB may be held.
pub unsafe fn reply(
    message: &Message<'_>,
    replier_space: &super::cptr::CSpace,
) -> Result<(), CapError> {
    let _guard = crate::sched::InterruptGuard::new();

    message.tag.validate()?;

    // SAFETY: We are the running thread.
    let server = unsafe { crate::sched::current() };

    let Some((caller_phys, _, _)) = server.reply_target() else {
        return Err(CapError::NoReplyTarget);
    };

    // Three comparisons: the slot still holds a TCB, it names the same
    // object, and the generation has not moved (ADR 036). The third is
    // what stops a reply reaching a *different* thread that happens to
    // occupy the same memory.
    if !server.reply_target_is_live() {
        server.clear_reply_target();
        STALE_REPLIES.fetch_add(1, Ordering::AcqRel);
        return Err(CapError::ReplyTargetGone);
    }

    // SAFETY: The checks above established the caller is live.
    let caller = unsafe { tcb::tcb_at(ObjectRef(PhysAddr(caller_phys))) };

    if caller.state() != ThreadState::BlockedOnReply {
        // The caller stopped waiting -- suspended, or woken by a
        // cancellation. Not an error in the caller's sense, but there
        // is nobody to answer.
        server.clear_reply_target();
        STALE_REPLIES.fetch_add(1, Ordering::AcqRel);
        return Err(CapError::ReplyTargetGone);
    }

    // SAFETY: Both TCBs live and distinct; interrupts disabled.
    let delivery = unsafe { complete_rendezvous(server, caller, message, 0, replier_space) };

    server.clear_reply_target();
    caller.set_replying_server(0);

    // The outcome, without which the caller wakes with `Pending` and
    // `send_inner` panics -- it refuses to invent a completion for a
    // thread that was resumed by *something*. That panic is the right
    // one; this is the line that keeps it from firing on the success
    // path.
    caller.record_send_outcome(tcb::SendOutcome::Delivered);

    // SAFETY: The caller is in BlockedOnReply, which is in no queue.
    unsafe {
        park_delivery(caller, delivery);
        make_runnable(caller);
    }

    REPLIES_SENT.fetch_add(1, Ordering::AcqRel);
    Ok(())
}

/// `ReplyRecv`: answer the current caller and wait for the next
/// request, in one system call.
///
/// The server loop's shape, and the reason it is one invocation rather
/// than two: a server that replied and then received would make two
/// syscalls per request, and the acceptance test counts them to show
/// this one does not.
///
/// # Safety
///
/// As [`reply`] and [`recv`].
pub unsafe fn reply_recv(
    reply_message: &Message<'_>,
    endpoint_slot: &Slot,
    replier_space: &super::cptr::CSpace,
) -> Result<Delivery, CapError> {
    REPLY_RECVS.fetch_add(1, Ordering::AcqRel);

    // Not a fastpath (ADR 040). The two halves run exactly the code
    // `reply` and `recv` run; what is saved is one kernel entry and
    // one exit, which is a real cost measured in M8 at 934 cycles --
    // and not a shortcut through either operation's checks.
    //
    // A reply that fails does not stop the receive: the server would
    // otherwise be left unable to serve anyone because one client
    // vanished. That is a deliberate asymmetry rather than a dropped
    // error -- the failure is counted in STALE_REPLIES, and a server
    // whose client disappeared between request and answer has nothing
    // to do about it except carry on.
    //
    // SAFETY: Caller's guarantee.
    let _ = unsafe { reply(reply_message, replier_space) };
    // SAFETY: Caller's guarantee.
    unsafe { recv(endpoint_slot) }
}

/// The CSpace a blocked sender's capabilities are resolved through.
///
/// Until M13 there is exactly one CSpace in the system — the init
/// CSpace boot built (ADR 022) — so a deferred transfer can find the
/// sender's capabilities without carrying a CSpace reference across a
/// thread switch.
///
/// This stops being correct the moment a second CSpace exists, which
/// is M13. Written here rather than hidden, so that the first
/// component with its own CSpace meets a named limitation instead of
/// a silently wrong lookup. The known gaps list it.
fn pending_sender_space() -> super::cptr::CSpace {
    super::boot::init_cspace()
}

// ---- Cancellation and death (ADR 039) ----------------------------------

/// Take a thread out of whatever IPC queue holds it.
///
/// **O(1)** — the whole of ADR 033's corrected decision. The thread's
/// own links name its neighbours and `blocked_on` names the endpoint,
/// so nothing is searched.
///
/// Two callers, both rare: `Suspend` on a blocked thread, and revoking
/// its TCB. Rare is not the same as cheap, though — the queue's length
/// is an *adversary's* thread count and this runs with interrupts
/// disabled, which is exactly why the singly linked version was
/// rejected. `ipc-cancel-cost` measures two lengths and requires the
/// figures to agree.
///
/// # Safety
///
/// `thread` must be live; interrupts disabled.
pub unsafe fn cancel_ipc(thread: &mut Tcb) {
    debug_assert!(!crate::sched::interrupts_enabled());

    if !thread.state().is_blocked_on_ipc() {
        return;
    }

    // Which wait is being cancelled, recorded **here** because this is
    // the last place it is known. A `Call` passes through both states,
    // so a thread woken later cannot tell from its own fields whether
    // it was queued on an endpoint or waiting for a reply -- and
    // ADR 039 gives those two different errors.
    let outcome = if thread.state() == ThreadState::BlockedOnReply {
        tcb::SendOutcome::ReplyGone
    } else {
        tcb::SendOutcome::Cancelled
    };

    CANCELLATIONS.fetch_add(1, Ordering::AcqRel);

    // Anything parked for this thread will never be collected.
    thread.discard_incoming();

    let endpoint_phys = thread.blocked_on();
    if endpoint_phys != 0 {
        // SAFETY: `blocked_on` holds the address of the endpoint this
        // thread queued on, written when it blocked.
        let endpoint = unsafe { endpoint_at(ObjectRef(PhysAddr(endpoint_phys))) };
        // SAFETY: Interrupts disabled; the thread is live.
        unsafe {
            endpoint.remove(thread);
        }
        thread.set_blocked_on(0);
    }

    // A thread in `BlockedOnReply` is in no queue at all (ADR 033), so
    // there is nothing to unlink — but a server may still hold a reply
    // right naming it.
    //
    // That right *is* cleared, and cheaply: `replying_server` is the
    // back-pointer ADR 039 added for exactly this, so finding the
    // server is a field read rather than a search. Doing it keeps a
    // later `Reply` from being charged as a stale one when the
    // staleness was already known here.
    //
    // The generation check in `reply_target_is_live` stays regardless:
    // the two are a belt and braces, and ADR 028 established the
    // pattern -- clean up *and* check validity, because the cleanup
    // cannot reach every path.
    let server_phys = thread.replying_server();
    if server_phys != 0 {
        // SAFETY: A non-zero `replying_server` names the live TCB that
        // owes this thread a reply; `set_reply_target` and this line
        // are the only writers.
        let server = unsafe { tcb::tcb_at(ObjectRef(PhysAddr(server_phys))) };
        server.clear_reply_target();
        thread.set_replying_server(0);
    }

    thread.record_send_outcome(outcome);
}

/// Tell the thread waiting on a dying server that no reply is coming
/// (ADR 036's "server crashes" promise, paid here).
///
/// Called when a TCB stops existing. Without it a client that made a
/// `Call` to a server that then died would wait forever — and ADR 036
/// says plainly that this case is one the kernel *can* know about, so
/// leaving the client blocked would be a defect rather than a
/// trade-off.
///
/// Returns whether anybody was woken, so the caller can say so.
///
/// # Safety
///
/// `server` must be live and interrupts disabled.
pub unsafe fn release_reply_obligation(server: &mut Tcb) -> bool {
    debug_assert!(!crate::sched::interrupts_enabled());

    let Some((caller_phys, _, _)) = server.reply_target() else {
        return false;
    };

    // The same three comparisons `Reply` makes. The caller may already
    // be gone -- both ends can die, in either order -- and waking
    // whatever now occupies its memory would be far worse than waking
    // nobody.
    if !server.reply_target_is_live() {
        server.clear_reply_target();
        return false;
    }

    // SAFETY: The check above established the caller is the thread it
    // was.
    let caller = unsafe { tcb::tcb_at(ObjectRef(PhysAddr(caller_phys))) };
    if caller.state() != ThreadState::BlockedOnReply {
        server.clear_reply_target();
        return false;
    }

    caller.discard_incoming();
    caller.set_replying_server(0);
    caller.record_send_outcome(tcb::SendOutcome::ReplyGone);
    server.clear_reply_target();
    CANCELLATIONS.fetch_add(1, Ordering::AcqRel);

    // SAFETY: A thread in BlockedOnReply is in no queue at all.
    unsafe {
        make_runnable(caller);
    }
    true
}

/// Wake every thread waiting on an endpoint that is being revoked.
///
/// Woken with an error rather than refused, and the reasoning is
/// **not** ADR 026's. There, refusing to revoke a busy TCB was safe
/// because the thread's owner could always suspend it first. Here the
/// waiters may belong to *other components*, so refusing would let any
/// component make an endpoint un-revokable simply by parking a thread
/// on it — a denial of service against whoever owns it (ADR 039).
///
/// # Safety
///
/// `endpoint` must be live; interrupts disabled.
pub unsafe fn cancel_all_waiters(endpoint: &mut Endpoint) -> usize {
    debug_assert!(!crate::sched::interrupts_enabled());

    let mut woken = 0usize;
    // The queue shrinks by one each time round, so this terminates in
    // at most its current length: no unbounded loop on a kernel path.
    //
    // SAFETY: Interrupts disabled.
    while let Some(thread) = unsafe { endpoint.dequeue() } {
        thread.discard_incoming();
        thread.set_blocked_on(0);
        thread.record_send_outcome(tcb::SendOutcome::Cancelled);
        CANCELLATIONS.fetch_add(1, Ordering::AcqRel);

        // SAFETY: Just taken off the queue, so on no queue at all.
        unsafe {
            make_runnable(thread);
        }
        woken += 1;

        assert!(
            woken <= MAX_QUEUE_LENGTH,
            "an endpoint's queue did not shrink while being drained; it \
             has a cycle"
        );
    }

    endpoint.set_state(EndpointState::Idle);
    woken
}

// ---- Notifications (ADR 038) -------------------------------------------

/// `Signal`: OR the capability's badge into the notification's mask.
///
/// **Never blocks**, and that is the reason notifications exist: M12's
/// interrupt handler will call this, and a handler cannot block.
///
/// # Safety
///
/// `slot` must hold a live Notification capability.
pub unsafe fn signal(slot: &Slot) -> Result<(), CapError> {
    let _guard = crate::sched::InterruptGuard::new();

    if !slot.rights().contains(Rights::WRITE) {
        return Err(CapError::InsufficientRights {
            needed: Rights::WRITE,
            held: slot.rights(),
        });
    }

    let badge = slot.as_badge();
    // SAFETY: Caller's guarantee.
    let notification = unsafe { notification_from_slot(slot) }?;

    // A mask, not a counter. Signalling the same source twice sets the
    // same bit twice, which is the same bit — so there is no overflow,
    // and therefore no decision about what to do when one happens
    // (ADR 038). What is lost is "how many"; what is kept is "from
    // where", and only the second can be answered correctly at all.
    notification.signalled |= badge;

    // Somebody blocked directly in `Wait`?
    if notification.waiter() != 0 {
        // SAFETY: A non-zero waiter is the address of a live TCB that
        // blocked in `wait`.
        let waiter = unsafe { tcb::tcb_at(ObjectRef(PhysAddr(notification.waiter()))) };
        notification.clear_waiter();

        let delivery = Delivery {
            tag: MessageTag::EMPTY.as_notification(),
            badge: notification.take(),
            registers: [0; REGISTER_WORDS],
        };

        SIGNALS_DELIVERED.fetch_add(1, Ordering::AcqRel);
        waiter.record_send_outcome(tcb::SendOutcome::Delivered);

        // SAFETY: A direct waiter sits in no endpoint queue.
        unsafe {
            park_delivery(waiter, delivery);
            make_runnable(waiter);
        }
        return Ok(());
    }

    // Or a thread blocked in `Recv` with this notification bound?
    if notification.bound_tcb() != 0 {
        // SAFETY: A non-zero bound TCB names a live thread.
        let bound = unsafe { tcb::tcb_at(ObjectRef(PhysAddr(notification.bound_tcb()))) };
        if bound.state() == ThreadState::BlockedOnEndpoint {
            // It is queued on an endpoint and this signal can wake it.
            //
            // Alternation does not arise here: if the endpoint had
            // anything for this thread, it would not still be queued.
            // Only one source is ready, so there is nothing to
            // alternate between.
            let endpoint_phys = bound.blocked_on();
            if endpoint_phys != 0 {
                // SAFETY: `blocked_on` names the endpoint it queued on.
                let endpoint = unsafe { endpoint_at(ObjectRef(PhysAddr(endpoint_phys))) };
                // SAFETY: Interrupts disabled; the thread is live.
                unsafe {
                    endpoint.remove(bound);
                }
            }

            let delivery = Delivery {
                tag: MessageTag::EMPTY.as_notification(),
                badge: notification.take(),
                registers: [0; REGISTER_WORDS],
            };

            // The bit records what was delivered last, so the next
            // *contested* decision can pick the other source. This
            // delivery is uncontested -- the thread was blocked and
            // only the notification was ready -- but it is still a
            // delivery, and the next decision should know a
            // notification went most recently.
            //
            // Worth recording that this line was removed during
            // M9c and then put back. The starvation scenario was
            // reporting `chose endpoint 7 / notification 0`, and
            // deleting this looked like the cause: it inverted the
            // figures to 0/7, which is not a fix but the same defect
            // mirrored. The actual explanation was in the test, and
            // it took a trace of the decision state to see it. See
            // `ipc-starvation` for what the figures mean.
            bound.set_last_delivery_was_notification(true);
            bound.record_send_outcome(tcb::SendOutcome::Delivered);
            SIGNALS_DELIVERED.fetch_add(1, Ordering::AcqRel);
            BOUND_DELIVERIES.fetch_add(1, Ordering::AcqRel);

            // SAFETY: Just removed from the endpoint's queue.
            unsafe {
                park_delivery(bound, delivery);
                make_runnable(bound);
            }
            return Ok(());
        }
    }

    // Nobody to wake. The bits stay set and whoever waits next sees
    // them — which is the only thing in M9 that is left for later, and
    // a mask is not a buffer: it does not grow.
    Ok(())
}

/// `Wait`: block until this notification is signalled.
///
/// # Safety
///
/// `slot` must hold a live Notification capability; no other mutable
/// borrow of the current TCB may be held.
pub unsafe fn wait(slot: &Slot) -> Result<u32, CapError> {
    let _guard = crate::sched::InterruptGuard::new();

    if !slot.rights().contains(Rights::READ) {
        return Err(CapError::InsufficientRights {
            needed: Rights::READ,
            held: slot.rights(),
        });
    }

    // SAFETY: Caller's guarantee.
    let notification = unsafe { notification_from_slot(slot) }?;
    // SAFETY: We are the running thread.
    let thread = unsafe { crate::sched::current() };

    if notification.has_pending() {
        SIGNALS_DELIVERED.fetch_add(1, Ordering::AcqRel);
        return Ok(notification.take());
    }

    // At most one waiter (ADR 038). A second would see the same
    // accumulated mask, and "which of them gets it" has no good
    // answer — so the question is refused rather than answered badly.
    if notification.waiter() != 0 {
        return Err(CapError::NotificationBusy);
    }

    // SAFETY: Every TCB lives in the physical window.
    let phys = unsafe { tcb::tcb_phys(thread) }.0;
    notification.set_waiter(phys);

    // SAFETY: We are the running thread; `suspend` yields here and
    // returns when something resumes us.
    unsafe {
        thread.set_state(ThreadState::BlockedOnEndpoint);
        crate::sched::block_current();
    }

    // Woken: a signal arrived, or the Notification was revoked.
    //
    // The outcome is matched **exhaustively**, and that is M9b's
    // lesson applied to a path M9b never reached. The first version
    // of this code had a `_ =>` arm, which looks tidy and is not: it
    // folds `Pending` — "nothing happened, and yet here you are" —
    // into the same answer as a cancelled wait. A thread resumed from
    // an IPC wait was resumed by *something*, and that something owes
    // it an answer; three separate wake paths in M9b turned out not
    // to be recording one, and every one of them was found by this
    // panic rather than by a wrong result.
    //
    // A catch-all would have hidden all three.
    match thread.take_send_outcome() {
        tcb::SendOutcome::Delivered => {
            // SAFETY: Running again, interrupts disabled.
            Ok(unsafe { take_delivery(thread) }.badge)
        }
        // The Notification was revoked while this thread waited on it
        // (ADR 039). `cancel_notification_wait` already cleared the
        // waiter field — the object may not exist any more, so
        // touching it here would be a use-after-free.
        tcb::SendOutcome::Cancelled => Err(CapError::EndpointDead),
        // Not reachable from a Notification wait: `ReplyGone` is
        // recorded only for a thread in `BlockedOnReply`, and `Wait`
        // never enters that state. Stated as a claim about the state
        // machine rather than handled as a case.
        tcb::SendOutcome::ReplyGone => panic!(
            "a thread waiting on a Notification woke with outcome \
             ReplyGone, which is only recorded for a thread in \
             BlockedOnReply -- Wait never enters that state"
        ),
        outcome @ tcb::SendOutcome::Pending => {
            unexpected_wake("Wait, blocked on a Notification", outcome)
        }
    }
}

/// Wake the thread waiting on a Notification that is being revoked
/// (ADR 039).
///
/// The Notification's counterpart to [`cancel_all_waiters`], and the
/// reasoning is the same one ADR 039 gives for endpoints: the waiter
/// may belong to *another component*, so refusing the revoke would
/// let any component make a Notification immortal by parking a
/// thread on it.
///
/// At most one thread can be waiting (ADR 038), so this is O(1) and
/// there is no queue to drain.
///
/// # Safety
///
/// `notification` must be live and interrupts disabled.
pub unsafe fn cancel_notification_waiter(notification: &mut Notification) -> bool {
    debug_assert!(!crate::sched::interrupts_enabled());

    let waiter_phys = notification.waiter();
    if waiter_phys == 0 {
        return false;
    }

    // SAFETY: A non-zero waiter is the address of a live TCB that
    // blocked in `wait`; `wait` is the only writer and it clears the
    // field on every exit.
    let waiter = unsafe { tcb::tcb_at(ObjectRef(PhysAddr(waiter_phys))) };

    // Cleared **here**, before the object goes away. `wait` cannot do
    // it on the way out: by then the Notification may have been
    // retyped into something else, and clearing a field through that
    // pointer would write into a stranger.
    notification.clear_waiter();

    waiter.discard_incoming();
    waiter.record_send_outcome(tcb::SendOutcome::Cancelled);
    CANCELLATIONS.fetch_add(1, Ordering::AcqRel);

    // SAFETY: A thread blocked in `wait` sits in no endpoint queue.
    unsafe {
        make_runnable(waiter);
    }
    true
}

/// Break the two-way link between a dying Notification and the thread
/// bound to it (ADR 038).
///
/// Without this, a revoked Notification would leave `bound_notification`
/// pointing at memory that may since have been retyped — and the next
/// `Recv` by that thread calls `bound_signal_pending`, which would
/// read a stranger's bytes as a signal mask.
///
/// # Safety
///
/// As [`cancel_notification_waiter`].
pub unsafe fn release_bound_tcb(notification: &mut Notification) -> bool {
    debug_assert!(!crate::sched::interrupts_enabled());

    let bound_phys = notification.bound_tcb();
    if bound_phys == 0 {
        return false;
    }

    // SAFETY: A non-zero `bound_tcb` names the live TCB that
    // `bind_notification` linked; both ends are written together.
    let bound = unsafe { tcb::tcb_at(ObjectRef(PhysAddr(bound_phys))) };
    bound.set_bound_notification(0);
    notification.set_bound_tcb(0);
    true
}

/// `Poll`: read and clear the mask without blocking.
///
/// Exists where `NBRecv` does not (ADR 033), and the asymmetry is
/// deliberate: a notification is already a non-blocking mechanism, so
/// "has anything arrived" is its natural question. Asking the same of
/// an endpoint would be a polling server — the design error ADR 033
/// declines to make convenient.
///
/// # Safety
///
/// `slot` must hold a live Notification capability.
pub unsafe fn poll(slot: &Slot) -> Result<u32, CapError> {
    let _guard = crate::sched::InterruptGuard::new();

    if !slot.rights().contains(Rights::READ) {
        return Err(CapError::InsufficientRights {
            needed: Rights::READ,
            held: slot.rights(),
        });
    }

    // SAFETY: Caller's guarantee.
    let notification = unsafe { notification_from_slot(slot) }?;
    let mask = notification.take();
    if mask != 0 {
        SIGNALS_DELIVERED.fetch_add(1, Ordering::AcqRel);
    }
    Ok(mask)
}

/// Whether a thread's bound notification is holding a signal.
///
/// # Safety
///
/// `thread` must be live.
unsafe fn bound_signal_pending(thread: &Tcb) -> bool {
    let bound = thread.bound_notification();
    if bound == 0 {
        return false;
    }
    // SAFETY: A non-zero bound notification names a live object:
    // `bind_notification` established it and `unbind` clears it.
    let notification = unsafe { notification_at(ObjectRef(PhysAddr(bound))) };
    notification.has_pending()
}

/// Hand a thread the signal its bound notification is holding.
///
/// # Safety
///
/// `thread` must be the running thread with a live bound notification
/// holding a pending signal; interrupts disabled.
unsafe fn deliver_bound_signal(thread: &mut Tcb) -> Delivery {
    // SAFETY: Caller's guarantee.
    let notification = unsafe { notification_at(ObjectRef(PhysAddr(thread.bound_notification()))) };
    let mask = notification.take();

    thread.set_last_delivery_was_notification(true);
    SIGNALS_DELIVERED.fetch_add(1, Ordering::AcqRel);
    BOUND_DELIVERIES.fetch_add(1, Ordering::AcqRel);

    Delivery {
        tag: MessageTag::EMPTY.as_notification(),
        badge: mask,
        registers: [0; REGISTER_WORDS],
    }
}

/// `BindNotification`: let one thread wake from either an endpoint or
/// a notification (ADR 038).
///
/// M13's driver is why this exists: a request arrives on an endpoint,
/// an interrupt arrives on a notification, and one thread must serve
/// both. Two threads would mean sharing the driver's state, which
/// means locking — on one CPU, because preemption exists.
///
/// # Safety
///
/// Both slots must hold live capabilities of the right types.
pub unsafe fn bind_notification(tcb_slot: &Slot, ntfn_slot: &Slot) -> Result<(), CapError> {
    let _guard = crate::sched::InterruptGuard::new();

    // SAFETY: Caller's guarantee.
    let thread = unsafe { tcb::from_slot(tcb_slot) }?;
    // SAFETY: Caller's guarantee.
    let notification = unsafe { notification_from_slot(ntfn_slot) }?;

    if thread.bound_notification() != 0 || notification.bound_tcb() != 0 {
        return Err(CapError::AlreadyBound);
    }
    if notification.waiter() != 0 {
        // Somebody is already blocked in `Wait` on it; binding now
        // would give the notification two claimants.
        return Err(CapError::NotificationBusy);
    }
    if thread.state().is_busy() || thread.state().is_blocked_on_ipc() {
        // ADR 026's rule: do not rearrange a thread the scheduler or
        // an endpoint is holding.
        return Err(CapError::ThreadBusy {
            state: thread.state().name(),
        });
    }

    // SAFETY: Both objects live in the physical window.
    let thread_phys = unsafe { tcb::tcb_phys(thread) }.0;
    // SAFETY: As above.
    let ntfn_phys = unsafe { notification_phys(notification) };

    thread.set_bound_notification(ntfn_phys);
    notification.set_bound_tcb(thread_phys);
    Ok(())
}

/// `UnbindNotification`: undo [`bind_notification`].
///
/// # Safety
///
/// As [`bind_notification`].
pub unsafe fn unbind_notification(tcb_slot: &Slot) -> Result<(), CapError> {
    let _guard = crate::sched::InterruptGuard::new();

    // SAFETY: Caller's guarantee.
    let thread = unsafe { tcb::from_slot(tcb_slot) }?;

    let bound = thread.bound_notification();
    if bound == 0 {
        return Err(CapError::InvalidArgument);
    }
    if thread.state().is_blocked_on_ipc() {
        return Err(CapError::ThreadBusy {
            state: thread.state().name(),
        });
    }

    // SAFETY: A non-zero bound notification names a live object.
    let notification = unsafe { notification_at(ObjectRef(PhysAddr(bound))) };
    notification.set_bound_tcb(0);
    thread.set_bound_notification(0);
    Ok(())
}

// ---- Receive slots (ADR 035) -------------------------------------------

/// `SetReceiveSlots`: open a window for incoming capabilities.
///
/// Without it a thread receives no capabilities — and that is not a
/// refusal but an **absence**. A sender cannot name a destination in
/// another thread's CSpace (if it could, the full/empty distinction
/// would let it scan), so with no window open there is nowhere for a
/// capability to land. The transfer loop simply does not run.
///
/// The difference from "a safe default" matters: a default can be
/// misconfigured, and this cannot be configured at all from the
/// sending side. **Taking authority is an action.**
///
/// # Safety
///
/// `tcb_slot` must hold a live TCB capability and `cnode_slot`, if
/// given, a live CNode capability.
pub unsafe fn set_receive_slots(
    tcb_slot: &Slot,
    cnode_slot: Option<&Slot>,
    index: usize,
    count: usize,
) -> Result<(), CapError> {
    let _guard = crate::sched::InterruptGuard::new();

    // SAFETY: Caller's guarantee.
    let thread = unsafe { tcb::from_slot(tcb_slot) }?;

    let Some(cnode_slot) = cnode_slot else {
        thread.clear_receive_slots();
        return Ok(());
    };

    if cnode_slot.is_empty() {
        return Err(CapError::EmptySlot);
    }
    if cnode_slot.cap_type() != super::CapType::CNode {
        return Err(CapError::WrongType {
            found: cnode_slot.cap_type(),
        });
    }
    if count > MAX_TRANSFER_CAPS {
        return Err(CapError::TooManyCaps { caps: count });
    }

    let radix = cnode_slot.as_radix();
    let slots = 1usize << radix;
    if index.saturating_add(count) > slots {
        return Err(CapError::CPtrOutOfRange);
    }

    let index: u16 = u16::try_from(index).map_err(|_| CapError::InvalidArgument)?;
    let count: u16 = u16::try_from(count).map_err(|_| CapError::InvalidArgument)?;

    // SAFETY: The capability names a live CNode of `1 << radix` slots,
    // and the window was just checked to lie inside it.
    unsafe {
        thread.set_receive_slots(cnode_slot.object().0, radix, index, count);
    }
    Ok(())
}
