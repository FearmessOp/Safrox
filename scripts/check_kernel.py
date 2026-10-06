#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
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


def fail(message: str) -> None:
    print(f"check_kernel: {message}", file=sys.stderr)
    sys.exit(1)


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
    if e_shoff != 0:
        for i in range(e_shnum):
            off = e_shoff + i * e_shentsize
            sh_name, _sh_type, _sh_flags, sh_addr, _sh_offset, sh_size = (
                struct.unpack_from("<IIQQQQ", data, off)
            )
            sections[section_name(sh_name)] = (sh_addr, sh_size)

    requests = sections.get(".limine_requests")
    if requests is None or requests[1] == 0:
        fail(
            ".limine_requests is missing or empty; the bootloader will ignore "
            "every request (check KEEP() in linker.ld and the 'used' attribute)"
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