// SPDX-License-Identifier: MPL-2.0
//! Address space acceptance tests.
//!
//! The check that matters is the fifth: two threads reading the *same*
//! virtual address and seeing different bytes. Everything else here
//! could pass on a kernel with one address space and a lot of
//! bookkeeping; only that one cannot.
//!
//! Ring 3 does not exist yet. These threads run at CPL 0 in their own
//! CR3, which is deliberate -- M7 proves the address space, M8 proves
//! the privilege level, and separating them means a fault has one
//! suspect instead of two.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::cap::cnode::CNode;
use crate::cap::tcb::{self, ThreadState};
use crate::cap::vspace;
use crate::cap::{boot, untyped, CapError, CapType};
use crate::cpu;
use crate::memory::{layout, paging, VirtAddr};
use crate::println;
use crate::sched;

/// Where the test maps its frames in the user half.
///
/// 4 MiB: comfortably above the unmapped first 64 KiB (`USER_MIN`), and
/// a round number that is easy to recognise in a fault report.
const TEST_ADDRESS: u64 = 0x0040_0000;

/// A second address, in a different PDPT entry from [`TEST_ADDRESS`].
///
/// 512 GiB apart, so reaching it needs its own PDPT -- which is what
/// makes the "a missing table is an error, not an allocation" check
/// mean something.
const FAR_ADDRESS: u64 = 0x0000_0080_0000_0000;

/// What each thread writes into its own frame.
const PATTERN_A: u64 = 0xAAAA_AAAA_1111_1111;
const PATTERN_B: u64 = 0xBBBB_BBBB_2222_2222;

/// KSTACK slots for the test threads. 0 and 1 belong to IST1 and the
/// main stack (ADR 009).
const VTHREAD_A_SLOT: u64 = 6;
const VTHREAD_B_SLOT: u64 = 7;

/// How many switches the CR3 measurement times.
const MEASURE_SWITCHES: u64 = 10_000;

// ---- Shared state ------------------------------------------------------

/// What each thread read back from TEST_ADDRESS, in its own VSpace.
static SEEN_BY_A: AtomicU64 = AtomicU64::new(0);
static SEEN_BY_B: AtomicU64 = AtomicU64::new(0);

/// Set once each thread has looked.
static A_DONE: AtomicBool = AtomicBool::new(false);
static B_DONE: AtomicBool = AtomicBool::new(false);

/// Tells the measurement partner to stop.
static MEASURE_STOP: AtomicBool = AtomicBool::new(false);

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

/// Read CR3.
fn read_cr3() -> u64 {
    let value: u64;
    // SAFETY: Reading CR3 has no side effects and requires CPL 0, which
    // holds everywhere in this kernel.
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// The CNode the test retypes into.
fn object_cnode() -> CNode {
    // SAFETY: boot::init put a CNode in this root slot.
    unsafe { boot::leaf_cnode(boot::ROOT_SLOT_OBJECTS) }
}

/// Find a root Untyped with at least `bytes` free.
///
/// Searched rather than hard-coded, and the reason is a failed run:
/// the first version named Untyped 5, which happened to be 8 KiB on
/// this machine when the test needed 20. Boot carves the memory map
/// into power-of-two blocks, so both the sizes and their order depend
/// on the machine -- a fixed index is a guess about someone else's
/// hardware.
///
/// Bounded by the number of root Untypeds, which boot counted.
fn untyped_with_room(bytes: u64) -> usize {
    for index in 0..boot::root_untyped_count() {
        // SAFETY: Index below the count.
        let slot = unsafe { boot::root_untyped(index) };
        // SAFETY: Root slots hold live Untypeds.
        let untyped = unsafe { untyped::Untyped::from_slot(slot) }
            .expect("a root slot does not hold an Untyped");
        if !untyped.is_device() && untyped.remaining() >= bytes {
            return index;
        }
    }
    panic!(
        "no root Untyped has {} bytes free; the test cannot build an \
         address space",
        bytes
    );
}

/// Bytes one test address space costs: a root, three tables, a frame.
const SPACE_BYTES: u64 = 5 * 4096;

/// Bytes one test thread costs: a TCB plus four stack frames.
const THREAD_BYTES: u64 = 2048 + 4 * 4096;

// ---- Building an address space -----------------------------------------

/// Everything one test address space needs.
struct TestSpace {
    /// Slot holding the root PageTable capability.
    root_slot: usize,
    /// Slot holding the Frame mapped at TEST_ADDRESS.
    frame_slot: usize,
    /// Slots holding the three intermediate tables: PDPT, PD, PT.
    table_slots: [usize; 3],
    /// Index of the root Untyped everything above came from.
    ///
    /// Recorded because tearing the space down means revoking *this*,
    /// not the root PageTable. The root and the tables are siblings --
    /// all children of this Untyped -- so revoking the root reaches
    /// none of them. See `vspace::revoke_vspace`.
    untyped_index: usize,
}

/// Retype a root, three intermediate tables and a frame, then wire them
/// together into a working address space.
///
/// The intermediate tables are mapped one at a time, deepest last. That
/// is not an implementation detail of the test: the kernel refuses to
/// allocate them (ADR 027), so this sequence is what any caller has to
/// do, and writing it out here is the demonstration that the contract
/// is usable.
fn build_space(base_slot: usize, pattern: u64) -> TestSpace {
    let cnode = object_cnode();
    let untyped_index = untyped_with_room(SPACE_BYTES);
    // SAFETY: Index below the count boot reported.
    let parent = unsafe { boot::root_untyped(untyped_index) };

    // Five objects: the root, three tables, one frame.
    //
    // SAFETY: Live Untyped and CNode; the destination slots are empty.
    unsafe { untyped::retype(parent, CapType::PageTable, 0, 4, &cnode, base_slot) }
        .expect("could not retype the page tables");
    // SAFETY: As above.
    unsafe { untyped::retype(parent, CapType::Frame, 0, 1, &cnode, base_slot + 4) }
        .expect("could not retype the frame");

    let root_slot = base_slot;
    let table_slots = [base_slot + 1, base_slot + 2, base_slot + 3];
    let frame_slot = base_slot + 4;

    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(root_slot) };

    // SAFETY: The slot holds a freshly retyped PageTable that nothing
    // has mapped anywhere.
    unsafe { vspace::create_vspace(root) }.expect("could not create the VSpace");

    // Walk down: PDPT at level 3, PD at 2, PT at 1.
    let address = VirtAddr::new(TEST_ADDRESS);
    for (index, level) in [3u8, 2, 1].iter().enumerate() {
        // SAFETY: Index in range.
        let table = unsafe { cnode.slot(table_slots[index]) };
        // SAFETY: Both slots hold live capabilities.
        unsafe { vspace::map_table(root, table, address, *level) }
            .unwrap_or_else(|error| panic!("could not map the level-{} table: {}", level, error));
    }

    // Now the frame itself.
    // SAFETY: Index in range.
    let frame = unsafe { cnode.slot(frame_slot) };
    // SAFETY: Both slots hold live capabilities, and the tables above
    // are in place.
    unsafe { vspace::map_frame(root, frame, address, vspace::MapPermissions::ReadWrite) }
        .expect("could not map the frame");

    // Write the pattern through the physical window, before anything
    // runs in this address space. Going through PMAP rather than
    // through the new mapping is the point: it proves the mapping
    // reaches the frame the capability names, rather than merely
    // reaching *something*.
    //
    // SAFETY: The Frame came from a retype, so nothing else owns it,
    // and PMAP maps all RAM writably.
    unsafe {
        let ptr = crate::memory::phys_to_virt(frame.object().0).as_ptr::<u64>();
        ptr.write_volatile(pattern);
    }

    println!(
        "[vspace] space at root {}: frame {} mapped at {:#018x}, pattern \
         {:#018x}",
        frame.object(),
        frame.object(),
        TEST_ADDRESS,
        pattern
    );

    TestSpace {
        root_slot,
        frame_slot,
        table_slots,
        untyped_index,
    }
}

