// SPDX-License-Identifier: BSD-2-Clause
//! CPU feature discovery and control-register setup.
//!
//! Three features matter here, and all three must be enabled *before* we
//! install our own page tables:
//!
//! * **EFER.NXE** — without it the NX bit in a page-table entry is a
//!   *reserved* bit, and setting it makes every access to that page fault.
//!   The entire W^X design depends on this one bit.
//! * **CR4.SMEP/SMAP** — stop the kernel executing or reading user pages.
//!   No user mappings exist until M8, but enabling them now means M8
//!   inherits the protection rather than having to remember it.
//! * **CR4.PGE** — global pages, so kernel mappings survive a CR3 reload.
//!
//! Nothing here trusts the bootloader. Limine very likely sets NXE, but
//! "very likely" is not a property we can build W^X on, and the cost of
//! checking is a few instructions on the boot path.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use bitflags::bitflags;

use crate::println;

/// `IA32_EFER`, the Extended Feature Enable Register.
///
/// Reference: Intel SDM Vol. 4, Table 2-2 ("IA-32 Architectural MSRs"),
/// MSR address 0xC000_0080.
const IA32_EFER: u32 = 0xC000_0080;

bitflags! {
    /// Bits of `IA32_EFER` that this kernel cares about.
    ///
    /// Reference: Intel SDM Vol. 3A, Figure 2-8 "IA32_EFER MSR Layout".
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Efer: u64 {
        /// Bit 0, SCE: SYSCALL/SYSRET enable. Needed in M8; not set here.
        const SCE = 1 << 0;
        /// Bit 8, LME: long mode enable. Already set -- we are in long mode.
        const LME = 1 << 8;
        /// Bit 10, LMA: long mode active. Read-only status bit.
        const LMA = 1 << 10;
        /// Bit 11, NXE: no-execute enable.
        ///
        /// When clear, bit 63 of a page-table entry is reserved, and any
        /// access through an entry with it set raises #PF with the
        /// reserved-bit error code. When set, bit 63 means "not
        /// executable".
        ///
        /// Reference: Intel SDM Vol. 3A, 4.6 "Access Rights" and 4.9.2
        /// ("execute-disable ... only if IA32_EFER.NXE = 1").
        const NXE = 1 << 11;
    }
}

bitflags! {
    /// Bits of CR4 that this kernel sets.
    ///
    /// Reference: Intel SDM Vol. 3A, Figure 2-7 "Control Registers".
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Cr4: u64 {
        /// Bit 7, PGE: page global enable. Without it the global bit in a
        /// page-table entry is ignored and every CR3 reload flushes the
        /// kernel's mappings too.
        const PGE = 1 << 7;
        /// Bit 20, SMEP: supervisor mode execution prevention. Fetching an
        /// instruction from a user page at CPL 0 faults.
        const SMEP = 1 << 20;
        /// Bit 21, SMAP: supervisor mode access prevention. Reading or
        /// writing a user page at CPL 0 faults unless EFLAGS.AC is set
        /// (via `stac`/`clac`).
        const SMAP = 1 << 21;
    }
}

/// What the CPU told us it supports, and what we managed to enable.
#[derive(Clone, Copy, Debug)]
pub struct Features {
    pub nx_supported: bool,
    pub nx_enabled: bool,
    pub smep_supported: bool,
    pub smep_enabled: bool,
    pub smap_supported: bool,
    pub smap_enabled: bool,
    pub pge_enabled: bool,
}

/// [`Features`] packed into a word, so [`features`] can hand it back later.
///
/// The kernel switches stacks partway through boot (see
/// `vm::switch_to_main_stack`), and nothing can be carried across that
/// switch in a local. A static is the only way for code on the far side to
/// learn what the CPU supports without asking the CPU again.
static FEATURE_BITS: AtomicU64 = AtomicU64::new(0);

impl Features {
    /// Set once [`init`] has stored a real value, so that reading before
    /// then is a panic rather than "nothing is supported".
    const VALID: u64 = 1 << 7;

    fn to_bits(self) -> u64 {
        (self.nx_supported as u64)
            | (self.nx_enabled as u64) << 1
            | (self.smep_supported as u64) << 2
            | (self.smep_enabled as u64) << 3
            | (self.smap_supported as u64) << 4
            | (self.smap_enabled as u64) << 5
            | (self.pge_enabled as u64) << 6
            | Self::VALID
    }

    fn from_bits(bits: u64) -> Self {
        Self {
            nx_supported: bits & 1 != 0,
            nx_enabled: bits & (1 << 1) != 0,
            smep_supported: bits & (1 << 2) != 0,
            smep_enabled: bits & (1 << 3) != 0,
            smap_supported: bits & (1 << 4) != 0,
            smap_enabled: bits & (1 << 5) != 0,
            pge_enabled: bits & (1 << 6) != 0,
        }
    }
}

