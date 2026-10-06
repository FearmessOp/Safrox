#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""Report which build tools are present and how to install the missing ones."""

from __future__ import annotations

import shutil
import subprocess
import sys

# (binary, required, what it is used for)
TOOLS = [
    ("rustup", True, "manage the pinned nightly toolchain"),
    ("cargo", True, "build the kernel"),
    ("rustc", True, "compile the kernel"),
    ("make", True, "drive the build"),
    ("git", True, "fetch the Limine bootloader"),
    ("xorriso", True, "build the hybrid BIOS/UEFI ISO"),
    ("qemu-system-x86_64", True, "run the acceptance tests"),
    ("python3", True, "build and test helper scripts"),
    ("gdb", False, "`make debug`"),
]

APT_PACKAGES = {
    "make": "build-essential",
    "git": "git",
    "xorriso": "xorriso",
    "qemu-system-x86_64": "qemu-system-x86",
    "python3": "python3",
    "gdb": "gdb",
}

RUSTUP_INSTALL = (
    "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- \\\n"
    "        -y --default-toolchain nightly --profile minimal \\\n"
    "        --component rust-src,llvm-tools"
)

# rust-src is not optional: -Z build-std compiles core and compiler_builtins
# from source for x86_64-unknown-none, which has no precompiled std.
REQUIRED_RUST_COMPONENTS = ["rust-src"]
REQUIRED_RUST_TARGET = "x86_64-unknown-none"


def run(args: list[str]) -> tuple[int, str]:
    try:
        completed = subprocess.run(
            args,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=30,
            check=False,
        )
        return completed.returncode, completed.stdout.decode("utf-8", "replace")
    except (OSError, subprocess.SubprocessError) as exc:
        return 1, str(exc)


def version_of(path: str) -> str:
    for flag in ("--version", "-version"):
        code, out = run([path, flag])
        if code == 0 and out.strip():
            return out.splitlines()[0].strip()
    return "unknown version"


def check_rust_details() -> list[str]:
    """Verify the toolchain can actually build a bare-metal kernel."""
    problems: list[str] = []

    code, out = run(["rustc", "--version"])
    if code == 0 and "nightly" not in out:
        problems.append(
            f"rustc is not a nightly build ({out.strip()}); -Z build-std "
            f"requires nightly. rust-toolchain.toml should pin this "
            f"automatically -- try running from the project root."
        )

    code, out = run(["rustup", "component", "list", "--installed"])
    if code == 0:
        for component in REQUIRED_RUST_COMPONENTS:
            if component not in out:
                problems.append(
                    f"rustup component '{component}' is missing; install with:\n"
                    f"        rustup component add {component}"
                )

    return problems


def main() -> int:
    missing_required: list[str] = []
    missing_optional: list[str] = []

    width = max(len(name) for name, _, _ in TOOLS)

    for name, required, purpose in TOOLS:
        path = shutil.which(name)
        if path is not None:
            print(f"  [ok]      {name:<{width}}  {version_of(path)}")
        else:
            tag = "MISSING" if required else "absent"
            print(f"  [{tag:<7}] {name:<{width}}  needed to {purpose}")
            (missing_required if required else missing_optional).append(name)

    problems: list[str] = []
    if shutil.which("rustup") and shutil.which("rustc"):
        problems = check_rust_details()
        for problem in problems:
            print(f"  [PROBLEM] {problem}")

    if missing_optional:
        print(f"\nOptional tools not found: {', '.join(missing_optional)}")

    if missing_required or problems:
        if missing_required:
            print(f"\nMissing required tools: {', '.join(missing_required)}")

        rust_missing = {"rustup", "cargo", "rustc"} & set(missing_required)
        apt_missing = sorted(
            {APT_PACKAGES[t] for t in missing_required if t in APT_PACKAGES}
        )

        if rust_missing:
            print(f"\nInstall the Rust toolchain:\n    {RUSTUP_INSTALL}")
            print(
                f"\n    rust-toolchain.toml pins the channel and the "
                f"{REQUIRED_RUST_TARGET} target, so rustup will fetch the "
                f"right one on first build."
            )

        if apt_missing:
            print(
                f"\nOn Debian/Ubuntu (including WSL):\n"
                f"    sudo apt-get update\n"
                f"    sudo apt-get install -y {' '.join(apt_missing)}\n"
                f"\nOn Fedora:\n"
                f"    sudo dnf install -y make git xorriso qemu-system-x86 python3\n"
                f"\nOn Arch:\n"
                f"    sudo pacman -S --needed base-devel git xorriso qemu-base python"
            )
        return 1

    print("\nAll required tools are present.")
    return 0


if __name__ == "__main__":
    sys.exit(main())