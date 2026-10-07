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
    # M5: the heap draws from a reserve taken before the frame allocator
    # retired, not from the allocator itself. Required on every boot --
    # without it the first allocation after boot panics, which is exactly
    # what happened the first time M5 ran.
    re.compile(
        r"^\[heap\] reserve: 64 MiB of contiguous physical memory at "
        r"0x[0-9a-f]{16}, taken before the frame allocator retired",
        re.M,
    ),
    # 64 MiB and not a byte less: the reserve covers the heap's whole
    # limit plus the page tables that address it. Sized at exactly the
    # limit, the heap ran out of reserve 34 frames short of its own 64
    # MiB -- which is what the tables cost -- and `heap-oom` reported
    # the wrong exhaustion.
]

# M5 brings the capability system up on every boot, and -- critically --
# retires the frame allocator as it does. Required everywhere, because
# every scenario after this point runs on a kernel whose only source of
# memory is a capability.
CAP_INIT_LINES = [
    # The measured object sizes. ADR 017's table is a set of ceilings; the
    # build enforces them, and these lines are what makes the real figures
    # visible rather than merely bounded.
    re.compile(r"^\[cap\] slot 32 B; Untyped 2\^4\.\.2\^47; CNode 32 B/slot", re.M),
    re.compile(
        r"^\[cap\] TCB 2048 B \(1024 context \+ 1024 ipc\), Endpoint 32 B, "
        r"Notification 64 B",
        re.M,
    ),
    re.compile(r"^\[cap\] limits: retype count <= 256, derivation depth <= 8", re.M),
    # The CSpace has the shape ADR 018 requires: a root of CNodes, two
    # levels, no more.
    re.compile(
        r"^\[cap\] init cspace: root radix 4 at 0x[0-9a-f]{16}, untyped cnode "
        r"radix 8, object cnode radix 8",
        re.M,
    ),
    re.compile(r"^\[cap\] \d+ root untypeds covering \d+ MiB", re.M),
    # Every frame the allocator had is now either kernel memory or Untyped
    # memory. Requiring "0 after" rather than a vague "fewer" is the point:
    # a frame left behind would be memory nobody can account for.
    re.compile(
        r"^\[cap\] frame allocator: \d+ free before, 0 after; \d+ frames became "
        r"untyped memory",
        re.M,
    ),
    re.compile(
        r"^\[cap\] frame allocator retired; allocation now goes through retype "
        r"only",
        re.M,
    ),
]

# M6 brings up threads and interrupts on every boot. Required
# everywhere below the capability system, because from here on every
# line of the kernel runs with interrupts enabled -- the assumption
# that held from M2 to M5 is gone and these lines are the evidence
# that what replaced it is in place.
THREAD_INIT_LINES = [
    # The boot thread gets a TCB before anything can switch. Without
    # it the first timer tick would have nowhere to save registers.
    re.compile(
        r"^\[cap\] boot tcb: object slot \d+, stack 0x[0-9a-f]{16} \.\. "
        r"0x[0-9a-f]{16} \(the main stack, mapped by vm::init\)",
        re.M,
    ),
    re.compile(
        r"^\[sched\] boot thread adopted: tcb at 0x[0-9a-f]{16}, stack "
        r"0x[0-9a-f]{16}\.\.0x[0-9a-f]{16}",
        re.M,
    ),
    # The legacy PIC must be masked before IF is set: the 8259s deliver
    # to vectors that overlap the CPU's own exceptions, so a stray IRQ0
    # would arrive as #DF.
    re.compile(r"^\[apic\] legacy 8259 PICs masked \(all 16 lines\)", re.M),
    re.compile(
        r"^\[apic\] lapic id \d+, version 0x[0-9a-f]+, registers at "
        r"0xffffd00000000000 \(phys 0x[0-9a-f]{16}\)",
        re.M,
    ),
    # 100 Hz on vector 32. Requiring the vector rules out a timer
    # wired to a vector the IDT does not populate, which would fault
    # rather than tick.
    re.compile(
        r"^\[apic\] timer: 100 Hz on vector 32, divisor 16, initial count "
        r"\d+ \(calibrated to \d+ ticks/s\)",
        re.M,
    ),
    re.compile(
        r"^\[boot\] interrupts enabled; the \"IF is always clear\" assumption",
        re.M,
    ),
]

