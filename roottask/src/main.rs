// SPDX-License-Identifier: BSD-2-Clause
#![no_std]
#![no_main]

//! Safrox root task — the first user component (ADR 044).
//!
//! **This is the milestone.** Until M11 every component was built by
//! kernel code: `build_component` computed the CPtrs, wrote the
//! program, and bound the spaces. Here the kernel creates exactly one
//! thread and then stops; everything else in the system is created
//! from this file, at CPL 3, through `syscall`.
//!
//! What it does, in order:
//!
//! 1. reads its [`BootInfo`] and reports it;
//! 2. builds two components out of its own Untyped;
//! 3. connects them with an Endpoint and lets them talk;
//! 4. checks the kernel's accounting adds up.
//!
//! # What it cannot do
//!
//! It cannot call a kernel function — it is a separate crate and
//! links against nothing but `core` and the ABI definitions. If a
//! line here needed a kernel type, it would not compile. That is the
//! claim M12 makes, enforced rather than asserted.
//!
//! It has **no heap and no writable statics** (see `link.ld`). Its
//! state is on its stack. A `static mut` would be in `.data`, which
//! is mapped read-execute with the code, and the linker script fails
//! the build rather than letting it fault at run time.

use safrox_abi as abi;
use safrox_abi::{cap_type, method, perm, BootInfo, CPtr, UntypedDesc};

// ---- Entry -------------------------------------------------------------

/// Where the kernel enters, at CPL 3.
///
/// In `.text.start` so the linker puts it first: the kernel jumps to
/// the load address, so the entry point has to *be* the load address.
/// An `ENTRY(_start)` alone would not guarantee that — it records an
/// entry point in the ELF header, and the kernel does not read ELF
/// headers (ADR 044).
///
/// # Safety
///
/// Called by the kernel with a valid stack and nothing else. Never
/// returns: there is nowhere to return to.
#[no_mangle]
#[link_section = ".text.start"]
pub extern "C" fn _start() -> ! {
    main()
}

/// Reached only if something the root task cannot handle goes wrong.
///
/// It cannot print: printing needs a driver, a driver is a component,
/// and components are what this file creates. So it reports through
/// the one channel it has and then spins — the kernel's deadline
/// turns the spin into a test failure with the channel's value in the
/// log, which is more than a silent halt would give.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    echo(CHANNEL_PANIC, 0);
    loop {
        core::hint::spin_loop();
    }
}

// ---- The syscall ------------------------------------------------------

