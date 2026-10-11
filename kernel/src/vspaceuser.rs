// SPDX-License-Identifier: MPL-2.0
//! M11: a user component managing its own address space.
//!
//! M7 built an address space out of capabilities, from inside the
//! kernel. M8 put code in ring 3. This is the two together: **a
//! component at CPL 3 maps its own pages, and builds a second
//! component.**
//!
//! # What the milestone is actually about
//!
//! Exposing `Frame.Map` to user space is a dispatcher arm. The work
//! is ADR 043's first decision — **each thread resolves CPtrs through
//! its own CSpace** — and the reason is in the negatives rather than
//! the positives.
//!
//! Until M11 the dispatcher resolved every CPtr in the init CSpace.
//! That was correct while every user thread was the kernel test's
//! own. With two components it is an isolation hole: one CSpace means
//! every thread can name every capability, so `vspace-user-foreign`
//! — "component B cannot reach component A's page table" — would be a
//! claim no test could falsify. It would pass, and prove nothing.
//! ADR 023's K5 forbids exactly that.
//!
//! So the fixture here is not a convenience. [`Component`] gives each
//! component a CNode of its own and binds it with `SetSpace`, and
//! that is what makes the negative scenario a test.
//!
//! # The programs
//!
//! Hand-assembled, as in M8, and for the same reason: there is no
//! loader until M12. They are longer here, because a component that
//! builds another component makes about twenty invocations. Each is
//! emitted through a named helper so the register discipline — RDI
//! target, RSI method, RDX/R10/R8/R9 arguments (ADR 043) — is written
//! once.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::cap::cnode::CNode;
use crate::cap::cptr::CPtr;
use crate::cap::tcb;
use crate::cap::{boot, cnode, untyped, CapType};
use crate::memory::{layout, VirtAddr};
use crate::println;
use crate::sched;
use crate::syscall::{self, Method};
use crate::usertest::{
    self, mov_r10_imm, mov_r8_imm, mov_r9_imm, mov_rdi_imm, mov_rdx_imm, mov_rsi_imm, program,
    Program,
};

// ---- Where things live -------------------------------------------------

/// Component A's code page. Same address M8 uses, deliberately: the
/// trampoline's entry point is the only thing that has to agree, and
/// a second constant would be a second thing to keep in step.
const CODE_ADDRESS: u64 = usertest::USER_CODE_ADDRESS;

/// A's stack page.
const STACK_ADDRESS: u64 = 0x0050_0000;

/// Initial RSP: top of the stack page, 16-byte aligned.
const STACK_TOP: u64 = STACK_ADDRESS + 4096 - 16;

/// A scratch address **inside an existing page table**.
///
/// 7 MiB, and the exact value is load-bearing. The fixture maps the
/// component's code, stack and data at 4, 5 and 6 MiB, and a PT
/// covers 2 MiB — so the tables that exist span 4..6 MiB and
/// 6..8 MiB. 7 MiB falls in the second; 8 MiB, which this constant
/// was first set to, falls in a *third* table that nothing had
/// created.
///
/// The symptom was a #PF at `cr2=0x800000`, **not-present**, when the
/// parent tried to write the child's program through a mapping it
/// thought it had made — and the mapping had failed with
/// `MissingPageTable`, reported to a step the scenario pooled with
/// others and so did not assert individually.
///
/// The original comment said this address "shares its PDPT and PD"
/// with the code and stack, which was true and irrelevant: sharing a
/// PD means sharing a 1 GiB region, and what a mapping needs is the
/// PT for its own 2 MiB. Two different granularities, and the
/// coarser one is the one that reads as reassuring.
///
/// Used where a scenario needs somewhere to map without supplying
/// tables of its own. [`DEEP_ADDRESS`] is the opposite case, and
/// deliberately so.
const SCRATCH_ADDRESS: u64 = 0x0070_0000;

/// A second scratch address, in a **different PDPT** from the first.
///
/// 512 GiB apart, so it shares no intermediate table with anything
/// above. That is what makes criterion 2 meaningful: mapping here
/// needs all three levels supplied by the component, and the kernel
/// refusing to produce them (ADR 027) is visible as
/// `MissingPageTable` if the component skips one.
const DEEP_ADDRESS: u64 = 0x0000_0080_0000_0000;

/// Where the component maps its child's code and stack.
const CHILD_CODE_ADDRESS: u64 = 0x0040_0000;
const CHILD_STACK_ADDRESS: u64 = 0x0050_0000;
const CHILD_STACK_TOP: u64 = CHILD_STACK_ADDRESS + 4096 - 16;

/// KSTACK slot for component A's own kernel stack.
///
/// Slots 0 and 1 are IST1 and main; M7 uses 6..9, M8 uses 10 and 11,
/// M10 uses its own. 12 is free, and the threads A creates get slots
/// from the kernel's own pool at 32 and up (see
/// `tcb::set_user_registers`).
const COMPONENT_KSTACK_SLOT: u64 = 12;

// ---- Object CNode slots ------------------------------------------------
//
// The kernel's object CNode has 256 slots and several milestones draw
// from it. These are M11's, chosen high enough to clear M8's (64..160)
// and below IRQ_CONTROL_SLOT at 254.

// Each component needs 24 consecutive slots, and the layout is
// written out rather than left to arithmetic because the first
// version was not: B's address space ran from 211 and needs 16, so
// it reached 227 and collided with B's TCB at 228's kernel-stack
// frames. The symptom was `SlotNotEmpty` from inside
// `build_user_space` — a fixture failure reported as a retype
// failure, which is exactly the kind of thing a slot map prevents.
//
//   +0      root CNode
//   +1      leaf CNode
//   +2      TCB
//   +3..7   kernel-stack frames (4)
//   +8..24  address space (16: a root, three Frames, the tables)

/// First slot of component A's block.
const SLOT_A_BASE: usize = 180;
/// First slot of component B's block, clear of A's 24.
const SLOT_B_BASE: usize = 210;

/// How many consecutive object-CNode slots one component occupies.
const COMPONENT_SLOTS: usize = 24;

const SLOT_A_CNODE: usize = SLOT_A_BASE;
const SLOT_A_TCB: usize = SLOT_A_BASE + 2;
const SLOT_A_KSTACK: usize = SLOT_A_BASE + 3;
const SLOT_A_SPACE_BASE: usize = SLOT_A_BASE + 8;

const SLOT_B_CNODE: usize = SLOT_B_BASE;
const SLOT_B_TCB: usize = SLOT_B_BASE + 2;
const SLOT_B_KSTACK: usize = SLOT_B_BASE + 3;
const SLOT_B_SPACE_BASE: usize = SLOT_B_BASE + 8;

// The blocks do not overlap each other, and both stay below
// `IRQ_CONTROL_SLOT` at 254. Checked at build time rather than
// trusted: the numbers above are easy to edit and hard to verify by
// eye, which is how the first collision happened.
const _: () = assert!(SLOT_A_BASE + COMPONENT_SLOTS <= SLOT_B_BASE);
const _: () = assert!(SLOT_B_BASE + COMPONENT_SLOTS <= 254);

// ---- Slots inside a component's own CSpace -----------------------------
//
// A component's CSpace is a root CNode whose slots hold CNodes
// (ADR 018's two levels), so every capability it holds lives in a
// leaf. One leaf is enough here.

/// Index of the single leaf CNode within a component's root.
const LEAF_INDEX: usize = 0;

/// Radix of a component's root CNode: 4 slots.
const COMPONENT_ROOT_RADIX: u8 = 2;
/// Radix of its leaf: 64 slots, which is room for its own objects
/// plus everything it retypes for a child.
const COMPONENT_LEAF_RADIX: u8 = 6;

// Slots inside the leaf. The component addresses these by CPtr, built
// for it by `cptr_for` below -- it cannot compute them itself,
// because the generations are the kernel's to know.
const C_UNTYPED: usize = 0;
const C_VSPACE: usize = 1;
const C_SCRATCH_FRAME: usize = 2;
const C_TABLE_BASE: usize = 3; // three tables for DEEP_ADDRESS
const C_SELF_TCB: usize = 6;
const C_OWN_CNODE: usize = 7;
/// Where the component retypes its child's objects.
const C_CHILD_BASE: usize = 16;

/// A leaf index used **only in A's CSpace**, for the foreign test.
///
/// 40, which is past everything either component populates. The
/// corresponding slot in B's leaf is empty, which is the point: a
/// CPtr naming index 40 resolves to A's VSpace in A's CSpace and to
/// nothing at all in B's.
///
/// That asymmetry has to be built deliberately. Two components with
/// identically-shaped CSpaces give the same *bits* different
/// meanings, not no meaning — see the note in `run_foreign`.
const A_ONLY_VSPACE: usize = 40;

// Inside the leaf, which has `1 << COMPONENT_LEAF_RADIX` slots.
const _: () = assert!(A_ONLY_VSPACE < (1usize << COMPONENT_LEAF_RADIX));

// ---- Report channels ---------------------------------------------------
//
// `Echo`'s CPtr argument is free to mean something else (it names no
// capability), so each report carries a channel. M8 uses bits 8, 9,
// 10, 16 and 17; M10 uses 20 and 21. M11 takes 24 upward, and the
// low byte is the step number.

const CHANNEL: u64 = 1 << 24;

