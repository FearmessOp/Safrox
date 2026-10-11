// SPDX-License-Identifier: MPL-2.0
//! Building the keyboard and console components (M15).
//!
//! **Two components, no new kernel mechanism.** M13 invented the
//! IOPort capability for the serial port; M14 found the first real
//! user of device Untypeds. This milestone invents nothing: the
//! keyboard is M13's pattern pointed at a second device, and the
//! console is M14's mapping pointed at the same framebuffer.
//!
//! That is the claim, and it is checked by what this file does
//! *not* contain — no new method number, no new capability type, no
//! kernel change. `retype_component_sized`, `setup_component_patched`
//! and `issue_hardware`'s shape all come from the two milestones
//! before it.
//!
//! The topology:
//!
//! ```text
//!   keyboard --Endpoint(badge)--> console
//!   IOPort 0x60, IOPort 0x64     6 x device Untyped (framebuffer)
//!   IRQHandler 1 + Notification  PageTables, its own VSpace
//! ```
//!
//! Neither holds an ordinary Untyped, so neither can create objects
//! at run time. Neither holds a capability naming the other's
//! objects, which is what `kbd-isolation` demonstrates from both
//! directions.

use safrox_abi as abi;
use safrox_abi::{cap_type, method, BootInfo, CPtr};

use crate::driver::{retype_component_sized, Component};
use crate::screen::Framebuffer;
use crate::{echo, first_error, invoke, slot, take, take_run, CHANNEL};

/// The keyboard component's flat image.
static KEYBOARD_IMAGE: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../build/keyboard.bin"));

/// The console component's flat image.
static CONSOLE_IMAGE: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../build/console.bin"));

/// Byte offset of `KEYBOARD_SCENARIO` within the keyboard's image.
const KEYBOARD_SCENARIO_OFFSET: usize = include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../build/keyboard-scenario-offset"
));

/// Byte offset of `CONSOLE_GEOMETRY` within the console's image.
const CONSOLE_GEOMETRY_OFFSET: usize = include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../build/console-geometry-offset"
));

/// Pages for the keyboard's code.
///
/// Two, matching `keyboard/link.ld` and `scripts/build-keyboard.sh`.
/// Three places, so a change has to be made three times on purpose.
const KEYBOARD_CODE_PAGES: u32 = 2;

/// Pages for the console's code. Three, same agreement.
const CONSOLE_CODE_PAGES: u32 = 3;

/// Pages for the console's **stack**.
///
/// Four, where every other component gets one. The character grid
/// is 160x50 bytes in `serve`'s locals and does not fit in a page.
const CONSOLE_STACK_PAGES: u32 = 4;

// ---- Report steps ------------------------------------------------------

/// The keyboard's objects were created.
pub const STEP_KBD_RETYPE: u64 = 60;
/// Its address space and CSpace were built.
pub const STEP_KBD_SETUP: u64 = 61;
/// `IOPortControl.Issue` for 0x60.
pub const STEP_KBD_IOPORT_DATA: u64 = 62;
/// `IOPortControl.Issue` for 0x64.
pub const STEP_KBD_IOPORT_STATUS: u64 = 63;
/// `IRQControl.Get` for line 1.
pub const STEP_KBD_IRQ: u64 = 64;
/// Its capabilities were handed over.
pub const STEP_KBD_WIRED: u64 = 65;
/// It was started.
pub const STEP_KBD_RESUME: u64 = 66;
/// The console's objects were created.
pub const STEP_CON_RETYPE: u64 = 67;
/// Its address space and CSpace were built.
pub const STEP_CON_SETUP: u64 = 68;
/// Its capabilities were handed over.
pub const STEP_CON_WIRED: u64 = 69;
/// It was started.
pub const STEP_CON_RESUME: u64 = 70;
/// The console answered a ping: it is receiving.
pub const STEP_CON_READY: u64 = 71;
/// The keyboard's capabilities were revoked; how many died.
pub const STEP_KBD_REVOKED: u64 = 72;
/// A second `IOPortControl.Issue` for 0x60 was refused.
pub const STEP_SECOND_ISSUE: u64 = 73;
/// A second `IRQControl.Get` for line 1 was refused.
pub const STEP_SECOND_IRQ: u64 = 74;