/// Invoke a capability.
///
/// One syscall and the method is an argument (ADR 017). The register
/// assignment is ADR 030's, widened by ADR 043:
///
/// ```text
///   RDI  the capability          RDX  arg0     R8  arg2
///   RSI  the method              R10  arg1     R9  arg3
/// ```
///
/// `syscall` destroys RCX and R11, which is why R10 carries arg1
/// rather than RCX — the SysV convention would have put it there and
/// the instruction would have eaten it.
fn invoke(cptr: u64, method: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let result: u64;
    // SAFETY: `syscall` is the only way out of ring 3 and the kernel
    // validates every argument -- a wrong capability pointer returns
    // an error rather than reaching an object (ADR 018). RCX and R11
    // are declared clobbered because the instruction destroys them;
    // the kernel restores everything else (ADR 030).
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rdi") cptr => _,
            inlateout("rsi") method => _,
            inlateout("rdx") arg0 => _,
            inlateout("r10") arg1 => _,
            inlateout("r8") arg2 => _,
            inlateout("r9") arg3 => _,
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Report a value on a channel.
///
/// `Echo` names no capability, so its CPtr argument is free to mean
/// something else — which is what makes it usable as a test channel
/// without adding a debug syscall the kernel would then have forever
/// (ADR 044).
fn echo(channel: u64, value: u64) -> u64 {
    invoke(channel, method::ECHO, value, 0, 0, 0)
}

// ---- Report channels ---------------------------------------------------
//
// Bit 28 upward; M8 uses 8..17, M10 uses 20 and 21, M11 uses 24 and
// 25. The low byte is the step.

const CHANNEL: u64 = 1 << 28;
const CHANNEL_DONE: u64 = 1 << 29;
const CHANNEL_PANIC: u64 = 1 << 30;

const STEP_MAGIC: u64 = 0;
const STEP_VERSION: u64 = 1;
const STEP_UNTYPED_COUNT: u64 = 2;
const STEP_UNTYPED_BYTES_LOW: u64 = 3;
const STEP_UNTYPED_BYTES_HIGH: u64 = 4;
const STEP_USABLE_LOW: u64 = 5;
const STEP_USABLE_HIGH: u64 = 6;
const STEP_KERNEL_BYTES_LOW: u64 = 7;
const STEP_KERNEL_BYTES_HIGH: u64 = 8;
const STEP_ACCOUNTING: u64 = 9;
/// Bytes in no Untyped and not kept by the kernel either.
const STEP_LOST_LOW: u64 = 15;
const STEP_LOST_HIGH: u64 = 16;
const STEP_CHILD_RETYPE: u64 = 10;
const STEP_CHILD_SETUP: u64 = 11;
const STEP_CHILD_RESUME: u64 = 12;
/// Reported by the **sending** child: what `Send` returned.
const STEP_SENT: u64 = 13;
/// Reported by the **receiving** child: the badge it woke with.
const STEP_RECEIVED: u64 = 14;

/// What the sending child puts in its message.
///
/// There is no matching `PONG`, and the absence is the honest shape
/// of what M12 can check. The *words* of a message are not readable
/// by a receiver yet — they live in the IPC buffer, which is inside
/// the TCB and mapped into no component's address space (see
/// `Method::Send`). What the receiver gets is the **badge**, which
/// is in a register and is the stronger evidence anyway: the sender
/// did not choose it.
///
/// So the claim the test can make is "a message arrived, from the
/// capability the root task minted", and not "these bytes came
/// through". The second needs the buffer mapped, which is M13.
const PING: u64 = 0x1234_5678_9ABC_DEF0;

// ---- Reading BootInfo --------------------------------------------------

/// Borrow the BootInfo the kernel mapped.
///
/// # Safety
///
/// Called once, at startup, before anything else could have unmapped
/// the page. The kernel maps it read-only and this takes a shared
/// reference, so the two agree.
unsafe fn boot_info() -> &'static BootInfo {
    // SAFETY: The kernel maps a page at this address before entering
    // ring 3 (ADR 044's ABI) and never unmaps it. The address is
    // page-aligned, so it satisfies BootInfo's alignment.
    unsafe { &*(abi::BOOTINFO_ADDRESS as *const BootInfo) }
}

/// Borrow the Untyped descriptor array.
///
/// # Safety
///
/// As [`boot_info`]; `count` must be the `untyped_count` that came
/// from the same header.
unsafe fn untyped_descs(count: u32) -> &'static [UntypedDesc] {
    let base = abi::BOOTINFO_ADDRESS as usize + abi::BOOTINFO_UNTYPED_OFFSET;
    // SAFETY: The kernel writes `count` descriptors starting here and
    // asserts the count fits the page, so the slice is inside the
    // mapping. 16-byte descriptors at a 128-byte offset from a page
    // boundary are aligned.
    unsafe { core::slice::from_raw_parts(base as *const UntypedDesc, count as usize) }
}

// ---- The program -------------------------------------------------------

