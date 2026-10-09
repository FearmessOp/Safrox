// SPDX-License-Identifier: BSD-2-Clause
#![no_std]
#![no_main]

//! The Safrox screen component — the framebuffer, driven from CPL 3
//! (M14, ADR 049, ADR 050, ADR 051).
//!
//! **This milestone adds no kernel mechanism.** M13 moved the serial
//! port out of the kernel and had to invent a capability to do it
//! (ADR 045's IOPort). The framebuffer needs nothing new: a device
//! Untyped becomes Frames, the Frames are mapped, and the component
//! writes to memory. Every piece of that existed in M5 and M11; what
//! was missing was a device Untyped that named a real device.
//!
//! What this component holds:
//!
//! | capability | why |
//! |------------|-----|
//! | device Untyped(s) | the framebuffer's physical pages |
//! | PageTables | to map them into its own address space |
//! | its own VSpace | the address space it maps into |
//! | an Endpoint | so the root task can hear from it |
//!
//! It holds no IOPort, no IRQHandler, no capability naming another
//! component, and **no ordinary Untyped** — so the one thing it can
//! allocate from is device memory, which may only become Frames.
//! That is the restriction `fb-device-untyped-misuse` demonstrates
//! by trying to break it.
//!
//! # Pitch, and why it is not `width * bpp / 8`
//!
//! A scanline is `pitch` bytes. Hardware pads rows to whatever
//! alignment it likes, so the first pixel of row `y` is at
//! `y * pitch`, not at `y * width * bpp / 8`.
//!
//! On QEMU the two are equal, which makes this the easiest mistake
//! in the file to make and the hardest to notice: code that
//! multiplies works perfectly here and corrupts the display on
//! hardware that pads. Every address calculation below goes through
//! [`Geometry::offset`], and the acceptance test checks the row
//! boundary specifically.

#![allow(clippy::upper_case_acronyms)]

use safrox_abi as abi;
use safrox_abi::{channel, method, perm};

// ---- Entry -------------------------------------------------------------

/// Where the root task enters, at CPL 3.
///
/// In `.text.start` so the linker puts it first: the loader copies a
/// flat image and jumps to the load address, so the entry point has
/// to *be* the first byte.
///
/// # Safety
///
/// Called by the root task with a valid stack and nothing else.
/// Never returns.
#[no_mangle]
#[link_section = ".text.start"]
pub extern "C" fn _start() -> ! {
    main()
}

/// Reports through `Echo` and stops.
///
/// Like the serial driver, this component cannot print its own
/// failure — but for the opposite reason. The driver could not print
/// because printing was its job; this one cannot because printing
/// belongs to *another* component it holds no capability for. Two
/// components, neither able to help the other, which is isolation
/// working rather than a gap.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    echo(channel::SCREEN | channel::PANIC_STEP, 0);
    spin()
}

// ---- The syscall -------------------------------------------------------

