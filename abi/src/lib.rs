// SPDX-License-Identifier: MPL-2.0
#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

//! The kernel/user ABI.
//!
//! **One definition, two readers.** Everything here is seen by both
//! the kernel and every user component, and the only way to keep
//! them in step is to have a single copy — a struct written twice is
//! two structs that happen to agree today.
//!
//! This crate has no dependencies, no allocation and no `unsafe`
//! outside the accessors at the bottom. It is a description of a
//! wire format, not a library.
//!
//! # What is frozen here
//!
//! The method numbers, the error codes and [`BootInfo`]'s layout are
//! an **ABI**: a component compiled against one kernel must keep
//! working against the next. The rule that follows is simple and
//! absolute — **numbers are appended, never inserted or reused**. A
//! renumbering is indistinguishable, from a compiled component's
//! point of view, from the kernel lying about what happened.

// ---- BootInfo (ADR 044) ------------------------------------------------

/// Where the kernel maps the root task's code.
pub const ROOT_CODE_ADDRESS: u64 = 0x0040_0000;

/// Where the kernel maps the **lowest page** of the root task's
/// stack.
pub const ROOT_STACK_ADDRESS: u64 = 0x0050_0000;

/// How many pages that stack is.
///
/// **Four from M14, one before that.** The root task assembles its
/// children's programs into 1032-byte buffers that live on the
/// stack — it has no writable statics and no heap — and M14's extra
/// scenario branch gave `main` a frame spanning more than a page.
///
/// LLVM emits a stack probe for a frame that large, touching each
/// page in turn, and the probe faulted: `cr2 == rsp == 0x4fffb0`,
/// sixteen bytes below the single page's base. That is the probe
/// working, finding the end of the stack before anything was
/// written past it.
pub const ROOT_STACK_PAGES: u64 = 4;

/// The root task's initial RSP: top of the highest stack page,
/// 16-byte aligned as the SysV ABI requires at a function entry.
///
/// The stack grows **down** from here towards
/// [`ROOT_STACK_ADDRESS`], which is why that constant names the
/// lowest page rather than the first one used.
pub const ROOT_STACK_TOP: u64 = ROOT_STACK_ADDRESS + ROOT_STACK_PAGES * 4096 - 16;

/// Where the kernel maps [`BootInfo`]. **Read-only** (ADR 044).
///
/// Read-only is not caution. BootInfo is what the kernel says about
/// the root task; if the root task could write it, it could read
/// back its own words as though they were the kernel's.
pub const BOOTINFO_ADDRESS: u64 = 0x0060_0000;

/// Where the screen component maps the framebuffer, in its own
/// address space (M14).
///
/// 16 MiB: clear of any component's code at 4 and stack at 5.
///
/// **In the ABI because the kernel's test needs it**, which is a
/// weaker reason than the other constants here and is worth being
/// honest about. The address is the component's own business — it
/// holds the VSpace and decides what goes where (ADR 043) — and
/// nothing in the kernel consults it.
///
/// What needs it is `fb-out-of-bounds`. That scenario's claim is
/// that CR2 lands exactly one page past the mapping, and the kernel
/// cannot learn the address from the component: the component
/// announces it and then faults, with no syscall in between, so the
/// waiting kernel thread is never scheduled to hear. Computing it
/// from this constant and the geometry is how the expected value
/// gets printed *before* the fault rather than never.
pub const FRAMEBUFFER_BASE: u64 = 0x0100_0000;

/// `"SafrBIV1"`, big-endian so it reads as text in a hex dump.
pub const BOOTINFO_MAGIC: u64 = 0x5361_6672_4249_5631;

/// The version this header describes.
///
/// **2 since M14** (ADR 049). M13 added `ioport_control_slot` without
/// bumping this, and that was right: a root task that did not read
/// the field lost nothing by not reading it.
///
/// M14's fields are different in kind. `framebuffer_untyped_slot`
/// *identifies* a capability inside the Untyped list — a device
/// Untyped that may only become a Frame (ADR 050). A root task that
/// does not know about it treats the framebuffer as ordinary memory,
/// tries to retype a TCB out of it, is refused with
/// `DeviceUntypedFrameOnly`, and concludes it has run out of memory.
/// The refusal is correct and the diagnosis is wrong.
///
/// A version bump turns that silent misreading into a stated
/// mismatch.
pub const BOOTINFO_VERSION: u32 = 2;

