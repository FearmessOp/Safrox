#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
#
# ADR 034, open question 7: what does `buffer[i - 3]` cost?
#
# Builds src/lib.rs with the kernel's target and optimisation level and
# prints the generated assembly for the two variants side by side. The
# answer goes into ADR 034's "Olcum" section, with the assembly quoted
# rather than summarised -- "no difference" is not a verifiable claim;
# two listings are.
set -e

cd "$(dirname "$0")"

# A non-interactive shell does not read the profile, so cargo's PATH
# entry is missing. The kernel build goes through `make`, which runs in
# a login shell; this probe does not.
if ! command -v rustc >/dev/null 2>&1; then
    . "$HOME/.cargo/env"
fi

rustc --edition 2021 --crate-type lib -O \
      --target x86_64-unknown-none \
      --emit asm -o probe.s src/lib.rs

# Symbols are mangled (v0: `_RNvCs..._3lib11read_offset`), so match on
# the suffix and stop at the next top-level label.
body() {
    awk -v want="$1" '
        /^[^[:space:].]+:$/ { inside = (index($0, want ":") > 0) }
        inside && /^[[:space:]]+[a-z]/ { print }
    ' probe.s
}

show() {
    echo "=== $1 ==="
    body "$1"
    echo
}

count() {
    body "$1" | grep -c . || true
}

for f in read_wasteful read_offset copy_wasteful copy_offset; do
    show "$f"
done

echo "=== instruction counts ==="
for f in read_wasteful read_offset copy_wasteful copy_offset; do
    printf '%-16s %s\n' "$f" "$(count "$f")"
done