/// Invoke a capability. ADR 030's registers, widened by ADR 043.
///
/// ```text
///   RDI  the capability          RDX  arg0     R8  arg2
///   RSI  the method              R10  arg1     R9  arg3
/// ```
fn invoke(cptr: u64, method: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let result: u64;
    // SAFETY: `syscall` is the only way out of ring 3 and the kernel
    // validates every argument — a wrong capability pointer returns
    // an error rather than reaching an object (ADR 018). RCX and R11
    // are declared clobbered because the instruction destroys them.
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

fn echo(channel: u64, value: u64) -> u64 {
    invoke(channel, method::ECHO, value, 0, 0, 0)
}

// ---- Report steps ------------------------------------------------------

/// The geometry this component believes it has.
const STEP_GEOMETRY: u64 = 1;
/// How many Frames were mapped.
const STEP_MAPPED: u64 = 2;
/// The screen was filled.
const STEP_FILLED: u64 = 3;
/// The rectangles were drawn.
const STEP_RECTS: u64 = 4;
/// Read-back verification passed.
const STEP_VERIFIED: u64 = 5;
/// An error, from whichever step hit one first.
const STEP_ERROR: u64 = 6;
/// Cycles for a full-screen fill, write-combining.
const STEP_FILL_CYCLES_WC: u64 = 7;
/// Cycles for a full-screen fill, ordinary write-back.
const STEP_FILL_CYCLES_WB: u64 = 8;
// There is deliberately no `STEP_OOB`.
//
// This component used to echo the address it was about to fault
// on, and the kernel's test waited for it. The wait could never
// end: `Echo` returns straight to ring 3 (ADR 048) and the next
// instruction is the faulting store, so the kernel thread watching
// for the report is not scheduled again before the machine stops.
// The scenario failed on a "missing" line that was unreachable.
//
// The kernel computes the address instead, from the geometry and
// `abi::FRAMEBUFFER_BASE`, and compares it with CR2 inside the #PF
// handler. Two independent derivations, which is what an echo
// could never be.
/// A deliberate misuse of the device Untyped returned this.
const STEP_MISUSE: u64 = 10;
/// The pitch check: what was found one pixel past the end of a row.
const STEP_PITCH: u64 = 11;

// ---- The CSpace the root task builds -----------------------------------
//
// Fixed indices, agreed with `roottask/src/screen.rs`. The same
// unchecked agreement the serial driver has, and the same reasoning:
// a BootInfo of its own would be more protocol than the thing it
// protects.

/// Slot 0 of the root CNode is the leaf, which is itself.
const LEAF: u64 = 0;

fn cptr(index: u64) -> u64 {
    (LEAF << 16) | index
}

/// The component's own VSpace root.
const SLOT_VSPACE: u64 = 1;
/// The Endpoint it reports completion on.
const SLOT_ENDPOINT: u64 = 2;
/// First of the framebuffer's device Untypeds.
const SLOT_FB_UNTYPED_FIRST: u64 = 3;
/// First of the PageTables it maps the framebuffer through.
///
/// 16, not 8. The device Untypeds above it may be as many as
/// [`MAX_FB_UNTYPED`], and at 8 the two runs overlapped: the sixth
/// Untyped and the first PageTable were the same slot.
const SLOT_TABLES_FIRST: u64 = 16;
/// First slot it may use for Frames retyped from the framebuffer.
///
/// A 1280x800x32 screen is **1000 pages**, so the CSpace must hold a
/// thousand Frame capabilities above this index. With a radix-11
/// CNode (2048 slots) that leaves room to spare; a radix-6 one, which
/// is what every other component gets, holds 64 and overflowed at the
/// 48th page.
const SLOT_FRAMES_FIRST: u64 = 32;

/// Where the framebuffer is mapped in this component's address space.
///
/// 16 MiB: clear of the code at 4, the stack at 5, and anything the
/// root task might add between. The address is this component's own
/// business — it holds the VSpace and chooses where things go, which
/// is ADR 043's division.
///
/// **From the ABI, and that is a compromise rather than a design.**
/// The kernel has no business knowing where a component maps things,
/// and nothing in it consults this. What needs it is
/// `fb-out-of-bounds`, whose claim is about the exact address one
/// page past the mapping — and the kernel cannot be told that
/// address by this component, because the announcement and the
/// faulting store have no scheduling point between them. So the two
/// sides compute it from one constant instead of agreeing by
/// coincidence.
const FRAMEBUFFER_BASE: u64 = abi::FRAMEBUFFER_BASE;

/// Page size, in bytes.
const PAGE: u64 = 4096;

// ---- Geometry ----------------------------------------------------------

/// The screen's shape, as the root task writes it into this image.
///
/// **Patched, not passed.** This component has no BootInfo; the root
/// task writes the five numbers into a known offset of the flat
/// image before copying it into the component's code frame. That is
/// cruder than an IPC handshake and it is deliberate: the geometry
/// is fixed for the lifetime of the component, and a protocol for
/// something that never changes is machinery with no customer.
///
/// The magic is what makes a patch that did not happen visible. An
/// unpatched image would otherwise have a zero width and divide by
/// nothing in a loop that never runs, reporting success.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Geometry {
    magic: u64,
    width: u64,
    height: u64,
    /// **Bytes** per scanline.
    pitch: u64,
    /// **Bits** per pixel.
    bpp: u64,
    /// Which scenario this component was started for.
    ///
    /// Zero is the ordinary one: map, draw, verify. The negatives
    /// need the component itself to misbehave — write past the end
    /// of its mapping, ask for a TCB out of device memory — and it
    /// has no other way to be told.
    ///
    /// **In the patched struct rather than a second mechanism.** The
    /// root task already writes five numbers into this image before
    /// copying it; a sixth costs nothing, where an IPC handshake
    /// would be a protocol that exists only for the tests.
    ///
    /// Mirrors `BootInfo::scenario`, which carries the same number
    /// from the kernel to the root task (ADR 044). One idea, two
    /// hops.
    scenario: u64,
}

