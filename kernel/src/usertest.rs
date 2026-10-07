// SPDX-License-Identifier: BSD-2-Clause
//! Ring 3 acceptance tests.
//!
//! M7 proved the address space. This proves the privilege level, and
//! the claim that matters is narrower than it sounds: **code is
//! executing with CPL 3, and it cannot do the things CPL 0 can.**
//!
//! The positive checks show the mechanism works. The five negative
//! scenarios are where the value is — a kernel that lets user code
//! run but not be stopped has gained nothing over M7.
//!
//! # The user code
//!
//! Ring 3 needs instructions in a page marked USER, and this kernel
//! has no loader. The code is therefore written as bytes, assembled by
//! hand, and copied into a Frame. Short enough to read, which is the
//! only reason it is tolerable: anything longer would need a loader,
//! and a loader is M12.
//!
//! Each snippet is annotated with its disassembly. A wrong byte here
//! produces a #UD at an address with no symbol, so the comments are
//! load-bearing.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::cap::cnode::CNode;
use crate::cap::tcb;
use crate::cap::vspace::{self, MapPermissions};
use crate::cap::{boot, cptr, untyped, CapType};
use crate::cpu;
use crate::memory::{layout, paging, VirtAddr};
use crate::println;
use crate::sched;
use crate::syscall::{self, Method};

// ---- The user address space layout -------------------------------------

/// Where user code is mapped. 4 MiB: clear of the unmapped first
/// 64 KiB, and a round number in a fault report.
const USER_CODE_ADDRESS: u64 = 0x0040_0000;

/// Where the user stack's single page is mapped.
const USER_STACK_ADDRESS: u64 = 0x0050_0000;

/// Where the user data page is mapped — the one the isolation check
/// gives different contents in two address spaces.
const USER_DATA_ADDRESS: u64 = 0x0060_0000;

/// Initial user RSP: the top of the stack page, 16-byte aligned.
const USER_STACK_TOP: u64 = USER_STACK_ADDRESS + 4096 - 16;

/// Patterns the two isolated user threads find at `USER_DATA_ADDRESS`.
const PATTERN_A: u64 = 0xA5A5_A5A5_1111_1111;
const PATTERN_B: u64 = 0x5A5A_5A5A_2222_2222;

/// KSTACK slots for the user threads' *kernel* stacks.
///
/// Slots 0 and 1 are IST1 and the main stack; M7's tests use 6 to 9.
const UTHREAD_A_SLOT: u64 = 10;
const UTHREAD_B_SLOT: u64 = 11;

// ---- Hand-assembled user programs --------------------------------------

/// Report `CS` and a syscall result, then spin.
///
/// ```text
///   mov  rax, cs            48 8c c8   -- `mov r/m64, Sreg`
///   mov  rsi, 3             (method Echo)
///   ...
/// ```
///
/// Rather than one long program, the pieces are separate constants
/// assembled into the page by [`write_program`]. Each is short enough
/// to check by eye against the encoding.
mod program {
    /// `mov rax, cs` — read the code selector into RAX.
    ///
    /// `8c /r` is `mov r/m16, Sreg`; with REX.W it writes the full
    /// 64-bit register, zero-extended. CS's low two bits are the CPL,
    /// which is the whole point of the test.
    ///
    /// Reference: Intel SDM Vol. 2B, "MOV—Move to/from Segment
    /// Registers".
    pub const READ_CS: [u8; 3] = [0x48, 0x8c, 0xc8];

    /// `syscall` — enter the kernel.
    ///
    /// Reference: Intel SDM Vol. 2B, "SYSCALL".
    pub const SYSCALL: [u8; 2] = [0x0f, 0x05];

    /// `cli` — clear the interrupt flag. **Privileged**: #GP at CPL 3.
    pub const CLI: [u8; 1] = [0xfa];

    /// `hlt` — halt. **Privileged**: #GP at CPL 3.
    pub const HLT: [u8; 1] = [0xf4];

    /// `rdmsr` — read a model-specific register. **Privileged**.
    pub const RDMSR: [u8; 2] = [0x0f, 0x32];

    /// `lgdt [rsp]` — load the GDT register. **Privileged**.
    pub const LGDT_RSP: [u8; 4] = [0x0f, 0x01, 0x14, 0x24];

    /// `jmp $` — a two-byte infinite loop (`eb fe`).
    ///
    /// Where a user thread parks when it has nothing left to do. A
    /// `hlt` would be the obvious choice and is privileged, so this
    /// is the only way for ring 3 to idle without a syscall.
    pub const SPIN: [u8; 2] = [0xeb, 0xfe];
}

/// Encode `mov rax, imm64` (`48 b8 + 8 bytes`).
fn mov_rax_imm(value: u64) -> [u8; 10] {
    let mut bytes = [0u8; 10];
    bytes[0] = 0x48;
    bytes[1] = 0xb8;
    bytes[2..].copy_from_slice(&value.to_le_bytes());
    bytes
}

/// Encode `mov rdi, imm64` (`48 bf + 8 bytes`).
fn mov_rdi_imm(value: u64) -> [u8; 10] {
    let mut bytes = [0u8; 10];
    bytes[0] = 0x48;
    bytes[1] = 0xbf;
    bytes[2..].copy_from_slice(&value.to_le_bytes());
    bytes
}

/// Encode `mov rsi, imm64` (`48 be + 8 bytes`).
fn mov_rsi_imm(value: u64) -> [u8; 10] {
    let mut bytes = [0u8; 10];
    bytes[0] = 0x48;
    bytes[1] = 0xbe;
    bytes[2..].copy_from_slice(&value.to_le_bytes());
    bytes
}

/// Encode `mov rdx, imm64` (`48 ba + 8 bytes`).
fn mov_rdx_imm(value: u64) -> [u8; 10] {
    let mut bytes = [0u8; 10];
    bytes[0] = 0x48;
    bytes[1] = 0xba;
    bytes[2..].copy_from_slice(&value.to_le_bytes());
    bytes
}

/// A buffer a user program is assembled into before being copied to a
/// Frame.
struct Program {
    bytes: [u8; 512],
    length: usize,
}

impl Program {
    fn new() -> Program {
        Program {
            bytes: [0; 512],
            length: 0,
        }
    }

    fn emit(&mut self, code: &[u8]) -> &mut Program {
        assert!(
            self.length + code.len() <= self.bytes.len(),
            "the user program does not fit in its buffer; it is meant to \
             be short enough to read"
        );
        self.bytes[self.length..self.length + code.len()].copy_from_slice(code);
        self.length += code.len();
        self
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}

// ---- Shared state ------------------------------------------------------

/// What each user thread reported through `Echo`.
static ECHOED_BY_A: AtomicU64 = AtomicU64::new(0);
static ECHOED_BY_B: AtomicU64 = AtomicU64::new(0);

/// The CS value each user thread read.
static CS_SEEN_BY_A: AtomicU64 = AtomicU64::new(0);
static CS_SEEN_BY_B: AtomicU64 = AtomicU64::new(0);

/// Which kernel stack each thread's syscall arrived on.
static RSP_SEEN_BY_A: AtomicU64 = AtomicU64::new(0);
static RSP_SEEN_BY_B: AtomicU64 = AtomicU64::new(0);

/// Set once each thread has made its report.
static A_REPORTED: AtomicBool = AtomicBool::new(false);
static B_REPORTED: AtomicBool = AtomicBool::new(false);

fn read_tsc() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: RDTSC is unprivileged, reads no memory, writes EDX:EAX.
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

fn object_cnode() -> CNode {
    // SAFETY: boot::init put a CNode in this root slot.
    unsafe { boot::leaf_cnode(boot::ROOT_SLOT_OBJECTS) }
}

/// Find a root Untyped with at least `bytes` free.
///
/// Searched rather than hard-coded, for the reason M7 recorded: boot
/// carves the memory map into power-of-two blocks, so both the sizes
/// and their order depend on the machine.
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
        "no root Untyped has {} bytes free; the test cannot build a user \
         address space",
        bytes
    );
}

// ---- Building a user address space -------------------------------------

