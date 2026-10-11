// SPDX-License-Identifier: MPL-2.0
//! Building the serial driver, from CPL 3 (M13).
//!
//! **The root task is the loader.** The kernel has no ELF loader and
//! must not grow one — loading is a component's job (ADR 043) — so
//! the driver's flat image is embedded in this binary's `.rodata`,
//! copied into a Frame this component owns, and mapped read-execute
//! into an address space this component built.
//!
//! What the driver gets, and nothing else:
//!
//! | capability   | why |
//! |--------------|-----|
//! | IOPort       | 0x3F8..=0x3FF. Not 0x70, which is the test. |
//! | IRQHandler   | line 4, so the device can wake it. |
//! | Notification | where that interrupt is delivered (ADR 042). |
//! | Endpoint     | where clients ask for things. |
//! | its own TCB  | so it can bind the Notification to itself. |
//!
//! It holds **no Untyped**, so it cannot create objects; no
//! capability naming another component, so it cannot reach one; and
//! no IOPortControl, so it cannot widen its own port range. Every
//! one of those is a thing the root task holds and did not hand over.

use safrox_abi as abi;
use safrox_abi::{cap_type, method, perm, BootInfo, CPtr};

use crate::{echo, first_error, invoke, slot, take, take_run, Code, CHANNEL};

/// The driver's flat image, built by `scripts/build-driver.sh`.
///
/// `include_bytes!` for the reason the kernel embeds the root task
/// the same way: there is no filesystem, no loader and no second
/// thing to load from. The build script checks the size and the
/// entry point, because this component copies bytes and jumps.
static DRIVER_IMAGE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../build/serial-driver.bin"
));

/// Pages the root task maps for the driver's code.
///
/// Two, matching the ASSERT in `driver/link.ld` and the check in
/// `scripts/build-driver.sh`. Written in three places on purpose: a
/// change has to be made three times deliberately rather than once
/// by accident.
const DRIVER_CODE_PAGES: u32 = 2;

/// The channel the **driver and its clients** report on.
///
/// Bit 31, matching `DRIVER_CHANNEL` in `kernel/src/drivertest.rs`
/// and `CHANNEL` in `driver/src/main.rs`. Three files agreeing on
/// one number is a hazard, and it bit once already: the hostile
/// component reported on the root task's channel (bit 28) while the
/// test listened on bit 31, so three refusals happened, were
/// reported, and were recorded nowhere. The scenario failed saying
/// the attempts never occurred.
///
/// The right fix is for this to live in the ABI crate, which both
/// sides already depend on — a channel number is as much a
/// kernel/user agreement as a method number. That is a small change
/// and it is not this milestone's; recorded here rather than done
/// quietly, because the next person to add a channel will hit the
/// same thing.
const DRIVER_CHANNEL: u64 = 1 << 31;

// ---- Report steps ------------------------------------------------------

/// The IOPort capability was issued.
pub const STEP_IOPORT_ISSUE: u64 = 22;
/// The IRQ 4 handler was claimed.
pub const STEP_IRQ_GET: u64 = 23;
/// The driver thread was resumed.
pub const STEP_DRIVER_RESUME: u64 = 24;
/// The client thread was resumed.
pub const STEP_CLIENT_RESUME: u64 = 25;
/// A second `IOPortControl.Issue` for the same range was refused.
pub const STEP_SECOND_ISSUE: u64 = 26;
/// A second `IRQControl.Get` for line 4 was refused.
pub const STEP_SECOND_IRQ: u64 = 27;
/// The driver's capabilities were revoked; how many died.
pub const STEP_REVOKED: u64 = 28;
/// A replacement driver was built and started.
pub const STEP_RESTARTED: u64 = 29;
/// A component with no authority tried to reach the hardware.
///
/// Three consecutive steps: 30, 31, 32.
pub const STEP_HOSTILE: u64 = 30;
/// The handshake that tells the root task its driver is serving.
pub const STEP_PING: u64 = 33;