/// How many Untyped descriptors fit in the BootInfo page.
///
/// One page holds the header plus this many 16-byte descriptors.
/// Boot creates about 90 on a 256 MiB machine, so 128 is headroom
/// rather than a limit being approached — and the kernel asserts it
/// rather than truncating, because a silently short list would make
/// the accounting in scenario 3 wrong in a way that looks right.
pub const BOOTINFO_MAX_UNTYPED: usize = 128;

/// What the root task is told at startup (ADR 044).
///
/// `magic` and `version` are first **and that order is part of the
/// ABI**: they are the only two fields a root task of the wrong
/// version may rely on. Everything after them means whatever the
/// version says it means.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootInfo {
    /// [`BOOTINFO_MAGIC`].
    pub magic: u64,
    /// [`BOOTINFO_VERSION`].
    pub version: u32,
    /// Size of this header, descriptors excluded.
    ///
    /// For forward compatibility: a later kernel may add fields, and
    /// an older root task skips to the descriptors using this rather
    /// than `size_of::<BootInfo>()`, which would be its own idea of
    /// the size.
    pub header_size: u32,

    /// How many Untyped capabilities the root task holds.
    pub untyped_count: u32,
    /// Leaf-CNode index of the first of them. They are consecutive.
    pub untyped_first_slot: u32,

    /// Where the one IRQControl capability is (ADR 041).
    ///
    /// M13's IOPortControl is the obvious neighbour for this field
    /// and is deliberately **not** here: it is appended at the end of
    /// the struct, past `lost_bytes`. Inserting it would shift every
    /// field below, and a root task built against the old layout
    /// would go on reading the old offsets — getting
    /// `root_cnode_slot` where it expected its own TCB. The rule for
    /// struct offsets is the rule for method numbers.
    pub irq_control_slot: u32,
    /// The root task's own CNode, named within its own CSpace.
    ///
    /// Needed because almost every invocation that creates something
    /// has to say where to put it, and "my own CNode" is the usual
    /// answer.
    pub root_cnode_slot: u32,
    /// Its own TCB.
    pub tcb_slot: u32,
    /// Its own VSpace root.
    pub vspace_slot: u32,

    /// First free leaf slot, and one past the last.
    ///
    /// The root task allocates from this range. The kernel does not
    /// track what it does with them — that is the point of handing
    /// the range over rather than a slot at a time.
    pub free_slot_first: u32,
    /// One past the last free slot.
    pub free_slot_end: u32,

    /// Usable bytes the kernel counted at boot, before anything was
    /// spent.
    ///
    /// **Scenario 3's anchor.** With it the root task can do the
    /// accounting itself — sum the Untypeds, add what the kernel
    /// kept, compare — rather than being told a figure and believing
    /// it.
    pub usable_bytes: u64,

    /// Bytes the kernel kept for itself: the heap's reserve, its own
    /// page tables, the image, the stacks.
    ///
    /// Stated separately so the sum is checkable. A single
    /// "available to you" number would be unfalsifiable.
    pub kernel_bytes: u64,

    /// Bytes that are in **no** Untyped and were not kept by the
    /// kernel either — memory nothing can ever reach again.
    ///
    /// **This field exists because the honest answer is not always
    /// zero**, and discovering that is what M12's accounting is for.
    ///
    /// `cap::boot` turns free memory into Untypeds of `2^k` bytes at
    /// `2^k` boundaries (ADR 020). A free frame that cannot be fitted
    /// into such a block — or that arrives after the Untyped CNode is
    /// full — is in no Untyped at all, and `retire()` then removes
    /// the only other way to reach it. It is not leaked in the
    /// ordinary sense; it is *unnameable*, which is worse, because no
    /// amount of revoking brings it back.
    ///
    /// Reporting it separately is the difference between a sum that
    /// balances and a sum that balances **for the right reason**.
    /// Folding it into [`kernel_bytes`](Self::kernel_bytes) would make
    /// the root task's check pass while the memory was still gone,
    /// which is precisely the unfalsifiable arithmetic ADR 044
    /// refused.
    pub lost_bytes: u64,

    /// Leaf index of the one IOPortControl capability (M13, ADR 045).
    ///
    /// Appended at the end, after [`lost_bytes`](Self::lost_bytes),
    /// because every field above it is already an ABI. `header_size`
    /// is what lets an older root task notice it is reading a newer
    /// header and skip to the descriptors correctly.
    ///
    /// Authority to hand out I/O port ranges. One exists, created at
    /// boot; nothing can retype a second, which is what makes the
    /// range table's one-owner rule meaningful.
    pub ioport_control_slot: u32,

    /// Which acceptance scenario the kernel was started for.
    ///
    /// Zero means the ordinary one. M13's negatives need the root
    /// task to behave differently — kill its driver, start a second,
    /// hand a component authority it should not have — and the root
    /// task is a separate binary with no other way to be told.
    ///
    /// **This is a test affordance and is written down as one.** The
    /// alternative was four root-task images selected at build time,
    /// which would mean four things to keep in step and three of them
    /// exercised only by one scenario each. One image that branches
    /// is less code and, more importantly, makes the common path the
    /// one every scenario runs through.
    ///
    /// It also happens to occupy the padding `#[repr(C)]` would have
    /// inserted here anyway, so it costs nothing — but it is named
    /// rather than left as `reserved`, because a reserved field
    /// quietly given a meaning is how an ABI stops being one.
    pub scenario: u32,

    // ---- Framebuffer (M14, ADR 049) ---------------------------------
    //
    // Appended, like everything before them. The geometry is copied
    // out of Limine's response during boot, before the reclaim that
    // makes that response unreadable (ADR 013).
    /// Leaf index of the framebuffer's device Untyped, or **0 if the
    /// machine has no framebuffer**.
    ///
    /// Zero is a real answer rather than an error. A machine booted
    /// headless over serial has no framebuffer, and everything from
    /// M0 to M13 worked that way — so "no screen" must stay a
    /// condition the root task reports rather than one it fails on.
    ///
    /// The Untyped is **device memory**: it may only be retyped into
    /// Frames (ADR 017, ADR 050). Several consecutive slots may cover
    /// one framebuffer, because an Untyped is `2^k` bytes at a `2^k`
    /// boundary and a screen rarely is; this names the first.
    pub framebuffer_untyped_slot: u32,
    /// Visible width in pixels.
    pub framebuffer_width: u32,
    /// Visible height in pixels.
    pub framebuffer_height: u32,
    /// **Bytes** per scanline.
    ///
    /// Not `width * bpp / 8`. Hardware pads rows to an alignment it
    /// likes, so the first pixel of row `y` is at `y * pitch`, and
    /// the bytes between `width * bpp / 8` and `pitch` belong to
    /// nobody.
    ///
    /// On QEMU the two are usually equal, which makes this the most
    /// dangerous field in the struct: code that multiplies instead of
    /// using it works perfectly until it meets real hardware. M14's
    /// test checks the row boundary for exactly this reason.
    pub framebuffer_pitch: u32,
    /// **Bits** per pixel, as Limine reports it. Typically 32.
    pub framebuffer_bpp: u32,
    /// Physical address of the first byte of the framebuffer.
    ///
    /// Reported so a component can check that what it mapped is what
    /// the kernel was told about. It is not an authority: mapping it
    /// requires the Untyped above, and a bare address is a number
    /// rather than a capability (ADR 050).
    pub framebuffer_address: u64,
}
/// Report channels for `Echo`.
///
/// **One place, because three was a hazard and it fired.** M13 had
/// these numbers written out in the kernel's test, in the root task
/// and in the driver. The hostile component reported on bit 28 while
/// the test listened on bit 31, so three refusals happened, were
/// reported, and were recorded nowhere — the scenario failed saying
/// the attempts never occurred.
///
/// The note left behind then said the right fix was for this to live
/// in the ABI crate, which both sides already depend on. M14 adds a
/// second component, which would have made it four copies rather
/// than three; so it is here instead.
///
/// `Echo` names no capability, which is what frees its CPtr argument
/// to carry a channel number — and is why a test channel costs no
/// debug syscall the kernel would then own forever (ADR 044).
pub mod channel {
    /// The root task's own reports. The low byte is the step number.
    pub const ROOT: u64 = 1 << 28;
    /// The root task finished.
    pub const ROOT_DONE: u64 = 1 << 29;
    /// The root task's panic handler ran.
    pub const ROOT_PANIC: u64 = 1 << 30;
    /// The serial driver and its clients (M13).
    pub const DRIVER: u64 = 1 << 31;
    /// The screen component (M14).
    pub const SCREEN: u64 = 1 << 27;
    /// The keyboard component (M15).
    ///
    /// **Bit 26, and the first two choices were both taken.** The
    /// keyboard was written with bit 30 and the console with bit 29,
    /// which are [`ROOT_PANIC`] and [`ROOT_DONE`] — so every key
    /// event would have registered as the root task panicking, and
    /// the scenario would have failed somewhere with no visible
    /// relationship to the keyboard.
    ///
    /// Found by reading this module rather than by running the test,
    /// which is luck: the components name their channels as bare
    /// constants in their own source, so nothing checks that two
    /// agree or that one is free.
    ///
    /// The real fix is for every component to take its channel from
    /// here. ADR 047 said so in M13 and it has not been done; the
    /// cost of not doing it has now been paid twice.
    pub const KEYBOARD: u64 = 1 << 26;
    /// The console component (M15).
    pub const CONSOLE: u64 = 1 << 25;

