#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
#
# Wire M9c's four scenarios into the Makefile and the Limine config.
#
# A script rather than hand edits, and the reason is ADR 023's K7:
# M9a destroyed nine Makefile recipe tabs with search-and-replace,
# because a tab and eight spaces look identical and `make` accepts
# only one of them. Here every tab is written by this file as a
# literal \t through printf, so there is nothing to mistype.
#
# Idempotent: it checks for its own markers and does nothing if the
# edits are already present. Run from the repository root.
set -eu

cd "$(dirname "$0")/.."

if grep -q 'ISO_IPCCAP' Makefile; then
    echo "m9c-wire: Makefile already has the M9c scenarios; nothing to do"
else
    python3 - <<'PYTHON'
import re

TAB = "\t"
makefile = open("Makefile", encoding="utf-8").read()

# 1. The ISO variables, beside the M9b ones.
makefile = makefile.replace(
    "ISO_IPCCOST := $(BUILD_DIR)/safrox-ipccost.iso\n",
    "ISO_IPCCOST := $(BUILD_DIR)/safrox-ipccost.iso\n"
    "ISO_IPCCAP := $(BUILD_DIR)/safrox-ipccap.iso\n"
    "ISO_IPCCAPHOSTILE := $(BUILD_DIR)/safrox-ipccaphostile.iso\n"
    "ISO_IPCNTFNREV := $(BUILD_DIR)/safrox-ipcntfnrev.iso\n"
    "ISO_IPCCAPCOST := $(BUILD_DIR)/safrox-ipccapcost.iso\n",
    1,
)

# 2. The all-ISOs list.
makefile = makefile.replace(
    "\t\t$(ISO_IPCSELF) $(ISO_IPCCOST)\n",
    "\t\t$(ISO_IPCSELF) $(ISO_IPCCOST) \\\n"
    "\t\t$(ISO_IPCCAP) $(ISO_IPCCAPHOSTILE) $(ISO_IPCNTFNREV) \\\n"
    "\t\t$(ISO_IPCCAPCOST)\n",
    1,
)

# 3. The .PHONY list and the `test` target's dependencies. Both read
#    "run-ipccost \" today; the first keeps its backslash, the second
#    ends the list.
makefile = makefile.replace(
    "        run-ipccost \\\n",
    "        run-ipccost run-ipccap run-ipccaphostile \\\n"
    "        run-ipcntfnrev run-ipccapcost \\\n",
    1,
)
makefile = makefile.replace(
    "        run-ipccost\n",
    "        run-ipccost run-ipccap run-ipccaphostile \\\n"
    "        run-ipcntfnrev run-ipccapcost\n",
    1,
)

# 4. The ISO build rules. Entry indices continue from M9b's 41.
iso_rules = ""
for name, index in (
    ("ISO_IPCCAP", 42),
    ("ISO_IPCCAPHOSTILE", 43),
    ("ISO_IPCNTFNREV", 44),
    ("ISO_IPCCAPCOST", 45),
):
    iso_rules += (
        f"\n$({name}): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)\n"
        f"{TAB}$(call build_iso,$@,{index})\n"
    )

anchor = (
    "$(ISO_IPCCOST): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)\n"
    f"{TAB}$(call build_iso,$@,41)\n"
)
assert anchor in makefile, "the ISO_IPCCOST rule is not where expected"
makefile = makefile.replace(anchor, anchor + iso_rules, 1)

# 5. The run targets.
run_rules = "\n# M9c scenarios: capability transfer and notifications.\n"
for target, iso, expect in (
    ("run-ipccap", "ISO_IPCCAP", "ipccap"),
    ("run-ipccaphostile", "ISO_IPCCAPHOSTILE", "ipccaphostile"),
    ("run-ipcntfnrev", "ISO_IPCNTFNREV", "ipcntfnrev"),
    ("run-ipccapcost", "ISO_IPCCAPCOST", "ipccapcost"),
):
    run_rules += (
        f"\n{target}: $({iso})\n"
        f"{TAB}@$(PYTHON) scripts/run.py --qemu \"$(QEMU)\" --iso $({iso}) "
        f"--expect {expect} -- $(QEMU_FLAGS)\n"
    )

anchor = (
    "run-ipccost: $(ISO_IPCCOST)\n"
    f"{TAB}@$(PYTHON) scripts/run.py --qemu \"$(QEMU)\" --iso $(ISO_IPCCOST) "
    "--expect ipccost -- $(QEMU_FLAGS)\n"
)
assert anchor in makefile, "the run-ipccost target is not where expected"
makefile = makefile.replace(anchor, anchor + run_rules, 1)

open("Makefile", "w", encoding="utf-8", newline="\n").write(makefile)
print("m9c-wire: Makefile updated")
PYTHON
fi

if grep -q 'ipc-cap-test' boot/limine.conf.in; then
    echo "m9c-wire: limine.conf.in already has the M9c entries"
else
    cat >> boot/limine.conf.in <<'EOF'

/+Safrox (M9c: capability transfer)
    comment: ADR 035's authority half. Four capabilities at most, into
    comment: slots the RECEIVER opened -- a sender cannot name a
    comment: destination in another CSpace, so there is nothing for it
    comment: to force. Six transfer scenarios and three notification
    comment: ones, including the starvation test that is the whole
    comment: reason ADR 038's alternation bit exists. Exit 33.
    protocol: limine
    path: boot():/boot/kernel.elf
    cmdline: ipc-cap-test

/+Safrox (M9c: hostile capability transfers)
    comment: Malformed transfers: reserved CPtr bits, a leaf index past
    comment: its CNode, a root index past the root, and a well-formed
    comment: capability without GRANT. Each is dropped silently (ADR
    comment: 035 refuses to tell either side, because telling the
    comment: sender would let it scan the receiver's CSpace) and every
    comment: message still arrives. The kernel stays up: these are
    comment: caller errors, and from M13 the caller is a component.
    comment: Exit 33.
    protocol: limine
    path: boot():/boot/kernel.elf
    cmdline: ipc-cap-hostile

/+Safrox (M9c: revoked notification)
    comment: A thread blocked in Wait when the Notification's
    comment: capability is deleted. It wakes with EndpointDead rather
    comment: than waiting forever -- ADR 039's reasoning, which is not
    comment: ADR 026's: the waiter may be another component's thread,
    comment: so refusing the revoke would make the object immortal.
    comment: Without the Notification arm in release_ipc_references
    comment: this does not fail, it hangs. Exit 33.
    protocol: limine
    path: boot():/boot/kernel.elf
    cmdline: ipc-notification-revoked

/+Safrox (M9c: capability transfer cost)
    comment: What a capability costs on the IPC path: Send+Recv with 0,
    comment: 1 and 4 of them. ADR 035 chose four on the grounds that
    comment: each is bounded work -- a CPtr resolution, a rights check,
    comment: a slot check and a derivation insertion -- and this is the
    comment: figure behind that claim. Three points rather than two, so
    comment: the fourth capability can be compared against the first.
    comment: Exit 33.
    protocol: limine
    path: boot():/boot/kernel.elf
    cmdline: ipc-cap-cost
EOF
    echo "m9c-wire: limine.conf.in updated"
fi