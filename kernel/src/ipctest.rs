// SPDX-License-Identifier: BSD-2-Clause
//! IPC acceptance tests — **M9a: synchronous rendezvous**.
//!
//! M9 is landing in three parts. This file covers the first: an
//! Endpoint object, `Send`, `Recv`, the doubly linked wait queue, and
//! cancellation. No capability transfer, no `Call`/`Reply`, no
//! Notification — those are M9b and M9c, and the code for them sits in
//! `cap::ipc` marked `#[allow(dead_code)]` rather than tested here.
//!
//! # The two scenarios that carry the milestone
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
/// Workers. Each takes six slots: one TCB and up to four stack frames.
const SLOT_WORKER_BASE: usize = 8;
const SLOT_WORKER_STRIDE: usize = 8;

/// KSTACK slots 0 and 1 belong to IST1 and the main stack (ADR 009).
const STACK_SLOT_BASE: u64 = 2;

const LABEL_PING: u32 = 0x7071;
const LABEL_LONG: u32 = 0x4C4F;

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
    let cnode = object_cnode();
    // SAFETY: The CSpace is built. This panics with a diagnosis
    // rather than returning a too-small Untyped.
    let parent = unsafe { boot::root_untyped_with_room(WORKER_BYTES) };
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
    start_phase_and_wait_for_block(0, "the receiver to block", phase::RECV, || {
        ipc::receiver_blocked_count()
    });

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
    start_phase_and_wait_for_block(0, "the receiver to block", phase::RECV, || {
        ipc::receiver_blocked_count()
    });
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
    start_phase_and_wait_for_block(0, "the receiver to block", phase::RECV, || {
        ipc::receiver_blocked_count()
    });
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

    start_phase_and_wait_for_block(0, "the long receiver to block", phase::RECV_LONG, || {
        ipc::receiver_blocked_count()
    });
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
