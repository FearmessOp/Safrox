// SPDX-License-Identifier: MPL-2.0
//! M13 acceptance: the first user-space driver.
//!
//! **The claim is what the kernel stopped doing.** Every log line
//! from M0 to M12 was written by the kernel's own 16550A driver;
//! this milestone moves that code into a component and the test has
//! to show it actually moved rather than merely being renamed.
//!
//! Seven criteria. The ones that could most easily prove nothing are
//! 4 and 5, and most of the commentary below is about how they
//! avoid that.
//!
//! 1. the root task builds and starts the serial driver;
//! 2. a **third** component sends it bytes and they reach the wire,
//!    by a path the kernel never touches;
//! 3. IRQ 4 is claimed, bound to a Notification and acknowledged;
//! 4. the kernel drives **zero** peripheral devices, and the ones it
//!    still touches are named and counted;
//! 5. the driver's port access is **bounded**: 0x3F8 yes, 0x70 no;
//! 6. the previous 59 scenarios still pass;
//! 7. exit 33.
//!
//! # Why criterion 4 is not a grep
//!
//! "No driver code in the kernel" is unfalsifiable by inspection:
//! the code could be renamed, inlined, or moved to a module nobody
//! greps for. What is checkable is *behaviour* — the number of
//! configuration writes the kernel makes to the UART while a
//! component owns it, which is a counter that must not move.
//!
//! # Why criterion 5 is not "the driver failed"
//!
//! A driver with no port capability at all would also fail to reach
//! 0x70, and would prove nothing about ranges. So the driver holds a
//! live IOPort with full rights, and the error has to be
//! `PortNotInRange` specifically — not `EmptySlot`, not
//! `InsufficientRights`. The hostile scenario tests the other claim
//! separately, which is why there are two of them.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use safrox_abi as abi;

use crate::cap::ioport;
use crate::emergency;
use crate::println;
use crate::roottask;
use crate::sched;
use crate::syscall;

// ---- Channels ----------------------------------------------------------
//
// Bit 31 is the driver's own; the root task reports on bit 28 as it
// has since M12, and `roottest::record_step` already handles that.

const DRIVER_CHANNEL: u64 = 1 << 31;

const STEP_COUNT: usize = 64;

/// Steps the **driver** reports, on bit 31.
const DRV_READY: u64 = 1;
const DRV_WROTE: u64 = 2;
const DRV_IRQ: u64 = 3;
const DRV_BYTE: u64 = 4;
const DRV_DENIED: u64 = 5;
const DRV_ERROR: u64 = 6;
/// The driver's panic handler ran.
const DRV_PANIC: u64 = 0xFF;

const NOT_REPORTED: u64 = u64::MAX;

static STEPS: [AtomicU64; STEP_COUNT] = [const { AtomicU64::new(NOT_REPORTED) }; STEP_COUNT];
static DRIVER_PANICKED: AtomicBool = AtomicBool::new(false);

/// Record a driver step, from the dispatcher's `Echo` path.
///
/// Errors are sticky, for the reason M11 recorded and M12 repeated:
/// several invocations share a step and "last write wins" lets a
/// failure be hidden by whatever follows it.
pub fn record_step(channel: u64, value: u64) -> bool {
    if channel & DRIVER_CHANNEL == 0 {
        return false;
    }

    let index = (channel & 0xFF) as usize;
    if index == DRV_PANIC as usize {
        DRIVER_PANICKED.store(true, Ordering::Release);
        return true;
    }
    if index < STEP_COUNT {
        let previous = STEPS[index].load(Ordering::Acquire);
        if previous == NOT_REPORTED || !syscall::is_error(previous) {
            STEPS[index].store(value, Ordering::Release);
        }
    }
    true
}

fn step(which: u64) -> u64 {
    STEPS[which as usize].load(Ordering::Acquire)
}

struct StepDump;

