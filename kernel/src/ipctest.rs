// SPDX-License-Identifier: BSD-2-Clause
//! IPC acceptance tests — **M9a and M9b**.
//!
//! M9 is landing in three parts. This file covers the first two: an
//! Endpoint object, `Send`, `Recv`, the doubly linked wait queue and
//! cancellation (M9a); then `Call`, `Reply`, `ReplyRecv`, badges and
//! reply validation (M9b). No capability transfer and no
//! Notification — those are M9c, and the code for them sits in
//! `cap::ipc` marked `#[allow(dead_code)]` rather than tested here.
//!
//! # The scenarios that carry the two milestones
//!
//! * **`ipc-cancel-cost`** measures one operation at two queue lengths
//!   and requires the figures to agree. A single figure would not show
//!   O(1) — the *equality of two* does, and if the queue were singly
//!   linked the second would be ten times the first. This is the
//!   evidence for ADR 033's corrected decision, and without it the
//!   doubly linked list is a claim rather than a result.
//! * **`ipc-endpoint-revoked`** is what proves `cancel_ipc` is wired
//!   in at all. Three threads wait, the endpoint's capability is
//!   deleted, and all three must wake. If the call were missing the
//!   scenario would hang, which is the right failure: a queue holding
//!   threads nobody can reach is not a thing to report politely.
//! * **`check_reply_is_not_stolen`** is M9b's: a second server calls
//!   `Recv` while the first is holding a caller, and must come away
//!   with nothing. The window is opened by the test rather than by
//!   the timer, so it fails on *every* run of a kernel that writes
//!   `reply_to` late — see the function for why that distinction is
//!   the whole scenario (ADR 023's K8).
//! * **`ipc-stale-reply`** carries two separate claims, because on
//!   the live path the back-pointer's cleanup always fires before the
//!   generation check. Asserting "the generation refused it" there
//!   would be a test passing for the wrong reason.
//!
//! # What M9b's first runs found
//!
//! Four kernel defects, each on the first run of the scenario that
//! reached it, and three of them in code M9a had shipped and tested.
//! They are described where they were fixed — see the scope table in
//! [`crate::cap::ipc`] — and they are the argument for having split
//! the milestone: written all at once, they would have arrived
//! together in a build that had never run.
//!
//! # How the worker threads are driven
//!
//! Through statics, because a thread entry point is an
//! `extern "C" fn() -> !` with nowhere to put an argument. One worker
//! moved through phases rather than a thread per case: threads cannot
//! be destroyed here and their KSTACK slots cannot be reclaimed, so a
//! thread per case would run out of address space long before the
//! suite finished.

use core::sync::atomic::{AtomicU64, Ordering};
// `AtomicU64` is still used by the PHASES/DONE arrays below.

use crate::cap::cnode::{self, CNode};
use crate::cap::ipc::{self, Message, MessageTag, REGISTER_WORDS};
use crate::cap::tcb;
use crate::cap::untyped;
use crate::cap::{boot, CapError, CapType};
use crate::memory::{layout, VirtAddr};
use crate::{println, qemu, sched};

// ---- Slot layout -------------------------------------------------------

const SLOT_ENDPOINT: usize = 0;
/// Badged copies of the endpoint capability, one per client (M9b).
///
/// Below the worker slots and clear of them. A badge lives in the
/// *capability*, not in the object (ADR 037), so two clients talking
/// to one endpoint need two slots — which is the mechanism rather
/// than an artefact of the test.
const SLOT_BADGED_BASE: usize = 3;
/// Workers. Each takes six slots: one TCB and up to four stack frames.
const SLOT_WORKER_BASE: usize = 8;
const SLOT_WORKER_STRIDE: usize = 8;

/// KSTACK slots 0 and 1 belong to IST1 and the main stack (ADR 009).
const STACK_SLOT_BASE: u64 = 2;

const LABEL_PING: u32 = 0x7071;
const LABEL_LONG: u32 = 0x4C4F;
/// A server's answer. Distinct from the request's label so a reply
/// cannot be mistaken for an echo of the request.
const LABEL_PONG: u32 = 0x504F;

/// Queue lengths for the cancellation measurement (ADR 033).
///
/// Ten times apart, which is what makes the result readable: with a
/// singly linked queue the second figure would be about ten times the
/// first, and with a doubly linked one they are the same.
const CANCEL_SHORT: usize = 50;
const CANCEL_LONG: usize = 500;

// ---- Worker phases -----------------------------------------------------

mod phase {
    pub const IDLE: u64 = 0;
    /// Receive once, record what arrived.
    pub const RECV: u64 = 1;
    /// Send a short message, blocking until somebody takes it.
    pub const SEND: u64 = 2;
    /// Receive and verify a full-length message word by word.
    pub const RECV_LONG: u64 = 3;
    /// Send a message carrying this worker's own index, for the FIFO
    /// check.
    pub const SEND_ORDERED: u64 = 4;
    /// Send a zero-word message.
    pub const SEND_EMPTY: u64 = 5;

    // ---- M9b ----
    /// `Call` through the plain endpoint capability and record the
    /// reply.
    pub const CALL: u64 = 6;
    /// `Call` through this worker's own *badged* capability, so the
    /// server can tell it from the other client.
    pub const CALL_BADGED: u64 = 7;
    /// Serve one request: `Recv`, then `Reply` with a value derived
    /// from the badge.
    pub const SERVE_ONCE: u64 = 8;
    /// Serve one request, but **wait to be released** between the
    /// `Recv` and the `Reply`.
    ///
    /// The whole of the reply-stealing scenario turns on this gap
    /// being under the test's control rather than the timer's: see
    /// [`super::check_reply_is_not_stolen`].
    pub const SERVE_SLOWLY: u64 = 9;
    /// `Recv` once and record whether anything arrived. Used as the
    /// *second* server in the stealing scenario — it must come away
    /// empty-handed.
    pub const RECV_EXPECT_NOTHING: u64 = 10;

    pub const STOP: u64 = 99;
}

/// One entry per worker. Index 0 is the first worker.
const MAX_WORKERS: usize = 8;