// ---- The keyboard's CSpace ---------------------------------------------
//
// The other half of the constants in `keyboard/src/main.rs`.
//
// Slot 0 is the component's own LEAF and is deliberately **not**
// named here. The root task never addresses it: every invocation
// below goes through `slot(component.cnode)`, which is the root
// task's name for the child's CNode, not the child's name for
// itself. A constant for it would be a name nothing can use.

const KBD_ENDPOINT: u32 = 1;
const KBD_IOPORT_DATA: u32 = 2;
const KBD_IOPORT_STATUS: u32 = 3;
const KBD_IRQ: u32 = 4;
const KBD_NOTIFICATION: u32 = 5;

// ---- The console's CSpace ----------------------------------------------
//
// The screen component's layout, slot for slot, because the console
// maps the framebuffer the same way.

const CON_VSPACE: u32 = 1;
const CON_ENDPOINT: u32 = 2;
const CON_FB_UNTYPED_FIRST: u32 = 3;
const CON_TABLES_FIRST: u32 = 16;

/// How many device Untypeds the console may be given.
const MAX_FB_UNTYPED: u32 = 8;
/// How many PageTables it needs. One per 2 MiB, no upper levels.
const CONSOLE_TABLES: u32 = 8;

/// Radix of the console's CNode: 2048 slots.
///
/// The screen's figure, for the screen's reason — a 1280x800
/// framebuffer is 1000 Frames and each needs a slot.
const CONSOLE_CNODE_RADIX: u64 = 11;

/// Radix of the keyboard's CNode: 64 slots. It holds six.
const KEYBOARD_CNODE_RADIX: u64 = 6;

// ---- The hardware ------------------------------------------------------

/// The i8042's data register.
///
/// **Two single-port capabilities rather than one `[0x60,0x64]`
/// range.** The range would also carry 0x61 — the PC speaker and the
/// NMI status register — and 0x62/0x63. `MAX_RANGES` is 16 and two
/// are in use, so the narrower authority costs nothing (ADR 053).
const PORT_DATA: u64 = 0x60;
/// The i8042's status and command register.
const PORT_STATUS: u64 = 0x64;

/// The line the keyboard raises. Fixed by the IBM PC.
const KEYBOARD_IRQ: u64 = 1;

/// The keyboard's identity, as the root task assigns it.
///
/// The console reads this out of the badge and **the keyboard
/// cannot choose it** — it is attached when this component mints the
/// capability (ADR 037). Named here and in `console/src/main.rs`.
const BADGE_KEYBOARD: u64 = 0xB0A2D;

/// The console's ping label: "are you receiving yet?"
const CMD_PING: u64 = 0xFFFF;

/// `"SafrCN01"`, the magic the console checks.
const CONSOLE_GEOMETRY_MAGIC: u64 = 0x5361_6672_434E_3031;

/// Values for the console's patched `scenario` field.
const CONSOLE_SCENARIO_ISOLATION: u64 = 1;
/// Values for the keyboard's patched scenario word.
const KEYBOARD_SCENARIO_ISOLATION: u64 = 0x1501_A710;

// ---- What the root task holds ------------------------------------------

/// Slots the keyboard needs beyond an ordinary component's.
#[derive(Clone, Copy)]
pub struct KeyboardCaps {
    /// IOPort for 0x60.
    pub ioport_data: u32,
    /// IOPort for 0x64.
    pub ioport_status: u32,
    /// IRQHandler for line 1.
    pub irq: u32,
    /// The Notification that line delivers to.
    pub notification: u32,
    /// The Endpoint it sends on. **The root task's**, shared with
    /// the console — the same reasoning `DriverCaps` carries: a
    /// service's name must outlive any one driver.
    pub endpoint: u32,
}

impl KeyboardCaps {
    pub fn allocate(next: &mut u32, endpoint: u32) -> KeyboardCaps {
        KeyboardCaps {
            ioport_data: take(next),
            ioport_status: take(next),
            irq: take(next),
            notification: take(next),
            endpoint,
        }
    }
}

/// Slots the console needs beyond an ordinary component's.
#[derive(Clone, Copy)]
pub struct ConsoleCaps {
    /// First of the PageTables it maps the framebuffer through.
    pub tables: u32,
    /// The Endpoint it receives on.
    pub endpoint: u32,
}

impl ConsoleCaps {
    pub fn allocate(next: &mut u32, endpoint: u32) -> ConsoleCaps {
        ConsoleCaps {
            tables: take_run(next, CONSOLE_TABLES),
            endpoint,
        }
    }
}

