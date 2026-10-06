#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""Boot a PythOS ISO under QEMU and validate the acceptance criteria.

Why a runner instead of plain `qemu ... && echo ok`?

QEMU's `isa-debug-exit` device does not let the guest choose its own host
exit status. When the guest writes VALUE to the device's I/O port, QEMU
terminates with:

    host_status = (VALUE << 1) | 1

The low bit is hard-wired to 1, so the guest can never produce status 0 --
status 0 is reserved for "the VM powered down normally". A naive runner that
treats any non-zero status as failure would therefore report a *successful*
kernel run as a failure. This script decodes the status instead.

    guest writes 0x10 -> host status 33  -> success
    guest writes 0x11 -> host status 35  -> deliberate panic

Anything else (0, a timeout, a QEMU-internal error, a signal) is a failure.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass

# Must stay in sync with kernel/include/qemu.h
GUEST_EXIT_SUCCESS = 0x10
GUEST_EXIT_FAILURE = 0x11


def host_status(value: int) -> int:
    """Host process exit status produced by a guest write of ``value``."""
    return (value << 1) | 1


STATUS_SUCCESS = host_status(GUEST_EXIT_SUCCESS)  # 33
STATUS_PANIC = host_status(GUEST_EXIT_FAILURE)  # 35

STATUS_NAMES = {
    0: "guest powered off without using isa-debug-exit",
    1: "QEMU itself failed to start (bad flags, missing ISO, ...)",
    STATUS_SUCCESS: f"guest wrote {GUEST_EXIT_SUCCESS:#04x} (success)",
    STATUS_PANIC: f"guest wrote {GUEST_EXIT_FAILURE:#04x} (panic)",
}


def describe_status(status: int) -> str:
    if status in STATUS_NAMES:
        return STATUS_NAMES[status]
    if status > 1 and status % 2 == 1:
        return f"guest wrote {(status - 1) >> 1:#04x} via isa-debug-exit"
    return "unexpected status"


# The kernel prints this exactly once per boot. Seeing it twice means the
# machine reset and started over -- the signature of a triple fault, which
# `-no-reboot` is supposed to turn into a clean exit instead. Every scenario
# is checked for this, because "it eventually printed the right thing, after
# rebooting" is not passing.
BANNER = re.compile(r"^PythOS \S+", re.M)


@dataclass
class Expectation:
    """What a given test mode requires of a run."""

    name: str
    status: int
    required: list[re.Pattern[str]]
    forbidden: list[re.Pattern[str]]


# Lines every successful boot must carry, whatever the scenario: the kernel
# identifies itself, Limine identifies itself, and the kernel reports where
# it was loaded.
COMMON_BOOT_LINES = [
    re.compile(r"^PythOS \S+", re.M),  # name/version
    re.compile(r"^\[boot\] bootloader: \S+ \S+", re.M),  # from Limine
    re.compile(r"^\[boot\] higher-half base: 0xffffffff[0-9a-f]+", re.M),
]

# M1 brings up its own descriptor tables before any fault can be injected,
# so these must appear in every scenario too.
CPU_INIT_LINES = [
    re.compile(r"^\[cpu\] gdt: loaded", re.M),
    re.compile(r"^\[cpu\] idt: 32 exception vectors installed", re.M),
]