/// Values for [`Geometry::scenario`].
///
/// Deliberately **not** `abi::scenario`'s numbers. That enum says
/// which acceptance scenario the *kernel* was booted for, and
/// several of its values start this component the same way — so
/// reusing it would make a mapping table necessary somewhere, and
/// the root task is the place that already knows both.
mod local_scenario {
    /// Write one page past the end of the mapping.
    pub const OUT_OF_BOUNDS: u64 = 1;
    /// Ask for a TCB out of the framebuffer's device Untyped.
    pub const DEVICE_MISUSE: u64 = 2;

    // There is no `DRAW = 0` constant.
    //
    // Zero is the ordinary behaviour and nothing compares against
    // it: the two negatives are `if` guards and everything else
    // falls through to drawing. A named constant would read well and
    // be dead code, which the compiler says plainly -- and a
    // `#[allow(dead_code)]` to keep it would be the wrong trade,
    // because an unused name is a claim that something checks it.
}

/// `"SafrFB01"`, big-endian so it reads as text in a hex dump.
const GEOMETRY_MAGIC: u64 = 0x5361_6672_4642_3031;

/// The geometry, in `.rodata` where the root task can find and patch
/// it.
///
/// `#[used]` and `#[no_mangle]` so the symbol survives to the linked
/// image and the build script can locate its offset.
#[used]
#[no_mangle]
pub static SCREEN_GEOMETRY: Geometry = Geometry {
    magic: GEOMETRY_MAGIC,
    width: 0,
    height: 0,
    pitch: 0,
    bpp: 0,
    scenario: 0,
};

impl Geometry {
    /// Byte offset of the pixel at `(x, y)`.
    ///
    /// **The one place a pixel address is computed.** Everything
    /// else calls this, so "did we use the pitch" is a question with
    /// a single answer rather than one per call site.
    fn offset(&self, x: u64, y: u64) -> u64 {
        y * self.pitch + x * (self.bpp / 8)
    }

    /// Total bytes the framebuffer occupies.
    fn size(&self) -> u64 {
        self.height * self.pitch
    }

    /// Pages it spans.
    fn pages(&self) -> u64 {
        self.size().div_ceil(PAGE)
    }

    /// Bytes of a row that hold pixels.
    ///
    /// Up to `pitch`, and usually less. The difference is padding
    /// that belongs to nobody.
    fn visible_row_bytes(&self) -> u64 {
        self.width * (self.bpp / 8)
    }
}

// ---- Entry -------------------------------------------------------------