// ---- The driver's own CSpace -------------------------------------------
//
// These indices are the other half of the constants in
// `driver/src/main.rs`. Two files that must agree, and the agreement
// is unchecked — which is a real weakness and worth naming rather
// than hiding. A BootInfo of its own would fix it; the driver is one
// component with five capabilities, and a protocol for that would be
// more code than the thing it protects.

const DRIVER_LEAF: u32 = 0;
const DRIVER_ENDPOINT: u32 = 1;
const DRIVER_IOPORT: u32 = 2;
const DRIVER_IRQ: u32 = 3;
const DRIVER_NOTIFICATION: u32 = 4;

/// The client's CSpace: just the Endpoint.
const CLIENT_LEAF: u32 = 0;
const CLIENT_ENDPOINT: u32 = 1;

/// Where a component's code and stack go, in its own address space.
const CODE: u64 = 0x0040_0000;
const STACK: u64 = 0x0050_0000;

/// COM1's register window (ADR 045).
const COM1_FIRST: u64 = 0x3F8;
const COM1_LAST: u64 = 0x3FF;

/// The line COM1 raises. Fixed by the IBM PC: COM1 and COM3 share
/// IRQ 4, COM2 and COM4 share IRQ 3.
const COM1_IRQ: u64 = 4;

/// The client's identity, as the root task assigns it (ADR 047).
///
/// The driver reads this out of the badge and **the client cannot
/// choose it** — it is attached when this component mints the
/// Endpoint capability, which is the entire reason a badge is worth
/// anything (ADR 037).
const CLIENT_ID: u64 = 0xC11E;

/// Commands, as the label encodes them (ADR 047).
const CMD_WRITE: u64 = 1;
const CMD_FINISH: u64 = 2;
/// "Are you serving yet?" — see [`ping_driver`].
const CMD_PING: u64 = 3;

/// Slots a component needs in the root task's own leaf CNode.
#[derive(Clone, Copy)]
pub struct Component {
    pub tcb: u32,
    pub cnode: u32,
    pub vspace: u32,
    pub tables: [u32; 3],
    pub code: u32,
    pub stack: u32,
    /// How many pages of stack. One for everybody until M15.
    pub stack_pages: u32,
    pub kstack: u32,
}

impl Component {
    /// Reserve the slots one component needs.
    pub fn allocate(next: &mut u32, code_pages: u32) -> Component {
        Component::allocate_sized(next, code_pages, 1)
    }

    /// As [`Component::allocate`], with the stack's size spelled out.
    ///
    /// # Why the stack became a parameter
    ///
    /// It was one page for everybody, and that was right for every
    /// component up to M14: a driver's state is a few counters in
    /// its main loop's locals.
    ///
    /// The console keeps a **character grid** — one byte per cell,
    /// 160x50 on this screen, 8000 bytes — and it lives in `serve`'s
    /// locals like everybody else's state, because the image is
    /// mapped read-execute and `link.ld` asserts `.bss` is empty.
    /// 8000 bytes does not fit in 4096.
    ///
    /// A parameter rather than raising it for everyone: a four-page
    /// stack for a component that needs a few hundred bytes is waste
    /// charged to the wrong budget (ADR 022), and three of the four
    /// pages would never be touched.
    ///
    /// The alternative was a Frame mapped read-write for the grid.
    /// It would work and buys nothing — the grid is private,
    /// single-threaded, and exactly as large as a stack can hold.
    pub fn allocate_sized(next: &mut u32, code_pages: u32, stack_pages: u32) -> Component {
        Component {
            tcb: take(next),
            cnode: take(next),
            vspace: take(next),
            tables: [take(next), take(next), take(next)],
            code: take_run(next, code_pages),
            stack: take_run(next, stack_pages),
            stack_pages,
            kstack: take_run(next, 4),
        }
    }
}

/// Radix of the CNode an ordinary component gets: 64 slots.
///
/// Enough for a driver's five capabilities and a client's one, with
/// room to spare. A component that owns a large device needs more —
/// see `screen::SCREEN_CNODE_RADIX`, which is 11 because a
/// framebuffer is a thousand Frames and each needs a slot.
const COMPONENT_CNODE_RADIX: u64 = 6;