/// Create a thread bound to an address space.
fn build_thread(
    tcb_slot_index: usize,
    stack_slot: u64,
    entry: extern "C" fn() -> !,
    space: Option<&TestSpace>,
) -> usize {
    let cnode = object_cnode();
    let untyped_index = untyped_with_room(THREAD_BYTES);
    // SAFETY: Index below the count boot reported.
    let parent = unsafe { boot::root_untyped(untyped_index) };

    // SAFETY: Live Untyped and CNode; the destination slot is empty.
    unsafe { untyped::retype(parent, CapType::Tcb, 0, 1, &cnode, tcb_slot_index) }
        .expect("could not retype a TCB");

    let (_, bottom, top) = layout::kstack_slot(stack_slot);

    // The thread's kernel stack. It lives in the kernel half, so it is
    // reachable from every address space -- which is exactly why the
    // kernel half is shared (ADR 027). A thread whose stack vanished
    // when CR3 changed could not take an interrupt.
    let frames_needed = (layout::KSTACK_SIZE / crate::memory::FRAME_SIZE) as usize;
    let stack_base_slot = tcb_slot_index + 1;

    // SAFETY: Live Untyped and CNode; the destination slots are empty.
    unsafe {
        untyped::retype(
            parent,
            CapType::Frame,
            0,
            frames_needed,
            &cnode,
            stack_base_slot,
        )
    }
    .expect("could not retype the stack frames");

    for index in 0..frames_needed {
        // SAFETY: Index in range.
        let frame_slot = unsafe { cnode.slot(stack_base_slot + index) };
        let frame = crate::memory::PhysFrame::containing(frame_slot.object().0);
        let page = VirtAddr::new(bottom.0 + (index as u64) * crate::memory::FRAME_SIZE);

        // SAFETY: The kernel's tables are live, the address is inside
        // this thread's unmapped KSTACK slot, and the frame came from a
        // retype so nothing else owns it.
        unsafe {
            let table = crate::memory::vm::kernel_table();
            let mut source = crate::memory::heap::reserve_frames();
            let flags =
                paging::PageFlags::READ | paging::PageFlags::WRITE | paging::PageFlags::GLOBAL;
            let tlb = paging::map(table, page, frame, flags, &mut source)
                .expect("could not map a thread stack page");
            tlb.flush();
        }
    }

    // SAFETY: Index in range.
    let tcb_slot = unsafe { cnode.slot(tcb_slot_index) };

    // SAFETY: The slot holds a live TCB; the stack is mapped and owned
    // by nothing else. `entry` diverges.
    unsafe { tcb::set_registers(tcb_slot, entry, top, bottom, stack_slot) }
        .expect("SetRegisters failed");

    if let Some(space) = space {
        // SAFETY: Index in range.
        let root = unsafe { cnode.slot(space.root_slot) };
        // SAFETY: Both slots hold live capabilities and the thread is
        // not running. `None` for the CSpace: M7's threads are the
        // test's own and keep the init CSpace (ADR 043).
        unsafe { tcb::set_space(tcb_slot, Some(root), None) }.expect("SetSpace failed");
    }

    tcb_slot_index
}

/// Borrow a TCB by slot index.
///
/// # Safety
///
/// The caller must not hold another mutable borrow of the same TCB.
unsafe fn tcb_at(slot_index: usize) -> &'static mut tcb::Tcb {
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let slot = unsafe { cnode.slot(slot_index) };
    // SAFETY: The slot holds a live TCB; caller's aliasing guarantee.
    unsafe { tcb::from_slot(slot) }.expect("the slot no longer holds a TCB")
}

// ---- The two isolated threads ------------------------------------------

/// Read TEST_ADDRESS and report what this address space contains.
///
/// The `UserAccess` guard is not decoration. These pages are marked
/// USER -- that is what makes them part of a user address space -- and
/// SMAP forbids CPL 0 from touching such a page unless RFLAGS.AC is
/// set. Every thread here is still at CPL 0 (ring 3 is M8), so without
/// the guard this read is a protection violation on a page that is
/// plainly present. It was, on the first run.
extern "C" fn thread_a() -> ! {
    let value = {
        let _user = cpu::UserAccess::new();
        // SAFETY: TEST_ADDRESS is mapped in this thread's VSpace,
        // readable, and belongs to this thread alone. The guard lifts
        // SMAP for the duration of the read.
        unsafe { (TEST_ADDRESS as *const u64).read_volatile() }
    };
    SEEN_BY_A.store(value, Ordering::Release);
    A_DONE.store(true, Ordering::Release);

    loop {
        sched::yield_now();
    }
}

extern "C" fn thread_b() -> ! {
    let value = {
        let _user = cpu::UserAccess::new();
        // SAFETY: As thread A, in a different address space.
        unsafe { (TEST_ADDRESS as *const u64).read_volatile() }
    };
    SEEN_BY_B.store(value, Ordering::Release);
    B_DONE.store(true, Ordering::Release);

    loop {
        sched::yield_now();
    }
}

/// A partner for the measurement: yields and nothing else.
extern "C" fn thread_yield_forever() -> ! {
    loop {
        if MEASURE_STOP.load(Ordering::Acquire) {
            // SAFETY: We are this thread.
            unsafe {
                let me = sched::current();
                sched::suspend(me);
            }
        }
        sched::yield_now();
    }
}

// ---- The checks --------------------------------------------------------

/// Run the address space test suite.
pub fn run() {
    println!("[vspace] starting address space tests");

    check_kernel_half_shared();
    let space_a = check_build_space();
    check_translate_agrees(&space_a);
    check_smap_guards_user_pages(&space_a);
    check_rights_narrowing();
    check_isolation();
    check_unmap(&space_a);
    check_revoke_returns_memory();
    measure_cr3_cost();

    vspace::report();
    sched::report();
    println!("[vspace] all address space tests passed");
}