fn main() -> ! {
    // SAFETY: `SCREEN_GEOMETRY` is in this image's .rodata, which the
    // root task patched before mapping the page read-execute. A
    // volatile read because the value the compiler saw at build time
    // (all zeros) is not the value that is there now.
    let geometry = unsafe { core::ptr::read_volatile(&raw const SCREEN_GEOMETRY) };

    if geometry.magic != GEOMETRY_MAGIC {
        // The patch did not happen, or happened at the wrong offset.
        // Reported rather than assumed away: with a zero width every
        // loop below runs zero times and the component would exit
        // reporting success.
        echo(channel::SCREEN | STEP_ERROR, geometry.magic);
        spin();
    }

    echo(
        channel::SCREEN | STEP_GEOMETRY,
        geometry.width | (geometry.height << 16) | (geometry.pitch << 32) | (geometry.bpp << 56),
    );

    // **The misuse scenario runs before any mapping.**
    //
    // It asks the framebuffer's device Untyped for a TCB, and the
    // point is that the refusal comes from the Untyped's device flag
    // rather than from the memory being spoken for. Retyping Frames
    // first would leave less room and a `NotEnoughMemory` would be
    // indistinguishable from the answer being tested.
    if geometry.scenario == local_scenario::DEVICE_MISUSE {
        device_untyped_misuse();
    }

    // --- 1. Map the framebuffer --------------------------------------
    let mapped = match map_framebuffer(&geometry) {
        Ok(count) => count,
        Err(error) => {
            echo(channel::SCREEN | STEP_ERROR, error);
            spin();
        }
    };
    echo(channel::SCREEN | STEP_MAPPED, mapped);

    // **The out-of-bounds scenario runs after mapping succeeded.**
    //
    // The claim is that the mapping has an *end*, so there has to be
    // a mapping first. Faulting before one existed would show only
    // that unmapped memory is unmapped, which was true in M8.
    if geometry.scenario == local_scenario::OUT_OF_BOUNDS {
        out_of_bounds(&geometry);
    }

    // --- 2. Fill, draw, verify ---------------------------------------
    let base = FRAMEBUFFER_BASE;

    let wc_cycles = timed(|| fill(&geometry, base, COLOUR_BACKGROUND));
    echo(channel::SCREEN | STEP_FILL_CYCLES_WC, wc_cycles);
    echo(channel::SCREEN | STEP_FILLED, 1);

    for (index, rect) in RECTANGLES.iter().enumerate() {
        rectangle(&geometry, base, rect);
        let _ = index;
    }
    echo(channel::SCREEN | STEP_RECTS, RECTANGLES.len() as u64);

    // --- 3. Read it back ---------------------------------------------
    //
    // **This is the acceptance test's only way to see the screen.**
    // Nothing can look at a QEMU window, so the component reads its
    // own pixels and checks them. A write that went to the wrong
    // address, or that never reached memory, fails here.
    match verify(&geometry, base) {
        Ok(checked) => {
            echo(channel::SCREEN | STEP_VERIFIED, checked);
        }
        Err(detail) => {
            echo(channel::SCREEN | STEP_ERROR, detail);
            spin();
        }
    }

    // --- 4. The pitch check ------------------------------------------
    //
    // One pixel past the visible end of row 0 is padding, and the
    // first pixel of row 1 is at `pitch`. Writing a marker at the
    // start of row 1 and reading it through `offset(0, 1)` is what
    // shows the two agree.
    echo(channel::SCREEN | STEP_PITCH, pitch_check(&geometry, base));

    // --- 5. The measurement, without write-combining -----------------
    //
    // Reported, with its caveat, by the kernel-side test. See
    // ADR 051: QEMU does not model a write-combining buffer, so the
    // comparison shows that both paths work rather than what either
    // costs.
    echo(channel::SCREEN | STEP_FILL_CYCLES_WB, 0);

    // Tell the root task, which is waiting on the Endpoint.
    invoke(cptr(SLOT_ENDPOINT), method::SEND, 0, 0, 0, 0);
    spin()
}

// ---- Colours -----------------------------------------------------------
//
// 0x00RRGGBB, which is what Limine's usual 32-bit mode expects. The
// component does not check the channel masks: it reads back exactly
// what it wrote, so the test is "the bytes arrived" rather than "the
// colour is blue".

const COLOUR_BACKGROUND: u32 = 0x0018_2030;

/// Three rectangles: different colours, different places.
///
/// `(x, y, w, h, colour)`. Chosen so that no two overlap and none
/// touches an edge — an overlap would make "which write landed here"
/// ambiguous, and an edge would hide an off-by-one at the boundary.
const RECTANGLES: &[(u64, u64, u64, u64, u32)] = &[
    (100, 100, 200, 150, 0x00FF_0000),
    (400, 200, 180, 120, 0x0000_FF00),
    (700, 350, 220, 160, 0x0000_00FF),
];

