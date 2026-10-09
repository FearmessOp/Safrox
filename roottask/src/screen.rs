// SPDX-License-Identifier: MPL-2.0
//! Building the screen component, from CPL 3 (M14).
//!
//! **The second device, and the point is what it did not need.**
//! M13's serial driver required a new capability type — ADR 045's
//! IOPort — because x86 ports are not in any address space and no
//! page table can describe them. The framebuffer is memory, so it
//! needs nothing: a device Untyped becomes Frames, the Frames are
//! mapped, and the component writes.
//!
//! Every piece of that has existed since M5 and M11. What was
//! missing was a device Untyped naming a real device (ADR 050), and
//! finding that missing was most of this milestone.
//!
//! What the screen component gets:
//!
//! | capability | why |
//! |------------|-----|
//! | its own VSpace | to map the framebuffer into |
//! | PageTables | the levels that mapping needs |
//! | device Untyped(s) | the framebuffer's physical pages |
//! | an Endpoint | to say it has finished |
//!
//! It holds **no ordinary Untyped**, which is the interesting
//! omission: the only memory it can allocate from is device memory,
//! and device memory may only become Frames. A component that wanted
//! a thread of its own could not have one — which is what
//! `fb-device-untyped-misuse` demonstrates by asking.

use safrox_abi as abi;
use safrox_abi::{cap_type, channel, method, BootInfo, CPtr};

use crate::driver::{retype_component_sized, Component};
use crate::{echo, first_error, invoke, slot, take, take_run, CHANNEL};

/// The screen component's flat image, built by
/// `scripts/build-screen.sh`.
static SCREEN_IMAGE: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../build/screen.bin"));

/// Byte offset of `SCREEN_GEOMETRY` within that image.
///
/// **Read from the symbol table at build time**, not hard-coded. The
/// build script finds the symbol and writes the number here; a fixed
/// offset would break the first time a line moved above the struct,
/// and break silently — the component would read its magic out of
/// the middle of an instruction.
const GEOMETRY_OFFSET: usize = include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../build/screen-geometry-offset"
));

/// Pages the root task maps for the screen component's code.
///
/// Four, matching the ASSERT in `screen/link.ld` and the check in
/// `scripts/build-screen.sh`. Three places on purpose: a change has
/// to be made three times deliberately rather than once by accident.
const SCREEN_CODE_PAGES: u32 = 4;

// ---- Report steps ------------------------------------------------------

/// The framebuffer's geometry, as BootInfo reported it.
pub const STEP_FB_GEOMETRY: u64 = 40;
/// The screen component's objects were created.
pub const STEP_SCREEN_RETYPE: u64 = 41;
/// Its address space and CSpace were built.
pub const STEP_SCREEN_SETUP: u64 = 42;
/// The framebuffer's capabilities were handed over.
pub const STEP_SCREEN_WIRED: u64 = 43;
/// It was started.
pub const STEP_SCREEN_RESUME: u64 = 44;
/// It reported finishing, over the Endpoint.
pub const STEP_SCREEN_DONE: u64 = 45;
/// A component with no framebuffer capability tried to map it.
pub const STEP_NO_CAP: u64 = 46;

// ---- The screen component's CSpace -------------------------------------
//
// The other half of the constants in `screen/src/main.rs`. Two files
// that must agree, unchecked, as with the serial driver — and named
// here as a weakness rather than hidden.

const SCREEN_LEAF: u32 = 0;
const SCREEN_VSPACE: u32 = 1;
const SCREEN_ENDPOINT: u32 = 2;
/// First of up to [`MAX_FB_UNTYPED`] device Untypeds.
const SCREEN_FB_UNTYPED_FIRST: u32 = 3;
/// First of the PageTables it maps the framebuffer through.
///
/// 16, matching `SLOT_TABLES_FIRST` in `screen/src/main.rs`. It was
/// 8, which collided with the device Untypeds the moment there were
/// more than five of them.
const SCREEN_TABLES_FIRST: u32 = 16;

/// How many device Untypeds the component may be given.
///
/// **Eight, and five was an arithmetic error.** A framebuffer is
/// carved into `2^k` blocks at `2^k` boundaries (ADR 020), so a size
/// that is not a power of two becomes one block per set bit. QEMU's
/// 1280x800x32 screen is 4 096 000 bytes and makes **six** of them;
/// five covered 992 of its 1000 pages and the component reported
/// `NotEnoughMemory` for the last eight.
///
/// This is a ceiling rather than a count: [`wire_screen`] hands over
/// as many as exist. Copying a slot past the last device Untyped
/// would give this component **ordinary RAM**, and "it holds no
/// ordinary Untyped" is exactly what `fb-device-untyped-misuse`
/// depends on.
const MAX_FB_UNTYPED: u32 = 8;