fn main() -> ! {
    // SAFETY: First thing the root task does; the kernel mapped the
    // page before entering ring 3.
    let info = unsafe { boot_info() };

    // --- 1. Report what we were given --------------------------------
    //
    // Magic and version first, and in that order, because they are
    // the only two fields a root task of the wrong version may rely
    // on (ADR 044). Reporting them before anything else means a
    // mismatch shows up as a mismatch rather than as nonsense
    // further down.
    echo(CHANNEL | STEP_MAGIC, info.magic);
    echo(CHANNEL | STEP_VERSION, info.version as u64);

    if info.magic != abi::BOOTINFO_MAGIC || info.version != abi::BOOTINFO_VERSION {
        // Nothing below this point can be trusted: every offset is
        // version-dependent. Stop, having said why.
        echo(CHANNEL_DONE, 1);
        spin();
    }

    echo(CHANNEL | STEP_UNTYPED_COUNT, info.untyped_count as u64);

    // SAFETY: The count came from the header we just validated.
    let untypeds = unsafe { untyped_descs(info.untyped_count) };

    // --- 2. The accounting -------------------------------------------
    //
    // **The root task does this itself**, rather than being told a
    // figure. It holds the descriptors; it can sum them. The kernel
    // reports `usable_bytes` and `kernel_bytes` separately, so the
    // three numbers can be checked against each other -- a single
    // "available to you" figure would be unfalsifiable.
    let mut total: u64 = 0;
    for desc in untypeds {
        if desc.is_device == 0 {
            total = total.wrapping_add(desc.size());
        }
    }

    // Reported in halves: `Echo` carries one 64-bit argument and the
    // test wants the exact value, not a truncation.
    echo(CHANNEL | STEP_UNTYPED_BYTES_LOW, total & 0xFFFF_FFFF);
    echo(CHANNEL | STEP_UNTYPED_BYTES_HIGH, total >> 32);
    echo(CHANNEL | STEP_USABLE_LOW, info.usable_bytes & 0xFFFF_FFFF);
    echo(CHANNEL | STEP_USABLE_HIGH, info.usable_bytes >> 32);
    echo(
        CHANNEL | STEP_KERNEL_BYTES_LOW,
        info.kernel_bytes & 0xFFFF_FFFF,
    );
    echo(CHANNEL | STEP_KERNEL_BYTES_HIGH, info.kernel_bytes >> 32);
    echo(CHANNEL | STEP_LOST_LOW, info.lost_bytes & 0xFFFF_FFFF);
    echo(CHANNEL | STEP_LOST_HIGH, info.lost_bytes >> 32);

    // And the verdict, computed here. Every byte the kernel counted
    // as usable at boot is in exactly one of three places: an
    // Untyped the root task holds, what the kernel kept, or the
    // unreachable remainder.
    //
    // **The third term is reported, not absorbed.** Adding
    // `lost_bytes` to this sum is what makes the identity hold, and
    // stating it separately is what keeps the identity from being
    // vacuous: a kernel that lost half the machine would still
    // balance here, and the test asserts on `lost_bytes` itself
    // rather than on the balance alone.
    let accounted = total
        .wrapping_add(info.kernel_bytes)
        .wrapping_add(info.lost_bytes);
    echo(
        CHANNEL | STEP_ACCOUNTING,
        if accounted == info.usable_bytes { 1 } else { 0 },
    );

    // --- 3. Build two components and let them talk -------------------
    build_and_connect(info, untypeds);

    echo(CHANNEL_DONE, 0);
    spin();
}

/// A child's slots inside the root task's own leaf CNode.
///
/// Grouped because a component needs eleven capabilities and passing
/// them positionally is how an off-by-one becomes a mapping into the
/// wrong address space.
#[derive(Clone, Copy)]
struct ChildSlots {
    tcb: u32,
    cnode: u32,
    vspace: u32,
    /// Three levels for the code page. The stack is 1 MiB away and
    /// shares all three, so it needs none of its own.
    tables: [u32; 3],
    code: u32,
    stack: u32,
    /// First of four frames for the child's kernel stack.
    ///
    /// The child cannot name where these go — kernel-half memory is
    /// named by no capability — but it pays for them out of its
    /// parent's budget, which is the split ADR 043 drew.
    kstack: u32,
}

