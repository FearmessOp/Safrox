#!/bin/bash
# SPDX-License-Identifier: BSD-2-Clause
#
# Build the serial driver and emit its flat image.
#
# Same shape as scripts/build-roottask.sh and for the same reason:
# Cargo *merges* `rustflags` across config files rather than letting
# the nearer one win, so a `driver/.cargo/config.toml` would produce
# a link line carrying both linker scripts. `RUSTFLAGS` replaces the
# whole list, which is the hazard the kernel's .cargo/config.toml
# warns about, used here deliberately. See build-roottask.sh for the
# long version -- this script is its sibling, not a new discovery.
#
# The one difference worth naming: this image is consumed by the
# **root task**, not by the kernel. It ends up in the root task's
# .rodata via include_bytes!, and the root task copies it into a
# Frame it owns and maps into the driver's address space (ADR 043:
# loading is a component's job).

set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)

if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

OUT=${1:-build/serial-driver.bin}
PROFILE=${PROFILE:-release}

LINK_LD="$ROOT/driver/link.ld"

export RUSTFLAGS="\
-C link-arg=-T$LINK_LD \
-C relocation-model=static \
-C link-arg=--build-id=none \
-C link-arg=-znoexecstack"

cargo build \
    --manifest-path driver/Cargo.toml \
    --target x86_64-unknown-none \
    --profile "$PROFILE" \
    -Z build-std=core,compiler_builtins \
    -Z build-std-features=compiler-builtins-mem \
    --target-dir "$ROOT/target/driver"

ELF="$ROOT/target/driver/x86_64-unknown-none/$PROFILE/serial-driver"

mkdir -p "$(dirname "$OUT")"
rust-objcopy -O binary "$ELF" "$OUT" 2>/dev/null \
    || llvm-objcopy -O binary "$ELF" "$OUT" 2>/dev/null \
    || objcopy -O binary "$ELF" "$OUT"

# The root task jumps to the load address, so `_start` must be the
# first byte of the image. Checked here because the root task cannot
# check it: it copies bytes and jumps.
ENTRY=$(readelf -h "$ELF" | awk '/Entry point/ { print $NF }')
if [ "$ENTRY" != "0x400000" ]; then
    echo "serial-driver entry is $ENTRY, expected 0x400000." >&2
    echo "The root task jumps to the load address, so _start must be" >&2
    echo "the first byte of the image -- check .text.start in" >&2
    echo "driver/link.ld." >&2
    exit 1
fi

# Two pages, and the figure is measured rather than chosen. The root
# task maps exactly this many frames, the linker script asserts the
# same bound, and they are written down in both places so that a
# change has to be made twice on purpose rather than once by accident.
MAX=8192
SIZE=$(stat -c %s "$OUT")
if [ "$SIZE" -gt "$MAX" ]; then
    echo "serial-driver image is $SIZE bytes; the root task maps $MAX." >&2
    echo "Either raise DRIVER_CODE_PAGES in roottask/src/main.rs and" >&2
    echo "the ASSERT in driver/link.ld together, or make the driver" >&2
    echo "smaller." >&2
    exit 1
fi

echo "serial-driver: $SIZE bytes, entry $ENTRY -> $OUT"