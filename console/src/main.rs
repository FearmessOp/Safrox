// SPDX-License-Identifier: MPL-2.0
#![no_std]
#![no_main]

//! The Safrox console — characters on the framebuffer, driven from
//! CPL 3 (M15, ADR 054, ADR 055).
//!
//! It receives key events from the keyboard component over an
//! Endpoint and draws them. What it holds:
//!
//! | capability | why |
//! |------------|-----|
//! | device Untyped(s) | the framebuffer's physical pages |
//! | PageTables | to map them into its own address space |
//! | its own VSpace | the address space it maps into |
//! | an Endpoint | where key events arrive |
//!
//! No IOPort, no IRQHandler, no capability naming the keyboard's
//! objects. Criterion 6's other half: this component cannot drive
//! the keyboard, and the reason is that there is nothing in its
//! CSpace to name (ADR 018).
//!
//! # It does not use `screen/`
//!
//! M14's screen component proves a component can drive the
//! framebuffer; it is not a service. Zero `Recv`, no drawing
//! protocol, one pass and then it spins. Giving it one would make it
//! a display server, and *that* is a layer-2 design — who may draw,
//! z-order, input routing, powerbox — which deserves its own pass
//! rather than being improvised here.
//!
//! So this component maps the framebuffer itself, exactly as the
//! screen does, and the two never run in the same scenario.
//!
//! # The framebuffer is written, never read
//!
//! It is mapped write-combining (ADR 051), and reading WC memory
//! forces the write buffer out and costs a bus round trip per
//! access. Every drawing path here writes; the only reads are in
//! `verify`, which samples a few dozen pixels on purpose (ADR 054).

#![allow(clippy::upper_case_acronyms)]

mod font;

use safrox_abi as abi;
use safrox_abi::{method, perm};

// ---- Entry -------------------------------------------------------------

/// Where the root task enters, at CPL 3.
///
/// # Safety
///
/// Called by the root task with a valid stack and nothing else.
#[no_mangle]
#[link_section = ".text.start"]
pub extern "C" fn _start() -> ! {
    main()
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    echo(CHANNEL_PANIC, 0);
    spin()
}

// ---- The syscall -------------------------------------------------------

/// Invoke a capability. ADR 030's registers, widened by ADR 043.
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

// ---- Report channel ----------------------------------------------------
//
// **From the ABI.** The first draft wrote `1 << 29` here, which is
// `channel::ROOT_DONE` — see the note in `abi/src/lib.rs`, where
// this milestone's two channel collisions are recorded.

use safrox_abi::channel;

/// The console's channel.
const CHANNEL: u64 = channel::CONSOLE;
/// Its panic channel.
const CHANNEL_PANIC: u64 = channel::CONSOLE | channel::PANIC_STEP;

/// The geometry this component believes it has.
const STEP_GEOMETRY: u64 = 1;
/// How many framebuffer pages were mapped.
const STEP_MAPPED: u64 = 2;
/// The grid was sized; the value is columns and rows.
const STEP_GRID: u64 = 3;
/// Ready to receive. The root task's ping is answered from here.
const STEP_READY: u64 = 4;
/// A character was drawn; the value is the character.
const STEP_DREW: u64 = 5;
/// A TSC reading taken right after a character reached the glass.
const STEP_PIXEL_TSC: u64 = 6;
/// Read-back verification of one character passed.
const STEP_VERIFIED: u64 = 7;
/// Read-back verification failed; the value says how.
const STEP_VERIFY_FAIL: u64 = 8;
/// The screen scrolled; the value is how many times.
const STEP_SCROLLED: u64 = 9;
/// Cycles one scroll took.
const STEP_SCROLL_CYCLES: u64 = 10;
/// Cells redrawn by the last scroll.
const STEP_SCROLL_CELLS: u64 = 11;
/// Scroll verification: what was read above and below.
const STEP_SCROLL_VERIFY: u64 = 12;
/// An invocation failed.
const STEP_ERROR: u64 = 13;
/// Trying to drive the keyboard's port returned this.
const STEP_ISOLATION: u64 = 14;
/// Keys received so far.
const STEP_KEYS: u64 = 15;
/// Finished.
const STEP_DONE: u64 = 16;