impl core::fmt::Debug for StepDump {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (index, slot) in STEPS.iter().enumerate() {
            let value = slot.load(Ordering::Acquire);
            if value != NOT_REPORTED {
                write!(f, " {}={:#x}", index, value)?;
            }
        }
        Ok(())
    }
}

fn read_tsc() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: RDTSC is unprivileged, reads no memory, writes EDX:EAX.
    unsafe {
        core::arch::asm!("rdtsc", out("eax") low, out("edx") high,
                         options(nomem, nostack, preserves_flags));
    }
    ((high as u64) << 32) | (low as u64)
}

const DEADLINE: u64 = 30_000_000_000;

/// Wait until a step is reported, or the deadline passes.
///
/// Waiting for an **event** rather than for a duration (ADR 023,
/// K8): `Resume` schedules a thread rather than running it, so
/// asserting immediately would blame the kernel for a race the test
/// created.
fn wait_for(which: u64) -> bool {
    let deadline = read_tsc() + DEADLINE;
    while step(which) == NOT_REPORTED && read_tsc() < deadline {
        sched::yield_now();
    }
    step(which) != NOT_REPORTED
}

// ---- Criterion 4: what the kernel still touches ------------------------

/// Peripheral devices the kernel drives.
///
/// **Zero, and the list below is why that number means something.**
///
/// A peripheral is a device the kernel would have to *own* — know
/// the registers of, configure, field interrupts from, arbitrate
/// between clients for. The four entries that remain are not that,
/// and each is here with its reason:
///
/// * **LAPIC** — the scheduler's time source and every interrupt's
///   EOI. Part of the CPU rather than a peripheral; handing it to a
///   component would let that component stop the scheduler.
/// * **I/O APIC** — interrupt routing, and what physically enforces
///   ADR 042's "no second delivery before Ack". A component holding
///   it could redirect another component's interrupts.
/// * **8259 PIC** — two `outb`, once, at boot, to mask it entirely.
///   Silenced rather than driven.
/// * **isa-debug-exit** — one `outl`, and it does not exist on real
///   hardware. The test harness's only way out of QEMU (ADR 002).
///
/// COM1 is counted separately below, because it is the one this
/// milestone is about.
const KERNEL_HARDWARE: &[(&str, &str)] = &[
    ("LAPIC", "timer + EOI; part of the CPU, not a peripheral"),
    ("I/O APIC", "interrupt routing and masking (ADR 042)"),
    ("8259 PIC", "masked once at boot; silenced, not driven"),
    (
        "isa-debug-exit",
        "test harness only; absent on real hardware",
    ),
];

/// Report what the kernel still touches, and check the claim.
///
/// `config_at_start` is the configuration-write count taken before
/// the driver was given the device. The claim is that it **does not
/// move** while a component owns the UART.
fn report_hardware(config_at_start: u64) {
    println!("[driver] kernel still drives 0 peripheral devices");
    for (name, why) in KERNEL_HARDWARE {
        println!("[driver] kernel hardware: {} -- {}", name, why);
    }
    println!(
        "[driver] kernel hardware: COM1 -- emergency console only \
         (ADR 046): LSR and THR, no configuration while a driver owns it"
    );

    // **The falsifiable half of criterion 4.**
    //
    // Six writes at boot, before any component existed, and none
    // since. A kernel that re-programmed the baud rate, flipped
    // DLAB, or re-enabled an interrupt behind the driver's back
    // would move this counter — and such a kernel has not really
    // handed the device over, whatever the module layout says.
    let config_now = emergency::config_writes();
    assert_eq!(
        config_now,
        config_at_start,
        "the kernel made {} configuration write(s) to COM1 while a user \
         component owned it ({} before the driver started, {} now). \
         ADR 046 allows exactly two registers -- LSR and THR -- outside \
         boot and the panic path. A device whose settings the kernel may \
         change at any moment does not belong to the component that \
         configured it.",
        config_now - config_at_start,
        config_at_start,
        config_now
    );

    println!(
        "[driver] the kernel made {} configuration write(s) to COM1 at \
         boot and {} since the driver took over",
        config_at_start,
        config_now - config_at_start
    );
}