/// A user address space: a root, the tables, and three pages.
///
/// Only the root slot is read back after construction -- the others
/// are here because naming them is what makes `build_user_space`
/// readable, and because a scenario that needs to reach into a page
/// afterwards will want them.
struct UserSpace {
    root_slot: usize,
}

/// Bytes one user space costs: a root, nine tables (three levels for
/// each of three addresses, though some are shared), three frames.
const SPACE_BYTES: u64 = 16 * 4096;

/// Map one page into a VSpace, creating the tables it needs.
///
/// The kernel refuses to allocate intermediate tables (ADR 027), so
/// this is the sequence any caller has to perform. `AlreadyMapped` on
/// a table is not an error here: two of these addresses share a PDPT,
/// and discovering that by being told is simpler than computing it.
fn map_user_page(
    cnode: &CNode,
    untyped_index: usize,
    root_slot: usize,
    frame_slot: usize,
    address: u64,
    permissions: MapPermissions,
    table_base_slot: usize,
) {
    // SAFETY: Index below the count boot reported.
    let parent = unsafe { boot::root_untyped(untyped_index) };

    // SAFETY: Live Untyped and CNode; the destination slots are empty.
    unsafe { untyped::retype(parent, CapType::PageTable, 0, 3, cnode, table_base_slot) }
        .expect("could not retype the intermediate tables");

    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(root_slot) };
    let virt = VirtAddr::new(address);

    for (index, level) in [3u8, 2, 1].iter().enumerate() {
        // SAFETY: Index in range.
        let table = unsafe { cnode.slot(table_base_slot + index) };
        // SAFETY: Both slots hold live capabilities.
        match unsafe { vspace::map_table(root, table, virt, *level) } {
            Ok(()) => {}
            // A table at this level already covers the address. The
            // three test addresses are 1 MiB apart, so they share
            // everything above the PT -- expected, not a failure.
            Err(crate::cap::CapError::AlreadyMapped { .. }) => {}
            Err(error) => panic!("could not map the level-{} table: {}", level, error),
        }
    }

    // SAFETY: Index in range.
    let frame = unsafe { cnode.slot(frame_slot) };
    // SAFETY: Both slots hold live capabilities and the tables are in
    // place.
    unsafe { vspace::map_frame(root, frame, virt, permissions) }
        .unwrap_or_else(|error| panic!("could not map {:#018x}: {}", address, error));
}

/// Copy a program into a Frame, through the physical window.
///
/// Written through PMAP rather than through the user mapping, and for
/// two reasons. The user mapping is `ReadExecute` — not writable, by
/// ADR 032 — so writing through it would fault. And writing through
/// PMAP proves the mapping reaches *the frame the capability names*
/// rather than merely reaching something.
fn write_program(cnode: &CNode, frame_slot: usize, program: &[u8]) {
    // SAFETY: Index in range.
    let frame = unsafe { cnode.slot(frame_slot) };
    // SAFETY: The Frame came from a retype, so nothing else owns it,
    // and PMAP maps all RAM writably. The program fits in a page.
    unsafe {
        let ptr = crate::memory::phys_to_virt(frame.object().0).as_ptr::<u8>();
        core::ptr::copy_nonoverlapping(program.as_ptr(), ptr, program.len());
    }
}

/// Write a u64 into a Frame through the physical window.
fn write_data(cnode: &CNode, frame_slot: usize, value: u64) {
    // SAFETY: Index in range.
    let frame = unsafe { cnode.slot(frame_slot) };
    // SAFETY: As `write_program`.
    unsafe {
        let ptr = crate::memory::phys_to_virt(frame.object().0).as_ptr::<u64>();
        ptr.write_volatile(value);
    }
}

/// Build a complete user address space: code, stack and data.
///
/// `base_slot` is the first of 16 consecutive object-CNode slots this
/// space occupies.
fn build_user_space(base_slot: usize, program: &[u8], data: u64) -> UserSpace {
    let cnode = object_cnode();
    let untyped_index = untyped_with_room(SPACE_BYTES);
    // SAFETY: Index below the count boot reported.
    let parent = unsafe { boot::root_untyped(untyped_index) };

    // The root, then three frames.
    //
    // SAFETY: Live Untyped and CNode; the destination slots are empty.
    unsafe { untyped::retype(parent, CapType::PageTable, 0, 1, &cnode, base_slot) }
        .expect("could not retype the root page table");
    // SAFETY: As above.
    unsafe { untyped::retype(parent, CapType::Frame, 0, 3, &cnode, base_slot + 1) }
        .expect("could not retype the user frames");

    let root_slot = base_slot;
    let code_slot = base_slot + 1;
    let stack_slot = base_slot + 2;
    let data_slot = base_slot + 3;

    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(root_slot) };
    // SAFETY: Freshly retyped PageTable that nothing has mapped.
    unsafe { vspace::create_vspace(root) }.expect("could not create the VSpace");

    // The contents go in before the mappings, because the code page is
    // mapped ReadExecute and could not be written afterwards.
    write_program(&cnode, code_slot, program);
    write_data(&cnode, data_slot, data);

    // Code: readable and executable, **not writable** (ADR 032). The
    // type system is what enforces that -- there is no variant for
    // "writable and executable".
    map_user_page(
        &cnode,
        untyped_index,
        root_slot,
        code_slot,
        USER_CODE_ADDRESS,
        MapPermissions::ReadExecute,
        base_slot + 4,
    );
    map_user_page(
        &cnode,
        untyped_index,
        root_slot,
        stack_slot,
        USER_STACK_ADDRESS,
        MapPermissions::ReadWrite,
        base_slot + 7,
    );
    map_user_page(
        &cnode,
        untyped_index,
        root_slot,
        data_slot,
        USER_DATA_ADDRESS,
        MapPermissions::ReadWrite,
        base_slot + 10,
    );

    UserSpace { root_slot }
}

/// Create a thread that starts at CPL 3 in `space`.
fn build_user_thread(
    tcb_slot_index: usize,
    kernel_stack_slot: u64,
    space: &UserSpace,
    argument: u64,
) -> usize {
    let cnode = object_cnode();
    let thread_bytes = 2048 + 4 * 4096;
    let untyped_index = untyped_with_room(thread_bytes);
    // SAFETY: Index below the count boot reported.
    let parent = unsafe { boot::root_untyped(untyped_index) };

    // SAFETY: Live Untyped and CNode; the destination slot is empty.
    unsafe { untyped::retype(parent, CapType::Tcb, 0, 1, &cnode, tcb_slot_index) }
        .expect("could not retype a TCB");

    let (_, bottom, top) = layout::kstack_slot(kernel_stack_slot);

    // The kernel stack. In the kernel half, so it is reachable from
    // every address space -- which is why the kernel half is shared
    // (ADR 027). A thread whose kernel stack vanished when CR3 changed
    // could not take an interrupt, let alone a syscall.
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
    .expect("could not retype the kernel stack frames");

    for index in 0..frames_needed {
        // SAFETY: Index in range.
        let frame_slot = unsafe { cnode.slot(stack_base_slot + index) };
        let frame = crate::memory::PhysFrame::containing(frame_slot.object().0);
        let page = VirtAddr::new(bottom.0 + (index as u64) * crate::memory::FRAME_SIZE);

        // SAFETY: The kernel's tables are live, the address is inside
        // this thread's own unmapped KSTACK slot, and the frame came
        // from a retype so nothing else owns it.
        unsafe {
            let table = crate::memory::vm::kernel_table();
            let mut source = crate::memory::heap::reserve_frames();
            let flags =
                paging::PageFlags::READ | paging::PageFlags::WRITE | paging::PageFlags::GLOBAL;
            let tlb = paging::map(table, page, frame, flags, &mut source)
                .expect("could not map a kernel stack page");
            tlb.flush();
        }
    }

    // SAFETY: Index in range.
    let tcb_slot = unsafe { cnode.slot(tcb_slot_index) };
    // SAFETY: The slot holds a live TCB nothing else borrows.
    let tcb = unsafe { tcb::from_slot(tcb_slot) }.expect("the slot does not hold a TCB");

    // SAFETY: The kernel stack is mapped and owned by nothing else;
    // the user entry and stack are mapped in the VSpace bound below.
    unsafe {
        tcb.configure_user(
            USER_CODE_ADDRESS,
            USER_STACK_TOP,
            argument,
            top,
            bottom,
            kernel_stack_slot,
        );
    }

    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(space.root_slot) };
    // SAFETY: Both slots hold live capabilities; the thread is not
    // running.
    unsafe { tcb::set_space(tcb_slot, Some(root)) }.expect("SetSpace failed");

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

