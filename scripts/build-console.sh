#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# Build the console component and emit its flat image.
#
# Sibling of scripts/build-screen.sh, and the same two jobs: produce
# the flat image, and find the offset of the geometry struct the root
# task patches into it. See build-screen.sh for why the offset is
# read from the symbol table rather than fixed.

set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)

if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

OUT=${1:-build/console.bin}
PROFILE=${PROFILE:-release}

LINK_LD="$ROOT/console/link.ld"

export RUSTFLAGS="\
-C link-arg=-T$LINK_LD \
-C relocation-model=static \
-C link-arg=--build-id=none \
-C link-arg=-znoexecstack"

cargo build \
    --manifest-path console/Cargo.toml \
    --target x86_64-unknown-none \
    --profile "$PROFILE" \
    -Z build-std=core,compiler_builtins \
    -Z build-std-features=compiler-builtins-mem \
    --target-dir "$ROOT/target/console"

ELF="$ROOT/target/console/x86_64-unknown-none/$PROFILE/console"

mkdir -p "$(dirname "$OUT")"
rust-objcopy -O binary "$ELF" "$OUT" 2>/dev/null \
    || llvm-objcopy -O binary "$ELF" "$OUT" 2>/dev/null \
    || objcopy -O binary "$ELF" "$OUT"

ENTRY=$(readelf -h "$ELF" | awk '/Entry point/ { print $NF }')
if [ "$ENTRY" != "0x400000" ]; then
    echo "console entry is $ENTRY, expected 0x400000." >&2
    echo "The root task jumps to the load address, so _start must be" >&2
    echo "the first byte of the image -- check .text.start in" >&2
    echo "console/link.ld." >&2
    exit 1
fi

GEOM_ADDR=$(readelf -sW "$ELF" | awk '$8 == "CONSOLE_GEOMETRY" { print $2 }')
if [ -z "$GEOM_ADDR" ]; then
    echo "console: CONSOLE_GEOMETRY not found in the symbol table." >&2
    echo "The root task patches the geometry into the image at this" >&2
    echo "symbol's offset; without it the component would start with" >&2
    echo "a zero width, draw nothing, and report success." >&2
    exit 1
fi

GEOM_OFFSET=$(( 0x$GEOM_ADDR - 0x400000 ))
if [ "$GEOM_OFFSET" -lt 0 ]; then
    echo "console: CONSOLE_GEOMETRY is at 0x$GEOM_ADDR, below the load" >&2
    echo "address 0x400000. The linker script has changed shape." >&2
    exit 1
fi

echo "$GEOM_OFFSET" > "$(dirname "$OUT")/console-geometry-offset"

# Three pages, measured. The first guess was four, the same way the
# keyboard's was three and the real figure two -- which is what this
# line is for. The root task maps exactly this many frames and
# console/link.ld asserts the same bound.
MAX=12288
SIZE=$(stat -c %s "$OUT")
if [ "$SIZE" -gt "$MAX" ]; then
    echo "console image is $SIZE bytes; the root task maps $MAX." >&2
    echo "Either raise CONSOLE_CODE_PAGES in roottask/src/console.rs" >&2
    echo "and the ASSERT in console/link.ld together, or make the" >&2
    echo "component smaller." >&2
    exit 1
fi

echo "console: $SIZE bytes, entry $ENTRY, geometry at +$GEOM_OFFSET -> $OUT"