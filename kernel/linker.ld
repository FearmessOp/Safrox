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
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
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