// ---- The CSpace the root task builds -----------------------------------
//
// The screen component's layout, slot for slot, because this
// component maps the framebuffer the same way. Agreed with
// `roottask/src/console.rs`.

/// Slot 0 of the root CNode is the leaf, which is itself.
const LEAF: u64 = 0;

fn cptr(index: u64) -> u64 {
    (LEAF << 16) | index
}

/// The component's own VSpace root.
const SLOT_VSPACE: u64 = 1;
/// The Endpoint key events arrive on.
const SLOT_ENDPOINT: u64 = 2;
/// First of the framebuffer's device Untypeds.
const SLOT_FB_UNTYPED_FIRST: u64 = 3;
/// First of the PageTables the framebuffer is mapped through.
const SLOT_TABLES_FIRST: u64 = 16;
/// First slot for Frames retyped from the framebuffer.
const SLOT_FRAMES_FIRST: u64 = 32;

/// Where the framebuffer is mapped in this component's address
/// space. The same constant the screen uses, from the ABI.
const FRAMEBUFFER_BASE: u64 = abi::FRAMEBUFFER_BASE;

/// Page size, in bytes.
const PAGE: u64 = 4096;

/// How many device Untypeds the root task may hand over.
///
/// Eight, for the arithmetic reason `screen/src/main.rs` sets out: a
/// 4 096 000-byte framebuffer decomposes into six power-of-two
/// blocks, and five was wrong.
const MAX_FB_UNTYPED: u64 = 8;

// ---- Geometry ----------------------------------------------------------

/// The screen's shape, patched into this image by the root task.
///
/// The same mechanism and the same struct shape as the screen
/// component's (ADR 049): the geometry never changes, so a protocol
/// for it would be machinery with no customer.
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
    scenario: u64,
}

/// `"SafrCN01"`, big-endian so it reads as text in a hex dump.
///
/// Different from the screen's `SafrFB01` on purpose: if the root
/// task ever patched the wrong image, a magic that matched would
/// hide it.
const GEOMETRY_MAGIC: u64 = 0x5361_6672_434E_3031;

/// Values for [`Geometry::scenario`].
mod local_scenario {
    /// Try to read the keyboard's data port.
    pub const ISOLATION: u64 = 1;

    // There is deliberately no `SCROLL` constant.
    //
    // The first draft had one, on the assumption that the scroll
    // measurement needed a scenario of its own. It does not:
    // scrolling happens when text reaches the bottom of the screen,
    // which is a consequence of how many keys arrive rather than a
    // mode this component is put into. `kbd-scroll` sends enough
    // keys; this component behaves identically either way and
    // reports the cycles whenever it scrolls.
    //
    // The compiler found it, because an unused constant is a claim
    // that something checks it. The screen component carries the
    // same note about a `DRAW = 0` that was never needed.
}

/// The geometry, in `.rodata` where the build script can find its
/// offset and the root task can patch it.
#[used]
#[no_mangle]
pub static CONSOLE_GEOMETRY: Geometry = Geometry {
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
    /// **The one place a pixel address is computed**, so "did we use
    /// the pitch" has a single answer. On QEMU `pitch` equals
    /// `width * bpp / 8` and the distinction cannot be observed —
    /// `hardware-todo.md` item 9 records that the mutation survives.
    fn offset(&self, x: u64, y: u64) -> u64 {
        y * self.pitch + x * (self.bpp / 8)
    }

    fn size(&self) -> u64 {
        self.height * self.pitch
    }

    fn pages(&self) -> u64 {
        self.size().div_ceil(PAGE)
    }

    /// Character columns that fit.
    fn columns(&self) -> u64 {
        self.width / font::WIDTH
    }

    /// Character rows that fit.
    fn rows(&self) -> u64 {
        self.height / font::HEIGHT
    }
}