/// Create the objects a component is made of.
pub fn retype_component(
    untyped: CPtr,
    own_cnode: CPtr,
    component: &Component,
    code_pages: u32,
) -> u64 {
    retype_component_sized(
        untyped,
        own_cnode,
        component,
        code_pages,
        COMPONENT_CNODE_RADIX,
    )
}

/// As [`retype_component`], with the CSpace's size spelled out.
///
/// # Why the radix became a parameter
///
/// It was 6 for everybody, which is 64 slots, and that was right for
/// every component up to M13 — a driver holds five capabilities and
/// a client holds one.
///
/// The screen component holds **one per page of the framebuffer**,
/// because mapping is per page and each mapping needs a Frame
/// capability in a slot. A 1280x800 screen is 1000 of them. With 64
/// slots the component mapped 47 pages and the 48th retype came back
/// `CPtrOutOfRange` — the CSpace, not the memory, was what ran out.
///
/// A parameter rather than raising the constant for everyone: a
/// 64 KiB CNode for a component that needs six slots would be waste
/// charged to the wrong budget, and ADR 022's point is that somebody
/// pays for every object.
pub fn retype_component_sized(
    untyped: CPtr,
    own_cnode: CPtr,
    component: &Component,
    code_pages: u32,
    cnode_radix: u64,
) -> u64 {
    let mut worst = 0u64;
    worst = first_error(
        worst,
        crate::retype(untyped, cap_type::TCB, 0, 1, own_cnode, component.tcb),
    );
    worst = first_error(
        worst,
        crate::retype(
            untyped,
            cap_type::CNODE,
            cnode_radix,
            1,
            own_cnode,
            component.cnode,
        ),
    );
    worst = first_error(
        worst,
        crate::retype(
            untyped,
            cap_type::PAGE_TABLE,
            0,
            1,
            own_cnode,
            component.vspace,
        ),
    );
    worst = first_error(
        worst,
        crate::retype(
            untyped,
            cap_type::PAGE_TABLE,
            0,
            3,
            own_cnode,
            component.tables[0],
        ),
    );
    worst = first_error(
        worst,
        crate::retype(
            untyped,
            cap_type::FRAME,
            0,
            code_pages as u64,
            own_cnode,
            component.code,
        ),
    );
    worst = first_error(
        worst,
        crate::retype(
            untyped,
            cap_type::FRAME,
            0,
            component.stack_pages as u64,
            own_cnode,
            component.stack,
        ),
    );
    worst = first_error(
        worst,
        crate::retype(untyped, cap_type::FRAME, 0, 4, own_cnode, component.kstack),
    );
    worst
}

/// Give a component an address space, a CSpace and a program.
///
/// Returns the first error, or zero. Everything here is an
/// invocation: this component holds no kernel privilege and is
/// building another one entirely out of capabilities it was given.
pub fn setup_component(
    info: &BootInfo,
    component: &Component,
    image: &[u8],
    code_pages: u32,
) -> u64 {
    setup_component_patched(info, component, image, code_pages, None)
}