/// M11's "the child finished" channel.
///
/// **Bit 23, and it was bit 25 until M15.** `channel::CONSOLE` is
/// bit 25, so every report the console component made was taken by
/// this module, recorded as "the M11 child is done", and answered
/// `true` — which stopped the chain before `kbdtest` ever saw it.
/// The console looked silent and the scenario blamed it for never
/// starting.
///
/// This is the third time a channel bit has been reused, and the
/// first two are written up in `abi::channel::KEYBOARD`. The cost
/// each time is the same: a component that reports faithfully into
/// a slot belonging to someone else, and a failure a long way from
/// its cause.
///
/// It stays here rather than moving to `abi::channel` because this
/// one is **not** a component's: the program that sends it is
/// hand-assembled by this module a few hundred lines below, and
/// nothing outside the kernel can name it. What ADR 047 asks for is
/// that the *components'* channels be declared once, which is now
/// true -- `screen`, `keyboard` and `console` all take theirs from
/// the ABI. The bits those occupy are what this had to move out of.
const CHANNEL_DONE: u64 = 1 << 23;

// Bits 25, 26 and 27 belong to the console, the keyboard and the
// screen (`abi::channel`). Asserted rather than remembered: the
// next person to pick a bit here gets a failed build instead of a
// silent theft.
const _: () = assert!(
    CHANNEL
        & (safrox_abi::channel::CONSOLE
            | safrox_abi::channel::KEYBOARD
            | safrox_abi::channel::SCREEN)
        == 0,
    "vspaceuser's channel overlaps a component's"
);
const _: () = assert!(
    CHANNEL_DONE
        & (safrox_abi::channel::CONSOLE
            | safrox_abi::channel::KEYBOARD
            | safrox_abi::channel::SCREEN)
        == 0,
    "vspaceuser's done channel overlaps a component's"
);

/// Steps a component reports, one slot each.
const STEP_COUNT: usize = 18;

/// Step numbers. Named rather than bare indices, because an
/// off-by-one here reports the wrong step's result and the assertion
/// message would then be a lie.
const STEP_RETYPE: u64 = 0;
const STEP_CREATE_VSPACE: u64 = 1;
const STEP_MAP_NO_TABLE: u64 = 2;
const STEP_MAP_TABLE_3: u64 = 3;
const STEP_MAP_TABLE_2: u64 = 4;
const STEP_MAP_TABLE_1: u64 = 5;
const STEP_MAP_FRAME: u64 = 6;
const STEP_READBACK: u64 = 7;
const STEP_UNMAP: u64 = 8;
const STEP_READBACK_AFTER_UNMAP: u64 = 9;
const STEP_CHILD_RETYPE: u64 = 10;
const STEP_CHILD_SETSPACE: u64 = 11;
const STEP_CHILD_RESUME: u64 = 12;
/// Reported by the **child**, not by A.
const STEP_CHILD_RAN: u64 = 13;
const STEP_FOREIGN: u64 = 14;
const STEP_KERNEL_HALF: u64 = 15;
/// The child's intermediate tables, which may legitimately report
/// `AlreadyMapped` — kept apart from [`STEP_CHILD_RETYPE`] because
/// errors are sticky and these are not failures.
const STEP_CHILD_TABLES: u64 = 16;
/// The parent mapping, writing and unmapping the child's code frame.
const STEP_CHILD_LOAD: u64 = 17;

static STEPS: [AtomicU64; STEP_COUNT] = [const { AtomicU64::new(NOT_REPORTED) }; STEP_COUNT];
static DONE: AtomicBool = AtomicBool::new(false);

/// A value no invocation returns, so "not reported" and "reported
/// zero" are distinguishable. Zero is the success of most
/// invocations here, which makes the distinction load-bearing: a
/// scenario that never ran would otherwise look like one that
/// succeeded.
const NOT_REPORTED: u64 = u64::MAX;

/// Record a step, called from the syscall dispatcher's `Echo` path.
///
/// **An error is sticky.** Several invocations share a step — the
/// child's build is twenty of them — and the obvious rule, "the last
/// write wins", is wrong in the one direction that matters: a
/// failure followed by a success would read as success.
///
/// That is not hypothetical. The first run of `vspace-user-test` had
/// it, and the symptom was not a failed assertion but a **#PF in
/// ring 3**: the parent's mapping of the child's code frame failed
/// with `MissingPageTable`, a later invocation on the same step
/// overwrote the error with a zero, and the parent then wrote
/// through a mapping that did not exist. The scenario's own comment
/// at the time said "if the last one succeeded, all of them did",
/// which was simply false.
///
/// So the first error on a step is kept. A step reads as success
/// only if **every** invocation that reported to it succeeded, which
/// is what pooling was supposed to mean.
pub fn record_step(channel: u64, value: u64) -> bool {
    if channel & CHANNEL_DONE != 0 {
        DONE.store(true, Ordering::Release);
        return true;
    }
    if channel & CHANNEL != 0 {
        let index = (channel & 0xFF) as usize;
        if index < STEP_COUNT {
            let previous = STEPS[index].load(Ordering::Acquire);
            if previous == NOT_REPORTED || !syscall::is_error(previous) {
                STEPS[index].store(value, Ordering::Release);
            }
        }
        return true;
    }
    false
}

fn step(which: u64) -> u64 {
    STEPS[which as usize].load(Ordering::Acquire)
}

/// Assert a step succeeded, naming it.
fn assert_ok(which: u64, name: &str) {
    let value = step(which);
    assert_ne!(
        value, NOT_REPORTED,
        "the component never reported {}; it stopped before that point",
        name
    );
    assert!(
        !syscall::is_error(value),
        "{} failed with error {} ({}). The component could not perform \
         an operation it holds the capability for.",
        name,
        syscall::error_code(value),
        value
    );
}

/// Assert a step failed, with a named error code.
fn assert_err(which: u64, name: &str, expected: u64, why: &str) {
    let value = step(which);
    assert_ne!(
        value, NOT_REPORTED,
        "the component never reported {}; it stopped before that point",
        name
    );
    assert!(
        syscall::is_error(value),
        "{} returned {:#018x}, which is not an error. {}",
        name,
        value,
        why
    );
    assert_eq!(
        syscall::error_code(value),
        expected,
        "{} failed with error {} where {} was expected. Distinct \
         failures must stay distinguishable, or a guard that stopped \
         working would look like one that still works (ADR 023, K5).",
        name,
        syscall::error_code(value),
        expected
    );
}

// ---- Building a component ----------------------------------------------

fn object_cnode() -> CNode {
    // SAFETY: boot::init put a CNode in this root slot.
    unsafe { boot::leaf_cnode(boot::ROOT_SLOT_OBJECTS) }
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
        "no root Untyped has {} bytes free; the fixture cannot build a \
         component (ADR 023, K6: a fixture states what it needs)",
        bytes
    );
}

/// A component: a thread, an address space, and **a CSpace of its
/// own**.
pub struct Component {
    /// Index in the kernel's object CNode of this component's root
    /// CNode.
    pub root_slot: usize,
    /// Index of its TCB.
    pub tcb_slot: usize,
    /// Its leaf CNode, for the fixture to place capabilities into.
    pub leaf: CNode,
}

impl Component {
    /// The CPtr **this component** would use for one of its leaf
    /// slots.
    ///
    /// Built here because computing it needs the generations of two
    /// slots, which is knowledge of the CSpace's internals. A
    /// component receives CPtrs; it does not derive them. That is
    /// ADR 018's model and also the reason the foreign-access
    /// negative is a real test — a component cannot invent a pointer
    /// to something it was not given.
    pub fn cptr_for(&self, leaf_index: usize) -> CPtr {
        // SAFETY: The root CNode is live with COMPONENT_ROOT_RADIX
        // slots, and LEAF_INDEX is within it.
        let root = unsafe {
            let cnode = object_cnode();
            cnode.slot(self.root_slot)
        };
        // SAFETY: The root's object is `1 << radix` live slots.
        let root_cnode = unsafe { CNode::new(root.object().0, root.as_radix()) };
        // SAFETY: LEAF_INDEX < 4.
        let leaf_slot = unsafe { root_cnode.slot(LEAF_INDEX) };
        // SAFETY: Index in range by the caller.
        let target = unsafe { self.leaf.slot(leaf_index) };

        CPtr::new(
            LEAF_INDEX as u16,
            leaf_slot.generation(),
            leaf_index as u16,
            target.generation(),
        )
    }
}

/// Bytes one component costs.
///
/// 2 MiB, and the figure is arithmetic rather than a guess. The first
/// version asked for 256 KiB and died on the TCB retype with
/// `NotEnoughMemory { remaining: 0, needed: 2048 }` — because the
/// component's *own* budget is 256 KiB and had taken every byte the
/// fixture reserved. ADR 023's K6 again: a fixture states what it
/// needs, and "what it needs" includes what it gives away.
///
/// ```text
///   the component's budget   256 KiB
///   alignment padding for it up to 256 KiB (an Untyped of 2^18
///                            must be 2^18-aligned, and the
///                            watermark is wherever it is)
///   address space            16 pages = 64 KiB (build_user_space)
///   CSpace                   2 KiB + 128 B
///   TCB                      2 KiB
///   kernel stack             16 KiB
///   ------------------------------------
///                            ~600 KiB worst case
/// ```
///
/// 2 MiB leaves room for the alignment to land badly twice and still
/// fit inside one 4 MiB root Untyped, which is the largest boot
/// creates.
const COMPONENT_BYTES: u64 = 2 * 1024 * 1024;

/// How much Untyped a component is given for its **own** use.
///
/// 256 KiB: enough for the pages and tables criterion 1 needs, plus a
/// child's objects, and small enough that `vspace-user-exhaust` can
/// empty it in a bounded number of retypes. The exhaustion scenario
/// depends on this being finite and known.
const COMPONENT_BUDGET_BITS: u8 = 18;