    /// A component's panic handler ran: its channel OR this.
    ///
    /// `0xFF` rather than another bit, because the low byte is the
    /// step and 255 is not a step any component reports.
    pub const PANIC_STEP: u64 = 0xFF;
}

/// Values for [`BootInfo::scenario`].
///
/// **Zero keeps M12's meaning**, and that was a correction rather
/// than a first instinct. The obvious numbering made the new
/// milestone's scenario the default, which would have changed what
/// an unmodified root task does — and M12's bootstrap protocol is
/// something this milestone is supposed to leave alone. A new field
/// whose zero value means "behave as before" is the only kind that
/// can be added to a live ABI without reading every caller.
pub mod scenario {
    /// M12's: build two children and let them exchange a message.
    ///
    /// The default, because it is what a root task did before this
    /// field existed.
    pub const ROOT_TEST: u32 = 0;
    /// Build a serial driver and a client and let them talk.
    pub const DRIVER_TEST: u32 = 1;
    /// Build a driver, then revoke it and show the system survives.
    pub const DRIVER_DIES: u32 = 2;
    /// As [`DRIVER_DIES`], then build a second driver in its place.
    pub const DRIVER_RESTART: u32 = 3;
    /// Give a component no port or IRQ authority and watch it try.
    pub const DRIVER_HOSTILE: u32 = 4;