// ---- The reporting program ---------------------------------------------

/// Build the program each isolated user thread runs.
///
/// In order:
///
/// 1. read CS, report it through `Echo`;
/// 2. read the data page, report that;
/// 3. write a pattern into XMM0 and loop, so the FPU check has
///    something to preserve.
///
/// `which` selects the report channel, so the two threads' results can
/// be told apart.
fn reporting_program(which: u64, own_tcb_cptr: u64) -> Program {
    let mut program = Program::new();

    // --- a real capability invocation ---
    //
    // `ReadRegisters` on this thread's own TCB. It is here because
    // everything else the program does goes through `Echo`, and Echo
    // resolves no capability at all -- so without this, "ring 3 can
    // use the capability path" would be a claim nothing tested. The
    // first run of this scenario reported `last capability
    // invocation: <none yet>`, which is exactly that gap showing.
    //
    // The result is reported back, so a refusal is visible rather
    // than silent: an error arrives with the top bit set and
    // `check_user_code_runs` rejects it.
    program.emit(&mov_rdi_imm(own_tcb_cptr));
    program.emit(&mov_rsi_imm(Method::ReadRegisters as u64));
    program.emit(&program::SYSCALL);
    program.emit(&[0x48, 0x89, 0xc2]); // mov rdx, rax
    program.emit(&mov_rdi_imm(which | REPORT_INVOKE_FLAG));
    program.emit(&mov_rsi_imm(Method::Echo as u64));
    program.emit(&program::SYSCALL);

    // --- report CS ---
    //
    // RAX gets CS, then it moves to RDX, the Echo argument. RDI (the
    // CPtr) is ignored by Echo, so it carries `which` instead -- the
    // kernel reads it out of the frame to know who is calling.
    program.emit(&program::READ_CS);
    // mov rdx, rax  (48 89 c2)
    program.emit(&[0x48, 0x89, 0xc2]);
    program.emit(&mov_rdi_imm(which));
    program.emit(&mov_rsi_imm(Method::Echo as u64));
    program.emit(&program::SYSCALL);

    // --- report the data page ---
    //
    // mov rdx, [USER_DATA_ADDRESS] -- through RAX, since there is no
    // short encoding for a 64-bit absolute load into RDX.
    program.emit(&mov_rax_imm(USER_DATA_ADDRESS));
    // mov rdx, [rax]  (48 8b 10)
    program.emit(&[0x48, 0x8b, 0x10]);
    program.emit(&mov_rdi_imm(which | REPORT_DATA_FLAG));
    program.emit(&mov_rsi_imm(Method::Echo as u64));
    program.emit(&program::SYSCALL);

    // --- claim an XMM register and spin ---
    //
    // movq xmm0, rax  (66 48 0f 6e c0), with RAX holding a per-thread
    // pattern. The thread then loops forever; the kernel preempts it,
    // runs the other one, and comes back. If the FPU state were not
    // switched, the value would be the other thread's.
    program.emit(&mov_rax_imm(xmm_pattern(which)));
    program.emit(&[0x66, 0x48, 0x0f, 0x6e, 0xc0]);

    // The loop body: read XMM0 back and report it, forever. Reporting
    // repeatedly is what makes the check meaningful -- the value must
    // survive *every* round trip, not just the first.
    //
    // movq rdx, xmm0  (66 48 0f 7e c2)
    program.emit(&[0x66, 0x48, 0x0f, 0x7e, 0xc2]);
    program.emit(&mov_rdi_imm(which | REPORT_XMM_FLAG));
    program.emit(&mov_rsi_imm(Method::Echo as u64));
    program.emit(&program::SYSCALL);
    // jmp back to the `movq rdx, xmm0` above. The displacement is
    // computed rather than written as a literal, because every edit to
    // the three instructions above would change it.
    let loop_start = program.length - (5 + 10 + 10 + 2);
    let displacement = (loop_start as i64) - (program.length as i64 + 2);
    let displacement = i8::try_from(displacement).expect(
        "the user program's loop body grew past a short jump's reach; it \
         is meant to stay small enough to read",
    );
    program.emit(&[0xeb, displacement as u8]);

    program
}

/// Marks an `Echo` report as carrying the data page's contents.
const REPORT_DATA_FLAG: u64 = 1 << 8;
/// Marks an `Echo` report as carrying XMM0.
const REPORT_XMM_FLAG: u64 = 1 << 9;
/// Marks an `Echo` report as carrying a `ReadRegisters` result.
const REPORT_INVOKE_FLAG: u64 = 1 << 10;

/// The XMM pattern thread `which` writes.
fn xmm_pattern(which: u64) -> u64 {
    if which == 0 {
        0xDEAD_BEEF_0000_1111
    } else {
        0xFEED_FACE_0000_2222
    }
}

// ---- Collecting reports from the syscall path --------------------------

/// Record what a user thread reported.
///
/// Called by the syscall dispatcher when it sees an `Echo` whose RDI
/// matches one of the test's channels. Keeping this out of
/// `syscall.rs` matters: the kernel's syscall path should not know
/// what a test wants, and `Echo` is defined to touch nothing.
///
/// The kernel RSP is recorded here too, because this is the one place
/// that runs on the thread's kernel stack with the thread's identity
/// known.
pub fn record_report(channel: u64, value: u64, kernel_rsp: u64) {
    let which = channel & 0xFF;
    let store = |slot: &AtomicU64| slot.store(value, Ordering::Release);

    if channel & REPORT_XMM_FLAG != 0 {
        if which == 0 {
            XMM_SEEN_BY_A.store(value, Ordering::Release);
        } else {
            XMM_SEEN_BY_B.store(value, Ordering::Release);
        }
        XMM_REPORTS.fetch_add(1, Ordering::AcqRel);
    } else if channel & REPORT_DATA_FLAG != 0 {
        if which == 0 {
            store(&ECHOED_BY_A);
            A_REPORTED.store(true, Ordering::Release);
        } else {
            store(&ECHOED_BY_B);
            B_REPORTED.store(true, Ordering::Release);
        }
    } else if channel & REPORT_INVOKE_FLAG != 0 {
        if which == 0 {
            INVOKE_RESULT_A.store(value, Ordering::Release);
        } else {
            INVOKE_RESULT_B.store(value, Ordering::Release);
        }
    } else if which == 0 {
        CS_SEEN_BY_A.store(value, Ordering::Release);
        RSP_SEEN_BY_A.store(kernel_rsp, Ordering::Release);
    } else {
        CS_SEEN_BY_B.store(value, Ordering::Release);
        RSP_SEEN_BY_B.store(kernel_rsp, Ordering::Release);
    }
}

/// What each thread's `ReadRegisters` invocation returned.
///
/// Reported so that a refusal is visible. An invocation that failed
/// would come back with the error flag set, and a test that only
/// counted syscalls would not notice.
static INVOKE_RESULT_A: AtomicU64 = AtomicU64::new(0);
static INVOKE_RESULT_B: AtomicU64 = AtomicU64::new(0);

/// XMM0 as last reported by each thread.
static XMM_SEEN_BY_A: AtomicU64 = AtomicU64::new(0);
static XMM_SEEN_BY_B: AtomicU64 = AtomicU64::new(0);
/// How many XMM reports have arrived, so the test can wait for enough
/// round trips to have happened.
static XMM_REPORTS: AtomicU64 = AtomicU64::new(0);

// ---- The suite ---------------------------------------------------------