/// As [`setup_component`], but patching each code page after it is
/// copied and before it is mapped read-execute.
///
/// One caller: the screen component, whose geometry has to be in its
/// image before it runs (ADR 049).
///
/// A second entry point rather than a fifth parameter on the first,
/// because four of the five callers have nothing to patch and
/// `None` at each of them would be noise that has to be read.
pub fn setup_component_patched(
    info: &BootInfo,
    component: &Component,
    image: &[u8],
    code_pages: u32,
    patch: Option<&dyn Fn(u32, *mut u8)>,
) -> u64 {
    let own_cnode = slot(info.root_cnode_slot);
    let mut worst = 0u64;

    // **A program must fit the pages it is being given.**
    //
    // Checked here rather than left to the copy, because the copy
    // would happily write a prefix and the prefix would *run*: the
    // rest of the frame is zeroed, `00 00` decodes as
    // `add [rax], al`, and the component faults somewhere with no
    // visible relationship to its program being short.
    //
    // That happened — a client wrote four of its five bytes and
    // then took a #PF at a plausible-looking address. The check is
    // one comparison; the debugging was not.
    if image.len() > (code_pages as usize) * 4096 {
        return abi::ERROR_FLAG | abi::error::INVALID_ARGUMENT;
    }

    // The child's CSpace: one CNode copied into its own slot 0, which
    // is the smallest shape a two-level CPtr resolves through
    // (ADR 018). `Copy`, not `Mint` — a badge would overwrite the
    // radix and every index into the CSpace would be out of range.
    worst = first_error(
        worst,
        invoke(
            own_cnode.0,
            method::COPY,
            component.cnode as u64,
            slot(component.cnode).0,
            DRIVER_LEAF as u64,
            0,
        ),
    );

    // A fresh PageTable is not an address space: its kernel half is
    // empty and a thread running in it could not take an interrupt.
    worst = first_error(
        worst,
        invoke(slot(component.vspace).0, method::CREATE_VSPACE, 0, 0, 0, 0),
    );

    for (index, level) in [3u64, 2, 1].iter().enumerate() {
        worst = first_error(
            worst,
            invoke(
                slot(component.tables[index]).0,
                method::PAGE_TABLE_MAP,
                slot(component.vspace).0,
                CODE,
                *level,
                0,
            ),
        );
    }

    worst = first_error(worst, load_image(info, component, image, code_pages, patch));

    // The stack grows **down**, so a multi-page stack is mapped with
    // its last page at `STACK` and the earlier ones below it: page
    // `n` of `stack_pages` goes to `STACK - (pages - 1 - n) * 4096`.
    //
    // Mapping them upwards from `STACK` instead would put the spare
    // pages where the stack never goes, and the first push past
    // 4 KiB would fault — a four-page stack that behaves exactly
    // like a one-page stack, which is ADR 023's K5 in a mapping
    // loop.
    for page in 0..component.stack_pages {
        let address = STACK - ((component.stack_pages - 1 - page) as u64) * 4096;
        worst = first_error(
            worst,
            invoke(
                slot(component.stack + page).0,
                method::FRAME_MAP,
                slot(component.vspace).0,
                address,
                perm::READ_WRITE,
                0,
            ),
        );
    }

    worst = first_error(
        worst,
        invoke(
            slot(component.tcb).0,
            method::SET_USER_REGISTERS,
            CODE,
            STACK + 4096 - 16,
            own_cnode.0,
            component.kstack as u64,
        ),
    );
    worst = first_error(
        worst,
        invoke(
            slot(component.tcb).0,
            method::SET_SPACE,
            slot(component.vspace).0,
            slot(component.cnode).0,
            0,
            0,
        ),
    );

    worst
}

/// Where the root task maps a frame it is about to write into.
///
/// 7 MiB: the kernel mapped code at 4, stack at 5 and BootInfo at 6,
/// and a PT covers 2 MiB — so 6..8 is covered and 7 is free. 8 would
/// need a table nothing has created, and the kernel does not create
/// them (ADR 027).
const SCRATCH: u64 = 0x0070_0000;

