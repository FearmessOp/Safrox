// SPDX-License-Identifier: BSD-2-Clause
//! Starting the root task, and then stopping (ADR 044).
//!
//! **This module is the whole of what the kernel does for user space
//! at boot, and its length is the point.** Everything in it runs
//! once, before the seal; after that the kernel has no code that
//! could build a component even if something asked it to.
//!
//! Six steps:
//!
//! 1. build a CSpace for the root task — one root CNode, one leaf;
//! 2. put **every** root Untyped and the IRQControl into that leaf;
//! 3. build an address space: code, stack, BootInfo;
//! 4. write BootInfo, describing all of the above;
//! 5. create a thread at [`abi::ROOT_CODE_ADDRESS`] and start it;
//! 6. [`seal`] — and from here the kernel only answers syscalls and
//!    delivers interrupts.
//!
//! # What the kernel does *not* do
//!
//! It does not create the root task's children, map their pages,
//! connect them, or know they exist. That is [`crate::roottask`]'s
//! counterpart in `roottask/src/main.rs`, which is a separate crate
//! that links against `core` and the ABI and nothing else — so the
//! claim "the kernel contains no component-building code" is
//! enforced by the linker rather than asserted in a comment.
//!
//! # The accounting
//!
//! Criterion 3 asks that no frame disappear. Writing that check
//! required finding out that **it is not true by construction**, and
//! the shape of this module's arithmetic follows from that. See
//! [`Accounting`].

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use safrox_abi as abi;

use crate::cap::cnode::{self, CNode};
use crate::cap::{boot, untyped, CapType, ObjectRef};
use crate::memory::{layout, PhysFrame, VirtAddr, FRAME_SIZE};
use crate::println;

/// The root task's program, built by `scripts/build-roottask.sh` and
/// embedded in `.rodata` (ADR 044).
///
/// `include_bytes!` rather than a Limine module: ADR 002's "one
/// binary" survives, and the dependency is resolved at compile time
/// where a missing file is a build error rather than a boot-time
/// one.
///
/// The build script writes this file before the kernel is compiled;
/// the Makefile's ordering is what guarantees it exists.
static ROOT_TASK_IMAGE: &[u8] = include_bytes!("../../build/roottask.bin");

/// How many pages the root task's image occupies.
///
/// Two, and the figure is measured rather than assumed: the image is
/// 7432 bytes, because a static binary still carries a `.got`. The
/// first version of this said one page and would have mapped the
/// program with its tail missing — an illegal instruction a long way
/// from the cause.
///
/// Checked against the real size below, so the constant cannot drift
/// away from the file.
/// **Four from M13**, and the increase is a real one rather than
/// headroom. The root task now embeds the serial driver's flat image
/// in its own `.rodata` (`include_bytes!`), because the kernel has
/// no ELF loader and must not grow one — loading is a component's
/// job (ADR 043), so the component that loads the driver has to
/// carry it.
///
/// Two pages failed the link, which is the guard working: the
/// message named the limit rather than producing a program with its
/// tail missing.
const ROOT_CODE_PAGES: usize = 8;

const _: () = assert!(
    ROOT_CODE_PAGES * 4096 >= 21000,
    "ROOT_CODE_PAGES is too small for the root task image"
);

/// Pages of stack the root task gets.
///
/// **Four from M14, and one before that** — a number that had no
/// name until it stopped being enough.
///
/// The root task hand-assembles its children's programs into
/// `Code` buffers, which are 1032 bytes each and live on the stack
/// because this component has no writable statics and no heap (see
/// `roottask/link.ld`). M14 added a third and fourth scenario
/// branch, LLVM gave `main` a frame large enough for all of their
/// locals at once, and the frame crossed a page.
///
/// The symptom was a #PF at `rip=0x400311` with `cr2 == rsp ==
/// 0x4fffb0`, sixteen bytes below the stack's base — inside LLVM's
/// **stack probe**, the `sub $0x1000; movq $0,(%rsp)` sequence it
/// emits precisely so that a frame larger than a page touches each
/// page in turn rather than skipping over a guard. The probe did
/// its job: it found the end of the stack before anything was
/// written past it.
///
/// Four rather than two, because the next scenario should not have
/// to rediscover this. The cost is three frames of a 256 MiB
/// machine.
const ROOT_STACK_PAGES: usize = 4;

/// Radix of the root task's root CNode: 4 slots.
///
/// Small deliberately. ADR 018 fixes the hierarchy at two levels, so
/// the root is an index of CNodes rather than a store of
/// capabilities, and the root task needs exactly one leaf.
const ROOT_TASK_ROOT_RADIX: u8 = 2;

/// Radix of its leaf: 512 slots.
///
/// It has to hold every root Untyped — about 90 on a 256 MiB
/// machine — plus the IRQControl, its own TCB, VSpace and CNode, and
/// then every capability it creates for its children. 512 is the
/// first power of two with room for all of that.
const ROOT_TASK_LEAF_RADIX: u8 = 9;

/// Index of the leaf within the root task's root CNode.
///
/// Zero, and the root task hard-codes the same number: its
/// [`CPtr::new(0, 0, index, 0)`] names leaf 0 with both generations
/// zero. That works because this CNode is freshly retyped and the
/// root task never revokes.
const LEAF_INDEX: usize = 0;

// ---- The leaf's layout -------------------------------------------------
//
// Fixed offsets, reported to the root task in BootInfo rather than
// agreed by both sides independently. The root task reads
// `untyped_first_slot` and friends; these constants are the kernel's
// side of that one statement.

/// Leaf slot holding the root task's own root CNode.
const SLOT_OWN_CNODE: usize = 0;
/// Its own TCB.
const SLOT_OWN_TCB: usize = 1;
/// Its own VSpace root.
const SLOT_OWN_VSPACE: usize = 2;
/// The one IRQControl (ADR 041).
const SLOT_IRQ_CONTROL: usize = 3;
/// The one IOPortControl (M13, ADR 045).
///
/// Slot 4: 0..3 are taken and the Untypeds start at 8, so the gap
/// was left for exactly this.
const SLOT_IOPORT_CONTROL: usize = 4;
/// First slot of the handed-over Untypeds. They are consecutive.
const SLOT_UNTYPED_FIRST: usize = 8;