/// 1. Every VSpace carries the kernel's half, and only the kernel's.
fn check_kernel_half_shared() {
    let entries = vspace::kernel_entry_count();
    assert!(
        entries > 0,
        "the kernel occupies no top-level entries, which cannot be true \
         -- the snapshot was taken before the mappings existed"
    );

    // The kernel's own address space must be the one running now.
    let active = paging::active_table().start().0;
    let kernel = crate::memory::vm::kernel_pml4().0;
    assert_eq!(
        active, kernel,
        "the boot thread is not running in the kernel's address space"
    );

    println!(
        "[vspace] kernel half: {} top-level entries, shared by every \
         address space; cr3 is {:#018x}",
        entries, active
    );
}

/// 2. A VSpace can be built out of capabilities, written to and read back.
fn check_build_space() -> TestSpace {
    let space = build_space(64, PATTERN_A);
    let cnode = object_cnode();

    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(space.root_slot) };

    // The user half has exactly what was put there, and the kernel half
    // is present. Checked through the capability path's own translate,
    // because that is the path a component will use.
    // SAFETY: The slot holds a live PageTable.
    let (phys, flags) = unsafe { vspace::translate(root, VirtAddr::new(TEST_ADDRESS)) }
        .expect("the mapping just made does not resolve");

    // SAFETY: Index in range.
    let frame = unsafe { cnode.slot(space.frame_slot) };
    assert_eq!(
        phys.0,
        frame.object().0 .0,
        "the mapping resolves to {:#018x} rather than the Frame the \
         capability names, {:#018x}",
        phys.0,
        frame.object().0 .0
    );
    assert!(
        flags.contains(paging::PageFlags::USER),
        "a mapping in a user VSpace is not marked USER: {:?}",
        flags
    );
    assert!(
        !flags.contains(paging::PageFlags::EXEC),
        "a user mapping is executable; M7 grants no EXEC at all (ADR 027)"
    );

    println!(
        "[vspace] built from capabilities: {:#018x} -> {} {:?}",
        TEST_ADDRESS, phys, flags
    );

    space
}

/// 3. The capability path and M3's paging agree, because they describe
///    the same tables.
fn check_translate_agrees(space: &TestSpace) {
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let root_slot = unsafe { cnode.slot(space.root_slot) };

    // SAFETY: The slot holds a live PageTable.
    let (cap_phys, cap_flags) =
        unsafe { vspace::translate(root_slot, VirtAddr::new(TEST_ADDRESS)) }
            .expect("capability translate failed");

    // The same question, asked of M3's API directly.
    //
    // SAFETY: The capability names a live PML4 reachable through PMAP.
    let (raw_phys, raw_flags) = unsafe {
        let table =
            &*crate::memory::phys_to_virt(root_slot.object().0).as_ptr::<paging::PageTable>();
        paging::translate(table, VirtAddr::new(TEST_ADDRESS))
    }
    .expect("paging translate failed");

    assert_eq!(
        cap_phys.0, raw_phys.0,
        "the capability path says {:#018x} and the paging layer says \
         {:#018x}; they are reading the same tables and must agree",
        cap_phys.0, raw_phys.0
    );
    assert_eq!(cap_flags, raw_flags, "the two paths disagree about flags");

    println!(
        "[vspace] translate: the capability path and M3's paging agree \
         ({:#018x} -> {}, {:?})",
        TEST_ADDRESS, cap_phys, cap_flags
    );
}

/// 4. SMAP keeps the kernel out of user pages, and the guard lets it
///    back in deliberately.
///
/// This check exists because M7 tripped over it. The known-gaps list
/// had `stac`/`clac` filed under M8, "once user mappings exist" -- but
/// SMAP keys off the page's USER *bit*, not the privilege level of the
/// code touching it. M7 creates USER pages while every thread is still
/// at CPL 0, so the first SMAP violation arrived a milestone before
/// the first ring-3 instruction.
fn check_smap_guards_user_pages(space: &TestSpace) {
    if !cpu::UserAccess::enforcing() {
        // Saying so rather than passing quietly. A machine without
        // SMAP exercises none of this, and a test that printed
        // nothing would let a reader believe otherwise.
        println!(
            "[vspace] smap: NOT ENFORCED on this CPU; the UserAccess \
             guard is a no-op here and this check proves nothing"
        );
        return;
    }

    // AC is clear outside any guard: the kernel's resting state is
    // "cannot touch user pages".
    assert!(
        !cpu::user_access_allowed(),
        "RFLAGS.AC is set in ordinary kernel code; SMAP is protecting \
         nothing, because every stray supervisor access to a user page \
         is already permitted"
    );

    // Inside a guard it is set, and on the way out it is clear again.
    // The restore is the part that matters: a `stac` whose `clac` was
    // skipped by an early return disables SMAP for everything that
    // follows, silently.
    {
        let _user = cpu::UserAccess::new();
        assert!(
            cpu::user_access_allowed(),
            "UserAccess did not set RFLAGS.AC; reads of user pages would \
             still fault"
        );

        // Nested: the inner guard must not revoke the outer one's
        // permission when it drops. Same shape as InterruptGuard's
        // nesting rule (ADR 025).
        {
            let _inner = cpu::UserAccess::new();
        }
        assert!(
            cpu::user_access_allowed(),
            "a nested UserAccess cleared AC when the inner guard \
             dropped, leaving the outer region unable to touch the \
             pages it had just been given access to"
        );
    }
    assert!(
        !cpu::user_access_allowed(),
        "UserAccess did not clear RFLAGS.AC on drop; SMAP is now \
         disabled for every access that follows, and nothing would \
         report it"
    );

    // And the mapping really is USER-marked, which is what makes all
    // of the above apply to it rather than being an abstract exercise.
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(space.root_slot) };
    // SAFETY: The slot holds a live PageTable.
    let (_, flags) = unsafe { vspace::translate(root, VirtAddr::new(TEST_ADDRESS)) }
        .expect("the test mapping does not resolve");
    assert!(
        flags.contains(paging::PageFlags::USER),
        "the mapping is not USER-marked, so SMAP would not apply to it \
         and this check would be vacuous"
    );

    println!(
        "[vspace] smap: enforced; AC is clear in ordinary kernel code, \
         set inside a UserAccess guard, and clear again after it -- \
         nested guards do not revoke the outer one"
    );
}