static PHASES: [AtomicU64; MAX_WORKERS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
static DONE: [AtomicU64; MAX_WORKERS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// What the last receiver saw.
static GOT_TAG: AtomicU64 = AtomicU64::new(0);
static GOT_WORD0: AtomicU64 = AtomicU64::new(0);
static GOT_ERROR: AtomicU64 = AtomicU64::new(0);
static GOT_MISMATCHES: AtomicU64 = AtomicU64::new(0);

/// The order in which messages were delivered, for the FIFO check.
static DELIVERY_ORDER: AtomicU64 = AtomicU64::new(0);

// ---- M9b: what each client got back -----------------------------------
//
// One slot per worker rather than one shared pair, because the whole
// claim of the badge scenario is that two replies did **not** cross.
// A single pair of statics could not express the failure it is meant
// to catch: whichever client wrote last would look correct.

/// The first reply word worker `i` received from its `Call`.
static REPLY_WORD: [AtomicU64; MAX_WORKERS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// The error number worker `i`'s `Call` returned, or 0 for success.
static REPLY_ERROR: [AtomicU64; MAX_WORKERS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// The badge a serving worker saw on the request it took.
static SERVED_BADGE: [AtomicU64; MAX_WORKERS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Set by a `SERVE_SLOWLY` worker once it holds a request, and
/// cleared by the main thread when it may answer.
///
/// Two flags rather than a sleep: the stealing scenario has to put a
/// *second* server onto the endpoint while the first is partway
/// through a transaction, and "partway" must be a position the test
/// chooses rather than one the timer happens to produce. ADR 023's K8
/// is the rule this follows.
static SERVER_HOLDS_REQUEST: AtomicU64 = AtomicU64::new(0);
static SERVER_MAY_REPLY: AtomicU64 = AtomicU64::new(0);

/// A small number per error, so a worker can say which one it saw.
///
/// Distinct values rather than a flag: "it failed" and "it failed the
/// way we predicted" are different claims, and ADR 023's K5 wants the
/// second.
fn error_number(error: CapError) -> u64 {
    match error {
        CapError::EndpointDead => 1,
        CapError::MessageTooLong { .. } => 6,
        CapError::TooManyCaps { .. } => 7,
        CapError::InsufficientRights { .. } => 8,
        CapError::InvalidArgument => 9,
        // M9b. Added because the catch-all below turned out to be a
        // trap rather than a convenience: `ipc-server-dies` compared
        // the client's recorded number against `error_number(
        // ReplyTargetGone)`, both sides went through this function,
        // and both came out 99 -- so the assertion was comparing the
        // fallback with itself and could not have distinguished
        // ReplyTargetGone from NoReplyTarget or anything else.
        //
        // The first run failed for a different reason and exposed it.
        CapError::ReplyTargetGone => 2,
        CapError::NoReplyTarget => 3,
        CapError::NotificationBusy => 4,
        CapError::AlreadyBound => 5,
        // Still a catch-all, but now only for errors no IPC scenario
        // expects. Seeing 99 in a log means "an error nobody planned
        // for", which is information; it used to mean "one of five
        // errors, and this function will not say which".
        _ => 99,
    }
}

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

// ---- Fixtures ----------------------------------------------------------

fn object_cnode() -> CNode {
    // SAFETY: boot::init put a CNode in this root slot.
    unsafe { boot::leaf_cnode(boot::ROOT_SLOT_OBJECTS) }
}

fn slot(index: usize) -> &'static mut cnode::Slot {
    // SAFETY: Every index here is below the CNode's radix, which boot
    // reported as 8 (256 slots).
    unsafe { object_cnode().slot(index) }
}

fn worker_tcb_slot(id: usize) -> usize {
    SLOT_WORKER_BASE + id * SLOT_WORKER_STRIDE
}

/// Bytes one worker costs: a TCB plus the frames for its stack.
///
/// Stated rather than guessed, because this is what the fixture asks
/// the capability system for. Alignment can push the real cost a
/// little higher, so the request rounds up to the next power of two —
/// a root Untyped that cannot spare 32 KiB cannot reliably spare 18.
const WORKER_BYTES: u64 = 32 * 1024;

fn make_endpoint(dest: usize) {
    let cnode = object_cnode();
    // SAFETY: The CSpace is built; the request is small.
    let parent = unsafe { boot::root_untyped_with_room(crate::cap::ENDPOINT_SIZE as u64 * 4) };
    // SAFETY: Live Untyped and CNode; the destination slot is empty.
    unsafe { untyped::retype(parent, CapType::Endpoint, 0, 1, &cnode, dest) }
        .expect("could not retype an Endpoint");
}

/// Build a thread with its own stack, as M6's tests do.
///
/// **Asks for space rather than naming an Untyped.** The first draft
/// drew every worker from `root_untyped(1)` and ran it dry partway
/// through the second thread's stack:
///
/// ```text
/// NotEnoughMemory { remaining: 10240, needed: 18432 }
/// ```
///
/// The obvious fix — pick a different index — failed the same way
/// with different numbers, because the mistake was not the index but
/// the *indexing*. A fixture that names a root Untyped by position is
/// relying on the shape of this machine's memory map, and it works
/// until somebody adds one more object (ADR 023).
///
/// That the failure arrived as a named error with the watermark
/// untouched is ADR 020's atomicity doing its job: a half-built
/// thread would have been far harder to diagnose.
fn create_worker(id: usize, entry: extern "C" fn() -> !) {
    create_worker_from(id, entry, 0);
}

/// As [`create_worker`], but drawing from an Untyped other than the
/// one the default would pick.
///
/// Only `ipc-stale-reply` needs this, and the reason does not
/// contradict K6. That scenario has to **revoke** the client's Untyped
/// to get the TCB's memory back, and a revoke destroys every object
/// carved from it — so a client and a server sharing one would mean
/// killing the running server as a side effect.
///
/// The requirement is therefore not "give me Untyped number 3" but
/// "give me one that is not the one you gave the server", which is
/// still a statement about what the fixture *needs* rather than about
/// this machine's memory map, and still fails loudly when it cannot
/// be met.
fn create_worker_from(id: usize, entry: extern "C" fn() -> !, skip: usize) {
    let cnode = object_cnode();
    // SAFETY: The CSpace is built. This panics with a diagnosis
    // rather than returning a too-small Untyped.
    let parent = unsafe { boot::root_untyped_with_room_from(WORKER_BYTES, skip) };
    let tcb_slot_index = worker_tcb_slot(id);
    let stack_slot = STACK_SLOT_BASE + id as u64;

    // SAFETY: Live Untyped and CNode; the slot is empty.
    unsafe { untyped::retype(parent, CapType::Tcb, 0, 1, &cnode, tcb_slot_index) }
        .expect("could not retype a TCB");

    let (_, bottom, top) = layout::kstack_slot(stack_slot);
    let frames_needed = (layout::KSTACK_SIZE / crate::memory::FRAME_SIZE) as usize;
    let frame_base = tcb_slot_index + 1;

    // SAFETY: As above.
    unsafe { untyped::retype(parent, CapType::Frame, 0, frames_needed, &cnode, frame_base) }
        .expect("could not retype the stack frames");

    for index in 0..frames_needed {
        // SAFETY: Index in range.
        let frame_slot = unsafe { cnode.slot(frame_base + index) };
        let frame = crate::memory::PhysFrame::containing(frame_slot.object().0);
        let page = VirtAddr::new(bottom.0 + (index as u64) * crate::memory::FRAME_SIZE);

        // SAFETY: The kernel's tables are live, the address is inside
        // this thread's own unmapped KSTACK slot, and the frame came
        // from a retype so nothing else owns it.
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
    // SAFETY: The slot holds a live TCB; the stack is mapped and owned
    // by nothing else, and `entry` diverges.
    unsafe { tcb::set_registers(tcb_slot, entry, top, bottom, stack_slot) }
        .expect("SetRegisters failed");

    // SAFETY: The slot holds a live, inactive TCB.
    let thread = unsafe { tcb::from_slot(tcb_slot) }.expect("not a TCB");
    // SAFETY: The thread is Inactive.
    unsafe { sched::resume(thread) }.expect("could not start a worker");
}

/// Yield until a condition holds, saying what was awaited if it never
/// does. Bounded: a test that spins forever reports nothing.
fn spin_until(what: &str, mut condition: impl FnMut() -> bool) {
    const LIMIT: u64 = 2_000_000;
    let mut spins = 0u64;
    while !condition() {
        sched::yield_now();
        spins += 1;
        assert!(
            spins < LIMIT,
            "waited {} yields for {} and it never happened",
            spins,
            what
        );
    }
}

/// Ask worker `id` to do something and wait for it to *block*.
///
/// Waiting for the block rather than for completion, because every
/// scenario here needs the worker parked on the endpoint before the
/// main thread does its half of the rendezvous.
fn start_phase_and_wait_for_block(id: usize, what: &str, p: u64, counter: impl Fn() -> u64) {
    let before = counter();
    PHASES[id].store(p, Ordering::Release);
    spin_until(what, || counter() > before);
}

// ---- The worker --------------------------------------------------------

/// Which worker is running, found by looking up its own TCB.
///
/// Not `NEXT_WORKER_ID.fetch_add()` on entry, which is what the first
/// draft did and which deadlocked: the id a thread claimed depended
/// on the order the scheduler happened to run them in, so the main
/// thread would write `PHASES[0]` while the thread that had taken id
/// 0 was a different one than intended — and sometimes no thread had
/// taken it yet at all.
///
/// A thread's identity is not something to be raced for. It is
/// already decided by which TCB it is running on, so that is what is
/// consulted.
fn worker_id() -> usize {
    // `current_phys()` rather than `current()` plus `tcb_phys()`:
    // the scheduler already holds this thread's physical address, so
    // the round trip through a borrow and back was both longer and
    // one more thing to get wrong.
    let phys = sched::current_phys();

    for id in 0..MAX_WORKERS {
        let s = slot(worker_tcb_slot(id));
        if !s.is_empty() && s.object().0 .0 == phys {
            return id;
        }
    }

    // Diagnostic rather than a bare refusal: the useful questions are
    // *which* TCB is running and what the fixture actually put where.
    // A message without those costs a second debugging session.
    println!("[ipc] worker_id: running on TCB {:#018x}", phys);
    for id in 0..MAX_WORKERS {
        let s = slot(worker_tcb_slot(id));
        if s.is_empty() {
            println!("[ipc]   slot {} (id {}): empty", worker_tcb_slot(id), id);
        } else {
            println!(
                "[ipc]   slot {} (id {}): {} at {:#018x}",
                worker_tcb_slot(id),
                id,
                s.cap_type(),
                s.object().0 .0
            );
        }
    }
    panic!(
        "a worker thread is running on a TCB that is not in any worker \
         slot; the fixture and the scheduler disagree about which \
         threads exist"
    );
}

extern "C" fn worker() -> ! {
    let id = worker_id();

    loop {
        let p = PHASES[id].swap(phase::IDLE, Ordering::AcqRel);
        match p {
            phase::IDLE => {
                sched::yield_now();
                continue;
            }
            phase::RECV => {
                let ep = slot(SLOT_ENDPOINT);
                // SAFETY: Live Endpoint; no other borrow of this TCB.
                match unsafe { ipc::recv(ep) } {
                    Ok(d) => {
                        GOT_TAG.store(d.tag.bits(), Ordering::Release);
                        GOT_WORD0.store(d.registers[0], Ordering::Release);
                    }
                    Err(e) => GOT_ERROR.store(error_number(e), Ordering::Release),
                }
            }
            phase::SEND => send_words(LABEL_PING, [0xAAAA, 0xBBBB, 0xCCCC], REGISTER_WORDS),
            phase::SEND_EMPTY => send_words(LABEL_PING, [0, 0, 0], 0),
            phase::SEND_ORDERED => {
                // Each worker sends its own id, so the receiver can
                // check the order they come back in.
                send_words(LABEL_PING, [id as u64, 0, 0], REGISTER_WORDS);
            }
            phase::RECV_LONG => recv_and_verify_long(),

            // ---- M9b ----
            phase::CALL => do_call(id, SLOT_ENDPOINT),
            phase::CALL_BADGED => do_call(id, badged_slot_for(id)),
            phase::SERVE_ONCE => serve_one_request(id, false),
            phase::SERVE_SLOWLY => serve_one_request(id, true),
            phase::RECV_EXPECT_NOTHING => {
                let ep = slot(SLOT_ENDPOINT);
                // SAFETY: Live Endpoint; no other borrow of this TCB.
                match unsafe { ipc::recv(ep) } {
                    Ok(d) => SERVED_BADGE[id].store(d.badge as u64 + 1, Ordering::Release),
                    Err(e) => REPLY_ERROR[id].store(error_number(e), Ordering::Release),
                }
            }

            phase::STOP => break,
            other => panic!("worker {} was given an unknown phase {}", id, other),
        }
        DONE[id].fetch_add(1, Ordering::AcqRel);
    }

    park()
}

fn send_words(label: u32, registers: [u64; REGISTER_WORDS], length: usize) {
    let ep = slot(SLOT_ENDPOINT);
    let space = boot::init_cspace();
    let message = Message {
        tag: MessageTag::new(label, length, 0),
        registers,
        caps: &[],
    };
    // SAFETY: Live Endpoint and this thread's own CSpace.
    if let Err(e) = unsafe { ipc::send(ep, &message, &space) } {
        GOT_ERROR.store(error_number(e), Ordering::Release);
    }
}

/// Check a 128-word message word by word.
fn recv_and_verify_long() {
    let ep = slot(SLOT_ENDPOINT);
    // SAFETY: Live Endpoint; no other borrow of this TCB.
    let delivery = match unsafe { ipc::recv(ep) } {
        Ok(d) => d,
        Err(e) => {
            GOT_ERROR.store(error_number(e), Ordering::Release);
            return;
        }
    };

    let mut wrong = 0u64;
    if delivery.registers != [0xF00D, 0xBEEF, 0xCAFE] {
        wrong += 1;
    }
    if delivery.tag.length() != ipc::MAX_MESSAGE_WORDS {
        wrong += 1;
    }

    // SAFETY: We are the running thread.
    let me = unsafe { sched::current() };
    let buffer = me.ipc_buffer();
    // Iterated with `enumerate` over the tail rather than indexed by
    // a range: the index is still needed, because the expected value
    // is derived from it, but the slice bound then comes from the
    // slice itself rather than from a constant that has to agree with
    // it.
    for (offset, word) in buffer[REGISTER_WORDS..ipc::MAX_MESSAGE_WORDS]
        .iter()
        .enumerate()
    {
        let index = REGISTER_WORDS + offset;
        if *word != 0x1000_0000 + index as u64 {
            wrong += 1;
        }
    }

    GOT_MISMATCHES.store(wrong, Ordering::Release);
    GOT_TAG.store(delivery.tag.bits(), Ordering::Release);
}
// ---- M9b: the client and server halves ---------------------------------

/// Slot holding worker `id`'s badged capability to the endpoint.
///
/// Each client gets its own, because a badge is attached to a
/// *capability* rather than to the object (ADR 037) — that is the whole
/// mechanism, and sharing one slot would make the scenario untestable.
fn badged_slot_for(id: usize) -> usize {
    SLOT_BADGED_BASE + id
}

/// The badge worker `id` carries.
///
/// Non-zero and distinct. Zero means "no badge" (ADR 037), so a client
/// assigned 0 would be indistinguishable from an unbadged one — the
/// test would then pass without the badge mechanism doing anything.
fn badge_for(id: usize) -> u32 {
    0x1000 + id as u32
}

/// What a server replies to a request carrying `badge`.
///
/// Derived from the badge rather than fixed, so a reply delivered to
/// the wrong client is visible in the client's own check. Two constants
/// would both look correct whichever way they were crossed.
fn reply_value_for(badge: u32) -> u64 {
    0xBEEF_0000_0000_0000 | badge as u64
}

/// `Call` through `endpoint_slot` and record what came back.
fn do_call(id: usize, endpoint_slot: usize) {
    let ep = slot(endpoint_slot);
    let space = boot::init_cspace();
    // The slot holding *this thread's own* TCB capability. ADR 036
    // needs it to record a reply target that can be checked for
    // staleness later: an address alone would not survive the TCB
    // being revoked and its memory reused.
    let own = slot(worker_tcb_slot(id));

    let message = Message {
        tag: MessageTag::new(LABEL_PING, REGISTER_WORDS, 0),
        registers: [id as u64, 0, 0],
        caps: &[],
    };

    // SAFETY: Live Endpoint, this thread's own CSpace, and `own` holds
    // this thread's TCB capability.
    match unsafe { ipc::call(ep, &message, &space, own) } {
        Ok(delivery) => REPLY_WORD[id].store(delivery.registers[0], Ordering::Release),
        Err(e) => REPLY_ERROR[id].store(error_number(e), Ordering::Release),
    }
}

/// Take one request and answer it.
///
/// With `slowly`, the server stops between the `Recv` and the `Reply`
/// and waits to be released. That pause is the reply-stealing
/// scenario's whole apparatus: it puts the server in the exact state
/// ADR 036 is worried about — holding a caller, not yet having
/// answered — and holds it there for as long as the test needs,
/// rather than for however long a timer tick happens to last.
fn serve_one_request(id: usize, slowly: bool) {
    let ep = slot(SLOT_ENDPOINT);
    let space = boot::init_cspace();

    // SAFETY: Live Endpoint; no other borrow of this TCB.
    let request = match unsafe { ipc::recv(ep) } {
        Ok(d) => d,
        Err(e) => {
            REPLY_ERROR[id].store(error_number(e), Ordering::Release);
            return;
        }
    };

    // The badge, recorded before anything else can change it. This is
    // what a real server would switch on to find the client's state.
    SERVED_BADGE[id].store(request.badge as u64, Ordering::Release);

    if slowly {
        // Say we are holding a request, then wait for permission.
        //
        // `yield_now` rather than a spin: the main thread has to run
        // to release us, and on one CPU a tight loop would simply
        // consume the timeslice.
        SERVER_HOLDS_REQUEST.store(1, Ordering::Release);
        spin_until("the test to release the slow server", || {
            SERVER_MAY_REPLY.load(Ordering::Acquire) != 0
        });
    }

    let answer = Message {
        tag: MessageTag::new(LABEL_PONG, REGISTER_WORDS, 0),
        registers: [reply_value_for(request.badge), 0, 0],
        caps: &[],
    };

    // SAFETY: This thread's own CSpace; no other borrow of this TCB.
    if let Err(e) = unsafe { ipc::reply(&answer, &space) } {
        REPLY_ERROR[id].store(error_number(e), Ordering::Release);
    }
}

/// A worker that blocks on the endpoint and never comes back.
///
/// Used by the cancellation scenarios, where the point is to have a
/// queue to cancel from.
extern "C" fn blocker() -> ! {
    let ep = slot(SLOT_ENDPOINT);
    // SAFETY: Live Endpoint; no other borrow of this TCB.
    match unsafe { ipc::recv(ep) } {
        Ok(_) => GOT_WORD0.fetch_add(1, Ordering::AcqRel),
        Err(e) => {
            GOT_ERROR.store(error_number(e), Ordering::Release);
            DELIVERY_ORDER.fetch_add(1, Ordering::AcqRel)
        }
    };
    park()
}

/// Suspend forever. Cheaper than spinning, and it does not compete
/// with whatever the main thread is still doing.
fn park() -> ! {
    loop {
        // SAFETY: We are the running thread.
        let me = unsafe { sched::current() };
        // SAFETY: Suspending the current thread yields; if anything
        // resumes it, the loop suspends it again.
        unsafe {
            sched::suspend(me);
        }
    }
}

// ---- The positive suite ------------------------------------------------

/// `ipc-test`: five scenarios.
pub fn run() -> ! {
    println!("[ipc] M9a: synchronous rendezvous (ADR 033, ADR 034)");

    check_object_sizes();
    make_endpoint(SLOT_ENDPOINT);

    // Three workers. Each asks for the space it needs; where that
    // comes from is the capability system's problem, not the test's.
    create_worker(0, worker);
    create_worker(1, worker);
    create_worker(2, worker);

    check_rendezvous_both_orders();
    check_message_lengths();
    check_tag_validation();
    check_fifo_order();

    ipc::report();
    println!("[ipc] all M9a IPC tests passed");

    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        qemu::EXIT_SUCCESS,
        qemu::HOST_STATUS_SUCCESS
    );
    qemu::exit(qemu::EXIT_SUCCESS)
}

/// 1. Object sizes, against ADR 017's ceilings.
fn check_object_sizes() {
    use core::mem::size_of;

    let endpoint = size_of::<ipc::Endpoint>();
    let tcb = size_of::<tcb::Tcb>();

    assert_eq!(endpoint, 32, "Endpoint is not 32 bytes");
    assert_eq!(
        tcb, 2048,
        "the TCB grew past ADR 017's ceiling when M9 added its fields"
    );

    println!(
        "[ipc] sizes: Endpoint {} B, TCB {} B with M9's new fields -- \
         ADR 017's ceilings unchanged",
        endpoint, tcb
    );
    println!(
        "[ipc] message: {} words in registers, {} total ({} B); the \
         buffer's first {} words are unused, which was measured rather \
         than assumed (ADR 034)",
        REGISTER_WORDS,
        ipc::MAX_MESSAGE_WORDS,
        ipc::MAX_MESSAGE_WORDS * 8,
        REGISTER_WORDS
    );
}

/// 2-3. Rendezvous in both orders.
fn check_rendezvous_both_orders() {
    // (a) Receiver first: worker 0 blocks, this thread sends.
    start_phase_and_wait_for_block(
        0,
        "the receiver to block",
        phase::RECV,
        ipc::receiver_blocked_count,
    );

    let ep = slot(SLOT_ENDPOINT);
    let space = boot::init_cspace();
    let done = DONE[0].load(Ordering::Acquire);
    // SAFETY: Live Endpoint and CSpace; no other borrow of this TCB.
    unsafe {
        ipc::send(
            ep,
            &Message {
                tag: MessageTag::new(LABEL_PING, REGISTER_WORDS, 0),
                registers: [1, 2, 3],
                caps: &[],
            },
            &space,
        )
    }
    .expect("Send to a waiting receiver failed");
    spin_until("the receiver to report", || {
        DONE[0].load(Ordering::Acquire) > done
    });

    assert_eq!(GOT_ERROR.load(Ordering::Acquire), 0, "the receiver errored");
    assert_eq!(
        GOT_WORD0.load(Ordering::Acquire),
        1,
        "the receiver got the wrong first word"
    );
    let receiver_first = ipc::rendezvous_count();

    // (b) Sender first: worker 1 blocks in `Send`, this thread
    //     receives. The message has to survive the sender being
    //     descheduled, which is what ADR 033 means by "the message
    //     lives in the sender's TCB".
    start_phase_and_wait_for_block(1, "the sender to block", phase::SEND, || {
        ipc::sender_blocked_count()
    });

    // SAFETY: Live Endpoint; no other borrow of this TCB.
    let delivery = unsafe { ipc::recv(ep) }.expect("Recv from a waiting sender failed");
    assert_eq!(
        delivery.registers[0], 0xAAAA,
        "the message from the blocked sender was wrong"
    );
    assert_eq!(delivery.tag.label(), LABEL_PING);

    println!(
        "[ipc] rendezvous: both orders work -- {} senders and {} receivers \
         had to block, {} rendezvous completed ({} of them receiver-first)",
        ipc::sender_blocked_count(),
        ipc::receiver_blocked_count(),
        ipc::rendezvous_count(),
        receiver_first
    );
}

/// 4. Message lengths: 0, 3 and 128 words.
fn check_message_lengths() {
    let ep = slot(SLOT_ENDPOINT);
    let space = boot::init_cspace();

    // (a) Zero words. A legal message, and the one most likely to be
    //     mishandled by a copy loop that assumes there is something to
    //     copy.
    start_phase_and_wait_for_block(
        0,
        "the receiver to block",
        phase::RECV,
        ipc::receiver_blocked_count,
    );
    let done = DONE[0].load(Ordering::Acquire);
    // SAFETY: Live Endpoint and CSpace.
    unsafe {
        ipc::send(
            ep,
            &Message {
                tag: MessageTag::new(LABEL_PING, 0, 0),
                registers: [0; REGISTER_WORDS],
                caps: &[],
            },
            &space,
        )
    }
    .expect("a zero-word send failed");
    spin_until("the empty message to arrive", || {
        DONE[0].load(Ordering::Acquire) > done
    });
    assert_eq!(
        MessageTag::from_bits(GOT_TAG.load(Ordering::Acquire)).length(),
        0,
        "a zero-word message arrived with a non-zero length"
    );

    // (b) Three words: the register-only case, which never touches the
    //     buffer at all.
    start_phase_and_wait_for_block(
        0,
        "the receiver to block",
        phase::RECV,
        ipc::receiver_blocked_count,
    );
    let done = DONE[0].load(Ordering::Acquire);
    // SAFETY: As above.
    unsafe {
        ipc::send(
            ep,
            &Message {
                tag: MessageTag::new(LABEL_PING, REGISTER_WORDS, 0),
                registers: [0x11, 0x22, 0x33],
                caps: &[],
            },
            &space,
        )
    }
    .expect("a three-word send failed");
    spin_until("the three-word message to arrive", || {
        DONE[0].load(Ordering::Acquire) > done
    });
    assert_eq!(
        GOT_WORD0.load(Ordering::Acquire),
        0x11,
        "the three-word message was wrong"
    );

    // (c) The full 128. Fill this thread's buffer; the receiver reads
    //     its own, so only a correct copy gets the values across.
    //
    // SAFETY: We are the running thread.
    let me = unsafe { sched::current() };
    {
        let buffer = me.ipc_buffer_mut();
        for (offset, word) in buffer[REGISTER_WORDS..ipc::MAX_MESSAGE_WORDS]
            .iter_mut()
            .enumerate()
        {
            *word = 0x1000_0000 + (REGISTER_WORDS + offset) as u64;
        }
    }

    start_phase_and_wait_for_block(
        0,
        "the long receiver to block",
        phase::RECV_LONG,
        ipc::receiver_blocked_count,
    );
    let done = DONE[0].load(Ordering::Acquire);
    // SAFETY: As above.
    unsafe {
        ipc::send(
            ep,
            &Message {
                tag: MessageTag::new(LABEL_LONG, ipc::MAX_MESSAGE_WORDS, 0),
                registers: [0xF00D, 0xBEEF, 0xCAFE],
                caps: &[],
            },
            &space,
        )
    }
    .expect("the long message failed");
    spin_until("the long message to be verified", || {
        DONE[0].load(Ordering::Acquire) > done
    });

    assert_eq!(
        GOT_MISMATCHES.load(Ordering::Acquire),
        0,
        "the receiver found wrong words in a {}-word message",
        ipc::MAX_MESSAGE_WORDS
    );

    println!(
        "[ipc] lengths: 0, {} and {} words all delivered intact; the \
         receiver checked every word of the longest",
        REGISTER_WORDS,
        ipc::MAX_MESSAGE_WORDS
    );
}

/// 4b. Every tag field a sender controls is checked.
///
/// Four of ADR 034's five validations are reachable in M9a; the fifth
/// (`caps > 4`) is too, because the tag is checked before anything
/// looks at whether capability transfer is implemented.
///
/// ADR 023's K5: a check nothing reaches is indistinguishable from its
/// own absence.
fn check_tag_validation() {
    let ep = slot(SLOT_ENDPOINT);
    let space = boot::init_cspace();

    let cases: [(&str, MessageTag, u64); 5] = [
        ("reserved low bits", MessageTag::from_bits(0b0001), 9),
        ("reserved high bits", MessageTag::from_bits(1 << 21), 9),
        (
            "IS_NOTIFICATION set by a sender",
            MessageTag::from_bits(1 << 20),
            9,
        ),
        ("length past the buffer", MessageTag::new(0, 129, 0), 6),
        ("more than four caps", MessageTag::new(0, 0, 5), 7),
    ];

    for (name, tag, expected) in cases {
        let message = Message {
            tag,
            registers: [0; REGISTER_WORDS],
            caps: &[],
        };
        // SAFETY: Live Endpoint and CSpace.
        match unsafe { ipc::send(ep, &message, &space) } {
            Err(e) if error_number(e) == expected => {}
            Err(e) => panic!("{} gave {}, which is not the expected error", name, e),
            Ok(()) => panic!("{} was accepted; that is not a refusal", name),
        }
    }

    println!(
        "[ipc] tag: all 5 invalid fields refused, each by its own check -- \
         reserved low, reserved high, IS_NOTIFICATION, length > {}, caps > {}",
        ipc::MAX_MESSAGE_WORDS,
        ipc::MAX_TRANSFER_CAPS
    );
}

/// 5. Three senders queue up and are delivered in FIFO order.
fn check_fifo_order() {
    // Each worker sends its own id. They block in order, so the
    // receiver must see 0, 1, 2.
    for id in 0..3 {
        start_phase_and_wait_for_block(
            id,
            "a sender to join the queue",
            phase::SEND_ORDERED,
            ipc::sender_blocked_count,
        );
    }

    let ep = slot(SLOT_ENDPOINT);
    // SAFETY: The slot holds a live Endpoint, and interrupts are
    // disabled while the length is read so the queue cannot change
    // underneath it.
    let queued = {
        let _guard = sched::InterruptGuard::new();
        // SAFETY: As above.
        let endpoint = unsafe { ipc::endpoint_from_slot(ep) }.expect("not an Endpoint");
        endpoint.queue_length()
    };
    assert_eq!(queued, 3, "three senders should be queued, not {}", queued);

    for expected in 0..3u64 {
        // SAFETY: Live Endpoint; no other borrow of this TCB.
        let delivery = unsafe { ipc::recv(ep) }.expect("a queued message did not arrive");
        assert_eq!(
            delivery.registers[0], expected,
            "the queue delivered sender {} where {} was expected; it is \
             not FIFO",
            delivery.registers[0], expected
        );
    }

    println!(
        "[ipc] fifo: three senders queued and were delivered 0, 1, 2 -- in \
         the order they arrived (ADR 033)"
    );
}

// ---- Negative scenarios ------------------------------------------------

/// `ipc-cancel-cost`: cancellation is O(1), shown at two lengths.
///
/// **The measurement that carries M9a.** One figure would not
/// demonstrate O(1); two figures ten times apart, agreeing, do. If the
/// queue were singly linked — as M6's ready queue is, and as ADR 033
/// originally said this one would be — the second figure would be
/// about ten times the first.
///
/// The reason that matters is not speed. It is that an endpoint's
/// queue length is *an adversary's* thread count, and the walk happens
/// with interrupts disabled.
pub fn run_cancel_cost() -> ! {
    println!(
        "[ipc] cancel-cost: removing one waiter from queues of {} and {}",
        CANCEL_SHORT, CANCEL_LONG
    );

    make_endpoint(SLOT_ENDPOINT);

    let short = measure_cancellation(CANCEL_SHORT);
    let long = measure_cancellation(CANCEL_LONG);

    println!(
        "[ipc] cancel-cost: {} cycles with {} waiting, {} cycles with {} \
         waiting",
        short, CANCEL_SHORT, long, CANCEL_LONG
    );

    // The claim is "the same order of magnitude", not "identical":
    // this is a virtual TSC under emulation, and a cache miss on the
    // longer queue's neighbours is real. A factor of four is generous
    // for O(1) and nowhere near the factor of ten a linear walk would
    // show.
    let ratio_numerator = long.max(short);
    let ratio_denominator = short.min(long).max(1);
    assert!(
        ratio_numerator <= ratio_denominator * 4,
        "cancelling from a {}-thread queue cost {} cycles against {} for \
         a {}-thread queue -- a factor of {}. That is a linear walk, not \
         the O(1) ADR 033 requires",
        CANCEL_LONG,
        long,
        short,
        CANCEL_SHORT,
        ratio_numerator / ratio_denominator
    );

    println!(
        "[ipc] cancel-cost: the two figures agree within a factor of {}, \
         so the cost does not depend on the queue's length. A singly \
         linked queue would have shown about 10x -- and that length is \
         an adversary's thread count, with interrupts disabled (ADR 033)",
        ratio_numerator / ratio_denominator
    );

    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        qemu::EXIT_SUCCESS,
        qemu::HOST_STATUS_SUCCESS
    );
    qemu::exit(qemu::EXIT_SUCCESS)
}

/// Queue `count` threads on the endpoint and time removing a middle
/// one.
///
/// The threads are TCB *objects* queued by hand rather than running
/// threads with stacks. Five hundred real threads would need five
/// hundred KSTACK slots and two thousand frames, and none of that is
/// what the measurement is about: the question is what **one queue
/// operation** costs, and the queue holds TCBs whether or not they
/// have ever run.
///
/// The TCBs go into a CNode retyped for the purpose, because the
/// object CNode has 256 slots and the fixtures already use some.
fn measure_cancellation(count: usize) -> u64 {
    // Radix large enough for `count`, and a separate Untyped per run
    // so the second measurement does not inherit the first's
    // watermark.
    let radix: u8 = if count <= 256 { 9 } else { 10 };
    let scratch_slot = if count == CANCEL_SHORT {
        1usize
    } else {
        2usize
    };

    // A CNode of `1 << radix` 32-byte slots, plus `count` TCBs of
    // 2 KiB each, plus alignment. Asked for as one figure, because
    // retype draws both from the same Untyped and running out halfway
    // is exactly the failure this helper exists to prevent.
    let needed = ((crate::cap::SLOT_SIZE as u64) << radix)
        + (count as u64) * crate::cap::TCB_SIZE as u64
        + 64 * 1024;

    let outer = object_cnode();
    // SAFETY: The CSpace is built. Panics with a diagnosis rather
    // than handing back something too small.
    let parent = unsafe { boot::root_untyped_with_room(needed) };

    // SAFETY: Live Untyped and CNode; the destination slot is empty
    // (a different one for each of the two runs).
    unsafe { untyped::retype(parent, CapType::CNode, radix, 1, &outer, scratch_slot) }
        .expect("could not retype a CNode for the measurement");

    // SAFETY: The slot now holds a CNode of `1 << radix` slots.
    let scratch = unsafe { CNode::new(outer.slot(scratch_slot).object().0, radix) };

    // The TCBs, in batches: `retype` takes at most 256 per call
    // (ADR 020).
    let mut made = 0usize;
    while made < count {
        let batch = (count - made).min(crate::cap::RETYPE_MAX_COUNT);
        // SAFETY: Live Untyped and CNode; the destination range is
        // inside the CNode and every slot in it is empty.
        unsafe { untyped::retype(parent, CapType::Tcb, 0, batch, &scratch, made) }
            .expect("could not retype the measurement's TCBs");
        made += batch;
    }

    // Queue them, time the removal of the middle one, then drain.
    //
    // Queued directly rather than through `recv`, because `recv`
    // blocks the *calling* thread and there is only one of those.
    // What is being measured is `Endpoint::remove`, which is the same
    // code on either path.
    let _guard = sched::InterruptGuard::new();

    // SAFETY: The slot holds a live Endpoint and interrupts are off.
    let endpoint =
        unsafe { ipc::endpoint_from_slot(slot(SLOT_ENDPOINT)) }.expect("not an Endpoint");

    for index in 0..count {
        // SAFETY: Index in range.
        let tcb_slot = unsafe { scratch.slot(index) };
        // SAFETY: The slot holds a TCB that retype zeroed just now,
        // and nothing else borrows it.
        let thread = unsafe { tcb::tcb_at(tcb_slot.object()) };
        // SAFETY: Interrupts disabled; the thread is live and on no
        // queue, because it has never run.
        unsafe {
            ipc::queue_for_measurement(endpoint, thread);
        }
    }

    // Time many removals and keep the **fastest**.
    //
    // Not one removal, and not an average, and two broken attempts
    // are why. The first timed a single removal: 108066 cycles for a
    // queue of 50 against 336 for a queue of 500 — the short queue
    // appearing three hundred times *slower*, which is not a cost
    // model at all. Warming up on one neighbouring thread brought it
    // to 24696 against 198, and a re-run of the same binary gave
    // 101094 against 546.
    //
    // Those numbers are not measuring `remove`. They are measuring
    // the first touch of memory `retype` has just zeroed — a page
    // walk and a cache fill per TCB — plus whatever the emulator's
    // virtual TSC felt like reporting. The work itself is a handful
    // of instructions, far below that floor.
    //
    // The minimum over many samples is the standard answer: noise
    // only ever adds, so the smallest observation is the one closest
    // to the real cost. An average would be dominated by the
    // outliers, which is exactly what went wrong above.
    //
    // Worth recording plainly, because the broken experiment failed
    // in the direction nobody inspects. A ratio of one-to-three-
    // hundred reads as a spectacular result rather than as a
    // mistake; only the assertion being symmetric (`max/min` rather
    // than `long/short`) caught it. A one-sided check would have
    // passed and the figure would have gone into an ADR.
    const SAMPLES: usize = 16;
    let mut best = u64::MAX;

    for sample in 0..SAMPLES {
        // A different victim each time, all from the middle of the
        // queue so none of them is the head or the tail -- those are
        // the two cases with a shorter code path.
        let index = count / 4 + sample;
        // SAFETY: `count / 4 + 16` is below `count` for both queue
        // lengths this runs with.
        let victim_slot = unsafe { scratch.slot(index) };
        // SAFETY: The slot holds a live TCB queued above.
        let victim = unsafe { tcb::tcb_at(victim_slot.object()) };

        // Touch the links before the clock starts. `remove` reads
        // both and writes to both neighbours; that list surgery is
        // the work being measured, and the page walk to reach it is
        // not.
        core::hint::black_box(victim.ipc_prev_for_measurement());
        core::hint::black_box(victim.ipc_next_for_measurement());

        let start = read_tsc();
        // SAFETY: Interrupts disabled; the thread is live and queued.
        unsafe {
            ipc::unqueue_for_measurement(endpoint, victim);
        }
        let elapsed = read_tsc() - start;

        if elapsed < best {
            best = elapsed;
        }
    }

    let cycles = best;

    // Empty the endpoint so the next run starts clean.
    //
    // SAFETY: Interrupts disabled; the endpoint is live and every
    // thread on it was queued by this function.
    unsafe {
        ipc::drain_for_measurement(endpoint);
    }

    cycles
}

/// `ipc-endpoint-revoked`: waiters are woken, not stranded.
///
/// **The scenario that proves `cancel_ipc` is wired in.** Three
/// threads block on an endpoint; its capability is deleted; all three
/// must wake with `EndpointDead`. If the call were missing from
/// `cnode::delete` this would hang, and a hang is the right failure
/// for a queue full of threads nobody can reach.
///
/// Woken rather than the delete being refused, and the reasoning is
/// ADR 039's rather than ADR 026's: the waiters may belong to *other
/// components*, so refusing would let any of them make an endpoint
/// un-revokable.
pub fn run_endpoint_revoked() -> ! {
    println!("[ipc] endpoint-revoked: three threads waiting, then the capability goes");

    make_endpoint(SLOT_ENDPOINT);

    for id in 0..3 {
        create_worker(id, blocker);
    }
    spin_until("three threads to block on the endpoint", || {
        ipc::receiver_blocked_count() >= 3
    });

    let queued = {
        let _guard = sched::InterruptGuard::new();
        // SAFETY: The slot holds a live Endpoint and interrupts are
        // disabled.
        let endpoint =
            unsafe { ipc::endpoint_from_slot(slot(SLOT_ENDPOINT)) }.expect("not an Endpoint");
        endpoint.queue_length()
    };
    assert_eq!(
        queued, 3,
        "three threads should be queued before the revoke, not {}",
        queued
    );

    // Delete the capability. `release_ipc_references` is what has to
    // run here, and if it does not the three threads stay queued
    // forever.
    //
    // SAFETY: The slot holds a live Endpoint capability.
    unsafe { cnode::delete(slot(SLOT_ENDPOINT)) }.expect("delete failed");

    spin_until("all three waiters to wake", || {
        DELIVERY_ORDER.load(Ordering::Acquire) >= 3
    });

    assert_eq!(
        GOT_ERROR.load(Ordering::Acquire),
        1,
        "a woken waiter reported something other than EndpointDead"
    );
    assert_eq!(
        GOT_WORD0.load(Ordering::Acquire),
        0,
        "a waiter received a message from an endpoint that was deleted"
    );

    println!(
        "[ipc] endpoint-revoked: all 3 waiters woke with EndpointDead and \
         none received a message; {} cancellations recorded",
        ipc::cancellations()
    );
    println!(
        "[ipc] endpoint-revoked: the kernel is still running. Refusing the \
         delete would have been safe for its owner and a denial of \
         service for everyone else -- the waiters are other components' \
         threads (ADR 039)"
    );

    ipc::report();
    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        qemu::EXIT_SUCCESS,
        qemu::HOST_STATUS_SUCCESS
    );
    qemu::exit(qemu::EXIT_SUCCESS)
}

// ---- M9b: Call, Reply, ReplyRecv and badges ---------------------------

/// Give worker `id` a badged capability to the endpoint (ADR 037).
///
/// `mint` rather than `copy`, because `copy` carries the badge across
/// unchanged and the source has none. Minting needs `GRANT`: attaching
/// a badge creates a new identity in the server's eyes, which ADR 021
/// counts as producing authority.
fn mint_badged_endpoint(id: usize) {
    let cnode = object_cnode();
    // SAFETY: Indices are below the CNode's radix of 8 (256 slots).
    let (src, dest) = unsafe { (cnode.slot(SLOT_ENDPOINT), cnode.slot(badged_slot_for(id))) };
    // SAFETY: The source holds a live Endpoint with full rights and
    // the destination is empty.
    unsafe { cnode::mint(src, dest, crate::cap::Rights::ALL, badge_for(id)) }
        .expect("could not mint a badged endpoint capability");
}

/// `ipc-call-test`: the request-response pattern.
///
/// Four scenarios, and the fourth is the one with teeth.
pub fn run_call() -> ! {
    println!("[ipc] M9b: Call, Reply, ReplyRecv and badges (ADR 036, ADR 037)");

    make_endpoint(SLOT_ENDPOINT);

    // Four workers: two clients, two servers. The second server only
    // matters in the stealing scenario, where its job is to arrive at
    // the wrong moment and come away with nothing.
    for id in 0..4 {
        create_worker(id, worker);
    }

    check_call_reply_round_trip();
    check_reply_recv_is_one_syscall();
    check_badges_keep_clients_apart();
    check_reply_is_not_stolen();

    ipc::report();
    println!("[ipc] all M9b call/reply tests passed");

    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        qemu::EXIT_SUCCESS,
        qemu::HOST_STATUS_SUCCESS
    );
    qemu::exit(qemu::EXIT_SUCCESS)
}

/// 1. A `Call` goes out, a `Reply` comes back, and the client checks it.
fn check_call_reply_round_trip() {
    let calls_before = ipc::calls();
    let replies_before = ipc::replies_sent();

    // Server first, so it is already waiting when the client calls.
    start_phase_and_wait_for_block(
        1,
        "the server to wait for a request",
        phase::SERVE_ONCE,
        ipc::receiver_blocked_count,
    );

    let done = DONE[0].load(Ordering::Acquire);
    PHASES[0].store(phase::CALL, Ordering::Release);
    spin_until("the client's Call to return", || {
        DONE[0].load(Ordering::Acquire) > done
    });

    assert_eq!(
        REPLY_ERROR[0].load(Ordering::Acquire),
        0,
        "the client's Call returned an error"
    );
    // The reply must be the value the *server* computed, which is
    // derived from the badge. An unbadged capability carries badge 0,
    // so this is `reply_value_for(0)` -- and checking the exact value
    // rather than "something arrived" is what rules out a reply that
    // was never written.
    assert_eq!(
        REPLY_WORD[0].load(Ordering::Acquire),
        reply_value_for(0),
        "the client got the wrong reply"
    );

    assert_eq!(
        ipc::calls() - calls_before,
        1,
        "the Call was not counted as one"
    );
    assert_eq!(
        ipc::replies_sent() - replies_before,
        1,
        "the Reply was not delivered to a waiting caller"
    );

    println!(
        "[ipc] call/reply: the round trip completed -- client sent, server \
         answered {:#018x}, client verified it; {} calls and {} replies so \
         far",
        REPLY_WORD[0].load(Ordering::Acquire),
        ipc::calls(),
        ipc::replies_sent()
    );
}

/// 2. `ReplyRecv` does two operations in one kernel entry.
///
/// The claim is about *syscall count*, so that is what is counted. A
/// test that only checked "the reply arrived and a request came back"
/// would pass just as well on a server making two separate calls --
/// which is precisely the thing ADR 036 says `ReplyRecv` exists to
/// avoid.
fn check_reply_recv_is_one_syscall() {
    let reply_recvs_before = ipc::reply_recvs();
    let replies_before = ipc::replies_sent();

    // A server holding a request, so that its ReplyRecv has something
    // to answer.
    start_phase_and_wait_for_block(
        1,
        "the server to take a request",
        phase::SERVE_SLOWLY,
        ipc::receiver_blocked_count,
    );
    SERVER_HOLDS_REQUEST.store(0, Ordering::Release);
    SERVER_MAY_REPLY.store(0, Ordering::Release);

    let client_done = DONE[0].load(Ordering::Acquire);
    PHASES[0].store(phase::CALL, Ordering::Release);
    spin_until("the server to hold the request", || {
        SERVER_HOLDS_REQUEST.load(Ordering::Acquire) != 0
    });

    // Release it. The server replies and returns; the client wakes.
    SERVER_MAY_REPLY.store(1, Ordering::Release);
    spin_until("the client to get its reply", || {
        DONE[0].load(Ordering::Acquire) > client_done
    });

    assert_eq!(
        REPLY_ERROR[0].load(Ordering::Acquire),
        0,
        "the client's Call failed during the ReplyRecv check"
    );

    // Now the measurement that matters: a ReplyRecv round, counted.
    let calls_before = ipc::calls();
    let server_done = DONE[1].load(Ordering::Acquire);

    // The server answers the next request and waits for one more, in
    // a single invocation.
    let ep = slot(SLOT_ENDPOINT);
    let space = boot::init_cspace();

    // This thread plays the server for one exchange, so the count is
    // attributable: a worker thread's syscalls would be mixed in with
    // its loop's.
    let client2_done = DONE[2].load(Ordering::Acquire);
    PHASES[2].store(phase::CALL, Ordering::Release);
    spin_until("a second client to call", || ipc::calls() > calls_before);

    // SAFETY: Live Endpoint; this thread holds no other borrow of its
    // own TCB.
    let request = unsafe { ipc::recv(ep) }.expect("the server did not get the request");
    assert_eq!(
        request.tag.label(),
        LABEL_PING,
        "the request carried the wrong label"
    );

    // One invocation: answer this client and wait for the next.
    //
    // There is no next, so this would block -- which is why the third
    // client is started first and why the reply half is what is being
    // counted.
    let answer = Message {
        tag: MessageTag::new(LABEL_PONG, REGISTER_WORDS, 0),
        registers: [reply_value_for(request.badge), 0, 0],
        caps: &[],
    };

    PHASES[3].store(phase::CALL, Ordering::Release);

    // SAFETY: Live Endpoint and this thread's own CSpace.
    let next = unsafe { ipc::reply_recv(&answer, ep, &space) }
        .expect("ReplyRecv failed to collect the next request");
    assert_eq!(
        next.tag.label(),
        LABEL_PING,
        "ReplyRecv's receive half returned the wrong message"
    );

    // Answer the one it collected, so nobody is left blocked.
    let answer2 = Message {
        tag: MessageTag::new(LABEL_PONG, REGISTER_WORDS, 0),
        registers: [reply_value_for(next.badge), 0, 0],
        caps: &[],
    };
    // SAFETY: As above.
    unsafe { ipc::reply(&answer2, &space) }.expect("the final reply failed");

    spin_until("both clients to finish", || {
        DONE[2].load(Ordering::Acquire) > client2_done && DONE[3].load(Ordering::Acquire) > 0
    });

    let reply_recvs = ipc::reply_recvs() - reply_recvs_before;
    let replies = ipc::replies_sent() - replies_before;

    assert_eq!(
        reply_recvs, 1,
        "expected exactly one ReplyRecv, saw {}",
        reply_recvs
    );
    assert!(
        replies >= 3,
        "only {} replies were delivered; the ReplyRecv round did not \
         complete",
        replies
    );

    let _ = server_done;

    println!(
        "[ipc] replyrecv: one invocation both answered a client and \
         collected the next request -- {} ReplyRecv against {} replies \
         delivered. Two separate calls would have cost one more kernel \
         entry, measured at ~934 cycles in M8 (ADR 036)",
        ipc::reply_recvs(),
        ipc::replies_sent()
    );
}

/// 3. Two clients, one server, and the replies do not cross.
///
/// The badge is what makes this answerable: a client cannot say who it
/// is, because it could lie (ADR 037). The server reads the badge the
/// kernel attached and derives its answer from it, so a reply arriving
/// at the wrong client is visible in that client's own check.
fn check_badges_keep_clients_apart() {
    mint_badged_endpoint(0);
    mint_badged_endpoint(2);

    REPLY_WORD[0].store(0, Ordering::Release);
    REPLY_WORD[2].store(0, Ordering::Release);
    REPLY_ERROR[0].store(0, Ordering::Release);
    REPLY_ERROR[2].store(0, Ordering::Release);

    // Both clients call. They queue on the endpoint in order.
    let done0 = DONE[0].load(Ordering::Acquire);
    let done2 = DONE[2].load(Ordering::Acquire);
    start_phase_and_wait_for_block(0, "client 0 to call", phase::CALL_BADGED, || {
        ipc::sender_blocked_count()
    });
    start_phase_and_wait_for_block(2, "client 2 to call", phase::CALL_BADGED, || {
        ipc::sender_blocked_count()
    });

    // Serve both, from this thread, so the badges are checked here.
    let ep = slot(SLOT_ENDPOINT);
    let space = boot::init_cspace();
    let mut seen = [0u32; 2];

    for (index, seen_badge) in seen.iter_mut().enumerate() {
        // SAFETY: Live Endpoint; no other borrow of this TCB.
        let request = unsafe { ipc::recv(ep) }.expect("a badged request did not arrive");
        *seen_badge = request.badge;

        let answer = Message {
            tag: MessageTag::new(LABEL_PONG, REGISTER_WORDS, 0),
            registers: [reply_value_for(request.badge), 0, 0],
            caps: &[],
        };
        // SAFETY: This thread's own CSpace.
        unsafe { ipc::reply(&answer, &space) }
            .unwrap_or_else(|e| panic!("reply {} failed: {}", index, e));
    }

    spin_until("both badged clients to return", || {
        DONE[0].load(Ordering::Acquire) > done0 && DONE[2].load(Ordering::Acquire) > done2
    });

    // The badges the server saw were the two that were minted, and
    // they were different from each other.
    assert_ne!(
        seen[0], seen[1],
        "both requests carried badge {:#x}; the server cannot tell its \
         clients apart",
        seen[0]
    );
    assert!(
        (seen[0] == badge_for(0) && seen[1] == badge_for(2))
            || (seen[0] == badge_for(2) && seen[1] == badge_for(0)),
        "the server saw badges {:#x} and {:#x}, which are not the two \
         that were minted ({:#x} and {:#x})",
        seen[0],
        seen[1],
        badge_for(0),
        badge_for(2)
    );

    // **And each client got the answer meant for it.** This is the
    // check that would fail if the two replies crossed -- every other
    // line above would still pass.
    assert_eq!(
        REPLY_WORD[0].load(Ordering::Acquire),
        reply_value_for(badge_for(0)),
        "client 0 got the reply meant for another client"
    );
    assert_eq!(
        REPLY_WORD[2].load(Ordering::Acquire),
        reply_value_for(badge_for(2)),
        "client 2 got the reply meant for another client"
    );

    println!(
        "[ipc] badge: two clients on one endpoint carried {:#x} and {:#x}; \
         the server told them apart and each got its own answer \
         ({:#018x} and {:#018x}) -- the identity is the kernel's, not the \
         client's claim (ADR 037)",
        badge_for(0),
        badge_for(2),
        REPLY_WORD[0].load(Ordering::Acquire),
        REPLY_WORD[2].load(Ordering::Acquire)
    );
}

/// 4. A second server does not steal the first server's caller.
///
/// **ADR 036's finest decision, and the one that is easiest to get
/// wrong.** `reply_to` is written at the *rendezvous*, not when `Recv`
/// returns. If it were written later, this sequence would hand the
/// caller to whichever server asked second:
///
/// ```text
///   client   Call(ep)              -> queues
///   server A Recv(ep)              -> takes the request
///   (A is preempted before Reply)
///   server B Recv(ep)              -> must NOT get A's caller
///   server A Reply(...)            -> must reach the client
/// ```
///
/// # Why this is not a timing test
///
/// "A is preempted before Reply" is the hard part, and leaving it to
/// the timer would make the scenario measure luck. On a run where A
/// happened to finish within its slice, the test would pass without
/// ever creating the situation it names — and ADR 023's K8 was added
/// in M9a for exactly this failure.
///
/// So the gap is *constructed*: server A stops between `Recv` and
/// `Reply` and waits for a flag this function sets. The window is
/// therefore as long as the test wants, and the second server's `Recv`
/// is guaranteed to land inside it. The scenario is deterministic and
/// would fail on every run of a kernel that wrote `reply_to` late.
fn check_reply_is_not_stolen() {
    REPLY_WORD[0].store(0, Ordering::Release);
    REPLY_ERROR[0].store(0, Ordering::Release);
    SERVED_BADGE[1].store(0, Ordering::Release);
    SERVED_BADGE[3].store(0, Ordering::Release);
    REPLY_ERROR[3].store(0, Ordering::Release);
    SERVER_HOLDS_REQUEST.store(0, Ordering::Release);
    SERVER_MAY_REPLY.store(0, Ordering::Release);

    // Server A waits for a request and will hold it.
    start_phase_and_wait_for_block(
        1,
        "server A to wait",
        phase::SERVE_SLOWLY,
        ipc::receiver_blocked_count,
    );

    // The client calls. A takes the request and stops.
    let client_done = DONE[0].load(Ordering::Acquire);
    PHASES[0].store(phase::CALL, Ordering::Release);
    spin_until("server A to be holding the request", || {
        SERVER_HOLDS_REQUEST.load(Ordering::Acquire) != 0
    });

    // **The window.** A is between its Recv and its Reply, and it will
    // stay there until released. Server B now arrives.
    let b_done = DONE[3].load(Ordering::Acquire);
    start_phase_and_wait_for_block(
        3,
        "server B to block on the endpoint",
        phase::RECV_EXPECT_NOTHING,
        ipc::receiver_blocked_count,
    );

    // B must be *blocked*, not holding anything: the endpoint is empty
    // because the client's request was already taken, and the client
    // is in BlockedOnReply, which is in no queue at all (ADR 033).
    assert_eq!(
        DONE[3].load(Ordering::Acquire),
        b_done,
        "server B returned from Recv while server A was still holding \
         the only request; B took A's caller"
    );

    // Release A. It replies to the caller it was given.
    SERVER_MAY_REPLY.store(1, Ordering::Release);
    spin_until("the client to receive A's reply", || {
        DONE[0].load(Ordering::Acquire) > client_done
    });

    assert_eq!(
        REPLY_ERROR[0].load(Ordering::Acquire),
        0,
        "the client's Call failed; server A could not answer the caller \
         it was holding"
    );
    assert_eq!(
        REPLY_WORD[0].load(Ordering::Acquire),
        reply_value_for(0),
        "the client got {:#018x} rather than server A's answer",
        REPLY_WORD[0].load(Ordering::Acquire)
    );

    // B is still waiting, having stolen nothing. Wake it so the
    // scenario can exit: deleting the endpoint would do it, but a
    // Send is less disruptive and proves B's Recv still works.
    let ep = slot(SLOT_ENDPOINT);
    let space = boot::init_cspace();
    // SAFETY: Live Endpoint and this thread's CSpace.
    unsafe {
        ipc::send(
            ep,
            &Message {
                tag: MessageTag::new(LABEL_PING, 0, 0),
                registers: [0; REGISTER_WORDS],
                caps: &[],
            },
            &space,
        )
    }
    .expect("could not wake server B");
    spin_until("server B to finish", || {
        DONE[3].load(Ordering::Acquire) > b_done
    });

    println!(
        "[ipc] reply-to-at-rendezvous: server B called Recv while server A \
         held a caller and came away with nothing; A's reply reached the \
         right client. The window was opened by the test rather than by a \
         timer, so this fails on every run of a kernel that writes \
         reply_to late (ADR 036, ADR 023 K8)"
    );
}

// ---- M9b: negative scenarios ------------------------------------------

/// `ipc-server-dies`: a server revoked mid-transaction wakes its
/// caller.
///
/// ADR 036 distinguishes two ways a client can be left waiting, and
/// treats them differently:
///
/// * the server **declines** to answer — the client waits forever,
///   and that is accepted, because a timeout needs a scheduling
///   budget the project does not have;
/// * the server **dies** — the kernel can see the obligation, so
///   leaving the client blocked would be a defect rather than a
///   trade-off.
///
/// This is the second. Exits 33: a woken client is the success path.
pub fn run_server_dies() -> ! {
    println!("[ipc] server-dies: a server is revoked while holding a caller");

    make_endpoint(SLOT_ENDPOINT);
    create_worker(0, worker); // the client
    create_worker(1, worker); // the server

    SERVER_HOLDS_REQUEST.store(0, Ordering::Release);
    SERVER_MAY_REPLY.store(0, Ordering::Release);

    // The server takes a request and stops before answering.
    start_phase_and_wait_for_block(
        1,
        "the server to wait",
        phase::SERVE_SLOWLY,
        ipc::receiver_blocked_count,
    );
    PHASES[0].store(phase::CALL, Ordering::Release);
    spin_until("the server to be holding the caller", || {
        SERVER_HOLDS_REQUEST.load(Ordering::Acquire) != 0
    });

    // The client is now in BlockedOnReply: in no queue at all, held
    // only by the server's `reply_to` field (ADR 033).
    let client_phys = {
        let s = slot(worker_tcb_slot(0));
        s.object().0 .0
    };
    // SAFETY: The slot holds the client's live TCB.
    let client_state =
        unsafe { tcb::tcb_at(crate::cap::ObjectRef(crate::memory::PhysAddr(client_phys))) }.state();
    assert_eq!(
        client_state.name(),
        "BlockedOnReply",
        "the client is {} rather than waiting for a reply; the scenario \
         would not be testing what it claims",
        client_state.name()
    );

    // Kill the server. `release_ipc_references` must notice the debt.
    //
    // Suspend first: ADR 026 refuses to revoke a thread the scheduler
    // is holding, and the two-step sequence is what makes that
    // refusal something other than a dead end.
    {
        // SAFETY: The slot holds the server's live TCB.
        let server = unsafe { tcb::from_slot(slot(worker_tcb_slot(1))) }.expect("not a TCB");
        // SAFETY: Live TCB; this thread is not the server.
        unsafe {
            sched::suspend(server);
        }
    }
    // SAFETY: The slot holds a live TCB capability.
    unsafe { cnode::delete(slot(worker_tcb_slot(1))) }.expect("deleting the server failed");

    spin_until("the client to wake", || DONE[0].load(Ordering::Acquire) > 0);

    assert_eq!(
        REPLY_ERROR[0].load(Ordering::Acquire),
        error_number(CapError::ReplyTargetGone),
        "the client woke with error {} rather than ReplyTargetGone",
        REPLY_ERROR[0].load(Ordering::Acquire)
    );
    assert_eq!(
        REPLY_WORD[0].load(Ordering::Acquire),
        0,
        "the client received a reply from a server that never sent one"
    );

    println!(
        "[ipc] server-dies: the server was revoked holding a caller; the \
         client woke with ReplyTargetGone rather than waiting forever. \
         ADR 036 pays this case because the kernel can see the \
         obligation -- a server that merely declines to answer is a \
         different matter, and that one does wait"
    );

    ipc::report();
    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        qemu::EXIT_SUCCESS,
        qemu::HOST_STATUS_SUCCESS
    );
    qemu::exit(qemu::EXIT_SUCCESS)
}