    // ---- M14 ---------------------------------------------------------
    /// Build a screen component and let it draw.
    pub const FB_TEST: u32 = 5;
    /// A component with no framebuffer capability tries to reach it.
    pub const FB_NO_CAP: u32 = 6;
    /// The screen component writes past the end of the framebuffer.
    pub const FB_OUT_OF_BOUNDS: u32 = 7;
    /// The screen component tries to retype a TCB from device memory.
    pub const FB_DEVICE_UNTYPED_MISUSE: u32 = 8;

    // ---- M15 ---------------------------------------------------------
    //
    // Appended, like every block before them. The numbers are frozen:
    // a component that has learned what 8 means must keep being right
    // about it.
    //
    // **The screen component does not run in any of these.** It is
    // M14's acceptance artefact rather than a display service (zero
    // `Recv`), so the console maps the framebuffer itself and the two
    // never share a scenario — see ADR 054.

    /// Build a keyboard and a console, inject keys, read the pixels
    /// back.
    pub const KBD_TEST: u32 = 9;
    /// The keyboard component is built without its IOPort
    /// capabilities and tries to reach the device anyway.
    pub const KBD_NO_CAP: u32 = 10;
    /// The keyboard is killed mid-stream; the console and the system
    /// carry on.
    pub const KBD_DRIVER_DIES: u32 = 11;
    /// Keys arrive faster than the console consumes them.
    ///
    /// What overflows is the **i8042's own queue**, not a buffer in
    /// the driver: `Send` blocks, so the driver cannot accumulate
    /// (ADR 053). The device reports the overrun and the driver
    /// counts it.
    pub const KBD_FLOOD: u32 = 12;
    /// Each component tries to reach the other's hardware.
    ///
    /// Two claims in one scenario: the keyboard touching the
    /// framebuffer's address takes a #PF, the console naming an
    /// IOPort gets `EmptySlot`. Different mechanisms — paging and
    /// the capability model — so both are shown.
    pub const KBD_ISOLATION: u32 = 13;
    /// Enough keys to fill the screen and scroll it.
    pub const KBD_SCROLL: u32 = 14;
}