// ---- The positive scenario ---------------------------------------------

/// `driver-test`: the seven criteria.
pub fn run() -> ! {
    println!("[driver] the kernel has started the root task; the serial driver is a component");

    // Taken **now**, before the driver runs: everything after this
    // is what the kernel did while a component owned the device.
    let config_at_start = emergency::config_writes();
    assert!(
        config_at_start > 0,
        "the kernel configured COM1 zero times at boot. The counter is \
         not being incremented, so 'it does not move afterwards' would \
         be true of a counter that never moves -- a check \
         indistinguishable from its own absence (ADR 023, K5)."
    );

    // ---- 1. The driver was built and started -------------------------
    assert!(
        wait_for(DRV_READY),
        "the serial driver never reported itself ready. It is built by \
         the root task out of the root task's own memory, so a failure \
         here is a failure in a ring 3 program -- check the root task's \
         steps. Driver steps: {:?}",
        StepDump
    );
    assert!(
        !DRIVER_PANICKED.load(Ordering::Acquire),
        "the serial driver's panic handler ran. It cannot print -- \
         printing is what it provides -- so this is all the detail \
         there is. Steps: {:?}",
        StepDump
    );

    let error = step(DRV_ERROR);
    assert_eq!(
        error,
        NOT_REPORTED,
        "the driver reported error {:#018x} ({}) while configuring the \
         UART or arranging its interrupt",
        error,
        syscall::error_code(error)
    );

    println!(
        "[driver] the serial driver is running at CPL 3; it configured \
         COM1 through an IOPort capability and the kernel executed every \
         `out` on its behalf (ADR 045)"
    );

    // ---- 5. Its port access is bounded --------------------------------
    //
    // **Checked before the write path**, because this is the
    // criterion most likely to be satisfied vacuously and the first
    // thing to establish is that the refusal has the right cause.
    let denied = step(DRV_DENIED);
    assert_ne!(
        denied, NOT_REPORTED,
        "the driver never reported its attempt on the CMOS port, so \
         criterion 5 has nothing to say"
    );
    assert!(
        syscall::is_error(denied),
        "the driver wrote to the CMOS index register at 0x70 and the \
         kernel allowed it. It holds an IOPort for 0x3F8..=0x3FF; the \
         range check in `cap::ioport::out8` is what keeps that from \
         meaning 'every port on the machine'.",
    );

    let code = syscall::error_code(denied);
    assert_eq!(
        code,
        abi::error::PORT_NOT_IN_RANGE,
        "the driver's write to 0x70 failed with error {} where \
         PortNotInRange ({}) was expected.\n\
         \n\
         The distinction is the whole of criterion 5. A driver holding \
         no IOPort at all would also fail -- with EmptySlot -- and that \
         would prove nothing about ranges, only that an absent \
         capability is absent. This driver holds a live IOPort with \
         full rights; what it does not hold is one covering this port.",
        code,
        abi::error::PORT_NOT_IN_RANGE
    );

    println!(
        "[driver] the driver holds a live IOPort with full rights and was \
         refused 0x70 with PortNotInRange: the bound is the capability's \
         range, not the absence of one"
    );

    // ---- 2. A third component wrote to the wire -----------------------
    //
    // **The milestone.** The client holds one capability — a badged
    // Endpoint — and no way to reach hardware. Its bytes reached the
    // UART because the driver put them there.
    assert!(
        wait_for(DRV_WROTE),
        "the driver never reported writing a byte. The client sends \
         through an Endpoint and blocks until the driver receives \
         (ADR 033), so either the client never ran or the driver never \
         got as far as its loop. Steps: {:?}",
        StepDump
    );

    // Wait for all five, not just the first: one byte could be a
    // fluke of initialisation, five is a conversation.
    let deadline = read_tsc() + DEADLINE;
    while step(DRV_WROTE) < 5 && read_tsc() < deadline {
        sched::yield_now();
    }

    let written = step(DRV_WROTE);
    assert!(
        written >= 5,
        "the driver wrote {} byte(s); the client sends 5. A short count \
         means the rendezvous stopped partway, which `Send` blocking \
         should make impossible unless the driver stopped receiving.",
        written
    );

    println!(
        "[driver] a third component sent {} bytes through the driver's \
         Endpoint and they reached COM1. The client holds no IOPort, the \
         kernel never looked at the data, and the only code that touched \
         the device was a ring 3 program.",
        written
    );

    // ---- 3. The interrupt path ---------------------------------------
    //
    // **Reported rather than asserted**, and the reason is K8.
    //
    // IRQ 4 fires when a byte *arrives* on the serial line. Nothing
    // sends one: `make test` runs QEMU with no input, so the line
    // stays quiet and the interrupt legitimately never comes.
    //
    // An assertion here would therefore be a test of whether
    // somebody typed into the console, which is not a property of
    // this kernel. What *is* checkable is that the line was claimed,
    // bound and left in the right state — which the kernel can see
    // from its own side, and which is asserted below.
    let interrupts = step(DRV_IRQ);
    if interrupts != NOT_REPORTED {
        println!(
            "[driver] the driver was woken by IRQ 4 {} time(s) and read \
             the byte with IOPort.In8",
            interrupts
        );
        let byte = step(DRV_BYTE);
        if byte != NOT_REPORTED {
            println!("[driver] it received {:#04x} from the device", byte);
        }
    } else {
        println!(
            "[driver] no byte arrived on COM1, so IRQ 4 never fired -- \
             which is correct for a run with no serial input, and is \
             why this is reported rather than asserted (ADR 023, K8)"
        );
    }

    // What the kernel can check without anybody typing: the line has
    // an owner, and that owner is not the kernel.
    assert!(
        crate::cap::irq::is_owned(4),
        "IRQ 4 has no owner. The driver claimed it with IRQControl.Get \
         during startup and reported success, so the ownership table \
         and the capability disagree."
    );

    println!(
        "[driver] IRQ 4 is owned by a user component; the kernel's \
         interrupt path does four things and stops (ADR 042)"
    );

    // ---- 4. The kernel drives nothing ---------------------------------
    report_hardware(config_at_start);

    // And the port layer's own view.
    ioport::report();
    emergency::report();

    println!("[driver] all driver tests passed");

    syscall::report();

    println!("[boot] M13 checks passed");
    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        crate::qemu::EXIT_SUCCESS,
        crate::qemu::HOST_STATUS_SUCCESS
    );

    // ---- 7. Exit 33 ---------------------------------------------------
    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

