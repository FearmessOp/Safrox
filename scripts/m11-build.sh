#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# M11 build helper. Same shape as `m10-build.sh`, same two reasons:
#
#   * `cargo` is installed per-user and a non-login `wsl bash -c` shell
#     does not read ~/.profile, so cargo is not on PATH.
#   * `$?`, `$(...)` and quotes are expanded by the Windows shell before
#     bash ever sees them. A script file is passed through untouched.
#
# A separate file rather than an edit to the M10 one: the milestone
# scripts are a record of how each milestone was built, and rewriting
# M10's would make its log name a lie.

set -u

cd /mnt/c/Users/user/pyth || exit 1

if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

LOG=/tmp/m11-build.log

case "${1:-build}" in
    build)
        cargo build --profile release > "$LOG" 2>&1
        status=$?
        ;;
    clippy)
        cargo clippy --profile release --all-targets -- -D warnings \
            > "$LOG" 2>&1
        status=$?
        ;;
    fmt)
        cargo fmt --check > "$LOG" 2>&1
        status=$?
        ;;
    *)
        echo "usage: $0 [build|clippy|fmt]"
        exit 2
        ;;
esac

echo "EXIT=$status"
if [ "$status" -ne 0 ]; then
    # Errors only, and the first few: a cascade of follow-on errors
    # buries the one worth reading.
    grep -E '^(error|warning)' "$LOG" | head -40
    echo "--- first error in context ---"
    grep -n -A 12 -m 1 '^error' "$LOG"
else
    tail -5 "$LOG"
fi