// ---- The character grid ------------------------------------------------
//
// **Not a pixel shadow buffer**, and that is a change from ADR 054's
// first draft; see the note on `scroll`.

/// Columns the grid can hold.
///
/// 1280 / 8 = 160 on the geometry QEMU reports. The grid is a fixed
/// array because this component has no allocator and no ordinary
/// Untyped to retype one from; a screen wider than this is clamped
/// and the clamping is reported rather than silent.
const MAX_COLUMNS: usize = 160;
/// Rows the grid can hold. 800 / 16 = 50.
const MAX_ROWS: usize = 50;

/// What is on the screen, one byte per cell.
///
/// 8000 bytes, which does **not** fit the one-page stack every other
/// component gets — the root task maps this component a larger one.
/// Not a `static mut`: the image is mapped read-execute and
/// `link.ld` asserts `.bss` and `.data` are empty.
type Grid = [[u8; MAX_COLUMNS]; MAX_ROWS];

// ---- Colours -----------------------------------------------------------

/// 0x00RRGGBB, as Limine's usual 32-bit mode expects.
const COLOUR_BACKGROUND: u32 = 0x0010_1018;
/// Character ink.
const COLOUR_TEXT: u32 = 0x00E0_E0E0;
/// The cursor block.
const COLOUR_CURSOR: u32 = 0x0060_A060;

// ---- Protocol ----------------------------------------------------------
//
// The keyboard's label: scan code in bits 15..8, character in bits
// 7..0. Bit 15 of the scan code byte means the key was extended
// (ADR 053).

/// The badge the root task minted onto the keyboard's Endpoint.
///
/// `0xB0A2D` has no meaning; it is simply unlikely to arise by
/// accident, which is all a badge used for one check needs to be.
/// An earlier draft wrote `0xKBD0` to read as "keyboard" — `K` is
/// not a hex digit, and the compiler said so.
///
/// Named here and in `roottask/src/console.rs`; two files agreeing
/// by hand, which is the hazard ADR 047 recorded and this milestone
/// does not fix either.
const BADGE_KEYBOARD: u64 = 0xB0A2D;

/// The root task's ping, to learn this component is receiving.
///
/// Not a character and not a scan code: 0xFFFF cannot arise from a
/// key event, whose label is a byte of scan code and a byte of
/// character.
const CMD_PING: u64 = 0xFFFF;

/// Bound on the serve loop, so the scenario ends.
const MAX_EVENTS: u32 = 512;

// ---- Entry -------------------------------------------------------------

fn main() -> ! {
    // SAFETY: `CONSOLE_GEOMETRY` is in this image's .rodata, which
    // the root task patched before mapping the page read-execute.
    // Volatile because the value the compiler saw is not the value
    // that is there now.
    let geometry = unsafe { core::ptr::read_volatile(&raw const CONSOLE_GEOMETRY) };

    if geometry.magic != GEOMETRY_MAGIC {
        // The patch did not happen, or landed at the wrong offset.
        // Reported: with a zero width every loop below runs zero
        // times and this component would report success having
        // drawn nothing.
        echo(CHANNEL | STEP_ERROR, geometry.magic);
        spin();
    }

    echo(
        CHANNEL | STEP_GEOMETRY,
        geometry.width | (geometry.height << 16) | (geometry.pitch << 32) | (geometry.bpp << 56),
    );

    // Criterion 6, the half this component demonstrates: it holds no
    // IOPort, so naming one is naming an empty slot. Expected
    // `EMPTY_SLOT` rather than a refusal — isolation by construction
    // (ADR 018), not by enforcement.
    if geometry.scenario == local_scenario::ISOLATION {
        let absent = cptr(SLOT_FRAMES_FIRST + 1000);
        let result = invoke(absent, method::IOPORT_IN8, 0x60, 0, 0, 0);
        echo(CHANNEL | STEP_ISOLATION, result);
        invoke(cptr(SLOT_ENDPOINT), method::SEND, 0, 0, 0, 0);
        spin();
    }

    let mapped = match map_framebuffer(&geometry) {
        Ok(count) => count,
        Err(error) => {
            echo(CHANNEL | STEP_ERROR, error);
            spin();
        }
    };
    echo(CHANNEL | STEP_MAPPED, mapped);

    let columns = (geometry.columns() as usize).min(MAX_COLUMNS);
    let rows = (geometry.rows() as usize).min(MAX_ROWS);
    echo(CHANNEL | STEP_GRID, (columns as u64) | ((rows as u64) << 32));

    serve(&geometry, columns, rows)
}