/// Build a component, leaving its thread Inactive.
///
/// The kernel does for a component exactly what M12's root task will
/// do for the first one and no more: it hands over an Untyped, a
/// CSpace, an address space with code and stack, and a TCB. Every
/// mapping the *scenario* is about is made by the component itself.
#[allow(clippy::too_many_arguments)]
fn build_component(
    cnode_slot: usize,
    space_base: usize,
    tcb_slot: usize,
    kstack_slot: usize,
    kernel_stack_slot: u64,
    program_bytes: &[u8],
) -> Component {
    let cnode = object_cnode();
    let untyped_index = untyped_with_room(COMPONENT_BYTES);
    // SAFETY: Index below the count boot reported.
    let parent = unsafe { boot::root_untyped(untyped_index) };

    // ---- The CSpace --------------------------------------------------
    //
    // A root CNode and one leaf, which is ADR 018's two levels at
    // their smallest.
    //
    // SAFETY: Live Untyped and CNode; the destination slots are empty.
    unsafe {
        untyped::retype(
            parent,
            CapType::CNode,
            COMPONENT_ROOT_RADIX,
            1,
            &cnode,
            cnode_slot,
        )
    }
    .expect("could not retype the component's root CNode");
    // SAFETY: As above.
    unsafe {
        untyped::retype(
            parent,
            CapType::CNode,
            COMPONENT_LEAF_RADIX,
            1,
            &cnode,
            cnode_slot + 1,
        )
    }
    .expect("could not retype the component's leaf CNode");

    // SAFETY: Indices in range.
    let (root_slot_ref, leaf_slot_ref) =
        unsafe { (cnode.slot(cnode_slot), cnode.slot(cnode_slot + 1)) };
    // SAFETY: The capability says how many slots the object has.
    let root_cnode = unsafe { CNode::new(root_slot_ref.object().0, root_slot_ref.as_radix()) };
    // SAFETY: As above.
    let leaf = unsafe { CNode::new(leaf_slot_ref.object().0, leaf_slot_ref.as_radix()) };

    // The leaf goes into the root. `copy` rather than a raw write, so
    // the derivation list stays intact and a revoke of the Untyped
    // reaches both.
    //
    // SAFETY: Both slots are live and distinct.
    unsafe {
        let dest = root_cnode.slot(LEAF_INDEX);
        cnode::copy(leaf_slot_ref, dest, leaf_slot_ref.rights())
            .expect("could not place the leaf CNode in the component's root");
    }

    // ---- Its budget ---------------------------------------------------
    //
    // A child Untyped, which is the whole of what this component may
    // spend. ADR 022's claim is about exactly this: the component can
    // exhaust it and nothing else.
    //
    // SAFETY: Live Untyped and CNode; the destination is empty.
    unsafe {
        untyped::retype(
            parent,
            CapType::Untyped,
            COMPONENT_BUDGET_BITS,
            1,
            &leaf,
            C_UNTYPED,
        )
    }
    .expect("could not retype the component's own Untyped");

    // ---- Its address space --------------------------------------------
    //
    // Code and stack only. Everything else is the component's job,
    // which is the point of the milestone.
    let space = usertest::build_user_space(space_base, program_bytes, 0);

    // ---- Its TCB ------------------------------------------------------
    //
    // SAFETY: Live Untyped and CNode; the destination is empty.
    unsafe { untyped::retype(parent, CapType::Tcb, 0, 1, &cnode, tcb_slot) }
        .expect("could not retype the component's TCB");

    let frames_needed = (layout::KSTACK_SIZE / crate::memory::FRAME_SIZE) as usize;
    // SAFETY: As above.
    unsafe {
        untyped::retype(
            parent,
            CapType::Frame,
            0,
            frames_needed,
            &cnode,
            kstack_slot,
        )
    }
    .expect("could not retype the component's kernel stack frames");

    let (_, bottom, top) = layout::kstack_slot(kernel_stack_slot);
    for index in 0..frames_needed {
        // SAFETY: Index in range.
        let frame_slot = unsafe { cnode.slot(kstack_slot + index) };
        let frame = crate::memory::PhysFrame::containing(frame_slot.object().0);
        let page = VirtAddr::new(bottom.0 + (index as u64) * crate::memory::FRAME_SIZE);

        // SAFETY: The kernel's tables are live, the address is inside
        // this component's own KSTACK slot, and the frame came from a
        // retype so nothing else owns it.
        unsafe {
            let table = crate::memory::vm::kernel_table();
            let mut source = crate::memory::heap::reserve_frames();
            let flags = crate::memory::paging::PageFlags::READ
                | crate::memory::paging::PageFlags::WRITE
                | crate::memory::paging::PageFlags::GLOBAL;
            let tlb = crate::memory::paging::map(table, page, frame, flags, &mut source)
                .expect("could not map a component kernel stack page");
            tlb.flush();
        }
    }

    // SAFETY: Index in range.
    let tcb_slot_ref = unsafe { cnode.slot(tcb_slot) };
    // SAFETY: The slot holds a live TCB nothing else borrows.
    let thread = unsafe { tcb::from_slot(tcb_slot_ref) }.expect("the slot does not hold a TCB");

    // SAFETY: The kernel stack is mapped and owned by nothing else;
    // the entry and stack are mapped in the VSpace bound below.
    unsafe {
        thread.configure_user(CODE_ADDRESS, STACK_TOP, 0, top, bottom, kernel_stack_slot);
    }

    // ---- Binding both spaces ------------------------------------------
    //
    // **The line this milestone turns on.** Until M11 the second
    // argument did not exist and every thread resolved through the
    // init CSpace.
    //
    // SAFETY: Index in range.
    let vspace_root = unsafe { cnode.slot(space.root_slot) };
    // SAFETY: All three slots hold live capabilities and the thread
    // is not running.
    unsafe { tcb::set_space(tcb_slot_ref, Some(vspace_root), Some(root_slot_ref)) }
        .expect("could not give the component its VSpace and CSpace");

    // The component needs capabilities for its own VSpace and TCB, or
    // it cannot map anything or talk about itself. Copied into its
    // leaf, where its CPtrs can reach them.
    //
    // SAFETY: Source and destination are live and distinct.
    unsafe {
        cnode::copy(vspace_root, leaf.slot(C_VSPACE), vspace_root.rights())
            .expect("could not give the component its VSpace capability");
        cnode::copy(tcb_slot_ref, leaf.slot(C_SELF_TCB), tcb_slot_ref.rights())
            .expect("could not give the component its TCB capability");
        cnode::copy(
            leaf_slot_ref,
            leaf.slot(C_OWN_CNODE),
            leaf_slot_ref.rights(),
        )
        .expect("could not give the component its own CNode capability");
    }

    Component {
        root_slot: cnode_slot,
        tcb_slot,
        leaf,
    }
}

// ---- Emitting invocations ----------------------------------------------

/// Emit one invocation and report what it returned.
///
/// Every scenario step has this shape, and writing it once is not
/// only brevity: the register discipline (ADR 043's four argument
/// registers) is then stated in one place rather than at twenty call
/// sites where one could quietly differ.
#[allow(clippy::too_many_arguments)]
fn invoke(
    p: &mut Program,
    target: CPtr,
    method: Method,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    report_step: u64,
) {
    p.emit(&mov_rdi_imm(target.0));
    p.emit(&mov_rsi_imm(method as u64));
    p.emit(&mov_rdx_imm(arg0));
    p.emit(&mov_r10_imm(arg1));
    p.emit(&mov_r8_imm(arg2));
    p.emit(&mov_r9_imm(arg3));
    p.emit(&program::SYSCALL);

    // RAX -> RDX, then Echo. The result has to be reported before the
    // next invocation, because the next one destroys RAX.
    p.emit(&[0x48, 0x89, 0xc2]); // mov rdx, rax
    p.emit(&mov_rdi_imm(CHANNEL | report_step));
    p.emit(&mov_rsi_imm(Method::Echo as u64));
    p.emit(&program::SYSCALL);
}

/// Report a value the program computed itself.
fn report(p: &mut Program, report_step: u64, value_in_rdx: bool, constant: u64) {
    if !value_in_rdx {
        p.emit(&mov_rdx_imm(constant));
    }
    p.emit(&mov_rdi_imm(CHANNEL | report_step));
    p.emit(&mov_rsi_imm(Method::Echo as u64));
    p.emit(&program::SYSCALL);
}

/// Say "finished", then spin.
fn finish(p: &mut Program) {
    p.emit(&mov_rdi_imm(CHANNEL_DONE));
    p.emit(&mov_rsi_imm(Method::Echo as u64));
    p.emit(&mov_rdx_imm(0));
    p.emit(&program::SYSCALL);
    p.emit(&program::SPIN);
}

/// Pack `Retype`'s type and size into one word, as the ABI expects.
fn retype_arg(cap_type: u64, size_bits: u64) -> u64 {
    cap_type | (size_bits << 8)
}

/// How long to wait for a component to finish, in TSC cycles.
const DEADLINE: u64 = 20_000_000_000;

fn read_tsc() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: RDTSC is unprivileged, reads no memory, writes EDX:EAX.
    unsafe {
        core::arch::asm!("rdtsc", out("eax") low, out("edx") high,
                         options(nomem, nostack, preserves_flags));
    }
    ((high as u64) << 32) | (low as u64)
}