/// KSTACK slot for the root task's kernel stack.
///
/// 13: slots 0 and 1 are IST1 and main, M7 uses 6..9, M8 uses 10 and
/// 11, M11 uses 12.
const ROOT_KSTACK_SLOT: u64 = 13;

/// Object-CNode slots this module uses for the root task's own
/// objects.
///
/// These live in the *kernel's* CSpace, not the root task's: they are
/// how the kernel names what it is building. 20 and up, clear of
/// M8's block (64..160) and M11's (180..234).
///
/// # The layout is written out, not computed
///
/// Each constant is derived from the one before it and the count it
/// needs, and every count is named. M11 learned this the hard way:
/// its component B's address space ran from 211 and needed 16, so it
/// reached 227 and collided with the kernel-stack frames at 228. The
/// symptom was `SlotNotEmpty` from inside `build_user_space` -- a
/// fixture failure reported as a retype failure.
///
/// M12 then repeated it in miniature. [`KSLOT_TABLES`] was given
/// three slots on the belief that three page tables were enough; it
/// is four (see the mapping code), so the fourth would have been
/// written over [`KSLOT_CODE`] and the root task's program loaded
/// into a page table.
///
/// Hence `TABLE_COUNT` below: the number appears once, in the slot
/// map and in the retype, and the two cannot drift.
const KSLOT_BASE: usize = 20;

/// Intermediate page tables: a shared PDPT, a shared PD, and **two**
/// PTs -- one for 4..6 MiB (code and stack) and one for 6..8 MiB
/// (BootInfo). A PT covers 2 MiB, which is what makes it two.
const TABLE_COUNT: usize = 4;

/// Frames for the kernel stack.
const KSTACK_FRAMES: usize = 4;

const KSLOT_ROOT_CNODE: usize = KSLOT_BASE;
const KSLOT_LEAF_CNODE: usize = KSLOT_ROOT_CNODE + 1;
const KSLOT_TCB: usize = KSLOT_LEAF_CNODE + 1;
const KSLOT_VSPACE: usize = KSLOT_TCB + 1;
const KSLOT_TABLES: usize = KSLOT_VSPACE + 1;
const KSLOT_CODE: usize = KSLOT_TABLES + TABLE_COUNT;
const KSLOT_STACK: usize = KSLOT_CODE + ROOT_CODE_PAGES;
const KSLOT_BOOTINFO: usize = KSLOT_STACK + ROOT_STACK_PAGES;
const KSLOT_KSTACK: usize = KSLOT_BOOTINFO + 1;
/// One past the last slot this module touches.
const KSLOT_END: usize = KSLOT_KSTACK + KSTACK_FRAMES;

// The kernel's object CNode has 256 slots; M8 uses 64..160 and M11
// uses 180..234. This block runs from 20 and must stop before 64.
//
// Checked rather than believed, and checked against **the other
// milestones' bases** rather than against 256 alone: an upper bound
// of 256 would be satisfied by a block that overlapped M8's
// entirely. The failure that produces is `SlotNotEmpty` from inside
// a retype, which reads as a capability bug rather than a layout
// one.
const _: () = assert!(
    KSLOT_END <= 64,
    "the root task's kernel-side slots run into M8's block at 64"
);
const _: () = assert!(
    KSLOT_END <= 256,
    "the root task's kernel-side slots run past the object CNode"
);

/// Bytes the root task's own objects cost.
///
/// A CNode of radix 9 is 512 * 32 = 16 KiB, the root 4 * 32 = 128 B
/// (rounded to a page), a TCB 2 KiB, four kernel-stack pages 16 KiB,
/// four address-space pages, three tables. Round up generously: this
/// comes out of one Untyped and a shortfall here is a boot failure,
/// not a component's problem.
const ROOT_TASK_BYTES: u64 = 256 * 1024;

// ---- Accounting --------------------------------------------------------

/// What became of every frame the allocator ever held.
///
/// # Why this is measured rather than derived
///
/// The obvious implementation of criterion 3 is to report
/// `kernel = usable - untyped` and let the root task check
/// `untyped + kernel == usable`. That identity is **true by
/// construction**: it cannot fail, so it tests nothing. ADR 044
/// called a single "available to you" figure unfalsifiable, and a
/// derived one is the same thing wearing arithmetic.
///
/// So the three figures come from three different places:
///
/// * `usable` from the **bitmap** — how many frames were free after
///   `memory::init` plus everything `reclaim` donated;
/// * `untyped` from the **capabilities** — summed over the root
///   Untypeds, which is what the root task actually holds;
/// * `kernel` from **call counting** — `allocate` and
///   `allocate_specific` increment a counter at the call site
///   (ADR 022), so this is what the kernel *asked for* rather than
///   what the bitmap says it got.
///
/// A frame that left the pool without going through those entry
/// points, or an Untyped covering memory nobody allocated, makes the
/// three disagree.
///
/// # The fourth figure
///
/// They disagree anyway, and the remainder is real. `cap::boot`
/// builds Untypeds of `2^k` bytes at `2^k` boundaries (ADR 020); a
/// free frame that fits no such block, or that arrives once the
/// Untyped CNode's 256 slots are full, ends up in no Untyped. After
/// `retire()` nothing can reach it again.
///
/// That is [`lost`](Self::lost), and it is reported rather than
/// absorbed. Folding it into `kernel` would restore the tidy
/// identity and hide the fact.
#[derive(Clone, Copy)]
pub struct Accounting {
    /// Frames the allocator ever held, from the bitmap.
    pub usable: u64,
    /// Frames now covered by root Untypeds.
    pub untyped: u64,
    /// Frames the kernel allocated for itself, call-counted.
    pub kernel: u64,
    /// Frames in neither, and now unreachable.
    pub lost: u64,
}
/// Object-CNode index of the root task's TCB.
///
/// Exposed so `root-dies` can delete it. That the kernel still holds
/// this capability is what makes the negative scenario possible at
/// all — and it is unreachable from ring 3, because a CPtr is
/// meaningful only relative to a root (ADR 018) and no component
/// holds the init CSpace's.
pub const ROOT_TCB_SLOT: usize = KSLOT_TCB;