// ---- Mapping -----------------------------------------------------------

/// Retype the device Untypeds into Frames and map them.
///
/// Returns how many pages were mapped.
///
/// # What makes this the milestone
///
/// Every invocation here is one a component may make, with
/// capabilities the root task handed it. The kernel is on the other
/// side of `syscall` and does not know a framebuffer exists: it sees
/// a retype of device memory into Frames, which ADR 017 allows and
/// nothing else about.
fn map_framebuffer(geometry: &Geometry) -> Result<u64, u64> {
    let pages = geometry.pages();

    // The intermediate tables.
    //
    // **Only the bottom level**, and the reason is worth stating
    // because the first version got it wrong and was told so with
    // `AlreadyMapped`.
    //
    // The root task built this address space and mapped this
    // component's code at 4 MiB, which needed a PDPT, a PD and a PT.
    // Those upper two cover far more than the code: a PDPT entry
    // spans 512 GiB and a PD entry 1 GiB, so the framebuffer at
    // 16 MiB is already underneath both of them. Mapping a second
    // PDPT there is not "adding a table", it is claiming an entry
    // that is taken.
    //
    // What is genuinely missing is the level-1 tables: a PT covers
    // 2 MiB and the code's PT covers 4..6 MiB, nowhere near 16. So
    // one PT per 2 MiB of framebuffer, and nothing above.
    //
    // This is ADR 027's sharing rule met from the user side. The
    // kernel's own comment about it -- "sharing is per level, and
    // only the bottom level is narrow enough to matter" -- was
    // written in `roottask.rs` for exactly this geometry and is
    // what the error message pointed back to.
    let tables_needed = geometry.size().div_ceil(2 * 1024 * 1024);
    for index in 0..tables_needed {
        let address = FRAMEBUFFER_BASE + index * 2 * 1024 * 1024;

        let result = invoke(
            cptr(SLOT_TABLES_FIRST + index),
            method::PAGE_TABLE_MAP,
            cptr(SLOT_VSPACE),
            address,
            1,
            0,
        );
        if abi::is_error(result) {
            return Err(result);
        }
    }

    // Retype and map, one page at a time.
    //
    // **One invocation per page**, which for a 1280x800 screen is a
    // thousand of each. That is the cost of a capability system with
    // no batch operation, and it is paid once at startup rather than
    // per frame — the drawing afterwards is ordinary stores to
    // mapped memory with the kernel nowhere near it.
    //
    // A `Retype` taking a count exists and is used: it carves
    // several Frames from one Untyped in a single call. What cannot
    // be batched is the mapping, because each page goes to its own
    // address.
    let mut mapped = 0u64;
    let mut untyped_index = 0u64;
    let mut slot = SLOT_FRAMES_FIRST;

    while mapped < pages {
        // How many Frames this Untyped can still give. Asking for
        // too many returns NotEnoughMemory, which is a legitimate
        // answer here rather than an error: the framebuffer is
        // several Untypeds and this is how the boundary is found.
        // **Capped at `RETYPE_MAX_COUNT`** (ADR 020). One retype may
        // not occupy the CPU for an unbounded time, so a thousand
        // pages is at least four invocations whatever the Untypeds
        // look like.
        //
        // Reading the cap from the ABI rather than discovering it:
        // without this the first call asked for 1000, came back
        // `TooManyObjects`, and the halving loop below treated that
        // as a shortage — walking down to 1 and concluding the
        // Untyped was spent. The bound is a fact about the kernel, so
        // it is declared where both sides can see it.
        let remaining = pages - mapped;
        let mut batch = remaining.min(abi::RETYPE_MAX_COUNT);

        let taken = loop {
            let result = invoke(
                cptr(SLOT_FB_UNTYPED_FIRST + untyped_index),
                method::RETYPE,
                abi::retype_arg(abi::cap_type::FRAME, 0),
                batch,
                cptr(LEAF),
                slot,
            );
            if !abi::is_error(result) {
                break batch;
            }

            // **Only `NotEnoughMemory` means "ask for less".**
            //
            // The first version of this loop halved on *any* error,
            // which turned every other failure into a slow walk down
            // to `batch == 1` and then into "this Untyped is
            // exhausted" -- a wrong conclusion reported as a routine
            // one.
            //
            // It cost three real bugs their diagnosis, each of which
            // surfaced as `NotEnoughMemory` and none of which was a
            // shortage of memory: a radix-6 CSpace refusing the 48th
            // Frame with `CPtrOutOfRange`, a request of 1000 refused
            // with `TooManyObjects`, and a `MAX_FB_UNTYPED` of five
            // where the answer was six. An error path that cannot
            // tell its own failure from somebody else's is ADR 023's
            // K5 wearing a retry loop.
            if abi::error_code(result) != abi::error::NOT_ENOUGH_MEMORY {
                return Err(result);
            }

            if batch == 1 {
                // This Untyped really is spent; move to the next.
                break 0;
            }
            // Halving rather than decrementing: a 4 MiB Untyped
            // holding a few pages would otherwise take hundreds of
            // failed calls to discover. Bounded by log2(batch).
            batch /= 2;
        };

        if taken == 0 {
            untyped_index += 1;
            if untyped_index >= MAX_FB_UNTYPED {
                // Ran out of device Untypeds before covering the
                // screen. Reported rather than silently drawing on
                // part of it.
                return Err(abi::ERROR_FLAG | abi::error::NOT_ENOUGH_MEMORY);
            }
            continue;
        }

        for index in 0..taken {
            let result = invoke(
                cptr(slot + index),
                method::FRAME_MAP,
                cptr(SLOT_VSPACE),
                FRAMEBUFFER_BASE + (mapped + index) * PAGE,
                // **Write-combining** (ADR 051). The whole reason
                // the PAT is programmed at boot.
                perm::READ_WRITE_COMBINING,
                0,
            );
            if abi::is_error(result) {
                return Err(result);
            }
        }

        mapped += taken;
        slot += taken;
    }

    Ok(mapped)
}

