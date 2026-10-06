// SPDX-License-Identifier: BSD-2-Clause
//! Capability system acceptance tests, run inside the kernel.
//!
//! Written to ADR 023's rules: every check reports a number, resource
//! consumption is observed rather than assumed, and a check that could
//! only pass by accident is not worth running. Several assertions below
//! exist because the first draft of the design was wrong in a way a
//! weaker test would have missed -- test 10 in particular.

use crate::cap::cnode::{self, CNode, Slot};
use crate::cap::cptr::{self, CPtr};
use crate::cap::untyped::{self, Untyped};
use crate::cap::{boot, CapError, CapType, Rights};
use crate::memory::frame_allocator;
use crate::println;

/// Run the capability test suite.
pub fn run() {
    println!("[captest] starting capability tests");

    check_object_sizes();
    check_rights_algebra();
    check_root_untypeds();
    check_retype();
    check_retype_atomicity();
    check_copy_narrowing();
    check_rights_escalation();
    check_cptr_errors();
    check_cptr_invariant();
    check_derivation_depth();
    check_device_untyped();
    check_delete_vs_revoke();
    check_revoke();

    println!("[captest] all capability tests passed");
}

// ---- Shared fixtures ---------------------------------------------------

/// Index of the root Untyped the tests carve from.
///
/// Chosen by **size**, not by remaining space. Size never changes, so
/// every call picks the same Untyped. Selecting by remaining space would
/// silently move the tests to a different region as the watermark
/// advanced, and two checks that both said "the scratch Untyped" would
/// be talking about different memory.
fn scratch_index() -> usize {
    let count = boot::root_untyped_count();
    let mut best = 0usize;
    let mut best_size = 0u64;

    for index in 0..count {
        // SAFETY: Index below the count boot reported.
        let slot = unsafe { boot::root_untyped(index) };
        // SAFETY: The slot holds an Untyped capability.
        let untyped = unsafe { Untyped::from_slot(slot) }.expect("root slot is not an Untyped");
        if untyped.size() > best_size {
            best_size = untyped.size();
            best = index;
        }
    }

    assert!(best_size > 0, "no root Untyped was created");
    best
}

/// The scratch Untyped's slot.
fn scratch_slot() -> &'static mut Slot {
    // SAFETY: `scratch_index` returns an index boot created.
    unsafe { boot::root_untyped(scratch_index()) }
}

/// The CNode the tests retype into.
fn object_cnode() -> CNode {
    // SAFETY: boot::init populated this root slot with a CNode.
    unsafe { boot::leaf_cnode(boot::ROOT_SLOT_OBJECTS) }
}

/// Empty every slot in the object CNode, so each check starts clean.
///
/// Deletes the capabilities but does **not** return the memory: only
/// revoking the Untyped does that (ADR 020), so the watermark keeps
/// climbing across the suite. That is why the checks below compare
/// watermarks against each other rather than against a fixed value.
fn clear_objects() {
    let cnode = object_cnode();
    for index in 0..cnode.len() {
        // SAFETY: Index in range.
        let slot = unsafe { cnode.slot(index) };
        if !slot.is_empty() {
            // SAFETY: The slot is live and in the derivation list.
            let _ = unsafe { cnode::delete(slot) };
        }
    }
}

/// A slot's object address as a plain integer.
fn addr_of(slot: &Slot) -> u64 {
    slot.object().0 .0
}

// ---- 1. Object sizes ---------------------------------------------------

/// Sizes match ADR 017 and obey its alignment rule.
fn check_object_sizes() {
    use crate::cap::{
        ENDPOINT_SIZE, FRAME_SIZE, IPC_BUFFER_SIZE, IRQ_HANDLER_SIZE, NOTIFICATION_SIZE,
        PAGE_TABLE_SIZE, SLOT_SIZE, TCB_SIZE,
    };

    // The `const` assertions in cap/mod.rs already make exceeding a
    // ceiling a build failure. Printing the figures means ADR 017's
    // table can be checked against reality rather than trusted.
    println!(
        "[captest] sizes: slot {} B, TCB {} B ({} ipc), Endpoint {} B, \
         Notification {} B",
        SLOT_SIZE, TCB_SIZE, IPC_BUFFER_SIZE, ENDPOINT_SIZE, NOTIFICATION_SIZE
    );
    println!(
        "[captest] sizes: Frame {} B, PageTable {} B, IRQHandler {} B",
        FRAME_SIZE, PAGE_TABLE_SIZE, IRQ_HANDLER_SIZE
    );

    // A slot must be exactly 32 bytes: ADR 019 budgets 8 for the object,
    // 8 for the packed fields and 16 for the derivation links, and every
    // CNode in the system is indexed at that stride.
    assert_eq!(
        core::mem::size_of::<Slot>(),
        SLOT_SIZE,
        "a slot is not 32 bytes; CNode indexing assumes that stride"
    );
    assert_eq!(
        core::mem::align_of::<Slot>(),
        32,
        "a slot is not 32-byte aligned; a CNode must be an array of them"
    );

    // ADR 017's alignment rule, checked for every fixed-size type rather
    // than one representative: retype's placement arithmetic is a single
    // mask, which is only correct while this holds.
    for cap_type in [
        CapType::Tcb,
        CapType::Endpoint,
        CapType::Notification,
        CapType::Frame,
        CapType::PageTable,
        CapType::IrqHandler,
    ] {
        let size = cap_type.fixed_size().expect("fixed-size type has no size");
        let align = cap_type.alignment();
        assert!(
            align.is_power_of_two(),
            "{} alignment {} is not a power of two",
            cap_type,
            align
        );
        assert!(
            align >= size,
            "{} alignment {} is smaller than its size {}",
            cap_type,
            align,
            size
        );
    }

    println!("[captest] sizes: every alignment is a power of two >= size (ADR 017)");
}

// ---- 1b. The rights algebra --------------------------------------------