// ---- Mapping -----------------------------------------------------------

/// Retype the device Untypeds into Frames and map them.
///
/// The screen component's routine, including the hard-won details:
/// only level-1 page tables are supplied (the upper levels already
/// cover this address, ADR 027), the retype count is capped by
/// `RETYPE_MAX_COUNT`, and **only** `NotEnoughMemory` means "ask for
/// less" — an error path that cannot tell its own failure from
/// somebody else's cost M14 three diagnoses.
fn map_framebuffer(geometry: &Geometry) -> Result<u64, u64> {
    let pages = geometry.pages();

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

    let mut mapped = 0u64;
    let mut untyped_index = 0u64;
    let mut slot = SLOT_FRAMES_FIRST;

    while mapped < pages {
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
            if abi::error_code(result) != abi::error::NOT_ENOUGH_MEMORY {
                return Err(result);
            }
            if batch == 1 {
                break 0;
            }
            batch /= 2;
        };

        if taken == 0 {
            untyped_index += 1;
            if untyped_index >= MAX_FB_UNTYPED {
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

// ---- Drawing -----------------------------------------------------------

/// Write one pixel.
///
/// # Safety
///
/// `address` must be inside the mapped framebuffer.
unsafe fn write_pixel(address: u64, colour: u32) {
    // SAFETY: Caller's guarantee. Volatile because the point of the
    // write is its side effect and nothing in this component reads
    // it back on the ordinary path.
    unsafe {
        core::ptr::write_volatile(address as *mut u32, colour);
    }
}

/// Read one pixel.
///
/// **Expensive**, and used only by `verify`: the framebuffer is
/// write-combining, so a read drains the write buffer and costs a
/// bus round trip (ADR 054).
///
/// # Safety
///
/// `address` must be inside the mapped framebuffer.
unsafe fn read_pixel(address: u64) -> u32 {
    // SAFETY: Caller's guarantee. Volatile so the compiler does not
    // assume it knows what is there from an earlier write — which is
    // precisely the assumption this read exists to disprove.
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

/// Paint one character cell.
///
/// Every pixel of the cell is written, set or clear. Writing only
/// the set pixels would leave whatever was there before showing
/// through, which is how a 'o' drawn over an 'e' becomes a blot.
fn draw_cell(geometry: &Geometry, column: usize, row: usize, ch: u8) {
    let x0 = column as u64 * font::WIDTH;
    let y0 = row as u64 * font::HEIGHT;

    for dy in 0..font::HEIGHT {
        for dx in 0..font::WIDTH {
            let colour = if font::pixel(ch, dx, dy) {
                COLOUR_TEXT
            } else {
                COLOUR_BACKGROUND
            };
            // SAFETY: The cell is inside the visible area — the
            // caller's column and row come from the clamped grid
            // dimensions — so the offset is below `size()` and every
            // page of that range was mapped read-write.
            unsafe {
                write_pixel(FRAMEBUFFER_BASE + geometry.offset(x0 + dx, y0 + dy), colour);
            }
        }
    }
}

/// Draw or erase the cursor block: the cell's bottom two rows.
fn draw_cursor(geometry: &Geometry, column: usize, row: usize, visible: bool) {
    let x0 = column as u64 * font::WIDTH;
    let y0 = row as u64 * font::HEIGHT;
    let colour = if visible {
        COLOUR_CURSOR
    } else {
        COLOUR_BACKGROUND
    };

    for dy in (font::HEIGHT - 2)..font::HEIGHT {
        for dx in 0..font::WIDTH {
            // SAFETY: Inside the visible area, as `draw_cell`.
            unsafe {
                write_pixel(FRAMEBUFFER_BASE + geometry.offset(x0 + dx, y0 + dy), colour);
            }
        }
    }
}

/// Fill the whole screen with the background colour.
fn clear(geometry: &Geometry) {
    for y in 0..geometry.height {
        for x in 0..geometry.width {
            // SAFETY: Inside the visible area and so inside the
            // mapping.
            unsafe {
                write_pixel(FRAMEBUFFER_BASE + geometry.offset(x, y), COLOUR_BACKGROUND);
            }
        }
    }
}

// ---- Scrolling ---------------------------------------------------------

/// Move every row up one and clear the last.
///
/// Returns how many cells had to be repainted.
///
/// # A character grid, not a pixel shadow buffer
///
/// **This is a change from ADR 054's first draft**, made while
/// writing the code and recorded rather than slipped in.
///
/// The ADR specified a 3.9 MiB shadow buffer in RAM: scroll by
/// `copy_within` there, then blit the whole thing to the
/// framebuffer. The reasoning — never read from write-combining
/// memory — was right. The structure was wrong, for a text console:
///
/// | | shadow buffer | character grid |
/// |---|---|---|
/// | RAM | 3.9 MiB, 1000 Frames | 8 KiB, no Frames |
/// | CSpace | 2000 slots (radix 12) | 1000 slots (radix 11) |
/// | per scroll, RAM | 3.9 MiB moved | 8 KiB moved |
/// | per scroll, to glass | 3.9 MiB, always | only cells that changed |
///
/// The last row is the one that matters. A console is mostly blank,
/// and after a scroll a blank cell that was already blank needs no
/// pixels at all. The shadow buffer cannot know that — it holds
/// pixels, and every pixel moved. The grid holds characters, so
/// "this cell is unchanged" is one byte comparison.
///
/// Neither version ever reads the framebuffer, which was the point
/// the ADR was protecting. This one costs a thousand fewer Frames
/// and repaints a fraction of the screen.
///
/// The count returned is reported, so the saving is a measurement
/// rather than a claim: a full screen of text would repaint every
/// cell and the number would say so.
fn scroll(geometry: &Geometry, grid: &mut Grid, columns: usize, rows: usize) -> u64 {
    let mut repainted = 0u64;

    for row in 0..rows - 1 {
        for column in 0..columns {
            let next = grid[row + 1][column];
            if grid[row][column] != next {
                grid[row][column] = next;
                draw_cell(geometry, column, row, next);
                repainted += 1;
            }
        }
    }

    for column in 0..columns {
        if grid[rows - 1][column] != b' ' {
            grid[rows - 1][column] = b' ';
            draw_cell(geometry, column, rows - 1, b' ');
            repainted += 1;
        }
    }

    repainted
}

// ---- Verification ------------------------------------------------------

/// Read a drawn character back from the **real framebuffer** and
/// check it against the font.
///
/// Returns `Ok(pixels checked)`, or `Err` packed as
/// `(which << 32) | found`.
///
/// # Why this samples instead of reading the cell
///
/// Reading 128 pixels per character from write-combining memory is
/// slow enough to matter, and unnecessary: eight well-chosen pixels
/// distinguish every failure a drawing path produces (ADR 054).
///
/// * **four set** — the glyph was drawn at all;
/// * **two clear**, inside the glyph's body — the cell was not just
///   filled solid;
/// * **two outside the cell** — the glyph went into the right cell.
///
/// Dropping any group leaves a real failure undetected. Only the
/// set pixels: a cell painted entirely white passes. Only cells
/// inside: a glyph one column to the left passes.
fn verify_cell(geometry: &Geometry, column: usize, row: usize, ch: u8) -> Result<u64, u64> {
    let Some((set, clear)) = font::sample_points(ch) else {
        // A character with too few of either kind. Reported as a
        // failure rather than skipped: a verifier that silently
        // checks nothing is the thing this whole function exists to
        // avoid.
        return Err((9u64 << 32) | ch as u64);
    };

    let x0 = column as u64 * font::WIDTH;
    let y0 = row as u64 * font::HEIGHT;
    let mut checked = 0u64;

    for (dx, dy) in set {
        // SAFETY: Inside the cell, which is inside the visible area.
        let found = unsafe { read_pixel(FRAMEBUFFER_BASE + geometry.offset(x0 + dx, y0 + dy)) };
        if found != COLOUR_TEXT {
            return Err((1u64 << 32) | found as u64);
        }
        checked += 1;
    }

    for (dx, dy) in clear {
        // SAFETY: As above.
        let found = unsafe { read_pixel(FRAMEBUFFER_BASE + geometry.offset(x0 + dx, y0 + dy)) };
        if found != COLOUR_BACKGROUND {
            return Err((2u64 << 32) | found as u64);
        }
        checked += 1;
    }

    // Just outside the cell: one pixel left of its top-left corner,
    // one below its bottom-right. Both must be background — a glyph
    // drawn one cell over would have put ink in one of them.
    if x0 > 0 {
        // SAFETY: `x0 - 1` is on the screen because `x0 > 0`.
        let found = unsafe { read_pixel(FRAMEBUFFER_BASE + geometry.offset(x0 - 1, y0)) };
        if found != COLOUR_BACKGROUND {
            return Err((3u64 << 32) | found as u64);
        }
        checked += 1;
    }
    let below = y0 + font::HEIGHT;
    if below < geometry.height {
        // SAFETY: Bounds-checked immediately above.
        let found = unsafe { read_pixel(FRAMEBUFFER_BASE + geometry.offset(x0, below)) };
        if found != COLOUR_BACKGROUND {
            return Err((4u64 << 32) | found as u64);
        }
        checked += 1;
    }

    Ok(checked)
}

/// Sample the top and bottom rows, for the scroll check.
///
/// Returns a word the test reads: low 32 bits say how many of the
/// three top samples were ink, high 32 how many of the three bottom
/// ones were. After a scroll the top should carry what was on the
/// second row and the bottom should be blank.
///
/// Both ends, because checking only the top would pass a scroll that
/// moved the text up and left the old last row untouched.
fn sample_rows(geometry: &Geometry, columns: usize, rows: usize) -> u64 {
    let mut top = 0u64;
    let mut bottom = 0u64;

    for index in 0..3u64 {
        let column = (index as usize * columns / 4).min(columns - 1);
        let x = column as u64 * font::WIDTH + 2;

        // Row 0, a few pixels down into the glyph body.
        // SAFETY: Inside the visible area.
        let found = unsafe { read_pixel(FRAMEBUFFER_BASE + geometry.offset(x, 6)) };
        if found == COLOUR_TEXT {
            top += 1;
        }

        let y = (rows as u64 - 1) * font::HEIGHT + 6;
        // SAFETY: The last text row is on the screen by construction.
        let found = unsafe { read_pixel(FRAMEBUFFER_BASE + geometry.offset(x, y)) };
        if found == COLOUR_TEXT {
            bottom += 1;
        }
    }

    top | (bottom << 32)
}

// ---- Measurement -------------------------------------------------------

fn read_tsc() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: RDTSC is unprivileged, reads no memory, writes EDX:EAX.
    // CR4.TSD is clear, which is the only thing that would make it
    // fault at CPL 3.
    unsafe {
        core::arch::asm!("rdtsc", out("eax") low, out("edx") high,
                         options(nomem, nostack, preserves_flags));
    }
    ((high as u64) << 32) | (low as u64)
}

// ---- The serve loop ----------------------------------------------------

fn serve(geometry: &Geometry, columns: usize, rows: usize) -> ! {
    // On the stack. 8000 bytes, which is why the root task maps this
    // component more than the one page every other component gets —
    // and why `link.ld` can still assert `.bss` is empty.
    let mut grid: Grid = [[b' '; MAX_COLUMNS]; MAX_ROWS];

    clear(geometry);

    let mut column = 0usize;
    let mut row = 0usize;
    let mut keys = 0u64;
    let mut scrolls = 0u64;

    draw_cursor(geometry, column, row, true);
    echo(CHANNEL | STEP_READY, 0);

    for _ in 0..MAX_EVENTS {
        let reply = invoke(cptr(SLOT_ENDPOINT), method::RECV, 0, 0, 0, 0);
        if abi::is_error(reply) {
            // The Endpoint was revoked: `kbd-driver-dies` is taking
            // the system apart around this component. Report and
            // stop.
            echo(CHANNEL | STEP_ERROR, reply);
            spin();
        }

        let badge = reply & 0xFFFF_FFFF;
        let label = reply >> 32;

        // The badge is the sender's identity and the sender could
        // not choose it (ADR 037). Checked even though there is one
        // sender: a second input source is the obvious next change,
        // and a check added later is a check that was missing in
        // between.
        if badge != BADGE_KEYBOARD && badge != 0 {
            echo(CHANNEL | STEP_ERROR, badge);
            continue;
        }

        if label == CMD_PING {
            // The root task learning this component is receiving.
            // `Send` is a rendezvous (ADR 033), so its return is the
            // event — no sleep, no retry.
            continue;
        }

        let character = (label & 0xFF) as u8;
        let code = ((label >> 8) & 0xFF) as u8;
        let extended = code & 0x80 != 0;

        keys += 1;

        draw_cursor(geometry, column, row, false);

        if extended {
            // Cursor movement. The only reason the console
            // interprets extended keys at all: without it, "the
            // 0xE0 prefix was decoded" would rest entirely on the
            // keyboard's own report of itself (criterion 3).
            match code & 0x7F {
                0x4B => column = column.saturating_sub(1),
                0x4D => column = (column + 1).min(columns - 1),
                0x48 => row = row.saturating_sub(1),
                0x50 => row = (row + 1).min(rows - 1),
                _ => {}
            }
        } else if character == b'\n' || code == 0x1C {
            column = 0;
            row += 1;
        } else if code == 0x0E {
            // Backspace.
            if column > 0 {
                column -= 1;
                grid[row][column] = b' ';
                draw_cell(geometry, column, row, b' ');
            }
        } else if character >= font::FIRST && character <= font::LAST {
            grid[row][column] = character;
            draw_cell(geometry, column, row, character);

            // **Right after the pixels reach the glass.** Paired
            // with the keyboard's reading at interrupt time; the
            // test subtracts them. Both are TSC on the same CPU, so
            // the difference is the whole path: IRQ → driver → IPC →
            // here → pixel.
            echo(CHANNEL | STEP_PIXEL_TSC, read_tsc());
            echo(CHANNEL | STEP_DREW, character as u64);

            // Criterion 4. Read back from the **real framebuffer**,
            // not from any buffer of this component's own.
            match verify_cell(geometry, column, row, character) {
                Ok(checked) => {
                    echo(CHANNEL | STEP_VERIFIED, checked | ((character as u64) << 32));
                }
                Err(detail) => {
                    echo(CHANNEL | STEP_VERIFY_FAIL, detail);
                }
            }

            column += 1;
        }

        if column >= columns {
            column = 0;
            row += 1;
        }

        if row >= rows {
            row = rows - 1;
            let start = read_tsc();
            let repainted = scroll(geometry, &mut grid, columns, rows);
            let cycles = read_tsc().wrapping_sub(start);

            scrolls += 1;
            echo(CHANNEL | STEP_SCROLLED, scrolls);
            echo(CHANNEL | STEP_SCROLL_CYCLES, cycles);
            echo(CHANNEL | STEP_SCROLL_CELLS, repainted);
            echo(
                CHANNEL | STEP_SCROLL_VERIFY,
                sample_rows(geometry, columns, rows),
            );
        }

        draw_cursor(geometry, column, row, true);
        echo(CHANNEL | STEP_KEYS, keys);
    }

    echo(CHANNEL | STEP_DONE, keys);
    spin()
}

fn spin() -> ! {
    loop {
        core::hint::spin_loop();
    }
}