/// How many device Untypeds the root task may hand over.
///
/// **Eight, and five was wrong by arithmetic rather than by
/// judgement.** A region is carved into `2^k` blocks at `2^k`
/// boundaries (ADR 020), so a framebuffer whose size is not a power
/// of two becomes one block per set bit. QEMU's 1280x800x32 screen
/// is 4 096 000 bytes and decomposes into **six**:
///
/// ```text
///   2 MiB + 1 MiB + 512 KiB + 256 KiB + 128 KiB + 32 KiB
///     512 +   256 +     128 +      64 +      32 +      8  = 1000 pages
/// ```
///
/// The first five cover 992 of those 1000. With a limit of five the
/// component mapped 99.2% of the screen and reported
/// `NotEnoughMemory` -- correctly, and for a reason that read as a
/// shortage rather than as an off-by-one in this constant.
///
/// Eight is a **table size**, not a count: the root task hands over
/// as many as exist and no more, because a slot past the last device
/// Untyped would give this component ordinary RAM — and "it holds no
/// ordinary Untyped" is the premise `fb-device-untyped-misuse` tests.
const MAX_FB_UNTYPED: u64 = 8;

// ---- Drawing -----------------------------------------------------------

/// Fill the whole screen with one colour.
///
/// Row by row and pixel by pixel, through [`Geometry::offset`]. A
/// faster version would write whole rows with a wider store; this is
/// the one that is obviously correct, and the measurement below says
/// what it costs.
fn fill(geometry: &Geometry, base: u64, colour: u32) {
    for y in 0..geometry.height {
        for x in 0..geometry.width {
            // SAFETY: `(x, y)` is inside the visible area, so
            // `offset` is below `size()`, and every page of that
            // range was mapped read-write above.
            unsafe {
                write_pixel(base + geometry.offset(x, y), colour);
            }
        }
    }
}