/// How many PageTables the component needs.
///
/// One per 2 MiB of framebuffer, and **no upper levels**: the root
/// task already mapped this component's code at 4 MiB, which created
/// a PDPT spanning 512 GiB and a PD spanning 1 GiB. The framebuffer
/// at 16 MiB is underneath both. Only the level-1 tables are narrow
/// enough to be missing (ADR 027).
///
/// Eight covers a 16 MiB screen, which is more than the device
/// Untypeds above can describe.
const SCREEN_TABLES: u32 = 8;

/// Radix of the screen component's CNode: 2048 slots.
///
/// **Every other component gets radix 6, and this one cannot.** A
/// 1280x800x32 framebuffer is 1000 pages and each page needs a Frame
/// capability in a slot of its own, so the CSpace has to hold a
/// thousand of them above `SLOT_FRAMES_FIRST`.
///
/// With 64 slots the component mapped 47 pages and the 48th retype
/// was refused with `CPtrOutOfRange` — which its retry loop then
/// mistook for a spent Untyped. Two bugs, one misleading symptom.
///
/// 2048 slots is 64 KiB of CNode, paid once by a component that owns
/// a four-megabyte device.
const SCREEN_CNODE_RADIX: u64 = 11;

// Where the component's code and stack go is `driver::setup_component`'s
// business, not this module's: it is the same 4 MiB and 5 MiB every
// component gets, and naming them again here would be two constants
// that have to agree.

// ---- What the root task holds ------------------------------------------

/// Slots the screen component needs, beyond an ordinary component's.
#[derive(Clone, Copy)]
pub struct ScreenCaps {
    /// The Endpoint it reports completion on.
    pub endpoint: u32,
    /// First of the PageTables it will map through.
    pub tables: u32,
}

impl ScreenCaps {
    pub fn allocate(next: &mut u32) -> ScreenCaps {
        ScreenCaps {
            endpoint: take(next),
            tables: take_run(next, SCREEN_TABLES),
        }
    }
}

/// The framebuffer's geometry, read out of BootInfo.
#[derive(Clone, Copy)]
pub struct Framebuffer {
    pub untyped_slot: u32,
    /// How many consecutive device Untypeds describe it.
    ///
    /// **Counted from the descriptor array, not assumed.** BootInfo
    /// names the first slot and says they are consecutive (ADR 049);
    /// how many there are depends on how the screen's size
    /// decomposes into `2^k` blocks, which is a property of the
    /// machine rather than a constant.
    ///
    /// Guessing is what `MAX_FB_UNTYPED` used to do, and it guessed
    /// five where the answer was six. Worse, a guess that is too
    /// *high* hands the component the slot after the last device
    /// Untyped — which is ordinary RAM, and would silently give a
    /// screen driver a memory budget.
    pub untyped_count: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
}

// There is deliberately no `address` field.
//
// BootInfo carries one and the root task does not read it, which
// the compiler pointed out when the field sat here unused with a
// comment claiming it was "for reporting".
//
// Deleting it rather than finding a use for it is the honest fix,
// and the absence states the milestone's claim better than the
// comment did: **the root task cannot reach the framebuffer and
// does not need to know where it is.** Mapping needs a Frame, a
// Frame needs a retype, and a retype needs the Untyped named by
// `untyped_slot`. The address is a number; the slot is the
// authority (ADR 050).
//
// The kernel's test reads the address from its own copy of
// Limine's answer, which is where a check on it belongs.

impl Framebuffer {
    /// Read it from BootInfo, or `None` if there is no screen.
    ///
    /// A zero slot means the machine has none (ADR 049), which is an
    /// ordinary condition — every milestone up to M13 ran headless.
    pub fn from_bootinfo(info: &BootInfo, untypeds: &[abi::UntypedDesc]) -> Option<Framebuffer> {
        if info.framebuffer_untyped_slot == 0 {
            return None;
        }

        // How many device Untypeds follow the first, counted by
        // walking until one is not device memory.
        //
        // The descriptors are indexed from zero while the slot is a
        // CSpace index offset by `untyped_first_slot`; subtracting
        // converts between them. Bounded by the array's length, so
        // a malformed BootInfo stops the count rather than the
        // component.
        let first = info
            .framebuffer_untyped_slot
            .saturating_sub(info.untyped_first_slot) as usize;

        let mut count = 0u32;
        while first + (count as usize) < untypeds.len()
            && untypeds[first + count as usize].is_device != 0
        {
            count += 1;
        }

        Some(Framebuffer {
            untyped_slot: info.framebuffer_untyped_slot,
            untyped_count: count,
            width: info.framebuffer_width,
            height: info.framebuffer_height,
            pitch: info.framebuffer_pitch,
            bpp: info.framebuffer_bpp,
        })
    }