/// 5. Rights narrow into permissions (ADR 027).
///
/// Three cases, and the third is the one that is easy to get wrong by
/// being too generous: a Frame with no READ cannot be mapped at all.
fn check_rights_narrowing() {
    let cnode = object_cnode();

    // A separate space so the earlier mappings are undisturbed.
    let space = build_space(96, PATTERN_B);

    // Two frames for the rights checks, from wherever there is room.
    let parent_index = untyped_with_room(2 * 4096);
    // SAFETY: Index below the count boot reported.
    let parent = unsafe { boot::root_untyped(parent_index) };
    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(space.root_slot) };

    // Two more frames and the tables to reach a second address.
    // SAFETY: Live Untyped and CNode; the slots are empty.
    unsafe { untyped::retype(parent, CapType::Frame, 0, 2, &cnode, 110) }
        .expect("could not retype the rights-test frames");

    // Read-only: copy the capability down to READ alone.
    // SAFETY: Indices in range, and the two slots are distinct.
    unsafe {
        let ro_source = cnode.slot(110);
        let ro_dest = cnode.slot(112);
        crate::cap::cnode::copy(ro_source, ro_dest, crate::cap::Rights::READ)
    }
    .expect("could not copy a read-only Frame capability");

    // The page next to the one already mapped, so the tables are there.
    let ro_address = VirtAddr::new(TEST_ADDRESS + crate::memory::FRAME_SIZE);
    // SAFETY: Index in range.
    let ro_cap = unsafe { cnode.slot(112) };
    // SAFETY: Both slots hold live capabilities.
    unsafe { vspace::map_frame(root, ro_cap, ro_address, vspace::MapPermissions::ReadOnly) }
        .expect("mapping a read-only Frame");

    // SAFETY: The slot holds a live PageTable.
    let (_, ro_flags) = unsafe { vspace::translate(root, ro_address) }.expect("translate");
    assert!(
        !ro_flags.contains(paging::PageFlags::WRITE),
        "a Frame capability without WRITE mapped writable: {:?}",
        ro_flags
    );
    assert!(
        ro_flags.contains(paging::PageFlags::READ),
        "a READ capability did not map readable: {:?}",
        ro_flags
    );

    // Asking for *more* than the capability holds is refused, not
    // quietly downgraded. M7 downgraded, and ADR 032 changed it: a
    // caller that wanted a writable mapping and silently got a
    // read-only one discovers it at its first store, which is a #PF
    // in a different function with no trace of the real cause.
    let rw_attempt = VirtAddr::new(TEST_ADDRESS + 3 * crate::memory::FRAME_SIZE);
    // SAFETY: Both slots hold live capabilities; expected to fail.
    match unsafe { vspace::map_frame(root, ro_cap, rw_attempt, vspace::MapPermissions::ReadWrite) }
    {
        Err(CapError::InsufficientRights { needed, .. }) => {
            assert_eq!(
                needed,
                crate::cap::Rights::WRITE,
                "the error named the wrong missing right"
            );
        }
        other => panic!(
            "asking for ReadWrite with a READ-only capability returned \
             {:?} instead of InsufficientRights. M7 downgraded silently \
             here; ADR 032 made it an error, because the caller finds \
             out at its first store otherwise.",
            other
        ),
    }

    // No rights at all: refused, rather than mapped unreadable.
    // SAFETY: Indices in range, and the two slots are distinct.
    unsafe {
        let null_source = cnode.slot(111);
        let null_dest = cnode.slot(113);
        crate::cap::cnode::copy(null_source, null_dest, crate::cap::Rights::NONE)
    }
    .expect("could not copy a no-rights Frame capability");

    // SAFETY: Index in range.
    let null_cap = unsafe { cnode.slot(113) };
    let far = VirtAddr::new(TEST_ADDRESS + 2 * crate::memory::FRAME_SIZE);
    // SAFETY: Both slots hold live capabilities; expected to fail.
    match unsafe { vspace::map_frame(root, null_cap, far, vspace::MapPermissions::ReadOnly) } {
        Err(CapError::InsufficientRights { .. }) => {}
        other => panic!(
            "mapping a Frame with no rights returned {:?} instead of \
             InsufficientRights; a mapping nothing may touch is not a \
             weaker mapping, it is a meaningless one",
            other
        ),
    }

    println!(
        "[vspace] rights: WRITE withheld maps read-only ({:?}); no rights \
         at all is refused with InsufficientRights (ADR 027)",
        ro_flags
    );

    // Leave the second space for the isolation check.
    SECOND_SPACE_ROOT.store(space.root_slot as u64, Ordering::Release);
    SECOND_SPACE_FRAME.store(space.frame_slot as u64, Ordering::Release);
}

/// Slots of the space built by `check_rights_narrowing`, reused by the
/// isolation check so it does not have to build a third.
static SECOND_SPACE_ROOT: AtomicU64 = AtomicU64::new(0);
static SECOND_SPACE_FRAME: AtomicU64 = AtomicU64::new(0);
static SECOND_SPACE_UNTYPED: AtomicU64 = AtomicU64::new(0);

/// 5. **The check that cannot be faked.**
///
/// Two threads, two address spaces, one virtual address, two different
/// values. Every other check in this file would pass on a kernel with a
/// single address space and careful bookkeeping; this one would not.
fn check_isolation() {
    A_DONE.store(false, Ordering::Release);
    B_DONE.store(false, Ordering::Release);
    SEEN_BY_A.store(0, Ordering::Release);
    SEEN_BY_B.store(0, Ordering::Release);

    let space_a = TestSpace {
        root_slot: 64,
        frame_slot: 68,
        table_slots: [65, 66, 67],
        // Not torn down by this check, so the Untyped is not consulted.
        untyped_index: usize::MAX,
    };
    let space_b = TestSpace {
        root_slot: SECOND_SPACE_ROOT.load(Ordering::Acquire) as usize,
        frame_slot: SECOND_SPACE_FRAME.load(Ordering::Acquire) as usize,
        table_slots: [97, 98, 99],
        untyped_index: SECOND_SPACE_UNTYPED.load(Ordering::Acquire) as usize,
    };

    // The two spaces must really be different tables, or the rest of
    // this proves nothing.
    let cnode = object_cnode();
    // SAFETY: Indices in range.
    let (root_a, root_b) = unsafe {
        (
            cnode.slot(space_a.root_slot).object().0 .0,
            cnode.slot(space_b.root_slot).object().0 .0,
        )
    };
    assert_ne!(
        root_a, root_b,
        "both threads would use the same root page table; the test could \
         not tell isolation from its absence"
    );

    let tcb_a = build_thread(128, VTHREAD_A_SLOT, thread_a, Some(&space_a));
    let tcb_b = build_thread(160, VTHREAD_B_SLOT, thread_b, Some(&space_b));

    // SetSpace stuck, and `ReadRegisters` reports it. Checked through
    // the invocation rather than the TCB's own field: that is the path
    // a debugger or a parent component will use, and a `SetSpace` that
    // wrote nowhere would look identical from the inside.
    // SAFETY: Indices in range.
    let (regs_a, regs_b) = unsafe {
        (
            tcb::read_registers(cnode.slot(tcb_a)).expect("ReadRegisters A"),
            tcb::read_registers(cnode.slot(tcb_b)).expect("ReadRegisters B"),
        )
    };
    assert_eq!(
        regs_a.vspace_root, root_a,
        "ReadRegisters says thread A is in {:#018x}, but SetSpace was \
         given {:#018x}",
        regs_a.vspace_root, root_a
    );
    assert_eq!(regs_b.vspace_root, root_b, "same for thread B");
    assert_ne!(
        regs_a.vspace_root, 0,
        "thread A is in the kernel's address space; SetSpace did nothing"
    );

    // The generation was recorded too. Zero would mean the staleness
    // check has nothing to compare against, and ADR 028's case B would
    // silently stop working -- a thread would keep running in a
    // revoked address space.
    // SAFETY: Live TCB; no other borrow held.
    let generation = unsafe { tcb_at(tcb_a) }.vspace_generation();
    // SAFETY: Index in range.
    let slot_generation = unsafe { cnode.slot(space_a.root_slot) }.generation();
    assert_eq!(
        generation, slot_generation,
        "the TCB recorded generation {} but the slot is at {}; the \
         staleness check would fire immediately, or never",
        generation, slot_generation
    );

    let cr3_before = sched::cr3_switch_count();

    // SAFETY: Both TCBs are live and neither is queued.
    unsafe {
        sched::enqueue(tcb_at(tcb_a));
        sched::enqueue(tcb_at(tcb_b));
    }

    // Let them both run. Bounded: if a thread never gets to read, this
    // must fail rather than hang.
    let deadline = read_tsc() + 20_000_000_000;
    while (!A_DONE.load(Ordering::Acquire) || !B_DONE.load(Ordering::Acquire))
        && read_tsc() < deadline
    {
        sched::yield_now();
    }

    assert!(
        A_DONE.load(Ordering::Acquire) && B_DONE.load(Ordering::Acquire),
        "a thread never read its address space (A done: {}, B done: {})",
        A_DONE.load(Ordering::Acquire),
        B_DONE.load(Ordering::Acquire)
    );

    let seen_a = SEEN_BY_A.load(Ordering::Acquire);
    let seen_b = SEEN_BY_B.load(Ordering::Acquire);

    assert_eq!(
        seen_a, PATTERN_A,
        "thread A read {:#018x} from {:#018x}; its own address space holds \
         {:#018x} there",
        seen_a, TEST_ADDRESS, PATTERN_A
    );
    assert_eq!(
        seen_b, PATTERN_B,
        "thread B read {:#018x} from {:#018x}; its own address space holds \
         {:#018x} there",
        seen_b, TEST_ADDRESS, PATTERN_B
    );
    assert_ne!(
        seen_a, seen_b,
        "both threads read the same value from {:#018x}; they are sharing \
         an address space and nothing here is isolated",
        TEST_ADDRESS
    );

    let cr3_switches = sched::cr3_switch_count() - cr3_before;
    assert!(
        cr3_switches > 0,
        "two threads in different address spaces ran without CR3 ever \
         being reloaded"
    );

    println!(
        "[vspace] isolation: at {:#018x}, thread A reads {:#018x} and \
         thread B reads {:#018x}; same address, different frames, {} cr3 \
         reloads",
        TEST_ADDRESS, seen_a, seen_b, cr3_switches
    );

    // Stop them before anything else runs.
    // SAFETY: Both TCBs live; no other borrow held.
    unsafe {
        sched::suspend(tcb_at(tcb_a));
        sched::suspend(tcb_at(tcb_b));
    }
}