/// Build two components, connect them with an Endpoint, and let them
/// exchange a message.
///
/// **This is the milestone's claim.** Everything here runs at CPL 3
/// and uses nothing but capabilities from the root task's own CSpace.
/// The kernel is on the other side of `syscall` and nowhere else: it
/// has no code that could build a component even if it wanted to
/// (ADR 044's seal).
fn build_and_connect(info: &BootInfo, untypeds: &[UntypedDesc]) {
    // The biggest non-device Untyped. Biggest rather than first
    // because the first may be a few KiB -- boot carves the memory
    // map into aligned power-of-two blocks and the sizes vary by
    // orders of magnitude.
    let mut budget_slot = u32::MAX;
    let mut budget_bits = 0u8;
    for (index, desc) in untypeds.iter().enumerate() {
        if desc.is_device == 0 && desc.size_bits > budget_bits {
            budget_bits = desc.size_bits;
            budget_slot = info.untyped_first_slot + index as u32;
        }
    }
    if budget_slot == u32::MAX {
        // **A real error code, not a bare flag.**
        //
        // The first version reported `abi::ERROR_FLAG` alone, which
        // is error *zero* with the flag set — and zero is not an
        // error code. The scenario printed "reported error 0" and
        // the test accepted it, because the test only asked whether
        // the flag was set.
        //
        // That is a check indistinguishable from its own absence
        // (ADR 023, K5): a root task that reported the flag for any
        // reason at all — or for none — would pass identically.
        // `NotEnoughMemory` says *which* resource ran out, and it is
        // the caller's own budget, which is precisely the claim.
        echo(
            CHANNEL | STEP_CHILD_RETYPE,
            abi::ERROR_FLAG | abi::error::NOT_ENOUGH_MEMORY,
        );
        return;
    }

    let untyped = slot(budget_slot);
    let own_cnode = slot(info.root_cnode_slot);
    let mut next = info.free_slot_first;

    // One Endpoint, shared. Both children get a copy in their own
    // CSpace -- which is the whole mechanism: they cannot name each
    // other, cannot name the root task, and can reach exactly one
    // object in common.
    let ep = next;
    next += 1;

    let sender = ChildSlots {
        tcb: take(&mut next),
        cnode: take(&mut next),
        vspace: take(&mut next),
        tables: [take(&mut next), take(&mut next), take(&mut next)],
        code: take(&mut next),
        stack: take(&mut next),
        kstack: take_run(&mut next, 4),
    };
    let receiver = ChildSlots {
        tcb: take(&mut next),
        cnode: take(&mut next),
        vspace: take(&mut next),
        tables: [take(&mut next), take(&mut next), take(&mut next)],
        code: take(&mut next),
        stack: take(&mut next),
        kstack: take_run(&mut next, 4),
    };

    let mut worst = 0u64;

    // Everything both children need, in as few retypes as the types
    // allow. One Endpoint, two TCBs, two CNodes, eight PageTables
    // (one root plus three levels, each), four Frames (code and
    // stack each), eight Frames of kernel stack.
    worst = first_error(
        worst,
        retype(untyped, cap_type::ENDPOINT, 0, 1, own_cnode, ep),
    );
    for child in [&sender, &receiver] {
        worst = first_error(
            worst,
            retype(untyped, cap_type::TCB, 0, 1, own_cnode, child.tcb),
        );
        worst = first_error(
            worst,
            retype(untyped, cap_type::CNODE, 6, 1, own_cnode, child.cnode),
        );
        worst = first_error(
            worst,
            retype(untyped, cap_type::PAGE_TABLE, 0, 1, own_cnode, child.vspace),
        );
        worst = first_error(
            worst,
            retype(
                untyped,
                cap_type::PAGE_TABLE,
                0,
                3,
                own_cnode,
                child.tables[0],
            ),
        );
        worst = first_error(
            worst,
            retype(untyped, cap_type::FRAME, 0, 1, own_cnode, child.code),
        );
        worst = first_error(
            worst,
            retype(untyped, cap_type::FRAME, 0, 1, own_cnode, child.stack),
        );
        worst = first_error(
            worst,
            retype(untyped, cap_type::FRAME, 0, 4, own_cnode, child.kstack),
        );
    }

    echo(CHANNEL | STEP_CHILD_RETYPE, worst);
    if worst != 0 {
        return;
    }

    // The sender's program sends `PING` and spins; the receiver
    // waits, and reports the badge it woke with. Different badges,
    // so "a message arrived" can be told from "a message arrived
    // from the right place".
    worst = first_error(
        worst,
        setup_child(info, &sender, ep, SENDER_BADGE, &sender_program()),
    );
    worst = first_error(
        worst,
        setup_child(info, &receiver, ep, RECEIVER_BADGE, &receiver_program()),
    );

    echo(CHANNEL | STEP_CHILD_SETUP, worst);
    if worst != 0 {
        return;
    }

    // **The receiver first.** `Send` blocks until somebody receives
    // (ADR 033), so starting the sender alone would park it on the
    // endpoint and the scenario would wait for a rendezvous that
    // needs a thread nobody has started. Either order eventually
    // works -- the sender would simply block first -- but this order
    // makes the common case the fast one and the test's deadline is
    // not load-bearing.
    let r1 = invoke(slot(receiver.tcb).0, method::RESUME, 0, 0, 0, 0);
    let r2 = invoke(slot(sender.tcb).0, method::RESUME, 0, 0, 0, 0);
    echo(CHANNEL | STEP_CHILD_RESUME, first_error(r1, r2));
}

