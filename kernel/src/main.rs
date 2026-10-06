// SPDX-License-Identifier: BSD-2-Clause
//! PythOS — M0 bootstrap.
//!
//! What this milestone establishes:
//!   1. Limine boots us into 64-bit long mode in the higher half.
//!   2. COM1 carries a log the host test runner can assert on.
//!   3. A panic reports its source location and terminates the VM.
//!   4. QEMU exits with a status the runner can decode (33 / 35).
//!
//! Deliberately absent: memory management, GDT/IDT, interrupts, user space.
//! Those belong to later milestones.

#![no_std]
#![no_main]

mod io;
mod panic;
mod qemu;
mod serial;

use limine::request::{
    BootloaderInfoRequest, ExecutableAddressRequest, ExecutableCmdlineRequest, HhdmRequest,
    StackSizeRequest,
};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

/// Name reported on the serial console.
const KERNEL_NAME: &str = "PythOS";

/// Version reported on the serial console. Kept in step with Cargo.toml.
const KERNEL_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "-M0");

/// Lowest address of the higher half under the -2 GiB kernel code model.
///
/// With 4-level paging the canonical address space has a hole in the middle;
/// the top 2 GiB is the only region reachable from 32-bit sign-extended
/// displacements, which is what `-C code-model=kernel` assumes.
///
/// Reference: Intel SDM Vol. 1, section 3.3.7.1 "Canonical Addressing";
/// System V AMD64 ABI, section 3.5.1 (the "kernel" code model).
const HIGHER_HALF_BASE: u64 = 0xffff_ffff_8000_0000;

/// Stack Limine should hand us, in bytes.
///
/// The protocol's default is small and the formatting machinery in
/// `core::fmt` is not frugal with stack. 64 KiB leaves room without
/// allocating anything -- Limine carves this out of its own memory before
/// handing control over, so the kernel still allocates nothing itself.
const STACK_SIZE: u64 = 64 * 1024;

// ---- Limine requests ---------------------------------------------------
//
// Every item below must survive to the final binary with its address
// recorded in `.limine_requests`, because nothing in the kernel references
// them -- the bootloader finds them by scanning that section before we ever
// run. Two attributes make that work:
//
//   #[used]           stops rustc from discarding the static as dead code
//   #[link_section]   places it where the linker script KEEP()s it
//
// Without both, the requests vanish and every `response()` returns None at
// runtime: a silent boot with an empty serial console.

/// Negotiates the boot protocol revision.
///
/// Revision 3 is what this kernel is written against. The crate's
/// `BaseRevision::new()` would request its own maximum (6), which would make
/// the kernel's expectations drift with crate upgrades, so we pin it.
#[used]
#[link_section = ".limine_requests"]
static BASE_REVISION: BaseRevision = BaseRevision::with_revision(3);

#[used]
#[link_section = ".limine_requests"]
static BOOTLOADER_INFO: BootloaderInfoRequest = BootloaderInfoRequest::new();

#[used]
#[link_section = ".limine_requests"]
static EXECUTABLE_ADDRESS: ExecutableAddressRequest = ExecutableAddressRequest::new();

#[used]
#[link_section = ".limine_requests"]
static HHDM: HhdmRequest = HhdmRequest::new();

#[used]
#[link_section = ".limine_requests"]
static CMDLINE: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();

#[used]
#[link_section = ".limine_requests"]
static STACK: StackSizeRequest = StackSizeRequest::new(STACK_SIZE);

// The markers bracket the request section so the bootloader can scan a
// bounded range instead of the whole image. They must come first and last in
// their own sections; the linker script orders the three sections.
#[used]
#[link_section = ".limine_requests_start"]
static REQUESTS_START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[link_section = ".limine_requests_end"]
static REQUESTS_END: RequestsEndMarker = RequestsEndMarker::new();

// ---- Symbols defined by the linker script ------------------------------

unsafe extern "C" {
    static __kernel_image_start: u8;
    static __kernel_image_end: u8;
    static __bss_start: u8;
    static __bss_end: u8;
}

/// Address of a linker-defined symbol.
///
/// These symbols have no value of their own; only their addresses are
/// meaningful. Taking a reference and casting it is the standard way to read
/// one, and it never dereferences the (possibly zero-sized) storage.
fn linker_symbol(symbol: &'static u8) -> u64 {
    core::ptr::from_ref(symbol) as u64
}

