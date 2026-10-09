#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Static sanity checks on the linked kernel ELF.

These catch the classic silent-boot-failure causes before we ever spin up
QEMU, where the only symptom would be a blank serial console:

  * wrong ELF class/machine/type
  * a kernel that did not actually land in the higher half
  * PT_LOAD segments that are not page aligned (Limine rejects these)
  * the .limine_requests section being garbage-collected away, which makes
    the bootloader silently ignore every request
  * absolute build-machine paths leaking into the image (DWARF comp_dir,
    __FILE__), which breaks reproducible builds and makes panic output
    differ per developer
"""

from __future__ import annotations

import os
import re
import struct
import sys

HIGHER_HALF_BASE = 0xFFFFFFFF80000000
PAGE_SIZE = 0x1000

ELFCLASS64 = 2
ELFDATA2LSB = 1
ET_EXEC = 2
ET_DYN = 3
EM_X86_64 = 0x3E
PT_LOAD = 1

# Section header flags. Reference: ELF-64 spec, Figure 4-9.
SHF_WRITE = 0x1
SHF_EXECINSTR = 0x4

# Section types. Reference: ELF-64 spec, Figure 4-8.
SHT_SYMTAB = 2

# One Elf64_Sym: name(4) info(1) other(1) shndx(2) value(8) size(8).
SYM_SIZE = 24


def fail(message: str) -> None:
    print(f"check_kernel: {message}", file=sys.stderr)
    sys.exit(1)


def read_symbols(data, e_shoff, e_shnum, e_shentsize, section_name):
    """Map symbol name -> value, from the first SYMTAB in the file.

    Only the linker-defined markers are of interest here, and those are
    absolute addresses with no relocation to apply, so a flat dictionary
    is enough.

    Reference: ELF-64 Object File Format, section 4 ("Symbol Table").
    """
    symbols = {}
    if e_shoff == 0:
        return symbols

    for i in range(e_shnum):
        off = e_shoff + i * e_shentsize
        _sh_name, sh_type, _sh_flags, _sh_addr, sh_offset, sh_size = struct.unpack_from(
            "<IIQQQQ", data, off
        )
        if sh_type != SHT_SYMTAB:
            continue

        # sh_link names the string table holding this symtab's names.
        sh_link = struct.unpack_from("<I", data, off + 40)[0]
        str_off = struct.unpack_from("<Q", data, e_shoff + sh_link * e_shentsize + 24)[0]

        for entry in range(sh_size // SYM_SIZE):
            sym_off = sh_offset + entry * SYM_SIZE
            st_name, _st_info, _st_other, _st_shndx, st_value, _st_size = (
                struct.unpack_from("<IBBHQQ", data, sym_off)
            )
            if st_name == 0:
                continue
            end = data.index(b"\0", str_off + st_name)
            name = data[str_off + st_name : end].decode("ascii", errors="replace")
            symbols[name] = st_value

        break

    return symbols


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: check_kernel.py <kernel.elf>", file=sys.stderr)
        return 2

    path = sys.argv[1]
    with open(path, "rb") as handle:
        data = handle.read()

    if len(data) < 64 or data[:4] != b"\x7fELF":
        fail(f"{path} is not an ELF file")
    if data[4] != ELFCLASS64:
        fail("not a 64-bit ELF (expected ELFCLASS64)")
    if data[5] != ELFDATA2LSB:
        fail("not little-endian")

    e_type, e_machine = struct.unpack_from("<HH", data, 16)
    if e_machine != EM_X86_64:
        fail(f"wrong machine {e_machine:#x}, expected x86-64 ({EM_X86_64:#x})")
    if e_type not in (ET_EXEC, ET_DYN):
        fail(f"unexpected ELF type {e_type}")

    e_entry, e_phoff, e_shoff = struct.unpack_from("<QQQ", data, 24)
    e_phentsize, e_phnum = struct.unpack_from("<HH", data, 54)
    e_shentsize, e_shnum, e_shstrndx = struct.unpack_from("<HHH", data, 58)

    if e_entry < HIGHER_HALF_BASE:
        fail(
            f"entry point {e_entry:#x} is below {HIGHER_HALF_BASE:#x}; "
            f"the kernel is not higher-half (check -mcmodel=kernel and linker.ld)"
        )

    # --- Program headers -------------------------------------------------
    loads = 0
    for i in range(e_phnum):
        off = e_phoff + i * e_phentsize
        p_type, _p_flags, p_offset, p_vaddr, _p_paddr, p_filesz, p_memsz, p_align = (
            struct.unpack_from("<IIQQQQQQ", data, off)
        )
        if p_type != PT_LOAD:
            continue
        loads += 1

        if p_vaddr < HIGHER_HALF_BASE:
            fail(f"PT_LOAD segment at {p_vaddr:#x} is not in the higher half")
        if p_align < PAGE_SIZE:
            fail(f"PT_LOAD segment at {p_vaddr:#x} has alignment {p_align:#x} < 4K")
        # Limine maps file offset -> vaddr directly, so the two must be
        # congruent modulo the page size.
        if (p_vaddr - p_offset) % PAGE_SIZE != 0:
            fail(
                f"PT_LOAD segment at {p_vaddr:#x} is not congruent with its "
                f"file offset {p_offset:#x} modulo {PAGE_SIZE:#x}"
            )
        if p_memsz < p_filesz:
            fail(f"PT_LOAD segment at {p_vaddr:#x} has memsz < filesz")

    if loads == 0:
        fail("no PT_LOAD segments found")

    # --- Section headers -------------------------------------------------
    def section_name(name_off: int) -> str:
        sh_off = e_shoff + e_shstrndx * e_shentsize
        str_base = struct.unpack_from("<Q", data, sh_off + 24)[0]
        end = data.index(b"\0", str_base + name_off)
        return data[str_base + name_off : end].decode("ascii", errors="replace")

    sections = {}
    section_flags = {}
    if e_shoff != 0:
        for i in range(e_shnum):
            off = e_shoff + i * e_shentsize
            sh_name, _sh_type, sh_flags, sh_addr, _sh_offset, sh_size = (
                struct.unpack_from("<IIQQQQ", data, off)
            )
            name = section_name(sh_name)
            sections[name] = (sh_addr, sh_size)
            section_flags[name] = sh_flags

    requests = sections.get(".limine_requests")
    if requests is None or requests[1] == 0:
        fail(
            ".limine_requests is missing or empty; the bootloader will ignore "
            "every request (check KEEP() in linker.ld and the 'used' attribute)"
        )

    # --- Section permissions ---------------------------------------------
    #
    # M3 maps the image section by section, so the sections must exist and
    # must be page aligned: permissions have page granularity, and two
    # sections sharing a page would force the looser of the two onto both.
    #
    # The IST stacks used to be checked here. They are no longer in the
    # image at all: M3 allocates them in KSTACK with a genuinely unmapped
    # guard page below, which is verified at run time by the vm-guard-page
    # scenario rather than statically. The static check could only ever
    # confirm dead space, which was never real protection.
    for name in (".text", ".rodata"):
        section = sections.get(name)
        if section is None:
            fail(f"{name} is missing; vm::init maps the image by section")
        addr, size = section
        if addr % PAGE_SIZE != 0:
            fail(
                f"{name} starts at {addr:#x}, which is not page aligned; "
                f"it would share a page with the previous section and have "
                f"to take the looser permission of the two"
            )
        if size == 0:
            fail(f"{name} is empty")

    # .text must not be writable even in the ELF, or the W^X mapping would
    # be contradicting the file it came from.
    text_flags = section_flags.get(".text", 0)
    if text_flags & SHF_WRITE:
        fail(".text is marked writable in the ELF; W^X starts here")
    if not text_flags & SHF_EXECINSTR:
        fail(".text is not marked executable in the ELF")

    # --- Every byte of the image falls in exactly one mapped range -------
    #
    # vm::init maps the image in three pieces -- [__text_start,
    # __text_end), [__rodata_start, __rodata_end) and [__writable_start,
    # __writable_end) -- and maps nothing else. A section that falls
    # outside all three is simply absent from the new tables.
    #
    # This is not hypothetical. On the first run of M3 the linker script
    # placed .limine_requests after __rodata_end but before
    # __writable_start, leaving a one-page hole at 0xffffffff80015000. The
    # run-time pre-switch check caught it, which is the right outcome but
    # the late one: a missing page is a property of the linked file and
    # should fail the build, not the boot.
    symbols = read_symbols(data, e_shoff, e_shnum, e_shentsize, section_name)

    required = (
        "__kernel_image_start",
        "__kernel_image_end",
        "__text_start",
        "__text_end",
        "__rodata_start",
        "__rodata_end",
        "__writable_start",
        "__writable_end",
    )
    missing = [name for name in required if name not in symbols]
    if missing:
        fail(
            "linker.ld does not define " + ", ".join(missing) +
            "; vm::init needs them to map the image section by section"
        )

    ranges = [
        ("text", symbols["__text_start"], symbols["__text_end"]),
        ("rodata", symbols["__rodata_start"], symbols["__rodata_end"]),
        ("writable", symbols["__writable_start"], symbols["__writable_end"]),
    ]

    for name, start, end in ranges:
        if start % PAGE_SIZE or end % PAGE_SIZE:
            fail(
                f"the {name} range {start:#x}..{end:#x} is not page aligned; "
                f"permissions have page granularity"
            )
        if end < start:
            fail(f"the {name} range {start:#x}..{end:#x} runs backwards")

    # The three must not overlap, or a page would be mapped twice with
    # different permissions and the last one would silently win.
    ordered = sorted(ranges, key=lambda r: r[1])
    for (a_name, _, a_end), (b_name, b_start, _) in zip(ordered, ordered[1:]):
        if a_end > b_start:
            fail(
                f"the {a_name} and {b_name} ranges overlap at {b_start:#x}; "
                f"one page would be mapped twice with different permissions"
            )

    # And together they must cover the whole image, with no gaps.
    image_start = symbols["__kernel_image_start"]
    image_end = symbols["__kernel_image_end"]

    cursor = image_start
    for name, start, end in ordered:
        if start > cursor:
            fail(
                f"nothing maps {cursor:#x}..{start:#x} ({(start - cursor) // PAGE_SIZE} "
                f"page(s)) before the {name} range. Anything the linker placed "
                f"there would be absent from the kernel's own page tables, and "
                f"the first access after the CR3 switch would fault. Check the "
                f"order of sections and the __*_start/__*_end markers around "
                f"them in linker.ld."
            )
        cursor = max(cursor, end)

    if cursor < image_end:
        fail(
            f"nothing maps {cursor:#x}..{image_end:#x} at the end of the image; "
            f"the writable range stops short of __kernel_image_end"
        )

    # Allocated sections, specifically, must land inside one of the ranges.
    # The check above proves the ranges are contiguous; this proves the
    # sections are inside them rather than off past the end.
    for name, (addr, size) in sections.items():
        if size == 0 or addr == 0:
            continue  # non-allocated: debug info, symbol tables, comments
        if not any(start <= addr and addr + size <= end for _, start, end in ranges):
            fail(
                f"section {name} at {addr:#x}..{addr + size:#x} is not inside "
                f"any of the three mapped ranges; vm::init would not map it"
            )

    # --- Build-path leakage ---------------------------------------------
    #
    # -ffile-prefix-map should have rewritten every source path to a
    # project-relative one. If an absolute path survived, the binary is not
    # reproducible and panic messages would embed the builder's home
    # directory.
    leak_pattern = re.compile(
        rb"(?:/mnt/[a-z]/|/home/|/Users/|[A-Za-z]:\\\\)[^\x00]{0,120}"
    )
    leaks = {match.group(0) for match in leak_pattern.finditer(data)}

    # The directory we were invoked from is the one that must not appear.
    cwd = os.getcwd().encode()
    leaks = {leak for leak in leaks if cwd in leak or leak.startswith(cwd)}

    if leaks:
        sample = sorted(leaks)[:3]
        rendered = "\n    ".join(s.decode("utf-8", "replace") for s in sample)
        fail(
            "absolute build paths leaked into the image "
            "(is -ffile-prefix-map still in CFLAGS?):\n    " + rendered
        )

    print(
        f"check_kernel: OK  entry={e_entry:#x}  PT_LOAD={loads}  "
        f".limine_requests={requests[1]} bytes @ {requests[0]:#x}  "
        f"no build-path leaks"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())