/// `ipc-stale-reply`: a reply does not reach a stranger.
///
/// **The proof that the generation check is doing something.** The
/// client's TCB is revoked and a *new* TCB is retyped — and ADR 020's
/// watermark reset makes it land at the very same address. An
/// address-only check would then deliver the reply to whatever
/// occupies that memory now, silently.
///
/// # The K5 mutation, run rather than asserted
///
/// Deleting `&& slot.generation() == generation` from
/// `reply_target_is_live` kills this scenario: it panics at the stale
/// half of Claim 2 with "a reply target with a stale generation was
/// accepted", exits 35, and the runner reports three violations. The
/// check was then restored and the scenario passes again.
///
/// The first attempt at that mutation produced a finding of its own,
/// and it is the sort that is easy to mistake for success. The build
/// refused it — `-D warnings` turned the now-unused `generation`
/// binding into an error, so nothing was ever run. That is a real
/// line of defence and it is worth knowing it exists, but it is a
/// *weaker* one than it appears: renaming the binding to
/// `_generation` silences it in one keystroke, which is exactly what
/// an edit removing the check for some plausible-looking reason would
/// do without a second thought. The mutation was redone that way so
/// the test itself had to catch it, and it did.
///
/// The lesson generalises past this one check. "The compiler would
/// catch it" is evidence about a *particular* way of making the
/// mistake, not about the mistake.
///
/// # The test checks its own premise first
///
/// If the new TCB does not land at the old address, the scenario
/// proves nothing — the reply would be refused by the address
/// comparison alone and the generation check would never be reached.
/// So that is verified before anything is asserted, and said plainly
/// if it does not hold. M9a's measurement taught this: a test that
/// cannot tell "it worked" from "the situation never arose" reports
/// the second as the first.
pub fn run_stale_reply() -> ! {
    println!("[ipc] stale-reply: the caller's TCB is recycled under the server");

    make_endpoint(SLOT_ENDPOINT);
    // The server first, from the default Untyped; the client from a
    // *different* one. Revoking the client's Untyped is how its TCB's
    // memory comes back, and a revoke takes everything carved from
    // that Untyped -- so a shared one would kill the running server
    // as a side effect.
    create_worker(1, worker); // the server
    create_worker_from(0, worker, 1); // the client, elsewhere

    SERVER_HOLDS_REQUEST.store(0, Ordering::Release);
    SERVER_MAY_REPLY.store(0, Ordering::Release);

    start_phase_and_wait_for_block(
        1,
        "the server to wait",
        phase::SERVE_SLOWLY,
        ipc::receiver_blocked_count,
    );
    PHASES[0].store(phase::CALL, Ordering::Release);
    spin_until("the server to be holding the caller", || {
        SERVER_HOLDS_REQUEST.load(Ordering::Acquire) != 0
    });

    // What the server is holding, before anything is destroyed.
    let (old_address, old_generation) = {
        let s = slot(worker_tcb_slot(0));
        (s.object().0 .0, s.generation())
    };
    println!(
        "[ipc] stale-reply: the caller's TCB is at {:#018x}, slot \
         generation {}",
        old_address, old_generation
    );

    // Destroy the client, and get its memory *back*.
    //
    // `delete` is not enough, and the difference is ADR 020's: a
    // deleted capability stops existing, but the object's memory stays
    // spoken for -- the Untyped's watermark never moves backwards for
    // a delete. Only revoking the Untyped returns the range, and only
    // then can a retype land on the same address.
    //
    // The first version of this scenario used `delete` and the
    // replacement TCB appeared at 0x20800 against the original's
    // 0x10800. The test said so and stopped rather than passing: with
    // different addresses the reply would be refused by the address
    // comparison and the generation check -- the thing under test --
    // would never run. That is the premise check earning its place on
    // the first attempt.
    //
    // Which Untyped the client came from must therefore be known
    // rather than searched for again, so the fixture is asked once
    // and the answer reused.
    // **Not** suspended first, and that is the whole point.
    //
    // `Suspend` on a BlockedOnReply thread calls `cancel_ipc`, which
    // follows the `replying_server` back-pointer and clears the
    // server's `reply_to` on the spot. The server's later `Reply`
    // then gets `NoReplyTarget` -- "nobody ever called" -- and the
    // generation check is never reached.
    //
    // That is correct behaviour and it is ADR 028's pattern working:
    // clean up *and* validate. But it means the tidy path hides the
    // check this scenario exists to exercise. The second run showed
    // it plainly: error 3 (NoReplyTarget) where 2 (ReplyTargetGone)
    // was expected.
    //
    // So the client is destroyed *while still waiting*. Revoking the
    // Untyped deletes its TCB capability without going through
    // `Suspend`, which is also the realistic case: a component is
    // torn down by whoever holds its memory, not by asking it
    // politely. The server is then left holding a reply right whose
    // target has been recycled -- the only configuration in which
    // address-plus-generation can be seen to do its job.

    // Revoke the Untyped the workers were carved from. This deletes
    // every capability derived from it -- both workers' TCBs and
    // their stack frames -- and resets the watermark.
    //
    // SAFETY: The CSpace is built; this is the Untyped `create_worker`
    // drew from, and it is still live.
    let parent = unsafe { boot::root_untyped_with_room_from(WORKER_BYTES, 1) };
    // `revoke_untyped`, not `cnode::revoke`. The second deletes every
    // derived capability but leaves the watermark where it was, so the
    // memory stays spoken for and nothing can land on the old address
    // -- which is how the second attempt at this scenario failed, with
    // the watermark still reading 20480 after the revoke.
    //
    // Resetting the watermark is the *only* way memory comes back
    // (ADR 020), and `revoke_untyped` is the one operation that does
    // it. The distinction is exactly what this scenario depends on.
    //
    // SAFETY: A live Untyped capability, and every object carved from
    // it belongs to the client being destroyed.
    let report =
        unsafe { untyped::revoke_untyped(parent) }.expect("revoking the client's Untyped failed");
    println!(
        "[ipc] stale-reply: revoked the Untyped the client came from: \
         {} capabilities deleted. A delete would not do -- it stops the \
         capability existing but leaves the watermark where it was, so \
         nothing could land on the old address (ADR 020)",
        report.deleted
    );

    // What the revoke actually did to the Untyped, before anything is
    // retyped from it.
    //
    // Printed rather than assumed, because the first two attempts at
    // this scenario both failed on the address and the logs could not
    // say *why*: 0x20800 is not the start of any Untyped, so the
    // client's TCB had something ahead of it, and the question is
    // whether the revoke put the watermark back to zero or merely
    // somewhere earlier.
    {
        // SAFETY: A root slot holds a live Untyped capability.
        let untyped = unsafe { untyped::Untyped::from_slot(parent) }.expect("not an Untyped");
        println!(
            "[ipc] stale-reply: the client's Untyped covers {:#018x}, \
             {} bytes, watermark now {} after the revoke",
            untyped.base().0,
            untyped.size(),
            untyped.watermark()
        );
    }

    let new_generation = slot(worker_tcb_slot(0)).generation();
    assert_ne!(
        new_generation, old_generation,
        "revoking did not advance the slot's generation; the whole \
         mechanism rests on that increment (ADR 018)"
    );

    // Put a *new* TCB in the same slot, from the same Untyped.
    let cnode = object_cnode();
    // SAFETY: Live Untyped and CNode; the slot is empty again.
    unsafe { untyped::retype(parent, CapType::Tcb, 0, 1, &cnode, worker_tcb_slot(0)) }
        .expect("could not retype a replacement TCB");

    let new_address = slot(worker_tcb_slot(0)).object().0 .0;

    // **The premise, checked before the claim.**
    //
    // The reply must be refused *by the generation*, which can only be
    // demonstrated if the address matches. If it does not, the address
    // comparison would refuse the reply on its own and this scenario
    // would be a tautology -- so it says so rather than passing.
    assert_eq!(
        new_address, old_address,
        "the replacement TCB landed at {:#018x} rather than the old \
         address {:#018x}. The scenario cannot demonstrate anything: \
         the reply would be refused by the address comparison alone, \
         and the generation check -- the thing under test -- would \
         never be reached. Revoking should reset the Untyped's \
         watermark (ADR 020), so this not holding is itself a finding",
        new_address, old_address
    );
    println!(
        "[ipc] stale-reply: the replacement TCB landed at the SAME \
         address {:#018x}, generation {} -> {}. An address-only check \
         would now deliver the reply to a stranger",
        new_address, old_generation, new_generation
    );

    // **Two claims, and they have to be separated.**
    //
    // When the client's TCB was revoked, `cancel_ipc` followed the
    // `replying_server` back-pointer and cleared the server's
    // `reply_to` on the spot. So the server's `Reply` is refused by
    // the *first* of ADR 036's three comparisons -- "is there a reply
    // target at all" -- and returns `NoReplyTarget`.
    //
    // That is the right answer and the better one: the server is told
    // there is nobody to answer rather than being told its target
    // went stale. But it means the **generation** comparison is not
    // what refused this reply, and claiming otherwise would be the
    // kind of test that passes for the wrong reason.
    //
    // So the two are checked separately:
    //
    //  1. the live path, here: the reply is refused and the stranger
    //     occupying the old address is not woken;
    //  2. the generation check itself, below, by asking
    //     `reply_target_is_live` directly with a reply target that
    //     was *not* cleaned up -- which is what a second holder of
    //     the same right would look like.
    //
    // ADR 028 called this belt and braces. The cleanup reaches the
    // one server the back-pointer names; the generation check is what
    // stands behind it.
    let stale_before = ipc::stale_replies();
    SERVER_MAY_REPLY.store(1, Ordering::Release);
    spin_until("the server to attempt its reply", || {
        DONE[1].load(Ordering::Acquire) > 0
    });

    let refusal = REPLY_ERROR[1].load(Ordering::Acquire);
    assert_eq!(
        refusal,
        error_number(CapError::NoReplyTarget),
        "the server's Reply returned {} rather than NoReplyTarget. The \
         back-pointer should have cleared reply_to when the client was \
         revoked (ADR 039), so the first of the three comparisons is \
         the one that refuses",
        refusal
    );
    let _ = stale_before;

    println!(
        "[ipc] stale-reply: the Reply was refused with NoReplyTarget -- \
         the back-pointer cleared the server's reply_to when the client \
         died, so the first comparison caught it and the generation \
         never had to (ADR 039's two-way link)"
    );

    // And the new TCB was not disturbed: it never ran, so it is still
    // Inactive. A reply delivered to it would have made it runnable.
    // SAFETY: The slot holds the replacement TCB.
    let replacement = unsafe { tcb::from_slot(slot(worker_tcb_slot(0))) }.expect("not a TCB");
    assert_eq!(
        replacement.state().name(),
        "Inactive",
        "the replacement TCB is {}; the reply reached a thread it was \
         never meant for",
        replacement.state().name()
    );

    // ---- Claim 2: the generation comparison itself ----------------
    //
    // The cleanup above reaches the one server the back-pointer
    // names. The generation check is what stands behind it, and it
    // has to be exercised on its own -- ADR 023's K5: a guard that
    // nothing reaches is indistinguishable from its own absence.
    //
    // So a reply target is written by hand, naming the address the
    // client used and the generation it had *before* the slot was
    // reused. That is exactly the state a holder of the same right
    // would be left in if the back-pointer had not reached it.
    //
    // The server is used as the subject because it is a live TCB
    // that currently owes nothing.
    {
        let _guard = sched::InterruptGuard::new();

        // SAFETY: The slot holds the server's live TCB and this
        // thread is not it.
        let server = unsafe { tcb::from_slot(slot(worker_tcb_slot(1))) }.expect("not a TCB");
        let client_slot_phys = crate::memory::PhysAddr(
            core::ptr::from_ref(slot(worker_tcb_slot(0))) as u64 - crate::memory::pmap_base(),
        );

        // First with the generation the slot has *now*, to show the
        // check is not simply refusing everything: same address, same
        // generation, and a TCB in the slot -- so all three
        // comparisons hold and the target reads as live.
        //
        // SAFETY: A live TCB address and the slot that holds it.
        unsafe {
            server.set_reply_target(new_address, client_slot_phys, new_generation);
        }
        assert!(
            server.reply_target_is_live(),
            "a reply target naming a live TCB with the current \
             generation was rejected; the check refuses everything and \
             would pass the stale case for the wrong reason"
        );
        server.clear_reply_target();
        replacement.set_replying_server(0);

        // And now the stale one: the *same address*, the *old*
        // generation. Only the generation differs, so only the
        // generation can refuse it.
        //
        // SAFETY: As above.
        unsafe {
            server.set_reply_target(new_address, client_slot_phys, old_generation);
        }
        assert!(
            !server.reply_target_is_live(),
            "a reply target with a stale generation was accepted. The \
             address matches a live TCB and the slot holds a TCB, so \
             the first two comparisons pass -- the generation is the \
             only thing standing between this reply and a thread it \
             was never meant for (ADR 036)"
        );
        server.clear_reply_target();
        replacement.set_replying_server(0);
    }

    println!(
        "[ipc] stale-reply: the generation check, exercised directly: \
         generation {} is accepted and {} is refused at the SAME \
         address {:#018x}. Only the generation differs between the two, \
         so it is the comparison doing the work -- an address-only \
         check would accept both (ADR 036, ADR 018's idiom)",
        new_generation, old_generation, new_address
    );

    println!(
        "[ipc] stale-reply: the live path gave NoReplyTarget and the new \
         TCB at the same address is untouched (still Inactive). Two \
         guards, exercised separately because only one of them fires on \
         the live path: the back-pointer cleared the reply right when \
         the client died, and behind it the generation refuses the same \
         address at the old generation. Cleanup and validation -- \
         ADR 028's pattern"
    );

    ipc::report();
    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        qemu::EXIT_SUCCESS,
        qemu::HOST_STATUS_SUCCESS
    );
    qemu::exit(qemu::EXIT_SUCCESS)
}