    // There is deliberately no `size()`.
    //
    // One existed, and its only caller was a check that asked
    // whether the screen fitted in `MAX_FB_UNTYPED * 4 MiB` --
    // which was the wrong question twice over. It compared a *size*
    // against a *capacity* when what runs out is **slots**, and it
    // computed that capacity from an assumption about how a region
    // decomposes into power-of-two blocks (ADR 020) rather than
    // from how it actually did.
    //
    // The real answer is `untyped_count`, which is counted from the
    // descriptor array. With that, the size is nothing this
    // component needs: it does not map the framebuffer and does not
    // compute an address in it. The screen component does, from the
    // geometry it is patched with.
    //
    // Deleted rather than kept behind an `#[allow(dead_code)]`,
    // because an unused accessor is a claim that somebody checks
    // it -- and the same reasoning already removed the `address`
    // field above.
}

// ---- Building it -------------------------------------------------------

/// Create the screen component's objects.
///
/// Reuses `driver::retype_component`, which is not laziness: the
/// positive and negative scenarios must build the component the same
/// way, and a second construction path would test itself.
pub fn retype_screen(
    untyped: CPtr,
    own_cnode: CPtr,
    component: &Component,
    caps: &ScreenCaps,
) -> u64 {
    let mut worst = retype_component_sized(
        untyped,
        own_cnode,
        component,
        SCREEN_CODE_PAGES,
        SCREEN_CNODE_RADIX,
    );

    worst = first_error(
        worst,
        crate::retype(untyped, cap_type::ENDPOINT, 0, 1, own_cnode, caps.endpoint),
    );

    // The PageTables the component maps the framebuffer through.
    //
    // **Created by the root task, used by the component.** The
    // component could not make them: it holds no ordinary Untyped,
    // and a PageTable is a kernel object that device memory cannot
    // become. So the parent supplies the structure and the child
    // decides what goes in it — which is ADR 043's division, met
    // here from the other side.
    worst = first_error(
        worst,
        crate::retype(
            untyped,
            cap_type::PAGE_TABLE,
            0,
            SCREEN_TABLES as u64,
            own_cnode,
            caps.tables,
        ),
    );

    worst
}

/// Write the geometry into one page of the component's image.
///
/// Called by [`crate::driver::load_image`] for each code page, with
/// the page number and a pointer to that page mapped writable in
/// *this* component's address space.
///
/// # Why the image is patched rather than told
///
/// The screen component has no BootInfo. Giving it one means a
/// second bootstrap protocol for a single component; sending the
/// geometry over its Endpoint means it cannot know its own shape
/// until after it has started, so it must handle not knowing.
///
/// The geometry is fixed for the component's whole life. Writing it
/// in before the image runs matches that, and costs six stores.
///
/// # Why there is no buffer
///
/// The first version copied the image into a 16 KiB array, patched
/// that, and loaded it. The root task's stack is **one page**, so it
/// faulted at `0x4fffb0` -- sixteen bytes below the stack's base,
/// with `cr2 == rsp`, which is what a stack overflow looks like when
/// there is a guard page under it.
///
/// Patching where the copy already happens needs no buffer at all.
/// `load_image` maps each page writable, copies into it, and unmaps;
/// this runs in that window.
///
/// The magic word is what makes a patch that did not happen visible.
/// Without it an unpatched component reads a zero width, draws
/// nothing, verifies nothing, and reports success.
fn patch_geometry(page: u32, base: *mut u8, fb: &Framebuffer, scenario: u64) {
    // Six u64s: magic, width, height, pitch, bpp, scenario, laid out
    // by `#[repr(C)]` in screen/src/main.rs.
    let fields: [u64; 6] = [
        // "SafrFB01", the constant the component checks.
        0x5361_6672_4642_3031,
        fb.width as u64,
        fb.height as u64,
        fb.pitch as u64,
        fb.bpp as u64,
        scenario,
    ];

    const PAGE: usize = 4096;
    let page_start = page as usize * PAGE;

    for (index, value) in fields.iter().enumerate() {
        let at = GEOMETRY_OFFSET + index * 8;

        // Is this field on this page? The struct is 48 bytes and
        // aligned, so it cannot straddle a page boundary -- but the
        // check is per-field rather than per-struct because that is
        // the version that stays correct if the struct grows.
        if at < page_start || at + 8 > page_start + PAGE {
            continue;
        }

        // SAFETY: `base` points at a full page mapped read-write in
        // this component's address space (`load_image` has just
        // mapped it and has not yet unmapped it). The offset is
        // inside that page by the bounds check above.
        unsafe {
            core::ptr::copy_nonoverlapping(
                value.to_le_bytes().as_ptr(),
                base.add(at - page_start),
                8,
            );
        }
    }
}