/// Take the next free slot.
fn take(next: &mut u32) -> u32 {
    let slot = *next;
    *next += 1;
    slot
}

/// Take `count` consecutive slots and return the first.
///
/// `Retype` writes its objects into consecutive slots from an
/// offset, so a run has to be reserved as a run — allocating four
/// kernel-stack frames one at a time would give four slots that
/// happen to be adjacent today.
fn take_run(next: &mut u32, count: u32) -> u32 {
    let first = *next;
    *next += count;
    first
}

/// Keep the first error, ignore later successes.
///
/// A sequence of invocations is only as good as its worst result,
/// and "last write wins" would let a failure be hidden by whatever
/// followed it — the mistake M11 made and paid for with a #PF in
/// ring 3 (ADR 043).
fn first_error(worst: u64, result: u64) -> u64 {
    if abi::is_error(worst) {
        worst
    } else if abi::is_error(result) {
        result
    } else {
        0
    }
}

/// Badges, so the receiver can tell who it heard from.
///
/// A badge is attached when a capability is copied (ADR 037) and is
/// what lets a server distinguish clients without trusting them.
const SENDER_BADGE: u32 = 0x5EED;
const RECEIVER_BADGE: u32 = 0x600D;

/// Give a child an address space, a CSpace, a program and a thread.
///
/// Returns the first error, or zero.
fn setup_child(
    info: &BootInfo,
    child: &ChildSlots,
    endpoint_slot: u32,
    badge: u32,
    program: &Code,
) -> u64 {
    let own_cnode = slot(info.root_cnode_slot);
    let mut worst = 0u64;

    // Give the child a CSpace shape before putting anything in it.
    // `SetSpace` below binds `child.cnode` as the child's **root**
    // CNode, and ADR 018 requires a root's slots to hold only
    // CNodes — so the child's capabilities live in a leaf, and the
    // leaf has to exist first.
    //
    // The leaf is the same CNode: `child.cnode` is copied into its
    // own slot 0, so the root's slot 0 holds a CNode which is the
    // root. A two-level CSpace with one object, which is legal
    // (the rule is about *types*, not about distinctness) and is
    // the smallest CSpace that resolves a CPtr at all.
    //
    // **`Copy`, not `Mint`, and the difference is load-bearing.**
    // A badge is stored in the field that holds a CNode's `radix`
    // (ADR 037), so minting this would replace "64 slots" with the
    // badge and every index into the child's CSpace would then be
    // out of range. `Copy` carries the field across untouched.
    //
    // This is what showed that `Mint` alone was not enough: M12
    // added it as *the* way to hand a capability over, on the
    // reasoning that a zero badge made it a plain copy. It does
    // not — zero is written like any other value.
    worst = first_error(
        worst,
        invoke(
            own_cnode.0,
            method::COPY,
            child.cnode as u64,
            slot(child.cnode).0,
            CHILD_LEAF_SLOT as u64,
            0,
        ),
    );

    // A fresh PageTable is not an address space: its kernel half is
    // empty, and a thread running in it could not take an interrupt.
    // The root task cannot fill that in -- the entries name kernel
    // memory and it holds no capability for any -- so it asks.
    worst = first_error(
        worst,
        invoke(slot(child.vspace).0, method::CREATE_VSPACE, 0, 0, 0, 0),
    );

    // Three levels for the code page. The stack is 1 MiB away and
    // shares all three, so its mapping needs no tables of its own --
    // `AlreadyMapped` would come back for every one.
    for (index, level) in [3u64, 2, 1].iter().enumerate() {
        worst = first_error(
            worst,
            invoke(
                slot(child.tables[index]).0,
                method::PAGE_TABLE_MAP,
                slot(child.vspace).0,
                CHILD_CODE,
                *level,
                0,
            ),
        );
    }

    worst = first_error(worst, load_child(info, child.code, child.vspace, program));

    worst = first_error(
        worst,
        invoke(
            slot(child.stack).0,
            method::FRAME_MAP,
            slot(child.vspace).0,
            CHILD_STACK,
            perm::READ_WRITE,
            0,
        ),
    );

    // **The Endpoint goes into the child's own CSpace, badged.**
    //
    // This is what "connecting two components" means here, and it
    // is the only thing the two share: no capability naming each
    // other, no Untyped, no way to reach the root task. The
    // connection *is* the capability.
    //
    // The badge is how the receiver tells who it heard from
    // (ADR 037). It is attached at the copy, by the component doing
    // the handing-over, so the sender cannot choose its own
    // identity — which is the whole reason a badge is worth
    // anything.
    //
    // The child's leaf index is fixed at `CHILD_ENDPOINT_SLOT`: the
    // child's CSpace is one CNode it has never touched, so slot 0
    // is free and its generation is zero. That is what lets the
    // child's program name the Endpoint as an immediate without
    // being told anything at run time.
    worst = first_error(
        worst,
        invoke(
            own_cnode.0,
            method::MINT,
            endpoint_slot as u64,
            slot(child.cnode).0,
            CHILD_ENDPOINT_SLOT as u64,
            badge as u64,
        ),
    );

    worst = first_error(
        worst,
        invoke(
            slot(child.tcb).0,
            method::SET_USER_REGISTERS,
            CHILD_CODE,
            CHILD_STACK + 4096 - 16,
            own_cnode.0,
            child.kstack as u64,
        ),
    );
    worst = first_error(
        worst,
        invoke(
            slot(child.tcb).0,
            method::SET_SPACE,
            slot(child.vspace).0,
            slot(child.cnode).0,
            0,
            0,
        ),
    );

    worst
}