// ---- Patching ----------------------------------------------------------

/// Write the scenario word into one page of the keyboard's image.
fn patch_keyboard(page: u32, base: *mut u8, scenario: u64) {
    const PAGE: usize = 4096;
    let page_start = page as usize * PAGE;
    let at = KEYBOARD_SCENARIO_OFFSET;

    if at < page_start || at + 8 > page_start + PAGE {
        return;
    }

    // SAFETY: `base` points at a full page mapped read-write in this
    // component's address space (`load_image` has just mapped it and
    // has not yet unmapped it). The offset is inside that page by
    // the bounds check above.
    unsafe {
        core::ptr::copy_nonoverlapping(
            scenario.to_le_bytes().as_ptr(),
            base.add(at - page_start),
            8,
        );
    }
}

/// Write the geometry into one page of the console's image.
///
/// The screen component's mechanism (ADR 049), and the magic is what
/// makes a patch that did not happen visible: without it the console
/// would read a zero width, draw nothing and report success.
fn patch_console(page: u32, base: *mut u8, fb: &Framebuffer, scenario: u64) {
    let fields: [u64; 6] = [
        CONSOLE_GEOMETRY_MAGIC,
        fb.width as u64,
        fb.height as u64,
        fb.pitch as u64,
        fb.bpp as u64,
        scenario,
    ];

    const PAGE: usize = 4096;
    let page_start = page as usize * PAGE;

    for (index, value) in fields.iter().enumerate() {
        let at = CONSOLE_GEOMETRY_OFFSET + index * 8;
        if at < page_start || at + 8 > page_start + PAGE {
            continue;
        }
        // SAFETY: As `patch_keyboard` — a writable page, bounds
        // checked immediately above.
        unsafe {
            core::ptr::copy_nonoverlapping(
                value.to_le_bytes().as_ptr(),
                base.add(at - page_start),
                8,
            );
        }
    }
}

// ---- Building the console ----------------------------------------------

/// Create the console's objects.
pub fn retype_console(
    untyped: CPtr,
    own_cnode: CPtr,
    component: &Component,
    caps: &ConsoleCaps,
) -> u64 {
    let mut worst = retype_component_sized(
        untyped,
        own_cnode,
        component,
        CONSOLE_CODE_PAGES,
        CONSOLE_CNODE_RADIX,
    );

    // The PageTables it maps the framebuffer through. Created here
    // because the component holds no ordinary Untyped and a
    // PageTable is not something device memory can become — the
    // parent supplies the structure, the child decides its contents
    // (ADR 043).
    worst = first_error(
        worst,
        crate::retype(
            untyped,
            cap_type::PAGE_TABLE,
            0,
            CONSOLE_TABLES as u64,
            own_cnode,
            caps.tables,
        ),
    );

    worst
}

/// Hand the console its capabilities.
pub fn wire_console(
    info: &BootInfo,
    component: &Component,
    caps: &ConsoleCaps,
    fb: &Framebuffer,
) -> u64 {
    let own_cnode = slot(info.root_cnode_slot);
    let child = slot(component.cnode);
    let mut worst = 0u64;

    // Its own VSpace: what makes it able to decide what is in its
    // address space (ADR 043).
    worst = first_error(
        worst,
        invoke(
            own_cnode.0,
            method::COPY,
            component.vspace as u64,
            child.0,
            CON_VSPACE as u64,
            0,
        ),
    );

    // The Endpoint, **unbadged**. It receives; the keyboard's copy
    // is minted with a badge. That asymmetry is the whole of ADR
    // 037: the receiver learns who sent without trusting anybody,
    // and the sender cannot choose what it is called.
    worst = first_error(
        worst,
        invoke(
            own_cnode.0,
            method::COPY,
            caps.endpoint as u64,
            child.0,
            CON_ENDPOINT as u64,
            0,
        ),
    );

    // The framebuffer: **as many device Untypeds as exist, never
    // more.** The slot after the last one holds ordinary RAM, and
    // copying it would quietly give this component a memory budget
    // it is not supposed to have.
    let handed = fb.untyped_count.min(MAX_FB_UNTYPED);
    for index in 0..handed {
        worst = first_error(
            worst,
            invoke(
                own_cnode.0,
                method::COPY,
                (fb.untyped_slot + index) as u64,
                child.0,
                (CON_FB_UNTYPED_FIRST + index) as u64,
                0,
            ),
        );
    }

    for index in 0..CONSOLE_TABLES {
        worst = first_error(
            worst,
            invoke(
                own_cnode.0,
                method::COPY,
                (caps.tables + index) as u64,
                child.0,
                (CON_TABLES_FIRST + index) as u64,
                0,
            ),
        );
    }

    worst
}