/// Give the screen component everything it needs and start it.
///
/// Returns the first error, or zero.
pub fn wire_screen(
    info: &BootInfo,
    component: &Component,
    caps: &ScreenCaps,
    fb: &Framebuffer,
) -> u64 {
    let own_cnode = slot(info.root_cnode_slot);
    let child = slot(component.cnode);
    let mut worst = 0u64;

    // Its own VSpace, so it can map things into itself. **This is
    // the capability that makes the component a driver rather than a
    // program**: holding your own address space is what lets you
    // decide what is in it (ADR 043).
    worst = first_error(
        worst,
        invoke(
            own_cnode.0,
            method::COPY,
            component.vspace as u64,
            child.0,
            SCREEN_VSPACE as u64,
            0,
        ),
    );

    worst = first_error(
        worst,
        invoke(
            own_cnode.0,
            method::COPY,
            caps.endpoint as u64,
            child.0,
            SCREEN_ENDPOINT as u64,
            0,
        ),
    );

    // The framebuffer itself: up to five device Untypeds, in
    // consecutive slots.
    //
    // **Copied, not minted.** A badge would overwrite the size_bits
    // field — the same hazard M12 found with CNodes (ADR 037), and
    // an Untyped whose size became a badge would carve objects out
    // of a window of the wrong length.
    // **As many as exist, never more** (`fb.untyped_count`).
    //
    // The count comes from the descriptor array rather than from
    // this constant, and the difference matters: the slot after the
    // last device Untyped holds **ordinary RAM**, and copying it
    // here would quietly give the screen component a budget it is
    // not supposed to have. `fb-device-untyped-misuse` asserts that
    // it cannot make a TCB; with one ordinary Untyped it could, and
    // the scenario would fail for a reason that looked like a kernel
    // bug.
    let handed = fb.untyped_count.min(MAX_FB_UNTYPED);
    for index in 0..handed {
        worst = first_error(
            worst,
            invoke(
                own_cnode.0,
                method::COPY,
                (fb.untyped_slot + index) as u64,
                child.0,
                (SCREEN_FB_UNTYPED_FIRST + index) as u64,
                0,
            ),
        );
    }

    for index in 0..SCREEN_TABLES {
        worst = first_error(
            worst,
            invoke(
                own_cnode.0,
                method::COPY,
                (caps.tables + index) as u64,
                child.0,
                (SCREEN_TABLES_FIRST + index) as u64,
                0,
            ),
        );
    }

    worst
}