/// What [`init`] found, for code that runs after the stack switch.
pub fn features() -> Features {
    let bits = FEATURE_BITS.load(Ordering::Acquire);
    assert!(
        bits & Features::VALID != 0,
        "cpu::features() called before cpu::init()"
    );
    Features::from_bits(bits)
}

/// Execute CPUID.
///
/// # Safety
///
/// CPUID is unprivileged and has no side effects, but it clobbers all four
/// of EAX/EBX/ECX/EDX, and `rbx` is callee-saved in the SysV ABI -- so it
/// has to be preserved by hand.
fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let (eax, ebx, ecx, edx);
    // SAFETY: CPUID reads no memory and writes only the four output
    // registers. RBX is callee-saved under SysV, and the `cpuid` instruction
    // clobbers it, so it is saved and restored around the instruction --
    // LLVM will not let us name rbx as an operand directly.
    //
    // Reference: Intel SDM Vol. 2A, "CPUID - CPU Identification".
    unsafe {
        asm!(
            // `:r` names the 64-bit form explicitly. Without it the
            // compiler warns: the operand's type is u32, but `xchg` with
            // rbx needs a 64-bit register, and silently guessing which
            // width was meant is exactly the kind of thing that produces a
            // subtly wrong instruction.
            "xchg {tmp:r}, rbx",
            "cpuid",
            "xchg {tmp:r}, rbx",
            tmp = out(reg) ebx,
            inout("eax") leaf => eax,
            inout("ecx") subleaf => ecx,
            out("edx") edx,
            options(nostack, preserves_flags),
        );
    }
    (eax, ebx, ecx, edx)
}

/// Read a model-specific register.
///
/// # Safety
///
/// `msr` must name a register this CPU implements: reading an unimplemented
/// MSR raises #GP. Requires CPL 0.
unsafe fn read_msr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: The caller guarantees the MSR exists. RDMSR returns the value
    // split across EDX:EAX and touches no memory.
    //
    // Reference: Intel SDM Vol. 2B, "RDMSR - Read from Model Specific
    // Register".
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((high as u64) << 32) | (low as u64)
}

