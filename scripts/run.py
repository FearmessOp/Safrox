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


@dataclass
class Expectation:
    """What a given test mode requires of a run."""

    name: str
    status: int
    required: list[re.Pattern[str]]
    forbidden: list[re.Pattern[str]]


# Lines the acceptance test demands on the serial console.
EXPECTATIONS = {
    "success": Expectation(
        name="normal boot",
        status=STATUS_SUCCESS,
        required=[
            re.compile(r"^PythOS \S+", re.M),                       # name/version
            re.compile(r"^\[boot\] bootloader: \S+ \S+", re.M),     # from Limine
            re.compile(r"^\[boot\] higher-half base: 0xffffffff[0-9a-f]+", re.M),
            re.compile(r"^\[boot\] M0 checks passed", re.M),
        ],
        forbidden=[re.compile(r"KERNEL PANIC", re.M)],
    ),
    "panic": Expectation(
        name="panic test",
        status=STATUS_PANIC,
        required=[
            re.compile(r"^\*\*\* KERNEL PANIC \*\*\*", re.M),
            # The panic must name a real source file and line, not just say
            # "something went wrong". Rust's PanicInfo gives file:line:column.
            re.compile(r"^\[PANIC\] at \S+\.rs:\d+:\d+", re.M),
            re.compile(r"^\[PANIC\] deliberate panic requested", re.M),
        ],
        forbidden=[re.compile(r"M0 checks passed", re.M)],
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