/// `is_subset_of` really is the subset relation.
///
/// ADR 021's whole argument for not walking a capability's ancestors is
/// that `copy` enforces narrowing at every step, so the invariant holds
/// along a chain by induction. That induction is only as good as this
/// one function, and nothing else in the suite would notice if it were
/// subtly wrong -- a buggy version that merely returned `true` too often
/// would pass every other check here, because they all exercise cases
/// that are *supposed* to succeed.
///
/// So: exhaustively. There are sixteen possible rights sets, which is
/// 256 pairs, and checking all of them against an independent
/// definition costs nothing.
fn check_rights_algebra() {
    let mut subset_pairs = 0usize;

    for a in 0u8..16 {
        for b in 0u8..16 {
            let x = Rights::from_bits(a);
            let y = Rights::from_bits(b);

            // The independent definition: every bit of `x` appears in
            // `y`. Written differently from the implementation on
            // purpose -- `a & !b == 0` is what the implementation does,
            // so repeating it here would check nothing.
            let expected = (0..4).all(|bit| (a >> bit) & 1 == 0 || (b >> bit) & 1 == 1);
            assert_eq!(
                x.is_subset_of(y),
                expected,
                "is_subset_of({:?}, {:?}) said {}, expected {}",
                x,
                y,
                x.is_subset_of(y),
                expected
            );

            if expected {
                subset_pairs += 1;
            }

            // An intersection is a subset of both operands -- this is
            // what makes "narrow to the intersection" a safe default
            // anywhere the kernel needs to combine two rights sets.
            let both = x.intersection(y);
            assert!(
                both.is_subset_of(x) && both.is_subset_of(y),
                "{:?} & {:?} = {:?}, which is not a subset of both",
                x,
                y,
                both
            );

            // A union contains both. Not used on any narrowing path --
            // that would be escalation -- but `mint` and the boot code
            // build rights sets with it.
            let either = x.union(y);
            assert!(
                x.is_subset_of(either) && y.is_subset_of(either),
                "{:?} | {:?} = {:?}, which does not contain both",
                x,
                y,
                either
            );

            // `contains` and `is_subset_of` are the same relation read
            // in opposite directions. They are separate functions and
            // could drift apart.
            assert_eq!(
                y.contains(x),
                x.is_subset_of(y),
                "contains and is_subset_of disagree for {:?} and {:?}",
                x,
                y
            );
        }
    }

    // NONE is below everything and ALL above everything: the two ends of
    // the lattice, which is what lets `copy` use NONE as "no authority"
    // and retype use ALL as "full authority" without special cases.
    for bits in 0u8..16 {
        let r = Rights::from_bits(bits);
        assert!(Rights::NONE.is_subset_of(r), "NONE is not below {:?}", r);
        assert!(r.is_subset_of(Rights::ALL), "{:?} is not below ALL", r);
    }
    assert!(Rights::NONE.is_empty(), "NONE is not empty");
    assert!(!Rights::ALL.is_empty(), "ALL is empty");
    assert_eq!(
        Rights::GRANT.intersection(Rights::GRANTREPLY),
        Rights::NONE,
        "GRANT and GRANTREPLY overlap; they are meant to be separate bits \
         so that a capability can carry replies without carrying caps"
    );

    // 3^4: for each of the four bits, the pair (in x, in y) may be
    // (0,0), (0,1) or (1,1) -- never (1,0), which is what a subset
    // forbids. Stating the figure rather than just counting means a
    // wrong count is visible rather than self-confirming.
    assert_eq!(
        subset_pairs, 81,
        "{} of the 256 pairs were subsets; with four independent bits it \
         must be 3^4 = 81",
        subset_pairs
    );

    println!(
        "[captest] rights algebra: all 256 pairs checked against an \
         independent definition; {} subsets as predicted by 3^4; NONE and \
         ALL bound the lattice",
        subset_pairs
    );
}

// ---- 2. Root Untypeds --------------------------------------------------

/// Boot built root Untypeds, and each obeys ADR 020's shape rule.
fn check_root_untypeds() {
    let count = boot::root_untyped_count();
    let reported = boot::root_untyped_bytes();

    assert!(count > 0, "boot created no root Untypeds");
    assert!(reported > 0, "the root Untypeds cover no memory");

    let mut total = 0u64;
    let mut smallest = u64::MAX;
    let mut largest = 0u64;

    for index in 0..count {
        // SAFETY: Index below the reported count.
        let slot = unsafe { boot::root_untyped(index) };
        assert_eq!(
            slot.cap_type(),
            CapType::Untyped,
            "root untyped {} holds a {}",
            index,
            slot.cap_type()
        );
        assert_eq!(slot.depth(), 0, "a root Untyped must be at depth 0");

        // SAFETY: The slot holds an Untyped.
        let untyped = unsafe { Untyped::from_slot(slot) }.expect("not an Untyped");

        assert!(
            untyped.size().is_power_of_two(),
            "Untyped {} has size {}, which is not a power of two; ADR 020's \
             placement arithmetic depends on it",
            index,
            untyped.size()
        );
        assert!(
            untyped.base().0.is_multiple_of(untyped.size()),
            "Untyped {} at {} is not aligned to its own size {}",
            index,
            untyped.base(),
            untyped.size()
        );

        total += untyped.size();
        smallest = smallest.min(untyped.size());
        largest = largest.max(untyped.size());
    }

    // Summing the capabilities and comparing with what boot counted
    // separately. A mismatch means a capability was written with a
    // different size from the block claimed for it -- a bookkeeping
    // error nothing else here would notice.
    assert_eq!(
        total, reported,
        "the Untyped capabilities total {} bytes but boot reported {}",
        total, reported
    );

    println!(
        "[captest] root untypeds: {} covering {} MiB, sizes {} KiB .. {} KiB, \
         all power-of-two and self-aligned",
        count,
        total / (1024 * 1024),
        smallest / 1024,
        largest / 1024
    );
}

// ---- 3. Retype ---------------------------------------------------------