/// Build the screen component and start it.
///
/// `scenario` is what the component itself should do: draw, or one
/// of the two misbehaviours the negatives need.
pub fn build_and_start(
    info: &BootInfo,
    untyped: CPtr,
    next: &mut u32,
    fb: &Framebuffer,
    scenario: u64,
) -> u64 {
    let own_cnode = slot(info.root_cnode_slot);

    // What the root task was told, reported before anything is
    // built. The kernel's test compares this against its own copy:
    // two routes from one source (ADR 049), and a mismatch means
    // the BootInfo fields were read wrongly here.
    echo(
        CHANNEL | STEP_FB_GEOMETRY,
        (fb.width as u64) | ((fb.height as u64) << 16) | ((fb.pitch as u64) << 32),
    );

    // **The screen must fit the Untypeds being handed over.**
    //
    // Five device Untypeds, each at most 4 MiB (boot's
    // `MAX_ROOT_UNTYPED_BITS`). A larger framebuffer would be
    // partly unreachable: the component would map what it could,
    // draw on a fraction of the screen, verify that fraction, and
    // report success.
    //
    // Refused here rather than discovered there. The component
    // notices too and says `NotEnoughMemory`, which is true and
    // explains nothing.
    if fb.untyped_count == 0 || fb.untyped_count > MAX_FB_UNTYPED {
        echo(
            CHANNEL | STEP_FB_GEOMETRY,
            abi::ERROR_FLAG | abi::error::NOT_ENOUGH_MEMORY,
        );
        return abi::ERROR_FLAG | abi::error::NOT_ENOUGH_MEMORY;
    }

    let component = Component::allocate(next, SCREEN_CODE_PAGES);
    let caps = ScreenCaps::allocate(next);

    let worst = retype_screen(untyped, own_cnode, &component, &caps);
    echo(CHANNEL | STEP_SCREEN_RETYPE, worst);
    if worst != 0 {
        return worst;
    }

    // **The geometry must land inside the image.**
    //
    // The offset comes from the build script reading the symbol
    // table, so a mismatch means the two disagree about the layout
    // -- and writing past the end of the image would corrupt
    // whatever page followed rather than failing.
    if GEOMETRY_OFFSET + 48 > SCREEN_IMAGE.len() {
        echo(
            CHANNEL | STEP_SCREEN_SETUP,
            abi::ERROR_FLAG | abi::error::INVALID_ARGUMENT,
        );
        return abi::ERROR_FLAG | abi::error::INVALID_ARGUMENT;
    }

    let worst = crate::driver::setup_component_patched(
        info,
        &component,
        SCREEN_IMAGE,
        SCREEN_CODE_PAGES,
        Some(&|page, base| patch_geometry(page, base, fb, scenario)),
    );
    echo(CHANNEL | STEP_SCREEN_SETUP, worst);
    if worst != 0 {
        return worst;
    }

    let worst = wire_screen(info, &component, &caps, fb);
    echo(CHANNEL | STEP_SCREEN_WIRED, worst);
    if worst != 0 {
        return worst;
    }

    let result = invoke(slot(component.tcb).0, method::RESUME, 0, 0, 0, 0);
    echo(CHANNEL | STEP_SCREEN_RESUME, result);
    if abi::is_error(result) {
        return result;
    }

    // Wait for it, by receiving on the Endpoint it was given.
    //
    // A rendezvous rather than a delay (ADR 023's K8, and the
    // lesson M13 paid for twice): `Recv` returns exactly when the
    // component sends, and blocks otherwise. If the component never
    // finishes, the kernel's own deadline reports it with a better
    // message than anything here could.
    let reply = invoke(slot(caps.endpoint).0, method::RECV, 0, 0, 0, 0);
    echo(CHANNEL | STEP_SCREEN_DONE, reply);

    0
}

/// The program a component with **no framebuffer capability** runs.
///
/// It holds an Endpoint and nothing else — no device Untyped, no
/// PageTables, not even its own VSpace capability. It tries to map
/// the framebuffer anyway and is refused at CPtr resolution: slot 3
/// of its CSpace is empty, so there is nothing to retype a Frame
/// from.
///
/// **The refusal is structural, not enforced.** A component cannot
/// name what it does not hold (ADR 018), so this is the same claim
/// `driver-hostile` made for ports, arriving at a device that *is*
/// memory — where one might have expected a physical address to be
/// enough. It is not: an address is a number, and mapping needs a
/// capability (ADR 050).
pub fn no_cap_program() -> crate::Code {
    let mut code = crate::Code::new();

    // Slot 3 would hold the first device Untyped. The root task put
    // nothing there.
    let absent = (SCREEN_LEAF as u64) << 16 | (SCREEN_FB_UNTYPED_FIRST as u64);

    // Try to retype a Frame out of it.
    code.load(crate::Reg::Rdi, absent)
        .load(crate::Reg::Rsi, method::RETYPE)
        .load(crate::Reg::Rdx, abi::retype_arg(cap_type::FRAME, 0))
        .load(crate::Reg::R10, 1)
        .load(crate::Reg::R8, (SCREEN_LEAF as u64) << 16)
        .load(crate::Reg::R9, 32)
        .syscall();

    code.emit(&[0x48, 0x89, 0xc2]) // mov rdx, rax
        .load(crate::Reg::Rdi, channel::SCREEN | STEP_NO_CAP)
        .load(crate::Reg::Rsi, method::ECHO)
        .syscall()
        .spin();

    code
}