# M3 switches to its own page tables on every boot, before any cmdline hook
# runs. If any of this is missing, the kernel is still on Limine's tables
# and nothing downstream means what it claims to.
VM_INIT_LINES = [
    # NX must be enabled before any table sets the NX bit, or every such
    # mapping faults on first use.
    re.compile(r"^\[cpu\] nx: supported \(efer\.nxe set\)", re.M),
    # SMEP and SMAP, asserted rather than merely reported. M3 ran with
    # `-cpu qemu64`, which advertises neither, so the detect-and-warn path
    # executed and the protection was never actually on. The Makefile now
    # asks for `+smep,+smap`; requiring the line here means a silent
    # regression in the CPU model -- or in cpu::init -- fails the test
    # instead of quietly dropping the protection again.
    re.compile(r"^\[cpu\] smep: enabled, smap: enabled, pge: enabled", re.M),
    re.compile(r"^\[vm\] pmap: \d+ MiB of RAM at 0xffff800000000000, rw\+nx\+global", re.M),
    # W^X: .text executable and not writable, .rodata neither.
    re.compile(r"^\[vm\] image \.text: .*PageFlags\(READ \| EXEC \| GLOBAL\)", re.M),
    re.compile(r"^\[vm\] image \.rodata: .*PageFlags\(READ \| GLOBAL\)", re.M),
    re.compile(r"^\[vm\] image \.writable: .*PageFlags\(READ \| WRITE \| GLOBAL\)", re.M),
    # The guard page is real now, not the dead space of M1/M2. Two stacks:
    # slot 0 for #DF via IST1, slot 1 for the kernel proper.
    re.compile(r"^\[vm\] kstack\[0\]: .* guard page at 0xffffc80000000000 \(ist1\)", re.M),
    re.compile(r"^\[vm\] kstack\[1\]: .* guard page at 0xffffc80000005000 \(main\)", re.M),
    # The pre-switch check is what stands between us and a triple fault.
    re.compile(r"^\[vm\] pre-switch check: \d+ image pages mapped", re.M),
    re.compile(r"^\[vm\] now running on our own page tables", re.M),
    # Off Limine's stack before anything poisons it. Limine allocates the
    # boot stack from BOOTLOADER_RECLAIMABLE memory, so reclaiming while
    # still standing on it overwrites the live stack -- which is what the
    # kernel did until this step existed, taking a #GP with
    # rip=0xdededededededede. The order of these two lines is the whole
    # point, so both are required.
    re.compile(r"^\[vm\] switching stack: 0x[0-9a-f]+ -> 0xffffc8000000[0-9a-f]+", re.M),
    re.compile(r"^\[boot\] now on the kernel's own stack", re.M),
    # Bootloader memory reclaimed, and the copies outlived it.
    re.compile(r"^\[mem\] poisoned \d+ KiB of reclaimable memory with 0xde", re.M),
    re.compile(r"^\[mem\] reclaimed \d+ frames \(\d+ KiB\) from bootloader-reclaimable", re.M),
    re.compile(r"^\[boot\] after reclaim: bootloader still reported as \S+ \S+", re.M),
]

# M4 brings the heap up on every boot, after reclaim. Required everywhere
# for the same reason as VM_INIT_LINES: if the heap is not there, nothing
# downstream that allocates means what it claims to.
HEAP_INIT_LINES = [
    re.compile(
        r"^\[heap\] base 0xffffc00000001000\.\.0xffffc00004001000 \(limit 64 MiB\), "
        r"\d+ page\(s\) mapped",
        re.M,
    ),
    # Both guards, named. The lower one catches pointer underflow, the
    # upper one an overrun past the limit; neither is ever mapped.
    re.compile(
        r"^\[heap\] guard pages: 0xffffc00000000000 \(below\), "
        r"0xffffc00004001000 \(above\)",
        re.M,
    ),
    re.compile(
        r"^\[heap\] 17 size classes, 16 B \.\. 1024 KiB; alloc and dealloc are "
        r"O\(1\) with no search",
        re.M,
    ),
]

# The poison pattern, as it appears when something follows a pointer into
# reclaimed bootloader memory. Forbidden everywhere: 0xDE was chosen so that
# a stale pointer produces a non-canonical address and an immediate fault
# rather than a plausible read, and seeing it in a register dump means
# something outlived the memory it pointed at.
POISON_IN_REGISTERS = re.compile(r"0xdededededededede", re.M)