/// Retype produces usable objects and moves the watermark sensibly.
fn check_retype() {
    clear_objects();
    let cnode = object_cnode();
    let slot = scratch_slot();

    // SAFETY: A live Untyped.
    let before = unsafe { Untyped::from_slot(slot) }.unwrap().watermark();

    // Three different size classes -- 512, 32 and 4096 bytes -- so the
    // alignment arithmetic is exercised rather than coincidentally right
    // for one size.
    //
    // SAFETY: The slot is a live Untyped, the CNode is live, and slots
    // 0..3 were emptied above.
    unsafe {
        untyped::retype(slot, CapType::CNode, 4, 1, &cnode, 0).expect("retype CNode failed");
        untyped::retype(slot, CapType::Endpoint, 0, 1, &cnode, 1).expect("retype Endpoint failed");
        untyped::retype(slot, CapType::Frame, 0, 1, &cnode, 2).expect("retype Frame failed");
    }

    // SAFETY: Still a live Untyped.
    let after = unsafe { Untyped::from_slot(slot) }.unwrap().watermark();

    let cnode_bytes = crate::cap::SLOT_SIZE << 4;
    let expectations = [
        (0usize, CapType::CNode, cnode_bytes),
        (1, CapType::Endpoint, crate::cap::ENDPOINT_SIZE),
        (2, CapType::Frame, crate::cap::FRAME_SIZE),
    ];

    for (index, expected_type, size) in expectations {
        // SAFETY: Index in range.
        let object_slot = unsafe { cnode.slot(index) };
        assert!(
            !object_slot.is_empty(),
            "slot {} is empty after a successful retype",
            index
        );
        assert_eq!(
            object_slot.cap_type(),
            expected_type,
            "slot {} holds a {} instead of a {}",
            index,
            object_slot.cap_type(),
            expected_type
        );
        assert_eq!(
            object_slot.depth(),
            1,
            "a retyped object must sit one level below its Untyped"
        );
        assert_eq!(
            object_slot.rights(),
            Rights::ALL,
            "a freshly retyped object should carry full rights"
        );

        let align = (size as u64).next_power_of_two();
        assert!(
            addr_of(object_slot).is_multiple_of(align),
            "{} at {:#x} is not {}-byte aligned",
            expected_type,
            addr_of(object_slot),
            align
        );
    }

    // The watermark must cover the objects, and the excess must be
    // explainable as alignment padding. An upper bound as well as a
    // lower one: "the watermark moved" would also be true of a retype
    // that had lost track of where it was.
    let moved = after - before;
    let objects =
        cnode_bytes as u64 + crate::cap::ENDPOINT_SIZE as u64 + crate::cap::FRAME_SIZE as u64;
    let max_padding = 2 * crate::cap::FRAME_SIZE as u64;
    assert!(
        moved >= objects,
        "the watermark moved {} bytes for {} bytes of objects",
        moved,
        objects
    );
    assert!(
        moved <= objects + max_padding,
        "the watermark moved {} bytes for {} bytes of objects; that is more \
         padding than the largest alignment ({}) can explain",
        moved,
        objects,
        crate::cap::FRAME_SIZE
    );

    // "Usable" means the object works, not that a capability exists. The
    // retyped CNode is the one object here whose use can be demonstrated
    // without the rest of the kernel: it must be an array of empty slots
    // of exactly the size its capability claims.
    //
    // SAFETY: Slot 0 holds a CNode whose radix is in `extra`.
    let inner = unsafe {
        let s = cnode.slot(0);
        CNode::new(s.object().0, s.extra() as u8)
    };
    assert_eq!(
        inner.len(),
        16,
        "the retyped CNode has the wrong slot count"
    );
    for index in 0..inner.len() {
        // SAFETY: Index in range.
        let inner_slot = unsafe { inner.slot(index) };
        assert!(
            inner_slot.is_empty(),
            "slot {} of a freshly retyped CNode is not empty; retype did not \
             zero the object",
            index
        );
        assert_eq!(
            inner_slot.generation(),
            0,
            "a freshly retyped CNode's slots should start at generation 0"
        );
    }

    println!(
        "[captest] retype: CNode(16) + Endpoint + Frame; watermark {} -> {} \
         (+{} B for {} B of objects); the new CNode's 16 slots are all zero",
        before, after, moved, objects
    );
}

// ---- 4. Atomicity ------------------------------------------------------

/// A retype that cannot fit changes nothing at all.
///
/// Carved from a small child Untyped rather than the 4 MiB scratch one:
/// `RETYPE_MAX_COUNT` Frames is 1 MiB, which would fit comfortably in
/// the scratch Untyped, and a test whose failure case cannot be reached
/// is worse than no test.
fn check_retype_atomicity() {
    clear_objects();
    let cnode = object_cnode();
    let parent = scratch_slot();

    // 64 KiB: room for 16 Frames, so a request for 256 cannot be met.
    const CHILD_BITS: u8 = 16;

    // SAFETY: Live Untyped and CNode; slot 0 is empty after clearing.
    unsafe { untyped::retype(parent, CapType::Untyped, CHILD_BITS, 1, &cnode, 0) }
        .expect("could not create the small child Untyped");

    // SAFETY: Live slot holding the child Untyped.
    let child = unsafe { cnode.slot(0) };
    // SAFETY: It is an Untyped.
    let small = unsafe { Untyped::from_slot(child) }.unwrap();

    let watermark_before = small.watermark();
    let remaining = small.remaining();
    let count = crate::cap::RETYPE_MAX_COUNT;
    let needed = (count * crate::cap::FRAME_SIZE) as u64;
    assert!(
        needed > remaining,
        "the request must be impossible for this test to mean anything: \
         {} bytes needed, {} available",
        needed,
        remaining
    );

    // SAFETY: Live Untyped and CNode; this call is expected to fail.
    let result = unsafe { untyped::retype(child, CapType::Frame, 0, count, &cnode, 8) };

    match result {
        Err(CapError::NotEnoughMemory { remaining: r, .. }) => {
            assert_eq!(
                r, remaining,
                "the error reported {} bytes remaining, but the Untyped has {}",
                r, remaining
            );
        }
        other => panic!(
            "expected NotEnoughMemory for {} frames in {} bytes, got {:?}",
            count, remaining, other
        ),
    }

    // The proof that nothing happened. A retype that validated as it went
    // would leave the watermark partly advanced *and* the first few
    // destination slots filled; both are checked, because either alone
    // could pass by luck.
    // SAFETY: Live Untyped.
    let watermark_after = unsafe { Untyped::from_slot(child) }.unwrap().watermark();
    assert_eq!(
        watermark_after, watermark_before,
        "a failed retype moved the watermark from {} to {}; it is not atomic",
        watermark_before, watermark_after
    );

    let inspected = (cnode.len() - 8).min(count);
    for index in 8..8 + inspected {
        // SAFETY: Index in range.
        let dest = unsafe { cnode.slot(index) };
        assert!(
            dest.is_empty(),
            "a failed retype left a capability in destination slot {}",
            index
        );
    }

    println!(
        "[captest] atomicity: {} frames ({} B) refused by a {} B Untyped; \
         watermark still {}, all {} destination slots still empty",
        count, needed, remaining, watermark_after, inspected
    );
}

// ---- 5. Copy with narrowed rights --------------------------------------