/// Run the ring 3 acceptance tests.
pub fn run() -> ! {
    println!("[user] starting ring 3 tests");

    check_segments_and_msrs();
    check_fpu_configured();
    let space_a = check_build_user_space();
    check_wx_is_unrepresentable(&space_a);
    check_user_code_runs();
    check_kernel_stacks_differ();
    check_isolation_in_ring3();
    check_xmm_preserved();
    check_preemption_in_ring3();
    measure_syscall_cost();

    vspace::report();
    sched::report();
    syscall::report();
    println!("[user] all ring 3 tests passed");

    // The banner the runner requires of every successful scenario.
    // Printed here rather than in `kmain`, because this function
    // exits QEMU and never returns there -- the same shape every
    // other `run()` has.
    println!("[boot] M8 checks passed");
    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        crate::qemu::EXIT_SUCCESS,
        crate::qemu::HOST_STATUS_SUCCESS
    );

    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

/// 1. The GDT and the syscall MSRs say what ADR 029 says they must.
fn check_segments_and_msrs() {
    let features = cpu::features();
    assert!(
        features.syscall_enabled,
        "EFER.SCE is clear, so `syscall` is #UD and no user thread could \
         ever reach the kernel"
    );

    // The selectors must carry RPL 3. A user descriptor loaded with
    // RPL 0 fails its own DPL check, and the resulting #GP names the
    // descriptor rather than the selector -- which sends the reader
    // looking in the wrong place.
    assert_eq!(
        crate::gdt::USER_CODE_SELECTOR & 3,
        3,
        "the user code selector does not have RPL 3"
    );
    assert_eq!(
        crate::gdt::USER_DATA_SELECTOR & 3,
        3,
        "the user data selector does not have RPL 3"
    );

    // And the arithmetic `sysret` performs, checked here as well as at
    // build time. The const assertions in `gdt` cover the source; this
    // covers the values that actually reached the MSR.
    assert_eq!(
        (crate::gdt::STAR_USER_BASE & !3) + 16,
        crate::gdt::USER_CODE_SELECTOR & !3,
        "sysret would load CS from the wrong slot"
    );
    assert_eq!(
        (crate::gdt::STAR_USER_BASE & !3) + 8,
        crate::gdt::USER_DATA_SELECTOR & !3,
        "sysret would load SS from the wrong slot"
    );

    println!(
        "[user] selectors: user cs {:#06x}, user ss {:#06x}, star base \
         {:#06x}; sysret's +8/+16 arithmetic lands on both (ADR 029)",
        crate::gdt::USER_CODE_SELECTOR,
        crate::gdt::USER_DATA_SELECTOR,
        crate::gdt::STAR_USER_BASE
    );

    check_fmask_masks_ac();
}

/// FMASK really clears AC, not merely claims to.
///
/// # Why this check exists
///
/// A mutation test put it here. Removing AC from `FMASK_BITS` was
/// caught -- but only by the *banner line*, because the message names
/// the bits it masks. Editing the message too would have made the
/// mutation invisible.
///
/// A guard whose only evidence is a string the same commit could have
/// changed is a guard indistinguishable from its own absence, which
/// ADR 023's K5 forbids. So the register is read instead.
///
/// # What is being prevented
///
/// `stac` is not privileged: it needs CR4.SMAP and nothing else. User
/// code can set AC and then `syscall`. If AC survived into the kernel,
/// `UserAccess::new` would see "already set", do nothing, and leave
/// SMAP open for the whole call -- with its `Drop` also doing nothing,
/// so no fault, no log, and no protection (ADR 030).
fn check_fmask_masks_ac() {
    // Read FMASK back from the MSR rather than from the constant. The
    // constant is what the code *meant*; the MSR is what the CPU will
    // act on, and `enable_syscall` is where the two could diverge.
    let fmask = cpu::fmask();

    const AC: u64 = 1 << 18;
    const IF: u64 = 1 << 9;
    const DF: u64 = 1 << 10;
    const TF: u64 = 1 << 8;

    for (bit, name, why) in [
        (
            AC,
            "AC",
            "user code can run `stac` -- it is unprivileged -- and hand \
             the kernel an open SMAP window that the UserAccess guard \
             would read as somebody else's",
        ),
        (
            IF,
            "IF",
            "an interrupt could arrive between the syscall and the stack \
             switch, while RSP still points at user memory",
        ),
        (
            DF,
            "DF",
            "the kernel's first string operation would run backwards",
        ),
        (
            TF,
            "TF",
            "the kernel's first instruction would take #DB and \
             single-stepping would walk through kernel code",
        ),
    ] {
        assert!(
            fmask & bit != 0,
            "IA32_FMASK does not clear {} on syscall entry: {}. The MSR \
             holds {:#018x}.",
            name,
            why,
            fmask
        );
    }

    println!(
        "[user] fmask: read back as {:#018x} from the MSR -- IF, DF, TF \
         and AC all cleared on entry, checked against the register \
         rather than against the banner (ADR 030)",
        fmask
    );
}

/// 2. The FPU is configured for eager switching (ADR 031).
fn check_fpu_configured() {
    let features = cpu::features();
    assert!(
        features.fxsr_enabled,
        "CR4.OSFXSR is clear; SSE is #UD and fxsave does not exist"
    );
    assert!(
        features.xmmexcpt_enabled,
        "CR4.OSXMMEXCPT is clear; an SSE numeric error would arrive as #UD"
    );

    // A new thread's saved MXCSR must be the architectural default,
    // not zero. Zero is what retype leaves behind, and it unmasks
    // every SSE exception -- so a thread restored from it raises #XM
    // on its first floating-point operation. "Initialised" and
    // "zeroed" are indistinguishable until then, which is why the
    // value is read back rather than assumed (ADR 031).
    // SAFETY: The boot TCB is live and nothing else borrows it here.
    let mxcsr = unsafe { tcb_at(boot::BOOT_TCB_SLOT) }.saved_mxcsr();
    assert_eq!(
        mxcsr, 0x1F80,
        "a thread's saved MXCSR is {:#06x} rather than the architectural \
         default 0x1f80. Zero means every SSE exception is unmasked, and \
         the first floating-point operation after a restore would raise \
         #XM.",
        mxcsr
    );

    println!(
        "[user] fpu: osfxsr and osxmmexcpt set, cr0.ts clear, saved mxcsr \
         {:#06x} -- every switch saves, none is lazy (ADR 031)",
        mxcsr
    );
}

/// 3. A user address space can be built, with the code page executable
///    and not writable.
fn check_build_user_space() -> UserSpace {
    // A placeholder program, rewritten below. The real one needs a
    // CPtr to the thread's own TCB, and that capability does not
    // exist until `build_user_thread` retypes it -- a CPtr carries
    // the slot's generation, so one computed against an empty slot
    // would be stale the moment the slot was filled (ADR 018).
    let placeholder = reporting_program(0, 0);
    let space = build_user_space(64, placeholder.as_slice(), PATTERN_A);

    let cnode = object_cnode();
    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(space.root_slot) };

    // SAFETY: The slot holds a live PageTable.
    let (code_phys, code_flags) =
        unsafe { vspace::translate(root, VirtAddr::new(USER_CODE_ADDRESS)) }
            .expect("the code mapping does not resolve");

    assert!(
        code_flags.contains(paging::PageFlags::USER),
        "the code page is not USER, so ring 3 cannot fetch from it: {:?}",
        code_flags
    );
    assert!(
        code_flags.contains(paging::PageFlags::EXEC),
        "the code page is not executable; the first user instruction \
         would be a #PF with the instruction-fetch bit set: {:?}",
        code_flags
    );
    assert!(
        !code_flags.contains(paging::PageFlags::WRITE),
        "the code page is writable *and* executable, which ADR 032 makes \
         unrepresentable -- so something built these flags by hand: {:?}",
        code_flags
    );

    // SAFETY: As above.
    let (_, stack_flags) = unsafe { vspace::translate(root, VirtAddr::new(USER_STACK_ADDRESS)) }
        .expect("the stack mapping does not resolve");
    assert!(
        stack_flags.contains(paging::PageFlags::WRITE),
        "the user stack is not writable: {:?}",
        stack_flags
    );
    assert!(
        !stack_flags.contains(paging::PageFlags::EXEC),
        "the user stack is executable; W^X is not holding: {:?}",
        stack_flags
    );

    println!(
        "[user] address space: code at {:#018x} -> {} {}, stack {}, data \
         {}",
        USER_CODE_ADDRESS,
        code_phys,
        MapPermissions::ReadExecute.name(),
        MapPermissions::ReadWrite.name(),
        MapPermissions::ReadWrite.name()
    );

    space
}

