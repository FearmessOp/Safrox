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

# Lines the acceptance test demands on the serial console.
EXPECTATIONS = {
    "success": Expectation(
        name="normal boot",
        status=STATUS_SUCCESS,
        required=[
            *COMMON_BOOT_LINES,
            *CPU_INIT_LINES,
            re.compile(r"^\[boot\] M2 checks passed", re.M),
        ],
        forbidden=[
            re.compile(r"KERNEL PANIC", re.M),
            re.compile(r"EXCEPTION", re.M),
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
        forbidden=[re.compile(r"M2 checks passed", re.M)],
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
            re.compile(r"^\[boot\] M2 checks passed", re.M),
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
        forbidden=[re.compile(r"M2 checks passed", re.M)],
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
            re.compile(r"M2 checks passed", re.M),
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
            re.compile(r"^\[boot\] M2 checks passed", re.M),
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
            re.compile(r"M2 checks passed", re.M),
            # If this appeared, the allocator accepted the second free.
            re.compile(r"the allocator accepted a double free", re.M),
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