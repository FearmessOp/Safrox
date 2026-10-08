#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""Boot a Safrox ISO under QEMU and validate the acceptance criteria.

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
BANNER = re.compile(r"^Safrox \S+", re.M)


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
    re.compile(r"^Safrox \S+", re.M),  # name/version
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
            re.compile(r"^\[boot\] M9a checks passed", re.M),
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
        forbidden=[re.compile(r"M9a checks passed", re.M)],
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
            re.compile(r"^\[boot\] M9a checks passed", re.M),
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
        forbidden=[re.compile(r"M9a checks passed", re.M)],
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
            re.compile(r"M9a checks passed", re.M),
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
            re.compile(r"M9a checks passed", re.M),
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
            re.compile(r"M9a checks passed", re.M),
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
            re.compile(r"^\[boot\] M9a checks passed", re.M),
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
            re.compile(r"M9a checks passed", re.M),
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
            re.compile(r"M9a checks passed", re.M),
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
            re.compile(r"M9a checks passed", re.M),
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
            re.compile(r"^\[boot\] M9a checks passed", re.M),
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
            re.compile(r"M9a checks passed", re.M),
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
            re.compile(r"M9a checks passed", re.M),
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
            re.compile(r"^\[boot\] M9a checks passed", re.M),
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
            re.compile(r"M9a checks passed", re.M),
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
            re.compile(r"M9a checks passed", re.M),
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
            re.compile(r"^\[boot\] M9a checks passed", re.M),
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
            re.compile(r"M9a checks passed", re.M),
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
            re.compile(r"M9a checks passed", re.M),
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
            re.compile(r"M9a checks passed", re.M),
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
            re.compile(r"^\[boot\] M9a checks passed", re.M),
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
            # It does not reach "M9a checks passed": the scenario calls
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
            re.compile(r"M9a checks passed", re.M),
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
            re.compile(r"M9a checks passed", re.M),
            # The failure this scenario exists to rule out: the
            # scheduler switching into a thread whose page tables have
            # been handed back to their Untyped.
            re.compile(r"the scheduler switched into a thread", re.M),
            # And the other one: a thread succeeding in destroying the
            # address space it is executing in.
            re.compile(r"instead of VSpaceInUse", re.M),
        ],
    ),
    # ---- M8: ring 3 ----------------------------------------------------
    #
    # The positive scenario and five negatives. The negatives are where
    # the milestone's value is: a kernel that lets user code run but
    # cannot stop it has gained nothing over M7.
    "usertest": Expectation(
        name="ring 3",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            *VSPACE_INIT_LINES,
            # The MSRs and the segments. A wrong STAR would make sysret
            # load the wrong descriptor silently, so the values are
            # required rather than merely printed.
            re.compile(
                r"^\[cpu\] syscall: enabled \(efer\.sce set\), lstar "
                r"0x[0-9a-f]{16}, star 0x[0-9a-f]{16}",
                re.M,
            ),
            # AC in the mask, which is M7's lesson arriving late: `stac`
            # is unprivileged, so user code can hand the kernel an open
            # SMAP window.
            re.compile(
                r"^\[cpu\] syscall: fmask clears IF\|DF\|TF\|AC",
                re.M,
            ),
            # And the same claim checked against the *register* rather
            # than against the line above. A mutation that removed AC
            # from the mask was caught only by the banner -- which the
            # same edit could have reworded. ADR 023's K5 does not
            # accept evidence a guard's author controls.
            re.compile(
                r"^\[user\] fmask: read back as 0x[0-9a-f]{16} from the "
                r"MSR -- IF, DF, TF and AC all cleared on entry",
                re.M,
            ),
            re.compile(
                r"^\[cpu\] fpu: osfxsr set, osxmmexcpt set, cr0\.em clear, "
                r"cr0\.ts clear",
                re.M,
            ),
            re.compile(
                r"^\[user\] selectors: user cs 0x[0-9a-f]{4}, user ss "
                r"0x[0-9a-f]{4}",
                re.M,
            ),
            # The saved MXCSR must be the architectural default. Zero is
            # what retype leaves, and it unmasks every SSE exception --
            # indistinguishable from "initialised" until the first
            # floating-point operation.
            re.compile(r"^\[user\] fpu: .*saved mxcsr 0x1f80", re.M),
            # W^X on the user side: the code page is r-x, and no page is
            # both writable and executable.
            re.compile(r"^\[user\] address space: code at .* r-x", re.M),
            re.compile(
                r"^\[user\] w\^x: no page is both writable and executable",
                re.M,
            ),
            # **The claim of this milestone.** CS's low two bits are 3.
            # Nothing else here would fail on a kernel that ran the same
            # code at CPL 0.
            re.compile(
                r"^\[user\] ring 3: cs = 0x003b \(cpl 3\), data page reads "
                r"0x[0-9a-f]{16}, [1-9]\d* syscalls completed",
                re.M,
            ),
            # Two user threads, two kernel stacks. A single counter would
            # be satisfied by an implementation that wrote RSP0 once,
            # because the first value is already correct -- so the two
            # addresses are required to differ.
            re.compile(
                r"^\[user\] kernel stacks: thread A enters at "
                r"0x[0-9a-f]{16}, thread B at 0x[0-9a-f]{16}; each inside "
                r"its own stack, [1-9]\d* rsp0 updates",
                re.M,
            ),
            # M7's isolation, now at CPL 3.
            re.compile(
                r"^\[user\] isolation: at 0x[0-9a-f]{16}, thread A reads "
                r"0x[0-9a-f]{16} and thread B reads 0x[0-9a-f]{16}",
                re.M,
            ),
            # The FPU. The equality between fxsave and the switch count
            # is what rules out lazy switching: a correct lazy
            # implementation also preserves every register, so
            # preservation alone says nothing about the strategy.
            re.compile(
                r"^\[user\] xmm: thread A holds 0x[0-9a-f]{16}, thread B "
                r"0x[0-9a-f]{16} across [1-9]\d* reports; [1-9]\d* fxsave "
                r"over [1-9]\d* switches -- equal, so nothing is lazy",
                re.M,
            ),
            re.compile(
                r"^\[sched\] fpu: [1-9]\d* fxsave, [1-9]\d* fxrstor over "
                r"[1-9]\d* switches; [1-9]\d* rsp0 updates",
                re.M,
            ),
            # The timer reaches ring 3.
            re.compile(
                r"^\[user\] preemption: [1-9]\d* ticks while the kernel "
                r"span without yielding",
                re.M,
            ),
            # The measurement, with its emulation caveat.
            re.compile(
                r"^\[user\] syscall cost: [1-9]\d* calls in \d+ cycles",
                re.M,
            ),
            re.compile(
                r"^\[user\] syscall cost: measured under emulation",
                re.M,
            ),
            # The fast path was taken. Without this, ADR 029's
            # canonical-RCX check could be guarding a path nothing uses.
            re.compile(
                r"^\[syscall\] [1-9]\d* calls \(\d+ returned an error\), "
                r"[1-9]\d* via sysret",
                re.M,
            ),
            # A real capability invocation ran, not only Echo. Echo
            # resolves no capability at all, so "the capability path
            # works from ring 3" would otherwise rest on nothing --
            # and the first run of this scenario reported exactly
            # that: `last capability invocation: <none yet>`.
            re.compile(
                r"^\[user\] capability path: ReadRegisters from ring 3 "
                r"returned \d+",
                re.M,
            ),
            re.compile(
                r"^\[syscall\] last capability invocation: ReadRegisters",
                re.M,
            ),
            re.compile(r"^\[user\] all ring 3 tests passed", re.M),
            # `usertest::run()` is `-> !` and exits through `qemu::exit`
            # itself, so it never returns to `kmain` and never reaches
            # the milestone banner. Its own banner is the end of this
            # scenario, and requiring the other one would be requiring
            # M8's test to be rewritten for no reason -- the same
            # argument already recorded for `vskernel`.
            re.compile(r"^\[boot\] M8 checks passed", re.M),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            POISON_IN_REGISTERS,
            # CPL 0 in the CS report would mean the thread never left
            # the kernel, and every other check would still pass.
            re.compile(r"\(cpl 0\)", re.M),
        ],
    ),
    "userpriv": Expectation(
        name="privileged instruction from ring 3",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            re.compile(
                r"^\[user\] running `cli` at cpl 3; expected #GP",
                re.M,
            ),
            # #GP, vector 13.
            re.compile(
                r"^\*\*\* EXCEPTION: #GP general protection \(vector 13\)",
                re.M,
            ),
            # From ring 3: the saved CS must be the user selector. A #GP
            # reported with a kernel CS would mean the fault came from
            # somewhere else entirely, and the scenario would be
            # passing for the wrong reason.
            re.compile(r"^\[trap\] error_code=.*cs=0x003b", re.M),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
        ],
        forbidden=[
            re.compile(r"M9a checks passed", re.M),
            # The failure this exists to rule out.
            re.compile(r"without faulting", re.M),
        ],
    ),
    "userkread": Expectation(
        name="user reads the kernel half",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            re.compile(
                r"^\[user\] reading 0x[0-9a-f]{16} from cpl 3; expected #PF",
                re.M,
            ),
            re.compile(
                r"^\*\*\* EXCEPTION: #PF page fault \(vector 14\)",
                re.M,
            ),
            # The USER bit in the error code. This is the whole point:
            # it distinguishes "ring 3 reached for the kernel" from "a
            # kernel pointer went wrong", and the two mean entirely
            # different things. The decoder prints it as "user".
            re.compile(r"^\[trap\] cr2=0x[0-9a-f]{16}  \(user read", re.M),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
        ],
        forbidden=[
            re.compile(r"M9a checks passed", re.M),
            re.compile(r"without faulting", re.M),
            # A supervisor-mode fault here would mean the read happened
            # in the kernel rather than in ring 3.
            re.compile(r"\(supervisor read", re.M),
        ],
    ),
    "usersmep": Expectation(
        name="SMEP",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            # SMEP and SMAP are reported on one line, so the pattern
            # has to match that line rather than a hypothetical
            # per-feature one.
            re.compile(r"^\[cpu\] smep: enabled, smap: enabled", re.M),
            # Ring 3 executed the page first. Without this half, the
            # fault below would be explainable by the page simply not
            # being executable -- and the scenario would prove nothing
            # about SMEP.
            re.compile(
                r"^\[user\] ring 3 executed the page [1-9]\d* times; the "
                r"same page is now fetched from cpl 0",
                re.M,
            ),
            re.compile(
                r"^\[user\] switching to the user vspace .* at cpl 0; "
                r"expected #PF with the instruction-fetch bit",
                re.M,
            ),
            re.compile(
                r"^\*\*\* EXCEPTION: #PF page fault \(vector 14\)",
                re.M,
            ),
            # Bit 4 of the error code: an instruction fetch, from
            # supervisor mode, on a present page. That combination is
            # SMEP and nothing else.
            re.compile(
                r"^\[trap\] cr2=0x[0-9a-f]{16}  \(supervisor instruction "
                r"fetch protection-violation\)",
                re.M,
            ),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
        ],
        forbidden=[
            re.compile(r"M9a checks passed", re.M),
            # SMEP absent would make the scenario vacuous, and it says
            # so rather than passing quietly.
            re.compile(r"SMEP is not enabled", re.M),
        ],
    ),
    "usersmap": Expectation(
        name="SMAP",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            re.compile(r"^\[cpu\] smep: enabled, smap: enabled", re.M),
            # The guarded read, first and successful. It runs first
            # because the unguarded one ends the VM -- the other order
            # would make half the claim untestable.
            re.compile(
                r"^\[user\] smap: with a UserAccess guard, the kernel read "
                r"0x[0-9a-f]{16} from 0x[0-9a-f]{16} -- succeeded",
                re.M,
            ),
            re.compile(
                r"^\[user\] smap: now reading 0x[0-9a-f]{16} with no guard",
                re.M,
            ),
            re.compile(
                r"^\*\*\* EXCEPTION: #PF page fault \(vector 14\)",
                re.M,
            ),
            # Supervisor read, protection violation: present page, right
            # permissions, refused anyway. That is SMAP.
            re.compile(
                r"^\[trap\] cr2=0x[0-9a-f]{16}  \(supervisor read "
                r"protection-violation\)",
                re.M,
            ),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
        ],
        forbidden=[
            re.compile(r"M9a checks passed", re.M),
            re.compile(r"SMAP is not enabled", re.M),
            # The unguarded read succeeding would mean SMAP protects
            # nothing -- and every stray supervisor pointer into user
            # memory works silently.
            re.compile(r"without a guard and got", re.M),
        ],
    ),
    "userforge": Expectation(
        name="forged capability pointers",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            *VSPACE_INIT_LINES,
            # Six hostile invocations, each refused with its *own*
            # error. Six InvalidArguments would satisfy a count and
            # prove nothing -- the same mistake ADR 027's two half
            # checks made, found by mutation in M7.
            re.compile(
                r"^\[user\] cap-forge: all 6 hostile invocations refused, "
                r"each with its own error -- InvalidCPtr, CPtrOutOfRange, "
                r"StaleCPtr, EmptySlot, WrongType, InvalidArgument",
                re.M,
            ),
            # And the kernel survived. This is the scenario's actual
            # claim: a kernel that panics on a bad CPtr is a kernel any
            # user component can halt.
            re.compile(
                r"^\[user\] the kernel is still running; exiting normally",
                re.M,
            ),
            re.compile(
                r"^\[syscall\] [1-9]\d* calls \([1-9]\d* returned an error\)",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            POISON_IN_REGISTERS,
            # A forged pointer reaching an object.
            re.compile(r"which is not an error", re.M),
        ],
    ),
    # ---- M9a: synchronous rendezvous -----------------------------------
    #
    # Three scenarios, all exiting 33. The second is the milestone's
    # real result and the third is what proves `cancel_ipc` is wired in
    # at all -- without the call in `cnode::delete`, it hangs.
    "ipctest": Expectation(
        name="IPC rendezvous",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            *VM_INIT_LINES,
            *HEAP_INIT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            # 1. The objects are the size ADR 017 budgeted, *after*
            #    M9 added nine fields to the TCB. The ceiling is a
            #    build-time assertion too, but a figure in the log is
            #    what makes it checkable against the ADR's table.
            re.compile(
                r"^\[ipc\] sizes: Endpoint 32 B, TCB 2048 B with M9's new "
                r"fields -- ADR 017's ceilings unchanged",
                re.M,
            ),
            # The register/buffer split, and the note that the wasted
            # words were measured rather than guessed (ADR 034).
            re.compile(
                r"^\[ipc\] message: 3 words in registers, 128 total "
                r"\(1024 B\); the buffer's first 3 words are unused, which "
                r"was measured rather than assumed",
                re.M,
            ),
            # 2-3. Both orders, with non-zero counts on both sides.
            #      A kernel where only one order worked would still
            #      produce a line here, so both figures must move.
            re.compile(
                r"^\[ipc\] rendezvous: both orders work -- [1-9]\d* senders "
                r"and [1-9]\d* receivers had to block, [1-9]\d* rendezvous "
                r"completed",
                re.M,
            ),
            # 4. Three lengths, including both boundaries. The
            #    128-word case is checked word by word by the
            #    receiver, out of its own buffer.
            re.compile(
                r"^\[ipc\] lengths: 0, 3 and 128 words all delivered "
                r"intact; the receiver checked every word of the longest",
                re.M,
            ),
            # Every tag field the sender controls, each refused by its
            # own check (ADR 023's K5).
            re.compile(
                r"^\[ipc\] tag: all 5 invalid fields refused, each by its "
                r"own check",
                re.M,
            ),
            # 5. FIFO, with the order stated rather than implied.
            re.compile(
                r"^\[ipc\] fifo: three senders queued and were delivered "
                r"0, 1, 2 -- in the order they arrived",
                re.M,
            ),
            re.compile(r"^\[ipc\] all M9a IPC tests passed", re.M),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            POISON_IN_REGISTERS,
        ],
    ),
    "ipccancel": Expectation(
        name="cancellation cost (measurement)",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            # Both figures, both from a real queue. Requiring the two
            # lengths by name rules out a run that measured the same
            # queue twice.
            re.compile(
                r"^\[ipc\] cancel-cost: \d+ cycles with 50 waiting, \d+ "
                r"cycles with 500 waiting",
                re.M,
            ),
            # And the conclusion, with the ratio in it. The kernel
            # asserts the ratio as well; this is the outside check on
            # the same claim.
            re.compile(
                r"^\[ipc\] cancel-cost: the two figures agree within a "
                r"factor of \d+, so the cost does not depend on the "
                r"queue's length",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            # The failure this scenario exists to catch: a linear walk
            # would trip the kernel's own assertion, which says this.
            re.compile(r"That is a linear walk", re.M),
        ],
    ),
    "ipcrevoked": Expectation(
        name="revoked endpoint",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            # The kernel says so from inside `delete`, which is where
            # the call actually lives. Requiring this line is how the
            # wiring itself is tested rather than its effect.
            re.compile(
                r"^\[ipc\] endpoint at .* deleted with 3 thread\(s\) "
                r"waiting; all woken with EndpointDead",
                re.M,
            ),
            # All three, and none of them received a message. The
            # second half matters: a waiter that got a message from a
            # deleted endpoint would be reading an object that no
            # longer exists.
            re.compile(
                r"^\[ipc\] endpoint-revoked: all 3 waiters woke with "
                r"EndpointDead and none received a message; [1-9]\d* "
                r"cancellations recorded",
                re.M,
            ),
            # And the reasoning, which is not ADR 026's.
            re.compile(
                r"^\[ipc\] endpoint-revoked: the kernel is still running",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
        ],
    ),
    # ---- M9b: Call, Reply and badges -----------------------------------
    #
    # Five scenarios, all exiting 33 -- including the three negatives. A
    # client woken with an error is a caller-visible outcome rather than
    # a fault, and a kernel that died on any of them would be one a user
    # component could halt.
    "ipccall": Expectation(
        name="call and reply",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            # 1. The round trip, with the value the client checked. A
            #    bare "the reply arrived" would pass on a kernel that
            #    delivered an empty message.
            re.compile(
                r"^\[ipc\] call/reply: the round trip completed -- client "
                r"sent, server answered 0x[0-9a-f]{16}, client verified it; "
                r"[1-9]\d* calls and [1-9]\d* replies so far",
                re.M,
            ),
            # 2. ReplyRecv, counted rather than described. The claim is
            #    about syscall count, so the figure is what is required:
            #    a server making two separate calls would satisfy "the
            #    reply arrived and a request came back" equally well.
            re.compile(
                r"^\[ipc\] replyrecv: one invocation both answered a client "
                r"and collected the next request -- [1-9]\d* ReplyRecv "
                r"against [1-9]\d* replies delivered",
                re.M,
            ),
            # 3. Two badges, both named, and **both replies checked**.
            #    The last clause is the one that matters: every other
            #    line here would pass on a kernel that crossed the two
            #    answers.
            re.compile(
                r"^\[ipc\] badge: two clients on one endpoint carried "
                r"0x1000 and 0x1002; the server told them apart and each "
                r"got its own answer \(0x[0-9a-f]{16} and 0x[0-9a-f]{16}\)",
                re.M,
            ),
            # 4. ADR 036's finest decision, and the note that the
            #    scenario does not depend on timing. Required text
            #    rather than decoration: a later edit that replaced the
            #    controlled window with a sleep would still pass the
            #    other checks on a lucky run, and this line is what
            #    says the window was constructed.
            re.compile(
                r"^\[ipc\] reply-to-at-rendezvous: server B called Recv "
                r"while server A held a caller and came away with nothing; "
                r"A's reply reached the right client",
                re.M,
            ),
            re.compile(
                r"^\[ipc\] reply-to-at-rendezvous: .*The window was opened "
                r"by the test rather than by a timer",
                re.M | re.S,
            ),
            re.compile(r"^\[ipc\] all M9b call/reply tests passed", re.M),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            POISON_IN_REGISTERS,
        ],
    ),
    "ipcdead": Expectation(
        name="server dies owing a reply",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            # The kernel says so from inside `delete`, which is where
            # the obligation is actually discharged. Requiring this
            # line tests the wiring rather than only its effect --
            # the same reasoning as `ipcrevoked` above.
            re.compile(
                r"^\[ipc\] thread at .* died owing a reply; its caller was "
                r"woken with ReplyTargetGone",
                re.M,
            ),
            re.compile(
                r"^\[ipc\] server-dies: the server was revoked holding a "
                r"caller; the client woke with ReplyTargetGone rather than "
                r"waiting forever",
                re.M,
            ),
            # The distinction ADR 036 draws, in the log: a server that
            # *declines* to answer is a different case and does leave
            # its client waiting.
            re.compile(
                r"^\[ipc\] server-dies: .*a server that merely declines to "
                r"answer is a different matter",
                re.M | re.S,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
        ],
    ),
    "ipcstale": Expectation(
        name="stale reply target",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            # The premise, stated before the claim. If the replacement
            # TCB had not landed at the old address the scenario would
            # prove nothing, and this line is what shows it did --
            # "the SAME address" is required text.
            re.compile(
                r"^\[ipc\] stale-reply: the replacement TCB landed at the "
                r"SAME address 0x[0-9a-f]{16}, generation \d+ -> \d+",
                re.M,
            ),
            # Claim 1: the live path. The reply is refused, and by the
            # *first* of ADR 036's three comparisons rather than the
            # generation -- because the back-pointer cleared
            # `reply_to` when the client was revoked (ADR 039).
            #
            # Requiring NoReplyTarget by name rather than "an error"
            # is what keeps the two claims apart. The scenario's first
            # draft asserted ReplyTargetGone here and failed, which is
            # how the ordering between cleanup and validation became
            # visible at all.
            re.compile(
                r"^\[ipc\] stale-reply: the Reply was refused with "
                r"NoReplyTarget -- the back-pointer cleared the server's "
                r"reply_to when the client died",
                re.M,
            ),
            # Claim 2: the generation comparison itself, exercised
            # directly. **Both halves are required.**
            #
            # "Generation 0 is refused" on its own would pass on a
            # check that refuses everything. The accepted case is what
            # rules that out, and the two differ in nothing but the
            # generation -- same address, same slot, same type.
            re.compile(
                r"^\[ipc\] stale-reply: the generation check, exercised "
                r"directly: generation [0-9]+ is accepted and [0-9]+ is "
                r"refused at the SAME address 0x[0-9a-f]{16}",
                re.M,
            ),
            # And the stranger occupying the old address was never
            # woken. "still Inactive" is the load-bearing part: a reply
            # delivered to the replacement TCB would have made it
            # runnable, so this is the check that the wrong thread was
            # not woken rather than merely that an error was returned.
            re.compile(
                r"^\[ipc\] stale-reply: the live path gave NoReplyTarget "
                r"and the new TCB at the same address is untouched "
                r"\(still Inactive\)",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
        ],
    ),
    "ipcself": Expectation(
        name="self-call deadlock",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            # The deadlock exists and is named rather than prevented.
            re.compile(
                r"^\[ipc\] self-call: the thread is BlockedOnEndpoint with "
                r"nobody receiving -- a deadlock, and deliberately not "
                r"prevented",
                re.M,
            ),
            # And it is escapable. This is the promise ADR 036 makes in
            # exchange for not preventing it, so it is the part worth
            # testing.
            re.compile(
                r"^\[ipc\] self-call: Suspend took the thread off the queue "
                r"and Resume returned an error from the Call",
                re.M,
            ),
            # Rescue needs a capability. A system where any component
            # could unstick any other would have a different security
            # story, and the line records which one this is.
            re.compile(
                r"^\[ipc\] self-call: .*the escape needs a TCB capability",
                re.M | re.S,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
        ],
    ),
    "ipccost": Expectation(
        name="IPC cost (measurement)",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            # Three figures, all non-zero. A zero would mean the TSC
            # read the same value twice, which is a broken measurement
            # rather than an infinitely fast one.
            re.compile(
                r"^\[ipc\] cost: Send\+Recv [1-9]\d* cycles, Call\+Reply "
                r"[1-9]\d* cycles, ReplyRecv loop [1-9]\d* cycles "
                r"\(minimum of \d+ samples each\)",
                re.M,
            ),
            # The difference, and the comparison ADR 040 asked for.
            re.compile(
                r"^\[ipc\] cost: a Call round costs [1-9]\d* cycles more "
                r"than a bare rendezvous",
                re.M,
            ),
            # What the ReplyRecv figure does **not** show.
            #
            # ADR 036 claims ReplyRecv saves a kernel entry, which M8
            # measured at 934 cycles. The saving here is far smaller,
            # and necessarily so: there is no user mode until M13, so
            # `reply_recv` is reached by a function call and no
            # privilege transition is saved at all.
            #
            # Required as an alternation because either outcome is
            # legitimate -- the difference is around this measurement's
            # noise floor under TCG, so the loop coming out *dearer* is
            # not a failure. What is not legitimate is the figure going
            # unremarked: a reader who took the small saving for a
            # refutation of ADR 036 would be drawing a conclusion the
            # measurement cannot support, and a reader who took it for
            # a confirmation would be worse off still.
            re.compile(
                r"^\[ipc\] cost: the ReplyRecv loop (saves [1-9]\d* cycles "
                r"a round|did not come out cheaper).*(no user mode until "
                r"M13|until M13 the saving is one function call)",
                re.M | re.S,
            ),
            # The caveat is required text. These are emulated figures
            # and a reader who takes them for silicon would draw the
            # wrong conclusion -- the same rule M6 and M8 follow.
            re.compile(
                r"^\[ipc\] cost: measured under emulation",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            # The kernel's own sanity check on the measurement.
            re.compile(r"did not cost more than a bare rendezvous", re.M),
        ],
    ),
    # ---- M9c: capability transfer and notifications --------------------
    #
    # Four scenarios, all exiting 33 -- including both negatives. A
    # dropped capability and a woken waiter are outcomes, not faults.
    "ipccap": Expectation(
        name="capability transfer",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            # 1. All four counts, each landing in the right slot with
            #    rights intact. "in order" is required text: a
            #    transfer delivering the right *number* in the wrong
            #    order would satisfy a count.
            re.compile(
                r"^\[ipc\] caps: 1, 2, 3 and 4 capabilities each arrived "
                r"in the receiver's own slots, in order, with rights "
                r"unchanged; [1-9]\d* transferred in total",
                re.M,
            ),
            # 2. The fifth is refused by the tag rather than dropped by
            #    the loop -- the limit lives in the message format.
            re.compile(
                r"^\[ipc\] caps: a 5-capability message is refused by the "
                r"tag with TooManyCaps",
                re.M,
            ),
            # 3. **Two outcomes, and the second is the load-bearing
            #    one.** A kernel that failed the whole send would pass
            #    a check that only looked at the dropped capability.
            re.compile(
                r"^\[ipc\] caps: a capability without GRANT was dropped "
                r"silently and \*\*the message still arrived\*\* -- tag "
                r"says caps=0, label intact",
                re.M,
            ),
            # 4. No window is an absence rather than a refusal.
            re.compile(
                r"^\[ipc\] caps: with no receive window open, 2 "
                r"capabilities went nowhere and the message arrived intact",
                re.M,
            ),
            # 5. The obstacle in the middle: 1st and 3rd through, 2nd
            #    dropped, occupant untouched.
            re.compile(
                r"^\[ipc\] caps: three sent with the MIDDLE destination "
                r"occupied -- the 1st and 3rd arrived, the 2nd was "
                r"dropped, and the occupant is untouched",
                re.M,
            ),
            # 6. The delegation chain stops at the depth limit.
            re.compile(
                r"^\[ipc\] caps: a capability already at depth 8 was "
                r"dropped rather than carried to depth 9",
                re.M,
            ),
            # 7. Signals OR rather than overwrite. Requiring the
            #    combined mask by value is what rules out a
            #    "last signal wins" implementation.
            re.compile(
                r"^\[ipc\] notification: two badged signals accumulated to "
                r"0x101 -- both bits present, so the mask ORs rather than "
                r"overwrites",
                re.M,
            ),
            re.compile(
                r"^\[ipc\] notification: Wait blocked while the mask was "
                r"empty",
                re.M,
            ),
            # 8. One thread, both sources, and the tag bit telling them
            #    apart. Both halves required: a kernel that only ever
            #    delivered notifications would pass the first.
            re.compile(
                r"^\[ipc\] bound: one thread in Recv was woken by "
                r"\*\*both\*\* sources -- a signal \(mask 0x100, "
                r"IS_NOTIFICATION set\) and an endpoint message",
                re.M,
            ),
            # 9. **The starvation test.**
            #
            # The run length carries the claim: starvation is not
            # "slow once" but "always behind another one", so the
            # figure is the longest run of notifications between two
            # endpoint deliveries. Several endpoint deliveries are
            # required, because one proves nothing -- with a queued
            # sender the first round takes a message before any
            # signal can exist.
            #
            # The contested count must be non-zero: that is what says
            # the alternation branch ran at all. *Which way* each
            # contested decision went is deliberately not required --
            # see the note in `check_no_starvation` for why that is a
            # property of the stream's rhythm rather than of a
            # correct kernel. An earlier version of this pattern did
            # require both directions, and it was wrong.
            re.compile(
                r"^\[ipc\] starvation: under a continuous signal stream, "
                r"[1-9]\d* endpoint messages got through and the longest "
                r"run of notifications between any two of them was \d+ "
                r"\(bound 4, stream capped at 200\)\. [1-9]\d* decisions "
                r"were contested",
                re.M,
            ),
            # And the note that the contention is built rather than
            # hoped for. Required text, because a later edit replacing
            # the driven stream with a free-running one would still
            # pass on a lucky run (ADR 023's K8).
            re.compile(
                r"^\[ipc\] starvation: the stream is driven one signal per "
                r"round by the test itself",
                re.M,
            ),
            re.compile(r"^\[ipc\] all M9c capability transfer tests passed", re.M),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            POISON_IN_REGISTERS,
        ],
    ),
    "ipccaphostile": Expectation(
        name="hostile capability transfers",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            # Four shapes, and **zero transferred**. The count is what
            # makes this more than "no crash": a kernel that let one
            # through would still print a cheerful summary.
            re.compile(
                r"^\[ipc\] cap-hostile: 4 malformed transfers refused -- "
                r"reserved bits, leaf index out of range, root index out "
                r"of range, and a well-formed capability without GRANT\. "
                r"[1-9]\d* dropped, 0 transferred, and every message "
                r"still arrived",
                re.M,
            ),
            # The claim this scenario exists for.
            re.compile(
                r"^\[ipc\] cap-hostile: the kernel is still running",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            POISON_IN_REGISTERS,
        ],
    ),
    "ipcntfnrev": Expectation(
        name="revoked notification",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            # The kernel says so from inside `delete`, which is where
            # the wiring lives -- the same reasoning as `ipcrevoked`:
            # this tests that the call is present, not only that its
            # effect was achieved somehow.
            re.compile(
                r"^\[ipc\] notification at .* deleted with a thread "
                r"waiting; woken with EndpointDead",
                re.M,
            ),
            re.compile(
                r"^\[ipc\] notification-revoked: the waiter woke with "
                r"EndpointDead and received no mask",
                re.M,
            ),
            # And the reasoning, which is ADR 039's rather than
            # ADR 026's.
            re.compile(
                r"^\[ipc\] notification-revoked: woken rather than the "
                r"delete being refused",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
        ],
    ),
    "ipccapcost": Expectation(
        name="capability transfer cost (measurement)",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            # Three figures, all non-zero.
            re.compile(
                r"^\[ipc\] cap-cost: Send\+Recv with 0 caps [1-9]\d* "
                r"cycles, 1 cap [1-9]\d* cycles, 4 caps [1-9]\d* cycles",
                re.M,
            ),
            # The per-capability cost, which is ADR 035's actual claim:
            # four is a limit because each one is bounded work, not
            # because four happens to be affordable.
            #
            # The *first* capability's figure is deliberately not
            # constrained here. It carries the first touch of the
            # destination slots and moves between runs -- 756
            # standalone against 1806 inside the suite -- which made
            # an earlier assertion comparing it to the marginal cost
            # intermittent. The marginal figure is the stable one and
            # the one the ADR's argument is about.
            re.compile(
                r"^\[ipc\] cap-cost: the first capability costs [1-9]\d* "
                r"cycles, each of the next three about \d+",
                re.M,
            ),
            re.compile(
                r"^\[ipc\] cap-cost: each capability past the first costs "
                r"about [1-9]\d* cycles and the count does not change that",
                re.M,
            ),
            re.compile(
                r"^\[ipc\] cap-cost: measured under emulation",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            # The kernel's own sanity checks on the measurement.
            re.compile(r"did not cost more than carrying none", re.M),
            re.compile(r"is not plausibly flat", re.M),
        ],
    ),
    # ---- M10: interrupt handling ---------------------------------------
    #
    # One positive scenario and three negatives. The positive one
    # carries the milestone's claim; the negatives are where its value
    # is, because a kernel that delivers interrupts but cannot refuse
    # them has gained nothing over M9.
    "irqtest": Expectation(
        name="interrupt handling",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            *VM_INIT_LINES,
            *HEAP_INIT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            # The one IRQControl, seeded at boot. There is exactly one
            # and nothing can retype a second, so a boot without this
            # line is a boot in which no line can ever be claimed.
            re.compile(
                r"^\[cap\] IRQControl seeded in object slot \d+; \d+ "
                r"interrupt lines can be claimed from it",
                re.M,
            ),
            # 1. The one-owner rule, and the refusal leaving no trace.
            #    Requiring the second claim's refusal by name is what
            #    distinguishes this from a kernel that simply has one
            #    handler slot.
            re.compile(
                r"^\[irq\] derivation: IRQ \d+ claimed and masked; a second "
                r"claim on it gives IrqAlreadyOwned and leaves no trace; "
                r"IRQ \d+ claimed independently",
                re.M,
            ),
            # 2. Binding, and -- the load-bearing half -- the line still
            #    masked afterwards. A SetNotification that opened the
            #    line would make the Ack contract meaningless.
            re.compile(
                r"^\[irq\] binding: handler points at a Notification badged "
                r"0x00a50000; a non-Notification gives WrongType and an "
                r"empty slot gives EmptySlot; the line is still masked",
                re.M,
            ),
            # 3. **The milestone's claim.** A real interrupt, through a
            #    real vector, waking a thread that is not the kernel --
            #    with the badge from its own capability, which is what
            #    shows the path reads ADR 037's field rather than a
            #    constant.
            re.compile(
                r"^\[irq\] delivery: a self-IPI on vector \d+ reached a "
                r"thread blocked in Wait, with badge 0x00a50000; the line "
                r"was masked before the signal",
                re.M,
            ),
            # 4. **The criterion this milestone turns on**, in two
            #    halves that must both appear.
            #
            #    The first is positive evidence that the interrupt
            #    arrived and was *refused*: `while_masked` moving is
            #    what distinguishes a working mask from an interrupt
            #    that was never sent. A scenario that slept and then
            #    asserted nothing had happened could not tell those
            #    apart.
            #
            #    The `[1-9]` on the second figure is deliberate: the
            #    counter must actually move. `\d+` would be satisfied
            #    by 0 -> 0.
            re.compile(
                r"^\[irq\] masked: an interrupt on the shut line was "
                r"refused \(while_masked \d+ -> [1-9]\d*\) and not "
                r"delivered \(delivered stayed at \d+\)",
                re.M,
            ),
            #    The second half rules out the opposite failure: a
            #    kernel that masks a line permanently would pass the
            #    first and has not isolated a driver, it has silenced
            #    a device.
            re.compile(
                r"^\[irq\] ack: the identical stimulus was delivered once "
                r"the line was reopened \(\d+ -> [1-9]\d* delivered\)\. One "
                r"interrupt per Ack",
                re.M,
            ),
            # 5. The measurement, with its caveat as required text --
            #    the rule M6, M8 and M9 all follow. A reader taking an
            #    emulated figure for silicon would draw the wrong
            #    conclusion.
            re.compile(
                r"^\[irq\] wake latency: 32 samples, minimum [1-9]\d* "
                r"cycles, mean \d+, maximum \d+",
                re.M,
            ),
            re.compile(
                r"^\[irq\] wake latency: .*Measured under emulation",
                re.M | re.S,
            ),
            # The counters, which say what the whole run did. Orphaned
            # and stale must be zero here: both are real conditions
            # with their own scenarios, and seeing either in the
            # *positive* run would mean an interrupt went somewhere
            # unintended.
            re.compile(
                r"^\[irq\] refused: 0 orphaned \(no owner\), 0 stale "
                r"notification \(capability deleted\), \d+ unbound",
                re.M,
            ),
            re.compile(r"^\[irq\] all interrupt tests passed", re.M),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            POISON_IN_REGISTERS,
            # The scenario's own diagnoses, each of which names a way
            # the run could have proved nothing.
            re.compile(r"the mask has not been tested", re.M),
            re.compile(r"a second interrupt was delivered", re.M),
        ],
    ),
    "irqnocap": Expectation(
        name="claiming a line without IRQControl",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            re.compile(
                r"^\[irq\] refused with WrongType\(Endpoint\), as ADR 041 "
                r"requires",
                re.M,
            ),
            # The refusal left nothing behind. Both halves -- an
            # unowned line and an empty slot -- because a Get that
            # took the line but wrote no capability would be worse
            # than one that did neither: the line would be
            # permanently unclaimable.
            re.compile(
                r"^\[irq\] the line is unowned and the destination slot is "
                r"empty; a refused Get leaves no trace",
                re.M,
            ),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
            re.compile(
                r"^\[PANIC\] irq-no-cap completed its checks",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"M9a checks passed", re.M),
            re.compile(r"instead of WrongType", re.M),
            re.compile(r"still took ownership", re.M),
        ],
    ),
    "irqrevoked": Expectation(
        name="revoked interrupt destination",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            re.compile(
                r"^\[irq\] bound and open; now deleting the capability the "
                r"handler points at",
                re.M,
            ),
            # The detection, and the delivery that did not happen.
            # `[1-9]` on the stale counter: it must move, or the
            # interrupt never arrived and nothing was tested.
            re.compile(
                r"^\[irq\] detected: stale_notification \d+ -> [1-9]\d*, "
                r"delivered unchanged at \d+",
                re.M,
            ),
            # The line stays shut. A driver that has lost its
            # destination must not leave its device raising interrupts
            # that are refused forever.
            re.compile(
                r"^\[irq\] the kernel is still running and the line is "
                r"masked; a lost destination isolates one driver rather "
                r"than stopping the machine",
                re.M,
            ),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
            re.compile(
                r"^\[PANIC\] irq-revoked completed its checks",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"M9a checks passed", re.M),
            # The failures this scenario exists to rule out.
            re.compile(r"delivered through a deleted capability", re.M),
            re.compile(r"reopened after its destination vanished", re.M),
        ],
    ),
    "irqalloc": Expectation(
        name="allocation inside a device interrupt",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            re.compile(
                r"^\[irq\] armed; raising IRQ \d+ \(must panic in the "
                r"handler\)",
                re.M,
            ),
            # `in_interrupt=true` read from inside the handler. M5
            # drove this counter by hand and M6 reached it from the
            # timer; this is the first time it is reached from a path
            # a component triggers.
            re.compile(
                r"^\[irq\] inside the device interrupt handler, "
                r"in_interrupt=true; calling allocate\(\) \(must panic\)",
                re.M,
            ),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
            re.compile(
                r"^\[PANIC\] frame allocation from inside an interrupt handler",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"M9a checks passed", re.M),
            re.compile(r"does not work on the path that matters", re.M),
            re.compile(r"the tripwire never fired", re.M),
        ],
    ),
    "irquser": Expectation(
        name="a ring 3 thread waits for an interrupt",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            *VSPACE_INIT_LINES,
            # The thread is genuinely **blocked** before the interrupt
            # is raised. Required text, and load-bearing: a `Wait` that
            # returned immediately -- because the mask was already set,
            # say -- would wake "successfully" without the interrupt
            # path having been involved at all. The scenario checks the
            # thread's state rather than assuming it, and this line is
            # the outside evidence that it did.
            re.compile(
                r"^\[irq\] the user thread is blocked at cpl 3 inside Wait; "
                r"raising IRQ \d+",
                re.M,
            ),
            # **M10's purpose.** A device interrupt reaching a driver
            # outside the kernel, at the lowest privilege level, with
            # the badge its own capability carries.
            re.compile(
                r"^\[irq\] ring 3: a thread at cpl 3 blocked in Wait, a "
                r"hardware interrupt on vector \d+ arrived, and the thread "
                r"resumed with badge 0x00a50000",
                re.M,
            ),
            re.compile(
                r"^\[irq\] \d+ delivered, \d+ acks; \d+ arrived while masked",
                re.M,
            ),
            re.compile(r"^\[irq\] all ring 3 interrupt checks passed", re.M),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            POISON_IN_REGISTERS,
            # CPL 0 would mean the thread never left the kernel, and
            # every other line here would read the same.
            re.compile(r"\(cpl 0\)", re.M),
            # The scenario's own diagnoses.
            re.compile(r"reported but never blocked", re.M),
            re.compile(r"never woke", re.M),
        ],
    ),
    # ---- M11: user-space address space management ----------------------
    #
    # One positive scenario, three negatives and the read-only
    # enforcement check. Three of the five exit 33, including two
    # negatives: a refused request is an outcome, not a fault.
    "vsusertest": Expectation(
        name="a component manages its own address space",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            *VM_INIT_LINES,
            *HEAP_INIT_LINES,
            *CAP_INIT_LINES,
            *THREAD_INIT_LINES,
            *VSPACE_INIT_LINES,
            # The component really has its own CSpace. Without this
            # the scenario would be M8 with extra steps -- and the
            # foreign-access negative below would be untestable.
            re.compile(
                r"^\[vsuser\] component built: own CNode \(root radix \d+, "
                r"leaf radix \d+\), own Untyped of [1-9]\d* KiB, own "
                r"VSpace; the kernel mapped only its code and stack",
                re.M,
            ),
            # 1-4. The mapping sequence, with the address and the
            #      pattern both named. A bare "it worked" would pass
            #      on a kernel that mapped something else somewhere
            #      else.
            re.compile(
                r"^\[vsuser\] the component retyped a Frame and three "
                r"tables from its own Untyped, mapped them at "
                r"0x0000008000000000, wrote 0x5ec00ded11112222 and read "
                r"it back, then unmapped",
                re.M,
            ),
            # The refusal, which is ADR 027's rule seen from ring 3.
            # Required text: a kernel that silently allocated the
            # intermediate table would pass every other line here,
            # and the component would never know its budget had been
            # spent by someone else.
            re.compile(
                r"^\[vsuser\] the kernel refused to supply a missing "
                r"intermediate table \(MissingPageTable\); the component "
                r"supplied all three itself",
                re.M,
            ),
            # 5. **The milestone's result.** The child's own pattern,
            #    required by value: the parent could report success
            #    for three invocations on a kernel where Resume did
            #    nothing, and only a report from the child itself
            #    rules that out.
            re.compile(
                r"^\[vsuser\] a ring 3 component built a second component "
                r"-- its own CSpace, VSpace and TCB, all from its own "
                r"Untyped -- and started it; the child reported "
                r"0xc1d0000033334444 from cpl 3",
                re.M,
            ),
            re.compile(
                r"^\[vsuser\] all user-space address space tests passed",
                re.M,
            ),
            # The invocations went through the capability path, and
            # some returned errors -- the MissingPageTable and the
            # second Unmap, both deliberate.
            re.compile(
                r"^\[syscall\] [1-9]\d* calls \([1-9]\d* returned an error\)",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            POISON_IN_REGISTERS,
            # The scenario's own diagnoses, each naming a way the run
            # could have proved nothing.
            re.compile(r"never ran", re.M),
            re.compile(r"the kernel produced a page table", re.M),
        ],
    ),
    "vsuserkernel": Expectation(
        name="a component maps into the kernel half",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            *VSPACE_INIT_LINES,
            # All four refused, and the kernel's own mappings still
            # working. The second half is why this exits 33: a user
            # mapping in the kernel half would be visible in *every*
            # address space, because the top-level entries are shared
            # (ADR 027) -- so a failure here is not one component's
            # problem.
            re.compile(
                r"^\[vsuser\] all 4 refused with NotUserAddress and the "
                r"kernel's own mappings still work; this is an error "
                r"path, not a fault",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            POISON_IN_REGISTERS,
            re.compile(r"rather than an error", re.M),
        ],
    ),
    "vsuserforeign": Expectation(
        name="a component reaches for another's address space",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            *VSPACE_INIT_LINES,
            # B was given the pointer rather than guessing it. Stated
            # before the result, so the log carries the premise: if B
            # had had to guess, a failure would say nothing.
            re.compile(
                r"^\[vsuser\] B was handed the CPtr 0x[0-9a-f]{16}, which "
                r"names A's VSpace in A's CSpace\. B will use it unchanged\.",
                re.M,
            ),
            # **The claim**, in two halves that must both appear. The
            # failure alone would be consistent with Frame.Map being
            # broken; the control is what makes it about isolation.
            re.compile(
                r"^\[vsuser\] B's attempt on A's VSpace failed in CPtr "
                r"resolution \(error \d+\), and the identical call into "
                r"B's own VSpace succeeded\. The pointer is not forbidden "
                r"-- it names nothing in B's CSpace\.",
                re.M,
            ),
            re.compile(
                r"^\[vsuser\] isolation holds at the capability level",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            POISON_IN_REGISTERS,
            # The failure this scenario exists to rule out.
            re.compile(r"The two components are not isolated", re.M),
            # And the weaker outcome: a refusal from inside the
            # mapping rather than from resolution.
            re.compile(r"comes from inside the mapping", re.M),
        ],
    ),
    "vsuserexhaust": Expectation(
        name="a component exhausts its own budget",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            *VSPACE_INIT_LINES,
            # The budget is finite and stated, so "it ran out" is a
            # fact about a known quantity rather than about an
            # unknown one.
            re.compile(
                r"^\[vsuser\] the component holds [1-9]\d* KiB and will "
                r"retype \d+ page tables \(\d+ KiB\) from it",
                re.M,
            ),
            # **ADR 022's claim, measured.** The root Untypeds
            # unchanged is the load-bearing half: a clean error on
            # its own would be satisfied by a kernel that spent
            # somebody else's memory and then gave up.
            re.compile(
                r"^\[vsuser\] the component exhausted its own [1-9]\d* KiB "
                r"cleanly \(error \d+\), the root Untypeds still hold "
                r"[1-9]\d* KiB -- unchanged -- and the kernel heap did "
                r"not move",
                re.M,
            ),
            re.compile(
                r"^\[vsuser\] a component can starve itself and nothing "
                r"else",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
            POISON_IN_REGISTERS,
            # The three failures the scenario names.
            re.compile(r"the last call still succeeded", re.M),
            re.compile(r"while a component spent its own budget", re.M),
            re.compile(r"while a component exhausted its own Untyped", re.M),
        ],
    ),
    "vsuserro": Expectation(
        name="a component writes to its own read-only page",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            *VSPACE_INIT_LINES,
            re.compile(
                r"^\[vsuser\] mapping 0x[0-9a-f]{16} read-only, then "
                r"writing to it; expected #PF with the user and write "
                r"bits set",
                re.M,
            ),
            re.compile(
                r"^\*\*\* EXCEPTION: #PF page fault \(vector 14\)",
                re.M,
            ),
            # **User, write, protection-violation.** All three words,
            # and each rules out a different wrong reason for the
            # fault:
            #
            #   "user"       -- it happened at CPL 3, not in the
            #                   kernel;
            #   "write"      -- it was the store, not the read before
            #                   it;
            #   "protection" -- the page is PRESENT. A not-present
            #                   fault would mean the mapping was
            #                   never made, and the scenario would
            #                   have proved that Frame.Map failed
            #                   rather than that ReadOnly was
            #                   enforced.
            re.compile(
                r"^\[trap\] cr2=0x[0-9a-f]{16}  \(user write "
                r"protection-violation\)",
                re.M,
            ),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
        ],
        forbidden=[
            re.compile(r"M9a checks passed", re.M),
            # The failure this exists to rule out: the write
            # succeeding means ReadOnly reached the page table as
            # something writable.
            re.compile(r"did not fault", re.M),
            # A supervisor fault would mean the write happened in the
            # kernel rather than in ring 3.
            re.compile(r"\(supervisor write", re.M),
        ],
    ),
    # ---- M12: the root task --------------------------------------------
    "roottest": Expectation(
        name="the root task, and the kernel that stops",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *THREAD_INIT_LINES,
            *VSPACE_INIT_LINES,
            # The kernel handed everything over and then sealed. The
            # operation count is in the line, so a seal that closed
            # before the handover would read differently.
            re.compile(
                r"^\[root\] sealed after \d+ privileged operations", re.M
            ),
            # Criterion 1: it read what the kernel wrote.
            re.compile(
                r"^\[roottest\] the root task read its BootInfo at "
                r"0x[0-9a-f]{16}: magic ok, version \d+, \d+ untypeds",
                re.M,
            ),
            # Criterion 3. **All four terms**, and the regex demands
            # the "three mechanisms" sentence: the figures come from
            # the capabilities, from call counting and from the
            # bitmap, which is what lets them disagree. A single
            # derived number could not.
            re.compile(
                r"^\[roottest\] accounting: \d+ MiB in untypeds \+ \d+ "
                r"MiB kept by the kernel \+ \d+ KiB unreachable = \d+ "
                r"MiB usable",
                re.M,
            ),
            # Criterion 2, the milestone. The badge is in the line
            # because the badge is the claim -- the sender did not
            # choose it.
            re.compile(
                r"^\[roottest\] two components built by a ring 3 program "
                r"exchanged a message; the receiver woke with badge "
                r"0x5eed",
                re.M,
            ),
            # Criterion 4.
            re.compile(
                r"^\[roottest\] the kernel performed \d+ privileged "
                r"operations during boot and none since the seal",
                re.M,
            ),
            re.compile(r"^\[roottest\] all root task tests passed", re.M),
            re.compile(r"^\[boot\] M12 checks passed", re.M),
        ],
        forbidden=[
            # The root task's panic handler cannot print, so its only
            # trace is the kernel reporting that the channel fired.
            re.compile(r"the root task's panic handler ran", re.M),
            re.compile(r"never finished", re.M),
            # A sum that balances only because the remainder absorbed
            # the difference.
            re.compile(r"do not balance", re.M),
        ],
    ),
    "rootnountyped": Expectation(
        name="the root task is given no memory",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            re.compile(
                r"^\[root\] root-no-untyped: handing over 0 of \d+ "
                r"untypeds",
                re.M,
            ),
            # **Error 11 exactly**, not `\d+`. The first version of
            # this pattern accepted any digits and the kernel
            # printed "reported error 0" -- a bare ERROR_FLAG, which
            # says only that something went wrong. A root task that
            # set the flag for any reason would have matched.
            re.compile(
                r"^\[roottest\] the root task found no usable Untyped, "
                r"reported error 11, and stopped; the kernel is still "
                r"running and did nothing on its behalf",
                re.M,
            ),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            # Zero is not an error code.
            re.compile(r"reported error 0,", re.M),
            # The kernel stepping in would mean it had not really
            # handed the memory over.
            re.compile(
                r"the kernel performed privileged operations while its "
                r"component starved",
                re.M,
            ),
        ],
    ),
    "rootdies": Expectation(
        name="the root task dies",
        status=STATUS_PANIC,
        required=[
            *COMMON_BOOT_LINES,
            # It was genuinely running first. Without this the panic
            # would be consistent with a root task that never started.
            re.compile(
                r"^\[roottest\] the root task is running \(it read its "
                r"BootInfo\); revoking its TCB now",
                re.M,
            ),
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
        ],
        forbidden=[
            re.compile(r"M12 checks passed", re.M),
            # The failure this rules out: the kernel carrying on with
            # nothing able to name any resource.
            re.compile(r"carried on", re.M),
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