/// Write a model-specific register.
///
/// # Safety
///
/// `msr` must exist and `value` must be a legal value for it; writing a
/// reserved bit raises #GP. Changing EFER in particular alters how every
/// subsequent page-table walk is interpreted.
unsafe fn write_msr(msr: u32, value: u64) {
    // SAFETY: The caller guarantees the MSR and value are valid. WRMSR
    // takes the value in EDX:EAX and touches no memory.
    //
    // Reference: Intel SDM Vol. 2B, "WRMSR - Write to Model Specific
    // Register".
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") (value & 0xFFFF_FFFF) as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Read CR4.
fn read_cr4() -> u64 {
    let value: u64;
    // SAFETY: Reading a control register into a general register has no
    // side effects and touches no memory. Requires CPL 0, which holds.
    unsafe {
        asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Write CR4.
///
/// # Safety
///
/// Every bit of CR4 changes how the CPU behaves. Setting a reserved bit
/// raises #GP; clearing a bit the current configuration depends on (PAE,
/// for instance) is undefined while paging is active.
unsafe fn write_cr4(value: u64) {
    // SAFETY: The caller guarantees `value` is legal for the current mode.
    // Writing CR4 is a serialising operation and may flush the TLB, which
    // is harmless here.
    unsafe {
        asm!("mov cr4, {}", in(reg) value, options(nostack, preserves_flags));
    }
}

/// Discover and enable the features the kernel's memory protection needs.
///
/// Must run before any page table using the NX bit is installed.
///
/// # Safety
///
/// Call once, early in boot, with interrupts disabled. Modifies EFER and
/// CR4, which changes how every subsequent memory access is translated.
pub unsafe fn init() -> Features {
    // CPUID leaf 0x8000_0001, EDX bit 20: execute-disable support.
    //
    // Reference: Intel SDM Vol. 2A, Table 3-8 ("Information Returned by
    // CPUID"), leaf 80000001H, EDX bit 20 (XD / Execute Disable).
    let (_, _, _, ext_edx) = cpuid(0x8000_0001, 0);
    let nx_supported = ext_edx & (1 << 20) != 0;

    // CPUID leaf 7 subleaf 0: structured extended features.
    // EBX bit 7 = SMEP, bit 20 = SMAP.
    //
    // Reference: Intel SDM Vol. 2A, Table 3-8, leaf 07H, EBX.
    let (max_leaf, _, _, _) = cpuid(0, 0);
    let (smep_supported, smap_supported) = if max_leaf >= 7 {
        let (_, ebx, _, _) = cpuid(7, 0);
        (ebx & (1 << 7) != 0, ebx & (1 << 20) != 0)
    } else {
        (false, false)
    };

    // --- EFER.NXE -------------------------------------------------------
    //
    // This is the one that cannot be skipped. With NXE clear, bit 63 of a
    // page-table entry is reserved, and an access through an entry that
    // sets it raises #PF with the reserved-bit flag -- so every NX mapping
    // we are about to create would fault on first use.

    // SAFETY: IA32_EFER exists on every x86-64 CPU (it is how long mode was
    // entered). We are at CPL 0.
    let efer_before = Efer::from_bits_truncate(unsafe { read_msr(IA32_EFER) });

    let nx_enabled = if nx_supported {
        let efer_after = efer_before | Efer::NXE;
        // SAFETY: Only NXE is being set, on a CPU that just reported
        // support for it via CPUID. No page table currently in use sets
        // bit 63, so changing the interpretation of that bit cannot
        // invalidate an existing mapping.
        unsafe {
            write_msr(IA32_EFER, efer_after.bits());
        }

        // Read it back. A write that silently did not take would leave
        // every later NX mapping faulting, with the cause far behind.
        // SAFETY: Same as the read above.
        let efer_now = Efer::from_bits_truncate(unsafe { read_msr(IA32_EFER) });
        efer_now.contains(Efer::NXE)
    } else {
        false
    };

    // --- CR4: PGE, SMEP, SMAP -------------------------------------------

    let cr4_before = read_cr4();
    let mut cr4 = Cr4::from_bits_truncate(cr4_before);

    // Global pages: kernel mappings stay in the TLB across a CR3 reload.
    // Both halves of that are deliberate -- see docs/adr/012.
    cr4 |= Cr4::PGE;

    if smep_supported {
        cr4 |= Cr4::SMEP;
    }
    if smap_supported {
        // SMAP makes any kernel read or write of a user page fault unless
        // EFLAGS.AC is set. There are no user mappings until M8, so nothing
        // can trip over this yet -- and when M8 arrives, deliberate access
        // to user memory will have to go through stac/clac.
        cr4 |= Cr4::SMAP;
    }

    // Preserve the bits we do not manage (PAE, PSE, OSFXSR and friends):
    // start from the current value and only add.
    let cr4_after = cr4_before | cr4.bits();

    // SAFETY: Only PGE, SMEP and SMAP are being added, each on a CPU that
    // reported support (PGE is architectural on x86-64). Every other bit
    // keeps its current value, so no existing configuration is disturbed.
    unsafe {
        write_cr4(cr4_after);
    }

    // Read back: a feature bit that did not stick would otherwise be a
    // silent downgrade of protection.
    let cr4_now = Cr4::from_bits_truncate(read_cr4());

    let features = Features {
        nx_supported,
        nx_enabled,
        smep_supported,
        smep_enabled: cr4_now.contains(Cr4::SMEP),
        smap_supported,
        smap_enabled: cr4_now.contains(Cr4::SMAP),
        pge_enabled: cr4_now.contains(Cr4::PGE),
    };

    FEATURE_BITS.store(features.to_bits(), Ordering::Release);

    report(&features);

    // NX is not optional: the kernel image is about to be mapped W^X and
    // the physical window NX. Without it, either those mappings fault on
    // first use (NXE clear) or the protection silently does not exist.
    assert!(
        features.nx_enabled,
        "EFER.NXE could not be enabled (CPU support: {}); W^X is impossible \
         without it and every NX mapping would fault",
        nx_supported
    );

    features
}

/// Print what was found and what was enabled.
fn report(f: &Features) {
    println!(
        "[cpu] nx: {} (efer.nxe {})",
        if f.nx_supported {
            "supported"
        } else {
            "NOT SUPPORTED"
        },
        if f.nx_enabled { "set" } else { "CLEAR" }
    );

    // SMEP and SMAP protect nothing yet -- no user mappings exist -- but
    // they are enabled now so that M8 inherits the protection instead of
    // having to remember it.
    //
    // The test environment asks QEMU for `-cpu qemu64,+smep,+smap`. Until
    // M4 it did not, and the result was this path printing "unsupported"
    // on every run: the code worked, the protection did not exist, and
    // nothing failed. The acceptance runner now requires the "enabled"
    // form of this line, so a CPU model that stops advertising them is a
    // test failure rather than a silent downgrade.
    println!(
        "[cpu] smep: {}, smap: {}, pge: {}",
        feature_state(f.smep_supported, f.smep_enabled),
        feature_state(f.smap_supported, f.smap_enabled),
        if f.pge_enabled { "enabled" } else { "DISABLED" }
    );

    if !f.smep_supported || !f.smap_supported {
        println!(
            "[cpu] WARNING: SMEP/SMAP unavailable on this CPU; ring 3 in M8 \
             will have weaker isolation. Under QEMU, add +smep,+smap to -cpu."
        );
    }
}

fn feature_state(supported: bool, enabled: bool) -> &'static str {
    match (supported, enabled) {
        (true, true) => "enabled",
        (true, false) => "supported but NOT ENABLED",
        (false, _) => "unsupported",
    }
}