impl Accounting {
    /// Take the measurement. Call after `cap::boot::init_from_regions`.
    ///
    /// Public so the acceptance test can take it again and compare
    /// against what BootInfo claims. Two readings of the same
    /// mechanism, which is weaker than two mechanisms — but what it
    /// catches is the kernel reporting one thing and doing another,
    /// which is a different failure from miscounting.
    pub fn measure() -> Accounting {
        // Every frame the allocator ever had. `initial_free_count` is
        // raised by `release`, so this includes the bootloader's
        // donation.
        let usable = crate::memory::frame_allocator::initial_free_count();

        // What the root Untypeds cover. From the capabilities, which
        // is what will actually be handed over.
        let untyped = boot::root_untyped_bytes() / FRAME_SIZE;

        // What the kernel took, counted at the call sites. The root
        // Untypeds were claimed through `allocate_specific`, so they
        // are in this figure too and have to come back out.
        let allocated = crate::memory::frame_allocator::allocation_count()
            - crate::memory::frame_allocator::free_count_calls();
        let kernel = allocated.saturating_sub(untyped);

        // Whatever is still free at this point can never be reached
        // again: `retire()` has closed the only door.
        let lost = crate::memory::frame_allocator::free_count();

        Accounting {
            usable,
            untyped,
            kernel,
            lost,
        }
    }

    /// Whether the four figures add up.
    ///
    /// Checked in the kernel as well as in the root task, and the
    /// duplication is deliberate: the kernel's check catches a
    /// miscount before a component is told about it, and the root
    /// task's catches the kernel *lying* about the result. They are
    /// different claims.
    fn balances(&self) -> bool {
        self.untyped + self.kernel + self.lost == self.usable
    }

    fn report(&self) {
        println!(
            "[root] accounting: {} frames usable, {} in untypeds, {} \
             kept by the kernel, {} unreachable",
            self.usable, self.untyped, self.kernel, self.lost
        );
        if self.lost > 0 {
            println!(
                "[root] {} frames ({} KiB) are in no Untyped and were not \
                 kept by the kernel. They are memory `cap::boot` could not \
                 fit into a 2^k-aligned block (ADR 020), and `retire()` has \
                 removed the only other way to reach them. Reported rather \
                 than folded into the kernel's share, because a sum that \
                 balances by absorbing its own remainder proves nothing.",
                self.lost,
                self.lost * FRAME_SIZE / 1024
            );
        }
    }
}

// ---- The seal ----------------------------------------------------------

/// Whether [`seal`] has run.
static SEALED: AtomicBool = AtomicBool::new(false);

/// How many privileged operations the kernel has performed on its own
/// initiative.
///
/// Incremented by [`note_privileged_op`] from every path that creates
/// an object, maps a page, or starts a thread **outside a syscall**.
/// ADR 044's claim is that this stops moving at the seal, and a
/// counter is how a claim about "no code remains" becomes falsifiable:
/// the alternative is grepping, which tests the reader rather than the
/// kernel.
static PRIVILEGED_OPS: AtomicU64 = AtomicU64::new(0);

/// The value [`PRIVILEGED_OPS`] held when the seal closed.
static OPS_AT_SEAL: AtomicU64 = AtomicU64::new(0);

/// Record a privileged operation the kernel performed for itself.
///
/// # Panics
///
/// After the seal. That is the enforcement rather than the
/// measurement: a kernel that built something after boot would stop
/// here, with the operation named, instead of being noticed by a
/// counter nobody read.
pub fn note_privileged_op(what: &str) {
    assert!(
        !SEALED.load(Ordering::Acquire),
        "the kernel performed a privileged operation ({}) after the \
         seal. From the end of boot the kernel answers syscalls and \
         delivers interrupts; it does not create objects, map pages or \
         start threads on its own initiative (ADR 044).",
        what
    );
    PRIVILEGED_OPS.fetch_add(1, Ordering::AcqRel);
}

/// Close the kernel's boot-time powers.
///
/// # Safety
///
/// Call once, after the root task's thread is runnable.
unsafe fn seal() {
    let count = PRIVILEGED_OPS.load(Ordering::Acquire);
    OPS_AT_SEAL.store(count, Ordering::Release);
    SEALED.store(true, Ordering::Release);

    println!(
        "[root] sealed after {} privileged operations; the kernel now \
         answers syscalls and delivers interrupts and does nothing else \
         (ADR 044)",
        count
    );
}

/// Whether the seal has closed.
pub fn is_sealed() -> bool {
    SEALED.load(Ordering::Acquire)
}

/// Privileged operations performed so far.
pub fn privileged_ops() -> u64 {
    PRIVILEGED_OPS.load(Ordering::Acquire)
}

/// Privileged operations at the moment the seal closed.
pub fn ops_at_seal() -> u64 {
    OPS_AT_SEAL.load(Ordering::Acquire)
}

// ---- Building the root task --------------------------------------------

fn object_cnode() -> CNode {
    // SAFETY: boot::init put a CNode in this root slot.
    unsafe { boot::leaf_cnode(boot::ROOT_SLOT_OBJECTS) }
}

/// The root task's TCB, by object address.
///
/// Zero until [`start`] runs. Read by [`check_alive`] on the
/// scheduler's path, which is the only place a dead thread becomes
/// visible to the kernel.
static ROOT_TCB_OBJECT: AtomicU64 = AtomicU64::new(0);