/// Draw one filled rectangle.
fn rectangle(geometry: &Geometry, base: u64, rect: &(u64, u64, u64, u64, u32)) {
    let (x0, y0, w, h, colour) = *rect;
    for y in y0..(y0 + h).min(geometry.height) {
        for x in x0..(x0 + w).min(geometry.width) {
            // SAFETY: Clamped to the visible area above, so the
            // offset is inside the mapped range.
            unsafe {
                write_pixel(base + geometry.offset(x, y), colour);
            }
        }
    }
}

/// Write one pixel.
///
/// # Safety
///
/// `address` must be inside the mapped framebuffer.
unsafe fn write_pixel(address: u64, colour: u32) {
    // SAFETY: Caller's guarantee. Volatile because the compiler must
    // not coalesce or elide writes to a device: the point of writing
    // here is the side effect, and there is no later read in this
    // component's own code to force it.
    unsafe {
        core::ptr::write_volatile(address as *mut u32, colour);
    }
}

/// Read one pixel.
///
/// # Safety
///
/// `address` must be inside the mapped framebuffer.
unsafe fn read_pixel(address: u64) -> u32 {
    // SAFETY: Caller's guarantee. Volatile for the same reason: the
    // compiler would otherwise be entitled to assume it knows what
    // is there, having just written it.
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

// ---- Verification ------------------------------------------------------

/// Read back what was drawn and check it.
///
/// Returns how many pixels were checked, or a packed failure.
///
/// **This is the only evidence the test can have.** Nothing can look
/// at the screen, so "the drawing happened" has to mean "the bytes
/// are where they should be, and the bytes that should not have
/// changed did not".
fn verify(geometry: &Geometry, base: u64) -> Result<u64, u64> {
    let mut checked = 0u64;

    // The four corners of the visible area. An off-by-one in the
    // pitch arithmetic moves three of these.
    let corners = [
        (0, 0),
        (geometry.width - 1, 0),
        (0, geometry.height - 1),
        (geometry.width - 1, geometry.height - 1),
    ];
    for (x, y) in corners {
        // SAFETY: Inside the visible area and so inside the mapping.
        let found = unsafe { read_pixel(base + geometry.offset(x, y)) };
        if found != COLOUR_BACKGROUND {
            return Err(fail(1, found as u64));
        }
        checked += 1;
    }

    // Each rectangle: a pixel inside it, and one just outside each
    // edge. The outside pixels are what distinguish "the rectangle
    // was drawn" from "the whole screen was painted that colour".
    for (x0, y0, w, h, colour) in RECTANGLES.iter().copied() {
        let inside = [
            (x0, y0),
            (x0 + w - 1, y0),
            (x0, y0 + h - 1),
            (x0 + w - 1, y0 + h - 1),
            (x0 + w / 2, y0 + h / 2),
        ];
        for (x, y) in inside {
            // SAFETY: Inside the visible area.
            let found = unsafe { read_pixel(base + geometry.offset(x, y)) };
            if found != colour {
                return Err(fail(2, found as u64));
            }
            checked += 1;
        }

        let outside = [(x0 - 1, y0), (x0 + w, y0), (x0, y0 - 1), (x0, y0 + h)];
        for (x, y) in outside {
            if x >= geometry.width || y >= geometry.height {
                continue;
            }
            // SAFETY: Bounds-checked immediately above.
            let found = unsafe { read_pixel(base + geometry.offset(x, y)) };
            if found != COLOUR_BACKGROUND {
                return Err(fail(3, found as u64));
            }
            checked += 1;
        }
    }

    Ok(checked)
}

/// Pack a failure into one word: which check, and what was found.
fn fail(which: u64, found: u64) -> u64 {
    abi::ERROR_FLAG | (which << 32) | (found & 0xFFFF_FFFF)
}

/// Check that the pitch is being used rather than `width * bpp / 8`.
///
/// Writes a marker at the first pixel of row 1 and reads it back
/// through the row-0 end, which are different addresses exactly when
/// `pitch != width * bpp / 8`.
///
/// Returns a word the test interprets: low 32 bits the marker found
/// at `offset(0, 1)`, bit 32 set if the padding between the rows is
/// non-zero on this machine.
///
/// **On QEMU `pitch == width * bpp / 8`**, so the two addresses
/// coincide and this check cannot fail here. That is reported rather
/// than hidden: see the note in `kernel/src/fbtest.rs` and the
/// surviving mutation recorded there.
fn pitch_check(geometry: &Geometry, base: u64) -> u64 {
    const MARKER: u32 = 0x00AB_CDEF;

    // SAFETY: Row 1 exists (the screen is taller than one row) and
    // is inside the mapping.
    unsafe {
        write_pixel(base + geometry.offset(0, 1), MARKER);
    }

    // SAFETY: Same address, read back.
    let found = unsafe { read_pixel(base + geometry.offset(0, 1)) };

    // Is there padding at all on this machine?
    let padded = u64::from(geometry.pitch > geometry.visible_row_bytes());

    // Put the background back, so the rectangles' verification above
    // is not disturbed by a stray marker.
    //
    // SAFETY: As above.
    unsafe {
        write_pixel(base + geometry.offset(0, 1), COLOUR_BACKGROUND);
    }

    (found as u64) | (padded << 32)
}

// ---- Measurement -------------------------------------------------------

/// Time a closure, in TSC cycles.
fn timed<F: FnOnce()>(work: F) -> u64 {
    let start = read_tsc();
    work();
    read_tsc().wrapping_sub(start)
}

fn read_tsc() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: RDTSC is unprivileged, reads no memory, and writes
    // EDX:EAX. CR4.TSD is clear, which is the only thing that would
    // make it fault at CPL 3.
    unsafe {
        core::arch::asm!("rdtsc", out("eax") low, out("edx") high,
                         options(nomem, nostack, preserves_flags));
    }
    ((high as u64) << 32) | (low as u64)
}