/// One Untyped the root task holds. 16 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UntypedDesc {
    /// Physical base address.
    ///
    /// The root task cannot dereference this — it holds no mapping
    /// for it and physical addresses are not virtual ones. It is
    /// here so that a memory *manager* can reason about adjacency
    /// and about which Untyped covers a device's MMIO range.
    pub base: u64,
    /// Size as a power of two: the region is `1 << size_bits` bytes.
    pub size_bits: u8,
    /// Non-zero if this is device memory, which may only become a
    /// Frame (ADR 020).
    pub is_device: u8,
    /// Zero.
    pub reserved: [u8; 6],
}

impl UntypedDesc {
    /// Size in bytes.
    pub const fn size(&self) -> u64 {
        1u64 << self.size_bits
    }
}

/// Offset of the descriptor array within the BootInfo page.
///
/// A constant rather than `size_of::<BootInfo>()`, because the ABI
/// fixes it: a later kernel growing the header must not move the
/// descriptors for an older reader. `header_size` reports the real
/// header size; this is where the array begins.
pub const BOOTINFO_UNTYPED_OFFSET: usize = 128;

// The header must fit in front of the descriptors, and the whole
// thing in one page. Checked here rather than in the kernel so that
// a component depending on this crate gets the same guarantee.
const _: () = assert!(core::mem::size_of::<BootInfo>() <= BOOTINFO_UNTYPED_OFFSET);
const _: () = assert!(core::mem::size_of::<UntypedDesc>() == 16);
const _: () = assert!(BOOTINFO_UNTYPED_OFFSET + BOOTINFO_MAX_UNTYPED * 16 <= 4096);

// ---- The syscall ABI (ADR 030, ADR 043) --------------------------------

/// What a component may ask of a capability.
///
/// One syscall, and the method is an argument (ADR 017). These
/// numbers are frozen: **append only**.
pub mod method {
    /// Report what a thread is doing. Takes a TCB.
    pub const READ_REGISTERS: u64 = 1;
    /// Store a priority. Takes a TCB.
    pub const SET_PRIORITY: u64 = 2;
    /// Say hello. Names no capability.
    pub const ECHO: u64 = 3;
    /// Block until a Notification is signalled.
    pub const WAIT: u64 = 4;
    /// Map a Frame into an address space.
    pub const FRAME_MAP: u64 = 5;
    /// Remove a mapping. Targets the **VSpace**, not the Frame.
    pub const FRAME_UNMAP: u64 = 6;
    /// Supply an intermediate page table.
    pub const PAGE_TABLE_MAP: u64 = 7;
    /// Take one back out. Targets the VSpace.
    pub const PAGE_TABLE_UNMAP: u64 = 8;
    /// Turn memory into objects.
    pub const RETYPE: u64 = 9;
    /// Give a thread an address space and a CSpace.
    pub const SET_SPACE: u64 = 10;
    /// Make a configured thread runnable.
    pub const RESUME: u64 = 11;
    /// Turn a fresh page table into an address space.
    pub const CREATE_VSPACE: u64 = 12;
    /// Entry point and stack for a ring-3 thread.
    pub const SET_USER_REGISTERS: u64 = 13;
    /// Send a message. **Blocks** until a receiver takes it.
    pub const SEND: u64 = 14;
    /// Wait for a message. **Blocks.** Returns the sender's badge.
    pub const RECV: u64 = 15;
    /// Copy an **Endpoint or Notification** into another CNode, with
    /// a badge attached.
    ///
    /// **The invocation that connects two components.** Without it
    /// a component can create capabilities and never give one away,
    /// which is isolation without composition.
    ///
    /// Only the two badgeable types: a badge lives in a field that
    /// means `radix` on a CNode and `size_bits` on an Untyped, so
    /// minting either would overwrite the number that bounds it. Use
    /// [`COPY`] for everything else.
    pub const MINT: u64 = 16;
    /// Copy **any** capability into another CNode, unbadged.
    ///
    /// The more basic of the two hand-over operations, and the one
    /// that was missing: M12 added `Mint` as "the way to give a
    /// capability away" and then found that giving away a *CNode* —
    /// which is how a child gets a CSpace — needs no badge and must
    /// not have one.
    ///
    /// Rights may be narrowed but never widened (ADR 021).
    pub const COPY: u64 = 17;