fn check_copy_narrowing() {
    clear_objects();
    let cnode = object_cnode();
    let parent = scratch_slot();

    // SAFETY: Live Untyped and CNode; slot 0 is empty.
    unsafe { untyped::retype(parent, CapType::Endpoint, 0, 1, &cnode, 0) }
        .expect("retype Endpoint failed");

    // SAFETY: Live slots at distinct indices.
    let src = unsafe { cnode.slot(0) };
    // SAFETY: As above.
    let dest = unsafe { cnode.slot(1) };

    let full = src.rights();
    assert_eq!(
        full,
        Rights::ALL,
        "a retyped object should have full rights"
    );

    // SAFETY: Both slots live and distinct; the destination is empty.
    unsafe { cnode::copy(src, dest, Rights::WRITE) }.expect("narrowing copy failed");

    assert_eq!(
        dest.rights(),
        Rights::WRITE,
        "the copy has rights {:?}, asked for {:?}",
        dest.rights(),
        Rights::WRITE
    );
    assert!(
        dest.rights().is_subset_of(src.rights()),
        "the copy's rights {:?} are not a subset of the source's {:?}",
        dest.rights(),
        src.rights()
    );
    assert_eq!(
        dest.object(),
        src.object(),
        "the copy names a different object from its source"
    );
    assert_eq!(
        dest.cap_type(),
        src.cap_type(),
        "the copy has a different type from its source"
    );
    assert_eq!(
        dest.depth(),
        src.depth() + 1,
        "the copy is at depth {}, its source at {}",
        dest.depth(),
        src.depth()
    );

    println!(
        "[captest] copy: Endpoint {:?} -> {:?}, same object {}, depth {} -> {}",
        full,
        dest.rights(),
        dest.object(),
        src.depth(),
        dest.depth()
    );
}

// ---- 6. Rights escalation ----------------------------------------------

fn check_rights_escalation() {
    let cnode = object_cnode();

    // Slot 1 holds the WRITE-only copy made above.
    // SAFETY: Live slot.
    let narrowed = unsafe { cnode.slot(1) };
    assert_eq!(narrowed.rights(), Rights::WRITE, "test fixture changed");

    // SAFETY: Live slot; empty.
    let dest = unsafe { cnode.slot(2) };

    // Ask for READ, which the source does not hold.
    let asked = Rights::READ.union(Rights::WRITE);
    // SAFETY: Both live and distinct; expected to fail.
    let result = unsafe { cnode::copy(narrowed, dest, asked) };

    match result {
        Err(CapError::RightsEscalation { asked: a, held }) => {
            assert_eq!(a, asked);
            assert_eq!(held, Rights::WRITE);
            assert!(
                !a.is_subset_of(held),
                "the error fired for a request that was in fact a subset"
            );
        }
        other => panic!("expected RightsEscalation, got {:?}", other),
    }

    assert!(
        dest.is_empty(),
        "a rejected copy still wrote a capability into the destination"
    );

    // GRANT cannot be conjured either: a capability without it cannot
    // mint, because a badge is new authority rather than a subset of
    // existing authority (ADR 021).
    // SAFETY: Live slots, distinct.
    match unsafe { cnode::mint(narrowed, dest, Rights::WRITE, 7) } {
        Err(CapError::InsufficientRights { needed, held }) => {
            assert_eq!(needed, Rights::GRANT);
            assert_eq!(held, Rights::WRITE);
        }
        other => panic!("expected InsufficientRights from mint, got {:?}", other),
    }
    assert!(dest.is_empty(), "a rejected mint still wrote a capability");

    println!(
        "[captest] rights: W-only cap asked for R|W -> RightsEscalation; \
         asked to mint -> InsufficientRights(GRANT); destination untouched"
    );
}

// ---- 7-9. CPtr resolution ----------------------------------------------

/// The good case and five distinct failures.
fn check_cptr_errors() {
    let space = boot::init_cspace();
    let cnode = object_cnode();

    // Slot 0 holds the Endpoint from check_copy_narrowing.
    // SAFETY: Index in range.
    assert_eq!(unsafe { cnode.slot(0) }.cap_type(), CapType::Endpoint);

    // The good case first, so that the failures below mean something.
    // SAFETY: Both indices valid.
    let good = unsafe { cptr::address_of(&space, boot::ROOT_SLOT_OBJECTS, 0) }
        .expect("could not address the Endpoint");
    // SAFETY: Live CSpace.
    let slot = unsafe { cptr::resolve(&space, good) }.expect("a valid CPtr failed to resolve");
    assert_eq!(slot.cap_type(), CapType::Endpoint);

    // (a) Reserved bits not zero. Keeping the field zero today is what
    //     lets it mean something tomorrow, so a dirty pointer is
    //     rejected rather than masked off.
    let dirty = CPtr(good.0 | (1u64 << 32));
    // SAFETY: Live CSpace.
    match unsafe { cptr::resolve(&space, dirty) } {
        Err(CapError::InvalidCPtr) => {}
        other => panic!(
            "expected InvalidCPtr for non-zero reserved bits, got {:?}",
            other
        ),
    }

    // (b) Leaf index past the end of its CNode.
    let out_of_range = CPtr::new(
        boot::ROOT_SLOT_OBJECTS as u16,
        good.l1_generation(),
        60_000,
        0,
    );
    // SAFETY: Live CSpace.
    match unsafe { cptr::resolve(&space, out_of_range) } {
        Err(CapError::CPtrOutOfRange) => {}
        other => panic!("expected CPtrOutOfRange, got {:?}", other),
    }
    // (b2) Root index past the end of the root CNode. Derived from the
    //      CSpace's actual size rather than a number that merely looks
    //      too large -- the root radix is a constant today and will not
    //      stay one.
    let past_root = CPtr::new(space.root_slots() as u16, 0, 0, 0);
    // SAFETY: Live CSpace.
    match unsafe { cptr::resolve(&space, past_root) } {
        Err(CapError::CPtrOutOfRange) => {}
        other => panic!(
            "expected CPtrOutOfRange for root index {}, got {:?}",
            space.root_slots(),
            other
        ),
    }

    // (b3) A root slot that was never filled. The two-level rule makes
    //      this EmptySlot rather than a silent descent into nothing.
    let empty_root = CPtr::new(space.root_slots() as u16 - 1, 0, 0, 0);
    // SAFETY: Live CSpace.
    match unsafe { cptr::resolve(&space, empty_root) } {
        Err(CapError::EmptySlot) => {}
        other => panic!(
            "expected EmptySlot for an unused root slot, got {:?}",
            other
        ),
    }

    // (c) Wrong type for the operation.
    // SAFETY: Live CSpace.
    match unsafe { cptr::resolve_typed(&space, good, CapType::Tcb) } {
        Err(CapError::WrongType { found }) => assert_eq!(found, CapType::Endpoint),
        other => panic!("expected WrongType, got {:?}", other),
    }

    // (d) Empty slot: a well-formed pointer at a slot holding nothing.
    // SAFETY: Both indices valid; slot 200 was never filled.
    let empty = unsafe { cptr::address_of(&space, boot::ROOT_SLOT_OBJECTS, 200) }
        .expect("could not address an empty slot");
    // SAFETY: Live CSpace.
    match unsafe { cptr::resolve_typed(&space, empty, CapType::Endpoint) } {
        Err(CapError::EmptySlot) => {}
        other => panic!("expected EmptySlot, got {:?}", other),
    }

    // (e) Stale generation.
    let stale = good.with_l0_generation(good.l0_generation().wrapping_add(1));
    // SAFETY: Live CSpace.
    match unsafe { cptr::resolve(&space, stale) } {
        Err(CapError::StaleCPtr) => {}
        other => panic!(
            "expected StaleCPtr for a bumped generation, got {:?}",
            other
        ),
    }

    println!(
        "[captest] cptr: a valid pointer resolves; reserved-bits, \
         out-of-range, wrong-type, empty-slot and stale-generation each \
         give their own error"
    );
}