/// 4. W^X is enforced by the type, not by a check.
///
/// There is nothing to call here that would fail: the point of ADR
/// 032 is that "writable and executable" cannot be *written down*.
/// What can be checked is that the two permissions that do exist
/// produce disjoint flags, and that the capability layer refuses to
/// widen them.
fn check_wx_is_unrepresentable(space: &UserSpace) {
    let cnode = object_cnode();
    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(space.root_slot) };

    // SAFETY: The slot holds a live PageTable.
    let (_, code) = unsafe { vspace::translate(root, VirtAddr::new(USER_CODE_ADDRESS)) }.unwrap();
    // SAFETY: As above.
    let (_, stack) = unsafe { vspace::translate(root, VirtAddr::new(USER_STACK_ADDRESS)) }.unwrap();

    for (what, flags) in [("code", code), ("stack", stack)] {
        assert!(
            !(flags.contains(paging::PageFlags::WRITE) && flags.contains(paging::PageFlags::EXEC)),
            "the {} page is both writable and executable: {:?}",
            what,
            flags
        );
    }

    println!(
        "[user] w^x: no page is both writable and executable. The rule is \
         a missing enum variant rather than a runtime check -- there is \
         no MapPermissions::ReadWriteExecute to pass (ADR 032)"
    );
}

/// 5. **The check that cannot be faked.** User code runs at CPL 3.
fn check_user_code_runs() {
    // The space `check_build_user_space` built, at the slots
    // `build_user_space(64, ..)` assigns.
    let space = UserSpace { root_slot: 64 };

    let tcb = build_user_thread(128, UTHREAD_A_SLOT, &space, 0);

    // Now the TCB exists, so a CPtr naming it can be built with the
    // generation the slot actually has. `check_build_user_space` put
    // a placeholder in the code page; this replaces it.
    //
    // Written through PMAP rather than through the user mapping: the
    // code page is ReadExecute and not writable (ADR 032), which is
    // the point of mapping it that way.
    let cspace = boot::init_cspace();
    // SAFETY: Valid indices; slot 128 now holds thread A's TCB.
    let tcb_cptr = unsafe { cptr::address_of(&cspace, boot::ROOT_SLOT_OBJECTS, 128) }
        .expect("could not address thread A's TCB slot");
    let program = reporting_program(0, tcb_cptr.0);
    write_program(&object_cnode(), 65, program.as_slice());

    let syscalls_before = syscall::syscall_count();

    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(tcb_at(tcb));
    }

    // Bounded. A thread that never reports must fail the test rather
    // than hang it.
    let deadline = read_tsc() + 20_000_000_000;
    while !A_REPORTED.load(Ordering::Acquire) && read_tsc() < deadline {
        sched::yield_now();
    }

    assert!(
        A_REPORTED.load(Ordering::Acquire),
        "the user thread never reached the kernel. Either it never ran, \
         or its syscall never arrived -- check EFER.SCE and LSTAR."
    );

    let cs = CS_SEEN_BY_A.load(Ordering::Acquire);
    assert_eq!(
        cs & 3,
        3,
        "the thread read CS = {:#06x}, whose CPL is {}. It is not running \
         in ring 3, so every protection this milestone adds is absent.",
        cs,
        cs & 3
    );
    assert_eq!(
        cs,
        crate::gdt::USER_CODE_SELECTOR as u64,
        "CS is {:#06x} rather than the user code selector {:#06x}",
        cs,
        crate::gdt::USER_CODE_SELECTOR
    );

    let data = ECHOED_BY_A.load(Ordering::Acquire);
    assert_eq!(
        data, PATTERN_A,
        "the thread read {:#018x} from its data page, not {:#018x}",
        data, PATTERN_A
    );

    // The TCB agrees that this is a user thread. Checked because
    // `configure_user` sets the flag and nothing else reads it: a
    // field written and never read is a field that can be wrong
    // without anyone noticing.
    let records_user_mode = {
        // SAFETY: Live TCB; no other borrow held.
        unsafe { tcb_at(tcb) }.is_user_mode()
    };
    assert!(
        records_user_mode,
        "the TCB does not record this thread as running at CPL 3, even \
         though its CS says it is"
    );

    // The real capability invocation came back, and came back
    // successfully. Without this the whole "ring 3 reaches the
    // capability system" claim would rest on `Echo`, which resolves
    // no capability at all -- and the first run of this scenario
    // reported exactly that: `last capability invocation: <none
    // yet>`.
    let invoked = INVOKE_RESULT_A.load(Ordering::Acquire);
    assert!(
        !syscall::is_error(invoked),
        "the user thread's ReadRegisters invocation returned error {} \
         rather than a result. A user component cannot reach its own \
         TCB, so nothing here exercises the capability path.",
        syscall::error_code(invoked)
    );

    println!(
        "[user] capability path: ReadRegisters from ring 3 returned {} \
         (the thread's own switch count), through a CPtr the kernel \
         resolved and validated",
        invoked
    );

    let syscalls = syscall::syscall_count() - syscalls_before;
    assert!(
        syscalls >= 2,
        "only {} syscalls arrived; the thread makes at least two before \
         it reports",
        syscalls
    );

    assert!(
        syscall::sysret_count() > 0,
        "no syscall returned through sysret, so the fast path is never \
         exercised and ADR 029's canonical-RCX check guards nothing"
    );

    println!(
        "[user] ring 3: cs = {:#06x} (cpl {}), data page reads {:#018x}, \
         {} syscalls completed",
        cs,
        cs & 3,
        data,
        syscalls
    );

    // SAFETY: Live TCB.
    unsafe {
        sched::suspend(tcb_at(tcb));
    }
}

/// 6. Two user threads enter the kernel on **different** stacks.
fn check_kernel_stacks_differ() {
    // Thread A has already reported. Build B and let it report too.
    // Same two-step as thread A: a placeholder, then the real program
    // once the TCB exists and a CPtr to it can carry the right
    // generation.
    let placeholder = reporting_program(1, 0);
    let space_b = build_user_space(96, placeholder.as_slice(), PATTERN_B);
    let tcb_b = build_user_thread(160, UTHREAD_B_SLOT, &space_b, 1);

    let cspace = boot::init_cspace();
    // SAFETY: Valid indices; slot 160 now holds thread B's TCB.
    let tcb_cptr = unsafe { cptr::address_of(&cspace, boot::ROOT_SLOT_OBJECTS, 160) }
        .expect("could not address thread B's TCB slot");
    let program = reporting_program(1, tcb_cptr.0);
    write_program(&object_cnode(), 97, program.as_slice());

    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(tcb_at(tcb_b));
    }

    let deadline = read_tsc() + 20_000_000_000;
    while !B_REPORTED.load(Ordering::Acquire) && read_tsc() < deadline {
        sched::yield_now();
    }
    assert!(
        B_REPORTED.load(Ordering::Acquire),
        "the second user thread never reported"
    );

    let rsp_a = RSP_SEEN_BY_A.load(Ordering::Acquire);
    let rsp_b = RSP_SEEN_BY_B.load(Ordering::Acquire);

    assert!(rsp_a != 0 && rsp_b != 0, "a thread reported no kernel RSP");
    assert_ne!(
        rsp_a, rsp_b,
        "both user threads entered the kernel on the same stack \
         ({:#018x}). The second to enter would overwrite the first's \
         frame, and the symptom is random corruption far from its cause \
         (ADR 029).",
        rsp_a
    );

    // And each is inside its *own* thread's stack, not merely
    // different from the other. Two wrong-but-different addresses
    // would pass the check above.
    // SAFETY: Live TCBs; no other borrow held.
    let (owns_a, owns_b) = unsafe {
        (
            tcb_at(128).owns_stack_address(rsp_a),
            tcb_at(160).owns_stack_address(rsp_b),
        )
    };
    assert!(
        owns_a,
        "thread A entered the kernel at {:#018x}, which is not inside its \
         own kernel stack",
        rsp_a
    );
    assert!(
        owns_b,
        "thread B entered the kernel at {:#018x}, which is not inside its \
         own kernel stack",
        rsp_b
    );

    assert!(sched::rsp0_update_count() > 0, "TSS.RSP0 was never updated");

    println!(
        "[user] kernel stacks: thread A enters at {:#018x}, thread B at \
         {:#018x}; each inside its own stack, {} rsp0 updates",
        rsp_a,
        rsp_b,
        sched::rsp0_update_count()
    );
}