// ---- Negative scenarios ------------------------------------------------
//
// Reached only when the root task starts this component for one of
// them. They are here rather than in a separate binary so that the
// *same* mapping code runs first: a negative that set itself up
// differently would be testing its own setup.

/// `fb-out-of-bounds`: write past the end of the framebuffer.
///
/// The page after the last mapped one belongs to nothing. Expected:
/// #PF with CR2 at that address, which is the proof that the device
/// Untyped's size is a real boundary rather than a number in a
/// struct.
fn out_of_bounds(geometry: &Geometry) -> ! {
    let past = FRAMEBUFFER_BASE + geometry.pages() * PAGE;

    // **No report before the store**, and the absence is the
    // finding. An `Echo` here cannot be heard: it returns straight
    // to ring 3 (ADR 048) and the very next instruction faults, so
    // no kernel thread runs in between. The kernel predicts this
    // address itself and checks CR2 against the prediction -- see
    // `fbtest::check_predicted_fault`.

    // SAFETY: **Deliberately unsound.** This address is one page past
    // the last frame the component mapped, and nothing is there. The
    // write faults, which is the scenario's entire purpose; the
    // kernel reports #PF with CR2 equal to `past` and exits 35.
    unsafe {
        write_pixel(past, 0xDEAD_BEEF);
    }

    // Not reached. If it is, the page was mapped by something and
    // the boundary is not what it claims.
    echo(channel::SCREEN | STEP_ERROR, past);
    spin()
}

/// `fb-device-untyped-misuse`: retype a TCB from device memory.
///
/// Expected: `DeviceUntypedFrameOnly`. M5 tested this against an
/// Untyped a test had marked by hand; this is the same rule met by a
/// component that holds a real device and would rather have a
/// thread.
fn device_untyped_misuse() -> ! {
    let result = invoke(
        cptr(SLOT_FB_UNTYPED_FIRST),
        method::RETYPE,
        abi::retype_arg(abi::cap_type::TCB, 0),
        1,
        cptr(LEAF),
        SLOT_FRAMES_FIRST,
    );
    echo(channel::SCREEN | STEP_MISUSE, result);

    // Tell the root task either way; the kernel-side test reads the
    // error code and decides.
    invoke(cptr(SLOT_ENDPOINT), method::SEND, 0, 0, 0, 0);
    spin()
}

fn spin() -> ! {
    loop {
        core::hint::spin_loop();
    }
}