// ---- 10. The CPtr invariant --------------------------------------------

/// A CPtr resolves to the same slot every time, or returns an error. It
/// never resolves to a different slot.
///
/// This check exists because the first draft of ADR 018 broke it. A root
/// slot whose type changed would silently redirect an existing CPtr into
/// another object's slot zero -- the same ambiguity the ADR criticised
/// seL4's guard for, arrived at from a different direction. Two
/// mechanisms now prevent it; this exercises both.
fn check_cptr_invariant() {
    let space = boot::init_cspace();
    let cnode = object_cnode();

    // Slot 0 holds the Endpoint. Record what the pointer names now.
    // SAFETY: Valid indices.
    let before = unsafe { cptr::address_of(&space, boot::ROOT_SLOT_OBJECTS, 0) }.unwrap();
    // SAFETY: Live CSpace.
    let original = unsafe { cptr::resolve(&space, before) }.unwrap();
    let original_object = original.object();
    let original_type = original.cap_type();
    let original_generation = original.generation();

    // Delete it and put a *different type* in the same slot: the exact
    // sequence that defeated the first design.
    // SAFETY: Live slot in the derivation list.
    let slot = unsafe { cnode.slot(0) };
    // SAFETY: As above.
    unsafe { cnode::delete(slot) }.expect("delete failed");
    assert!(slot.is_empty(), "the slot is not empty after delete");
    assert_eq!(
        slot.generation(),
        original_generation.wrapping_add(1),
        "emptying a slot must advance its generation, or a stale pointer \
         would still resolve"
    );

    let parent = scratch_slot();
    // SAFETY: Live Untyped and CNode; slot 0 is now empty.
    unsafe { untyped::retype(parent, CapType::Notification, 0, 1, &cnode, 0) }
        .expect("retype into the reused slot failed");

    // SAFETY: Live slot.
    let replacement = unsafe { cnode.slot(0) };
    assert_eq!(replacement.cap_type(), CapType::Notification);
    // If the replacement landed on the same address, the test could not
    // tell "resolved to the old object" from "resolved to the new one",
    // so it would prove nothing.
    assert_ne!(
        replacement.object(),
        original_object,
        "the replacement object shares an address with the old one; this \
         check cannot distinguish the two cases"
    );

    // The old pointer must fail. It must NOT resolve to the Notification.
    // SAFETY: Live CSpace.
    match unsafe { cptr::resolve(&space, before) } {
        Err(CapError::StaleCPtr) => {}
        Ok(resolved) => panic!(
            "the CPtr invariant is broken: a pointer issued for a {} at {} \
             now resolves to a {} at {}",
            original_type,
            original_object,
            resolved.cap_type(),
            resolved.object()
        ),
        other => panic!("expected StaleCPtr, got {:?}", other),
    }

    // The other half of the invariant: a pointer built now works and
    // names the new object. An error or the same slot -- never a third
    // outcome.
    // SAFETY: Valid indices.
    let after = unsafe { cptr::address_of(&space, boot::ROOT_SLOT_OBJECTS, 0) }.unwrap();
    // SAFETY: Live CSpace.
    let now = unsafe { cptr::resolve(&space, after) }.expect("a fresh CPtr does not resolve");
    assert_eq!(now.cap_type(), CapType::Notification);
    assert_eq!(now.object(), replacement.object());
    assert_ne!(
        before.0, after.0,
        "the two pointers are bit-identical, so the generation is not \
         actually part of the CPtr"
    );

    // The root level is protected too: replacing a leaf CNode must
    // invalidate every pointer through it, not merely the one slot.
    // SAFETY: Valid index; the root slot holds a CNode.
    let root_slot = unsafe { space.root_slot(boot::ROOT_SLOT_OBJECTS) };
    let forged = CPtr::new(
        boot::ROOT_SLOT_OBJECTS as u16,
        root_slot.generation().wrapping_add(1),
        0,
        now.generation(),
    );
    // SAFETY: Live CSpace.
    match unsafe { cptr::resolve(&space, forged) } {
        Err(CapError::StaleCPtr) => {}
        other => panic!(
            "expected StaleCPtr for a wrong root generation, got {:?}",
            other
        ),
    }

    println!(
        "[captest] cptr invariant: slot 0 was {} gen {}, is now {} gen {}; \
         the old pointer gives StaleCPtr and never the new object; a wrong \
         root generation is caught too",
        original_type,
        original_generation,
        now.cap_type(),
        now.generation()
    );
}

// ---- 11. Derivation depth ----------------------------------------------