/// Where a child's code and stack go, in the child's own address
/// space.
const CHILD_CODE: u64 = 0x0040_0000;
const CHILD_STACK: u64 = 0x0050_0000;

/// Index, in a child's root CNode, of the leaf that is itself.
///
/// Zero: a child's CSpace is a single CNode minted into its own slot
/// 0, which is the smallest shape a two-level CPtr can resolve
/// through (ADR 018).
const CHILD_LEAF_SLOT: u32 = 0;

/// Index, in a child's leaf, of the Endpoint it was given.
///
/// One, because slot 0 holds the leaf itself.
const CHILD_ENDPOINT_SLOT: u32 = 1;

/// The CPtr a **child** uses for its Endpoint.
///
/// Computed here and baked into the child's program as an
/// immediate. Both generations are zero: the child's CNode was
/// retyped moments ago and nothing has been deleted from it. A
/// component that had started revoking would have to be told its
/// pointers rather than being able to assume them — which is why a
/// real system passes them in BootInfo rather than computing them.
const CHILD_EP_CPTR: u64 = (CHILD_LEAF_SLOT as u64) << 16 | (CHILD_ENDPOINT_SLOT as u64);

/// Where the root task maps a frame it is about to write into.
///
/// 7 MiB, inside a page table that already exists: the kernel mapped
/// the root task's code at 4 MiB, stack at 5 and BootInfo at 6, and
/// a PT covers 2 MiB — so 6..8 MiB is covered and 7 is free. 8 would
/// need a table nothing has created, and the kernel does not create
/// them (ADR 027).
const SCRATCH: u64 = 0x0070_0000;

