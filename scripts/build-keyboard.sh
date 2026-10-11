#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# Build the keyboard component and emit its flat image.
#
# Sibling of scripts/build-driver.sh and scripts/build-screen.sh; see
# the first for why RUSTFLAGS is used rather than a crate-local
# .cargo/config.toml (Cargo *merges* rustflags across config files,
# so a nearer file adds to the linker line rather than replacing it).
#
# Like the screen's, this finds the offset of a symbol the root task
# patches. Unlike the screen's, the patched value is one word: which
# scenario this image is being built for. A component cannot be told
# that over IPC, because the question is answered before it has
# anybody to ask.

set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)

if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

OUT=${1:-build/keyboard.bin}
PROFILE=${PROFILE:-release}

LINK_LD="$ROOT/keyboard/link.ld"

export RUSTFLAGS="\
-C link-arg=-T$LINK_LD \
-C relocation-model=static \
-C link-arg=--build-id=none \
-C link-arg=-znoexecstack"

cargo build \
    --manifest-path keyboard/Cargo.toml \
    --target x86_64-unknown-none \
    --profile "$PROFILE" \
    -Z build-std=core,compiler_builtins \
    -Z build-std-features=compiler-builtins-mem \
    --target-dir "$ROOT/target/keyboard"

ELF="$ROOT/target/keyboard/x86_64-unknown-none/$PROFILE/keyboard-driver"

mkdir -p "$(dirname "$OUT")"
rust-objcopy -O binary "$ELF" "$OUT" 2>/dev/null \
    || llvm-objcopy -O binary "$ELF" "$OUT" 2>/dev/null \
    || objcopy -O binary "$ELF" "$OUT"

# The root task jumps to the load address, so `_start` must be the
# first byte. Checked here because the root task cannot check it: it
# copies bytes and jumps.
ENTRY=$(readelf -h "$ELF" | awk '/Entry point/ { print $NF }')
if [ "$ENTRY" != "0x400000" ]; then
    echo "keyboard entry is $ENTRY, expected 0x400000." >&2
    echo "The root task jumps to the load address, so _start must be" >&2
    echo "the first byte of the image -- check .text.start in" >&2
    echo "keyboard/link.ld." >&2
    exit 1
fi

# Where the scenario word lives, as an offset into the flat image.
#
# Read from the symbol table rather than fixed, for the reason
# build-screen.sh gives: a fixed offset breaks silently the first
# time a line is added above it, and the component then reads its
# scenario out of the middle of an instruction.
SCEN_ADDR=$(readelf -sW "$ELF" | awk '$8 == "KEYBOARD_SCENARIO" { print $2 }')
if [ -z "$SCEN_ADDR" ]; then
    echo "keyboard: KEYBOARD_SCENARIO not found in the symbol table." >&2
    echo "The root task patches the scenario into the image at this" >&2
    echo "symbol's offset; without it every build would be the" >&2
    echo "ordinary scenario and kbd-isolation would silently test" >&2
    echo "nothing." >&2
    exit 1
fi

SCEN_OFFSET=$(( 0x$SCEN_ADDR - 0x400000 ))
if [ "$SCEN_OFFSET" -lt 0 ]; then
    echo "keyboard: KEYBOARD_SCENARIO is at 0x$SCEN_ADDR, below the" >&2
    echo "load address 0x400000. The linker script has changed shape." >&2
    exit 1
fi

echo "$SCEN_OFFSET" > "$(dirname "$OUT")/keyboard-scenario-offset"

# Two pages, measured rather than chosen -- and the first guess was
# three, until this line printed the real figure. The root task maps
# exactly this many frames and keyboard/link.ld asserts the same
# bound; written in both places so a change has to be made twice on
# purpose.
MAX=8192
SIZE=$(stat -c %s "$OUT")
if [ "$SIZE" -gt "$MAX" ]; then
    echo "keyboard image is $SIZE bytes; the root task maps $MAX." >&2
    echo "Either raise KEYBOARD_CODE_PAGES in roottask/src/keyboard.rs" >&2
    echo "and the ASSERT in keyboard/link.ld together, or make the" >&2
    echo "component smaller." >&2
    exit 1
fi

echo "keyboard: $SIZE bytes, entry $ENTRY, scenario at +$SCEN_OFFSET -> $OUT"