/// 6. Unmap returns the frame and the page stops resolving.
fn check_unmap(space: &TestSpace) {
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(space.root_slot) };
    // SAFETY: Index in range.
    let frame = unsafe { cnode.slot(space.frame_slot) };
    let expected = frame.object().0 .0;

    let address = VirtAddr::new(TEST_ADDRESS);

    // SAFETY: The slot holds a live PageTable.
    let returned = unsafe { vspace::unmap_frame(root, address) }.expect("unmap failed");
    assert_eq!(
        returned.0, expected,
        "unmap returned {:#018x}, not the frame that was mapped \
         ({:#018x})",
        returned.0, expected
    );

    // The address no longer resolves.
    // SAFETY: The slot holds a live PageTable.
    match unsafe { vspace::translate(root, address) } {
        Err(CapError::NotMapped { .. }) => {}
        other => panic!(
            "after unmap, {:#018x} resolves to {:?}; the mapping is still \
             there",
            TEST_ADDRESS, other
        ),
    }

    // The Frame capability is untouched -- unmapping is not destruction.
    assert!(
        !frame.is_empty() && frame.cap_type() == CapType::Frame,
        "unmap destroyed the Frame capability; it belongs to whoever \
         holds it, and removing a mapping does not take it away"
    );

    // And an intermediate table comes back the same way (ADR 015's
    // question, answered by changing the owner rather than counting
    // references).
    // SAFETY: Index in range.
    let pt = unsafe { cnode.slot(space.table_slots[2]) };
    let pt_object = pt.object().0 .0;
    // SAFETY: The slot holds a live PageTable.
    let pt_returned = unsafe { vspace::unmap_table(root, address, 1) }.expect("unmap_table failed");
    assert_eq!(
        pt_returned.0, pt_object,
        "unmap_table returned {:#018x} rather than the PT that was mapped \
         ({:#018x})",
        pt_returned.0, pt_object
    );
    assert!(
        !pt.is_empty() && pt.cap_type() == CapType::PageTable,
        "unmapping a page table destroyed its capability"
    );

    println!(
        "[vspace] unmap: the Frame and the PT both come back to their \
         holders and the address stops resolving; the kernel frees \
         neither (ADR 015 closed by ADR 027)"
    );
}

/// 7. Revoking a VSpace returns its memory to the Untyped.
fn check_revoke_returns_memory() {
    let cnode = object_cnode();

    let space = build_space(192, PATTERN_A);

    // The Untyped the space came out of. Read through `space` rather
    // than searched again: `build_space` may well have taken the last
    // room in it, so a fresh search would find a different one.
    // SAFETY: Index below the count boot reported.
    let parent = unsafe { boot::root_untyped(space.untyped_index) };
    // SAFETY: Live Untyped and CNode.
    let after_build = unsafe { untyped::Untyped::from_slot(parent) }
        .expect("the parent is not an Untyped")
        .watermark();
    assert!(
        after_build > 0,
        "building an address space consumed no memory"
    );

    // Nothing is running in it, so this is ADR 028's case B.
    //
    // The *Untyped* is revoked, not the root PageTable. They are
    // siblings -- both children of this Untyped -- so revoking the
    // root would reach neither the tables nor the Frame. The first
    // version of this test did exactly that and reported "0
    // capabilities deleted", which is how the mistake was found.
    // SAFETY: Indices in range; the slots hold live capabilities.
    let report = unsafe {
        let root = cnode.slot(space.root_slot);
        let untyped_slot = boot::root_untyped(space.untyped_index);
        vspace::revoke_vspace(untyped_slot, root)
    }
    .expect("revoke failed");

    // SAFETY: As above.
    let after_revoke = unsafe { untyped::Untyped::from_slot(parent) }
        .expect("the parent is not an Untyped")
        .watermark();

    assert!(
        report.deleted > 0,
        "revoke deleted nothing; the address space's objects are not \
         descendants of the Untyped they were retyped from"
    );
    assert!(
        after_revoke < after_build,
        "the watermark did not go back ({} -> {}); the memory was not \
         returned and a later retype cannot reuse it",
        after_build,
        after_revoke
    );

    // The root capability is gone with the rest.
    // SAFETY: Index in range.
    let root_after = unsafe { cnode.slot(space.root_slot) };
    assert!(
        root_after.is_empty(),
        "the root PageTable capability survived the revoke of its own \
         Untyped"
    );

    println!(
        "[vspace] revoke: {} capabilities deleted, watermark {} -> {}; \
         the page tables are Untyped memory again and the root \
         capability is gone",
        report.deleted, after_build, after_revoke
    );
}