// ---- Building the keyboard ---------------------------------------------

/// Create the keyboard's hardware capabilities.
///
/// The two invocations M13 opened to ring 3, pointed at a second
/// device: `IOPortControl.Issue` twice and `IRQControl.Get` once.
/// **Nothing here is new** — which is the milestone's claim.
pub fn issue_keyboard_hardware(info: &BootInfo, untyped: CPtr, caps: &KeyboardCaps) -> u64 {
    let own_cnode = slot(info.root_cnode_slot);
    let mut worst = 0u64;

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

    // 0x60 alone, and 0x64 alone.
    let r = invoke(
        slot(info.ioport_control_slot).0,
        method::IOPORT_ISSUE,
        PORT_DATA | (PORT_DATA << 16),
        untyped.0,
        own_cnode.0,
        caps.ioport_data as u64,
    );
    echo(CHANNEL | STEP_KBD_IOPORT_DATA, r);
    worst = first_error(worst, r);

    let r = invoke(
        slot(info.ioport_control_slot).0,
        method::IOPORT_ISSUE,
        PORT_STATUS | (PORT_STATUS << 16),
        untyped.0,
        own_cnode.0,
        caps.ioport_status as u64,
    );
    echo(CHANNEL | STEP_KBD_IOPORT_STATUS, r);
    worst = first_error(worst, r);

    let r = invoke(
        slot(info.irq_control_slot).0,
        method::IRQ_CONTROL_GET,
        KEYBOARD_IRQ,
        untyped.0,
        own_cnode.0,
        caps.irq as u64,
    );
    echo(CHANNEL | STEP_KBD_IRQ, r);
    worst = first_error(worst, r);

    worst
}