/// An eight-level chain builds; the ninth is refused.
fn check_derivation_depth() {
    clear_objects();
    let cnode = object_cnode();
    let parent = scratch_slot();

    // SAFETY: Live Untyped and CNode; slot 0 empty.
    unsafe { untyped::retype(parent, CapType::Frame, 0, 1, &cnode, 0) }.expect("retype failed");

    // SAFETY: Index in range.
    let seed_depth = unsafe { cnode.slot(0) }.depth();
    assert_eq!(seed_depth, 1, "a retyped object should be at depth 1");

    let cap = crate::cap::MAX_DERIVATION_DEPTH;
    let mut source = 0usize;
    let mut made = 0usize;
    let mut refused = false;

    // Bounded by the depth cap plus two, because that is the whole
    // claim: a loop that could run longer would be testing something
    // else.
    for step in 0..(cap as usize + 2) {
        let dest_index = 1 + step;
        // SAFETY: Both indices in range and distinct; destination empty.
        let result =
            unsafe { cnode::copy(cnode.slot(source), cnode.slot(dest_index), Rights::READ) };

        match result {
            Ok(()) => {
                // SAFETY: Index in range.
                let depth = unsafe { cnode.slot(dest_index) }.depth();
                assert!(
                    depth <= cap,
                    "a copy succeeded at depth {}, past the cap of {}",
                    depth,
                    cap
                );
                source = dest_index;
                made += 1;
            }
            Err(CapError::DerivationTooDeep { depth }) => {
                assert_eq!(
                    depth,
                    cap + 1,
                    "the limit fired reporting depth {} rather than {}",
                    depth,
                    cap + 1
                );
                // SAFETY: Index in range.
                let untouched = unsafe { cnode.slot(dest_index) }.is_empty();
                assert!(untouched, "a refused copy still wrote a capability");
                refused = true;
                break;
            }
            other => panic!("unexpected result while deepening the chain: {:?}", other),
        }
    }

    assert!(refused, "the chain never hit the depth cap");
    // SAFETY: Index in range.
    let final_depth = unsafe { cnode.slot(source) }.depth();
    assert_eq!(
        final_depth, cap,
        "the chain stopped at depth {} rather than the cap of {}",
        final_depth, cap
    );

    println!(
        "[captest] depth: chain reached {} in {} copies; the next gave \
         DerivationTooDeep and wrote nothing",
        final_depth, made
    );
}

// ---- 12. Device Untyped ------------------------------------------------

/// Device memory may only become a Frame.
fn check_device_untyped() {
    clear_objects();
    let cnode = object_cnode();
    let parent = scratch_slot();

    // The restriction under test is about the *flag*, not the address,
    // so this flags a child Untyped over ordinary RAM. Using real device
    // memory would mean writing to a peripheral's registers in order to
    // prove that the kernel refuses to write to a peripheral's
    // registers.
    // SAFETY: Live Untyped and CNode; slot 0 empty.
    unsafe { untyped::retype(parent, CapType::Untyped, 16, 1, &cnode, 0) }
        .expect("could not create a child Untyped");

    // SAFETY: Live slot holding an Untyped.
    let child = unsafe { cnode.slot(0) };
    // SAFETY: A live Untyped this test just created and exclusively
    // owns; nothing has been retyped out of it yet.
    unsafe { cnode::mark_device(child) }.expect("could not set the device flag");
    assert!(child.is_device(), "the device flag did not stick");

    // A Frame is allowed.
    // SAFETY: Live Untyped and CNode; slot 1 empty.
    unsafe { untyped::retype(child, CapType::Frame, 0, 1, &cnode, 1) }
        .expect("a device Untyped must still be able to produce a Frame");

    // SAFETY: Index in range.
    let frame = unsafe { cnode.slot(1) };
    assert_eq!(frame.cap_type(), CapType::Frame);
    assert!(
        frame.is_device(),
        "a Frame carved from device memory lost the flag; it could then be \
         recycled into a kernel object"
    );

    // Everything else is refused -- each type checked separately,
    // because the restriction is a single comparison and a mistake could
    // let exactly one type through.
    let forbidden = [
        (CapType::Tcb, 0u8),
        (CapType::CNode, 2),
        (CapType::Endpoint, 0),
        (CapType::Notification, 0),
        (CapType::PageTable, 0),
        (CapType::IrqHandler, 0),
        (CapType::Untyped, 12),
    ];

    for (cap_type, size_bits) in forbidden {
        // SAFETY: Live Untyped and CNode; expected to fail.
        let result = unsafe { untyped::retype(child, cap_type, size_bits, 1, &cnode, 2) };
        match result {
            Err(CapError::DeviceUntypedFrameOnly { asked }) => assert_eq!(asked, cap_type),
            other => panic!(
                "a device Untyped was asked for a {} and answered {:?} \
                 instead of refusing",
                cap_type, other
            ),
        }
        // SAFETY: Index in range.
        let untouched = unsafe { cnode.slot(2) }.is_empty();
        assert!(
            untouched,
            "a refused device retype for {} still wrote a capability",
            cap_type
        );
    }

    println!(
        "[captest] device untyped: Frame allowed and stays device-marked; \
         all {} other types refused with DeviceUntypedFrameOnly",
        forbidden.len()
    );
}

// ---- 13. delete versus revoke ------------------------------------------

/// delete removes one capability; revoke removes a subtree.
fn check_delete_vs_revoke() {
    clear_objects();
    let cnode = object_cnode();
    let parent = scratch_slot();

    // An Endpoint, a copy of it, and a copy of that copy.
    // SAFETY: Live Untyped and CNode; slots empty after clearing.
    unsafe { untyped::retype(parent, CapType::Endpoint, 0, 1, &cnode, 0) }.expect("retype failed");
    // SAFETY: Indices in range and distinct; destinations empty.
    unsafe {
        cnode::copy(cnode.slot(0), cnode.slot(1), Rights::ALL).expect("copy 1 failed");
        cnode::copy(cnode.slot(1), cnode.slot(2), Rights::READ).expect("copy 2 failed");
    }

    // SAFETY: Index in range.
    let origin = unsafe { cnode.slot(0) };
    // SAFETY: Live slot in the derivation list.
    let before = unsafe { cnode::count_descendants(origin) };
    assert_eq!(before, 2, "expected two descendants, found {}", before);

    // Delete the middle capability. Its own child must survive: that is
    // the entire difference between delete and revoke, and the reason
    // both exist.
    // SAFETY: Live slot in the list.
    unsafe { cnode::delete(cnode.slot(1)) }.expect("delete failed");

    // SAFETY: Indices in range.
    unsafe {
        assert!(cnode.slot(1).is_empty(), "delete left the slot occupied");
        assert!(
            !cnode.slot(2).is_empty(),
            "delete removed the grandchild as well; that is revoke's job"
        );
        assert!(!cnode.slot(0).is_empty(), "delete removed the origin");
    }

    // The grandchild is still a working capability, not merely a
    // non-empty slot.
    // SAFETY: Index in range.
    let grandchild = unsafe { cnode.slot(2) };
    assert_eq!(grandchild.cap_type(), CapType::Endpoint);
    assert_eq!(grandchild.object(), origin.object());
    assert_eq!(grandchild.rights(), Rights::READ);

    // SAFETY: Live slot.
    let after = unsafe { cnode::count_descendants(origin) };
    assert_eq!(
        after, 1,
        "the origin should have one descendant left, has {}",
        after
    );

    // Now revoke the origin: the survivor goes too.
    // SAFETY: Live slot.
    let report = unsafe { cnode::revoke(origin) }.expect("revoke failed");
    assert_eq!(report.deleted, 1, "revoke deleted {}", report.deleted);
    // SAFETY: Index in range.
    let grandchild_gone = unsafe { cnode.slot(2) }.is_empty();
    assert!(grandchild_gone, "revoke left the grandchild in place");
    assert!(!origin.is_empty(), "revoke deleted its own root");

    println!(
        "[captest] delete vs revoke: delete removed 1 of 3 and the \
         grandchild survived as a live R capability; revoke then removed \
         that descendant and left the origin"
    );
}