/// Panic if the root task's TCB has stopped existing (ADR 044).
///
/// **The kernel must notice this itself.** The first version of
/// `root-dies` deleted the TCB and then waited for a panic that
/// never came: the scheduler simply stopped scheduling the thread
/// and carried on with the boot thread, and the scenario's own
/// `unreachable!` fired instead. That is a test noticing, not a
/// kernel noticing, and it would have been worthless on a system
/// where nobody was watching.
///
/// What makes "the root task died" detectable at all is that the
/// kernel keeps one capability for its TCB, and `delete` clears the
/// slot and advances its generation (ADR 018). So the check is the
/// same address-and-generation idiom used for IRQ destinations
/// (ADR 042) and stale replies (ADR 036): the slot must still hold
/// a TCB, and it must still be *this* one.
///
/// Called from the timer tick, which is the cheapest place that is
/// guaranteed to keep running — a dead root task does not stop the
/// clock.
pub fn check_alive() {
    let expected = ROOT_TCB_OBJECT.load(Ordering::Acquire);
    if expected == 0 {
        // Not started yet; nothing to be dead.
        return;
    }

    // SAFETY: `cap::boot::init_from_regions` seeded this root slot
    // with the object CNode, and `start` has run.
    let cnode = unsafe { boot::leaf_cnode(boot::ROOT_SLOT_OBJECTS) };
    // SAFETY: Index in range, checked at build time.
    let slot = unsafe { cnode.slot(KSLOT_TCB) };

    let alive = slot.cap_type() == CapType::Tcb && slot.object().0 .0 == expected;
    if alive {
        return;
    }

    panic!(
        "the root task's TCB is gone. Every capability in the system \
         descends from one it held, so nothing can now name an Untyped, \
         an interrupt line or a frame of memory -- not because the \
         kernel refuses, but because a CPtr is meaningful only relative \
         to a root (ADR 018) and the only root that reached anything \
         has stopped existing.\n\
         \n\
         ADR 044 requires this to be a panic rather than an idle loop. \
         A kernel that \"continued\" here would continue doing nothing, \
         forever, with no way to be given work and no way to be told. \
         Rebooting was considered and rejected: it is the same \
         situation wearing a recovery story, and it hides the failure \
         from the person who needs to see it."
    );
}

/// Build the root task and start it.
///
/// Returns once the thread is runnable and the seal has closed.
///
/// # Safety
///
/// Call once, after `cap::boot::init_from_regions`, `sched::init`,
/// `apic::init` and `vspace::record_kernel_entries`, with the
/// scheduler running.
pub unsafe fn start() {
    println!(
        "[root] building the first user component: {} bytes of program, \
         embedded in .rodata (ADR 044)",
        ROOT_TASK_IMAGE.len()
    );

    assert!(
        ROOT_TASK_IMAGE.len() <= ROOT_CODE_PAGES * FRAME_SIZE as usize,
        "the root task image is {} bytes but only {} pages are mapped; \
         the tail would be missing and the program would run off the end \
         of its own code",
        ROOT_TASK_IMAGE.len(),
        ROOT_CODE_PAGES
    );

    let accounting = Accounting::measure();
    accounting.report();
    assert!(
        accounting.balances(),
        "the kernel's own accounting does not add up: {} usable, {} in \
         untypeds, {} kept, {} unreachable. The root task is about to be \
         told these figures, and telling it a sum that does not balance \
         would make its check meaningless.",
        accounting.usable,
        accounting.untyped,
        accounting.kernel,
        accounting.lost
    );

    let cnode = object_cnode();

    // An Untyped to build the root task out of. Its *own* objects
    // come from here; everything else is handed over whole.
    let parent_index = untyped_with_room(ROOT_TASK_BYTES);
    // SAFETY: Index below the count.
    let parent = unsafe { boot::root_untyped(parent_index) };

    // ---- 1. Its CSpace -----------------------------------------------
    note_privileged_op("retype the root task's CSpace");
    // SAFETY: Live Untyped and CNode; the destination slots are empty.
    unsafe {
        untyped::retype(
            parent,
            CapType::CNode,
            ROOT_TASK_ROOT_RADIX,
            1,
            &cnode,
            KSLOT_ROOT_CNODE,
        )
    }
    .expect("could not retype the root task's root CNode");
    // SAFETY: As above.
    unsafe {
        untyped::retype(
            parent,
            CapType::CNode,
            ROOT_TASK_LEAF_RADIX,
            1,
            &cnode,
            KSLOT_LEAF_CNODE,
        )
    }
    .expect("could not retype the root task's leaf CNode");

    // SAFETY: Indices in range.
    let (root_cnode_slot, leaf_cnode_slot) =
        unsafe { (cnode.slot(KSLOT_ROOT_CNODE), cnode.slot(KSLOT_LEAF_CNODE)) };
    // SAFETY: The capability says how many slots the object has.
    let root_cnode = unsafe { CNode::new(root_cnode_slot.object().0, root_cnode_slot.as_radix()) };
    // SAFETY: As above.
    let leaf = unsafe { CNode::new(leaf_cnode_slot.object().0, leaf_cnode_slot.as_radix()) };

    // The leaf goes into the root. `copy`, so the derivation list
    // stays intact and revoking the Untyped reaches both.
    //
    // SAFETY: Both slots live and distinct.
    unsafe {
        cnode::copy(
            leaf_cnode_slot,
            root_cnode.slot(LEAF_INDEX),
            leaf_cnode_slot.rights(),
        )
        .expect("could not place the leaf in the root task's root CNode");
    }

    // And the root task needs to be able to name its own CNode: every
    // invocation that creates something has to say where to put it.
    //
    // SAFETY: As above.
    unsafe {
        cnode::copy(
            leaf_cnode_slot,
            leaf.slot(SLOT_OWN_CNODE),
            leaf_cnode_slot.rights(),
        )
        .expect("could not give the root task a capability for its own CNode");
    }

    // ---- 2. Every Untyped, and the IRQControl ------------------------
    //
    // **All of them.** The kernel keeps no Untyped of its own: what
    // it needed, it has already taken, and ADR 022 retired the
    // allocator. A root task that held only some of the memory would
    // be a root task the kernel could still outvote.
    let untyped_count = hand_over_untypeds(&leaf);
    hand_over_irq_control(&leaf);
    hand_over_ioport_control(&leaf);

    // ---- 3. Its address space ----------------------------------------
    let (code_frames, bootinfo_frame) = build_address_space(parent, &cnode, &leaf);

    // ---- 4. BootInfo --------------------------------------------------
    write_boot_info(bootinfo_frame, untyped_count, &accounting);

    // ---- 5. Its thread ------------------------------------------------
    let tcb_slot = build_thread(parent, &cnode, root_cnode_slot);

    // The program goes in last, through PMAP: the code pages are
    // mapped ReadExecute (ADR 032), so they cannot be written through
    // the mapping the root task will use.
    load_image(code_frames);

    println!(
        "[root] root task ready: {} untypeds, IRQControl, {} KiB CSpace, \
         entry {:#018x}, stack {:#018x}, bootinfo {:#018x}",
        untyped_count,
        ((32usize << ROOT_TASK_LEAF_RADIX) / 1024),
        abi::ROOT_CODE_ADDRESS,
        abi::ROOT_STACK_TOP,
        abi::BOOTINFO_ADDRESS
    );

    // Remember which TCB this is, so the kernel can notice if it
    // stops existing. Recorded **before** the thread is enqueued:
    // afterwards it could in principle be scheduled, run, and die
    // before this line, and the kernel would then be watching for a
    // thread it never knew about.
    //
    // SAFETY: Index in range.
    let tcb_object = unsafe { cnode.slot(tcb_slot) }.object().0 .0;
    ROOT_TCB_OBJECT.store(tcb_object, Ordering::Release);

    // SAFETY: Index in range; the slot holds a live TCB.
    unsafe {
        let slot = cnode.slot(tcb_slot);
        let thread = crate::cap::tcb::from_slot(slot).expect("the slot does not hold a TCB");
        crate::sched::enqueue(thread);
    }

    // ---- 6. The seal ---------------------------------------------------
    //
    // SAFETY: The root task's thread is runnable and nothing else is
    // left to build.
    unsafe {
        seal();
    }
}