/// 7. M7's isolation, repeated in ring 3.
fn check_isolation_in_ring3() {
    let seen_a = ECHOED_BY_A.load(Ordering::Acquire);
    let seen_b = ECHOED_BY_B.load(Ordering::Acquire);

    assert_eq!(seen_a, PATTERN_A, "thread A read the wrong pattern");
    assert_eq!(seen_b, PATTERN_B, "thread B read the wrong pattern");
    assert_ne!(
        seen_a, seen_b,
        "both user threads read the same value from {:#018x}; they are \
         sharing an address space and nothing here is isolated",
        USER_DATA_ADDRESS
    );

    assert!(
        sched::cr3_switch_count() > 0,
        "CR3 was never reloaded even though the threads have different \
         address spaces"
    );

    println!(
        "[user] isolation: at {:#018x}, thread A reads {:#018x} and \
         thread B reads {:#018x} -- M7's proof, now at CPL 3",
        USER_DATA_ADDRESS, seen_a, seen_b
    );
}

/// 8. An XMM register survives preemption, and every switch saved it.
fn check_xmm_preserved() {
    // Wait for *switches*, not for reports.
    //
    // The first version waited for XMM reports and failed with "only
    // 8 fxsave operations": a user thread makes thousands of syscalls
    // between one preemption and the next, so reports accumulate far
    // faster than switches do. The question this check asks is about
    // switching, so switching is what it has to wait for.
    let switches_before = sched::switch_count();
    let reports_before = XMM_REPORTS.load(Ordering::Acquire);
    let target_switches = switches_before + 200;

    let deadline = read_tsc() + 30_000_000_000;
    while sched::switch_count() < target_switches && read_tsc() < deadline {
        // Spin rather than yield: yielding would switch *this* thread
        // and inflate the count without the user threads having run.
        core::hint::spin_loop();
    }

    let switched = sched::switch_count() - switches_before;
    assert!(
        switched >= 200,
        "only {} context switches happened while waiting; the equality \
         below would be comparing two small numbers and proving little",
        switched
    );
    assert!(
        XMM_REPORTS.load(Ordering::Acquire) > reports_before,
        "the user threads stopped reporting XMM0 while they were being \
         switched between"
    );

    let xmm_a = XMM_SEEN_BY_A.load(Ordering::Acquire);
    let xmm_b = XMM_SEEN_BY_B.load(Ordering::Acquire);

    assert_eq!(
        xmm_a,
        xmm_pattern(0),
        "thread A's XMM0 reads {:#018x} rather than the {:#018x} it \
         wrote. The FPU state is not being switched, and the value it \
         sees is another thread's.",
        xmm_a,
        xmm_pattern(0)
    );
    assert_eq!(
        xmm_b,
        xmm_pattern(1),
        "thread B's XMM0 reads {:#018x} rather than {:#018x}",
        xmm_b,
        xmm_pattern(1)
    );

    // The part that rules out lazy switching. Preservation alone would
    // pass on a correct lazy implementation too -- the question is
    // *when* the save happens, and the only visible answer is how many
    // times (ADR 031).
    let switches = sched::switch_count();
    let saves = sched::fxsave_count();
    let restores = sched::fxrstor_count();

    // `saves <= switches` and within one: the switch count is read
    // last, so at most one switch can have happened since the saves
    // were read. Exact equality would be a race, and a flaky test is
    // worse than a loose one -- M6's ping-pong taught that.
    assert!(
        saves <= switches && switches - saves <= 1,
        "there were {} switches but {} fxsave operations. They must \
         match to within the one switch that can land between the two \
         reads: equality is what shows the save is unconditional, and a \
         lazy implementation would save far fewer times while still \
         preserving every register correctly (ADR 031).",
        switches,
        saves
    );
    assert!(
        restores <= switches && switches - restores <= 1,
        "{} switches but {} fxrstor operations",
        switches,
        restores
    );
    assert!(
        saves > 200,
        "only {} fxsave operations; too few for the equality above to \
         mean anything",
        saves
    );

    println!(
        "[user] xmm: thread A holds {:#018x}, thread B {:#018x} across \
         {} reports; {} fxsave over {} switches -- equal, so nothing is \
         lazy (ADR 031)",
        xmm_a,
        xmm_b,
        XMM_REPORTS.load(Ordering::Acquire),
        saves,
        switches
    );
}

/// 9. The timer preempts ring 3.
fn check_preemption_in_ring3() {
    // Both user threads are in infinite loops that make syscalls. If
    // the timer could not preempt ring 3, the first one to run would
    // own the CPU -- and the second would never have reported, which
    // the earlier checks already establish.
    //
    // What is added here is the direct evidence: the switch counts of
    // both threads keep rising while this kernel thread waits.
    // SAFETY: Live TCBs; no other borrow held.
    let (before_a, before_b) = unsafe { (tcb_at(128).switch_count(), tcb_at(160).switch_count()) };

    let ticks_before = sched::tick_count();
    let deadline = read_tsc() + 5_000_000_000;
    while read_tsc() < deadline {
        core::hint::spin_loop();
    }
    let ticks = sched::tick_count() - ticks_before;

    // SAFETY: As above.
    let (after_a, after_b) = unsafe { (tcb_at(128).switch_count(), tcb_at(160).switch_count()) };

    assert!(
        ticks > 0,
        "no timer tick arrived while waiting; the preemption check below \
         would prove nothing"
    );
    assert!(
        after_a > before_a || after_b > before_b,
        "neither user thread was scheduled while this kernel thread span \
         without yielding. The timer is not preempting ring 3, so a user \
         thread in a tight loop would own the CPU for good."
    );

    println!(
        "[user] preemption: {} ticks while the kernel span without \
         yielding; thread A ran {} more times, thread B {} more",
        ticks,
        after_a - before_a,
        after_b - before_b
    );
}

/// The measurement: what does a syscall cost?
fn measure_syscall_cost() {
    let calls_before = syscall::syscall_count();
    let start = read_tsc();

    // Let the user threads make calls; they are already looping.
    let deadline = start + 2_000_000_000;
    while read_tsc() < deadline {
        core::hint::spin_loop();
    }

    let elapsed = read_tsc() - start;
    let calls = syscall::syscall_count() - calls_before;

    assert!(
        calls > 0,
        "no syscall completed during the measurement window"
    );

    println!(
        "[user] syscall cost: {} calls in {} cycles, {} cycles each \
         (round trip including the user-side loop)",
        calls,
        elapsed,
        elapsed / calls
    );
    println!(
        "[user] syscall cost: measured under emulation. QEMU's TCG \
         interpreter does not model the pipeline flush a privilege \
         transition costs on silicon, and that flush is most of what a \
         syscall pays for -- so this number is a floor, and the ratio \
         between sysret and iretq (ADR 029) is more meaningful than \
         either absolute."
    );
}

// ---- Negative scenario (a): privileged instructions --------------------