// ---- Entry point -------------------------------------------------------

/// Kernel entry point.
///
/// Limine enters here in 64-bit long mode with paging enabled, interrupts
/// disabled, and a valid stack, per the Limine boot protocol. The name
/// matches `ENTRY(kstart)` in the linker script.
///
/// `extern "C"` because the bootloader calls this across an ABI boundary;
/// `no_mangle` so the linker script can find it by name.
#[unsafe(no_mangle)]
pub extern "C" fn kstart() -> ! {
    // Serial first: without it there is no way to report anything at all,
    // including a failure to bring up serial.
    let uart_ok = serial::init();

    println!();
    println!("{} {}", KERNEL_NAME, KERNEL_VERSION);
    println!(
        "[boot] serial: COM1 115200 8N1 ({})",
        if uart_ok {
            "self-test ok"
        } else {
            "self-test FAILED"
        }
    );
    // `ready()` must agree with what `init()` just told us; if it does not,
    // the driver's state tracking is broken and later code cannot trust it.
    assert_eq!(
        serial::ready(),
        uart_ok,
        "serial readiness flag disagrees with init() result"
    );

    // If the bootloader did not honour the revision we asked for, every
    // response pointer below is meaningless and must not be trusted.
    assert!(
        BASE_REVISION.is_supported(),
        "Limine base revision 3 is not supported by this bootloader"
    );

    let info = BOOTLOADER_INFO
        .response()
        .expect("Limine did not answer the bootloader info request");
    println!("[boot] bootloader: {} {}", info.name(), info.version());

    let address = EXECUTABLE_ADDRESS
        .response()
        .expect("Limine did not answer the executable address request");
    let virtual_base = address.virtual_base;
    let physical_base = address.physical_base;
    println!(
        "[boot] higher-half base: {:#x} (phys {:#x}, slide {:#x})",
        virtual_base,
        physical_base,
        virtual_base.wrapping_sub(physical_base)
    );

    let hhdm = HHDM
        .response()
        .expect("Limine did not answer the HHDM request");
    println!("[boot] hhdm offset: {:#x}", hhdm.offset);

    // The stack size request has no data; the mere presence of a response
    // means Limine complied. If it did not, we are running on a smaller
    // stack than we sized our formatting code for.
    assert!(
        STACK.response().is_some(),
        "Limine did not honour the stack size request"
    );
    println!("[boot] stack: {} KiB (granted)", STACK_SIZE / 1024);

    // SAFETY: These four are linker-defined symbols. We only take their
    // addresses -- the storage behind them is never read or written, and
    // never dereferenced -- so there is no aliasing or initialisation
    // requirement to violate. The `static`s exist solely to give the symbols
    // a name the borrow checker can attach a lifetime to.
    let (image_start, image_end, bss_start, bss_end) = unsafe {
        (
            linker_symbol(&__kernel_image_start),
            linker_symbol(&__kernel_image_end),
            linker_symbol(&__bss_start),
            linker_symbol(&__bss_end),
        )
    };
    println!(
        "[boot] image: {:#x} .. {:#x} ({} KiB)",
        image_start,
        image_end,
        (image_end - image_start) / 1024
    );
    println!("[boot] bss:   {:#x} .. {:#x}", bss_start, bss_end);

    // A higher-half kernel that is not actually in the higher half means the
    // link address and the code model disagree, and every absolute address
    // we compute from here would be wrong.
    assert!(
        virtual_base >= HIGHER_HALF_BASE,
        "executable virtual base is not in the higher half"
    );

    // Limine hands back an empty string rather than a null pointer when no
    // command line was configured.
    let cmdline = CMDLINE.response().map(|r| r.cmdline()).unwrap_or("");
    println!(
        "[boot] cmdline: {}",
        if cmdline.is_empty() {
            "<none>"
        } else {
            cmdline
        }
    );

    // Negative-path hook. The panic ISO boots this very same binary with
    // `panic-test` on the command line, so the failure path is exercised on
    // the image we ship rather than on a special build.
    if cmdline.contains("panic-test") {
        println!("[boot] cmdline requested a panic; triggering now");
        panic!("deliberate panic requested via cmdline (token=panic-test)");
    }

    println!("[boot] M0 checks passed");
    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        qemu::EXIT_SUCCESS,
        qemu::HOST_STATUS_SUCCESS
    );

    qemu::exit(qemu::EXIT_SUCCESS)
}