/// Find a root Untyped with at least `bytes` free.
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
        "no root Untyped has {} bytes free; the kernel cannot build the \
         root task, and without it there is no user space at all",
        bytes
    );
}

/// Copy every root Untyped into the root task's leaf.
///
/// Returns how many were handed over.
///
/// The capabilities are **copied**, not moved: the kernel's own
/// copies stay in the init CSpace, which is what the acceptance tests
/// resolve through. That is a deliberate asymmetry and it is worth
/// being honest about — a production system would hand them over and
/// drop its own. Here the kernel's copies exist so that `root-test`
/// can measure what the root task was given; they are unreachable
/// from ring 3, because a CPtr is meaningful only relative to a root
/// (ADR 018) and no component holds the init CSpace's root.
fn hand_over_untypeds(leaf: &CNode) -> u32 {
    // Negative (a): hand over nothing.
    //
    // The *same* loop, bounded differently, rather than a second
    // code path. A scenario that took a different route through the
    // kernel would be testing the route rather than the kernel —
    // and the question here is what the handover code does when
    // there is nothing to hand over.
    let count = if crate::roottest::starving() {
        println!(
            "[root] root-no-untyped: handing over 0 of {} untypeds, so \
             the root task starts with no memory at all",
            boot::root_untyped_count()
        );
        0
    } else {
        boot::root_untyped_count()
    };
    assert!(
        count <= abi::BOOTINFO_MAX_UNTYPED,
        "boot created {} root Untypeds but BootInfo describes at most {}. \
         Truncating the list would hand the root task memory it cannot \
         name while telling it the accounting is complete (ADR 044).",
        count,
        abi::BOOTINFO_MAX_UNTYPED
    );
    assert!(
        SLOT_UNTYPED_FIRST + count <= leaf.len(),
        "the root task's leaf has {} slots and cannot hold {} Untypeds \
         from slot {}",
        leaf.len(),
        count,
        SLOT_UNTYPED_FIRST
    );

    for index in 0..count {
        note_privileged_op("hand over a root Untyped");
        // SAFETY: Index below the count.
        let source = unsafe { boot::root_untyped(index) };
        // SAFETY: Source and destination are live and distinct — the
        // destination is an empty slot in a freshly retyped CNode.
        unsafe {
            let dest = leaf.slot(SLOT_UNTYPED_FIRST + index);
            cnode::copy(source, dest, source.rights())
                .expect("could not hand a root Untyped to the root task");
        }
    }

    count as u32
}

/// Copy the IRQControl capability into the root task's leaf.
///
/// One capability, and after this the root task is the only thing
/// that can hand out interrupt lines — which is what makes a driver
/// a component rather than part of the kernel (ADR 041).
fn hand_over_irq_control(leaf: &CNode) {
    note_privileged_op("hand over IRQControl");
    let objects = object_cnode();
    // SAFETY: boot seeded this slot with the one IRQControl.
    let source = unsafe { objects.slot(boot::IRQ_CONTROL_SLOT) };
    assert_eq!(
        source.cap_type(),
        CapType::IrqControl,
        "object slot {} does not hold the IRQControl capability",
        boot::IRQ_CONTROL_SLOT
    );
    // SAFETY: Both slots live and distinct.
    unsafe {
        let dest = leaf.slot(SLOT_IRQ_CONTROL);
        cnode::copy(source, dest, source.rights())
            .expect("could not hand IRQControl to the root task");
    }
}

/// Copy the IOPortControl capability into the root task's leaf.
///
/// **The capability that makes M13 possible**, and the pair to
/// `hand_over_irq_control` above: a serial driver needs both a line
/// and a port range, and after this the kernel holds neither
/// exclusively — it keeps a copy it uses for nothing but the
/// emergency console's two registers (ADR 046).
///
/// `copy` rather than a move, because the capability is seeded as a
/// root in the kernel's own CSpace and that root is what the
/// derivation list hangs from. Handing the original over would leave
/// the kernel's slot empty and the root task's capability parentless.
fn hand_over_ioport_control(leaf: &CNode) {
    note_privileged_op("hand over IOPortControl");
    let objects = object_cnode();
    // SAFETY: boot seeded this slot with the one IOPortControl.
    let source = unsafe { objects.slot(boot::IOPORT_CONTROL_SLOT) };
    assert_eq!(
        source.cap_type(),
        CapType::IoPortControl,
        "object slot {} does not hold the IOPortControl capability",
        boot::IOPORT_CONTROL_SLOT
    );
    // SAFETY: Both slots live and distinct.
    unsafe {
        let dest = leaf.slot(SLOT_IOPORT_CONTROL);
        cnode::copy(source, dest, source.rights())
            .expect("could not hand IOPortControl to the root task");
    }
}