/// User code executes `cli`, `hlt`, `rdmsr` and `lgdt`.
///
/// Each is privileged, and each must raise #GP at CPL 3. Expected to
/// exit 35.
///
/// Only one can be observed per boot -- the first #GP is fatal -- so
/// the program puts all four in sequence and the *first* one faults.
/// That is enough: if `cli` were permitted, the report would name a
/// later instruction or none at all.
pub fn run_privileged() -> ! {
    println!("[user] user code will execute privileged instructions");

    let mut program = Program::new();

    // Report that we got here at all, so the log distinguishes "the
    // thread never ran" from "the thread ran and faulted".
    program.emit(&mov_rdi_imm(0));
    program.emit(&mov_rsi_imm(Method::Echo as u64));
    program.emit(&mov_rdx_imm(0xC11));
    program.emit(&program::SYSCALL);

    // Then the privileged instructions, in order. `cli` first: it is
    // the one most likely to be wrongly permitted, because it looks
    // harmless.
    program.emit(&program::CLI);
    program.emit(&program::HLT);
    // rdmsr reads the MSR named in ECX; zero is IA32_P5_MC_ADDR, which
    // exists. The point is the privilege check, which happens first.
    program.emit(&[0x31, 0xc9]); // xor ecx, ecx
    program.emit(&program::RDMSR);
    program.emit(&program::LGDT_RSP);
    program.emit(&program::SPIN);

    let space = build_user_space(64, program.as_slice(), PATTERN_A);
    let tcb = build_user_thread(128, UTHREAD_A_SLOT, &space, 0);

    println!(
        "[user] running `cli` at cpl 3; expected #GP, error code 0 (not \
         a segment-selector fault)"
    );

    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(tcb_at(tcb));
    }

    let deadline = read_tsc() + 20_000_000_000;
    while read_tsc() < deadline {
        sched::yield_now();
    }

    unreachable!(
        "user code executed cli, hlt, rdmsr and lgdt without faulting. \
         CPL 3 is not being enforced -- check the DPL in the user code \
         descriptor and the RPL in the selector."
    );
}

// ---- Negative scenario (b): reading the kernel half --------------------

/// User code reads an address in the kernel half.
///
/// Expected: #PF whose error code has the USER bit (bit 2) set. That
/// bit is the point — it distinguishes "ring 3 tried to touch the
/// kernel" from "the kernel touched something unmapped", and the two
/// have entirely different meanings.
pub fn run_kernel_read() -> ! {
    println!("[user] user code will read a kernel address");

    let mut program = Program::new();

    program.emit(&mov_rdi_imm(0));
    program.emit(&mov_rsi_imm(Method::Echo as u64));
    program.emit(&mov_rdx_imm(0x00C0_DE0B));
    program.emit(&program::SYSCALL);

    // mov rax, KHEAP_START; mov rax, [rax]
    program.emit(&mov_rax_imm(layout::KHEAP_START));
    program.emit(&[0x48, 0x8b, 0x00]);
    program.emit(&program::SPIN);

    let space = build_user_space(64, program.as_slice(), PATTERN_A);
    let tcb = build_user_thread(128, UTHREAD_A_SLOT, &space, 0);

    println!(
        "[user] reading {:#018x} from cpl 3; expected #PF with the user \
         bit set in the error code",
        layout::KHEAP_START
    );

    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(tcb_at(tcb));
    }

    let deadline = read_tsc() + 20_000_000_000;
    while read_tsc() < deadline {
        sched::yield_now();
    }

    unreachable!(
        "user code read a kernel address without faulting. The kernel \
         half is reachable from ring 3, which means the USER bit is set \
         on mappings that should not have it."
    );
}

// ---- Negative scenario (c): SMEP ---------------------------------------

/// The kernel jumps into a user page.
///
/// Expected: #PF with the instruction-fetch bit (bit 4) set. This is
/// the first time SMEP has anything to protect — it has been in CR4
/// since M3, with no user pages in existence.
///
/// The page used here is **the same one** ring 3 executes from
/// successfully. Using a different page would leave the observation
/// explainable by a mapping difference rather than by SMEP.
pub fn run_smep() -> ! {
    println!("[user] the kernel will jump into a user code page");

    if !cpu::features().smep_enabled {
        panic!(
            "SMEP is not enabled on this CPU, so this scenario cannot \
             demonstrate anything. Under QEMU, add +smep to -cpu."
        );
    }

    let mut program = Program::new();
    program.emit(&program::SPIN);

    let space = build_user_space(64, program.as_slice(), PATTERN_A);
    let cnode = object_cnode();

    // First: prove ring 3 can execute this page. Without this half the
    // fault below could mean "the page is not executable" rather than
    // "the kernel may not execute it".
    let tcb = build_user_thread(128, UTHREAD_A_SLOT, &space, 0);
    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(tcb_at(tcb));
    }
    // Let it run. It spins, so reaching its first instruction is all
    // that is being shown; the switch count rising proves it was
    // scheduled.
    for _ in 0..100 {
        sched::yield_now();
    }
    // SAFETY: Live TCB; no other borrow held.
    let ran = unsafe { tcb_at(tcb) }.switch_count();
    assert!(
        ran > 0,
        "the user thread never ran, so 'ring 3 can execute this page' is \
         not established and the SMEP fault below would be ambiguous"
    );
    println!(
        "[user] ring 3 executed the page {} times; the same page is now \
         fetched from cpl 0",
        ran
    );
    // SAFETY: Live TCB.
    unsafe {
        sched::suspend(tcb_at(tcb));
    }

    // Now the kernel jumps there. The address has to be reachable from
    // the kernel's own address space, so the *physical* frame is
    // reached through PMAP -- which is NX, and would fault for the
    // wrong reason.
    //
    // Instead the kernel switches into the user VSpace and jumps to
    // the user virtual address. Nothing else changes: the page is
    // USER, present and executable, and the only difference from the
    // successful run above is the privilege level of the code fetching
    // it.
    //
    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(space.root_slot) };
    let root_phys = vspace::root_address(root).expect("the root is not a PageTable");

    println!(
        "[user] switching to the user vspace ({:#018x}) and jumping to \
         {:#018x} at cpl 0; expected #PF with the instruction-fetch bit",
        root_phys.0, USER_CODE_ADDRESS
    );

    let _guard = sched::InterruptGuard::new();

    // SAFETY: The VSpace carries the kernel's top-level entries, so
    // this code, its stack and the IDT stay mapped across the switch
    // (ADR 027, K2).
    unsafe {
        crate::memory::paging::load_table(crate::memory::PhysFrame::containing(root_phys));
    }

    // SAFETY: Not safe, and that is the point. The target is a USER
    // page and this is CPL 0, so SMEP must refuse the fetch.
    unsafe {
        let target = USER_CODE_ADDRESS as *const ();
        let function: extern "C" fn() -> ! = core::mem::transmute(target);
        function();
    }
}

// ---- Negative scenario (d): SMAP ---------------------------------------

/// The kernel reads a user page, with and without the guard.
///
/// Both halves in one scenario, because either alone proves half the
/// claim. Without the guard the read must fault; with it, the same
/// read at the same address must succeed.
pub fn run_smap() -> ! {
    println!("[user] the kernel will read a user page, with and without the guard");

    if !cpu::UserAccess::enforcing() {
        panic!(
            "SMAP is not enabled on this CPU, so this scenario cannot \
             demonstrate anything. Under QEMU, add +smap to -cpu."
        );
    }

    let mut program = Program::new();
    program.emit(&program::SPIN);

    let space = build_user_space(64, program.as_slice(), PATTERN_A);
    let cnode = object_cnode();

    // SAFETY: Index in range.
    let root = unsafe { cnode.slot(space.root_slot) };
    let root_phys = vspace::root_address(root).expect("the root is not a PageTable");

    let _guard = sched::InterruptGuard::new();

    // SAFETY: Every VSpace carries the kernel's half (ADR 027, K2).
    unsafe {
        crate::memory::paging::load_table(crate::memory::PhysFrame::containing(root_phys));
    }

    // --- with the guard: must succeed ---
    //
    // This half runs first, deliberately. If it ran second it would
    // never run at all -- the unguarded read ends the VM -- and the
    // scenario would be making half its claim.
    let guarded = {
        let _user = cpu::UserAccess::new();
        // SAFETY: The address is mapped readable in the VSpace now in
        // CR3, and the guard lifts SMAP for the duration.
        unsafe { (USER_DATA_ADDRESS as *const u64).read_volatile() }
    };
    assert_eq!(
        guarded, PATTERN_A,
        "the guarded read returned {:#018x} rather than {:#018x}; the \
         mapping is not what the test thinks it is, and the unguarded \
         read below would fault for the wrong reason",
        guarded, PATTERN_A
    );
    println!(
        "[user] smap: with a UserAccess guard, the kernel read {:#018x} \
         from {:#018x} -- succeeded",
        guarded, USER_DATA_ADDRESS
    );

    assert!(
        !cpu::user_access_allowed(),
        "RFLAGS.AC is still set after the guard dropped; the unguarded \
         read below would succeed and prove nothing"
    );

    // --- without the guard: must fault ---
    println!(
        "[user] smap: now reading {:#018x} with no guard; expected #PF, \
         supervisor read protection-violation",
        USER_DATA_ADDRESS
    );

    // SAFETY: Not safe. The page is USER and AC is clear, so SMAP must
    // refuse this read. The fault is the result the scenario is for.
    let unguarded = unsafe { (USER_DATA_ADDRESS as *const u64).read_volatile() };

    unreachable!(
        "the kernel read a user page without a guard and got {:#018x}. \
         SMAP is not protecting anything: every stray supervisor pointer \
         into user memory now succeeds silently.",
        unguarded
    );
}