/// `ipc-self-call`: calling your own endpoint deadlocks, and `Suspend`
/// is the way out.
///
/// ADR 036 declines to prevent this. A check for "am I the receiver"
/// would catch only the direct case and miss A→B→A, so it would be
/// partial protection at full cost. What the design promises instead
/// is that the deadlock is *escapable* — and that promise is what this
/// scenario tests.
pub fn run_self_call() -> ! {
    println!("[ipc] self-call: a thread calls its own endpoint");

    make_endpoint(SLOT_ENDPOINT);
    create_worker(0, worker);

    // The worker calls an endpoint nobody is receiving on. It blocks
    // in BlockedOnEndpoint -- not BlockedOnReply, because no
    // rendezvous ever happened.
    start_phase_and_wait_for_block(0, "the self-caller to block", phase::CALL, || {
        ipc::sender_blocked_count()
    });

    // SAFETY: The slot holds the worker's live TCB.
    let caller = unsafe { tcb::from_slot(slot(worker_tcb_slot(0))) }.expect("not a TCB");
    assert_eq!(
        caller.state().name(),
        "BlockedOnEndpoint",
        "the self-caller is {} rather than queued on the endpoint; a Call \
         with no receiver never reaches the reply state",
        caller.state().name()
    );

    println!(
        "[ipc] self-call: the thread is BlockedOnEndpoint with nobody \
         receiving -- a deadlock, and deliberately not prevented. A \
         check for \"am I the receiver\" would catch this and miss \
         A->B->A, which is partial protection at full cost (ADR 036)"
    );

    // The way out. `Suspend` always works (ADR 026), and this is what
    // makes an unpreventable deadlock survivable: a supervising
    // component holding the TCB capability can free the thread.
    //
    // SAFETY: Live TCB; this thread is not the caller.
    unsafe {
        sched::suspend(caller);
    }
    assert_eq!(
        caller.state().name(),
        "Suspended",
        "Suspend left the thread {}",
        caller.state().name()
    );

    // And it is no longer on the endpoint's queue: `cancel_ipc` ran.
    let queued = {
        let _guard = sched::InterruptGuard::new();
        // SAFETY: Live Endpoint, interrupts disabled.
        let endpoint =
            unsafe { ipc::endpoint_from_slot(slot(SLOT_ENDPOINT)) }.expect("not an Endpoint");
        endpoint.queue_length()
    };
    assert_eq!(
        queued, 0,
        "the suspended thread is still in the endpoint's queue; a later \
         Send would hand a message to a thread that is not waiting"
    );

    // Resume it: the Call returns an error rather than resuming the
    // wait. A thread taken out of an IPC wait must be told.
    // SAFETY: Live TCB in a resumable state.
    unsafe { sched::resume(caller) }.expect("resume failed");
    spin_until("the rescued thread to report", || {
        DONE[0].load(Ordering::Acquire) > 0
    });

    assert_eq!(
        REPLY_ERROR[0].load(Ordering::Acquire),
        error_number(CapError::EndpointDead),
        "the rescued thread's Call returned {} rather than an error; a \
         cancelled wait must not look like a delivered message",
        REPLY_ERROR[0].load(Ordering::Acquire)
    );

    println!(
        "[ipc] self-call: Suspend took the thread off the queue and \
         Resume returned an error from the Call. The deadlock is not \
         prevented but it is escapable, and the escape needs a TCB \
         capability -- so rescue is itself an authority (ADR 036)"
    );

    ipc::report();
    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        qemu::EXIT_SUCCESS,
        qemu::HOST_STATUS_SUCCESS
    );
    qemu::exit(qemu::EXIT_SUCCESS)
}