/// The measurement: what does changing address space cost?
fn measure_cr3_cost() {
    MEASURE_STOP.store(false, Ordering::Release);

    // Partner one: the same address space as this thread (the kernel's).
    let same = build_thread(232, 8, thread_yield_forever, None);

    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(tcb_at(same));
    }

    let skipped_before = sched::cr3_skipped_count();
    let start = read_tsc();
    for _ in 0..(MEASURE_SWITCHES / 2) {
        sched::yield_now();
    }
    let same_cycles = read_tsc() - start;
    let skipped = sched::cr3_skipped_count() - skipped_before;

    // SAFETY: Live TCB.
    unsafe {
        sched::suspend(tcb_at(same));
    }

    // Partner two: its own address space, so every switch reloads CR3.
    //
    // Slot numbers chosen to stay inside the object CNode's 256 slots:
    // the first draft used 256 and 288, which is how `CPtrOutOfRange`
    // came to be exercised by accident.
    let space = build_space(208, PATTERN_B);
    let other = build_thread(216, 9, thread_yield_forever, Some(&space));

    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(tcb_at(other));
    }

    let reloads_before = sched::cr3_switch_count();
    let start = read_tsc();
    for _ in 0..(MEASURE_SWITCHES / 2) {
        sched::yield_now();
    }
    let other_cycles = read_tsc() - start;
    let reloads = sched::cr3_switch_count() - reloads_before;

    // SAFETY: Live TCB.
    unsafe {
        sched::suspend(tcb_at(other));
    }

    // Both counters must have moved, and this is the check that makes
    // the "only reload when it changes" rule testable at all. A kernel
    // that reloaded on every switch would have `skipped == 0`; one that
    // never reloaded would have `reloads == 0`. Requiring both rules
    // out both.
    assert!(
        skipped > 0,
        "no switch skipped the CR3 reload, so the kernel is reloading it \
         even between threads that share an address space -- which throws \
         away the whole non-global TLB for nothing (ADR 027)"
    );
    assert!(
        reloads > 0,
        "no switch reloaded CR3 even though the partner had its own \
         address space"
    );

    let per_same = same_cycles / MEASURE_SWITCHES.max(1);
    let per_other = other_cycles / MEASURE_SWITCHES.max(1);

    println!(
        "[vspace] cr3 cost: {} cycles/switch within one address space \
         ({} skipped), {} cycles/switch across two ({} reloads)",
        per_same, skipped, per_other, reloads
    );
    println!(
        "[vspace] cr3 cost: the difference is {} cycles per switch -- \
         measured under emulation, where QEMU's TLB is not silicon and a \
         miss costs what the interpreter charges rather than what the \
         hardware would. Expect this to be larger on real machines, \
         because the real cost is the cold TLB afterwards rather than \
         the write itself.",
        per_other.saturating_sub(per_same)
    );
}

// ---- Negative scenarios ------------------------------------------------

/// A `FrameSource` that yields nothing, for calling `paging::map`
/// directly without letting it allocate.
struct NoFramesHere;

impl paging::FrameSource for NoFramesHere {
    fn alloc_frame(&mut self) -> Option<crate::memory::PhysFrame> {
        None
    }
}

/// Map a Frame into the kernel half of a user VSpace.
///
/// This must be **refused, not fatal**: the caller is wrong, the kernel
/// is fine, and from M8 the caller is a user component. The scenario
/// therefore exits 33.
pub fn run_kernel_half() -> ! {
    println!("[vspace] mapping into the kernel half must be refused");

    let space = build_space(64, PATTERN_A);
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(space.root_slot) };
    // SAFETY: Index in range.
    let frame = unsafe { cnode.slot(space.frame_slot) };

    // Four addresses in the kernel half, each one somewhere a mistake
    // might plausibly land.
    let targets = [
        (layout::KERNEL_MIN, "the first kernel address"),
        (layout::PMAP_BASE + 0x1000, "inside the physical window"),
        (layout::KHEAP_START, "the kernel heap"),
        (layout::KIMAGE_BASE, "the kernel image"),
    ];

    for (address, what) in targets {
        // SAFETY: Both slots hold live capabilities; expected to fail.
        let result = unsafe {
            vspace::map_frame(
                root,
                frame,
                VirtAddr::new(address),
                vspace::MapPermissions::ReadWrite,
            )
        };
        match result {
            Err(CapError::NotUserAddress { address: reported }) => {
                assert_eq!(reported, address, "the error named the wrong address");
                println!(
                    "[vspace] refused {:#018x} ({}): NotUserAddress",
                    address, what
                );
            }
            other => panic!(
                "mapping at {:#018x} ({}) returned {:?} instead of \
                 NotUserAddress. The kernel's top-level entries are shared \
                 by every address space, so this mapping would have edited \
                 tables that every other VSpace walks -- not an isolation \
                 failure but a corrupted kernel (ADR 027, K1).",
                address, what, other
            ),
        }
    }

    // Which guard fired? Showing that needs a second, distinguishable
    // error -- and finding that out cost a mutation test.
    //
    // `paging::map` also refuses a kernel address, with its own
    // `WrongHalf`. Deleting the capability layer's check and re-running
    // this scenario left it **passing**, because the paging layer
    // refused the same four addresses and both were being reported as
    // `NotUserAddress`. The check was indistinguishable from its own
    // absence, which ADR 023's K5 says is no check at all.
    //
    // The two now have separate error values, and they are checking
    // genuinely different things. `NotUserAddress` is about ownership:
    // a user VSpace may not edit the kernel half, because those tables
    // are shared by every address space. `WrongHalf` is about a single
    // request being internally inconsistent -- the USER flag and the
    // address disagreeing about which half this is.
    //
    // Reaching the second one took two corrections, both from running
    // it:
    //
    //   * `KERNEL_MIN + 0x2000` is inside PMAP and already mapped, so
    //     `AlreadyMapped` came back before the half check ran;
    //   * a kernel address with USER *withheld* is perfectly
    //     consistent -- both say "kernel" -- so there is no
    //     disagreement to report, and the walk proceeded until it
    //     needed a frame.
    //
    // What provokes it is a USER mapping at a kernel address: the
    // flags claim user, the address says kernel. An unmapped corner
    // of KMMIO, which nothing else touches.
    let unmapped_kernel_address = layout::KMMIO_BASE + 0x1000_0000;

    // SAFETY: The capability names a live PML4 reachable through PMAP,
    // and no other mutable borrow of it exists here. The call is
    // expected to fail before touching anything.
    let flag_result = unsafe {
        let root_table =
            &mut *crate::memory::phys_to_virt(root.object().0).as_ptr::<paging::PageTable>();
        let mut nothing = NoFramesHere;
        paging::map(
            root_table,
            VirtAddr::new(unmapped_kernel_address),
            crate::memory::PhysFrame::containing(frame.object().0),
            paging::PageFlags::READ | paging::PageFlags::WRITE | paging::PageFlags::USER,
            &mut nothing,
        )
    };
    match flag_result {
        Err(paging::MapError::WrongHalf { .. }) => {
            println!(
                "[vspace] and the paging layer's own half check is still \
                 there and still separate: WrongHalf for a USER mapping at \
                 a kernel address"
            );
        }
        Err(other) => panic!(
            "the paging layer returned {:?} for a USER mapping at a kernel \
             address, where WrongHalf was expected. If the two half checks \
             have collapsed into one, only one of them is being tested and \
             deleting the other would go unnoticed (ADR 023, K5).",
            other
        ),
        Ok(tlb) => {
            tlb.ignore_because_caller_will_flush_all();
            panic!(
                "the paging layer *accepted* a mapping at a kernel address. \
                 Its half check is gone, and with it the second of the two \
                 independent guards on the kernel's shared tables."
            );
        }
    }

    // And the kernel's own mappings are untouched: the refusals did not
    // get halfway. Reading the heap proves it, because a damaged PML4
    // entry for KHEAP would fault here.
    let probe = layout::KHEAP_START as *const u64;
    // SAFETY: The heap's first page is mapped and readable.
    let value = unsafe { probe.read_volatile() };
    core::hint::black_box(value);

    println!(
        "[vspace] all four refused and the kernel's own mappings still \
         work; this is an error path, not a fault"
    );

    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