// ---- 14. revoke --------------------------------------------------------

/// revoke removes every descendant, keeps the root, and returns the
/// memory.
fn check_revoke() {
    clear_objects();
    let cnode = object_cnode();
    let parent = scratch_slot();

    // SAFETY: Live Untyped.
    let watermark_before = unsafe { Untyped::from_slot(parent) }.unwrap().watermark();

    // Four objects, each with a copy: two depths, eight capabilities.
    // SAFETY: Live Untyped and CNode; slots empty.
    unsafe { untyped::retype(parent, CapType::Endpoint, 0, 4, &cnode, 0) }.expect("retype failed");
    for index in 0..4 {
        // SAFETY: Indices in range and distinct; destinations empty.
        unsafe {
            cnode::copy(cnode.slot(index), cnode.slot(8 + index), Rights::READ)
                .expect("copy failed");
        }
    }

    // Where the objects landed, so that reuse can be observed rather
    // than inferred from the watermark alone.
    // SAFETY: Index in range.
    let lowest_object = unsafe { addr_of(cnode.slot(0)) };

    // SAFETY: Live slot.
    let descendants = unsafe { cnode::count_descendants(parent) };
    assert_eq!(
        descendants, 8,
        "expected 4 objects + 4 copies = 8 descendants, found {}",
        descendants
    );

    // SAFETY: Live Untyped capability.
    let report = unsafe { untyped::revoke_untyped(parent) }.expect("revoke failed");
    assert_eq!(
        report.deleted, 8,
        "revoke deleted {} capabilities, expected 8",
        report.deleted
    );

    for index in 0..4 {
        // SAFETY: Indices in range.
        unsafe {
            assert!(
                cnode.slot(index).is_empty(),
                "revoke left object {} in place",
                index
            );
            assert!(
                cnode.slot(8 + index).is_empty(),
                "revoke left copy {} in place",
                index
            );
        }
    }

    // The Untyped survives: revoke takes the descendants, not the root.
    assert!(
        !parent.is_empty(),
        "revoke deleted the Untyped itself; that is delete's job"
    );
    assert_eq!(parent.cap_type(), CapType::Untyped);
    assert_eq!(parent.depth(), 0);

    // The memory came back. The watermark resets to just past the
    // Untyped's header, not to whatever it was when this check started:
    // earlier checks had already advanced it, and revoke returns
    // *everything*, not the last transaction.
    // SAFETY: Live Untyped.
    let watermark_after = unsafe { Untyped::from_slot(parent) }.unwrap().watermark();
    assert!(
        watermark_after < watermark_before,
        "revoke returned no memory: watermark {} -> {}",
        watermark_before,
        watermark_after
    );

    // Observed, not assumed: a new object must now land at or below
    // where the revoked ones were. A watermark that merely *said* zero
    // while placement kept climbing would fail here.
    // SAFETY: Live Untyped and CNode; slot 0 is empty again.
    unsafe { untyped::retype(parent, CapType::Endpoint, 0, 1, &cnode, 0) }
        .expect("retype after revoke failed");
    // SAFETY: Index in range.
    let reused = unsafe { addr_of(cnode.slot(0)) };
    assert!(
        reused <= lowest_object,
        "after revoke a new object landed at {:#x}, above the revoked ones \
         at {:#x}; the memory was not actually returned",
        reused,
        lowest_object
    );

    // A pointer to a revoked capability must fail rather than resolve.
    let space = boot::init_cspace();
    // SAFETY: Valid indices.
    let dangling = unsafe { cptr::address_of(&space, boot::ROOT_SLOT_OBJECTS, 8) }.unwrap();
    // SAFETY: Live CSpace.
    match unsafe { cptr::resolve_typed(&space, dangling, CapType::Endpoint) } {
        Err(CapError::EmptySlot) | Err(CapError::StaleCPtr) => {}
        Ok(resolved) => panic!(
            "a pointer to a revoked capability resolved to a live {}",
            resolved.cap_type()
        ),
        other => panic!("unexpected error for a revoked capability: {:?}", other),
    }

    println!(
        "[captest] revoke: {} descendants deleted in {} visits; Untyped \
         survives, watermark {} -> {}, a new object reused {:#x} (was \
         {:#x}), old pointers return errors",
        report.deleted, report.visited, watermark_before, watermark_after, reused, lowest_object
    );
}

// ---- Negative scenario: the cost of a revoke ---------------------------