/// Copy an image into a component's code frames.
///
/// Map a frame here, write a page of the program into it, unmap, map
/// it into the child executable. Once per page: the scratch address
/// is reused, which is why the unmap is not optional.
///
/// `patch` is applied to each page **after** the copy, while the
/// page is still writable through the scratch mapping. It is given
/// the page number and may write anywhere in the 4 KiB there.
///
/// # Why patching happens here
///
/// The screen component needs its geometry written into its image
/// before it runs (ADR 049). The obvious way is to copy the image
/// into a buffer, patch the buffer, and load that — and the obvious
/// way overflowed the root task's stack, which is **one page**. A
/// 16 KiB buffer on a 4 KiB stack faulted at `0x4fffb0`, sixteen
/// bytes below the stack's base.
///
/// There is no buffer to overflow if the patch happens where the
/// copy already is. The scratch page is mapped writable for exactly
/// this window and unmapped immediately after, so nothing here is
/// reachable once the component starts.
fn load_image(
    info: &BootInfo,
    component: &Component,
    image: &[u8],
    code_pages: u32,
    patch: Option<&dyn Fn(u32, *mut u8)>,
) -> u64 {
    let own_vspace = slot(info.vspace_slot);

    for page in 0..code_pages {
        let frame = slot(component.code + page);

        let r = invoke(
            frame.0,
            method::FRAME_MAP,
            own_vspace.0,
            SCRATCH,
            perm::READ_WRITE,
            0,
        );
        if abi::is_error(r) {
            return r;
        }

        // The slice of the image this page carries. The last page is
        // usually partial; the frame was zeroed by retype, so the
        // remainder is already zero.
        let start = (page as usize) * 4096;
        let end = core::cmp::min(start + 4096, image.len());

        if start < image.len() {
            // SAFETY: The mapping above succeeded, so this page is
            // writable in this address space and belongs to the Frame
            // just retyped. The length is bounded by the page size
            // because `end - start <= 4096` by construction.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    image.as_ptr().add(start),
                    SCRATCH as *mut u8,
                    end - start,
                );
            }
        }

        // The patch, while the page is still writable here. After
        // the unmap below it is mapped read-execute in the child
        // and could not be written even by the component itself.
        if let Some(patch) = patch {
            patch(page, SCRATCH as *mut u8);
        }

        let r = invoke(own_vspace.0, method::FRAME_UNMAP, SCRATCH, 0, 0, 0);
        if abi::is_error(r) {
            return r;
        }

        // Read-execute in the child. **Not writable**, and the
        // encoding makes writable-and-executable unrepresentable
        // rather than merely refused (ADR 032).
        let r = invoke(
            frame.0,
            method::FRAME_MAP,
            slot(component.vspace).0,
            CODE + (page as u64) * 4096,
            perm::READ_EXECUTE,
            0,
        );
        if abi::is_error(r) {
            return r;
        }
    }

    0
}

/// Everything a driver **instance** needs beyond an ordinary
/// component.
///
/// # Why the Endpoint is passed in rather than allocated here
///
/// The other three are the driver's: its port window, its interrupt
/// line, the Notification that line delivers to. They are created
/// when it is built and destroyed when it dies.
///
/// The Endpoint is **not the driver's**. It is the name clients
/// know the *service* by, it belongs to the root task, and it has
/// to outlive any particular driver — otherwise `driver-restart`
/// is not a restart, it is a second service that happens to do the
/// same job, and every client would have to be re-minted a new
/// capability.
///
/// An earlier version allocated all four together, which made
/// `issue_hardware` create a fresh Endpoint for the replacement
/// driver. The replacement would then have received on an object
/// nobody was sending to while the client blocked forever on the
/// first one — and the scenario's claim, that the client never
/// learns anything happened, would have been false in the one place
/// it is made.
///
/// Taking it as a parameter is what makes that unrepresentable:
/// there is no longer a slot for a driver to own an Endpoint in.
#[derive(Clone, Copy)]
pub struct DriverCaps {
    /// The IOPort for COM1, in the root task's CSpace.
    pub ioport: u32,
    /// The IRQHandler for line 4.
    pub irq: u32,
    /// The Notification it is delivered to.
    pub notification: u32,
    /// The Endpoint clients send to. **The root task's**, shared by
    /// every driver that ever serves it.
    pub endpoint: u32,
}

impl DriverCaps {
    /// Reserve slots for one driver instance's hardware.
    ///
    /// `endpoint` names an Endpoint the caller already holds.
    pub fn allocate(next: &mut u32, endpoint: u32) -> DriverCaps {
        DriverCaps {
            ioport: take(next),
            irq: take(next),
            notification: take(next),
            endpoint,
        }
    }
}

