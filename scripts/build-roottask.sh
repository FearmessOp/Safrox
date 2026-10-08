#!/bin/bash
# SPDX-License-Identifier: BSD-2-Clause
#
# Build the root task and emit its flat image.
#
# # Why a script rather than a `.cargo/config.toml`
#
# **Cargo merges `rustflags` across config files; it does not let the
# nearer one win.** A `roottask/.cargo/config.toml` is read *in
# addition to* the workspace's, and the link line came out with both
# linker scripts:
#
#     -Tkernel/linker.ld ... -Troottask/link.ld
#
# failing with "cannot find linker script kernel/linker.ld" -- the
# paths are relative and the working directory was `roottask/`. Had
# it been found, both scripts would have applied in order and the
# root task would have linked at -2 GiB, in the kernel half, where a
# CPL 3 thread cannot fetch a single instruction.
#
# `RUSTFLAGS` **replaces** the whole list. That is the hazard the
# kernel's own .cargo/config.toml warns about at length -- "an
# environment RUSTFLAGS *replaces* the list above wholesale ... which
# would silently drop the linker script and produce a kernel linked
# at address 0" -- and here it is exactly what is wanted. The same
# property, read as a danger in one direction and a tool in the
# other.
#
# `CARGO_TARGET_X86_64_UNKNOWN_NONE_RUSTFLAGS` does **not** work:
# tried first, and the kernel's flags were still in the link line.
# A `[target.*]` table in a config file wins over the per-target
# environment variable; only the blanket `RUSTFLAGS` replaces.
#
# All of this was found by building. An earlier experiment put a
# test crate in /tmp -- outside the workspace, where there is no
# parent config to merge with -- and proved only that a separate
# crate *can* have its own linker script, which was not the question.

set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)

if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

OUT=${1:-build/roottask.bin}
PROFILE=${PROFILE:-release}

# Absolute path: the linker's working directory is not this one.
LINK_LD="$ROOT/roottask/link.ld"

# Replaces, not merges. Every flag the root task needs has to be
# here, including the ones the kernel's config also sets -- there is
# no inheritance, and that is the point.
export RUSTFLAGS="\
-C link-arg=-T$LINK_LD \
-C relocation-model=static \
-C link-arg=--build-id=none \
-C link-arg=-znoexecstack"

# `--manifest-path` rather than `cd roottask`, so the target
# directory stays under the workspace and `make clean` reaches it.
cargo build \
    --manifest-path roottask/Cargo.toml \
    --target x86_64-unknown-none \
    --profile "$PROFILE" \
    -Z build-std=core,compiler_builtins \
    -Z build-std-features=compiler-builtins-mem \
    --target-dir "$ROOT/target/roottask"

ELF="$ROOT/target/roottask/x86_64-unknown-none/$PROFILE/roottask"

# The kernel has no ELF loader and should not have one: loading is a
# component's job (ADR 043, and M11's parent did exactly that for its
# child). So the image is flattened here, at build time, into the one
# segment the linker script produces.
mkdir -p "$(dirname "$OUT")"
rust-objcopy -O binary "$ELF" "$OUT" 2>/dev/null \
    || llvm-objcopy -O binary "$ELF" "$OUT" 2>/dev/null \
    || objcopy -O binary "$ELF" "$OUT"

# Two things the kernel relies on, checked here because the kernel
# cannot check them: it copies a fixed range to a fixed address and
# jumps to the first byte.
ENTRY=$(readelf -h "$ELF" | awk '/Entry point/ { print $NF }')
if [ "$ENTRY" != "0x400000" ]; then
    echo "roottask entry is $ENTRY, expected 0x400000." >&2
    echo "The kernel jumps to the load address, so _start must be" >&2
    echo "the first byte of the image -- check .text.start in" >&2
    echo "roottask/link.ld." >&2
    exit 1
fi

# Two pages, and the figure is measured rather than chosen: the
# first version of this check said one page and the image came out
# at 7432 bytes. The kernel maps exactly this many frames, the
# linker script asserts the same bound, and they are written down
# in both places so that a change has to be made twice on purpose
# rather than once by accident.
MAX=16384
SIZE=$(stat -c %s "$OUT")
if [ "$SIZE" -gt "$MAX" ]; then
    echo "roottask image is $SIZE bytes; the kernel maps $MAX." >&2
    echo "Either raise ROOT_IMAGE_PAGES in the kernel and the" >&2
    echo "ASSERT in roottask/link.ld together, or make the root" >&2
    echo "task smaller." >&2
    exit 1
fi

echo "roottask: $SIZE bytes, entry $ENTRY -> $OUT"