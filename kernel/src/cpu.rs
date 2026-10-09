// SPDX-License-Identifier: MPL-2.0
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

/// `IA32_STAR`: the segment selectors `syscall` and `sysret` use.
///
/// Bits 47:32 are the base `syscall` derives CS and SS from; bits 63:48
/// are the base `sysret` derives them from. See ADR 029 for why the
/// GDT's order is dictated by this register.
///
/// Reference: Intel SDM Vol. 4, Table 2-2, MSR address 0xC000_0081.
const IA32_STAR: u32 = 0xC000_0081;

/// `IA32_LSTAR`: the address `syscall` jumps to in 64-bit mode.
///
/// Reference: Intel SDM Vol. 4, Table 2-2, MSR address 0xC000_0082.
const IA32_LSTAR: u32 = 0xC000_0082;

/// `IA32_FMASK`: RFLAGS bits cleared on `syscall`.
///
/// Reference: Intel SDM Vol. 4, Table 2-2, MSR address 0xC000_0084.
const IA32_FMASK: u32 = 0xC000_0084;

/// Page Attribute Table (ADR 051).
///
/// Eight one-byte entries; a leaf page-table entry picks one with its
/// PAT, PCD and PWT bits.
///
/// Reference: Intel SDM Vol. 3A, 12.12, MSR address 0x277.
const IA32_PAT: u32 = 0x277;

/// Memory types, as a PAT entry encodes them.
///
/// Reference: Intel SDM Vol. 3A, Table 12-10.
mod pat_type {
    /// Uncacheable, and not overridable by an MTRR.
    pub const UC: u64 = 0x00;
    /// Write-combining: writes gather and leave in bursts.
    pub const WC: u64 = 0x01;
    /// Write-through.
    pub const WT: u64 = 0x04;
    /// Uncacheable, overridable by an MTRR to WC.
    pub const UC_MINUS: u64 = 0x07;
    /// Write-back. The ordinary type for RAM.
    pub const WB: u64 = 0x06;
}

/// What this kernel programs into the PAT.
///
/// **Entries 0..3 keep their reset values and entry 4 becomes WC.**
///
/// The reset table (SDM Table 12-10) is WB, WT, UC-, UC and then the
/// same four again — so there is no write-combining entry at all
/// until this is written, which is why ADR 051 exists.
///
/// Leaving 0..3 alone is the load-bearing half. Every mapping the
/// kernel has already made, and every mapping Limine left behind,
/// means what it means because of those four: the physical window is
/// WB, the LAPIC and I/O APIC are PCD so UC-. Redefining one of them
/// would change what existing page-table entries mean without
/// touching a single entry, and the symptom would surface somewhere
/// with no visible connection to the change.
///
/// Entry 4 is selected by PAT=1, PCD=0, PWT=0 — a bit no leaf in this
/// kernel set before M14, so nothing is reinterpreted by defining it.
const PAT_VALUE: u64 = pat_type::WB
    | (pat_type::WT << 8)
    | (pat_type::UC_MINUS << 16)
    | (pat_type::UC << 24)
    | (pat_type::WC << 32)
    | (pat_type::WT << 40)
    | (pat_type::UC_MINUS << 48)
    | (pat_type::UC << 56);

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
        /// Bit 9, OSFXSR: the OS supports `fxsave`/`fxrstor`.
        ///
        /// Also the master switch for SSE: without it every SSE
        /// instruction raises #UD. M8 needs both halves -- user code
        /// uses SSE, and the context switch saves it (ADR 031).
        const OSFXSR = 1 << 9;
        /// Bit 10, OSXMMEXCPT: unmasked SSE floating-point errors are
        /// reported as #XM (vector 19).
        ///
        /// Without it the same errors arrive as #UD -- "invalid
        /// opcode", which is actively misleading about what happened.
        /// M1's IDT already fills vector 19.
        const OSXMMEXCPT = 1 << 10;
        /// Bit 20, SMEP: supervisor mode execution prevention. Fetching an
        /// instruction from a user page at CPL 0 faults.
        const SMEP = 1 << 20;
        /// Bit 21, SMAP: supervisor mode access prevention. Reading or
        /// writing a user page at CPL 0 faults unless EFLAGS.AC is set
        /// (via `stac`/`clac`).
        const SMAP = 1 << 21;
    }
}