/// Create the driver's hardware capabilities.
///
/// **The two invocations that make a user-space driver possible**,
/// and neither existed before this milestone could call them:
/// `IOPortControl.Issue` is new (ADR 045) and `IRQControl.Get` was
/// written in M10 and never opened to ring 3.
///
/// The Endpoint is not among them: see [`DriverCaps`].
pub fn issue_hardware(info: &BootInfo, untyped: CPtr, caps: &DriverCaps) -> u64 {
    let own_cnode = slot(info.root_cnode_slot);
    let mut worst = 0u64;

    // The Notification is an ordinary object out of ordinary memory.
    worst = first_error(
        worst,
        crate::retype(
            untyped,
            cap_type::NOTIFICATION,
            0,
            1,
            own_cnode,
            caps.notification,
        ),
    );

    // The port range. The object's 32 bytes come out of the Untyped;
    // the *authority* comes from IOPortControl, and the two are
    // separate for the reason ADR 045 gives — if memory bought
    // ports, every component with a budget would own the machine.
    let r = invoke(
        slot(info.ioport_control_slot).0,
        method::IOPORT_ISSUE,
        COM1_FIRST | (COM1_LAST << 16),
        untyped.0,
        own_cnode.0,
        caps.ioport as u64,
    );
    echo(CHANNEL | STEP_IOPORT_ISSUE, r);
    worst = first_error(worst, r);

    // The interrupt line, the same shape.
    let r = invoke(
        slot(info.irq_control_slot).0,
        method::IRQ_CONTROL_GET,
        COM1_IRQ,
        untyped.0,
        own_cnode.0,
        caps.irq as u64,
    );
    echo(CHANNEL | STEP_IRQ_GET, r);
    worst = first_error(worst, r);

    worst
}

/// Hand the driver its capabilities and bind its Notification.
///
/// # Why the *parent* binds, and why that is not a workaround
///
/// The driver cannot bind its own Notification, and the reason is
/// structural rather than an oversight in either component:
/// `bind_notification` refuses a thread the scheduler is holding
/// (ADR 026), and a thread making a syscall is by definition
/// running. The first version of this had the driver do it and got
/// `ThreadBusy` — which is the check working, not failing.
///
/// Loosening the check was the obvious fix and the wrong one. ADR
/// 026's rule exists because rearranging a thread's IPC state while
/// it may be in a queue is how a dangling reference gets made; a
/// special case for "unless it is binding itself" would be a hole
/// argued for by one caller's convenience.
///
/// So the root task binds, before `Resume`. It holds both
/// capabilities, the driver's thread has never run, and the
/// arrangement reads correctly besides: **what a component may
/// receive is part of how it was built**, decided by whoever built
/// it, in the same breath as its CSpace and its address space.
pub fn wire_driver(info: &BootInfo, component: &Component, caps: &DriverCaps) -> u64 {
    let own_cnode = slot(info.root_cnode_slot);
    let child = slot(component.cnode);
    let mut worst = 0u64;

    // Bind first, while the thread is still unstarted. After
    // `Resume` this would be refused, and the driver could never do
    // it for itself.
    worst = first_error(
        worst,
        invoke(
            slot(caps.notification).0,
            method::NOTIFICATION_BIND,
            slot(component.tcb).0,
            0,
            0,
            0,
        ),
    );

    // `Copy` for all four: none of them wants a badge.
    //
    // The Endpoint is the interesting one. It is copied **unbadged**
    // into the driver, and *minted with a badge* into each client —
    // which is what lets the driver tell its clients apart without
    // trusting them, and lets it receive on a capability that
    // carries no identity of its own (ADR 037).
    // Four, not five. An earlier version also handed over the
    // driver's own TCB, for a `Notification.Bind` the driver could
    // never have made — and a capability held for no reason is
    // authority waiting to be misused. A driver that can name its
    // own TCB can suspend itself; nothing here needs to.
    for (source, dest) in [
        (caps.endpoint, DRIVER_ENDPOINT),
        (caps.ioport, DRIVER_IOPORT),
        (caps.irq, DRIVER_IRQ),
        (caps.notification, DRIVER_NOTIFICATION),
    ] {
        worst = first_error(
            worst,
            invoke(
                own_cnode.0,
                method::COPY,
                source as u64,
                child.0,
                dest as u64,
                0,
            ),
        );
    }

    worst
}

