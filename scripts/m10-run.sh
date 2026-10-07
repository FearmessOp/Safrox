#!/bin/bash
# SPDX-License-Identifier: BSD-2-Clause
#
# Run one or more make targets with cargo on PATH.
#
# `wsl bash -c make ...` fails with "cargo: No such file or directory"
# because a non-login shell does not read ~/.profile, and make inherits
# that PATH. Sourcing cargo's env here fixes it for the whole subtree.
#
# Each target's output goes to its own file under /tmp and only the
# verdict plus the interesting lines come back, because terminal output
# is truncated on the way to the caller.

set -u

cd /mnt/c/Users/user/pyth || exit 1

if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

overall=0

for target in "$@"; do
    log="/tmp/m10-$target.log"
    make "$target" > "$log" 2>&1
    status=$?

    if [ "$status" -eq 0 ]; then
        echo "=== $target: PASS"
    else
        echo "=== $target: FAIL (exit $status)"
        overall=1
    fi

    # The scenario's own lines, whatever the outcome: on a pass they
    # are the evidence, and on a failure they are the diagnosis.
    grep -E '^\[(irq|PANIC|trap)\]|^\*\*\*|^\[FAIL\]|^ +- ' "$log" | head -45
    echo
done

exit $overall