/// Build the root task's address space.
///
/// Returns the code frames' physical addresses and the BootInfo
/// frame's, both of which are written through PMAP afterwards.
fn build_address_space(
    parent: &mut crate::cap::cnode::Slot,
    cnode: &CNode,
    leaf: &CNode,
) -> ([ObjectRef; ROOT_CODE_PAGES], ObjectRef) {
    use crate::cap::vspace::{self, MapPermissions};

    note_privileged_op("build the root task's address space");

    // SAFETY: Live Untyped and CNode; the destination slots are empty.
    unsafe { untyped::retype(parent, CapType::PageTable, 0, 1, cnode, KSLOT_VSPACE) }
        .expect("could not retype the root task's VSpace root");
    // SAFETY: As above.
    unsafe {
        untyped::retype(
            parent,
            CapType::PageTable,
            0,
            TABLE_COUNT,
            cnode,
            KSLOT_TABLES,
        )
    }
    .expect("could not retype the root task's page tables");
    // SAFETY: As above.
    unsafe {
        untyped::retype(
            parent,
            CapType::Frame,
            0,
            ROOT_CODE_PAGES,
            cnode,
            KSLOT_CODE,
        )
    }
    .expect("could not retype the root task's code frames");
    // SAFETY: As above.
    unsafe {
        untyped::retype(
            parent,
            CapType::Frame,
            0,
            ROOT_STACK_PAGES,
            cnode,
            KSLOT_STACK,
        )
    }
    .expect("could not retype the root task's stack frames");
    // SAFETY: As above.
    unsafe { untyped::retype(parent, CapType::Frame, 0, 1, cnode, KSLOT_BOOTINFO) }
        .expect("could not retype the root task's BootInfo frame");

    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(KSLOT_VSPACE) };
    // SAFETY: Freshly retyped PageTable that nothing has mapped.
    unsafe { vspace::create_vspace(root) }.expect("could not create the root task's VSpace");

    // ---- The intermediate tables -------------------------------------
    //
    // **Four, not three**, and the first version of this got it
    // wrong in a way worth keeping.
    //
    // Code is at 4 MiB, the stack at 5 and BootInfo at 6. Sharing is
    // per level, and only the bottom level is narrow enough to
    // matter:
    //
    //   level 3 (PDPT)  covers 512 GiB  -- one, shared by all three
    //   level 2 (PD)    covers   1 GiB  -- one, shared by all three
    //   level 1 (PT)    covers   2 MiB  -- **two**: 4..6 and 6..8 MiB
    //
    // So the code and the stack share a PT and BootInfo does not.
    //
    // The first version offered the same three capabilities twice,
    // once per address, and accepted `AlreadyMapped` throughout on
    // the reasoning that redundant offers would be harmlessly
    // refused. Two of the three are redundant; the third is not.
    // 6 MiB needs a PT that does not exist, and the capability being
    // offered for it was already mapped at 4 MiB -- so the offer was
    // refused as `AlreadyMapped` and the table was never created.
    //
    // The failure then surfaced one step later, as
    // `AlreadyMapped { address: 6291456 }` from `map_frame`, naming
    // the BootInfo mapping rather than the missing table underneath
    // it. Blanket-accepting an error turned a precise failure into a
    // misleading one.
    //
    // This is M11's `SCRATCH_ADDRESS` lesson arriving from the other
    // side. There, "it shares its PDPT and PD" was true, reassuring,
    // and silent about the PT. Here the same half-truth was written
    // into a loop.
    //
    // Each capability is now used exactly once and **no error is
    // tolerated**: with the sharing stated explicitly there is no
    // legitimate `AlreadyMapped` left, so accepting one would hide
    // the next mistake of this kind.
    let shared_pdpt = KSLOT_TABLES;
    let shared_pd = KSLOT_TABLES + 1;
    let pt_low = KSLOT_TABLES + 2; // 4..6 MiB: code and stack
    let pt_high = KSLOT_TABLES + 3; // 6..8 MiB: BootInfo

    let code = VirtAddr::new(abi::ROOT_CODE_ADDRESS);
    let info = VirtAddr::new(abi::BOOTINFO_ADDRESS);

    for (slot_index, address, level) in [
        (shared_pdpt, code, 3u8),
        (shared_pd, code, 2),
        (pt_low, code, 1),
        (pt_high, info, 1),
    ] {
        // SAFETY: Index in range.
        let table = unsafe { cnode.slot(slot_index) };
        // SAFETY: Both slots hold live capabilities.
        unsafe { vspace::map_table(root, table, address, level) }.unwrap_or_else(|error| {
            panic!(
                "could not map the level-{} table for {:#018x}: {}. Each \
                 table capability is used exactly once here, so \
                 AlreadyMapped would mean two addresses were expected to \
                 share a level they do not.",
                level, address.0, error
            )
        });
    }

    // The code pages, read-execute. **Not writable**, which is why
    // the image is written through PMAP below rather than here.
    let mut code_frames = [ObjectRef::NULL; ROOT_CODE_PAGES];
    for (index, entry) in code_frames.iter_mut().enumerate() {
        // SAFETY: Index in range.
        let frame = unsafe { cnode.slot(KSLOT_CODE + index) };
        *entry = frame.object();
        let address = abi::ROOT_CODE_ADDRESS + (index as u64) * FRAME_SIZE;
        // SAFETY: Both slots hold live capabilities and the tables
        // are in place.
        unsafe {
            vspace::map_frame(
                root,
                frame,
                VirtAddr::new(address),
                MapPermissions::ReadExecute,
            )
        }
        .unwrap_or_else(|e| {
            panic!(
                "could not map the root task's code at {:#018x}: {}",
                address, e
            )
        });
    }

    // The stack, read-write. Four pages from M14, growing **down**
    // from `ROOT_STACK_TOP` towards `ROOT_STACK_ADDRESS` -- which is
    // why that constant names the lowest page rather than the first
    // one the root task touches.
    for page in 0..ROOT_STACK_PAGES {
        let address = abi::ROOT_STACK_ADDRESS + (page as u64) * FRAME_SIZE;
        // SAFETY: Index in range.
        let stack = unsafe { cnode.slot(KSLOT_STACK + page) };
        // SAFETY: As above.
        unsafe {
            vspace::map_frame(
                root,
                stack,
                VirtAddr::new(address),
                MapPermissions::ReadWrite,
            )
        }
        .unwrap_or_else(|e| {
            panic!(
                "could not map the root task's stack at {:#018x}: {}",
                address, e
            )
        });
    }

    // BootInfo, **read-only**.
    //
    // Not caution: BootInfo is what the kernel says about the root
    // task, and a root task that could write it could read back its
    // own words as though they were the kernel's (ADR 044). The
    // kernel writes it through PMAP, where the capability's rights
    // do not apply because the kernel is not going through the
    // mapping.
    // SAFETY: Index in range.
    let bootinfo = unsafe { cnode.slot(KSLOT_BOOTINFO) };
    let bootinfo_object = bootinfo.object();
    // SAFETY: As above.
    unsafe {
        vspace::map_frame(
            root,
            bootinfo,
            VirtAddr::new(abi::BOOTINFO_ADDRESS),
            MapPermissions::ReadOnly,
        )
    }
    .expect("could not map the root task's BootInfo page");

    // The root task needs to name its own VSpace: it maps its
    // children's code through its own address space first (the W^X
    // dance), so it has to be able to say "my space".
    //
    // SAFETY: Both slots live and distinct.
    unsafe {
        cnode::copy(root, leaf.slot(SLOT_OWN_VSPACE), root.rights())
            .expect("could not give the root task a capability for its own VSpace");
    }

    (code_frames, bootinfo_object)
}