/// Give the client a badged Endpoint and start it.
pub fn wire_client(info: &BootInfo, client: &Component, endpoint: u32) -> u64 {
    let own_cnode = slot(info.root_cnode_slot);

    // **Minted, not copied.** The badge is the client's identity and
    // the root task chooses it; the client cannot change it, cannot
    // read it, and cannot forge another component's. That is the
    // whole mechanism (ADR 037), and it is why the driver can trust
    // what it sees in `Recv`.
    invoke(
        own_cnode.0,
        method::MINT,
        endpoint as u64,
        slot(client.cnode).0,
        CLIENT_ENDPOINT as u64,
        (CLIENT_ID << 16) as u64,
    )
}

/// Block until the driver is serving.
///
/// # Why a message and not a delay
///
/// `Resume` makes a thread runnable; it does not run it. Something
/// that wants to act on a started driver — `driver-dies` wants to
/// kill one — has to know the difference, and the first version of
/// this did not: it called `Echo` 64 times on the theory that
/// entering the kernel gives the scheduler a chance to switch.
///
/// It does, and the scheduler kept choosing this thread. 64 `Echo`
/// calls are a few microseconds, far inside one timer tick, so the
/// loop returned having yielded nothing. The scenario then revoked
/// a driver that had never executed an instruction, and the kernel
/// said so: *"released after 0 access(es)"*, with an empty step
/// dump.
///
/// Counting syscalls was the same error as counting milliseconds
/// (ADR 023, K8) wearing a disguise — it looked like an event
/// because a syscall is an event, but it was not *the* event.
///
/// **The event is the driver receiving a message.** `Send` is a
/// rendezvous (ADR 033): it does not return until a receiver takes
/// what was sent, and the driver receives only from inside its
/// serve loop, which it reaches only after configuring the device
/// and claiming its line. So a returned `Send` means precisely the
/// thing the caller needs to know, and it means it without a
/// timeout, a retry or a guess about speed.
///
/// If the driver never starts, this blocks — and that is the right
/// shape. The kernel's own deadline is still running and reports
/// *"the driver never started, so killing it would prove nothing"*,
/// which is a better message than any this component could produce.
pub fn ping_driver(endpoint: u32) -> u64 {
    invoke(slot(endpoint).0, method::SEND, CMD_PING << 8, 0, 0, 0)
}

/// The driver's image, for the caller to pass to `setup_component`.
pub fn driver_image() -> &'static [u8] {
    DRIVER_IMAGE
}

/// How many code pages the driver needs.
pub fn driver_code_pages() -> u32 {
    DRIVER_CODE_PAGES
}

/// The program the **client** runs.
///
/// Hand-assembled, like M12's children: it is three invocations long
/// and a second crate for it would be more build machinery than
/// code. The driver is a real crate because it is a real program —
/// it has a loop, a state machine and device knowledge.
///
/// Writes "OK" one byte at a time over the Endpoint, then sends
/// `FINISH`. Each `Send` blocks until the driver receives, so
/// reaching the end means every byte was taken.
pub fn client_program() -> Code {
    let mut code = Code::new();

    let ep = (CLIENT_LEAF as u64) << 16 | (CLIENT_ENDPOINT as u64);

    // **Criterion 2.** This component holds exactly one capability:
    // a badged Endpoint. No IOPort, no device memory, no way to name
    // the driver or the root task. The only path from here to the
    // wire runs through the driver's `IOPort.Out8`, and the kernel
    // never looks at the data.
    //
    // Recognisable bytes, because the test looks for them in QEMU's
    // serial output and a single letter could have come from
    // anywhere.
    for byte in *b"[M13]" {
        code.load(crate::Reg::Rdi, ep)
            .load(crate::Reg::Rsi, method::SEND)
            // **The label carries the request** (ADR 047). The badge
            // cannot: it is fixed when the capability is minted, so
            // a client that put its payload there could send exactly
            // one distinct value for as long as it held the
            // capability. That was the first draft of this protocol
            // and it did not survive being written down.
            .load(crate::Reg::Rdx, (CMD_WRITE << 8) | byte as u64)
            .load(crate::Reg::R10, 0)
            .load(crate::Reg::R8, 0)
            .load(crate::Reg::R9, 0)
            .syscall();
    }

    code.load(crate::Reg::Rdi, ep)
        .load(crate::Reg::Rsi, method::SEND)
        .load(crate::Reg::Rdx, CMD_FINISH << 8)
        .load(crate::Reg::R10, 0)
        .load(crate::Reg::R8, 0)
        .load(crate::Reg::R9, 0)
        .syscall()
        .spin();

    code
}

