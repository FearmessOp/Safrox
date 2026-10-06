/* SPDX-License-Identifier: BSD-2-Clause
 *
 * PythOS kernel link script — x86_64, Limine boot protocol (base revision 3).
 *
 * The kernel is a higher-half ELF executable loaded at -2 GiB, matching
 * `-C code-model=kernel`. The Limine request markers must stay in their own
 * section so the bootloader can find them by scanning a bounded range.
 *
 * Linked with LLD (rust-lld), which is what the x86_64-unknown-none target
 * uses by default.
 */

OUTPUT_FORMAT(elf64-x86-64)
ENTRY(kstart)

/* Limine maps each PT_LOAD segment independently, so segments must be page
 * aligned and must not share a page between differing permissions. */
PHDRS
{
    text     PT_LOAD;
    rodata   PT_LOAD;
    requests PT_LOAD;
    data     PT_LOAD;
}

SECTIONS
{
    /* Top 2 GiB of the canonical address space: the only range reachable
     * with the sign-extended 32-bit displacements that the "kernel" code
     * model emits. Intel SDM Vol. 1, 3.3.7.1. */
    . = 0xffffffff80000000;

    __kernel_image_start = .;

    .text : {
        *(.text .text.*)
    } :text

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    /* Limine base revision >= 2 requires the request markers to live in a
     * dedicated section. KEEP() defends them against --gc-sections: nothing
     * in the kernel references these objects, so without it they would be
     * dropped and every response pointer would stay null at runtime.
     *
     * Order matters: the start marker must precede every request and the end
     * marker must follow them all.
     *
     * This gets its own writable segment, not :rodata. The bootloader writes
     * the response pointer back into each request after loading the image,
     * so the pages must be mapped read/write -- which is also why the limine
     * crate wraps the pointer in an UnsafeCell. */
    .limine_requests : {
        KEEP(*(.limine_requests_start))
        KEEP(*(.limine_requests))
        KEEP(*(.limine_requests_end))
    } :requests

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    /* Relocation-model=static still leaves a (usually empty) GOT; place it
     * with the writable data so no read-only page needs fixing up. */
    .data.rel.ro : {
        *(.data.rel.ro .data.rel.ro.*)
    } :data

    .got : {
        *(.got .got.plt)
    } :data

    .data : {
        __data_start = .;
        *(.data .data.*)
        __data_end = .;
    } :data

    .bss : {
        __bss_start = .;
        /* Ordinary zero-initialised data.
         *
         * The IST stacks are deliberately NOT named `.bss.something`: input
         * sections go to the first output section that matches, so
         * `*(.bss.*)` here would swallow them before their own section
         * below ever got a chance. Giving them an unrelated name
         * (`.ist_stacks`) makes the separation structural instead of
         * depending on wildcard ordering. */
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    } :data

    /* ---- Interrupt stacks -------------------------------------------
     *
     * The IST stacks live at the very top of the image, after every other
     * symbol, with a page of dead space underneath.
     *
     * x86 stacks grow downwards, so an overflow runs off the *bottom* of
     * the stack. Placed anywhere else, that overflow would silently
     * overwrite whatever sits below -- which, before this was fixed, was
     * .data and the Limine request/response structures we are still
     * reading. The corruption would show up much later, somewhere
     * unrelated.
     *
     * The gap is not a guard page: nothing is unmapped, so a deep enough
     * overflow still walks into it quietly. It is dead space, which turns
     * a silent corruption into a harmless scribble and buys room for the
     * real guard page in M3, once page tables exist.
     *
     * Verified by scripts/check_kernel.py, which fails the build if any
     * symbol other than the IST markers lies at or above __ist_stacks_start.
     */
    . = ALIGN(CONSTANT(MAXPAGESIZE));
    __ist_guard_start = .;
    . += CONSTANT(MAXPAGESIZE);
    __ist_guard_end = .;

    .ist_stacks (NOLOAD) : {
        __ist_stacks_start = .;
        KEEP(*(.ist_stacks))
        __ist_stacks_end = .;
    } :data

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    __kernel_image_end = .;

    /* Unwind tables are dead weight: panic = "abort", so nothing unwinds.
     * .note.* is discarded as well, which is why the build passes
     * --build-id=none (otherwise the linker warns about dropping it). */
    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr)
        *(.note .note.*)
        *(.interp)
        *(.comment)
    }
}