/// Create the root task's thread.
///
/// Returns the object-CNode index of its TCB.
fn build_thread(
    parent: &mut crate::cap::cnode::Slot,
    cnode: &CNode,
    root_cnode_slot: &mut crate::cap::cnode::Slot,
) -> usize {
    note_privileged_op("create the root task's thread");

    // SAFETY: Live Untyped and CNode; the destination slot is empty.
    unsafe { untyped::retype(parent, CapType::Tcb, 0, 1, cnode, KSLOT_TCB) }
        .expect("could not retype the root task's TCB");

    let frames_needed = (layout::KSTACK_SIZE / FRAME_SIZE) as usize;
    // SAFETY: As above.
    unsafe {
        untyped::retype(
            parent,
            CapType::Frame,
            0,
            frames_needed,
            cnode,
            KSLOT_KSTACK,
        )
    }
    .expect("could not retype the root task's kernel stack frames");

    // The kernel stack goes in the kernel half, which no capability
    // names (ADR 027). The root task pays for the frames; the kernel
    // chooses where they go.
    let (_, bottom, top) = layout::kstack_slot(ROOT_KSTACK_SLOT);
    for index in 0..frames_needed {
        // SAFETY: Index in range.
        let frame_slot = unsafe { cnode.slot(KSLOT_KSTACK + index) };
        let frame = PhysFrame::containing(frame_slot.object().0);
        let page = VirtAddr::new(bottom.0 + (index as u64) * FRAME_SIZE);

        // SAFETY: The kernel's tables are live, the address is inside
        // this thread's own KSTACK slot, and the frame came from a
        // retype so nothing else owns it.
        unsafe {
            let table = crate::memory::vm::kernel_table();
            let mut source = crate::memory::heap::reserve_frames();
            let flags = crate::memory::paging::PageFlags::READ
                | crate::memory::paging::PageFlags::WRITE
                | crate::memory::paging::PageFlags::GLOBAL;
            let tlb = crate::memory::paging::map(table, page, frame, flags, &mut source)
                .expect("could not map a root task kernel stack page");
            tlb.flush();
        }
    }

    // SAFETY: Index in range.
    let tcb_slot = unsafe { cnode.slot(KSLOT_TCB) };
    // SAFETY: The slot holds a live TCB nothing else borrows.
    let thread =
        unsafe { crate::cap::tcb::from_slot(tcb_slot) }.expect("the slot does not hold a TCB");

    // SAFETY: The kernel stack is mapped and owned by nothing else;
    // the entry point and stack are mapped in the VSpace bound below.
    unsafe {
        thread.configure_user(
            abi::ROOT_CODE_ADDRESS,
            abi::ROOT_STACK_TOP,
            0,
            top,
            bottom,
            ROOT_KSTACK_SLOT,
        );
    }

    // SAFETY: Index in range.
    let vspace_root = unsafe { cnode.slot(KSLOT_VSPACE) };
    // SAFETY: All three slots hold live capabilities and the thread
    // is not running.
    unsafe { crate::cap::tcb::set_space(tcb_slot, Some(vspace_root), Some(root_cnode_slot)) }
        .expect("could not give the root task its VSpace and CSpace");

    // And a capability for its own TCB, so it can talk about itself.
    //
    // SAFETY: Both slots live and distinct.
    unsafe {
        let leaf_slot = cnode.slot(KSLOT_LEAF_CNODE);
        let leaf = CNode::new(leaf_slot.object().0, leaf_slot.as_radix());
        cnode::copy(tcb_slot, leaf.slot(SLOT_OWN_TCB), tcb_slot.rights())
            .expect("could not give the root task a capability for its own TCB");
    }

    KSLOT_TCB
}

