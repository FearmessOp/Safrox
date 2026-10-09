/* SPDX-License-Identifier: MPL-2.0
 *
 * The screen component's layout.
 *
 * Same shape as driver/link.ld and for the same reasons. Loaded by
 * the root task, not the kernel: ADR 043 puts loading in a
 * component, so this is a flat image embedded in the root task's
 * .rodata and copied into Frames the root task owns.
 *
 * The load address is 4 MiB, like every component. Each has its own
 * address space, so the number means something different in each of
 * them; reusing it keeps one constant in the ABI rather than a table.
 *
 * The framebuffer is mapped somewhere else entirely -- see
 * FRAMEBUFFER_BASE in src/main.rs -- and it is not part of this
 * image. The linker never sees it.
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

/* No writable statics; the image is mapped read-execute.
 *
 * This component has more state than the serial driver did -- the
 * geometry, a cursor, the figures it measures -- and all of it lives
 * on the stack. That is sufficient for the same reason: one thread,
 * one loop. It is worth noticing that the constraint has not yet
 * forced an awkward design, because the first time it does is the
 * moment to give a component a Frame for its data rather than to
 * loosen this.
 */
ASSERT(SIZEOF(.bss) == 0,
       "the screen component must have no .bss: the root task copies a flat image and zeroes nothing")
ASSERT(SIZEOF(.data) == 0,
       "the screen component must have no .data: its image is mapped read-execute, so a writable static would fault on first write")

ASSERT(SIZEOF(.got) <= 64,
       "the screen component's GOT has grown: something is being referenced indirectly in a binary that is loaded flat and never relocated")

/* Four pages.
 *
 * Twice the driver's budget, and the reason is the mapping loop: this
 * component maps a thousand frames one invocation at a time, which is
 * more code than configuring eight UART registers. Measured by
 * scripts/build-screen.sh, which fails rather than truncating -- M13
 * learned that a silently short image runs off its own end into
 * zeroed memory and faults somewhere unrelated.
 */
ASSERT(__image_end - 0x400000 <= 4 * 4096,
       "the screen component's image exceeds the four pages the root task maps for it")