    // ---- M13: the first user-space driver (ADR 045) ----
    //
    // Appended, as every block before them was. A component that has
    // learned what 17 means must keep being right about it.
    /// `IRQControl.Get`: claim an interrupt line (ADR 041).
    ///
    /// **M10 built this and never opened it**, the same shape as
    /// `Send` in M12 and `CreateVSpace` in M11: a function that took
    /// capabilities, checked rights, and had no door. M13 needs it,
    /// because the driver is a component and its line has to be
    /// claimed from ring 3.
    pub const IRQ_CONTROL_GET: u64 = 18;
    /// `IRQHandler.SetNotification`: where this line's interrupts go.
    pub const IRQ_SET_NOTIFICATION: u64 = 19;
    /// `IRQHandler.Ack`: the driver is finished; reopen the line.
    pub const IRQ_ACK: u64 = 20;
    /// `IOPortControl.Issue`: carve out a port range (ADR 045).
    pub const IOPORT_ISSUE: u64 = 21;
    /// `IOPort.In8`: read a byte from a port in range.
    pub const IOPORT_IN8: u64 = 22;
    /// `IOPort.Out8`: write a byte to a port in range.
    pub const IOPORT_OUT8: u64 = 23;
    /// `Notification.Bind`: deliver this Notification's signals to a
    /// thread even while it waits on an Endpoint (ADR 038).
    ///
    /// What makes a driver able to wait for **both** a client request
    /// and its device at once, which is the shape every driver has.
    pub const NOTIFICATION_BIND: u64 = 24;
    /// `TCB.Suspend`: stop a thread without destroying it.
    ///
    /// Needed by `driver-dies`: a TCB cannot be revoked while the
    /// scheduler holds it (ADR 026), so killing a driver is suspend
    /// then revoke.
    pub const TCB_SUSPEND: u64 = 25;
    /// `CNode.Revoke`: delete every capability derived from this one.
    ///
    /// The invocation `driver-dies` and `driver-restart` turn on.
    /// Targets the **CNode** holding the capability, with the index
    /// as an argument, for the reason `Mint` does: the authority to
    /// revoke lives where the capability lives.
    pub const CNODE_REVOKE: u64 = 26;
    /// `CNode.Delete`: remove one capability.
    pub const CNODE_DELETE: u64 = 27;
}

/// Object types, as `Retype` names them.
///
/// Deliberately **not** the kernel's `CapType` discriminants: these
/// are a user-visible ABI and inheriting them from an internal enum
/// would mean an internal renumbering silently changed what a
/// component asks for.
pub mod cap_type {
    /// Memory that has not been given a type yet.
    pub const UNTYPED: u64 = 1;
    /// An array of capability slots.
    pub const CNODE: u64 = 2;
    /// A thread.
    pub const TCB: u64 = 3;
    /// A synchronous rendezvous point.
    pub const ENDPOINT: u64 = 4;
    /// An asynchronous signal.
    pub const NOTIFICATION: u64 = 5;
    /// A page of memory.
    pub const FRAME: u64 = 6;
    /// A page table, at any level.
    pub const PAGE_TABLE: u64 = 7;
    // There is deliberately no `IRQ_HANDLER` or `IO_PORT` here.
    //
    // Both objects exist and both have a size, so `retype` *could*
    // produce them — which is exactly why their absence is written
    // down rather than left to the reader. An IRQHandler comes from
    // `IRQControl.Get` and an IOPort from `IOPortControl.Issue`,
    // because what they name is a line and a port range rather than
    // memory. One conjured from an Untyped would name nothing, and
    // the one-owner rule would have been bypassed rather than
    // enforced (ADR 041, ADR 045).
}

