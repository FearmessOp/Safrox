#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# M10 commit. A script rather than `git commit -m "..."` because the
# message is long and multi-paragraph, and quoting it through
# `wsl bash -c` from a Windows shell mangles it.

set -eu

cd /mnt/c/Users/user/pyth

MSG=$(mktemp)
cat > "$MSG" <<'COMMIT_MESSAGE'
M10: user-space interrupt handling

A hardware interrupt now reaches a thread at CPL 3. Everything before
this was the kernel talking to itself; a driver needs the other
direction.

    [irq] ring 3: a thread at cpl 3 blocked in Wait, a hardware
    interrupt on vector 38 arrived, and the thread resumed with badge
    0x00a50000

Two ADRs, written before the code:

* 041 -- IRQControl derives IRQHandler(n), one owner per line. The
  deciding argument is not "who gets the interrupt" or "who Acks it",
  both awkward, but that a second listener can watch the first's
  device activity as a side channel. Sharing is a user-space
  multiplexer's job (M12).
* 042 -- the interrupt path is four steps and there is no fifth: find
  the owner, mask the line, signal the Notification, EOI. Masking
  comes BEFORE signalling, because a signal makes the handler thread
  runnable and the device could otherwise raise a second interrupt
  before the first was started on -- and a signal is a mask rather
  than a count (ADR 038), so "one" and "two" would be
  indistinguishable.

Ack is the driver's job: the kernel does not know where the device's
status register is, and if it did, device logic would be in the
kernel. A driver that never Acks leaves its line shut -- isolation
rather than deadlock. Recovery is revoking the handler, not a timeout
(which needs a scheduling budget ADR 017 excluded and a policy ADR 011
forbids).

Six scenarios and three negatives, all run:

    [irq] masked: an interrupt on the shut line was refused
          (while_masked 0 -> 1) and not delivered (delivered stayed at 1)
    [irq] ack: the identical stimulus was delivered once the line was
          reopened (1 -> 2 delivered)
    [irq] wake latency: 32 samples, minimum 10752 cycles

Two counters, both necessary: one figure cannot distinguish "the mask
worked" from "nothing was sent". The scenario waits for while_masked to
MOVE rather than sleeping and asserting nothing happened.

K5: the prescribed mutation (Ack as a no-op) died -- but the first
failure came from an is_masked assertion, which reads the same flag Ack
writes. That is close to asking the mutation whether it mutated itself.
Removing those two assertions and re-running showed the behavioural
check kills it alone ("after Ack the same interrupt was still not
delivered"). A variant mutation -- Ack working exactly once -- was
needed to reach scenario 4's second half, and confirmed the two halves
test different properties. Recorded in ADR 042 and the README, because
a mutation result about the test is worth as much as one about the
kernel.

Also found: Slot::is_empty() tested the object ADDRESS, which for nine
types was equivalent to testing the type. IRQControl is the first
capability that names no object -- a line is not memory -- so it read
as empty and irq-test panicked with EmptySlot on a capability that was
visibly there. ADR 017 had already said which definition was meant:
"Null is not a ninth type: it is the absence of a capability ... so
that a slot is always a valid CapType rather than needing a separate
occupied flag." Not a bug in new code; two definitions that coincided
for nine types and diverged on the tenth.

The handler records the Notification capability's SLOT address plus
generation (ADR 018's idiom) rather than the object's address, which
was the ADR's first design. Three consequences: the badge is read from
the capability each time (ADR 037), a deleted capability is detected
rather than requiring bookkeeping from delete, and the signal goes
through cap::ipc::signal unchanged -- the same function a thread calls,
so a device-raised notification and a thread-raised one are not merely
alike but identical.

Method::Wait is this kernel's first blocking invocation. It needed no
special handling: a syscall runs on the calling thread's own kernel
stack (ADR 029), so blocking saves the context there like any other
switch.

M9c's recorded debt is paid: release_ipc_references now has its
IrqHandler arm, which releases and masks the line.

make test: 51 scenarios, run twice, clean both times.
COMMIT_MESSAGE

git commit -F "$MSG"
rm -f "$MSG"

git --no-pager log --oneline -1