// ---- M9b: the measurement (ADR 040) -----------------------------------

/// How many rounds each figure is taken over.
///
/// The minimum of these is reported, for the reason M9a's
/// cancellation measurement discovered the hard way: the first touch
/// of freshly-zeroed memory costs a page walk and a cache fill, and
/// those are real costs but not the one being measured. Noise only
/// ever adds, so the smallest observation is the closest to the truth.
const COST_SAMPLES: usize = 32;

/// Set by the measurement's partner thread so the main thread can see
/// it is ready.
static COST_PARTNER_READY: AtomicU64 = AtomicU64::new(0);
/// How many exchanges the partner should perform.
static COST_ROUNDS: AtomicU64 = AtomicU64::new(0);
/// Which measurement the partner is taking part in.
static COST_MODE: AtomicU64 = AtomicU64::new(0);

mod cost_mode {
    /// Partner receives, main thread sends.
    pub const RENDEZVOUS: u64 = 1;
    /// Partner serves with Recv + Reply; main thread calls.
    pub const CALL_REPLY: u64 = 2;
    /// Partner serves with a single ReplyRecv per request.
    pub const REPLY_RECV: u64 = 3;
}

/// `ipc-cost`: what the three IPC paths cost, against M8's syscall.
///
/// ADR 040 refused a fastpath and promised to measure the slow one
/// instead. This is that measurement — and the point of taking all
/// three is the *differences* between them rather than any single
/// figure:
///
/// * `Send`+`Recv` is one rendezvous and one context switch;
/// * `Call`+`Reply` is two rendezvous and two switches;
/// * `ReplyRecv` does the server's half in one kernel entry rather
///   than two, so the saving ADR 036 claims should show up here or
///   not be claimed.
pub fn run_ipc_cost() -> ! {
    println!("[ipc] cost: measuring the three IPC paths (ADR 040)");

    make_endpoint(SLOT_ENDPOINT);
    create_worker(0, cost_partner);

    let rendezvous = measure_path(cost_mode::RENDEZVOUS);
    let call_reply = measure_path(cost_mode::CALL_REPLY);
    let reply_recv = measure_path(cost_mode::REPLY_RECV);

    println!(
        "[ipc] cost: Send+Recv {} cycles, Call+Reply {} cycles, \
         ReplyRecv loop {} cycles (minimum of {} samples each)",
        rendezvous, call_reply, reply_recv, COST_SAMPLES
    );

    // A `Call` is two rendezvous where a `Send` is one, so it must
    // cost more. Not a performance target -- a sanity check on the
    // measurement itself: if these came out equal, the figures would
    // be dominated by something other than the work.
    assert!(
        call_reply > rendezvous,
        "a Call+Reply round ({} cycles) did not cost more than a bare \
         rendezvous ({} cycles). A Call is two rendezvous and two \
         switches; equal figures mean the measurement is reporting \
         something other than the IPC path",
        call_reply,
        rendezvous
    );

    println!(
        "[ipc] cost: a Call round costs {} cycles more than a bare \
         rendezvous -- it is two rendezvous and two switches rather \
         than one. M8 measured a syscall round trip at 934 cycles and \
         a context switch at ~1000; these figures are the same order, \
         which is what ADR 040 predicted when it refused a fastpath on \
         the grounds that the slow path had not been measured",
        call_reply - rendezvous
    );

    // The ReplyRecv figure, and what it does **not** show.
    //
    // ADR 036 claims ReplyRecv saves a kernel entry, which M8 measured
    // at 934 cycles. The saving here is nowhere near that, and the
    // reason is a property of the test rather than of the kernel:
    // until M13 there is no user mode, so `ipc::reply_recv` is reached
    // by a Rust function call and no privilege transition is saved at
    // all. What is saved is one call and one re-resolution of the
    // endpoint slot.
    //
    // Reported rather than asserted. An assertion that the saving is
    // ~934 would fail honestly today; an assertion that it is
    // "positive" would be satisfied by noise. The figure is worth
    // having on record because it is the baseline the M13 measurement
    // will be compared against -- that is when ADR 036's claim becomes
    // testable, and it should be tested then rather than assumed now.
    if call_reply > reply_recv {
        println!(
            "[ipc] cost: the ReplyRecv loop saves {} cycles a round \
             against Recv+Reply -- far less than M8's 934-cycle kernel \
             entry, and necessarily so: there is no user mode until \
             M13, so reply_recv() is a function call here and no \
             privilege transition is saved. What is measured is one \
             call and one endpoint re-resolution. ADR 036's claim \
             becomes testable at M13 and is left untested rather than \
             asserted from this figure",
            call_reply - reply_recv
        );
    } else {
        println!(
            "[ipc] cost: the ReplyRecv loop did not come out cheaper \
             ({} against {} cycles). Not a failure: with no user mode \
             until M13 the saving is one function call, which is below \
             this measurement's noise floor under TCG. ADR 036's claim \
             is about a kernel entry and cannot be seen from here",
            reply_recv, call_reply
        );
    }

    println!(
        "[ipc] cost: measured under emulation. QEMU's TCG does not \
         model the pipeline flush a privilege transition costs on \
         silicon, so every figure here is a floor and the ratios \
         between them mean more than any one of them"
    );

    ipc::report();
    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        qemu::EXIT_SUCCESS,
        qemu::HOST_STATUS_SUCCESS
    );
    qemu::exit(qemu::EXIT_SUCCESS)
}