bitflags! {
    /// Bits of CR0 that affect the FPU.
    ///
    /// Reference: Intel SDM Vol. 3A, Figure 2-7 and 10.2.1.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Cr0: u64 {
        /// Bit 1, MP: monitor coprocessor. Makes `wait`/`fwait` respect
        /// TS. TS is never set here, but MP is the configuration the
        /// architecture recommends and it costs nothing.
        const MP = 1 << 1;
        /// Bit 2, EM: emulation. When set, **every** SSE and x87
        /// instruction raises #UD. Must be clear.
        const EM = 1 << 2;
        /// Bit 3, TS: task switched.
        ///
        /// The key to lazy FPU switching, which ADR 031 rejected --
        /// partly because it is the basis of CVE-2018-3665. Cleared
        /// explicitly rather than left alone, so that "TS might have
        /// been set somewhere" is not a possibility to reason about.
        const TS = 1 << 3;
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
    /// M8: `fxsave`/`fxrstor` and SSE are usable (CR4.OSFXSR).
    pub fxsr_enabled: bool,
    /// M8: SSE errors arrive as #XM rather than #UD (CR4.OSXMMEXCPT).
    pub xmmexcpt_enabled: bool,
    /// M8: `syscall`/`sysret` are usable (EFER.SCE).
    pub syscall_enabled: bool,
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
            | (self.fxsr_enabled as u64) << 8
            | (self.xmmexcpt_enabled as u64) << 9
            | (self.syscall_enabled as u64) << 10
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
            fxsr_enabled: bits & (1 << 8) != 0,
            xmmexcpt_enabled: bits & (1 << 9) != 0,
            syscall_enabled: bits & (1 << 10) != 0,
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

/// Read CR0.
fn read_cr0() -> u64 {
    let value: u64;
    // SAFETY: Reading a control register into a general register has no
    // side effects and touches no memory. Requires CPL 0, which holds.
    unsafe {
        asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Write CR0.
///
/// # Safety
///
/// CR0 holds PE, PG and WP among others; clearing one of those while
/// paging is active is undefined. Only the FPU bits are touched here.
unsafe fn write_cr0(value: u64) {
    // SAFETY: The caller guarantees `value` is legal for the current
    // mode. Writing CR0 is serialising.
    unsafe {
        asm!("mov cr0, {}", in(reg) value, options(nostack, preserves_flags));
    }
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

    // --- FPU/SSE (M8, ADR 031) ------------------------------------------
    //
    // OSFXSR is the master switch for SSE *and* for fxsave/fxrstor. The
    // kernel builds with -mno-sse and does not need it, but user code
    // does -- there is no way to tell a user component not to use SSE,
    // and no sane memcpy avoids XMM.
    //
    // OSXMMEXCPT decides how an unmasked SSE error is reported. Without
    // it the CPU raises #UD -- "invalid opcode" -- for an instruction
    // that is perfectly valid and merely produced a NaN. The diagnostic
    // would point at the wrong thing entirely.
    //
    // OSXSAVE is deliberately *not* set. It would enable AVX, whose
    // state fxsave does not cover; leaving it clear means the state
    // this kernel cannot save is state user code cannot reach. See
    // ADR 031 for why xsave was not used instead.
    cr4 |= Cr4::OSFXSR | Cr4::OSXMMEXCPT;

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

    // --- CR0: the FPU must not be emulated, and TS must stay clear ------
    //
    // EM set would make every SSE instruction #UD. Limine leaves it
    // clear, but "Limine probably does X" is not something to build on
    // -- the same argument M3 made about NXE.
    //
    // TS is cleared explicitly rather than merely left alone. It is the
    // switch lazy FPU switching turns on, and ADR 031 rejected lazy
    // switching; clearing it here means "TS might be set somewhere" is
    // not a state anyone has to reason about.
    let cr0_before = read_cr0();
    let cr0_after = (cr0_before | Cr0::MP.bits()) & !(Cr0::EM.bits() | Cr0::TS.bits());

    // SAFETY: Only the three FPU bits are touched; PE, PG and WP keep
    // their current values, so paging and write protection are
    // undisturbed.
    unsafe {
        write_cr0(cr0_after);
    }

    let cr0_now = Cr0::from_bits_truncate(read_cr0());
    assert!(
        !cr0_now.contains(Cr0::EM),
        "CR0.EM is set: every SSE instruction would raise #UD, and user \
         code cannot avoid SSE"
    );
    assert!(
        !cr0_now.contains(Cr0::TS),
        "CR0.TS is set, which is the lazy-FPU switch ADR 031 rejected; \
         the first FPU instruction after a switch would take #NM and \
         nothing handles it"
    );

    let features = Features {
        nx_supported,
        nx_enabled,
        smep_supported,
        smep_enabled: cr4_now.contains(Cr4::SMEP),
        smap_supported,
        smap_enabled: cr4_now.contains(Cr4::SMAP),
        pge_enabled: cr4_now.contains(Cr4::PGE),
        fxsr_enabled: cr4_now.contains(Cr4::OSFXSR),
        xmmexcpt_enabled: cr4_now.contains(Cr4::OSXMMEXCPT),
        // Set later, by `enable_syscall`: it needs the GDT loaded and
        // the entry stub's address, neither of which exists here.
        syscall_enabled: false,
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
            "[cpu] WARNING: SMEP/SMAP unavailable on this CPU; ring 3 \
             isolation is weaker. Under QEMU, add +smep,+smap to -cpu."
        );
    }

    // M8. OSXSAVE is absent from this line deliberately: ADR 031 keeps
    // it clear, so that the state `fxsave` cannot save is also state
    // user code cannot reach.
    println!(
        "[cpu] fpu: osfxsr {}, osxmmexcpt {}, cr0.em clear, cr0.ts clear \
         (eager fxsave, no lazy switching -- ADR 031)",
        if f.fxsr_enabled { "set" } else { "CLEAR" },
        if f.xmmexcpt_enabled { "set" } else { "CLEAR" }
    );

    assert!(
        f.fxsr_enabled,
        "CR4.OSFXSR did not stick; SSE raises #UD and fxsave does not \
         exist, so no user thread could run and no context switch could \
         preserve what it did"
    );
    assert!(
        f.xmmexcpt_enabled,
        "CR4.OSXMMEXCPT did not stick; an SSE numeric error would arrive \
         as #UD and the report would name the wrong cause"
    );
}

// ---- syscall/sysret (M8, ADR 029 and 030) ------------------------------

/// RFLAGS bits cleared on entry to `syscall`, via `IA32_FMASK`.
///
/// Each one is a way user code could otherwise reach into the kernel's
/// execution environment:
///
/// * **IF (9)** — `syscall` is not an interrupt gate and does not clear
///   IF itself. Without this, an interrupt can arrive between entry and
///   the stack switch, while RSP still points at *user* memory: the
///   handler would push its frame to an address user code controls.
/// * **DF (10)** — the SysV ABI requires DF clear on entry to a C
///   function. User code can leave it set, and the kernel's first string
///   operation would then run backwards.
/// * **TF (8)** — single-step. Set by user code, the kernel's first
///   instruction takes #DB, and single-stepping walks through kernel
///   code.
/// * **AC (18)** — M7's lesson arriving late. `stac` is not privileged:
///   it needs CR4.SMAP and nothing else, so user code can set AC and
///   then `syscall`. The `UserAccess` guard reads "AC already set" and
///   does nothing at all -- neither `stac` nor `clac` -- on the
///   reasoning that an outer region owns the permission. SMAP would be
///   off for the whole syscall and nothing would say so.
///
/// Reference: Intel SDM Vol. 2B, "SYSCALL"; Vol. 1, Figure 3-8.
const FMASK_BITS: u64 = (1 << 8) | (1 << 9) | (1 << 10) | (1 << 18);

/// What `IA32_FMASK` actually holds.
///
/// Read from the MSR, not from [`FMASK_BITS`]. The constant is what
/// the code meant; the MSR is what the CPU will act on, and
/// [`enable_syscall`] is where the two could come apart.
///
/// Exists because a mutation test found the AC bit's only evidence
/// was a line of printed text -- which the same edit that removed the
/// bit could have removed too. ADR 023's K5: a guard indistinguishable
/// from its own absence is not a guard.
///
/// # Panics
///
/// Before [`enable_syscall`] has run, when the MSR is whatever the
/// firmware left.
pub fn fmask() -> u64 {
    assert!(
        features().syscall_enabled,
        "cpu::fmask() before enable_syscall(); the MSR holds whatever \
         the firmware left and says nothing about this kernel"
    );
    // SAFETY: IA32_FMASK exists on any CPU that implements SYSCALL,
    // which `enable_syscall` established before setting the flag this
    // assertion just checked. Reading an MSR has no side effects.
    unsafe { read_msr(IA32_FMASK) }
}

/// Turn on `syscall`/`sysret` and point them at the entry stub.
///
/// Separate from [`init`] because it needs two things that do not exist
/// that early: the GDT must be loaded, since STAR names selectors in it,
/// and the entry stub must have an address.
///
/// # Safety
///
/// Call once, after `gdt::init`, with interrupts disabled. `entry` must
/// be the address of a stub that obeys ADR 030's contract: it is entered
/// Program the PAT so that entry 4 means write-combining (ADR 051).
///
/// # Why this has to happen at all
///
/// The reset value of the PAT has **no write-combining entry**: the
/// eight slots are WB, WT, UC-, UC and then those four again (SDM
/// Vol. 3A, Table 12-10). A page-table entry asking for "entry 4"
/// gets write-back, which for a framebuffer means drawing that is
/// correct and invisible until something flushes it.
///
/// So M14's `PageFlags::WRITE_COMBINING` is inert until this runs.
/// That is worth stating because the failure would be quiet: the
/// mapping succeeds, the pixels are right, and only the timing is
/// wrong.
///
/// # Why entries 0..3 are left alone
///
/// Everything mapped before this call — the physical window, the
/// kernel image, the LAPIC, the I/O APIC, and whatever Limine set up
/// — means what it means through entries 0..3. Redefining one of
/// them would change the cache type of live mappings without
/// touching a page-table entry, and the symptom would appear
/// somewhere unrelated to the line that caused it.
///
/// # Ordering
///
/// Must run **before** any write-combining mapping is created, and
/// it is called from `kmain` right after `cpu::init`. Nothing before
/// that point asks for WC.
///
/// No TLB flush is needed here: no existing entry sets the PAT bit,
/// so no cached translation changes meaning. A kernel that
/// *redefined* an entry in use would have to flush, and would be
/// wrong for the reason above regardless.
///
/// # Safety
///
/// Called once, at boot, at CPL 0, before any mapping asks for
/// write-combining.
pub unsafe fn init_pat() {
    // CPUID leaf 1, EDX bit 16: the PAT exists.
    //
    // Checked rather than assumed: on a CPU without it, writing MSR
    // 0x277 raises #GP, and a #GP during boot is a triple fault with
    // nothing printed.
    //
    // Reference: Intel SDM Vol. 2A, Table 3-8, leaf 01H, EDX bit 16.
    let (_, _, _, edx) = cpuid(1, 0);
    assert!(
        edx & (1 << 16) != 0,
        "this CPU does not implement the Page Attribute Table, so a \
         write-combining mapping cannot be expressed (ADR 051). The \
         framebuffer would have to be uncacheable instead."
    );

    // SAFETY: The PAT exists (just checked) and `PAT_VALUE` contains
    // only the memory-type encodings the SDM defines -- no reserved
    // value, which is the one way a WRMSR here raises #GP. CPL 0.
    unsafe {
        write_msr(IA32_PAT, PAT_VALUE);
    }

    // Read it back, for the reason `enable_syscall` does: a write
    // that did not take would surface as a framebuffer that draws
    // slowly, which is not a symptom anybody traces back to an MSR.
    //
    // SAFETY: As above.
    let now = unsafe { read_msr(IA32_PAT) };
    assert_eq!(
        now, PAT_VALUE,
        "the PAT reads back as {:#018x} rather than the {:#018x} just \
         written",
        now, PAT_VALUE
    );

    println!(
        "[cpu] PAT: entry 4 = write-combining; entries 0..3 left at \
         their reset values (WB, WT, UC-, UC) so existing mappings \
         keep their meaning (ADR 051)"
    );
}

/// with RSP still pointing at user memory, RCX holding the return
/// address and R11 the saved RFLAGS.
pub unsafe fn enable_syscall(entry: u64) {
    // CPUID leaf 0x8000_0001, EDX bit 11: SYSCALL/SYSRET.
    //
    // Reference: Intel SDM Vol. 2A, Table 3-8, leaf 80000001H, EDX
    // bit 11.
    let (_, _, _, ext_edx) = cpuid(0x8000_0001, 0);
    assert!(
        ext_edx & (1 << 11) != 0,
        "this CPU does not implement SYSCALL/SYSRET, which is the only \
         way into the kernel from ring 3 that this kernel implements"
    );

    // STAR holds two 16-bit selector bases. The arithmetic the CPU
    // applies to each is in ADR 029, and `gdt`'s const assertions tie
    // the GDT's layout to it -- so a wrong slot is a build failure
    // rather than a silently wrong descriptor here.
    let star =
        ((crate::gdt::STAR_USER_BASE as u64) << 48) | ((crate::gdt::STAR_KERNEL_BASE as u64) << 32);

    // SAFETY: All four MSRs exist on a CPU that just reported SYSCALL
    // support, and we are at CPL 0. The selectors name descriptors in
    // the GDT the caller guarantees is loaded.
    //
    // SCE is written last. Until it is set `syscall` is #UD, so there
    // is no window in which the instruction works while its
    // configuration is half written.
    unsafe {
        write_msr(IA32_STAR, star);
        write_msr(IA32_LSTAR, entry);
        write_msr(IA32_FMASK, FMASK_BITS);

        let efer = read_msr(IA32_EFER) | Efer::SCE.bits();
        write_msr(IA32_EFER, efer);
    }

    // Read everything back. A write that silently did not take would
    // surface as a #GP or a jump into nowhere on the first syscall,
    // with nothing to suggest an MSR was the cause.
    //
    // SAFETY: As above.
    let (star_now, lstar_now, fmask_now, efer_now) = unsafe {
        (
            read_msr(IA32_STAR),
            read_msr(IA32_LSTAR),
            read_msr(IA32_FMASK),
            Efer::from_bits_truncate(read_msr(IA32_EFER)),
        )
    };

    assert_eq!(
        star_now, star,
        "IA32_STAR did not take the value written; sysret would load \
         whatever selectors happen to be there"
    );
    assert_eq!(
        lstar_now, entry,
        "IA32_LSTAR did not take the entry point; the first syscall would \
         jump to {:#018x}",
        lstar_now
    );
    assert_eq!(
        fmask_now, FMASK_BITS,
        "IA32_FMASK did not take the mask; at least one of IF, DF, TF and \
         AC would survive into the kernel"
    );
    assert!(
        efer_now.contains(Efer::SCE),
        "EFER.SCE did not stick; `syscall` is still #UD"
    );

    // Record it, so `features()` reports the truth rather than the
    // placeholder `init` stored before the GDT existed.
    let bits = FEATURE_BITS.load(Ordering::Acquire);
    FEATURE_BITS.store(bits | (1 << 10), Ordering::Release);

    println!(
        "[cpu] syscall: enabled (efer.sce set), lstar {:#018x}, star \
         {:#018x}",
        entry, star
    );
    println!(
        "[cpu] syscall: fmask clears IF|DF|TF|AC -- AC because `stac` is \
         unprivileged, so user code can hand the kernel an open SMAP \
         window and the UserAccess guard would read it as somebody \
         else's (ADR 030)"
    );
}

fn feature_state(supported: bool, enabled: bool) -> &'static str {
    match (supported, enabled) {
        (true, true) => "enabled",
        (true, false) => "supported but NOT ENABLED",
        (false, _) => "unsupported",
    }
}

// ---- SMAP: deliberate access to user pages -----------------------------

/// RFLAGS.AC, bit 18.
///
/// With CR4.SMAP set, a CPL 0 access to a page marked USER faults unless
/// this flag is set. The point is that touching user memory should be a
/// *decision*, not something a stray pointer can do by accident.
///
/// Reference: Intel SDM Vol. 3A, 4.6.1 "Determination of Access Rights",
/// and Vol. 1, Figure 3-8.
const RFLAGS_AC: u64 = 1 << 18;

/// Permission to touch user pages, for as long as this value lives.
///
/// # Why this arrived in M7 rather than M8
///
/// The known-gaps list said `stac`/`clac` would be needed "once user
/// mappings exist", and filed it under M8 with ring 3. That was one
/// milestone too late, and the reason is worth keeping: SMAP keys off
/// the page's USER *bit*, not off the privilege level of the code.
///
/// M7 creates pages marked USER -- that is what a user address space is
/// -- while every thread still runs at CPL 0. So the first user mapping
/// and the first SMAP violation arrive together, a milestone before the
/// first ring-3 instruction. The acceptance test found it immediately:
/// `supervisor read protection-violation` on a page that was plainly
/// present and plainly readable.
///
/// # Why a guard rather than a pair of calls
///
/// Hand-written `stac` ... `clac` pairs have one failure mode, and it
/// is silent: an early `return` between them leaves AC set, and from
/// that point SMAP protects nothing at all. Nothing faults, nothing
/// logs, and the protection is simply gone.
///
/// `Drop` runs on every path out of a scope, including a panic unwind
/// and an early return, so the pairing cannot be forgotten. The same
/// argument as `TlbFlush` in ADR 012: when a rule can be encoded in a
/// type, a comment is the weaker choice.
pub struct UserAccess {
    /// Whether AC was already set when this guard was created.
    ///
    /// Restored rather than unconditionally cleared, for the reason
    /// ADR 025 gives about `InterruptGuard`: an inner region that ended
    /// with `clac` would silently revoke an outer one's permission.
    was_set: bool,
}

impl UserAccess {
    /// Allow this CPU to touch USER pages until the guard is dropped.
    pub fn new() -> UserAccess {
        let was_set = read_rflags() & RFLAGS_AC != 0;

        // The condition carries the `stac` instruction's own
        // preconditions: on a CPU without CR4.SMAP it is #UD, and
        // setting AC when it is already set would make the restore on
        // drop clear somebody else's permission.
        if features().smap_enabled && !was_set {
            // SAFETY: `stac` sets RFLAGS.AC and has no other effect.
            // It requires CPL 0, which holds everywhere in this
            // kernel, and CR4.SMAP, which the condition above
            // established.
            //
            // Reference: Intel SDM Vol. 2B, "STAC".
            unsafe {
                core::arch::asm!("stac", options(nomem, nostack));
            }
        }

        UserAccess { was_set }
    }

    /// Whether SMAP is actually enforcing anything on this machine.
    ///
    /// Reported by the acceptance test, because on a CPU without SMAP
    /// this guard is a no-op and a test that did not say so would be
    /// claiming a protection it never exercised.
    pub fn enforcing() -> bool {
        features().smap_enabled
    }
}

impl Drop for UserAccess {
    fn drop(&mut self) {
        // Mirrors `new`: only clear AC if this guard is the one that
        // set it. An inner guard clearing an outer one's permission is
        // the nesting bug ADR 025 names for `InterruptGuard`.
        if features().smap_enabled && !self.was_set {
            // SAFETY: `clac` clears RFLAGS.AC and has no other effect;
            // the same CPL 0 and CR4.SMAP requirements as `stac`, both
            // established by the condition above.
            //
            // Reference: Intel SDM Vol. 2B, "CLAC".
            unsafe {
                core::arch::asm!("clac", options(nomem, nostack));
            }
        }
    }
}

/// Read RFLAGS.
fn read_rflags() -> u64 {
    let flags: u64;
    // SAFETY: `pushfq` pushes RFLAGS and `pop` takes it off again. Both
    // touch the stack, so `nostack` would be a lie.
    unsafe {
        core::arch::asm!("pushfq; pop {}", out(reg) flags, options(preserves_flags));
    }
    flags
}

/// Whether RFLAGS.AC is set, for the acceptance test.
pub fn user_access_allowed() -> bool {
    read_rflags() & RFLAGS_AC != 0
}