/// Touch an address that is mapped in another thread's VSpace but not
/// in this one.
///
/// Expected: #PF with CR2 at that address. This is isolation shown from
/// the failing side -- `check_isolation` shows two threads reading
/// different values, and this shows that reaching into the other one is
/// not merely discouraged.
pub fn run_isolation() -> ! {
    println!("[vspace] reading an address mapped only in another VSpace");

    // Two spaces. The second maps FAR_ADDRESS; the first does not.
    let space_a = build_space(64, PATTERN_A);
    let space_b = build_space(96, PATTERN_B);

    let cnode = object_cnode();
    // SAFETY: Indices in range.
    let root_b = unsafe { cnode.slot(space_b.root_slot) };

    // Give B a mapping at FAR_ADDRESS, which needs its own tables --
    // 512 GiB away is a different PDPT entry, so nothing on the path
    // to it already exists. That is the point of the address.
    let far_index = untyped_with_room(4 * 4096);
    // SAFETY: Index below the count; live Untyped and CNode; the
    // destination slots are empty.
    unsafe {
        let parent = boot::root_untyped(far_index);
        untyped::retype(parent, CapType::PageTable, 0, 3, &cnode, 120)
    }
    .expect("could not retype the far tables");
    // SAFETY: As above.
    unsafe {
        let parent = boot::root_untyped(far_index);
        untyped::retype(parent, CapType::Frame, 0, 1, &cnode, 123)
    }
    .expect("could not retype the far frame");

    let far = VirtAddr::new(FAR_ADDRESS);
    for (index, level) in [3u8, 2, 1].iter().enumerate() {
        // SAFETY: Index in range.
        let table = unsafe { cnode.slot(120 + index) };
        // SAFETY: Both slots hold live capabilities.
        unsafe { vspace::map_table(root_b, table, far, *level) }
            .expect("could not map a far table");
    }
    // SAFETY: Index in range.
    let far_frame = unsafe { cnode.slot(123) };
    // SAFETY: Both slots hold live capabilities.
    unsafe { vspace::map_frame(root_b, far_frame, far, vspace::MapPermissions::ReadWrite) }
        .expect("could not map the far frame");

    // B can see it; A cannot. Checked through translate first, so the
    // log carries the evidence before the fault arrives.
    // SAFETY: Both slots hold live PageTables.
    let in_b = unsafe { vspace::translate(root_b, far) };
    // SAFETY: Index in range.
    let root_a = unsafe { cnode.slot(space_a.root_slot) };
    // SAFETY: As above.
    let in_a = unsafe { vspace::translate(root_a, far) };

    println!(
        "[vspace] {:#018x} in B: {:?}; in A: {:?}",
        FAR_ADDRESS,
        in_b.map(|(phys, _)| phys),
        in_a.map(|(phys, _)| phys)
    );
    assert!(in_b.is_ok(), "B should have the mapping");
    assert!(in_a.is_err(), "A should not have the mapping");

    // Now switch into A and touch it. The fault is the point.
    let tcb_a = build_thread(128, VTHREAD_A_SLOT, thread_touch_far, Some(&space_a));

    println!(
        "[vspace] switching into A and reading {:#018x} (must fault)",
        FAR_ADDRESS
    );

    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(tcb_at(tcb_a));
    }

    let deadline = read_tsc() + 20_000_000_000;
    while read_tsc() < deadline {
        sched::yield_now();
    }

    unreachable!(
        "the thread read an address its address space does not map, and \
         did not fault; the two VSpaces are not isolated"
    );
}

/// Read FAR_ADDRESS, which this thread's VSpace does not map.
extern "C" fn thread_touch_far() -> ! {
    println!(
        "[vspace] thread running in its own VSpace (cr3 {:#018x}); \
         reading {:#018x}",
        read_cr3(),
        FAR_ADDRESS
    );

    // The guard is held across the faulting read, and that is
    // load-bearing rather than tidy.
    //
    // Without it SMAP would refuse the access first, and the fault
    // would be a *protection violation* on an address the test claims
    // is not mapped at all. The scenario would still end in #PF at the
    // right CR2 and would still look like a pass -- while proving
    // nothing whatever about address space isolation. The error code
    // is what distinguishes the two, and `run.py` requires 0:
    // not-present, not a permission failure.
    let _user = cpu::UserAccess::new();

    // SAFETY: Not safe, and that is the point. The address is mapped
    // in the *other* VSpace and deliberately absent from this one, so
    // the read must raise #PF with CR2 set to it.
    let value = unsafe { (FAR_ADDRESS as *const u64).read_volatile() };

    unreachable!(
        "reading an address this VSpace does not map returned {:#018x} \
         instead of faulting; the two address spaces are not isolated",
        value
    );
}