// ---- Negative scenario (e): hostile user code --------------------------

/// User code invokes with forged and stolen capability pointers.
///
/// The first genuinely hostile input this kernel has taken. Everything
/// must return an error; the kernel must still be running at the end;
/// the scenario exits 33.
///
/// ADR 023's K5 at its sharpest: a kernel that panics on a bad CPtr is
/// a kernel any user component can halt.
pub fn run_cap_forge() -> ! {
    println!("[user] hostile user code will forge capability pointers");

    let mut program = Program::new();

    // A series of invocations, each with a differently wrong CPtr.
    // After each the user code reports what came back, so the log
    // shows an error was returned rather than the call silently doing
    // nothing.
    //
    // The CPtrs are built here rather than in the user program,
    // because computing them needs the CSpace's shape -- which user
    // code, correctly, has no way to learn.
    let space = boot::init_cspace();

    // 1. Reserved bits set. ADR 018 keeps them zero so they can mean
    //    something later; a pointer with them set was never valid.
    let reserved_set = cptr::CPtr(1 << 32);

    // 2. An index past the end of the root CNode.
    let out_of_range = cptr::CPtr::new(0xFFFF, 0, 0, 0);

    // 3. A valid slot with the wrong generation -- the shape of a
    //    pointer held across a revoke (ADR 018).
    // SAFETY: Valid indices.
    let real = unsafe { cptr::address_of(&space, boot::ROOT_SLOT_OBJECTS, boot::BOOT_TCB_SLOT) }
        .expect("could not address the boot TCB");
    let stale = real.with_l0_generation(real.l0_generation().wrapping_add(1));

    // 4. An empty slot that has never held anything.
    // SAFETY: Valid indices; slot 250 is untouched by this test.
    let empty = unsafe { cptr::address_of(&space, boot::ROOT_SLOT_OBJECTS, 250) }
        .expect("could not address an empty slot");

    // 5. A valid CPtr to a real object of the *wrong type* -- the
    //    Untyped CNode, addressed as though it were a TCB.
    // SAFETY: Valid indices.
    let wrong_type = unsafe { cptr::address_of(&space, boot::ROOT_SLOT_UNTYPED, 0) }
        .expect("could not address a root Untyped");

    // 6. An unknown method number on an otherwise valid capability.
    //    The one failure that is not about the pointer.

    for (index, cptr) in [reserved_set, out_of_range, stale, empty, wrong_type]
        .iter()
        .enumerate()
    {
        program.emit(&mov_rdi_imm(cptr.0));
        program.emit(&mov_rsi_imm(Method::ReadRegisters as u64));
        program.emit(&program::SYSCALL);
        // Report what came back: RAX into RDX, then Echo.
        program.emit(&[0x48, 0x89, 0xc2]); // mov rdx, rax
        program.emit(&mov_rdi_imm(FORGE_CHANNEL | index as u64));
        program.emit(&mov_rsi_imm(Method::Echo as u64));
        program.emit(&program::SYSCALL);
    }

    // The unknown method.
    program.emit(&mov_rdi_imm(real.0));
    program.emit(&mov_rsi_imm(9999));
    program.emit(&program::SYSCALL);
    program.emit(&[0x48, 0x89, 0xc2]);
    program.emit(&mov_rdi_imm(FORGE_CHANNEL | 5));
    program.emit(&mov_rsi_imm(Method::Echo as u64));
    program.emit(&program::SYSCALL);

    // Done: say so, then spin.
    program.emit(&mov_rdi_imm(FORGE_DONE_CHANNEL));
    program.emit(&mov_rsi_imm(Method::Echo as u64));
    program.emit(&mov_rdx_imm(0));
    program.emit(&program::SYSCALL);
    program.emit(&program::SPIN);

    let user_space = build_user_space(64, program.as_slice(), PATTERN_A);
    let tcb = build_user_thread(128, UTHREAD_A_SLOT, &user_space, 0);

    let errors_before = syscall::error_count();

    // SAFETY: Live TCB, not queued.
    unsafe {
        sched::enqueue(tcb_at(tcb));
    }

    let deadline = read_tsc() + 20_000_000_000;
    while !FORGE_DONE.load(Ordering::Acquire) && read_tsc() < deadline {
        sched::yield_now();
    }

    assert!(
        FORGE_DONE.load(Ordering::Acquire),
        "the hostile thread never finished; it may have taken the kernel \
         with it"
    );

    let errors = syscall::error_count() - errors_before;
    assert_eq!(
        errors, 6,
        "{} of the 6 hostile invocations returned an error. Every one of \
         them is invalid, so any that succeeded reached an object it had \
         no right to.",
        errors
    );

    // Each one must have failed for its *own* reason. Six calls all
    // returning InvalidArgument would satisfy a count and prove
    // nothing -- the same mistake ADR 027's two half-checks made.
    let expected = [
        (0usize, "InvalidCPtr", 1u64),
        (1, "CPtrOutOfRange", 2),
        (2, "StaleCPtr", 4),
        (3, "EmptySlot", 3),
        (4, "WrongType", 6),
        (5, "InvalidArgument", 21),
    ];
    for (index, name, code) in expected {
        let got = FORGE_RESULTS[index].load(Ordering::Acquire);
        assert!(
            syscall::is_error(got),
            "hostile invocation {} ({}) returned {:#018x}, which is not \
             an error",
            index,
            name,
            got
        );
        assert_eq!(
            syscall::error_code(got),
            code,
            "hostile invocation {} returned error {} where {} ({}) was \
             expected. Distinct failures must be distinguishable, or a \
             guard that stopped working would look like one that still \
             works (ADR 023, K5).",
            index,
            syscall::error_code(got),
            code,
            name
        );
    }

    // SAFETY: Live TCB.
    unsafe {
        sched::suspend(tcb_at(tcb));
    }

    println!(
        "[user] cap-forge: all 6 hostile invocations refused, each with \
         its own error -- InvalidCPtr, CPtrOutOfRange, StaleCPtr, \
         EmptySlot, WrongType, InvalidArgument"
    );
    println!("[user] the kernel is still running; exiting normally");

    syscall::report();

    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

/// Channel base for the forgery scenario's reports.
const FORGE_CHANNEL: u64 = 1 << 16;
/// Channel the forgery scenario uses to say it has finished.
const FORGE_DONE_CHANNEL: u64 = 1 << 17;

/// What each forged invocation returned.
static FORGE_RESULTS: [AtomicU64; 6] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Set once the hostile thread has run every case.
static FORGE_DONE: AtomicBool = AtomicBool::new(false);

/// Record a forgery result, called from the syscall dispatcher.
pub fn record_forge_result(channel: u64, value: u64) -> bool {
    if channel & FORGE_DONE_CHANNEL != 0 {
        FORGE_DONE.store(true, Ordering::Release);
        return true;
    }
    if channel & FORGE_CHANNEL != 0 {
        let index = (channel & 0xFF) as usize;
        if index < FORGE_RESULTS.len() {
            FORGE_RESULTS[index].store(value, Ordering::Release);
        }
        return true;
    }
    false
}
