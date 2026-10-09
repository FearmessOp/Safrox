/* SPDX-License-Identifier: MPL-2.0
 *
 * The serial driver's layout.
 *
 * Same shape as the root task's and for the same reasons (see
 * roottask/link.ld), with one difference that matters: this image is
 * loaded by the **root task**, not by the kernel. The kernel has no
 * ELF loader and must not grow one — loading is a component's job
 * (ADR 043) — so the root task embeds this flat image, copies it into
 * a Frame it owns, and maps that Frame into the driver's address
 * space read-execute.
 *
 * The load address is the same 4 MiB every component uses. That is
 * not a collision: each component has its own address space, and
 * 0x400000 means something different in each of them. Reusing the
 * number keeps one constant in the ABI instead of a table.
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
 * `static mut` would fault on first write — and the driver genuinely
 * has state to keep (how many bytes it has written, what it last
 * read), which makes this constraint a real one rather than a
 * formality.
 *
 * The state lives on the stack, in the main loop's locals. That is
 * enough here because the driver is a single thread with a single
 * loop; a driver with two threads would need a Frame and a different
 * answer.
 */
ASSERT(SIZEOF(.bss) == 0,
       "the serial driver must have no .bss: the root task copies a flat image and zeroes nothing")
ASSERT(SIZEOF(.data) == 0,
       "the serial driver must have no .data: its image is mapped read-execute, so a writable static would fault on first write")

ASSERT(SIZEOF(.got) <= 64,
       "the serial driver's GOT has grown: something is being referenced indirectly in a binary that is loaded flat and never relocated")

/* Two pages, the same budget the root task gets.
 *
 * Measured rather than chosen: scripts/build-driver.sh prints the
 * real size and fails if it exceeds this. The kernel maps exactly
 * this many frames for the root task; the root task maps exactly this
 * many for the driver, and the two numbers are independent — this one
 * is the root task's decision about its child.
 */
ASSERT(__image_end - 0x400000 <= 2 * 4096,
       "the serial driver's image exceeds the two pages the root task maps for it")