/// Write BootInfo into its frame, through the physical window.
///
/// Through PMAP because the page is mapped read-only in the root
/// task's address space, which is the point of mapping it that way.
fn write_boot_info(frame: ObjectRef, untyped_count: u32, accounting: &Accounting) {
    note_privileged_op("write BootInfo");

    // The screen, if there is one (M14, ADR 049).
    //
    // Two sources that must agree: the geometry comes from the copy
    // `bootinfo::capture` made before reclaim, and the slot from
    // `cap::boot`'s seeding pass. Either can be absent -- a machine
    // with no framebuffer has neither -- and a machine that had
    // geometry but no slot would mean the seeding loop ran out of
    // CNode space, which is worth refusing rather than reporting as
    // "no screen".
    let fb = crate::memory::bootinfo::get().framebuffer();
    let fb_untyped = crate::cap::boot::framebuffer_untyped();

    assert_eq!(
        fb.is_some(),
        fb_untyped.is_some(),
        "the bootloader {} a framebuffer but the capability system {} \
         device Untypeds for it. The two come from different passes \
         (ADR 049's copy and ADR 050's seeding) and disagreeing means \
         one of them silently gave up -- most likely the Untyped CNode \
         filled before the device pass ran.",
        if fb.is_some() {
            "reported"
        } else {
            "did not report"
        },
        if fb_untyped.is_some() {
            "has"
        } else {
            "has no"
        }
    );

    let header = abi::BootInfo {
        magic: abi::BOOTINFO_MAGIC,
        version: abi::BOOTINFO_VERSION,
        header_size: core::mem::size_of::<abi::BootInfo>() as u32,
        untyped_count,
        untyped_first_slot: SLOT_UNTYPED_FIRST as u32,
        irq_control_slot: SLOT_IRQ_CONTROL as u32,
        ioport_control_slot: SLOT_IOPORT_CONTROL as u32,
        // Which scenario this boot is for (M13). Zero is M12's
        // behaviour, so this field changes nothing unless a test
        // asks it to.
        scenario: crate::roottest::scenario(),
        root_cnode_slot: SLOT_OWN_CNODE as u32,
        tcb_slot: SLOT_OWN_TCB as u32,
        vspace_slot: SLOT_OWN_VSPACE as u32,
        free_slot_first: (SLOT_UNTYPED_FIRST + untyped_count as usize) as u32,
        free_slot_end: (1u32 << ROOT_TASK_LEAF_RADIX),
        usable_bytes: accounting.usable * FRAME_SIZE,
        kernel_bytes: accounting.kernel * FRAME_SIZE,
        lost_bytes: accounting.lost * FRAME_SIZE,

        // Zero means "no screen" (ADR 049). The slot is an index into
        // the same leaf CNode the ordinary Untypeds live in, offset
        // the same way, because a device Untyped *is* an Untyped --
        // the flag is on the capability, not in a separate list.
        framebuffer_untyped_slot: match fb_untyped {
            Some((first, _)) => (SLOT_UNTYPED_FIRST + first) as u32,
            None => 0,
        },
        framebuffer_width: fb.map_or(0, |f| f.width as u32),
        framebuffer_height: fb.map_or(0, |f| f.height as u32),
        framebuffer_pitch: fb.map_or(0, |f| f.pitch as u32),
        framebuffer_bpp: fb.map_or(0, |f| f.bpp as u32),
        framebuffer_address: fb.map_or(0, |f| f.address),
    };

    // SAFETY: The frame came from a retype so nothing else owns it,
    // and PMAP maps all RAM writably. The page was zeroed by retype,
    // so the descriptor array starts zeroed too.
    unsafe {
        let base = crate::memory::phys_to_virt(frame.0).as_ptr::<u8>();
        core::ptr::write_bytes(base, 0, FRAME_SIZE as usize);
        core::ptr::write(base as *mut abi::BootInfo, header);

        // The descriptors, at the ABI's fixed offset.
        let descs = base.add(abi::BOOTINFO_UNTYPED_OFFSET) as *mut abi::UntypedDesc;
        for index in 0..untyped_count as usize {
            // SAFETY: Index below the count.
            let slot = boot::root_untyped(index);
            // SAFETY: Root slots hold live Untypeds.
            let untyped =
                untyped::Untyped::from_slot(slot).expect("a root slot does not hold an Untyped");
            core::ptr::write(
                descs.add(index),
                abi::UntypedDesc {
                    base: slot.object().0 .0,
                    size_bits: slot.as_size_bits(),
                    is_device: u8::from(untyped.is_device()),
                    reserved: [0; 6],
                },
            );
        }
    }

    println!(
        "[root] bootinfo at {:#018x}: {} untypeds from slot {}, free slots \
         {}..{}, {} MiB usable, {} MiB kept by the kernel, {} KiB \
         unreachable",
        abi::BOOTINFO_ADDRESS,
        untyped_count,
        SLOT_UNTYPED_FIRST,
        SLOT_UNTYPED_FIRST + untyped_count as usize,
        1u32 << ROOT_TASK_LEAF_RADIX,
        accounting.usable * FRAME_SIZE / (1024 * 1024),
        accounting.kernel * FRAME_SIZE / (1024 * 1024),
        accounting.lost * FRAME_SIZE / 1024
    );
}

/// Copy the root task's program into its code frames.
///
/// Through PMAP, for M8's two reasons: the pages are `ReadExecute`
/// and could not be written through the mapping, and writing through
/// PMAP proves the mapping reaches *the frames the capabilities
/// name*.
fn load_image(frames: [ObjectRef; ROOT_CODE_PAGES]) {
    note_privileged_op("load the root task's program");

    let mut written = 0usize;
    for frame in frames {
        if written >= ROOT_TASK_IMAGE.len() {
            break;
        }
        let remaining = ROOT_TASK_IMAGE.len() - written;
        let count = remaining.min(FRAME_SIZE as usize);

        // SAFETY: The frame came from a retype so nothing else owns
        // it, PMAP maps all RAM writably, and `count` is bounded by
        // the frame size.
        unsafe {
            let ptr = crate::memory::phys_to_virt(frame.0).as_ptr::<u8>();
            core::ptr::copy_nonoverlapping(ROOT_TASK_IMAGE.as_ptr().add(written), ptr, count);
        }
        written += count;
    }

    assert_eq!(
        written,
        ROOT_TASK_IMAGE.len(),
        "only {} of the root task's {} bytes were loaded",
        written,
        ROOT_TASK_IMAGE.len()
    );
}