/// Map a frame here, write a program into it, unmap, map it into the
/// child executable.
fn load_child(info: &BootInfo, code_slot: u32, vspace_slot: u32, program: &Code) -> u64 {
    let program = program.as_slice();
    let own_vspace = slot(info.vspace_slot);
    let code = slot(code_slot);

    let r = invoke(
        code.0,
        method::FRAME_MAP,
        own_vspace.0,
        SCRATCH,
        perm::READ_WRITE,
        0,
    );
    if abi::is_error(r) {
        return r;
    }

    // SAFETY: The mapping above succeeded, so this page is writable
    // in this address space and belongs to the Frame just retyped.
    // The length is checked against the page size by the caller's
    // program being a fixed small array.
    unsafe {
        let dest = SCRATCH as *mut u8;
        core::ptr::copy_nonoverlapping(program.as_ptr(), dest, program.len());
    }

    let r = invoke(own_vspace.0, method::FRAME_UNMAP, SCRATCH, 0, 0, 0);
    if abi::is_error(r) {
        return r;
    }

    invoke(
        code.0,
        method::FRAME_MAP,
        slot(vspace_slot).0,
        CHILD_CODE,
        perm::READ_EXECUTE,
        0,
    )
}

/// `Untyped.Retype`, with the ABI's argument packing.
fn retype(
    untyped: CPtr,
    cap_type: u64,
    size_bits: u64,
    count: u64,
    dest: CPtr,
    offset: u32,
) -> u64 {
    invoke(
        untyped.0,
        method::RETYPE,
        abi::retype_arg(cap_type, size_bits),
        count,
        dest.0,
        offset as u64,
    )
}

/// The CPtr for one of our own leaf slots.
///
/// Every capability the root task holds is in leaf 0 of its root
/// CNode, and both generations are zero on a freshly retyped CNode —
/// which is what lets this be arithmetic rather than a lookup. A
/// component that has started revoking things would need to track
/// generations; the root task does not revoke.
fn slot(index: u32) -> CPtr {
    CPtr::new(0, 0, index as u16, 0)
}

/// A buffer a child's program is assembled into.
///
/// Hand-assembled because there is no second binary to load and no
/// loader to load it with — the root task *is* the loader, and what
/// it loads has to come from somewhere. Short enough to read, which
/// is the only thing that makes it tolerable.
struct Code {
    bytes: [u8; 256],
    len: usize,
}

impl Code {
    const fn new() -> Code {
        Code {
            bytes: [0; 256],
            len: 0,
        }
    }