/// Time one IPC path, taking the minimum over [`COST_SAMPLES`] runs.
fn measure_path(mode: u64) -> u64 {
    let ep = slot(SLOT_ENDPOINT);
    let space = boot::init_cspace();
    let own = slot(boot::BOOT_TCB_SLOT);

    // One flag, raised here and lowered once the last exchange has
    // returned.
    //
    // The first version had two -- "begin" and "end" -- and
    // deadlocked on the first run: the partner waited for the main
    // thread to lower the flag while the main thread waited for the
    // partner to finish, each expecting the other to move first.
    //
    // The second signal was never needed. The partner knows how many
    // exchanges to serve because it is told, and the main thread
    // knows the run is over because its own last `Call` returned. A
    // handshake with nothing left to say is a handshake that can
    // deadlock.
    COST_MODE.store(mode, Ordering::Release);
    COST_ROUNDS.store(COST_SAMPLES as u64 + 1, Ordering::Release);
    COST_PARTNER_READY.store(1, Ordering::Release);

    let mut best = u64::MAX;

    // One warm-up exchange before the clock is trusted, then the
    // samples. The warm-up is not thrown away for tidiness: the first
    // pass touches both TCBs' buffers and both stacks for the first
    // time, and M9a's measurement showed those costs swamping the
    // operation by two orders of magnitude.
    for sample in 0..=COST_SAMPLES {
        let start = read_tsc();

        match mode {
            cost_mode::RENDEZVOUS => {
                // SAFETY: Live Endpoint and this thread's CSpace.
                unsafe {
                    ipc::send(
                        ep,
                        &Message {
                            tag: MessageTag::new(LABEL_PING, REGISTER_WORDS, 0),
                            registers: [sample as u64, 0, 0],
                            caps: &[],
                        },
                        &space,
                    )
                }
                .expect("the measurement's Send failed");
            }
            _ => {
                // SAFETY: As above, plus `own` holds this thread's TCB
                // capability for the reply target (ADR 036).
                unsafe {
                    ipc::call(
                        ep,
                        &Message {
                            tag: MessageTag::new(LABEL_PING, REGISTER_WORDS, 0),
                            registers: [sample as u64, 0, 0],
                            caps: &[],
                        },
                        &space,
                        own,
                    )
                }
                .expect("the measurement's Call failed");
            }
        }

        let elapsed = read_tsc() - start;
        if sample > 0 && elapsed < best {
            best = elapsed;
        }
    }

    // The partner has served its last exchange and is back at the top
    // of its loop; lower the flag so the next measurement can raise
    // it.
    COST_PARTNER_READY.store(0, Ordering::Release);
    best
}