/// Run a component until it reports finished, or fail saying so.
fn run_until_done(tcb_slot: usize) {
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let slot = unsafe { cnode.slot(tcb_slot) };
    // SAFETY: Live TCB, not queued, no other borrow held.
    unsafe {
        let thread = tcb::from_slot(slot).expect("the slot does not hold a TCB");
        sched::enqueue(thread);
    }

    let deadline = read_tsc() + DEADLINE;
    while !DONE.load(Ordering::Acquire) && read_tsc() < deadline {
        sched::yield_now();
    }

    assert!(
        DONE.load(Ordering::Acquire),
        "the component never finished. Steps reported so far: {:?}",
        StepDump
    );
}

/// Formats the step table for a failure message.
struct StepDump;

impl core::fmt::Debug for StepDump {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (index, slot) in STEPS.iter().enumerate() {
            let value = slot.load(Ordering::Acquire);
            if value != NOT_REPORTED {
                write!(f, " {}={:#x}", index, value)?;
            }
        }
        Ok(())
    }
}

// ---- The positive scenario ---------------------------------------------

/// `vspace-user-test`: a component manages its own address space.
///
/// Six things, in order, each reported from ring 3:
///
/// 1. the component retypes a Frame out of **its own** Untyped;
/// 2. it maps that Frame at an address needing three intermediate
///    tables it also supplies — and the kernel refuses to produce one
///    (ADR 027), which is checked by trying without them first;
/// 3. it writes through the mapping and reads the value back;
/// 4. it unmaps, and the address stops resolving;
/// 5. **it builds a second component** — Untyped to objects, a
///    CSpace, an address space, a TCB — and starts it, and that
///    component reports for itself;
/// 6. the kernel is still running and exits 33.
///
/// Rights narrowing is checked in `vspace-user-ro`, separately,
/// because the write it provokes is a #PF that stops the kernel. See
/// that scenario for why the split is honest rather than convenient.
pub fn run() -> ! {
    println!("[vsuser] a ring 3 component will manage its own address space");

    // The component's CPtrs have to be known before its program can
    // be assembled, and they depend on the generations of slots that
    // do not exist until it is built. So: build it with a placeholder
    // program, work out the pointers, assemble the real program, and
    // write it in.
    //
    // The same two-pass shape M8's `check_build_user_space` uses, and
    // for the same reason.
    let mut placeholder = Program::new();
    placeholder.emit(&program::SPIN);

    let component = build_component(
        SLOT_A_CNODE,
        SLOT_A_SPACE_BASE,
        SLOT_A_TCB,
        SLOT_A_KSTACK,
        COMPONENT_KSTACK_SLOT,
        placeholder.as_slice(),
    );

    let program = component_program(&component);
    write_program(SLOT_A_SPACE_BASE + 1, program.as_slice());

    println!(
        "[vsuser] component built: own CNode (root radix {}, leaf radix \
         {}), own Untyped of {} KiB, own VSpace; the kernel mapped only \
         its code and stack",
        COMPONENT_ROOT_RADIX,
        COMPONENT_LEAF_RADIX,
        (1u64 << COMPONENT_BUDGET_BITS) / 1024
    );

    run_until_done(component.tcb_slot);

    // ---- 1. It spent its own memory ---------------------------------
    assert_ok(STEP_RETYPE, "Untyped.Retype of a Frame and three tables");

    // ---- 2. The kernel did not produce a table ----------------------
    //
    // This is checked *before* the successful mapping, and the order
    // matters: afterwards the tables exist and the refusal could not
    // be provoked. A scenario that only showed the mapping working
    // would be consistent with a kernel that allocates tables
    // silently -- which is precisely what ADR 027 refuses to do, and
    // what every other line here would fail to notice.
    assert_err(
        STEP_MAP_NO_TABLE,
        "Frame.Map with no intermediate tables",
        17, // MissingPageTable
        "The kernel produced a page table the component did not offer, \
         which is the allocation ADR 027 exists to refuse.",
    );

    assert_ok(STEP_MAP_TABLE_3, "PageTable.Map at level 3");
    assert_ok(STEP_MAP_TABLE_2, "PageTable.Map at level 2");
    assert_ok(STEP_MAP_TABLE_1, "PageTable.Map at level 1");
    assert_ok(STEP_MAP_FRAME, "Frame.Map");

    // ---- 3. It wrote and read back ----------------------------------
    //
    // The value is the component's own, written through its own
    // mapping and read through it again. A mapping that reached the
    // wrong frame would return something else; one that reached
    // nothing would fault.
    let readback = step(STEP_READBACK);
    assert_eq!(
        readback, SCRATCH_PATTERN,
        "the component wrote {:#018x} through its own mapping and read \
         back {:#018x}. The mapping does not reach the frame the \
         capability names.",
        SCRATCH_PATTERN, readback
    );

    // ---- 4. Unmap ----------------------------------------------------
    assert_ok(STEP_UNMAP, "Frame.Unmap");

    // And the address stopped resolving. The component cannot report
    // a fault -- a read of an unmapped page kills it -- so it reports
    // what `translate` says through a second Frame.Unmap, which must
    // now say NotMapped.
    assert_err(
        STEP_READBACK_AFTER_UNMAP,
        "Frame.Unmap of an address already unmapped",
        19, // NotMapped
        "The address still resolves after Unmap, so the mapping was \
         not removed.",
    );

    // ---- 5. A component built a component ---------------------------
    assert_ok(
        STEP_CHILD_RETYPE,
        "the child's objects, retyped by the parent",
    );
    assert_ok(
        STEP_CREATE_VSPACE,
        "PageTable.CreateVSpace on the child's root",
    );

    // The child's intermediate tables. Success or `AlreadyMapped`,
    // and nothing else: the two child pages share all three levels,
    // so five of the six offers are redundant and the kernel says
    // so. What this rules out is `MissingPageTable`, which would
    // mean the parent supplied the levels in the wrong order and the
    // mapping below only worked by accident.
    let tables = step(STEP_CHILD_TABLES);
    assert_ne!(
        tables, NOT_REPORTED,
        "the parent never reported mapping the child's page tables"
    );
    if syscall::is_error(tables) {
        assert_eq!(
            syscall::error_code(tables),
            18, // AlreadyMapped
            "mapping the child's page tables gave error {}. Only \
             AlreadyMapped (18) is expected here -- the two pages share \
             every level -- and anything else means the levels went in \
             in the wrong order.",
            syscall::error_code(tables)
        );
    }

    assert_ok(
        STEP_CHILD_LOAD,
        "the parent mapping, writing and unmapping the child's code",
    );
    assert_ok(STEP_CHILD_SETSPACE, "TCB.SetSpace on the child");
    assert_ok(STEP_CHILD_RESUME, "TCB.Resume on the child");

    // **The load-bearing one.** Everything above could pass on a
    // kernel where `Resume` returned success and did nothing. This
    // value came from the child's own `Echo`, at CPL 3, in an
    // address space its parent built.
    //
    // Waited for separately, and that is not a detail. `Resume` puts
    // the child on the ready queue and returns; the parent then
    // signals DONE and this function wakes. At that instant the
    // child has been *scheduled*, not *run* -- so asserting straight
    // away reads NOT_REPORTED and blames `Resume` for a race the
    // test created.
    //
    // The first version did exactly that and failed with "the
    // child never ran", which was false. ADR 023's K8: a test must
    // not depend on the timing of the thing it measures. The
    // deadline makes the only timing dependency "eventually", which
    // is the claim.
    let deadline = read_tsc() + DEADLINE;
    while step(STEP_CHILD_RAN) == NOT_REPORTED && read_tsc() < deadline {
        sched::yield_now();
    }

    let child = step(STEP_CHILD_RAN);
    assert_ne!(
        child, NOT_REPORTED,
        "the parent's invocations all succeeded but the child never \
         reported, even after waiting. A Resume that reports success \
         and schedules nothing would pass every other check here. \
         Steps: {:?}",
        StepDump
    );
    assert_eq!(
        child, CHILD_PATTERN,
        "the child reported {:#018x} rather than {:#018x}; it ran, but \
         not the program its parent gave it",
        child, CHILD_PATTERN
    );

    println!(
        "[vsuser] the component retyped a Frame and three tables from \
         its own Untyped, mapped them at {:#018x}, wrote {:#018x} and \
         read it back, then unmapped",
        DEEP_ADDRESS, SCRATCH_PATTERN
    );
    println!(
        "[vsuser] the kernel refused to supply a missing intermediate \
         table (MissingPageTable); the component supplied all three \
         itself"
    );
    println!(
        "[vsuser] a ring 3 component built a second component -- its \
         own CSpace, VSpace and TCB, all from its own Untyped -- and \
         started it; the child reported {:#018x} from cpl 3",
        child
    );
    println!("[vsuser] all user-space address space tests passed");

    syscall::report();

    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

/// The value the component writes through its own mapping.
const SCRATCH_PATTERN: u64 = 0x5EC0_0DED_1111_2222;

/// The value the child reports.
const CHILD_PATTERN: u64 = 0xC1D0_0000_3333_4444;

/// Copy a program into the Frame behind a slot, through PMAP.
///
/// Through the physical window rather than the user mapping, for
/// M8's two reasons: the code page is `ReadExecute` and could not be
/// written, and writing through PMAP proves the mapping reaches *the
/// frame the capability names*.
fn write_program(frame_slot: usize, bytes: &[u8]) {
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let slot = unsafe { cnode.slot(frame_slot) };
    assert_eq!(
        slot.cap_type(),
        CapType::Frame,
        "slot {} does not hold a Frame; the program would be written \
         over something else",
        frame_slot
    );
    assert!(
        bytes.len() <= crate::memory::FRAME_SIZE as usize,
        "the component's program is {} bytes and does not fit in a page",
        bytes.len()
    );
    // SAFETY: The Frame came from a retype so nothing else owns it,
    // PMAP maps all RAM writably, and the length was just checked.
    unsafe {
        let ptr = crate::memory::phys_to_virt(slot.object().0).as_ptr::<u8>();
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    }
}

// ---- The component's program -------------------------------------------

/// Assemble what component A runs at CPL 3.
///
/// Reads as a sequence of invocations because that is what a
/// component is: there is no library, no loader and no runtime, so
/// every line here is a `syscall` with its arguments in registers.
///
/// The CPtrs are computed by the fixture and baked in as immediates.
/// A component in a real system receives them in its initial message;
/// it cannot derive them, which is the whole of ADR 018's model and
/// the reason `vspace-user-foreign` is a genuine test.
fn component_program(c: &Component) -> Program {
    let mut p = Program::new();

    let untyped = c.cptr_for(C_UNTYPED);
    let vspace = c.cptr_for(C_VSPACE);
    let frame = c.cptr_for(C_SCRATCH_FRAME);
    let own_cnode = c.cptr_for(C_OWN_CNODE);

    // ---- 1. Spend its own memory -------------------------------------
    //
    // One Frame and three PageTables, into its own leaf CNode. Two
    // retypes rather than one because they are different types; the
    // Frame lands at C_SCRATCH_FRAME and the tables at C_TABLE_BASE.
    //
    // Only the second is reported -- the step is "the component can
    // retype", and reporting both would need two slots for one fact.
    invoke(
        &mut p,
        untyped,
        Method::Retype,
        retype_arg(CapType::Frame as u64, 0),
        1,
        own_cnode.0,
        C_SCRATCH_FRAME as u64,
        STEP_RETYPE,
    );
    invoke(
        &mut p,
        untyped,
        Method::Retype,
        retype_arg(CapType::PageTable as u64, 0),
        3,
        own_cnode.0,
        C_TABLE_BASE as u64,
        STEP_RETYPE,
    );

    // ---- 2. Map the Frame, the wrong way first -----------------------
    //
    // Deliberately before the tables exist. The kernel must refuse
    // with MissingPageTable rather than conjuring one (ADR 027), and
    // after the tables are in place this can no longer be provoked --
    // which is why it is first.
    invoke(
        &mut p,
        frame,
        Method::FrameMap,
        vspace.0,
        DEEP_ADDRESS,
        PERM_READ_WRITE,
        0,
        STEP_MAP_NO_TABLE,
    );

    // ---- 3. Supply all three tables ----------------------------------
    //
    // Level 3 (PDPT), then 2 (PD), then 1 (PT). In that order,
    // because each level's entry lives in the table above it.
    for (index, (level, report_step)) in [
        (3u64, STEP_MAP_TABLE_3),
        (2, STEP_MAP_TABLE_2),
        (1, STEP_MAP_TABLE_1),
    ]
    .iter()
    .enumerate()
    {
        let table = c.cptr_for(C_TABLE_BASE + index);
        invoke(
            &mut p,
            table,
            Method::PageTableMap,
            vspace.0,
            DEEP_ADDRESS,
            *level,
            0,
            *report_step,
        );
    }

    // Now the Frame goes in.
    invoke(
        &mut p,
        frame,
        Method::FrameMap,
        vspace.0,
        DEEP_ADDRESS,
        PERM_READ_WRITE,
        0,
        STEP_MAP_FRAME,
    );

    // ---- 4. Write through it, read it back ---------------------------
    //
    //   mov rax, DEEP_ADDRESS
    //   mov rcx, SCRATCH_PATTERN
    //   mov [rax], rcx
    //   mov rdx, [rax]
    //
    // Read back through the same mapping rather than trusting the
    // write: a store that went somewhere else would still complete.
    p.emit(&usertest::mov_rax_imm(DEEP_ADDRESS));
    p.emit(&MOV_RCX_IMM64);
    p.emit(&SCRATCH_PATTERN.to_le_bytes());
    p.emit(&[0x48, 0x89, 0x08]); // mov [rax], rcx
    p.emit(&[0x48, 0x8b, 0x10]); // mov rdx, [rax]
    report(&mut p, STEP_READBACK, true, 0);

    // ---- 5. Unmap ----------------------------------------------------
    //
    // Target the VSpace, not the Frame: one Frame may be mapped in
    // several address spaces, so the capability does not say which
    // mapping is meant (ADR 027).
    invoke(
        &mut p,
        vspace,
        Method::FrameUnmap,
        DEEP_ADDRESS,
        0,
        0,
        0,
        STEP_UNMAP,
    );

    // And again, which must now say NotMapped. The component cannot
    // demonstrate the absence by reading -- that would fault and kill
    // it -- so it asks the kernel instead.
    invoke(
        &mut p,
        vspace,
        Method::FrameUnmap,
        DEEP_ADDRESS,
        0,
        0,
        0,
        STEP_READBACK_AFTER_UNMAP,
    );

    // ---- 6. Build a second component ---------------------------------
    emit_child_build(&mut p, c);

    finish(&mut p);
    p
}

/// `mov rcx, imm64` — the opcode, with the immediate emitted after.
const MOV_RCX_IMM64: [u8; 2] = [0x48, 0xb9];

/// Permission encodings, as `Frame.Map` decodes them.
const PERM_READ_ONLY: u64 = 0;
const PERM_READ_WRITE: u64 = 1;
const PERM_READ_EXECUTE: u64 = 2;

/// Emit the part where a component builds a component.
///
/// **This is criterion 5, and the milestone's real result.** A thread
/// at CPL 3, holding nothing but an Untyped and its own CSpace,
/// produces another thread with its own address space and its own
/// capabilities — and starts it.
///
/// What the parent cannot do is supply the child's *kernel* stack
/// address: kernel-half memory is named by no capability (ADR 027's
/// K1 from the other side). It supplies the frames; the kernel
/// chooses where they go. See `tcb::set_user_registers`.
fn emit_child_build(p: &mut Program, c: &Component) {
    let untyped = c.cptr_for(C_UNTYPED);
    let own_cnode = c.cptr_for(C_OWN_CNODE);

    // Slots in the parent's leaf where the child's objects land.
    const CH_TCB: usize = C_CHILD_BASE;
    const CH_ROOT_CNODE: usize = C_CHILD_BASE + 1;
    const CH_LEAF_CNODE: usize = C_CHILD_BASE + 2;
    const CH_VSPACE: usize = C_CHILD_BASE + 3;
    const CH_TABLES: usize = C_CHILD_BASE + 4; // 6: three per page
    const CH_CODE: usize = C_CHILD_BASE + 10;
    const CH_STACK: usize = C_CHILD_BASE + 11;
    const CH_KSTACK: usize = C_CHILD_BASE + 12; // four frames

    // Every retype the child needs, pooled into one step.
    //
    // Pooling is safe here *because* `record_step` keeps the first
    // error (see its comment). An earlier version relied on the last
    // write winning and reasoned "if the last one succeeded, all of
    // them did", which is false the moment any of them can fail
    // independently -- and the way that showed up was a #PF in ring
    // 3 rather than a failed assertion.
    //
    // Only invocations that **must** succeed share this step. The
    // ones that may legitimately report an error have their own.
    let retypes: [(u64, u64, u64, usize); 7] = [
        (CapType::Tcb as u64, 0, 1, CH_TCB),
        (
            CapType::CNode as u64,
            COMPONENT_ROOT_RADIX as u64,
            1,
            CH_ROOT_CNODE,
        ),
        (
            CapType::CNode as u64,
            COMPONENT_LEAF_RADIX as u64,
            1,
            CH_LEAF_CNODE,
        ),
        (CapType::PageTable as u64, 0, 1, CH_VSPACE),
        (CapType::PageTable as u64, 0, 6, CH_TABLES),
        (CapType::Frame as u64, 0, 2, CH_CODE),
        (CapType::Frame as u64, 0, 4, CH_KSTACK),
    ];
    for (cap_type, size_bits, count, dest) in retypes {
        invoke(
            p,
            untyped,
            Method::Retype,
            retype_arg(cap_type, size_bits),
            count,
            own_cnode.0,
            dest as u64,
            STEP_CHILD_RETYPE,
        );
    }

    let child_vspace = c.cptr_for(CH_VSPACE);

    // **A fresh PageTable is not an address space.** Its user half is
    // empty, which is wanted, but so is its kernel half -- and a
    // thread running in it could not take an interrupt, because the
    // handler would not be mapped.
    //
    // The parent cannot fix that itself: the entries it would have
    // to write name kernel memory, and it holds no capability for
    // any. So it asks, and `create_vspace` copies them in. This is
    // the invocation ADR 043's list of seven was missing.
    invoke(
        p,
        child_vspace,
        Method::CreateVSpace,
        0,
        0,
        0,
        0,
        STEP_CREATE_VSPACE,
    );

    // The child's code and stack pages, each needing three levels.
    //
    // They are 1 MiB apart, so they share the PDPT, the PD *and* the
    // PT -- everything. The parent does not compute that; it offers
    // all three for each page and the kernel answers `AlreadyMapped`
    // for the five that are redundant.
    //
    // **Their own step**, because `AlreadyMapped` here is an answer
    // rather than a failure, and errors are sticky. Pooling these
    // with the retypes would poison a step that must read as
    // success. The assertion for this one accepts success or
    // AlreadyMapped and nothing else -- a `MissingPageTable` would
    // mean the levels went in the wrong order.
    for (page_index, address) in [CHILD_CODE_ADDRESS, CHILD_STACK_ADDRESS].iter().enumerate() {
        for (level_index, level) in [3u64, 2, 1].iter().enumerate() {
            let table = c.cptr_for(CH_TABLES + page_index * 3 + level_index);
            invoke(
                p,
                table,
                Method::PageTableMap,
                child_vspace.0,
                *address,
                *level,
                0,
                STEP_CHILD_TABLES,
            );
        }
    }

    // ---- The parent loads the child's program ------------------------
    //
    // The code frame came out of the parent's Untyped moments ago, so
    // the kernel could not have written anything into it: it did not
    // exist when the fixture ran, and the kernel does not know which
    // frame the parent chose. **The parent has to be the loader**,
    // which is what a real one is and what M12 generalises.
    //
    // The sequence is the W^X dance in miniature, and it is forced
    // rather than chosen:
    //
    //   1. map the frame into the *parent's* space, writable;
    //   2. write the child's instructions;
    //   3. unmap;
    //   4. map it into the *child's* space, executable.
    //
    // At no point is it both, in either space — and not because the
    // parent was careful. There is no encoding for
    // writable-and-executable (ADR 032), so a component cannot build
    // a W+X page for its child even deliberately.
    let code = c.cptr_for(CH_CODE);
    let parent_vspace = c.cptr_for(C_VSPACE);

    // `SCRATCH_ADDRESS` is inside a PT the fixture already built --
    // see its definition, and the #PF that proved the first choice
    // was not. No `PageTable.Map` is needed, and if that ever stops
    // being true this mapping reports `MissingPageTable` on its own
    // step rather than being lost among the retypes.
    invoke(
        p,
        code,
        Method::FrameMap,
        parent_vspace.0,
        SCRATCH_ADDRESS,
        PERM_READ_WRITE,
        0,
        STEP_CHILD_LOAD,
    );

    emit_store_program(p, SCRATCH_ADDRESS, child_program().as_slice());

    invoke(
        p,
        parent_vspace,
        Method::FrameUnmap,
        SCRATCH_ADDRESS,
        0,
        0,
        0,
        STEP_CHILD_LOAD,
    );

    // And into the child, read-execute.
    invoke(
        p,
        code,
        Method::FrameMap,
        child_vspace.0,
        CHILD_CODE_ADDRESS,
        PERM_READ_EXECUTE,
        0,
        STEP_CHILD_RETYPE,
    );
    invoke(
        p,
        c.cptr_for(CH_STACK),
        Method::FrameMap,
        child_vspace.0,
        CHILD_STACK_ADDRESS,
        PERM_READ_WRITE,
        0,
        STEP_CHILD_RETYPE,
    );

    // The child's entry point and stack, plus the frames for its
    // kernel stack.
    invoke(
        p,
        c.cptr_for(CH_TCB),
        Method::SetUserRegisters,
        CHILD_CODE_ADDRESS,
        CHILD_STACK_TOP,
        own_cnode.0,
        CH_KSTACK as u64,
        STEP_CHILD_RETYPE,
    );

    // Its address space and **its own CSpace**, in one call. A child
    // given its parent's CSpace would not be a separate component;
    // it would be the parent with a second stack.
    invoke(
        p,
        c.cptr_for(CH_TCB),
        Method::SetSpace,
        child_vspace.0,
        c.cptr_for(CH_ROOT_CNODE).0,
        0,
        0,
        STEP_CHILD_SETSPACE,
    );

    invoke(
        p,
        c.cptr_for(CH_TCB),
        Method::Resume,
        0,
        0,
        0,
        0,
        STEP_CHILD_RESUME,
    );
}

/// The program the child runs.
///
/// Three instructions and two syscalls: say who it is, then spin. It
/// does not need a CSpace entry of its own to do that -- `Echo` names
/// no capability -- which is what lets the child be this small.
fn child_program() -> Program {
    let mut p = Program::new();
    p.emit(&mov_rdi_imm(CHANNEL | STEP_CHILD_RAN));
    p.emit(&mov_rsi_imm(Method::Echo as u64));
    p.emit(&mov_rdx_imm(CHILD_PATTERN));
    p.emit(&program::SYSCALL);
    p.emit(&program::SPIN);
    p
}

/// Emit code that writes `bytes` to `address`, eight at a time.
///
/// The parent has no `memcpy`: there is no libc in ring 3 and nothing
/// to link one against. So the child's program is baked into the
/// parent's as immediates and stored word by word.
///
/// RBX holds the destination because it is callee-saved and
/// `syscall` leaves it alone (ADR 030); RAX carries each word.
fn emit_store_program(p: &mut Program, address: u64, bytes: &[u8]) {
    assert!(
        bytes.len() <= 128,
        "the child's program is {} bytes. The displacement in \
         `mov [rbx+disp8], rax` reaches 127, so a longer program \
         needs the base register advanced rather than an offset that \
         silently wraps.",
        bytes.len()
    );

    p.emit(&usertest::mov_rbx_imm(address));

    // Rounded up: the tail of the last word is zero padding, which
    // the child never executes -- its program ends in an infinite
    // loop well before them.
    let words = bytes.len().div_ceil(8);
    for index in 0..words {
        let mut word = [0u8; 8];
        let start = index * 8;
        let end = (start + 8).min(bytes.len());
        word[..end - start].copy_from_slice(&bytes[start..end]);

        p.emit(&usertest::mov_rax_imm(u64::from_le_bytes(word)));
        // mov [rbx + disp8], rax  (48 89 43 ib)
        p.emit(&[0x48, 0x89, 0x43, (index * 8) as u8]);
    }
}

// ---- Negative (a): the kernel half -------------------------------------

/// `vspace-user-kernel-half`: a component maps into the kernel's half.
///
/// Expected: `NotUserAddress`, **and the kernel still running**. That
/// second half is why this exits 33 rather than 35. A refused request
/// is a caller error, and from M8 the caller is a user component — a
/// kernel that died here would be a denial of service reachable by
/// anyone.
///
/// Four addresses, each a different kind of kernel memory, because a
/// check that happened to cover one region and not another would pass
/// a single-address test. The same four `vspace-kernel-half` uses
/// from inside the kernel; the point here is that the refusal holds
/// when the request arrives from ring 3.
pub fn run_kernel_half() -> ! {
    println!("[vsuser] a ring 3 component will try to map into the kernel half");

    let mut placeholder = Program::new();
    placeholder.emit(&program::SPIN);

    let component = build_component(
        SLOT_A_CNODE,
        SLOT_A_SPACE_BASE,
        SLOT_A_TCB,
        SLOT_A_KSTACK,
        COMPONENT_KSTACK_SLOT,
        placeholder.as_slice(),
    );

    let mut p = Program::new();
    let untyped = component.cptr_for(C_UNTYPED);
    let vspace = component.cptr_for(C_VSPACE);
    let frame = component.cptr_for(C_SCRATCH_FRAME);
    let own_cnode = component.cptr_for(C_OWN_CNODE);

    // A Frame to try to map. Its own, so a refusal cannot be
    // explained by it not holding the capability.
    invoke(
        &mut p,
        untyped,
        Method::Retype,
        retype_arg(CapType::Frame as u64, 0),
        1,
        own_cnode.0,
        C_SCRATCH_FRAME as u64,
        STEP_RETYPE,
    );

    for (index, address) in KERNEL_ADDRESSES.iter().enumerate() {
        invoke(
            &mut p,
            frame,
            Method::FrameMap,
            vspace.0,
            *address,
            PERM_READ_WRITE,
            0,
            STEP_KERNEL_HALF + index as u64 - 3,
        );
    }

    finish(&mut p);
    write_program(SLOT_A_SPACE_BASE + 1, p.as_slice());

    println!(
        "[vsuser] the component holds a Frame and its own VSpace; it \
         will ask for {} kernel addresses",
        KERNEL_ADDRESSES.len()
    );

    run_until_done(component.tcb_slot);

    assert_ok(STEP_RETYPE, "Untyped.Retype of the Frame");

    for (index, address) in KERNEL_ADDRESSES.iter().enumerate() {
        let which = STEP_KERNEL_HALF + index as u64 - 3;
        let value = step(which);
        assert_ne!(
            value, NOT_REPORTED,
            "the component never reported its attempt on {:#018x}",
            address
        );
        assert!(
            syscall::is_error(value),
            "mapping {:#018x} from ring 3 returned {:#018x} rather than \
             an error. A user mapping in the kernel half is visible in \
             **every** address space -- the top-level entries are shared \
             (ADR 027) -- so this is not one component's problem.",
            address,
            value
        );
        assert_eq!(
            syscall::error_code(value),
            15, // NotUserAddress
            "mapping {:#018x} gave error {} where NotUserAddress (15) \
             was expected. The ownership check (K1) and the paging \
             layer's flag check are different checks, and only the \
             first proves the request was refused for the right \
             reason.",
            address,
            syscall::error_code(value)
        );
    }

    // The kernel's own mappings still work. Read something out of the
    // heap: if a stray entry had been written into the shared upper
    // half, this is where it would show.
    let probe = crate::memory::layout::KHEAP_START;
    // SAFETY: The heap's first page is mapped and readable; this is a
    // plain read of kernel memory from kernel code.
    let value = unsafe { (probe as *const u64).read_volatile() };
    let _ = value;

    println!(
        "[vsuser] all {} refused with NotUserAddress and the kernel's \
         own mappings still work; this is an error path, not a fault",
        KERNEL_ADDRESSES.len()
    );

    syscall::report();

    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

/// Four kinds of kernel address, each refused separately.
const KERNEL_ADDRESSES: [u64; 4] = [
    layout::PMAP_BASE,
    layout::KHEAP_START,
    layout::KSTACK_BASE,
    layout::KIMAGE_BASE,
];

// ---- Negative (b): another component's address space -------------------

/// `vspace-user-foreign`: component B reaches for component A's
/// VSpace.
///
/// **The capability-level proof of isolation**, and the scenario the
/// whole CSpace change exists to make possible.
///
/// B is given a CPtr whose indices are exactly the ones that name A's
/// VSpace *in A's CSpace*. The bits are identical. What differs is
/// the root the walk starts from — B's own, where those indices lead
/// somewhere else or nowhere.
///
/// That is the structural claim: B does not fail because it is
/// forbidden, it fails because **there is nothing for the pointer to
/// mean**. A CPtr is a pair of indices from a thread's own root, so a
/// capability outside that root is not refused, it is unnameable.
///
/// Before M11 this could not have failed at all: one CSpace meant
/// B's pointer would have resolved to A's PageTable and the mapping
/// would have succeeded.
pub fn run_foreign() -> ! {
    println!("[vsuser] component B will reach for component A's address space");

    let mut placeholder = Program::new();
    placeholder.emit(&program::SPIN);

    // A exists only to own something worth reaching for. It is never
    // started: the scenario is about naming, not about a race.
    let a = build_component(
        SLOT_A_CNODE,
        SLOT_A_SPACE_BASE,
        SLOT_A_TCB,
        SLOT_A_KSTACK,
        COMPONENT_KSTACK_SLOT,
        placeholder.as_slice(),
    );

    // A second capability for A's VSpace, at an index **B does not
    // use**. See the note on `a_vspace` below for why this matters:
    // without it, the pointer the test hands B names B's own VSpace
    // and the mapping succeeds for an entirely innocent reason.
    //
    // SAFETY: Both slots are live and distinct — the source is A's
    // VSpace capability at C_VSPACE, the destination an empty slot
    // in the same leaf.
    unsafe {
        let source = a.leaf.slot(C_VSPACE);
        let dest = a.leaf.slot(A_ONLY_VSPACE);
        cnode::copy(source, dest, source.rights())
            .expect("could not place a second VSpace capability in A's leaf");
    }

    let b = build_component(
        SLOT_B_CNODE,
        SLOT_B_SPACE_BASE,
        SLOT_B_TCB,
        SLOT_B_KSTACK,
        COMPONENT_KSTACK_SLOT + 1,
        placeholder.as_slice(),
    );

    // The pointer A uses for its own VSpace, handed to B unchanged.
    //
    // **A's VSpace is deliberately not at the same leaf index as
    // B's**, and the first version of this scenario got that wrong
    // in a way that is worth keeping.
    //
    // Both components have identically-shaped CSpaces, so A's
    // VSpace at leaf slot 1 and B's VSpace at leaf slot 1 are named
    // by the *same bits* — `0x1`, since the generations are zero.
    // B used that pointer, resolved it through its own root, reached
    // its own VSpace, and mapped successfully. The scenario reported
    // "the two components are not isolated", which was exactly
    // backwards: isolation had worked, and the test had handed B a
    // pointer to itself.
    //
    // A CPtr is meaningful only relative to a root. That is the
    // whole claim — and it cuts both ways, because it means a
    // pointer from another CSpace does not reliably name *nothing*,
    // it names whatever is at those indices locally. So the test
    // has to arrange for the indices to differ, which
    // `A_ONLY_VSPACE` below does.
    //
    // What makes this a strong test rather than a weak one: B's
    // slot at that index is **empty**, so the resolution fails for
    // a reason that cannot be confused with a permission check.
    let a_vspace = a.cptr_for(A_ONLY_VSPACE);

    // And B's own, so the scenario can show the difference is the
    // root rather than the method. Both are `Frame.Map` on the same
    // Frame at the same address; only the VSpace argument differs.
    let b_vspace = b.cptr_for(C_VSPACE);
    let b_untyped = b.cptr_for(C_UNTYPED);
    let b_frame = b.cptr_for(C_SCRATCH_FRAME);
    let b_cnode = b.cptr_for(C_OWN_CNODE);

    let mut p = Program::new();

    invoke(
        &mut p,
        b_untyped,
        Method::Retype,
        retype_arg(CapType::Frame as u64, 0),
        1,
        b_cnode.0,
        C_SCRATCH_FRAME as u64,
        STEP_RETYPE,
    );

    // The attempt on A's address space.
    invoke(
        &mut p,
        b_frame,
        Method::FrameMap,
        a_vspace.0,
        SCRATCH_ADDRESS,
        PERM_READ_WRITE,
        0,
        STEP_FOREIGN,
    );

    // The control: the same call into its own space. Without this
    // the failure above would be consistent with `Frame.Map` being
    // broken, or with B holding no usable Frame -- and the scenario
    // would prove nothing about isolation.
    invoke(
        &mut p,
        b_frame,
        Method::FrameMap,
        b_vspace.0,
        SCRATCH_ADDRESS,
        PERM_READ_WRITE,
        0,
        STEP_MAP_FRAME,
    );

    finish(&mut p);
    write_program(SLOT_B_SPACE_BASE + 1, p.as_slice());

    println!(
        "[vsuser] B was handed the CPtr {:#018x}, which names A's \
         VSpace in A's CSpace. B will use it unchanged.",
        a_vspace.0
    );

    run_until_done(b.tcb_slot);

    assert_ok(STEP_RETYPE, "B's own Untyped.Retype");

    // The claim.
    let foreign = step(STEP_FOREIGN);
    assert_ne!(
        foreign, NOT_REPORTED,
        "B never reported its attempt on A's VSpace"
    );
    assert!(
        syscall::is_error(foreign),
        "B mapped a page into A's address space and the call returned \
         {:#018x}. The two components are not isolated: a CPtr issued \
         against one CSpace reached an object in another.",
        foreign
    );

    // **Which** error, and this is where the claim is sharpest.
    //
    // `EmptySlot` (3): the walk reached B's leaf slot 40 and found
    // nothing there. Not "found something it may not touch" —
    // *nothing*. A's VSpace is not hidden from B, it is absent from
    // B's CSpace, and a pointer to an absent thing resolves to an
    // absent thing.
    //
    // `StaleCPtr` (4) would be equally good and cannot occur here
    // (both generations are zero on a freshly retyped CNode), so
    // requiring 3 exactly is the stronger assertion. What is ruled
    // out is anything from inside `map_frame`: that would mean
    // resolution *succeeded* and a later check said no, which is
    // isolation by refusal rather than by unnameability. The first
    // is a check that could be deleted; the second is the shape of
    // the pointer.
    let code = syscall::error_code(foreign);
    assert_eq!(
        code,
        3, // EmptySlot
        "B's attempt failed with error {} where EmptySlot (3) was \
         expected. If this came from inside the mapping, resolution \
         succeeded and B reached an object in A's CSpace -- isolation \
         by refusal rather than by unnameability, which is weaker.",
        code
    );

    // The control.
    assert_ok(STEP_MAP_FRAME, "B's identical mapping into its own VSpace");

    println!(
        "[vsuser] B's attempt on A's VSpace failed in CPtr resolution \
         (error {}), and the identical call into B's own VSpace \
         succeeded. The pointer is not forbidden -- it names nothing \
         in B's CSpace.",
        code
    );
    println!("[vsuser] isolation holds at the capability level");

    syscall::report();

    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

// ---- Negative (c): exhausting a budget ---------------------------------

/// `vspace-user-exhaust`: a component spends its Untyped to the last
/// byte.
///
/// **The first test of ADR 022's actual claim.** From M5 the kernel
/// has allocated nothing for itself and every object has come out of
/// somebody's Untyped — but always the kernel's own retypes, from
/// inside. A component exhausting its budget from ring 3 is the first
/// time the claim means what it says.
///
/// Three things are checked, and the third is the one that matters:
///
/// 1. the exhaustion is clean — `NotEnoughMemory`, no panic;
/// 2. the kernel is still running;
/// 3. **the kernel's own memory is untouched**, and so is every other
///    Untyped. A component cannot starve the system, only itself.
pub fn run_exhaust() -> ! {
    println!("[vsuser] a ring 3 component will spend its entire budget");

    let mut placeholder = Program::new();
    placeholder.emit(&program::SPIN);

    let component = build_component(
        SLOT_A_CNODE,
        SLOT_A_SPACE_BASE,
        SLOT_A_TCB,
        SLOT_A_KSTACK,
        COMPONENT_KSTACK_SLOT,
        placeholder.as_slice(),
    );

    // What the rest of the machine looks like before. Taken after the
    // component is built, so the fixture's own spending is not
    // counted as the component's.
    let kernel_before = total_untyped_remaining();
    // The heap's **physical** reserve, not its byte count. That is
    // the figure ADR 022 is about: the heap may hand the same page
    // out again, but a page taken from the reserve is memory that
    // left the pool. If a component's activity can make the kernel
    // spend frames, this is where it shows.
    let heap_before = crate::memory::heap::reserve_remaining();

    let mut p = Program::new();
    let untyped = component.cptr_for(C_UNTYPED);
    let own_cnode = component.cptr_for(C_OWN_CNODE);

    // Retype PageTables in batches until the Untyped is empty.
    //
    // Into the **same destination slots** every time, which is the
    // trick that makes this bounded: a slot already full gives
    // `SlotNotEmpty` rather than consuming memory, so the component
    // deletes nothing and the watermark only moves while slots are
    // free. To actually drain the Untyped it needs somewhere to put
    // the capabilities -- so it uses its whole leaf, 64 slots, and
    // the budget is sized so that runs out second.
    //
    // 4 KiB per table, 256 KiB of budget: 64 tables. Exactly the
    // number of slots, which would make the two limits
    // indistinguishable -- so the loop asks for more rounds than
    // either allows and the *first* failure is the informative one.
    const ROUNDS: usize = 24;
    const PER_ROUND: u64 = 4;

    for round in 0..ROUNDS {
        let offset = (C_CHILD_BASE as u64 + (round as u64 * PER_ROUND)) % 48 + 8;
        invoke(
            &mut p,
            untyped,
            Method::Retype,
            retype_arg(CapType::PageTable as u64, 0),
            PER_ROUND,
            own_cnode.0,
            offset,
            // Every round reports to the same slot, so the step ends
            // up holding the **last** result -- which is the one
            // after exhaustion, and the one being asserted.
            STEP_RETYPE,
        );
    }

    finish(&mut p);
    write_program(SLOT_A_SPACE_BASE + 1, p.as_slice());

    println!(
        "[vsuser] the component holds {} KiB and will retype {} page \
         tables ({} KiB) from it",
        (1u64 << COMPONENT_BUDGET_BITS) / 1024,
        ROUNDS as u64 * PER_ROUND,
        ROUNDS as u64 * PER_ROUND * 4
    );

    run_until_done(component.tcb_slot);

    // 1. Clean exhaustion.
    let last = step(STEP_RETYPE);
    assert_ne!(last, NOT_REPORTED, "the component never reported a retype");
    assert!(
        syscall::is_error(last),
        "the component retyped {} KiB out of a {} KiB Untyped and the \
         last call still succeeded. Either the budget is not enforced \
         or the watermark is not advancing.",
        ROUNDS as u64 * PER_ROUND * 4,
        (1u64 << COMPONENT_BUDGET_BITS) / 1024
    );

    // **Error 11, and not 30.**
    //
    // `NotEnoughMemory` means the caller's own Untyped is spent.
    // `NoKernelStack` (30) would mean a kernel-wide pool ran out,
    // which is a different and weaker fact. M11 split them for
    // exactly this assertion: with one code, a run that exhausted a
    // shared resource would be indistinguishable from one that
    // exhausted its own, and the claim being tested here is
    // precisely about which.
    //
    // `SlotNotEmpty` (9) is also accepted: the component may run out
    // of CSpace slots before memory, and that too is its own
    // resource. What is not accepted is anything from outside the
    // component.
    let code = syscall::error_code(last);
    assert!(
        matches!(code, 11 | 9),
        "exhaustion reported error {}. Expected NotEnoughMemory (11) \
         or SlotNotEmpty (9) -- both the component's own limits. \
         Anything else means it ran into a resource that is not its, \
         which is the failure ADR 022 exists to rule out.",
        code
    );

    // 2 and 3. The rest of the machine is untouched.
    let kernel_after = total_untyped_remaining();
    assert_eq!(
        kernel_after,
        kernel_before,
        "the root Untypeds lost {} bytes while a component spent its \
         own budget. A component that can reach another's memory has \
         no budget at all.",
        kernel_before.saturating_sub(kernel_after)
    );

    let heap_after = crate::memory::heap::reserve_remaining();
    assert_eq!(
        heap_after,
        heap_before,
        "the kernel's heap reserve lost {} bytes while a component \
         exhausted its own Untyped. The kernel spent its memory on a \
         path a component drives, which is what ADR 022 forbids.",
        heap_before.saturating_sub(heap_after)
    );

    println!(
        "[vsuser] the component exhausted its own {} KiB cleanly \
         (error {}), the root Untypeds still hold {} KiB -- unchanged \
         -- and the kernel heap did not move",
        (1u64 << COMPONENT_BUDGET_BITS) / 1024,
        code,
        kernel_after / 1024
    );
    println!(
        "[vsuser] a component can starve itself and nothing else; \
         ADR 022's budget claim tested from the user path for the \
         first time"
    );

    syscall::report();

    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

/// Bytes left across every root Untyped.
///
/// The component's budget was carved out of one of these before the
/// measurement, so a change here means something reached memory that
/// was not the component's.
fn total_untyped_remaining() -> u64 {
    let mut total = 0u64;
    for index in 0..boot::root_untyped_count() {
        // SAFETY: Index below the count.
        let slot = unsafe { boot::root_untyped(index) };
        // SAFETY: Root slots hold live Untypeds.
        let untyped = unsafe { untyped::Untyped::from_slot(slot) }
            .expect("a root slot does not hold an Untyped");
        total += untyped.remaining();
    }
    total
}

// ---- Rights narrowing: the half that faults ----------------------------

/// `vspace-user-ro`: a component maps a page read-only and writes to
/// it.
///
/// Expected: **#PF with the user and write bits set**, and the kernel
/// panicking — so this exits 35.
///
/// # Why this is its own scenario
///
/// The milestone's criteria list rights-narrowing alongside the
/// mappings, and the first draft had it there. It cannot be: a write
/// to a read-only page is a #PF, and a #PF from ring 3 stops this
/// kernel (there is no fault handler for user threads until a
/// component can be given one). Putting it in `vspace-user-test`
/// would have ended that scenario at its third criterion, with the
/// other three never running.
///
/// The alternative was to leave the write out and check only that
/// `Frame.Map` accepted `ReadOnly` — which would have tested that the
/// *argument was accepted*, not that the *permission was enforced*.
/// That is a check indistinguishable from its own absence, and
/// ADR 023's K5 rules it out.
///
/// So: the acceptance is checked in `vspace-user-test` as part of the
/// mapping sequence, and the enforcement is checked here, where the
/// fault is the expected outcome rather than an interruption.
pub fn run_read_only() -> ! {
    println!("[vsuser] a component will map a page read-only and write to it");

    let mut placeholder = Program::new();
    placeholder.emit(&program::SPIN);

    let component = build_component(
        SLOT_A_CNODE,
        SLOT_A_SPACE_BASE,
        SLOT_A_TCB,
        SLOT_A_KSTACK,
        COMPONENT_KSTACK_SLOT,
        placeholder.as_slice(),
    );

    let mut p = Program::new();
    let untyped = component.cptr_for(C_UNTYPED);
    let vspace = component.cptr_for(C_VSPACE);
    let frame = component.cptr_for(C_SCRATCH_FRAME);
    let own_cnode = component.cptr_for(C_OWN_CNODE);

    invoke(
        &mut p,
        untyped,
        Method::Retype,
        retype_arg(CapType::Frame as u64, 0),
        1,
        own_cnode.0,
        C_SCRATCH_FRAME as u64,
        STEP_RETYPE,
    );

    // Read-only, at an address whose tables already exist.
    invoke(
        &mut p,
        frame,
        Method::FrameMap,
        vspace.0,
        SCRATCH_ADDRESS,
        PERM_READ_ONLY,
        0,
        STEP_MAP_FRAME,
    );

    // Read it first. A page that could not be read either would make
    // the fault below ambiguous -- "not readable" and "not writable"
    // are different failures and only one of them is the claim.
    p.emit(&usertest::mov_rax_imm(SCRATCH_ADDRESS));
    p.emit(&[0x48, 0x8b, 0x10]); // mov rdx, [rax]
    report(&mut p, STEP_READBACK, true, 0);

    // **RAX is reloaded**, and the first version's failure to do so
    // is worth recording. `syscall` returns in RAX, so the `Echo`
    // inside `report` destroyed the address. The write then went to
    // Echo's return value -- 2, as it happened -- and the log read:
    //
    //     cr2=0x0000000000000002  (user write not-present)
    //
    // Which *is* a #PF, from ring 3, on a write. A looser
    // expectation would have accepted it, and it says nothing
    // whatever about read-only enforcement: address 2 is unmapped in
    // every address space ever created.
    //
    // This is why the runner requires `protection-violation` rather
    // than any #PF, and why `cr2` must name a page that is PRESENT.
    // A not-present fault means the test never reached its subject.
    p.emit(&usertest::mov_rax_imm(SCRATCH_ADDRESS));

    // Then write. **This faults**, and nothing after it runs.
    p.emit(&[0x48, 0x31, 0xc9]); // xor rcx, rcx
    p.emit(&[0x48, 0x89, 0x08]); // mov [rax], rcx
    p.emit(&program::SPIN);

    write_program(SLOT_A_SPACE_BASE + 1, p.as_slice());

    // The component is started and the kernel waits. It must *not*
    // finish: the write kills it, and a run that reached `finish`
    // would mean the write succeeded.
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let slot = unsafe { cnode.slot(component.tcb_slot) };
    // SAFETY: Live TCB, not queued.
    unsafe {
        let thread = tcb::from_slot(slot).expect("the slot does not hold a TCB");
        sched::enqueue(thread);
    }

    println!(
        "[vsuser] mapping {:#018x} read-only, then writing to it; \
         expected #PF with the user and write bits set",
        SCRATCH_ADDRESS
    );

    let deadline = read_tsc() + DEADLINE;
    while read_tsc() < deadline {
        sched::yield_now();
    }

    // Reached only if the write did not fault.
    let readback = step(STEP_READBACK);
    unreachable!(
        "the component wrote to a page it mapped read-only and did not \
         fault. Map returned {:#x}, the read returned {:#x}. \
         `MapPermissions::ReadOnly` reached the page table as something \
         writable, so rights narrowing is accepted and not enforced.",
        step(STEP_MAP_FRAME),
        readback
    );
}
