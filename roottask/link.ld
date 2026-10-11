/* SPDX-License-Identifier: MPL-2.0
 *
 * The root task's layout.
 *
 * One loadable segment at 4 MiB, in the user half. The kernel copies
 * it there as a flat image -- it has no ELF loader and should not
 * have one: loading is a *component's* job (ADR 043), and M11 showed
 * a component doing exactly that for its child.
 *
 * Everything is in one output section because the kernel copies one
 * range. A second section would need a second copy, which needs a
 * program header table, which needs a parser.
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

    /* The GOT.
     *
     * A static binary should not have one, and the first build did:
     * 16 bytes of it. `relocation-model=static` removes the need for
     * position independence but does not promise the linker will
     * emit no GOT at all -- LLD still routes some references through
     * one unless the section is placed and the entries resolve to
     * absolute addresses.
     *
     * Kept and placed rather than discarded. Discarding it would
     * leave the references pointing at nothing, which is a fault at
     * the first use of whatever used it -- and the kernel copies a
     * flat image, so there is no relocation step that could fix it
     * up. Placing it in the image means the absolute addresses the
     * linker computed are simply there.
     *
     * The ASSERT below bounds it: a GOT that started *growing* would
     * mean something is being referenced indirectly that should not
     * be, and the right response is to find out what rather than to
     * widen the bound.
     */
    .got : {
        *(.got .got.plt)
    }

    /* Writable data follows the code in the same image, and the whole
     * thing is mapped read-execute... which would make this
     * unwritable.
     *
     * It is not a problem because there is nothing here: the root
     * task's state lives on its stack. The assert below is what keeps
     * that true -- a `static mut` added later fails the build rather
     * than faulting at run time with a confusing #PF on a page the
     * programmer thought was data.
     */
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

/* No .bss, and no .data either.
 *
 * The kernel copies a flat image to a read-execute page. Writable
 * state would need a second page, a second mapping with different
 * permissions, and the kernel deciding where it goes -- which is a
 * loader, in the kernel, for one component. The root task keeps its
 * state in registers and on its stack instead.
 *
 * ASSERT rather than a comment, because "there is no .bss" is the
 * kind of claim that stops being true silently.
 */
ASSERT(SIZEOF(.bss) == 0,
       "the root task must have no .bss: the kernel copies a flat image and does not zero anything")
ASSERT(SIZEOF(.data) == 0,
       "the root task must have no .data: its image is mapped read-execute, so a writable static would fault on first write")

/* The GOT is small and must stay small. See the note above it. */
ASSERT(SIZEOF(.got) <= 64,
       "the root task's GOT has grown: something is being referenced indirectly in a binary that is loaded flat and never relocated")

/* And the whole image has to fit the frames the kernel maps.
 *
 * One page in the first draft; two when the image came out at 7432
 * bytes and scripts/build-roottask.sh said so; four from M13, when
 * the root task began embedding the serial driver's flat image in
 * its own .rodata; **eight from M14**, which added a second one;
 * **twelve from M15**, which added two and took the image to 44792
 * bytes.
 *
 * That embedding is not incidental. The kernel has no ELF loader and
 * must not grow one -- loading is a component's job (ADR 043) -- so
 * the component that loads the others is the one that carries them.
 * The root task's own code barely changes; its payload is what
 * grows, and it grows once per component:
 *
 *     serial-driver.bin    1212
 *     screen.bin           4230
 *     keyboard.bin         5885
 *     console.bin         10696
 *
 * Every figure here was measured rather than chosen, and each time
 * the check fired before anything subtle happened. M14's four-page
 * build failed at the link naming this limit rather than producing
 * a root task with its tail missing; M15's eight-page build did the
 * same -- which is what the guard is for, and the fourth time it has
 * earned its place.
 *
 * **This number and ROOT_CODE_PAGES in kernel/src/roottask.rs are
 * one decision written twice**, and they have to move together: this
 * one bounds what the linker emits, that one decides how many frames
 * the kernel maps. The kernel also carries a compile-time assert
 * against the measured size, so a change made in only one place
 * fails the build rather than mapping an image with its tail
 * missing.
 *
 * The rule for the next increase lives on ROOT_CODE_PAGES, because
 * that is where the reasoning about cost belongs. In short: this is
 * the third increase, and a fourth should be a discussion about
 * loading modules separately rather than another bump.
 */
ASSERT(__image_end - 0x400000 <= 12 * 4096,
       "the root task's image exceeds the twelve pages the kernel maps for it")