# M7 records the kernel's top-level page-table entries on every boot,
# before interrupts are enabled. Required everywhere: every VSpace
# created later inherits exactly this set, and the check that it has
# not changed since (ADR 027's K2) has nothing to compare against
# without it.
VSPACE_INIT_LINES = [
    re.compile(
        r"^\[vspace\] kernel half: \d+ of 256 top-level entries in use; "
        r"every VSpace shares them",
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
            *CAP_INIT_LINES,
            re.compile(r"^\[boot\] M7 checks passed", re.M),
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
        forbidden=[re.compile(r"M7 checks passed", re.M)],
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
            re.compile(r"^\[boot\] M7 checks passed", re.M),
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
        forbidden=[re.compile(r"M7 checks passed", re.M)],
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
            re.compile(r"M7 checks passed", re.M),
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
            # M5 added the heap's physical reserve, which is 16384 frames
            # and would have swamped this bound. It is subtracted by name
            # rather than folded into a larger bound, so the check on
            # per-frame boot allocation stays exactly as sharp as it was.
            re.compile(
                r"^\[memtest\] \d+ frames held by boot structures .* within "
                r"the bound of \d+; the heap's \d+ frame reserve is counted "
                r"separately",
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
        ],
        forbidden=[
            # Every invariant the kernel checks is an assert, so a violation
            # shows up as a panic. Forbidding it here means a weakened
            # in-kernel check cannot quietly turn into a pass.
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            # This scenario exits as soon as its own checks pass, before
            # the capability system comes up. It has to: the last phase
            # deliberately drains the frame allocator, and cap::boot needs
            # frames -- it panicked with "could not find 2 contiguous
            # frames for a CNode" when this ran to completion. Requiring
            # the line's *absence* keeps the two facts tied together.
            re.compile(r"M7 checks passed", re.M),
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
            re.compile(r"M7 checks passed", re.M),
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
            re.compile(r"^\[boot\] M7 checks passed", re.M),
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
            re.compile(r"M7 checks passed", re.M),
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
            re.compile(r"M7 checks passed", re.M),
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
            re.compile(r"M7 checks passed", re.M),
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
            *CAP_INIT_LINES,
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
            # really writable -- not merely accounted for. From M5 the
            # growth must also be paid for out of the heap's reserve, and
            # the two figures must agree: a page mapped without being
            # charged would leave the budget draining at the wrong rate
            # while the page count looked perfectly healthy.
            re.compile(
                r"^\[heaptest\] growth: \d+ bytes requested, heap grew from \d+ "
                r"to \d+ pages, both ends writable; reserve \d+ MiB -> \d+ MiB "
                r"\(\d+ KiB spent for \d+ pages\)",
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
            # What the run cost in physical memory, split between the
            # heap's pages and the page tables that address them. Both
            # come from the same reserve: a budget that covered the data
            # but not the structure needed to reach it would not be one.
            re.compile(
                r"^\[heaptest\] reserve: \d+ KiB of \d+ MiB spent -- \d+ KiB "
                r"of heap pages plus \d+ frames of page tables; the heap "
                r"never returns a page",
                re.M,
            ),
            re.compile(r"^\[heaptest\] all kernel heap tests passed", re.M),
            re.compile(r"^\[boot\] M7 checks passed", re.M),
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
            *CAP_INIT_LINES,
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
            re.compile(r"M7 checks passed", re.M),
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
            *CAP_INIT_LINES,
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
            re.compile(r"M7 checks passed", re.M),
            re.compile(r"the heap's upper guard page did not fault", re.M),
        ],
    ),
    # --- M5: capabilities ------------------------------------------------
    "captest": Expectation(
        name="capability system",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            *VM_INIT_LINES,
            *HEAP_INIT_LINES,
            *CAP_INIT_LINES,
            # 1. Object sizes, measured and printed, plus the alignment
            #    rule retype's placement arithmetic depends on.
            re.compile(
                r"^\[captest\] sizes: slot 32 B, TCB 2048 B \(1024 ipc\), "
                r"Endpoint 32 B, Notification 64 B",
                re.M,
            ),
            re.compile(
                r"^\[captest\] sizes: every alignment is a power of two >= size",
                re.M,
            ),
            # 1b. The rights lattice, exhaustively. 81 of 256 pairs are
            #     subsets -- 3^4, one choice of three per independent bit.
            #     Requiring the figure rather than "some" means a check
            #     that silently stopped testing would fail here.
            re.compile(
                r"^\[captest\] rights algebra: all 256 pairs checked against "
                r"an independent definition; 81 subsets as predicted by 3\^4",
                re.M,
            ),
            # 2. The root Untypeds, and the sum agreeing with what boot
            #    counted separately.
            re.compile(
                r"^\[captest\] root untypeds: \d+ covering \d+ MiB, sizes \d+ "
                r"KiB \.\. \d+ KiB, all power-of-two and self-aligned",
                re.M,
            ),
            # 3. Three size classes retyped from one Untyped, and the new
            #    CNode demonstrably zeroed.
            re.compile(
                r"^\[captest\] retype: CNode\(16\) \+ Endpoint \+ Frame; "
                r"watermark \d+ -> \d+ \(\+\d+ B for \d+ B of objects\); the "
                r"new CNode's 16 slots are all zero",
                re.M,
            ),
            # 4. Atomicity: the watermark unmoved *and* the destination
            #    slots untouched. Either alone could pass by luck.
            re.compile(
                r"^\[captest\] atomicity: 256 frames \(\d+ B\) refused by a "
                r"\d+ B Untyped; watermark still \d+, all \d+ destination "
                r"slots still empty",
                re.M,
            ),
            # 4b. An occupied destination is refused by every operation
            #     that writes a capability, and the occupant survives
            #     intact.
            #
            #     This was missing from the first version of the suite.
            #     Deleting retype's emptiness check -- turning it into a
            #     no-op -- left all nineteen scenarios passing, which
            #     means the check was not being tested at all. The
            #     atomicity line above exercises only the
            #     NotEnoughMemory path.
            re.compile(
                r"^\[captest\] occupied slot: retype, copy and mint all "
                r"refuse with SlotNotEmpty; the Endpoint occupant keeps its "
                r"object, rights, depth and generation, and still resolves",
                re.M,
            ),
            #     The obstacle goes in the *middle* of the range.
            #     "Refuse if the first slot is taken" is easy to satisfy
            #     by accident: an implementation that validated each
            #     slot immediately before writing it would pass that and
            #     still corrupt the slots preceding a later obstacle.
            re.compile(
                r"^\[captest\] partial overlap: a 4-object retype over slots "
                r"0\.\.4 with slot 2 occupied wrote nothing, not even the 2 "
                r"slots before the obstacle; watermark still \d+",
                re.M,
            ),
            # 4c. The error paths nothing else reaches.
            #
            #     Listing CapError's variants against the ones captest
            #     mentions showed three were never produced. The most
            #     serious is RootSlotMustBeCNode: it is the *only*
            #     enforcement of ADR 018's two-level rule, and that rule
            #     is half of the CPtr invariant. With it removed, a
            #     pointer through an Endpoint-holding root slot read the
            #     Endpoint's bytes as a slot array and returned a "Null
            #     capability".
            re.compile(
                r"^\[captest\] remaining errors: RootSlotMustBeCNode fires "
                r"for a non-CNode root slot; TooManyObjects at 257 but not "
                r"at 256; InvalidArgument for count 0, a sized Endpoint and "
                r"a radix-0 CNode",
                re.M,
            ),
            # 5-6. Narrowing works and widening does not, through either
            #      door -- copy or mint.
            re.compile(
                r"^\[captest\] copy: Endpoint R\|W\|G\|Gr -> W, same object",
                re.M,
            ),
            re.compile(
                r"^\[captest\] rights: W-only cap asked for R\|W -> "
                r"RightsEscalation; asked to mint -> InsufficientRights\(GRANT\); "
                r"destination untouched",
                re.M,
            ),
            # 7-9. Every way a CPtr can fail gives its own error, so a
            #      caller can tell a typo from a stale pointer.
            re.compile(
                r"^\[captest\] cptr: a valid pointer resolves; reserved-bits, "
                r"out-of-range, wrong-type, empty-slot and stale-generation "
                r"each give their own error",
                re.M,
            ),
            # 10. The invariant the first draft of ADR 018 broke.
            re.compile(
                r"^\[captest\] cptr invariant: slot 0 was Endpoint gen \d+, is "
                r"now Notification gen \d+; the old pointer gives StaleCPtr and "
                r"never the new object; a wrong root generation is caught too",
                re.M,
            ),
            # 11. Depth 8 reachable, depth 9 refused, nothing written.
            re.compile(
                r"^\[captest\] depth: chain reached 8 in \d+ copies; the next "
                r"gave DerivationTooDeep and wrote nothing",
                re.M,
            ),
            # 12. Device memory may only become a Frame -- every other
            #     type checked individually.
            re.compile(
                r"^\[captest\] device untyped: Frame allowed and stays "
                r"device-marked; all 7 other types refused with "
                r"DeviceUntypedFrameOnly",
                re.M,
            ),
            # 13. The distinction between delete and revoke, demonstrated
            #     rather than asserted.
            re.compile(
                r"^\[captest\] delete vs revoke: delete removed 1 of 3 and the "
                r"grandchild survived as a live R capability; revoke then "
                r"removed that descendant and left the origin",
                re.M,
            ),
            # 14. Revoke returns the memory, and the proof is that a new
            #     object lands back where the old ones were.
            re.compile(
                r"^\[captest\] revoke: 8 descendants deleted in \d+ visits; "
                r"Untyped survives, watermark \d+ -> \d+, a new object reused "
                r"0x[0-9a-f]+ \(was 0x[0-9a-f]+\), old pointers return errors",
                re.M,
            ),
            re.compile(r"^\[captest\] all capability tests passed", re.M),
            re.compile(r"^\[boot\] M7 checks passed", re.M),
        ],
        forbidden=[
            # Every invariant above is an assert, so a violation is a
            # panic. Forbidding panics means a weakened in-kernel check
            # cannot quietly become a pass.
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
        ],
    ),
    "caprevoke": Expectation(
        name="revoke cost (measurement)",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CAP_INIT_LINES,
            # The tree must be both as wide as the CSpace allows and as
            # deep as the cap permits. A shallower tree would be measuring
            # something easier than the case ADR 019 estimated.
            re.compile(
                r"^\[captest\] tree: 256 capabilities from one Untyped, "
                r"deepest level 8, \d+ chain restarts",
                re.M,
            ),
            re.compile(
                r"^\[captest\] revoke timing: 256 capabilities in \d+ TSC "
                r"cycles \(\d+ cycles each, \d+ slots visited\)",
                re.M,
            ),
            # The extrapolation must carry its caveats in the line itself.
            # A bare millisecond figure from 256 samples under emulation
            # would read as a measurement, and it is not one.
            re.compile(
                r"^\[captest\] extrapolated: 8400000 slots would be ~\d+ "
                r"cycles, ~\d+ ms at 3 GHz -- extrapolated from 256 samples "
                r"under emulation, not a measurement of the worst case",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
        ],
    ),
    "capintalloc": Expectation(
        name="allocation from an interrupt",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *CAP_INIT_LINES,
            re.compile(
                r"^\[captest\] interrupt depth is 1; calling allocate\(\) "
                r"\(must panic\)",
                re.M,
            ),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
            # The message must name the rule, not merely report a failure.
            # "The allocator panicked" would be far less useful than
            # "something allocated from an interrupt handler".
            re.compile(
                r"^\[PANIC\] frame allocation from inside an interrupt handler",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"M7 checks passed", re.M),
            re.compile(r"the ADR 022 tripwire is not working", re.M),
        ],
    ),
    "capretired": Expectation(
        name="allocation after retirement",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *CAP_INIT_LINES,
            re.compile(
                r"^\[captest\] allocator is retired; calling allocate\(\) "
                r"\(must panic\)",
                re.M,
            ),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
            re.compile(
                r"^\[PANIC\] frame allocation after the allocator was retired",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"M7 checks passed", re.M),
            re.compile(r"the ADR 022 retirement check is not working", re.M),
        ],
    ),
    # --- M6: threads and interrupts --------------------------------------
    "threadtest": Expectation(
        name="threads and the scheduler",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            *VM_INIT_LINES,
            *HEAP_INIT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            # 1. The TCB is the object ADR 017 fixed.
            re.compile(
                r"^\[thread\] tcb: 2048 B, 2048-byte aligned, 64 B context, "
                r"1024 B reserved for the IPC buffer",
                re.M,
            ),
            # 2. The interrupt flag, in each of the three kinds of
            #    region ADR 025 names. The third is the one that would
            #    be easy to get wrong by being too careful: revoke runs
            #    with interrupts *on*.
            re.compile(
                r"^\[thread\] IF discipline: ordinary kernel code runs with IF=1",
                re.M,
            ),
            re.compile(
                r"^\[thread\] IF discipline: inside an InterruptGuard, IF=0",
                re.M,
            ),
            re.compile(
                r"^\[thread\] IF discipline: nested guards do not re-open the "
                r"outer region",
                re.M,
            ),
            re.compile(
                r"^\[thread\] IF discipline: inside a PreemptGuard, IF=1 and "
                r"preemption is off",
                re.M,
            ),
            re.compile(
                r"^\[thread\] IF discipline: revoke ran and left IF=1",
                re.M,
            ),
            # 3. Two threads, two stacks, 200 handoffs. Requiring both
            #    KSTACK slot numbers rules out two threads sharing one
            #    stack, which every other check here would survive.
            #
            #    200 or 201, and the alternation is the point rather
            #    than laziness. Each thread increments the counter and
            #    only then looks at it, so the one that reaches 200
            #    suspends itself while its partner may still be on the
            #    ready queue -- a timer tick runs it once more before
            #    it notices. With two threads the overshoot is exactly
            #    one. `\d+` here would also accept 202, which would
            #    mean a thread ran after suspending itself; the kernel
            #    asserts the same bound, and this pattern is the
            #    outside check on it.
            #
            #    Before M6 the figure was deterministic because nothing
            #    could interrupt a thread between the increment and the
            #    test. Enabling interrupts made the exact value a
            #    timing accident -- an intermittent failure in roughly
            #    one run in thirty, which is how this was found.
            re.compile(
                r"^\[thread\] cooperative: 20[01] rounds over \d+ switches; A "
                r"at 0x[0-9a-f]{16} \(ran \d+x, kstack slot 2\), B at "
                r"0x[0-9a-f]{16} \(ran \d+x, kstack slot 3\); neither RSP is "
                r"in the other's stack",
                re.M,
            ),
            # 4. The timer fires, and keeps firing -- a handler that
            #    forgets its EOI delivers exactly one interrupt.
            re.compile(
                r"^\[thread\] timer: \d{3,} interrupts taken, \d+ ticks "
                r"processed, \d+ deferred by preempt-disabled regions, 0 "
                r"spurious",
                re.M,
            ),
            # 5. Preemption, which is the whole reason for the timer: a
            #    thread that never yields is still taken off the CPU.
            #
            #    "started with IF=1" is required text, not decoration.
            #    A thread whose first instruction runs with interrupts
            #    disabled can never be preempted, and if it also never
            #    yields it holds the CPU permanently -- the timer is
            #    masked in its own RFLAGS. That was a real bug (ADR
            #    025): the sti that ends a switch's cli region sits on
            #    the line after switch_context returns, which a thread
            #    that has never run does not pass through. It showed up
            #    as a thirty-second hang. Requiring the flag's value
            #    here turns a repeat into a named failure.
            re.compile(
                r"^\[thread\] preemption: the spinner started with IF=1, ran "
                r"\d+ iterations without yielding; \d+ ticks and \d+ switches "
                r"brought control back here",
                re.M,
            ),
            # 6. Suspend takes a thread out of the rotation.
            re.compile(
                r"^\[thread\] suspend/resume: a suspended thread was skipped "
                r"by \d+ yields, then resumed and ran again",
                re.M,
            ),
            # 7. ReadRegisters, with RSP checked against *that thread's*
            #    stack -- the check that catches a switch saving the
            #    wrong RSP.
            re.compile(
                r"^\[thread\] ReadRegisters: suspended thread at "
                r"rip=0xffffffff[0-9a-f]{8}, rsp=0x[0-9a-f]{16} \(inside its "
                r"own stack\), \d+ switches, priority 42 \(stored, not used",
                re.M,
            ),
            re.compile(
                r"^\[thread\] invocations: \d+ went through the capability path",
                re.M,
            ),
            # 8. ADR 026's two-step kill.
            re.compile(
                r"^\[thread\] kill a running thread: SetRegisters and Resume "
                r"both give ThreadBusy; Suspend always works",
                re.M,
            ),
            # The measurement. M9's IPC will be compared against it, so
            # the caveat about emulation is required text, not decoration.
            re.compile(
                r"^\[thread\] switch cost: \d+ switches in \d+ TSC cycles "
                r"\(\d+ cycles each\)",
                re.M,
            ),
            re.compile(
                r"^\[thread\] switch cost: ~\d+ ns at 3 GHz -- measured under "
                r"emulation, where the TSC is virtual and the figure is an "
                r"order of magnitude, not a measurement",
                re.M,
            ),
            re.compile(r"^\[thread\] all thread tests passed", re.M),
            re.compile(r"^\[boot\] M7 checks passed", re.M),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
        ],
    ),
    "threadovf": Expectation(
        name="thread stack overflow",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            # The thread names its own stack before overflowing. The
            # #DF handler cannot do it afterwards: by then the thread's
            # stack is unusable and anything read through it is
            # suspect.
            re.compile(
                r"^\[thread\] overflowing: stack 0xffffc800[0-9a-f]{8}\.\."
                r"0xffffc800[0-9a-f]{8}, rsp now 0xffffc800[0-9a-f]{8}",
                re.M,
            ),
            # RSP must actually descend. A recursion the optimiser
            # turned into a loop would never reach the guard, and the
            # test would hang rather than fault -- which is how M3's
            # version of this passed for the wrong reason.
            re.compile(r"^\[thread\] depth 8, rsp 0xffffc800[0-9a-f]{8}", re.M),
            re.compile(r"^\[thread\] depth \d{2,}, rsp 0xffffc800[0-9a-f]{8}", re.M),
            # And then #DF on IST1, for the same reason as M3's main
            # stack overflow: delivering the #PF needs the stack that
            # just ran out.
            re.compile(r"^\*\*\* EXCEPTION: #DF double fault \(vector 8\) \*\*\*", re.M),
            re.compile(r"^\[trap\] IST1: ok\s*$", re.M),
            re.compile(r"^\[trap\] unrecoverable; signalling QEMU", re.M),
        ],
        forbidden=[
            re.compile(r"M7 checks passed", re.M),
            re.compile(r"IST1: NOT ACTIVE", re.M),
            re.compile(r"the thread returned from an infinite recursion", re.M),
        ],
    ),
    "threadintalloc": Expectation(
        name="allocation inside a real interrupt",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            re.compile(
                r"^\[thread\] armed; waiting for the next timer tick",
                re.M,
            ),
            # The interrupt-depth tripwire must read true *inside the
            # handler*. M5's version of this test set the counter by
            # hand because there were no interrupts; this is the path
            # that matters.
            re.compile(
                r"^\[thread\] inside the timer handler, in_interrupt=true; "
                r"calling allocate\(\) \(must panic\)",
                re.M,
            ),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
            re.compile(
                r"^\[PANIC\] frame allocation from inside an interrupt handler",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"M7 checks passed", re.M),
            re.compile(r"does not work on the path that matters", re.M),
            re.compile(r"no timer interrupt arrived within the deadline", re.M),
        ],
    ),
    "threadkill": Expectation(
        name="operate on a running thread",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            re.compile(
                r"^\[thread\] the victim is (Ready|Running); attempting "
                r"SetRegisters",
                re.M,
            ),
            re.compile(
                r"^\[thread\] refused with ThreadBusy\((Ready|Running)\), as "
                r"ADR 026 requires",
                re.M,
            ),
            # The refusal must not be a dead end: ADR 026's whole
            # argument for choosing "refuse" over "force" rests on the
            # second step always working.
            re.compile(
                r"^\[thread\] after Suspend the same call succeeds: the "
                r"refusal is a sequencing requirement, not a dead end",
                re.M,
            ),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
            re.compile(
                r"^\[PANIC\] thread-kill-running completed its checks",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"M7 checks passed", re.M),
            re.compile(r"instead of ThreadBusy", re.M),
        ],
    ),
    # --- M7: address spaces ----------------------------------------------
    "vspacetest": Expectation(
        name="address spaces",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            *VM_INIT_LINES,
            *HEAP_INIT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            *VSPACE_INIT_LINES,
            # 1. The kernel half is shared, and the boot thread runs in
            #    the kernel's own address space.
            re.compile(
                r"^\[vspace\] kernel half: \d+ top-level entries, shared by "
                r"every address space; cr3 is 0x[0-9a-f]{16}",
                re.M,
            ),
            # 2. An address space assembled entirely out of
            #    capabilities: a root, three tables the caller supplied,
            #    one Frame.
            re.compile(
                r"^\[vspace\] built from capabilities: 0x0{10}400000 -> "
                r"0x[0-9a-f]{16} PageFlags\(",
                re.M,
            ),
            # 3. The capability path and M3's paging agree. They are
            #    reading the same tables, so a disagreement would mean
            #    one of them is walking something else entirely.
            re.compile(
                r"^\[vspace\] translate: the capability path and M3's paging "
                r"agree",
                re.M,
            ),
            # 4. Rights become permissions, checked in both directions:
            #    WRITE withheld maps read-only, and no rights at all is
            #    refused rather than mapped untouchable.
            re.compile(
                r"^\[vspace\] rights: WRITE withheld maps read-only .*; no "
                r"rights at all is refused with InsufficientRights",
                re.M,
            ),
            # 5. The check that cannot be faked. Two threads, one
            #    virtual address, two different values. Every other line
            #    in this scenario would pass on a kernel with a single
            #    address space and careful bookkeeping; this one would
            #    not.
            #
            #    Both patterns are required literally. A test that
            #    printed the same value twice, or two zeroes, would
            #    satisfy a looser pattern and prove nothing.
            re.compile(
                r"^\[vspace\] isolation: at 0x0{10}400000, thread A reads "
                r"0xaaaaaaaa11111111 and thread B reads 0xbbbbbbbb22222222; "
                r"same address, different frames, [1-9]\d* cr3 reloads",
                re.M,
            ),
            # 6. Unmap hands back both the Frame and the intermediate
            #    table. This is ADR 015's question, answered by moving
            #    the ownership rather than by counting references.
            re.compile(
                r"^\[vspace\] unmap: the Frame and the PT both come back to "
                r"their holders and the address stops resolving",
                re.M,
            ),
            # 7. Revoke returns the page tables to Untyped memory, and
            #    takes the root capability with them.
            #
            #    The count must be non-zero, and that is the whole
            #    point of requiring it. The first version of this test
            #    revoked the root *PageTable* and reported "0
            #    capabilities deleted" -- because the root, the three
            #    intermediate tables and the Frame are all children of
            #    the same Untyped. They are siblings, so revoking one
            #    reaches none of the others. A pattern accepting `\d+`
            #    would have passed on that.
            re.compile(
                r"^\[vspace\] revoke: [1-9]\d* capabilities deleted, "
                r"watermark \d+ -> \d+; the page tables are Untyped memory "
                r"again and the root capability is gone",
                re.M,
            ),
            # 8-9. The measurement, and the two counters that make the
            #      "reload CR3 only when it changes" rule testable at
            #      all. Both figures must be non-zero: reloading on
            #      every switch and never reloading are each wrong in
            #      their own way, and one counter could not tell them
            #      apart.
            re.compile(
                r"^\[vspace\] cr3 cost: \d+ cycles/switch within one address "
                r"space \([1-9]\d* skipped\), \d+ cycles/switch across two "
                r"\([1-9]\d* reloads\)",
                re.M,
            ),
            re.compile(
                r"^\[vspace\] cr3 cost: the difference is \d+ cycles per "
                r"switch -- measured under emulation",
                re.M,
            ),
            re.compile(
                r"^\[sched\] cr3: \d+ reloads, \d+ skipped \(same address "
                r"space\), \d+ threads stopped for a revoked vspace",
                re.M,
            ),
            re.compile(r"^\[vspace\] all address space tests passed", re.M),
            re.compile(r"^\[boot\] M7 checks passed", re.M),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            POISON_IN_REGISTERS,
        ],
    ),
    "vskernel": Expectation(
        name="mapping into the kernel half",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            *VSPACE_INIT_LINES,
            # All four refused, each one named. Requiring the
            # individual lines rather than a summary means a loop that
            # gave up after the first would fail here.
            re.compile(
                r"^\[vspace\] refused 0xffff800000000000 \(the first kernel "
                r"address\): NotUserAddress",
                re.M,
            ),
            re.compile(
                r"^\[vspace\] refused 0x[0-9a-f]{16} \(inside the physical "
                r"window\): NotUserAddress",
                re.M,
            ),
            re.compile(
                r"^\[vspace\] refused 0x[0-9a-f]{16} \(the kernel heap\): "
                r"NotUserAddress",
                re.M,
            ),
            re.compile(
                r"^\[vspace\] refused 0xffffffff80000000 \(the kernel "
                r"image\): NotUserAddress",
                re.M,
            ),
            # And the kernel is still standing afterwards. That is why
            # this scenario exits 33 rather than 35: a rejected request
            # is a caller error, and from M8 the caller is a user
            # component. A kernel that died here would be worse than one
            # that allowed the mapping -- it would be a denial of
            # service reachable from any component.
            # The kernel is still standing afterwards, which is the
            # whole reason this scenario exits 33.
            #
            # It does not reach "M7 checks passed": the scenario calls
            # `qemu::exit` itself rather than returning to `kmain`,
            # because the point is made as soon as the four refusals
            # have happened and the heap is still readable. Requiring
            # the banner here would be requiring the scenario to be
            # written differently for no reason.
            re.compile(
                r"^\[vspace\] all four refused and the kernel's own mappings "
                r"still work; this is an error path, not a fault",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
        ],
    ),
    "vsisolation": Expectation(
        name="address space isolation",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            *VSPACE_INIT_LINES,
            # The mapping exists in one space and not the other, stated
            # before the fault so the log carries the evidence either
            # way.
            re.compile(
                r"^\[vspace\] 0x0000008000000000 in B: Ok\(PhysAddr\(\d+\)\); "
                r"in A: Err\(NotMapped",
                re.M,
            ),
            # The thread really is in its own address space: CR3 is not
            # the kernel's. Without this the fault could be an ordinary
            # unmapped-address fault in the kernel's space, which would
            # say nothing about isolation.
            re.compile(
                r"^\[vspace\] thread running in its own VSpace \(cr3 "
                r"0x[0-9a-f]{16}\); reading 0x0000008000000000",
                re.M,
            ),
            re.compile(r"^\*\*\* EXCEPTION: #PF page fault \(vector 14\) \*\*\*", re.M),
            # CR2 is the exact address, and the error code is 0:
            # not-present, read, supervisor. A *present* page would mean
            # the address resolved to something, which is the failure
            # this scenario exists to rule out.
            re.compile(r"^\[trap\] cr2=0x0000008000000000\b", re.M),
            re.compile(r"^\[trap\] error_code=0x0{16}\b", re.M),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
        ],
        forbidden=[
            re.compile(r"M7 checks passed", re.M),
            re.compile(r"the two VSpaces are not isolated", re.M),
        ],
    ),
    "vsstale": Expectation(
        name="revoked address space",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            *VSPACE_INIT_LINES,
            # SetSpace on a running thread is refused, which is what
            # keeps ADR 028's case A from trapping a caller: it is the
            # reason a running thread can always be moved out of the way
            # first.
            re.compile(
                r"^\[vspace\] SetSpace on the running thread: ThreadBusy",
                re.M,
            ),
            # The victim really was running in its own address space.
            re.compile(
                r"^\[vspace\] victim running with cr3 0x[0-9a-f]{16}",
                re.M,
            ),
            # Case A, provoked the only way it can be: by the thread
            # that is running in the address space it asks to destroy.
            # Nothing outside can reach this branch, because control
            # returning to the main thread means CR3 is the kernel's
            # again -- and the kernel's PML4 has no capability naming
            # it.
            re.compile(
                r"^\[vspace\] a thread tried to revoke the address space it "
                r"is running in \(root 0x[0-9a-f]{16}\): VSpaceInUse",
                re.M,
            ),
            # Case B: revoking an idle VSpace succeeds.
            re.compile(
                r"^\[vspace\] revoked an idle VSpace: \d+ capabilities "
                r"deleted",
                re.M,
            ),
            # And the thread bound to it is stopped rather than run --
            # not silently dropped into the kernel's address space,
            # which ADR 028 rejected as worse than stopping.
            re.compile(
                r"^\[sched\] thread at 0x[0-9a-f]{16} has a revoked address "
                r"space; suspended rather than switched into",
                re.M,
            ),
            re.compile(
                r"^\[vspace\] the thread was suspended rather than run: "
                r"[1-9]\d* stale detections, state Suspended",
                re.M,
            ),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
            re.compile(
                r"^\[PANIC\] vspace-stale-cr3 completed its checks",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"M7 checks passed", re.M),
            # The failure this scenario exists to rule out: the
            # scheduler switching into a thread whose page tables have
            # been handed back to their Untyped.
            re.compile(r"the scheduler switched into a thread", re.M),
            # And the other one: a thread succeeding in destroying the
            # address space it is executing in.
            re.compile(r"instead of VSpaceInUse", re.M),
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