/// Mapping permissions (ADR 032).
///
/// Three values, and there is **no fourth**: writable-and-executable
/// has no encoding, so W^X is unrepresentable rather than merely
/// forbidden.
pub mod perm {
    /// Readable and writable, **write-combining** (M14, ADR 051).
    ///
    /// For a framebuffer and nothing else so far. Writes gather in a
    /// buffer and leave in bursts, which is right for a surface where
    /// ordering between pixels does not matter and wrong for a
    /// control register, where it does — those use the kernel's
    /// `NO_CACHE`, which is not exposed here because no component has
    /// ever needed to ask for it.
    ///
    /// Appended after [`READ_EXECUTE`], because these numbers are an
    /// ABI. Listed first in the source only because the doc comment
    /// is long and the others are one-liners.
    ///
    /// The cache type is inert unless the kernel has programmed PAT
    /// entry 4, which `cpu::init_pat` does at boot. A mapping made
    /// with this on a kernel that had not would be write-back:
    /// correct pixels, invisible until something flushed them.
    pub const READ_WRITE_COMBINING: u64 = 3;

    /// Readable. Not writable, not executable.
    pub const READ_ONLY: u64 = 0;
    /// Readable and writable. Not executable.
    pub const READ_WRITE: u64 = 1;
    /// Readable and executable. **Not writable.**
    pub const READ_EXECUTE: u64 = 2;
}

/// Bit 63 of a syscall's return value: this is an error.
///
/// One register, two outcomes, 63 bits left for a result.
pub const ERROR_FLAG: u64 = 1 << 63;

/// Error codes, as the kernel encodes them. **Append only.**
pub mod error {
    /// The reserved bits of a CPtr were not zero.
    pub const INVALID_CPTR: u64 = 1;
    /// An index was past the end of a CNode.
    pub const CPTR_OUT_OF_RANGE: u64 = 2;
    /// The slot holds nothing.
    pub const EMPTY_SLOT: u64 = 3;
    /// The slot has been reused since the pointer was issued.
    pub const STALE_CPTR: u64 = 4;
    /// A root CSpace slot held something other than a CNode.
    pub const ROOT_SLOT_MUST_BE_CNODE: u64 = 5;
    /// The capability is not of the type the method needs.
    pub const WRONG_TYPE: u64 = 6;
    /// The capability does not carry the rights needed.
    pub const INSUFFICIENT_RIGHTS: u64 = 7;
    /// A copy asked for rights the source does not hold.
    pub const RIGHTS_ESCALATION: u64 = 8;
    /// The destination slot is occupied.
    pub const SLOT_NOT_EMPTY: u64 = 9;
    /// The derivation chain is at its depth limit.
    pub const DERIVATION_TOO_DEEP: u64 = 10;
    /// **The caller's own Untyped is spent.**
    ///
    /// Distinct from [`NO_KERNEL_STACK`], which means a kernel-wide
    /// pool ran out. A component can exhaust the first and only the
    /// first (ADR 022).
    pub const NOT_ENOUGH_MEMORY: u64 = 11;
    /// A retype asked for more objects than the limit allows.
    pub const TOO_MANY_OBJECTS: u64 = 12;
    /// Device memory may only become a Frame.
    pub const DEVICE_UNTYPED_FRAME_ONLY: u64 = 13;
    /// The thread is running or ready; suspend it first.
    pub const THREAD_BUSY: u64 = 14;
    /// The address is not in the user half.
    pub const NOT_USER_ADDRESS: u64 = 15;
    /// The page flags disagree with the half the address is in.
    pub const FLAGS_WRONG_HALF: u64 = 16;
    /// An intermediate page table is missing. Supply one.
    pub const MISSING_PAGE_TABLE: u64 = 17;
    /// Something is already mapped there.
    pub const ALREADY_MAPPED: u64 = 18;
    /// Nothing is mapped there.
    pub const NOT_MAPPED: u64 = 19;
    /// The VSpace is the one currently in CR3.
    pub const VSPACE_IN_USE: u64 = 20;
    /// A malformed argument.
    pub const INVALID_ARGUMENT: u64 = 21;
    /// A message claimed more than 128 words.
    pub const MESSAGE_TOO_LONG: u64 = 22;
    /// A message claimed more than four capabilities.
    pub const TOO_MANY_CAPS: u64 = 23;
    /// The endpoint was revoked while this thread waited on it.
    pub const ENDPOINT_DEAD: u64 = 24;
    /// `Reply` found nobody waiting.
    pub const NO_REPLY_TARGET: u64 = 25;
    /// The caller is gone, or no longer waiting.
    pub const REPLY_TARGET_GONE: u64 = 26;
    /// A Notification already has a waiter, or is already bound.
    pub const NOTIFICATION_BUSY: u64 = 27;
    /// Already bound.
    pub const ALREADY_BOUND: u64 = 28;
    /// Somebody else owns that interrupt line.
    pub const IRQ_ALREADY_OWNED: u64 = 29;
    /// **A kernel-wide pool ran out**, not the caller's budget.
    pub const NO_KERNEL_STACK: u64 = 30;
    /// Part of the requested port range already has an owner
    /// (M13, ADR 045).
    pub const IO_PORTS_ALREADY_OWNED: u64 = 31;
    /// The port is outside this capability's range (M13, ADR 045).
    ///
    /// **Scenario 5's error code.** Distinct from
    /// [`INSUFFICIENT_RIGHTS`]: the caller holds a perfectly good
    /// IOPort capability with the right bits, it simply does not
    /// cover the port asked for. Collapsing the two would make the
    /// test unable to say which guard refused it.
    pub const PORT_NOT_IN_RANGE: u64 = 32;
}