/// Revoke the VSpace a running thread is using.
///
/// ADR 028 case A: refused, because the page tables are in CR3 and
/// revoke would hand them back to the Untyped while the CPU walks them.
/// Case B -- revoking the VSpace of a thread that is *not* running --
/// succeeds, and the thread is stopped when something next tries to
/// switch into it.
///
/// Exits 35 by panicking at the end, so the refusal above is asserted
/// on a panic-expecting run.
pub fn run_stale_cr3() -> ! {
    println!("[vspace] revoking a VSpace that is in use");

    let space = build_space(64, PATTERN_A);
    let cnode = object_cnode();

    // --- Case A: the VSpace the running thread is using ---------------
    //
    // Move this thread into it first, so that CR3 really points there.
    let boot_tcb_slot = crate::cap::boot::BOOT_TCB_SLOT;
    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(space.root_slot) };

    // The boot thread is Running, so SetSpace is refused -- which is
    // itself worth showing, because it is why case A cannot trap a
    // caller.
    // SAFETY: Index in range.
    let boot_slot = unsafe { cnode.slot(boot_tcb_slot) };
    // SAFETY: Both slots hold live capabilities; expected to fail.
    match unsafe { tcb::set_space(boot_slot, Some(root), None) } {
        Err(CapError::ThreadBusy { .. }) => {
            println!(
                "[vspace] SetSpace on the running thread: ThreadBusy, as \
                 ADR 026 requires"
            );
        }
        other => panic!("SetSpace on a running thread returned {:?}", other),
    }

    // So use a second thread instead: give it the VSpace, run it, and
    // have it try to revoke its own address space from the inside --
    // the only place from which case A's "is this root in CR3" test
    // can be true of a user VSpace.
    let victim = build_thread(128, VTHREAD_A_SLOT, thread_hold_vspace, Some(&space));

    // The thread entry point takes no arguments, so the slots it needs
    // travel through statics.
    VICTIM_ROOT_SLOT.store(space.root_slot as u64, Ordering::Release);
    VICTIM_UNTYPED.store(space.untyped_index as u64, Ordering::Release);

    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(tcb_at(victim));
    }
    sched::yield_now();

    // --- Case A: revoke the VSpace that is in CR3 right now -----------
    //
    // This cannot be provoked from here. Control being on this line
    // means the scheduler switched back, which means CR3 holds the
    // kernel's PML4 again -- and the kernel's PML4 has no capability
    // naming it, because `vm::init` built it rather than retyping it.
    // (That is also why the boot thread's address space can never be
    // revoked: there is nothing to revoke.)
    //
    // So the attempt has to come from inside. The victim asks to
    // destroy the address space it is executing in, which is the one
    // situation where ADR 028's "is this root in CR3" test can be true
    // of a user VSpace.
    REVOKE_OWN_VSPACE.store(true, Ordering::Release);
    // SAFETY: Live TCB in a resumable state.
    unsafe {
        let tcb = tcb_at(victim);
        let _ = sched::resume(tcb);
    }
    sched::yield_now();

    // Bounded wait for the victim to report back.
    let deadline = read_tsc() + 20_000_000_000;
    while !OWN_REVOKE_DONE.load(Ordering::Acquire) && read_tsc() < deadline {
        sched::yield_now();
    }
    assert!(
        OWN_REVOKE_DONE.load(Ordering::Acquire),
        "the victim never attempted to revoke its own address space"
    );
    assert!(
        OWN_REVOKE_REFUSED.load(Ordering::Acquire),
        "a thread revoked the address space it was running in. The page \
         tables would have gone back to their Untyped while the CPU was \
         still walking them -- not a fault but undefined behaviour, and \
         one no interrupt could report, because reaching the IDT goes \
         through those same tables (ADR 028, case A)."
    );

    // --- Case B: revoke it while the victim is suspended --------------
    //
    // SAFETY: Live TCB.
    unsafe {
        sched::suspend(tcb_at(victim));
    }

    // SAFETY: Indices in range; the slots hold live capabilities, and
    // nothing is running in this address space now.
    let report = unsafe {
        let root = cnode.slot(space.root_slot);
        let untyped_slot = boot::root_untyped(space.untyped_index);
        vspace::revoke_vspace(untyped_slot, root)
    }
    .expect("revoke of an idle VSpace");
    println!(
        "[vspace] revoked an idle VSpace: {} capabilities deleted",
        report.deleted
    );

    // Now try to run the victim again. ADR 028: it must be stopped
    // rather than switched into.
    let stale_before = sched::stale_vspace_count();

    // SAFETY: Live TCB in a resumable state.
    unsafe {
        let tcb = tcb_at(victim);
        let _ = sched::resume(tcb);
    }
    sched::yield_now();

    let stale_after = sched::stale_vspace_count();
    assert!(
        stale_after > stale_before,
        "the scheduler switched into a thread whose address space had been \
         revoked; the CPU would have walked page tables that belong to \
         somebody else now (ADR 028)"
    );

    // SAFETY: Live TCB.
    let state = unsafe { tcb_at(victim) }.state();
    assert_eq!(
        state,
        ThreadState::Suspended,
        "the thread with the revoked VSpace is {} rather than Suspended",
        state.name()
    );

    println!(
        "[vspace] the thread was suspended rather than run: {} stale \
         detections, state {} (ADR 028)",
        stale_after - stale_before,
        state.name()
    );

    panic!(
        "vspace-stale-cr3 completed its checks; exiting 35 by design so \
         that the ADR 028 paths are asserted on a panic-expecting run"
    );
}

/// Set by the main thread to ask the victim to revoke its own VSpace.
static REVOKE_OWN_VSPACE: AtomicBool = AtomicBool::new(false);
/// Set by the victim once it has tried.
static OWN_REVOKE_DONE: AtomicBool = AtomicBool::new(false);
/// Whether the attempt was refused, as ADR 028 case A requires.
static OWN_REVOKE_REFUSED: AtomicBool = AtomicBool::new(false);

/// Which slots the victim's address space occupies, so the thread can
/// reach them without arguments -- an entry point takes none.
static VICTIM_ROOT_SLOT: AtomicU64 = AtomicU64::new(0);
static VICTIM_UNTYPED: AtomicU64 = AtomicU64::new(0);

/// Sit in a VSpace, yielding; when asked, try to revoke it from the
/// inside.
///
/// This is the only honest way to reach ADR 028's case A. The check is
/// "is this root the one in CR3", and the only code for which that can
/// be true of a *user* VSpace is code running in it.
extern "C" fn thread_hold_vspace() -> ! {
    println!("[vspace] victim running with cr3 {:#018x}", read_cr3());

    loop {
        if REVOKE_OWN_VSPACE.load(Ordering::Acquire) && !OWN_REVOKE_DONE.load(Ordering::Acquire) {
            let cnode = object_cnode();
            let root_slot = VICTIM_ROOT_SLOT.load(Ordering::Acquire) as usize;
            let untyped_index = VICTIM_UNTYPED.load(Ordering::Acquire) as usize;

            // SAFETY: Indices in range; the slots hold live
            // capabilities. Expected to be refused -- this thread is
            // running in the address space it is asking to destroy.
            let result = unsafe {
                let root = cnode.slot(root_slot);
                let untyped_slot = boot::root_untyped(untyped_index);
                vspace::revoke_vspace(untyped_slot, root)
            };

            match result {
                Err(CapError::VSpaceInUse { root }) => {
                    println!(
                        "[vspace] a thread tried to revoke the address space \
                         it is running in (root {:#018x}): VSpaceInUse, as \
                         ADR 028 case A requires",
                        root
                    );
                    OWN_REVOKE_REFUSED.store(true, Ordering::Release);
                }
                other => {
                    println!(
                        "[vspace] revoking the running address space returned \
                         {:?} instead of VSpaceInUse",
                        other.map(|report| report.deleted)
                    );
                }
            }

            OWN_REVOKE_DONE.store(true, Ordering::Release);
        }

        sched::yield_now();
    }
}
