/* SPDX-License-Identifier: MPL-2.0
 *
 * The keyboard driver's layout.
 *
 * Same shape as driver/link.ld and for the same reasons: the image
 * is loaded by the **root task**, not the kernel (ADR 043), copied
 * flat into a Frame and mapped read-execute.
 *
 * The load address is the same 4 MiB every component uses. Not a
 * collision — each component has its own address space, and 0x400000
 * means something different in each of them.
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
        /* The scenario word first, so the root task's patch offset is
         * stable against anything added to .rodata later. The build
         * script reads the symbol's address rather than assuming
         * this, which is what makes a reordering a build failure
         * instead of a patch landing in the middle of a string.
         */
        *(.rodata.scenario)
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

/* No writable statics, and here the rule bit during design rather
 * than during the build.
 *
 * ADR 055's first draft said the driver's key buffer would live in
 * .bss. It could not: the image is mapped read-execute, so a
 * `static mut` faults on first write, and the root task copies a
 * flat image without allocating a .bss at all. The ADR was
 * describing a mechanism that does not exist (ADR 023, K10), and
 * these two assertions are what would have caught it.
 *
 * The driver's state lives in `serve`'s locals, the same answer the
 * serial driver reached.
 */
ASSERT(SIZEOF(.bss) == 0,
       "the keyboard driver must have no .bss: the root task copies a flat image and zeroes nothing")
ASSERT(SIZEOF(.data) == 0,
       "the keyboard driver must have no .data: its image is mapped read-execute, so a writable static would fault on first write")

ASSERT(SIZEOF(.got) <= 64,
       "the keyboard driver's GOT has grown: something is being referenced indirectly in a binary that is loaded flat and never relocated")

/* Two pages, the same budget the serial driver gets.
 *
 * **Measured, and the first guess was wrong.** Three were reserved
 * on the reasoning that this component carries more than the serial
 * driver does: two 64-byte layout tables and a self-test holding the
 * measured byte sequences as literals. The build then printed 5885
 * bytes, which is inside two pages with room to spare.
 *
 * Reserving the third would have been waste charged to this
 * component's budget for a reason that sounded right and was not
 * checked (ADR 022: somebody pays for every object).
 *
 * scripts/build-keyboard.sh prints the real size and fails if it
 * exceeds this; roottask names the same number.
 */
ASSERT(__image_end - 0x400000 <= 2 * 4096,
       "the keyboard driver's image exceeds the two pages the root task maps for it")