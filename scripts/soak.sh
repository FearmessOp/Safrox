#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# Run a target repeatedly and keep the first failing serial log.
#
# Written in M14, for a failure that happened once in four suite runs
# and never in eight isolated ones. ADR 023's K8 says a flaky failure
# is not closed by a single green run, and this is the cheapest way to
# obey that: a loop that stops on the first red and *keeps the
# evidence*.
#
# The evidence part is the whole point. Every scenario writes its
# serial output to a fixed path, so the next run overwrites it -- and
# the first attempt to diagnose this bug read a log from a later,
# passing run and concluded the failure was not reproducible. It was;
# the log had been replaced.
#
# Usage:
#   scripts/soak.sh <iterations> <make-target> [more targets...]
#
# Exits 0 if every iteration passed, 1 on the first failure.

set -u

iterations="${1:?usage: soak.sh <iterations> <make-target>...}"
shift
targets=("$@")

if [ "${#targets[@]}" -eq 0 ]; then
    echo "soak: no make target given" >&2
    exit 2
fi

keep="build/soak"
mkdir -p "$keep"
rm -f "$keep"/*.log 2>/dev/null || true

for i in $(seq 1 "$iterations"); do
    if make "${targets[@]}" > "$keep/last-run.log" 2>&1; then
        echo "soak: iteration $i/$iterations ok"
        continue
    fi

    echo "soak: iteration $i/$iterations FAILED"

    # The runner's own summary, and every scenario's serial log as it
    # stood at the moment of failure.
    cp build/safrox-*.log "$keep/" 2>/dev/null || true

    echo "soak: kept $(ls -1 "$keep" | wc -l) file(s) in $keep"
    grep -a 'FAIL]' -A 10 "$keep/last-run.log" | head -30
    exit 1
done

echo "soak: $iterations/$iterations iterations passed"