#!/bin/bash
# SPDX-License-Identifier: BSD-2-Clause
#
# Build the screen component and emit its flat image.
#
# Sibling of scripts/build-driver.sh; see that file for why RUSTFLAGS
# is used rather than a crate-local .cargo/config.toml (Cargo *merges*
# rustflags across config files, so a nearer file adds to the linker
# line rather than replacing it).
#
# One thing this does that the driver's does not: it finds the offset
# of the `SCREEN_GEOMETRY` symbol and writes it beside the image.
#
# The root task patches the screen's geometry into the image before
# copying it (see screen/src/main.rs). To do that it has to know
# where the struct is, and the alternatives were worse:
#
#   * a fixed offset -- which would break the first time a line was
#     added above it, silently, because the magic would then be read
#     out of the middle of an instruction;
#   * putting the struct first in .rodata with a linker section --
#     the same fragility with more machinery;
#   * passing the geometry over IPC -- a protocol for five numbers
#     that never change.
#
# Reading the symbol table is what the information is for. The build
# fails if the symbol is missing, so a rename cannot produce an image
# that is patched in the wrong place.

set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)

if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

OUT=${1:-build/screen.bin}
PROFILE=${PROFILE:-release}

LINK_LD="$ROOT/screen/link.ld"

export RUSTFLAGS="\
-C link-arg=-T$LINK_LD \
-C relocation-model=static \
-C link-arg=--build-id=none \
-C link-arg=-znoexecstack"

cargo build \
    --manifest-path screen/Cargo.toml \
    --target x86_64-unknown-none \
    --profile "$PROFILE" \
    -Z build-std=core,compiler_builtins \
    -Z build-std-features=compiler-builtins-mem \
    --target-dir "$ROOT/target/screen"

ELF="$ROOT/target/screen/x86_64-unknown-none/$PROFILE/screen"

mkdir -p "$(dirname "$OUT")"
rust-objcopy -O binary "$ELF" "$OUT" 2>/dev/null \
    || llvm-objcopy -O binary "$ELF" "$OUT" 2>/dev/null \
    || objcopy -O binary "$ELF" "$OUT"

# The root task jumps to the load address, so `_start` must be the
# first byte. Checked here because the root task cannot check it: it
# copies bytes and jumps.
ENTRY=$(readelf -h "$ELF" | awk '/Entry point/ { print $NF }')
if [ "$ENTRY" != "0x400000" ]; then
    echo "screen entry is $ENTRY, expected 0x400000." >&2
    echo "The root task jumps to the load address, so _start must be" >&2
    echo "the first byte of the image -- check .text.start in" >&2
    echo "screen/link.ld." >&2
    exit 1
fi

# Where the geometry struct lives, as an offset into the flat image.
#
# `readelf -s` gives the virtual address; the image starts at
# 0x400000, so the offset is the difference. Written to a file the
# root task's build includes, rather than passed through the
# environment, so that a stale value is a stale file rather than a
# silently missing variable.
GEOM_ADDR=$(readelf -sW "$ELF" | awk '$8 == "SCREEN_GEOMETRY" { print $2 }')
if [ -z "$GEOM_ADDR" ]; then
    echo "screen: SCREEN_GEOMETRY not found in the symbol table." >&2
    echo "The root task patches the geometry into the image at this" >&2
    echo "symbol's offset; without it the component would start with" >&2
    echo "a zero width and report success having drawn nothing." >&2
    exit 1
fi

GEOM_OFFSET=$(( 0x$GEOM_ADDR - 0x400000 ))
if [ "$GEOM_OFFSET" -lt 0 ]; then
    echo "screen: SCREEN_GEOMETRY is at 0x$GEOM_ADDR, below the load" >&2
    echo "address 0x400000. The linker script has changed shape." >&2
    exit 1
fi

echo "$GEOM_OFFSET" > "$(dirname "$OUT")/screen-geometry-offset"

# Four pages, measured rather than chosen. The root task maps exactly
# this many frames and screen/link.ld asserts the same bound; written
# in both places so a change has to be made twice on purpose.
MAX=16384
SIZE=$(stat -c %s "$OUT")
if [ "$SIZE" -gt "$MAX" ]; then
    echo "screen image is $SIZE bytes; the root task maps $MAX." >&2
    echo "Either raise SCREEN_CODE_PAGES in roottask/src/screen.rs and" >&2
    echo "the ASSERT in screen/link.ld together, or make the component" >&2
    echo "smaller." >&2
    exit 1
fi

echo "screen: $SIZE bytes, entry $ENTRY, geometry at +$GEOM_OFFSET -> $OUT"