/// Hand the keyboard its capabilities and bind its Notification.
///
/// The root task binds, before `Resume`, because a thread cannot
/// bind its own: `bind_notification` refuses a thread the scheduler
/// is holding (ADR 026) and a thread making a syscall is running.
/// M13 found this by getting `ThreadBusy`.
pub fn wire_keyboard(info: &BootInfo, component: &Component, caps: &KeyboardCaps) -> u64 {
    let own_cnode = slot(info.root_cnode_slot);
    let child = slot(component.cnode);
    let mut worst = 0u64;

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

    // The Endpoint is **minted with a badge**: the keyboard's
    // identity, which it cannot read, change or forge. Everything
    // else is copied — none of them wants one.
    worst = first_error(
        worst,
        invoke(
            own_cnode.0,
            method::MINT,
            caps.endpoint as u64,
            child.0,
            KBD_ENDPOINT as u64,
            BADGE_KEYBOARD,
        ),
    );

    for (source, dest) in [
        (caps.ioport_data, KBD_IOPORT_DATA),
        (caps.ioport_status, KBD_IOPORT_STATUS),
        (caps.irq, KBD_IRQ),
        (caps.notification, KBD_NOTIFICATION),
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

// ---- The whole thing ---------------------------------------------------

/// What `build_and_start` hands back, so the caller can take the
/// system apart again.
///
/// **The console is not in here**, and its absence is the point.
/// Nothing takes the console apart: `kbd-driver-dies` kills the
/// keyboard precisely so that the console can be seen surviving, and
/// a handle to it would be a handle nobody uses. That the console
/// was built at all is reported through `STEP_CON_RESUME` and
/// `STEP_CON_READY`, which is evidence rather than a capability.
#[derive(Clone, Copy)]
pub struct Built {
    pub keyboard: Component,
    pub keyboard_caps: KeyboardCaps,
    /// The Endpoint the two share.
    ///
    /// **Named here as well as in both caps structures, and that is
    /// not a third copy of the same thing.** The `*Caps` structures
    /// record *what was handed to a component* — the keyboard's
    /// badged mint, the console's unbadged copy. This field records
    /// *what the root task still holds*, which is the distinction
    /// `kill_keyboard` rests on: it revokes everything in
    /// `keyboard_caps` and deliberately leaves this one alone,
    /// because the console is still waiting on it.
    ///
    /// Reaching it through `keyboard_caps` would compile and would
    /// say the wrong thing: that the object belongs to the component
    /// about to be destroyed.
    pub endpoint: u32,
}

/// Build both components, wire them together, and start them.
///
/// **The console first.** `Send` is a rendezvous (ADR 033), so a
/// keyboard that produced a key before the console was receiving
/// would block — not fatal, but it would fold the console's startup
/// into the first key's measured latency.
///
/// That the console *is* receiving is established by a ping, not by
/// a delay: `Send` returns exactly when a receiver takes the
/// message, which is M13's `ping_driver` and ADR 023's K8.
pub fn build_and_start(
    info: &BootInfo,
    untyped: CPtr,
    next: &mut u32,
    fb: &Framebuffer,
    keyboard_scenario: u64,
    console_scenario: u64,
    with_keyboard_hardware: bool,
) -> Result<Built, u64> {
    let own_cnode = slot(info.root_cnode_slot);

    if fb.untyped_count == 0 || fb.untyped_count > MAX_FB_UNTYPED {
        return Err(abi::ERROR_FLAG | abi::error::NOT_ENOUGH_MEMORY);
    }

    // One Endpoint, created once, shared: the keyboard mints a
    // badged copy, the console an unbadged one. It belongs to
    // neither — which is what lets `kbd-driver-dies` revoke the
    // keyboard without the console losing the object it is waiting
    // on.
    let endpoint = take(next);
    let worst = crate::retype(untyped, cap_type::ENDPOINT, 0, 1, own_cnode, endpoint);
    if abi::is_error(worst) {
        return Err(worst);
    }

    // --- The console -------------------------------------------------
    let console = Component::allocate_sized(next, CONSOLE_CODE_PAGES, CONSOLE_STACK_PAGES);
    let console_caps = ConsoleCaps::allocate(next, endpoint);

    let worst = retype_console(untyped, own_cnode, &console, &console_caps);
    echo(CHANNEL | STEP_CON_RETYPE, worst);
    if worst != 0 {
        return Err(worst);
    }

    if CONSOLE_GEOMETRY_OFFSET + 48 > CONSOLE_IMAGE.len() {
        return Err(abi::ERROR_FLAG | abi::error::INVALID_ARGUMENT);
    }

    let worst = crate::driver::setup_component_patched(
        info,
        &console,
        CONSOLE_IMAGE,
        CONSOLE_CODE_PAGES,
        Some(&|page, base| patch_console(page, base, fb, console_scenario)),
    );
    echo(CHANNEL | STEP_CON_SETUP, worst);
    if worst != 0 {
        return Err(worst);
    }

    let worst = wire_console(info, &console, &console_caps, fb);
    echo(CHANNEL | STEP_CON_WIRED, worst);
    if worst != 0 {
        return Err(worst);
    }

    let result = invoke(slot(console.tcb).0, method::RESUME, 0, 0, 0, 0);
    echo(CHANNEL | STEP_CON_RESUME, result);
    if abi::is_error(result) {
        return Err(result);
    }

    // The isolation scenario's console reports and stops rather than
    // serving, so there is nobody to ping.
    if console_scenario != CONSOLE_SCENARIO_ISOLATION {
        // **Blocks until the console is receiving.** Not a delay:
        // `Send` returns when a receiver takes the message, so its
        // return means the console has mapped the framebuffer,
        // cleared the screen and reached its loop.
        let ping = invoke(slot(endpoint).0, method::SEND, CMD_PING, 0, 0, 0);
        echo(CHANNEL | STEP_CON_READY, ping);
    }

    // --- The keyboard ------------------------------------------------
    let keyboard = Component::allocate(next, KEYBOARD_CODE_PAGES);
    let keyboard_caps = KeyboardCaps::allocate(next, endpoint);

    let worst = retype_component_sized(
        untyped,
        own_cnode,
        &keyboard,
        KEYBOARD_CODE_PAGES,
        KEYBOARD_CNODE_RADIX,
    );
    echo(CHANNEL | STEP_KBD_RETYPE, worst);
    if worst != 0 {
        return Err(worst);
    }

    // `kbd-no-cap` skips this: the component is built identically
    // and simply never given the ports or the line. Building it a
    // different way would test the different way.
    if with_keyboard_hardware {
        let worst = issue_keyboard_hardware(info, untyped, &keyboard_caps);
        if worst != 0 {
            return Err(worst);
        }
    }

    let worst = crate::driver::setup_component_patched(
        info,
        &keyboard,
        KEYBOARD_IMAGE,
        KEYBOARD_CODE_PAGES,
        Some(&|page, base| patch_keyboard(page, base, keyboard_scenario)),
    );
    echo(CHANNEL | STEP_KBD_SETUP, worst);
    if worst != 0 {
        return Err(worst);
    }

    let worst = if with_keyboard_hardware {
        wire_keyboard(info, &keyboard, &keyboard_caps)
    } else {
        // Only the Endpoint. No ports, no line, no Notification —
        // so the component reaches an empty slot when it asks the
        // device anything, which is the scenario.
        invoke(
            own_cnode.0,
            method::MINT,
            endpoint as u64,
            slot(keyboard.cnode).0,
            KBD_ENDPOINT as u64,
            BADGE_KEYBOARD,
        )
    };
    echo(CHANNEL | STEP_KBD_WIRED, worst);
    if worst != 0 {
        return Err(worst);
    }

    let result = invoke(slot(keyboard.tcb).0, method::RESUME, 0, 0, 0, 0);
    echo(CHANNEL | STEP_KBD_RESUME, result);
    if abi::is_error(result) {
        return Err(result);
    }

    Ok(Built {
        keyboard,
        keyboard_caps,
        endpoint,
    })
}

/// Kill the keyboard: suspend its thread, then revoke its
/// capabilities.
///
/// Suspend **first**, because a TCB cannot be revoked while the
/// scheduler holds it (ADR 026) — M13's `driver-dies` learned this.
///
/// Returns how many revocations succeeded.
pub fn kill_keyboard(info: &BootInfo, built: &Built) -> u64 {
    let own_cnode = slot(info.root_cnode_slot);
    let mut died = 0u64;

    invoke(slot(built.keyboard.tcb).0, method::TCB_SUSPEND, 0, 0, 0, 0);

    for which in [
        built.keyboard_caps.ioport_data,
        built.keyboard_caps.ioport_status,
        built.keyboard_caps.irq,
        built.keyboard_caps.notification,
    ] {
        let result = invoke(own_cnode.0, method::CNODE_REVOKE, which as u64, 0, 0, 0);
        if !abi::is_error(result) {
            died += 1;
        }
    }

    // **The Endpoint is not revoked.** It is the root task's and the
    // console is still waiting on it; revoking it would take down
    // the component that is supposed to survive, and the scenario's
    // claim is precisely that it does.

    echo(CHANNEL | STEP_KBD_REVOKED, died);
    died
}

/// Check that the keyboard's hardware cannot be claimed twice.
///
/// Both must be refused while the keyboard holds them: an IOPort
/// range that overlaps a live one, and a line that already has an
/// owner. M13 made the same two checks for COM1 and IRQ 4; making
/// them again for a second device is how "the rule is general"
/// stops being an assertion.
pub fn check_exclusive(info: &BootInfo, untyped: CPtr, next: &mut u32) {
    let own_cnode = slot(info.root_cnode_slot);

    let spare = take(next);
    let r = invoke(
        slot(info.ioport_control_slot).0,
        method::IOPORT_ISSUE,
        PORT_DATA | (PORT_DATA << 16),
        untyped.0,
        own_cnode.0,
        spare as u64,
    );
    echo(CHANNEL | STEP_SECOND_ISSUE, r);

    let spare = take(next);
    let r = invoke(
        slot(info.irq_control_slot).0,
        method::IRQ_CONTROL_GET,
        KEYBOARD_IRQ,
        untyped.0,
        own_cnode.0,
        spare as u64,
    );
    echo(CHANNEL | STEP_SECOND_IRQ, r);
}

/// The keyboard scenario value that makes the component try to read
/// the framebuffer's address.
pub fn isolation_scenario() -> u64 {
    KEYBOARD_SCENARIO_ISOLATION
}

/// The console scenario value that makes it try to drive an IOPort.
pub fn console_isolation_scenario() -> u64 {
    CONSOLE_SCENARIO_ISOLATION
}

/// Receive one message on the shared Endpoint.
///
/// Used by `kbd-isolation`, where both components report and stop
/// rather than serving.
pub fn wait_for(endpoint: u32) -> u64 {
    invoke(slot(endpoint).0, method::RECV, 0, 0, 0, 0)
}