// ---- Negative (a): the driver dies -------------------------------------

/// `driver-dies`: the driver's capabilities are revoked while it runs.
///
/// Expected: **the system stays up**, IRQ 4 is released and masked,
/// and the client gets a clean error. Exit 33.
///
/// # Why this is the milestone's real output
///
/// "A driver crash does not take the system down" is the claim every
/// microkernel makes and the one that is hardest to believe without
/// seeing it. In a monolithic kernel this scenario is not a test; it
/// is a panic.
///
/// What makes it true here is nothing the driver does. Its authority
/// is three capabilities, and when they stop existing the authority
/// does too — the kernel does not have to notice, clean up, or be
/// told. `release_range` and `release_line` run from `delete`'s own
/// path (ADR 041, ADR 045), so forgetting them is not possible in
/// the way forgetting a cleanup call usually is.
pub fn run_dies() -> ! {
    println!("[driver] the driver will be revoked while it runs");

    let config_at_start = emergency::config_writes();

    assert!(
        wait_for(DRV_READY),
        "the driver never started, so killing it would prove nothing. \
         Steps: {:?}",
        StepDump
    );

    println!("[driver] the driver is running; the root task will now revoke it");

    // The root task does the killing — it holds the capabilities, so
    // it is the only thing that can. The kernel is a bystander,
    // which is the point: a kernel that had to participate would be
    // a kernel that knew what a driver was.
    let deadline = read_tsc() + DEADLINE;
    while crate::cap::irq::is_owned(4) && read_tsc() < deadline {
        sched::yield_now();
    }

    // **IRQ 4 has no owner and is masked.**
    //
    // Two separate claims and both matter. An ownerless line that
    // stayed open would deliver interrupts nobody handles — a storm
    // that accomplishes nothing and costs a dispatch each time. The
    // mask is what makes "the driver is gone" quiet rather than
    // noisy.
    assert!(
        !crate::cap::irq::is_owned(4),
        "IRQ 4 still has an owner after the driver's IRQHandler was \
         deleted. `release_line` runs from `release_ipc_references`, so \
         the ownership table should have been cleared on the way out."
    );
    assert!(
        crate::cap::irq::is_masked(4),
        "IRQ 4 is ownerless but still unmasked. Every interrupt on it \
         now reaches `handle_interrupt`, finds no owner, and is counted \
         and dropped -- a storm with no destination (ADR 041)."
    );

    // **The port range came back.** This is what makes a restart
    // possible, and it is checked here rather than only in
    // `driver-restart` so that the two claims stay separable: this
    // one is "the authority was released", that one is "a
    // replacement can use it".
    assert!(
        !ioport::overlaps(ioport::COM1_FIRST, ioport::COM1_LAST),
        "the port range {:#06x}..={:#06x} is still marked as issued \
         after the driver's IOPort was deleted. `release_range` runs \
         from the same hook `release_line` does; without it the window \
         is owned by an object that no longer exists and no replacement \
         driver could ever be issued one.",
        ioport::COM1_FIRST,
        ioport::COM1_LAST
    );

    println!(
        "[driver] the driver is gone: IRQ 4 is ownerless and masked, and \
         ports {:#06x}..={:#06x} are free to be issued again",
        ioport::COM1_FIRST,
        ioport::COM1_LAST
    );

    // **The kernel is still running**, which is the whole claim.
    // Reaching this line is most of it; the checks make it specific.
    let ops_now = roottask::privileged_ops();
    assert_eq!(
        ops_now,
        roottask::ops_at_seal(),
        "the kernel performed {} privileged operations while its driver \
         died. A kernel that steps in when a component fails has not \
         handed the device over.",
        ops_now - roottask::ops_at_seal()
    );

    report_hardware(config_at_start);

    println!(
        "[driver] the serial driver died and the system did not. This is \
         the claim the architecture exists to make, and it is true for a \
         structural reason rather than a careful one: the driver's \
         authority was three capabilities, and when they stopped \
         existing so did it."
    );

    syscall::report();

    println!("[boot] M13 driver-dies checks passed");
    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

// ---- Negative (b): a replacement driver --------------------------------

/// `driver-restart`: a second driver is built after the first dies.
///
/// Expected: it claims the same hardware and works. Exit 33.
///
/// # Crash-only, demonstrated rather than claimed
///
/// Nothing is repaired here. The first driver's objects are gone —
/// its TCB, its CSpace, its IOPort, its IRQHandler — and the second
/// is built from scratch out of the root task's memory. The hardware
/// does not know a driver changed, and neither does the client: it
/// holds an Endpoint capability, and the Endpoint outlived both
/// drivers because it belongs to the root task.
///
/// That last part is the design showing through. A client bound to
/// its *server* would have to be told; a client bound to a
/// *rendezvous point* does not care who is on the other side.
pub fn run_restart() -> ! {
    println!("[driver] a driver will be killed and replaced");

    let config_at_start = emergency::config_writes();

    assert!(
        wait_for(DRV_READY),
        "the first driver never started. Steps: {:?}",
        StepDump
    );

    // The first driver reported ready; the root task kills it and
    // builds another. Both report through the same channel, so what
    // this waits for is the *second* one writing bytes — which only
    // a working replacement can do.
    //
    // **All five, not the first.** Waiting for one byte was enough
    // to prove the replacement works, and it produced a log that
    // read `[M[driver] a replacement driver claimed...`: the client
    // was still sending while the kernel printed, and the two
    // writers share a wire.
    //
    // That interleaving is not a defect in either of them. The
    // driver owns COM1 and the kernel's emergency console may still
    // write to it (ADR 046) -- which is the whole point of having
    // one -- so bytes from the two can meet. What was wrong was the
    // test proceeding while the thing it describes was still
    // happening.
    //
    // Waiting for the whole conversation is also the better claim:
    // a replacement that received one byte and stopped would have
    // passed the old check.
    //
    // **Two waits, and the first is not redundant.** `NOT_REPORTED`
    // is `u64::MAX`, so a count-based loop spinning on
    // `step(..) < 5` falls straight through while the step is
    // *absent* -- the sentinel is larger than any real count. The
    // first version of this had only the second loop and the
    // assertion below fired before the root task had even revoked
    // the first driver.
    //
    // The sentinel sits at the far end of the range it shares with
    // the values it is distinguished from, which is a sharp edge;
    // it is kept because `Option` cannot live in an `AtomicU64`,
    // and `wait_for` exists precisely so that "has it been
    // reported" is asked with a name rather than a comparison.
    assert!(
        wait_for(DRV_WROTE),
        "the replacement driver never reported writing anything. \
         Steps: {:?}",
        StepDump
    );

    let deadline = read_tsc() + DEADLINE;
    while step(DRV_WROTE) < 5 && read_tsc() < deadline {
        sched::yield_now();
    }

    let written = step(DRV_WROTE);
    assert_ne!(
        written, NOT_REPORTED,
        "the replacement driver never wrote a byte. The client's \
         Endpoint capability survived the first driver -- it names the \
         root task's Endpoint object, not the driver -- so a working \
         replacement should have received on it. Steps: {:?}",
        StepDump
    );
    assert!(
        !syscall::is_error(written),
        "the replacement reported error {}",
        syscall::error_code(written)
    );
    assert!(
        written >= 5,
        "the replacement driver wrote {} of the client's 5 bytes. The \
         client blocks in `Send` until the driver receives (ADR 033), so \
         a short count means the replacement stopped serving partway.",
        written
    );

    // The hardware is owned again, by the new driver.
    assert!(
        crate::cap::irq::is_owned(4),
        "IRQ 4 has no owner after the restart. The replacement called \
         IRQControl.Get and should hold it."
    );
    assert!(
        ioport::overlaps(ioport::COM1_FIRST, ioport::COM1_LAST),
        "the COM1 port range is not issued after the restart, yet the \
         replacement is writing bytes. The range table and the \
         capability disagree."
    );

    println!(
        "[driver] a replacement driver claimed IRQ 4 and ports \
         {:#06x}..={:#06x} and is serving the same client, which never \
         learned anything had happened",
        ioport::COM1_FIRST,
        ioport::COM1_LAST
    );

    report_hardware(config_at_start);

    println!(
        "[driver] crash-only recovery: nothing was repaired. The first \
         driver's objects are gone and these are new ones, built from \
         the root task's memory by a ring 3 program."
    );

    syscall::report();

    println!("[boot] M13 driver-restart checks passed");
    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

// ---- Negative (c): a component with no authority -----------------------

/// `driver-hostile`: a component tries to take the hardware.
///
/// Expected: every attempt refused. Exit 33.
///
/// # What this tests that criterion 5 does not
///
/// Criterion 5 asks whether a capability's *range* binds its holder.
/// This asks whether a component with **no capability at all** can
/// reach the hardware anyway — by naming a slot it was never given,
/// by claiming an interrupt line, by issuing itself a port range.
///
/// The answers come back as `EmptySlot` rather than as a refusal
/// from deep inside the kernel, and that is the interesting part: a
/// component cannot *name* what it does not hold (ADR 018). The
/// isolation is structural rather than enforced, which is a stronger
/// property and a cheaper one.
pub fn run_hostile() -> ! {
    println!("[driver] a component with no hardware authority will try to take some");

    let config_at_start = emergency::config_writes();

    // Three attempts, reported on three consecutive steps by the
    // hostile component itself.
    const FIRST: u64 = 30;
    let names = [
        "write to COM1's data register",
        "claim IRQ 4",
        "issue itself the COM1 port range",
    ];

    for (offset, name) in names.iter().enumerate() {
        let which = FIRST + offset as u64;
        assert!(
            wait_for(which),
            "the hostile component never reported its attempt to {}. \
             Steps: {:?}",
            name,
            StepDump
        );

        let result = step(which);
        assert!(
            syscall::is_error(result),
            "a component holding no IOPort, no IRQHandler and no control \
             capability succeeded in: {}. It returned {:#018x}.",
            name,
            result
        );

        // **And refused for the right reason.**
        //
        // `is_error` alone would pass for any failure at all --
        // including a kernel that reached the device, got confused
        // and returned something. `EmptySlot` is the specific claim:
        // the invocation stopped at CPtr resolution, before any
        // method ran, because slot 2 of this component's CSpace
        // holds nothing. The hardware was never approached.
        let code = syscall::error_code(result);
        assert_eq!(
            code,
            abi::error::EMPTY_SLOT,
            "the attempt to {} was refused with error {} where EmptySlot \
             ({}) was expected.\n\
             \n\
             The difference is what this scenario is for. A refusal from \
             inside a method would mean the kernel looked at the request \
             and declined it; EmptySlot means there was nothing to look \
             at. A component cannot name what it does not hold (ADR 018), \
             so isolation here is structural rather than enforced -- and \
             a structural property is one that cannot be forgotten in a \
             later method.",
            name,
            code,
            abi::error::EMPTY_SLOT
        );

        println!("[driver] refused: {} -> error {}", name, code);
    }

    // **The attempts changed nothing**, which is a separate claim
    // from their having failed.
    //
    // This scenario starts no driver -- the hostile component is the
    // only thing running, which is what makes "it holds nothing" a
    // clean statement. So the check is not "the driver kept its
    // hardware" but the stronger and simpler one: **the hardware has
    // no owner at all**, and the component that asked for it twice
    // is not that owner.
    //
    // An earlier version asserted `is_owned(4)` here, copied from
    // the positive scenario without noticing there was no driver to
    // own it. It failed, correctly, and for a reason that had
    // nothing to do with the kernel: the test asserted a
    // precondition the scenario deliberately does not create.
    assert!(
        !crate::cap::irq::is_owned(4),
        "IRQ 4 has an owner after a component with no IRQControl \
         capability asked for it. No driver runs in this scenario, so \
         the only thing that could have claimed the line is the \
         component that was refused -- which means the refusal was \
         reported but not performed."
    );
    assert!(
        !ioport::overlaps(ioport::COM1_FIRST, ioport::COM1_LAST),
        "ports {:#06x}..={:#06x} are marked as issued after a component \
         with no IOPortControl capability asked for them. The range \
         table is what `Issue` consults, so an entry here means the \
         invocation ran despite the component holding nothing to run it \
         on.",
        ioport::COM1_FIRST,
        ioport::COM1_LAST
    );

    println!(
        "[driver] after three attempts IRQ 4 is still ownerless and \
         ports {:#06x}..={:#06x} are still unissued: the component did \
         not merely fail, it changed nothing",
        ioport::COM1_FIRST,
        ioport::COM1_LAST
    );

    report_hardware(config_at_start);

    println!(
        "[driver] every attempt was refused, and refused for the right \
         reason: the component could not name a capability it was never \
         given. A CPtr is meaningful only relative to a root (ADR 018), \
         so what it does not hold is unnameable rather than forbidden."
    );

    syscall::report();

    println!("[boot] M13 driver-hostile checks passed");
    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}