/// The program a **hostile** component runs.
///
/// It holds an Endpoint and nothing else — no IOPort, no
/// IRQHandler, no IOPortControl, no IRQControl. It tries to reach
/// the hardware anyway, three ways, and reports what each attempt
/// returned.
///
/// Every one must fail, and the *reason* must be right: these come
/// back `EMPTY_SLOT` or `WRONG_TYPE` because the capability is not
/// there, not because some check inside the kernel declined. A
/// component cannot name what it does not hold (ADR 018), which is
/// isolation by construction rather than by enforcement.
pub fn hostile_program() -> Code {
    let mut code = Code::new();

    // Slot 2 in its CSpace is empty: the root task put nothing there.
    let absent = (CLIENT_LEAF as u64) << 16 | 2u64;

    // 1. Write to COM1's data register directly.
    code.load(crate::Reg::Rdi, absent)
        .load(crate::Reg::Rsi, method::IOPORT_OUT8)
        .load(crate::Reg::Rdx, COM1_FIRST)
        .load(crate::Reg::R10, b'X' as u64)
        .load(crate::Reg::R8, 0)
        .load(crate::Reg::R9, 0)
        .syscall();

    code.emit(&[0x48, 0x89, 0xc2]) // mov rdx, rax
        .load(crate::Reg::Rdi, DRIVER_CHANNEL | STEP_HOSTILE)
        .load(crate::Reg::Rsi, method::ECHO)
        .syscall();

    // 2. Claim IRQ 4 for itself.
    code.load(crate::Reg::Rdi, absent)
        .load(crate::Reg::Rsi, method::IRQ_CONTROL_GET)
        .load(crate::Reg::Rdx, COM1_IRQ)
        .load(crate::Reg::R10, 0)
        .load(crate::Reg::R8, 0)
        .load(crate::Reg::R9, 0)
        .syscall();

    code.emit(&[0x48, 0x89, 0xc2])
        .load(crate::Reg::Rdi, DRIVER_CHANNEL | (STEP_HOSTILE + 1))
        .load(crate::Reg::Rsi, method::ECHO)
        .syscall();

    // 3. Issue itself a port range.
    code.load(crate::Reg::Rdi, absent)
        .load(crate::Reg::Rsi, method::IOPORT_ISSUE)
        .load(crate::Reg::Rdx, COM1_FIRST | (COM1_LAST << 16))
        .load(crate::Reg::R10, 0)
        .load(crate::Reg::R8, 0)
        .load(crate::Reg::R9, 0)
        .syscall();

    code.emit(&[0x48, 0x89, 0xc2])
        .load(crate::Reg::Rdi, DRIVER_CHANNEL | (STEP_HOSTILE + 2))
        .load(crate::Reg::Rsi, method::ECHO)
        .syscall()
        .spin();

    code
}

// There are deliberately no `write_badge`/`finish_badge` helpers.
//
// An earlier draft had them, and they encoded a protocol that could
// not work: the payload in the badge. A badge is attached when a
// capability is minted and the sender cannot vary it (ADR 037), so
// a client whose request lived there could make exactly one distinct
// request for as long as it held the capability.
//
// The mistake is worth leaving a note about because it came from
// taking the badge's best property — the sender does not choose it —
// and trying to use it as a channel the sender chooses. ADR 047 now
// says it plainly: what the client can choose is data, what it
// cannot is identity.