# Lines the acceptance test demands on the serial console.
EXPECTATIONS = {
    "success": Expectation(
        name="normal boot",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            *VM_INIT_LINES,
            *HEAP_INIT_LINES,
            re.compile(r"^\[boot\] M4 checks passed", re.M),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            POISON_IN_REGISTERS,
        ],
    ),
    "panic": Expectation(
        name="panic test",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
            # The panic must name a real source file and line, not just say
            # "something went wrong". Rust's PanicInfo gives file:line:column.
            re.compile(r"^\[PANIC\] at \S+\.rs:\d+:\d+", re.M),
            re.compile(r"^\[PANIC\] deliberate panic requested", re.M),
        ],
        forbidden=[re.compile(r"M4 checks passed", re.M)],
    ),
    # --- M1: exception scenarios ----------------------------------------
    "int3": Expectation(
        name="breakpoint (#BP)",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            re.compile(r"^\*\*\* EXCEPTION: #BP breakpoint \(vector 3\) \*\*\*", re.M),
            # RIP must be a real higher-half kernel address, not 0 or junk:
            # this is what shows the frame was decoded correctly.
            re.compile(r"^\[trap\] error_code=0x0{16}\s+rip=0xffffffff[0-9a-f]{8}", re.M),
            re.compile(r"^\[trap\] #BP is a trap; resuming at rip=0xffffffff", re.M),
            # The whole point: the handler returned and the kernel carried on.
            re.compile(r"^\[fault\] returned from int3 -- handler resumed", re.M),
            re.compile(r"^\[boot\] execution continued past the breakpoint", re.M),
            re.compile(r"^\[boot\] M4 checks passed", re.M),
        ],
        forbidden=[re.compile(r"KERNEL PANIC", re.M)],
    ),
    "pf": Expectation(
        name="page fault (#PF)",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            re.compile(r"^\*\*\* EXCEPTION: #PF page fault \(vector 14\) \*\*\*", re.M),
            # Error code 0x2 = not-present (bit 0 clear), write (bit 1 set),
            # supervisor (bit 2 clear).
            re.compile(r"^\[trap\] error_code=0x0{15}2\b", re.M),
            # CR2 must hold the exact address the kernel tried to write.
            re.compile(r"^\[trap\] cr2=0x0000600000000000\s+\(supervisor write not-present\)", re.M),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
            re.compile(r"^\[PANIC\] unhandled exception: #PF page fault \(vector 14\)", re.M),
        ],
        forbidden=[re.compile(r"M4 checks passed", re.M)],
    ),
    "df": Expectation(
        name="double fault (#DF)",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            re.compile(r"^\[cpu\] ist1: 0xffffffff[0-9a-f]+ \.\. 0xffffffff", re.M),
            re.compile(r"^\*\*\* EXCEPTION: #DF double fault \(vector 8\) \*\*\*", re.M),
            # The kernel checks for itself that it is running on IST1 and
            # says so. "ok" is the only accepted answer; the alternative
            # branch prints "NOT ACTIVE", which `forbidden` also rejects.
            # Anchored with \s*$ rather than $: the serial driver emits CRLF,
            # so a bare $ would have to match before a stray \r.
            re.compile(r"^\[trap\] IST1: ok\s*$", re.M),
            re.compile(r"^\[trap\] unrecoverable; signalling QEMU", re.M),
        ],
        forbidden=[
            re.compile(r"M4 checks passed", re.M),
            re.compile(r"IST1: NOT ACTIVE", re.M),
        ],
    ),
    # --- M2: physical memory --------------------------------------------
    "memtest": Expectation(
        name="frame allocator",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            # The memory map was read and summarised.
            re.compile(r"^\[mem\] memory map: \d+ entries", re.M),
            re.compile(r"^\[mem\]   0x[0-9a-f]{16}\.\.0x[0-9a-f]{16}\s+\d+ KiB  usable", re.M),
            re.compile(r"^\[mem\] total \d+ MiB, usable \d+ MiB \(\d+ frames of 4 KiB\)", re.M),
            # The bitmap lives in the memory it manages, not in a static.
            re.compile(r"^\[mem\] bitmap: \d+ KiB for \d+ frames, at phys 0x[0-9a-f]+", re.M),
            re.compile(r"^\[mem\] allocator ready: \d+ free frames", re.M),
            # The kernel's own verdict on each phase.
            re.compile(r"^\[mem\] image frames verified in use: \d+\.\.=\d+", re.M),
            # The frames the kernel's own page tables and stacks hold are
            # counted and bounded. M2 asserted the count had not moved at
            # all, which M3 made false -- vm::init allocates. Narrowed
            # rather than dropped: an unbounded drift here is a leak during
            # boot, and nothing else would catch it.
            re.compile(
                r"^\[memtest\] \d+ frames held by boot structures .* within "
                r"the bound of \d+",
                re.M,
            ),
            re.compile(r"^\[memtest\] alloc: 10000 unique, aligned, in-range", re.M),
            re.compile(
                r"^\[memtest\] alloc: none in the kernel image, none is frame 0", re.M
            ),
            re.compile(r"^\[memtest\] free: count restored \(\d+\)", re.M),
            re.compile(
                r"^\[memtest\] exhausted after \d+ frames; allocate\(\) returned None cleanly",
                re.M,
            ),
            re.compile(r"^\[memtest\] recovery: freed .* and got it straight back", re.M),
            re.compile(r"^\[memtest\] all frame allocator tests passed", re.M),
            re.compile(r"^\[boot\] M4 checks passed", re.M),
        ],
        forbidden=[
            # Every invariant the kernel checks is an assert, so a violation
            # shows up as a panic. Forbidding it here means a weakened
            # in-kernel check cannot quietly turn into a pass.
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
        ],
    ),
    "doublefree": Expectation(
        name="double free",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            re.compile(r"^\[memtest\] first free \(legitimate\)", re.M),
            re.compile(r"^\[memtest\] second free of the same frame -- this must panic", re.M),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
            # The panic must name the condition and the frame, so the log
            # says which frame was freed twice.
            re.compile(
                r"^\[PANIC\] double free detected: frame \d+ @ 0x[0-9a-f]{16} is already free",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"M4 checks passed", re.M),
            # If this appeared, the allocator accepted the second free.
            re.compile(r"the allocator accepted a double free", re.M),
        ],
    ),
    # --- M3: virtual memory ---------------------------------------------
    "vmtest": Expectation(
        name="virtual memory",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            *VM_INIT_LINES,
            re.compile(r"^\[vmtest\] cpu: efer\.nxe set, w\^x is meaningful", re.M),
            re.compile(
                r"^\[vmtest\] cr3: own tables; pmap at 0xffff800000000000 is rw\+nx",
                re.M,
            ),
            # The kernel is on a stack it owns, with a guard page, in memory
            # nobody may reclaim. The kernel checks all three and says so.
            re.compile(
                r"^\[vmtest\] stack: rsp 0xffffc8[0-9a-f]+ in kstack\[1\].*"
                r"guard page at 0xffffc80000005000 unmapped, not reclaimable",
                re.M | re.S,
            ),
            # map -> write -> read back.
            re.compile(r"^\[vmtest\] map: 0x[0-9a-f]+ -> frame \d+ @ 0x[0-9a-f]+, wrote and read back", re.M),
            # translate agrees about address and flags.
            re.compile(r"^\[vmtest\] translate: 0x[0-9a-f]+ -> 0x[0-9a-f]+ PageFlags\(", re.M),
            # unmap returns the same frame.
            re.compile(r"^\[vmtest\] unmap: returned frame \d+ @ 0x[0-9a-f]+ as expected", re.M),
            # The leaf frame comes back; the intermediate tables do not, and
            # the kernel asserts the exact count rather than ignoring it.
            re.compile(
                r"^\[vmtest\] frames: leaf frame returned; 3 page-table frames "
                r"remain allocated by design",
                re.M,
            ),
            # All four error cases return errors rather than panicking --
            # this is what matters for M11, where the caller is user space.
            re.compile(r"^\[vmtest\] error: map over a live mapping -> AlreadyMapped", re.M),
            re.compile(r"^\[vmtest\] error: unmap an absent page -> NotMapped", re.M),
            re.compile(r"^\[vmtest\] error: map at non-canonical .* -> NotCanonical", re.M),
            re.compile(r"^\[vmtest\] error: map with WRITE\|EXEC -> InvalidFlags", re.M),
            re.compile(r"^\[vmtest\] error: all four cases returned errors, none panicked", re.M),
            # The copies outlived the memory they came from.
            re.compile(r"^\[vmtest\] reclaim: boot info survived", re.M),
            re.compile(r"^\[vmtest\] all virtual memory tests passed", re.M),
            re.compile(r"^\[boot\] M4 checks passed", re.M),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
        ],
    ),
    "staletlb": Expectation(
        name="stale TLB (advisory)",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *VM_INIT_LINES,
            re.compile(r"^\[vmtest\] stale-TLB check \(advisory", re.M),
            re.compile(r"^\[vmtest\] unmapped it WITHOUT invalidating the TLB", re.M),
            # Either outcome is accepted. QEMU's TLB is not obliged to
            # behave like real silicon: it usually drops the entry, in
            # which case the read faults and we never get here. When it
            # does not, the kernel says so and the check is advisory --
            # it proves the test ran, not that the hardware is correct.
            # Re-running this on real hardware is in the known gaps.
            re.compile(r"^\[vmtest\] read after unmap returned", re.M),
        ],
        forbidden=[re.compile(r"KERNEL PANIC", re.M)],
    ),
    "guardpage": Expectation(
        name="guard page",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *VM_INIT_LINES,
            re.compile(r"^\[vmtest\] guard page test: writing to 0xffffc80000000000", re.M),
            re.compile(r"^\*\*\* EXCEPTION: #PF page fault \(vector 14\) \*\*\*", re.M),
            # CR2 must be the exact byte written: the last qword of the
            # guard page, which is where an overflow running off the bottom
            # of the IST stack lands first. In M1/M2 this address was
            # mapped dead space and the write simply succeeded.
            re.compile(r"^\[trap\] cr2=0xffffc80000000ff8\b", re.M),
            # Error code 0x2: not-present (bit 0 clear), write (bit 1 set),
            # supervisor (bit 2 clear). A *present* page would mean the
            # guard is mapped and the test proved nothing.
            re.compile(r"^\[trap\] error_code=0x0{15}2\b", re.M),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
        ],
        forbidden=[
            re.compile(r"M4 checks passed", re.M),
            re.compile(r"the guard page did not fault", re.M),
        ],
    ),
    "nx": Expectation(
        name="NX enforcement",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *VM_INIT_LINES,
            re.compile(r"^\[vmtest\] nx test: jumping to 0xffff8", re.M),
            re.compile(r"^\*\*\* EXCEPTION: #PF page fault \(vector 14\) \*\*\*", re.M),
            # Error-code bit 4 is instruction-fetch, and the CPU only sets
            # it when EFER.NXE is enabled -- so this line is also proof
            # that cpu::init did its job.
            # Bits: P=1 (protection violation), I/D=1 (fetch) -> 0x11.
            re.compile(r"^\[trap\] error_code=0x0{14}11\b", re.M),
            # And the kernel must say so in words. The decoder used to
            # report bit 4 as an ordinary read, which made an NX violation
            # indistinguishable from a permission fault -- so matching the
            # hex alone would not prove the kernel understood what happened.
            re.compile(
                r"^\[trap\] cr2=0xffff8[0-9a-f]+\s+\(supervisor instruction fetch "
                r"protection-violation\)",
                re.M,
            ),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
        ],
        forbidden=[
            re.compile(r"M4 checks passed", re.M),
            re.compile(r"executing from an NX page succeeded", re.M),
        ],
    ),
    "stackovf": Expectation(
        name="main stack overflow",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            *VM_INIT_LINES,
            re.compile(r"^\[vmtest\] stack overflow test: recursing off the bottom", re.M),
            # RSP must actually descend through the stack. Requiring a depth
            # well past zero rules out a run where the recursion was
            # optimised into something that does not grow the stack, which
            # would make the test pass without testing anything.
            re.compile(r"^\[vmtest\] depth 8, rsp 0xffffc8000000[0-9a-f]+", re.M),
            # The recursion must reach the lowest page of the stack
            # (0xffffc80000006000..0xffffc80000006fff), not stop somewhere
            # comfortable above it.
            re.compile(r"^\[vmtest\] depth \d+, rsp 0xffffc80000006[0-9a-f]{3}", re.M),
            # And then: #DF, not #PF.
            #
            # The first draft of this expectation demanded #PF and forbade
            # #DF, which was wrong, and wrong in a way worth keeping a note
            # about. RSP descends into the guard page; the CPU wants to
            # deliver #PF; delivering it means pushing a five-qword frame
            # onto the stack -- the stack that just ran out. The push
            # faults, the fault cannot be delivered, and the CPU escalates
            # to #DF.
            #
            # This is precisely what IST1 is for, and it is the first time
            # in the project that a genuine (not manufactured) double fault
            # has exercised it. A kernel without the IST would triple-fault
            # here and reset, which the banner count would catch.
            re.compile(r"^\*\*\* EXCEPTION: #DF double fault \(vector 8\) \*\*\*", re.M),
            # The faulting RSP is at the very bottom of the stack or just
            # inside the guard page below it. Which exact byte depends on
            # how large a frame the compiler happened to be pushing when it
            # ran out, so both the stack's last page (0x...6xxx) and the
            # guard page (0x...5xxx) are legitimate; observed so far are
            # 0xffffc80000005fc0 and 0xffffc80000006000.
            #
            # What is *not* legitimate is an RSP anywhere else, which would
            # mean something other than the overflow caused this fault.
            re.compile(r"^\[trap\] rflags=.*rsp=0xffffc8000000[56][0-9a-f]{3}", re.M),
            # The handler got its own stack, inside IST1's range.
            re.compile(
                r"^\[trap\] IST1 stack: 0xffffc80000001000 \.\. 0xffffc80000005000, "
                r"handler rsp=0xffffc8000000[1-4][0-9a-f]{3}",
                re.M,
            ),
            re.compile(r"^\[trap\] IST1: ok\s*$", re.M),
            re.compile(r"^\[trap\] unrecoverable; signalling QEMU", re.M),
        ],
        forbidden=[
            re.compile(r"M4 checks passed", re.M),
            re.compile(r"the stack overflow did not fault", re.M),
            # If the IST had not engaged, the handler would be running on
            # the dead stack -- and would fault again, into a triple fault.
            re.compile(r"IST1: NOT ACTIVE", re.M),
        ],
    ),
    # --- M4: the kernel heap --------------------------------------------
    "heaptest": Expectation(
        name="kernel heap",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            *VM_INIT_LINES,
            *HEAP_INIT_LINES,
            re.compile(r"^\[heaptest\] initial: base 0xffffc00000001000", re.M),
            # The four collections, each exercising a different pattern:
            # one fixed allocation, two that reallocate as they grow, and
            # one that allocates many small nodes and frees them out of
            # order.
            re.compile(
                r"^\[heaptest\] collections: Box, Vec\(1000\), String\(400\), "
                r"BTreeMap\(500\) all allocate and read back correctly",
                re.M,
            ),
            # Ten thousand live allocations across every class.
            re.compile(
                r"^\[heaptest\] alloc: 10000 blocks, \d+ B \.\. \d+ KiB, every "
                r"one aligned and inside the heap",
                re.M,
            ),
            # The canary check is what proves no two blocks overlap. It
            # runs after *all* the allocations, so an overlap written by a
            # later block is still visible in an earlier one.
            re.compile(
                r"^\[heaptest\] canaries: all 10000 blocks intact; no two "
                r"allocations overlap",
                re.M,
            ),
            # Internal fragmentation, measured rather than asserted in the
            # abstract. ADR 016 claims at most 2x for power-of-two classes.
            re.compile(
                r"^\[heaptest\] fragmentation: \d+ bytes requested, \d+ bytes "
                r"reserved \(1\d\d% -- the size classes guarantee at most 200%\)",
                re.M,
            ),
            # The heap grows on demand, and both ends of the new region are
            # really writable -- not merely accounted for.
            re.compile(
                r"^\[heaptest\] growth: \d+ bytes requested, heap grew from \d+ "
                r"to \d+ pages, both ends writable",
                re.M,
            ),
            # Back to the starting byte count. The pages stay mapped, and
            # that is reported rather than hidden.
            re.compile(
                r"^\[heaptest\] baseline: 0 bytes in use \(was 0\), \d+ pages "
                r"still mapped",
                re.M,
            ),
            # Where the freed memory went. A class with free blocks cannot
            # lend them to another class -- the cost of not coalescing,
            # made visible.
            re.compile(
                r"^\[heaptest\] free lists: \d+ blocks across \d+ of 17 classes",
                re.M,
            ),
            re.compile(r"^\[heaptest\] all kernel heap tests passed", re.M),
            re.compile(r"^\[boot\] M4 checks passed", re.M),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
        ],
    ),
    "heapoom": Expectation(
        name="heap exhaustion",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *HEAP_INIT_LINES,
            re.compile(r"^\[heaptest\] exhausting the heap", re.M),
            # The heap must actually grow on the way. Without this the
            # test would pass even if every allocation were optimised
            # away -- which is precisely what happened the first time it
            # was run, before the volatile writes were added.
            re.compile(
                r"^\[heaptest\] \d+ MiB allocated and leaked, \d{4,} pages mapped",
                re.M,
            ),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
            # The message must name the size, the alignment and the class,
            # so that a reader can tell what was being asked for.
            re.compile(
                r"^\[PANIC\] heap exhausted: allocating \d+ bytes \(align \d+, "
                r"class \d+\) would take the heap to \d+ KiB, past its 65536 "
                r"KiB limit\. \d+ bytes are live in \d+ allocations\.",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"M4 checks passed", re.M),
            # A null return instead of a panic: out of memory must be loud.
            re.compile(r"returned null instead of panicking", re.M),
            # The heap handing out more than its limit would mean the bound
            # is not enforced.
            re.compile(r"without hitting its \d+ MiB limit", re.M),
        ],
    ),
    "heapguard": Expectation(
        name="heap guard page",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *HEAP_INIT_LINES,
            re.compile(
                r"^\[heaptest\] guard page test: writing to 0xffffc00004001000",
                re.M,
            ),
            re.compile(r"^\*\*\* EXCEPTION: #PF page fault \(vector 14\) \*\*\*", re.M),
            # CR2 is the exact address written: the first byte above the
            # heap's limit.
            re.compile(r"^\[trap\] cr2=0xffffc00004001000\b", re.M),
            # Not-present, write, supervisor. A *present* page here would
            # mean the guard is mapped and the test proved nothing.
            re.compile(r"^\[trap\] error_code=0x0{15}2\b", re.M),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
        ],
        forbidden=[
            re.compile(r"M4 checks passed", re.M),
            re.compile(r"the heap's upper guard page did not fault", re.M),
        ],
    ),
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--qemu", default="qemu-system-x86_64")
    parser.add_argument("--iso", required=True)
    parser.add_argument("--expect", required=True, choices=sorted(EXPECTATIONS))
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument(
        "--self-test-expect-failure",
        action="store_true",
        help=(
            "invert the result: the run MUST fail its expectations. Used to "
            "prove the runner actually validates, by feeding it a panic ISO "
            "while asking for success. A runner that passed everything would "
            "otherwise be indistinguishable from a working one."
        ),
    )
    parser.add_argument(
        "--log",
        default=None,
        help="write the captured serial output here (default: alongside the ISO)",
    )
    parser.add_argument(
        "qemu_args",
        nargs=argparse.REMAINDER,
        help="extra QEMU flags, after a literal --",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    expect = EXPECTATIONS[args.expect]

    if not os.path.isfile(args.iso):
        print(f"error: ISO not found: {args.iso}", file=sys.stderr)
        return 1

    qemu = shutil.which(args.qemu)
    if qemu is None:
        print(
            f"error: {args.qemu} not found in PATH.\n"
            f"       Run `make toolchain-check` for installation hints.",
            file=sys.stderr,
        )
        return 1

    extra = list(args.qemu_args)
    if extra and extra[0] == "--":
        extra = extra[1:]

    cmd = [qemu, *extra, "-cdrom", args.iso, "-boot", "d"]

    print(f"[run] {expect.name}: {' '.join(cmd)}")
    started = time.monotonic()

    try:
        completed = subprocess.run(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=args.timeout,
            check=False,
        )
        output = completed.stdout.decode("utf-8", errors="replace")
        status = completed.returncode
        timed_out = False
    except subprocess.TimeoutExpired as exc:
        output = (exc.stdout or b"").decode("utf-8", errors="replace")
        status = None
        timed_out = True

    elapsed = time.monotonic() - started

    log_path = args.log or os.path.splitext(args.iso)[0] + f".{args.expect}.log"
    os.makedirs(os.path.dirname(log_path) or ".", exist_ok=True)
    with open(log_path, "w", encoding="utf-8") as handle:
        handle.write(output)

    print("--- serial output " + "-" * 50)
    print(output.rstrip("\n"))
    print("-" * 68)

    failures: list[str] = []

    # Triple-fault check, applied to every scenario.
    #
    # A triple fault resets the machine. With `-no-reboot` QEMU is supposed
    # to terminate instead, but if that flag were ever dropped -- or if the
    # firmware restarted us some other way -- the kernel would simply boot
    # again and might then print everything the test expects. Counting the
    # banner catches that: the kernel emits it exactly once per boot, so two
    # occurrences mean we went round twice.
    #
    # This matters most for the #DF scenario: without a working IST, the
    # double fault escalates to a triple fault, and a reboot loop is exactly
    # what we would see.
    boot_count = len(BANNER.findall(output))
    if boot_count > 1:
        failures.append(
            f"kernel banner appeared {boot_count} times: the machine reset "
            f"and re-booted (triple fault), it did not run once cleanly"
        )
    elif boot_count == 0:
        failures.append(
            "kernel banner never appeared: the kernel did not reach its "
            "first serial write"
        )

    if timed_out:
        failures.append(
            f"QEMU did not exit within {args.timeout:.0f}s; the kernel most "
            f"likely hung before reaching isa-debug-exit"
        )
    elif status != expect.status:
        failures.append(
            f"exit status {status} ({describe_status(status)}); "
            f"expected {expect.status} ({describe_status(expect.status)})"
        )

    for pattern in expect.required:
        if not pattern.search(output):
            failures.append(f"missing expected serial line: /{pattern.pattern}/")

    for pattern in expect.forbidden:
        if pattern.search(output):
            failures.append(f"unexpected serial line present: /{pattern.pattern}/")

    if args.self_test_expect_failure:
        # Inverted mode: the run is supposed to violate its expectations.
        if failures:
            print(
                f"[PASS] runner self-test: '{expect.name}' expectations "
                f"correctly rejected this run ({len(failures)} violation(s)):"
            )
            for failure in failures:
                print(f"       - {failure}")
            return 0

        print(
            "[FAIL] runner self-test: the runner ACCEPTED a run it should "
            "have rejected. Its checks are not actually validating anything.",
            file=sys.stderr,
        )
        return 1

    if failures:
        print(f"[FAIL] {expect.name} ({elapsed:.1f}s)", file=sys.stderr)
        for failure in failures:
            print(f"       - {failure}", file=sys.stderr)
        print(f"       serial log: {log_path}", file=sys.stderr)
        return 1

    print(
        f"[PASS] {expect.name}: exit status {status} "
        f"({describe_status(status)}), {elapsed:.1f}s"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())