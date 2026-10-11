/* SPDX-License-Identifier: MPL-2.0
 *
 * The console's layout.
 *
 * Same shape as screen/link.ld: loaded by the **root task**, not
 * the kernel (ADR 043), copied flat into Frames and mapped
 * read-execute at 4 MiB.
 */

ENTRY(_start)

SECTIONS
{
    . = 0x400000;

    .text : {
        *(.text.start)      /* _start first, so entry == load address */
        *(.text .text.*)
    }

    .rodata : {
        *(.rodata .rodata.*)
    }

    .got : {
        *(.got .got.plt)
    }

    .data : {
        *(.data .data.*)
    }

    .bss (NOLOAD) : {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note*)
        *(.comment)
    }

    __image_end = .;
}

/* No writable statics. The image is mapped read-execute, so a
 * `static mut` faults on first write, and the root task copies a
 * flat image without allocating a .bss at all.
 *
 * This component has the largest state of any so far -- an 8 KiB
 * character grid -- and it lives in `serve`'s locals like everybody
 * else's. What that costs is a **bigger stack**: 8000 bytes does not
 * fit the single page every other component gets, so the root task
 * maps this one four. The number is named in roottask/src/console.rs
 * and the grid's size is named here, in MAX_COLUMNS x MAX_ROWS; if
 * either grows past the other the component faults on entry, which
 * is loud rather than subtle.
 *
 * An alternative was a Frame mapped read-write for the grid. It
 * would work and it buys nothing: the grid is private, single-
 * threaded and exactly as large as a stack can hold.
 */
ASSERT(SIZEOF(.bss) == 0,
       "the console must have no .bss: the root task copies a flat image and zeroes nothing")
ASSERT(SIZEOF(.data) == 0,
       "the console must have no .data: its image is mapped read-execute, so a writable static would fault on first write")

ASSERT(SIZEOF(.got) <= 64,
       "the console's GOT has grown: something is being referenced indirectly in a binary that is loaded flat and never relocated")

/* Three pages. The font table alone is 1536 bytes of .rodata and
 * the drawing, scrolling and verification paths are each larger
 * than anything in the serial driver.
 *
 * Measured, and the first guess was four -- the same way the
 * keyboard's first guess was three and the real figure two. The
 * build prints 10696 bytes, which is inside three pages.
 *
 * scripts/build-console.sh prints the real size and fails if it
 * exceeds this; roottask names the same number.
 */
ASSERT(__image_end - 0x400000 <= 3 * 4096,
       "the console's image exceeds the three pages the root task maps for it")