/// Measure a revoke over a wide, deep derivation tree.
///
/// Not pass/fail. ADR 019 estimated ~56 ms for a worst case of 8.4
/// million slots, and that estimate has never been checked against
/// anything. This builds the largest tree the init CSpace can hold,
/// times the revoke, and reports the per-capability cost -- the figure
/// that scales.
pub fn run_revoke_depth() -> ! {
    println!("[captest] building a wide, deep derivation tree");

    clear_objects();
    let cnode = object_cnode();
    let parent = scratch_slot();

    let capacity = cnode.len();
    let objects = (capacity / 2).min(crate::cap::RETYPE_MAX_COUNT);

    // SAFETY: Live Untyped and CNode; slots empty after clearing.
    unsafe { untyped::retype(parent, CapType::Endpoint, 0, objects, &cnode, 0) }
        .expect("could not fill the CNode with Endpoints");

    // Fill the rest with chains of copies. Each chain runs until the
    // depth cap refuses it, then restarts from another shallow
    // capability, so the tree ends up both as deep and as wide as the
    // CSpace allows -- which is what makes the measurement worth
    // extrapolating from.
    //
    // The attempt counter is not decoration: without it a bug in the
    // depth check would turn this into an infinite loop, and an
    // unbounded loop is the one thing no kernel path here may contain.
    const MAX_ATTEMPTS: usize = 8192;
    let mut chain_source = 0usize;
    let mut next_free = objects;
    let mut deepest = 1u8;
    let mut restarts = 0usize;
    let mut attempts = 0usize;

    while next_free < capacity && attempts < MAX_ATTEMPTS {
        attempts += 1;
        // SAFETY: Both indices in range and distinct; destination empty.
        let result = unsafe {
            cnode::copy(
                cnode.slot(chain_source),
                cnode.slot(next_free),
                Rights::READ,
            )
        };
        match result {
            Ok(()) => {
                // SAFETY: Index in range.
                deepest = deepest.max(unsafe { cnode.slot(next_free) }.depth());
                chain_source = next_free;
                next_free += 1;
            }
            Err(CapError::DerivationTooDeep { .. }) => {
                chain_source = (chain_source + 1) % objects;
                restarts += 1;
            }
            Err(error) => panic!("unexpected error while building the tree: {:?}", error),
        }
    }

    assert_eq!(
        next_free, capacity,
        "only filled {} of {} slots in {} attempts",
        next_free, capacity, attempts
    );

    // SAFETY: Live slot.
    let total = unsafe { cnode::count_descendants(parent) };
    assert_eq!(
        total, capacity,
        "the derivation list holds {} capabilities but {} slots are filled; \
         the list and the CNode disagree",
        total, capacity
    );
    assert_eq!(
        deepest,
        crate::cap::MAX_DERIVATION_DEPTH,
        "the tree is only {} levels deep, so it is not exercising the case \
         ADR 019 estimated",
        deepest
    );

    println!(
        "[captest] tree: {} capabilities from one Untyped, deepest level {}, \
         {} chain restarts",
        total, deepest, restarts
    );

    // The TSC is the only clock the kernel has before M6's APIC, and it
    // is enough here: what matters is a per-item cost, not an absolute
    // time.
    let start = read_tsc();
    // SAFETY: Live Untyped capability.
    let report = unsafe { untyped::revoke_untyped(parent) }.expect("revoke failed");
    let elapsed = read_tsc() - start;

    assert_eq!(
        report.deleted, total,
        "revoke deleted {} of {} capabilities",
        report.deleted, total
    );

    let per_cap = elapsed / report.deleted.max(1) as u64;

    println!(
        "[captest] revoke timing: {} capabilities in {} TSC cycles \
         ({} cycles each, {} slots visited)",
        report.deleted, elapsed, per_cap, report.visited
    );

    // ADR 019's worst case, extrapolated. Reported with its caveats
    // attached rather than as a figure: this is a few hundred samples on
    // an emulated CPU, QEMU's TSC is not a real clock, and the cache
    // behaviour of walking eight million slots is nothing like that of
    // walking two hundred. The number is an order of magnitude, and the
    // ADR should record it as one.
    const ADR_019_WORST_CASE: u64 = 8_400_000;
    const ASSUMED_HZ: u64 = 3_000_000_000;
    let projected = per_cap.saturating_mul(ADR_019_WORST_CASE);
    println!(
        "[captest] extrapolated: {} slots would be ~{} cycles, ~{} ms at \
         {} GHz -- extrapolated from {} samples under emulation, not a \
         measurement of the worst case",
        ADR_019_WORST_CASE,
        projected,
        projected / (ASSUMED_HZ / 1000),
        ASSUMED_HZ / 1_000_000_000,
        report.deleted
    );

    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

/// Read the timestamp counter.
fn read_tsc() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: RDTSC is unprivileged whenever CR4.TSD is clear, which it
    // is -- nothing in this kernel sets it. The instruction reads no
    // memory and writes only EDX:EAX, both declared as outputs here.
    //
    // Reference: Intel SDM Vol. 2B, "RDTSC -- Read Time-Stamp Counter".
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

// ---- Negative scenario: allocating from an interrupt -------------------

/// Allocate from interrupt context. Must panic.
///
/// M2's soundness argument for the allocator rested on three legs: one
/// CPU, interrupts disabled, and one non-reentrant entry point. M6 will
/// enable interrupts and remove the second. ADR 022 chose to make
/// "handlers do not allocate" an enforced rule rather than add a lock
/// that would protect nothing on a single CPU; this scenario is what
/// makes a violation loud at the moment it happens, instead of leaving a
/// corrupted bitmap to be discovered later.
pub fn run_interrupt_alloc() -> ! {
    println!("[captest] entering interrupt context, then allocating");

    // The depth counter is what the allocator checks, so driving it
    // directly tests the tripwire itself rather than whichever interrupt
    // vector happens to be wired up today.
    frame_allocator::enter_interrupt();
    assert!(
        frame_allocator::in_interrupt(),
        "the interrupt depth counter did not register the entry"
    );

    println!("[captest] interrupt depth is 1; calling allocate() (must panic)");

    let frame = frame_allocator::allocate();

    // Reached only if the tripwire is gone. The depth is unwound first
    // so that the message below is the failure being reported rather
    // than a second one stacked behind it.
    frame_allocator::exit_interrupt();
    unreachable!(
        "allocation from interrupt context returned {:?} instead of \
         panicking; the ADR 022 tripwire is not working",
        frame.map(|f| f.0)
    );
}

// ---- Negative scenario: allocating after retirement --------------------

/// Allocate after the frame allocator has been retired. Must panic.
///
/// ADR 022 retires the allocator once the root Untypeds exist, so that
/// every later allocation goes through a capability and is charged to a
/// budget. A call that slips through is a kernel bug -- a path that was
/// meant to be capability-mediated and is not -- so it stops the kernel
/// rather than quietly working and leaving the budget wrong.
pub fn run_retired_allocator() -> ! {
    assert!(
        frame_allocator::is_retired(),
        "the allocator should have been retired at the end of boot; this \
         scenario cannot test anything until it is"
    );
    println!("[captest] allocator is retired; calling allocate() (must panic)");

    let frame = frame_allocator::allocate();

    unreachable!(
        "allocation after retirement returned {:?} instead of panicking; \
         the ADR 022 retirement check is not working",
        frame.map(|f| f.0)
    );
}