/// The other half of every measured exchange.
extern "C" fn cost_partner() -> ! {
    let ep = slot(SLOT_ENDPOINT);
    let space = boot::init_cspace();

    loop {
        spin_until("the measurement to begin", || {
            COST_PARTNER_READY.load(Ordering::Acquire) != 0
        });

        let mode = COST_MODE.load(Ordering::Acquire);
        let rounds = COST_ROUNDS.load(Ordering::Acquire);

        match mode {
            cost_mode::RENDEZVOUS => {
                for _ in 0..rounds {
                    // SAFETY: Live Endpoint; no other borrow of this
                    // TCB.
                    if unsafe { ipc::recv(ep) }.is_err() {
                        break;
                    }
                }
            }
            cost_mode::CALL_REPLY => {
                for _ in 0..rounds {
                    // SAFETY: As above.
                    let Ok(request) = (unsafe { ipc::recv(ep) }) else {
                        break;
                    };
                    let answer = Message {
                        tag: MessageTag::new(LABEL_PONG, REGISTER_WORDS, 0),
                        registers: [request.registers[0], 0, 0],
                        caps: &[],
                    };
                    // SAFETY: This thread's own CSpace.
                    if unsafe { ipc::reply(&answer, &space) }.is_err() {
                        break;
                    }
                }
            }
            cost_mode::REPLY_RECV => {
                // The server loop ADR 036 is about: one kernel entry
                // per request rather than two. The first Recv is
                // outside the loop because there is nothing to reply
                // to yet.
                //
                // SAFETY: Live Endpoint.
                let Ok(mut request) = (unsafe { ipc::recv(ep) }) else {
                    continue;
                };
                for _ in 1..rounds {
                    let answer = Message {
                        tag: MessageTag::new(LABEL_PONG, REGISTER_WORDS, 0),
                        registers: [request.registers[0], 0, 0],
                        caps: &[],
                    };
                    // SAFETY: Live Endpoint and this thread's CSpace.
                    match unsafe { ipc::reply_recv(&answer, ep, &space) } {
                        Ok(next) => request = next,
                        Err(_) => break,
                    }
                }
                // Answer the last one so the main thread is not left
                // blocked.
                let answer = Message {
                    tag: MessageTag::new(LABEL_PONG, REGISTER_WORDS, 0),
                    registers: [request.registers[0], 0, 0],
                    caps: &[],
                };
                // SAFETY: As above.
                let _ = unsafe { ipc::reply(&answer, &space) };
            }
            _ => {}
        }

        // Back to the top, where the next `spin_until` waits for the
        // flag to be raised again.
        //
        // This used to be a second `spin_until` waiting for the flag
        // to *drop*, which is what deadlocked: the main thread lowers
        // it only after its final exchange returns, and that exchange
        // needed this thread to serve it.
        sched::yield_now();
    }
}