    /// `mov r64, imm64` for one of the six registers an invocation
    /// uses, then the immediate.
    fn load(&mut self, reg: Reg, value: u64) -> &mut Code {
        let (rex, opcode) = match reg {
            Reg::Rdi => (0x48, 0xbf),
            Reg::Rsi => (0x48, 0xbe),
            Reg::Rdx => (0x48, 0xba),
            Reg::R10 => (0x49, 0xba),
            Reg::R8 => (0x49, 0xb8),
            Reg::R9 => (0x49, 0xb9),
        };
        self.emit(&[rex, opcode]);
        self.emit(&value.to_le_bytes())
    }

    fn emit(&mut self, bytes: &[u8]) -> &mut Code {
        // No panic: the root task's panic handler cannot print, so a
        // truncated program is reported through the channel instead
        // and the test sees a child that never spoke. Saturating
        // rather than wrapping, so an overflow cannot corrupt the
        // earlier instructions.
        let end = (self.len + bytes.len()).min(self.bytes.len());
        let take = end - self.len;
        self.bytes[self.len..end].copy_from_slice(&bytes[..take]);
        self.len = end;
        self
    }

    /// `syscall`.
    fn syscall(&mut self) -> &mut Code {
        self.emit(&[0x0f, 0x05])
    }

    /// `jmp $` — two bytes, and the only way for ring 3 to idle
    /// without a syscall (`hlt` is privileged).
    fn spin(&mut self) -> &mut Code {
        self.emit(&[0xeb, 0xfe])
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// The registers an invocation uses (ADR 030, widened by ADR 043).
enum Reg {
    Rdi,
    Rsi,
    Rdx,
    R10,
    R8,
    R9,
}

/// The program the **sending** child runs.
///
/// Sends `PING` over the Endpoint it was given, then reports and
/// spins. `Send` blocks until somebody receives, so reaching the
/// report at all means the rendezvous completed.
fn sender_program() -> Code {
    let mut code = Code::new();

    // Endpoint.Send(label = PING's low 32 bits, words = PING, 0, 0)
    //
    // The label carries the value because the three message words
    // are not readable by the receiver yet — the IPC buffer is not
    // mapped into any component (see `Method::Send`). The badge is
    // what the receiver checks, and the label is here so the
    // message is not empty.
    code.load(Reg::Rdi, CHILD_EP_CPTR)
        .load(Reg::Rsi, method::SEND)
        .load(Reg::Rdx, PING & 0xFFFF_FFFF)
        .load(Reg::R10, PING)
        .load(Reg::R8, 0)
        .load(Reg::R9, 0)
        .syscall();

    // Report what Send returned: zero on success, an error
    // otherwise. RAX holds it; move to RDX, which is Echo's
    // argument.
    code.emit(&[0x48, 0x89, 0xc2]) // mov rdx, rax
        .load(Reg::Rdi, CHANNEL | STEP_SENT)
        .load(Reg::Rsi, method::ECHO)
        .syscall()
        .spin();

    code
}

/// The program the **receiving** child runs.
///
/// Waits on the Endpoint and reports the badge it woke with. The
/// badge is the claim: it comes from the capability the *sender*
/// used, which the sender did not choose — the root task attached it
/// when it minted the copy (ADR 037). A message that arrived from
/// somewhere else would carry a different one.
fn receiver_program() -> Code {
    let mut code = Code::new();

    code.load(Reg::Rdi, CHILD_EP_CPTR)
        .load(Reg::Rsi, method::RECV)
        .load(Reg::Rdx, 0)
        .load(Reg::R10, 0)
        .load(Reg::R8, 0)
        .load(Reg::R9, 0)
        .syscall();

    code.emit(&[0x48, 0x89, 0xc2]) // mov rdx, rax
        .load(Reg::Rdi, CHANNEL | STEP_RECEIVED)
        .load(Reg::Rsi, method::ECHO)
        .syscall()
        .spin();

    code
}

fn spin() -> ! {
    loop {
        core::hint::spin_loop();
    }
}