/// Whether a returned value encodes an error.
pub const fn is_error(value: u64) -> bool {
    value & ERROR_FLAG != 0
}

/// The error code out of a returned value.
pub const fn error_code(value: u64) -> u64 {
    value & !ERROR_FLAG
}

// ---- Capability pointers (ADR 018) -------------------------------------

/// A capability pointer: two indices and two generations.
///
/// ```text
///  63      56 55      48 47            32 31            16 15          0
/// +----------+----------+----------------+---------------+-------------+
/// | L1 gen   | L0 gen   | reserved, zero |   L1 index    |  L0 index   |
/// +----------+----------+----------------+---------------+-------------+
/// ```
///
/// **A CPtr is meaningful only relative to a root.** The same bits
/// name different objects in different CSpaces — which is what makes
/// a capability outside a component's CSpace unnameable rather than
/// forbidden, and is also a trap for anyone who assumes a foreign
/// pointer means nothing. It means whatever is at those indices
/// there.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CPtr(pub u64);

impl CPtr {
    /// Build a pointer from its parts.
    pub const fn new(l1_index: u16, l1_generation: u8, l0_index: u16, l0_generation: u8) -> CPtr {
        CPtr(
            ((l1_generation as u64) << 56)
                | ((l0_generation as u64) << 48)
                | ((l1_index as u64) << 16)
                | (l0_index as u64),
        )
    }

    /// The leaf index.
    pub const fn l0_index(self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }
}

/// The most objects one `Retype` may create.
///
/// **Part of the ABI because a component has to know it.** Retype is
/// bounded so that a single invocation cannot occupy the CPU for an
/// unbounded time (ADR 020), and the bound is not discoverable: a
/// component that asks for more gets `TOO_MANY_OBJECTS` and has no
/// way to learn what would have been allowed except by searching.
///
/// The screen component needs a thousand Frames and found this out
/// the hard way — it asked for all of them at once, was refused, and
/// its retry loop read the refusal as "this Untyped is spent". The
/// figure belongs here, where both sides read it from one place.
pub const RETYPE_MAX_COUNT: u64 = 256;

/// Pack `Retype`'s type and size into one argument word.
///
/// Two fields in one register because they are one answer: "what
/// object". Unlike packing a value into an address's spare bits
/// (which ADR 043 rejected), neither of these is an address and
/// neither is meaningful alone.
pub const fn retype_arg(cap_type: u64, size_bits: u64) -> u64 {
    cap_type | (size_bits << 8)
}
