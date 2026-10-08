# SPDX-License-Identifier: BSD-2-Clause
#
# Safrox — M0 build system.
#
#   make            Build the kernel and both ISO images.
#   make run        Boot the normal ISO; expects QEMU host status 33.
#   make run-panic  Boot the panic ISO;  expects QEMU host status 35.
#   make test       Run both, plus the runner's own self-test.
#   make debug      Boot the normal ISO and wait for GDB on :1234.
#   make clean      Remove build artefacts.
#   make distclean  Also remove the vendored Limine checkout.
#
# NOTE: do not add RUSTFLAGS here. Cargo replaces, rather than merges,
# `[target.*.rustflags]` from .cargo/config.toml when RUSTFLAGS is set in the
# environment -- which would silently drop the linker script and produce a
# kernel linked at address 0. Warning levels live in Cargo.toml's [lints].

CARGO   ?= cargo
QEMU    ?= qemu-system-x86_64
PYTHON  ?= python3

PROFILE     ?= release
CARGO_FLAGS := --profile $(PROFILE)

# ---- Layout ------------------------------------------------------------

TARGET      := x86_64-unknown-none
CARGO_OUT   := target/$(TARGET)/$(PROFILE)/safrox

BUILD_DIR   := build
ISO_ROOT    := $(BUILD_DIR)/iso_root
KERNEL      := $(BUILD_DIR)/kernel.elf
ISO         := $(BUILD_DIR)/safrox.iso
ISO_PANIC   := $(BUILD_DIR)/safrox-panic.iso
# M1 fault-injection images. Same kernel.elf, different Limine default_entry.
ISO_INT3    := $(BUILD_DIR)/safrox-int3.iso
ISO_PF      := $(BUILD_DIR)/safrox-pf.iso
ISO_DF      := $(BUILD_DIR)/safrox-df.iso
# M2 frame allocator images.
ISO_MEMTEST := $(BUILD_DIR)/safrox-memtest.iso
ISO_DFREE   := $(BUILD_DIR)/safrox-doublefree.iso
# M3 virtual memory images.
ISO_VMTEST   := $(BUILD_DIR)/safrox-vmtest.iso
ISO_STALETLB := $(BUILD_DIR)/safrox-staletlb.iso
ISO_GUARD    := $(BUILD_DIR)/safrox-guardpage.iso
ISO_NX       := $(BUILD_DIR)/safrox-nx.iso
ISO_STACKOVF := $(BUILD_DIR)/safrox-stackovf.iso
# M4 kernel heap images.
ISO_HEAPTEST  := $(BUILD_DIR)/safrox-heaptest.iso
ISO_HEAPOOM   := $(BUILD_DIR)/safrox-heapoom.iso
ISO_HEAPGUARD := $(BUILD_DIR)/safrox-heapguard.iso
ISO_CAPTEST := $(BUILD_DIR)/safrox-captest.iso
ISO_CAPREVOKE := $(BUILD_DIR)/safrox-caprevoke.iso
ISO_CAPINTALLOC := $(BUILD_DIR)/safrox-capintalloc.iso
ISO_CAPRETIRED := $(BUILD_DIR)/safrox-capretired.iso
ISO_THREADTEST := $(BUILD_DIR)/safrox-threadtest.iso
ISO_THREADOVF := $(BUILD_DIR)/safrox-threadovf.iso
ISO_THREADINTALLOC := $(BUILD_DIR)/safrox-threadintalloc.iso
ISO_THREADKILL := $(BUILD_DIR)/safrox-threadkill.iso
# M7 address space images.
ISO_VSPACETEST := $(BUILD_DIR)/safrox-vspacetest.iso
ISO_VSKERNEL := $(BUILD_DIR)/safrox-vskernel.iso
ISO_VSISOLATION := $(BUILD_DIR)/safrox-vsisolation.iso
ISO_VSSTALE := $(BUILD_DIR)/safrox-vsstale.iso
# M8 ring 3 images.
ISO_USERTEST := $(BUILD_DIR)/safrox-usertest.iso
ISO_USERPRIV := $(BUILD_DIR)/safrox-userpriv.iso
ISO_USERKREAD := $(BUILD_DIR)/safrox-userkread.iso
ISO_USERSMEP := $(BUILD_DIR)/safrox-usersmep.iso
ISO_USERSMAP := $(BUILD_DIR)/safrox-usersmap.iso
ISO_USERFORGE := $(BUILD_DIR)/safrox-userforge.iso
# M9a IPC images.
ISO_IPCTEST := $(BUILD_DIR)/safrox-ipctest.iso
ISO_IPCCANCEL := $(BUILD_DIR)/safrox-ipccancel.iso
ISO_IPCREVOKED := $(BUILD_DIR)/safrox-ipcrevoked.iso
# M9b call/reply images.
ISO_IPCCALL := $(BUILD_DIR)/safrox-ipccall.iso
ISO_IPCDEAD := $(BUILD_DIR)/safrox-ipcdead.iso
ISO_IPCSTALE := $(BUILD_DIR)/safrox-ipcstale.iso
ISO_IPCSELF := $(BUILD_DIR)/safrox-ipcself.iso
ISO_IPCCOST := $(BUILD_DIR)/safrox-ipccost.iso
ISO_IPCCAP := $(BUILD_DIR)/safrox-ipccap.iso
ISO_IPCCAPHOSTILE := $(BUILD_DIR)/safrox-ipccaphostile.iso
ISO_IPCNTFNREV := $(BUILD_DIR)/safrox-ipcntfnrev.iso
ISO_IPCCAPCOST := $(BUILD_DIR)/safrox-ipccapcost.iso

ALL_ISOS    := $(ISO) $(ISO_PANIC) $(ISO_INT3) $(ISO_PF) $(ISO_DF) \
               $(ISO_MEMTEST) $(ISO_DFREE) \
               $(ISO_VMTEST) $(ISO_STALETLB) $(ISO_GUARD) $(ISO_NX) \
               $(ISO_STACKOVF) \
               $(ISO_HEAPTEST) $(ISO_HEAPOOM) $(ISO_HEAPGUARD) \
               $(ISO_CAPTEST) $(ISO_CAPREVOKE) $(ISO_CAPINTALLOC) \
               $(ISO_CAPRETIRED) $(ISO_THREADTEST) $(ISO_THREADOVF) \
               $(ISO_THREADINTALLOC) $(ISO_THREADKILL) \
               $(ISO_VSPACETEST) $(ISO_VSKERNEL) $(ISO_VSISOLATION) \
               $(ISO_VSSTALE) \
               $(ISO_USERTEST) $(ISO_USERPRIV) $(ISO_USERKREAD) \
               $(ISO_USERSMEP) $(ISO_USERSMAP) $(ISO_USERFORGE) \
               $(ISO_IPCTEST) $(ISO_IPCCANCEL) $(ISO_IPCREVOKED) \
               $(ISO_IPCCALL) $(ISO_IPCDEAD) $(ISO_IPCSTALE) \
               $(ISO_IPCSELF) $(ISO_IPCCOST)

LIMINE_DIR  := third_party/limine
LIMINE_EXE  := $(LIMINE_DIR)/limine
LIMINE_REPO := https://github.com/limine-bootloader/limine.git
# Pinned to the v9.x binary branch: the boot protocol revision the kernel
# requests (3) must match what the shipped bootloader blobs implement.
LIMINE_TAG  := v9.x-binary

# Every Rust source, at any depth. The flat `kernel/src/*.rs` this used to
# be stopped being correct the moment M3 added `kernel/src/memory/`: Make
# saw no change when a file in that directory was edited, skipped the
# recipe, and left the previous kernel.elf in place. Cargo rebuilt the
# binary anyway -- it does its own dependency tracking -- so the symptom was
# an ISO built from a stale copy and a test run that disagreed with the
# source in front of you.
RUST_SRCS := $(shell find kernel/src -name '*.rs')

# ---- QEMU --------------------------------------------------------------
#
# -no-reboot   a triple fault terminates the VM instead of resetting it, so a
#              broken kernel fails fast instead of looping forever
# -no-shutdown leaves the VM paused rather than exiting on guest-initiated
#              shutdown, so the only way out is isa-debug-exit
# isa-debug-exit turns a guest I/O write into a host process exit status;
#              see kernel/src/qemu.rs for the (value << 1) | 1 encoding

# The CPU model is `qemu64` plus two features it does not advertise by
# default. M3 enabled SMEP and SMAP from CPUID and found neither present,
# so the detect-and-warn path ran but the protection itself was never
# exercised. Asking for them explicitly means `cpu::init` reports
# "enabled" and the bits are actually in CR4 -- which is the state M8 will
# depend on.
#
# `+smep,+smap` rather than `-cpu max`: max pulls in everything the host
# and QEMU version happen to support, which makes the test environment
# depend on the developer's machine. Naming the two features keeps the
# run reproducible.
QEMU_FLAGS := \
	-M q35 \
	-m 256M \
	-cpu qemu64,+smep,+smap \
	-no-reboot \
	-no-shutdown \
	-device isa-debug-exit,iobase=0xf4,iosize=0x04 \
	-serial stdio \
	-display none

# `build` is listed here even though no such target exists, and that is the
# point: `build/` is a *directory*, so without .PHONY an accidental
# `make build` reports "Nothing to be done" and looks like a successful
# no-op build. It happened during M3 and cost a confusing few minutes. The
# alias below makes the obvious command do the obvious thing.
.PHONY: all build kernel isos run run-panic run-int3 run-pf run-df \
        run-memtest run-doublefree \
        run-vmtest run-staletlb run-guardpage run-nx run-stackovf \
        run-heaptest run-heapoom run-heapguard \
        run-captest run-caprevoke run-capintalloc run-capretired \
        run-threadtest run-threadovf run-threadintalloc run-threadkill \
        run-vspacetest run-vskernel run-vsisolation run-vsstale \
        run-usertest run-userpriv run-userkread run-usersmep \
        run-usersmap run-userforge \
        run-ipctest run-ipccancel run-ipcrevoked \
        run-ipccall run-ipcdead run-ipcstale run-ipcself \
        run-ipccost run-ipccap run-ipccaphostile \
        run-ipcntfnrev run-ipccapcost \
        test debug clean distclean limine toolchain-check fmt clippy

# `test` depends on fmt and clippy, and those must finish before any QEMU run
# starts. Prerequisite order is only honoured in a serial build, so disable
# parallelism for this Makefile: a `make -j test` that ran clippy alongside
# the VM would report lint failures after the fact, or interleave their
# output with the serial log the runner is parsing.
.NOTPARALLEL:

all: isos

kernel: $(KERNEL)

# Alias: `make build` is what everyone types first.
build: $(KERNEL)

isos: $(ALL_ISOS)

# ---- Limine ------------------------------------------------------------

limine: $(LIMINE_EXE)

$(LIMINE_DIR)/limine.h:
	@echo "  CLONE   limine $(LIMINE_TAG)"
	@git clone --depth=1 --branch=$(LIMINE_TAG) $(LIMINE_REPO) $(LIMINE_DIR)

# The binary branch ships prebuilt bootloader blobs plus the source of the
# small host-side `limine` utility, which stamps the hybrid BIOS boot record.
$(LIMINE_EXE): $(LIMINE_DIR)/limine.h
	@echo "  MAKE    limine host tool"
	@$(MAKE) -C $(LIMINE_DIR) --no-print-directory

# ---- Build & verify ----------------------------------------------------

# M12 adds `$(ROOTTASK_BIN)` to this rule's prerequisites, in the
# block at the bottom of the file. It cannot be added *here*: see
# the note there for why, and for the silent failure that taught it.
$(KERNEL): $(RUST_SRCS) kernel/linker.ld Cargo.toml .cargo/config.toml build.rs
	@echo "  CARGO   build --profile $(PROFILE)"
	@$(CARGO) build $(CARGO_FLAGS)
	@mkdir -p $(BUILD_DIR)
	@cp $(CARGO_OUT) $@
	@echo "  CHECK   kernel ELF"
	@$(PYTHON) scripts/check_kernel.py $@

# ---- ISO images --------------------------------------------------------
#
# $1 = output ISO, $2 = Limine default_entry index (1 normal, 2 panic)
define build_iso
	@echo "  ISO     $(1)"
	@rm -rf $(ISO_ROOT)
	@mkdir -p $(ISO_ROOT)/boot/limine $(ISO_ROOT)/EFI/BOOT
	@cp $(KERNEL) $(ISO_ROOT)/boot/kernel.elf
	@sed 's/@DEFAULT_ENTRY@/$(2)/' boot/limine.conf.in > $(ISO_ROOT)/boot/limine/limine.conf
	@cp $(LIMINE_DIR)/limine-bios.sys \
	    $(LIMINE_DIR)/limine-bios-cd.bin \
	    $(LIMINE_DIR)/limine-uefi-cd.bin $(ISO_ROOT)/boot/limine/
	@cp $(LIMINE_DIR)/BOOTX64.EFI $(LIMINE_DIR)/BOOTIA32.EFI $(ISO_ROOT)/EFI/BOOT/
	@xorriso -as mkisofs -quiet \
		-b boot/limine/limine-bios-cd.bin \
		-no-emul-boot -boot-load-size 4 -boot-info-table \
		--efi-boot boot/limine/limine-uefi-cd.bin \
		-efi-boot-part --efi-boot-image --protective-msdos-label \
		$(ISO_ROOT) -o $(1) 2>/dev/null
	@$(LIMINE_EXE) bios-install $(1) >/dev/null 2>&1
	@rm -rf $(ISO_ROOT)
endef

# The index selects which entry in boot/limine.conf.in becomes the default;
# see the table at the top of that file.
$(ISO): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,1)

$(ISO_PANIC): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,2)

$(ISO_INT3): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,3)

$(ISO_PF): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,4)

$(ISO_DF): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,5)

$(ISO_MEMTEST): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,6)

$(ISO_DFREE): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,7)

$(ISO_VMTEST): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,8)

$(ISO_STALETLB): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,9)

$(ISO_GUARD): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,10)

$(ISO_NX): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,11)

$(ISO_STACKOVF): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,12)

$(ISO_HEAPTEST): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,13)

$(ISO_HEAPOOM): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,14)

$(ISO_HEAPGUARD): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,15)

$(ISO_CAPTEST): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,16)

$(ISO_CAPREVOKE): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,17)

$(ISO_CAPINTALLOC): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,18)

$(ISO_CAPRETIRED): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,19)

$(ISO_THREADTEST): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,20)

$(ISO_THREADOVF): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,21)

$(ISO_THREADINTALLOC): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,22)

$(ISO_THREADKILL): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,23)

# ---- Run & test --------------------------------------------------------

run: $(ISO)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO) --expect success -- $(QEMU_FLAGS)

run-panic: $(ISO_PANIC)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_PANIC) --expect panic -- $(QEMU_FLAGS)

# M1 exception scenarios. int3 expects status 33 because #BP is a trap and
# the handler returns; the other two expect 35.
run-int3: $(ISO_INT3)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_INT3) --expect int3 -- $(QEMU_FLAGS)

run-pf: $(ISO_PF)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_PF) --expect pf -- $(QEMU_FLAGS)

run-df: $(ISO_DF)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_DF) --expect df -- $(QEMU_FLAGS)

# M2 frame allocator scenarios.
run-memtest: $(ISO_MEMTEST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_MEMTEST) --expect memtest -- $(QEMU_FLAGS)

run-doublefree: $(ISO_DFREE)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_DFREE) --expect doublefree -- $(QEMU_FLAGS)

# M3 virtual memory scenarios.
run-vmtest: $(ISO_VMTEST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_VMTEST) --expect vmtest -- $(QEMU_FLAGS)

run-staletlb: $(ISO_STALETLB)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_STALETLB) --expect staletlb -- $(QEMU_FLAGS)

run-guardpage: $(ISO_GUARD)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_GUARD) --expect guardpage -- $(QEMU_FLAGS)

run-nx: $(ISO_NX)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_NX) --expect nx -- $(QEMU_FLAGS)

run-stackovf: $(ISO_STACKOVF)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_STACKOVF) --expect stackovf -- $(QEMU_FLAGS)

# M4 kernel heap scenarios.
run-heaptest: $(ISO_HEAPTEST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_HEAPTEST) --expect heaptest -- $(QEMU_FLAGS)

run-heapoom: $(ISO_HEAPOOM)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_HEAPOOM) --expect heapoom -- $(QEMU_FLAGS)

run-heapguard: $(ISO_HEAPGUARD)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_HEAPGUARD) --expect heapguard -- $(QEMU_FLAGS)

# M5 capability scenarios. captest and caprevoke exit 33; the two
# tripwires exit 35, because each one is a rule being enforced.
run-captest: $(ISO_CAPTEST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_CAPTEST) --expect captest -- $(QEMU_FLAGS)

run-caprevoke: $(ISO_CAPREVOKE)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_CAPREVOKE) --expect caprevoke -- $(QEMU_FLAGS)

run-capintalloc: $(ISO_CAPINTALLOC)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_CAPINTALLOC) --expect capintalloc -- $(QEMU_FLAGS)

run-capretired: $(ISO_CAPRETIRED)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_CAPRETIRED) --expect capretired -- $(QEMU_FLAGS)

# M6 thread scenarios. threadtest exits 33; the three negatives exit 35.
run-threadtest: $(ISO_THREADTEST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_THREADTEST) --expect threadtest -- $(QEMU_FLAGS)

run-threadovf: $(ISO_THREADOVF)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_THREADOVF) --expect threadovf -- $(QEMU_FLAGS)

run-threadintalloc: $(ISO_THREADINTALLOC)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_THREADINTALLOC) --expect threadintalloc -- $(QEMU_FLAGS)

run-threadkill: $(ISO_THREADKILL)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_THREADKILL) --expect threadkill -- $(QEMU_FLAGS)

# The full acceptance suite, in order:
#
#   fmt        formatting is consistent
#   clippy     lints pass, including clippy::undocumented_unsafe_blocks --
#              this is what makes the `// SAFETY:` comment on every unsafe
#              block a compiler-enforced requirement rather than a habit.
#              It must run here, not only as a standalone target, or nobody
#              will type `make clippy` and the rule stops being enforced.
#   run        normal ISO boots and exits 33
#   run-panic  same binary panics on cue and exits 35
#   run-int3   #BP is caught, reported, and *returned from*: exits 33
#   run-pf     #PF is caught with the right error code and CR2: exits 35
#   run-df     #DF is caught on the IST1 stack, kernel confirms it: exits 35
#   run-memtest     bulk alloc/free with no leak, clean exhaustion: exits 33
#   run-doublefree  a double free is detected and panics: exits 35
#   run-vmtest      own page tables, map/unmap/translate, error cases: 33
#   run-staletlb    unmap without invalidating -- advisory, see run.py: 33
#   run-guardpage   writing below a kernel stack faults: exits 35
#   run-nx          executing from a data page faults: exits 35
#   run-stackovf    recursing off the main stack hits the guard: exits 35
#   run-heaptest    Box/Vec/BTreeMap/String, 10k allocations, growth: 33
#   run-heapoom     the heap runs out and says so, loudly: exits 35
#   run-heapguard   writing above the heap's limit faults: exits 35
#   self-test  the runner itself is proven to validate: handed the panic ISO
#              while expecting success, it must FAIL. Without this, a runner
#              that accepted everything would look identical to a working one.
#
# Every scenario additionally fails if the kernel banner appears twice,
# which would mean the machine triple-faulted and rebooted.
test: fmt clippy run run-panic run-int3 run-pf run-df run-memtest run-doublefree \
      run-vmtest run-staletlb run-guardpage run-nx run-stackovf \
      run-heaptest run-heapoom run-heapguard \
      run-captest run-caprevoke run-capintalloc run-capretired \
      run-threadtest run-threadovf run-threadintalloc run-threadkill \
      run-vspacetest run-vskernel run-vsisolation run-vsstale \
      run-usertest run-userpriv run-userkread run-usersmep \
      run-usersmap run-userforge \
      run-ipctest run-ipccancel run-ipcrevoked \
        run-ipccall run-ipcdead run-ipcstale run-ipcself \
        run-ipccost run-ipccap run-ipccaphostile \
        run-ipcntfnrev run-ipccapcost
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_PANIC) \
	--expect success --self-test-expect-failure -- $(QEMU_FLAGS)
	@echo "M9a: all acceptance tests passed"

debug: $(ISO)
	@echo "GDB: target remote :1234    (symbols in $(KERNEL))"
	@$(QEMU) $(QEMU_FLAGS) -cdrom $(ISO) -boot d -s -S

# ---- Housekeeping ------------------------------------------------------

toolchain-check:
	@$(PYTHON) scripts/check_toolchain.py

fmt:
	@echo "  FMT     check"
	@$(CARGO) fmt --check

# Lint levels come from Cargo.toml's [lints] table, which composes correctly
# with the linker flags in .cargo/config.toml. `-D warnings` is passed here
# as a clippy argument (after `--`), not through RUSTFLAGS, so it cannot
# replace those flags.
clippy:
	@echo "  CLIPPY  lint"
	@$(CARGO) clippy $(CARGO_FLAGS) --all-targets -- -D warnings

clean:
	@$(CARGO) clean
	@rm -rf $(BUILD_DIR)
	@echo "  CLEAN   target/ $(BUILD_DIR)"

distclean: clean
	@rm -rf $(LIMINE_DIR)
	@echo "  CLEAN   $(LIMINE_DIR)"
# ---- M7 address space images -------------------------------------------

$(ISO_VSPACETEST): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,24)

$(ISO_VSKERNEL): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,25)

$(ISO_VSISOLATION): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,26)

$(ISO_VSSTALE): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,27)

# M7 address space scenarios. vspacetest and vskernel exit 33 -- the
# second because a refused mapping is a caller error, not a kernel
# fault, and the kernel must still be running afterwards. The other
# two exit 35.
run-vspacetest: $(ISO_VSPACETEST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_VSPACETEST) --expect vspacetest -- $(QEMU_FLAGS)

run-vskernel: $(ISO_VSKERNEL)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_VSKERNEL) --expect vskernel -- $(QEMU_FLAGS)

run-vsisolation: $(ISO_VSISOLATION)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_VSISOLATION) --expect vsisolation -- $(QEMU_FLAGS)

run-vsstale: $(ISO_VSSTALE)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_VSSTALE) --expect vsstale -- $(QEMU_FLAGS)

# ---- M8 ring 3 images --------------------------------------------------

$(ISO_USERTEST): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,28)

$(ISO_USERPRIV): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,29)

$(ISO_USERKREAD): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,30)

$(ISO_USERSMEP): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,31)

$(ISO_USERSMAP): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,32)

$(ISO_USERFORGE): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,33)

# M8 ring 3 scenarios. usertest and userforge exit 33 -- the second
# because a refused invocation is a caller error and the kernel must
# still be running afterwards. The other four exit 35.

run-usertest: $(ISO_USERTEST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_USERTEST) --expect usertest -- $(QEMU_FLAGS)

run-userpriv: $(ISO_USERPRIV)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_USERPRIV) --expect userpriv -- $(QEMU_FLAGS)

run-userkread: $(ISO_USERKREAD)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_USERKREAD) --expect userkread -- $(QEMU_FLAGS)

run-usersmep: $(ISO_USERSMEP)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_USERSMEP) --expect usersmep -- $(QEMU_FLAGS)

run-usersmap: $(ISO_USERSMAP)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_USERSMAP) --expect usersmap -- $(QEMU_FLAGS)

run-userforge: $(ISO_USERFORGE)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_USERFORGE) --expect userforge -- $(QEMU_FLAGS)


# ---- M9a IPC images ----------------------------------------------------

$(ISO_IPCTEST): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,34)

$(ISO_IPCCANCEL): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,35)

$(ISO_IPCREVOKED): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,36)

# M9a IPC scenarios. All three exit 33, and the last one is the
# interesting case: three threads wake with an error and the kernel
# carries on, which is a caller-visible outcome rather than a fault.
# A kernel that died there would be worse than one that refused the
# delete -- it would be a denial of service reachable from any
# component that can park a thread on an endpoint.

run-ipctest: $(ISO_IPCTEST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IPCTEST) --expect ipctest -- $(QEMU_FLAGS)

run-ipccancel: $(ISO_IPCCANCEL)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IPCCANCEL) --expect ipccancel -- $(QEMU_FLAGS)

run-ipcrevoked: $(ISO_IPCREVOKED)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IPCREVOKED) --expect ipcrevoked -- $(QEMU_FLAGS)

# ---- M9b call/reply images --------------------------------------------

$(ISO_IPCCALL): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,37)

$(ISO_IPCDEAD): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,38)

$(ISO_IPCSTALE): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,39)

$(ISO_IPCSELF): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,40)

$(ISO_IPCCOST): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,41)

$(ISO_IPCCAP): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,42)

$(ISO_IPCCAPHOSTILE): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,43)

$(ISO_IPCNTFNREV): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,44)

$(ISO_IPCCAPCOST): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,45)

# M9b scenarios. All five exit 33, including the three negatives:
# a client woken with an error is a caller-visible outcome, not a
# fault, and a kernel that died on any of them would be one a user
# component could halt.

run-ipccall: $(ISO_IPCCALL)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IPCCALL) --expect ipccall -- $(QEMU_FLAGS)

run-ipcdead: $(ISO_IPCDEAD)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IPCDEAD) --expect ipcdead -- $(QEMU_FLAGS)

run-ipcstale: $(ISO_IPCSTALE)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IPCSTALE) --expect ipcstale -- $(QEMU_FLAGS)

run-ipcself: $(ISO_IPCSELF)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IPCSELF) --expect ipcself -- $(QEMU_FLAGS)

run-ipccost: $(ISO_IPCCOST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IPCCOST) --expect ipccost -- $(QEMU_FLAGS)

# M9c scenarios: capability transfer and notifications.

run-ipccap: $(ISO_IPCCAP)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IPCCAP) --expect ipccap -- $(QEMU_FLAGS)

run-ipccaphostile: $(ISO_IPCCAPHOSTILE)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IPCCAPHOSTILE) --expect ipccaphostile -- $(QEMU_FLAGS)

run-ipcntfnrev: $(ISO_IPCNTFNREV)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IPCNTFNREV) --expect ipcntfnrev -- $(QEMU_FLAGS)

run-ipccapcost: $(ISO_IPCCAPCOST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IPCCAPCOST) --expect ipccapcost -- $(QEMU_FLAGS)


# ---- M10 interrupt images ----------------------------------------------
#
# Appended rather than woven into the lists above, and that is K7 from
# ADR 023: a Makefile is not edited with search-and-replace. GNU Make
# accumulates `+=`, takes any number of `.PHONY:` lines, and lets a
# recipe-less rule add prerequisites to an existing target -- so a
# milestone can add itself without touching a line another milestone
# depends on.

ISO_IRQTEST := $(BUILD_DIR)/safrox-irqtest.iso
ISO_IRQNOCAP := $(BUILD_DIR)/safrox-irqnocap.iso
ISO_IRQREVOKED := $(BUILD_DIR)/safrox-irqrevoked.iso
ISO_IRQALLOC := $(BUILD_DIR)/safrox-irqalloc.iso

ALL_ISOS += $(ISO_IRQTEST) $(ISO_IRQNOCAP) $(ISO_IRQREVOKED) $(ISO_IRQALLOC)

.PHONY: run-irqtest run-irqnocap run-irqrevoked run-irqalloc

$(ISO_IRQTEST): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,46)

$(ISO_IRQNOCAP): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,47)

$(ISO_IRQREVOKED): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,48)

$(ISO_IRQALLOC): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,49)

# M10 scenarios. irqtest exits 33; the three negatives exit 35.
#
# irq-no-cap and irq-revoked are 35 **by design rather than by
# failure** -- each ends in a deliberate panic so that its checks are
# asserted on a panic-expecting run, the arrangement `thread-kill-running`
# established. The kernel survives both; the log is the evidence, and
# run.py requires the lines that carry it.
#
# irq-alloc is 35 for the ordinary reason: it is a rule being enforced.

run-irqtest: $(ISO_IRQTEST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IRQTEST) --expect irqtest -- $(QEMU_FLAGS)

run-irqnocap: $(ISO_IRQNOCAP)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IRQNOCAP) --expect irqnocap -- $(QEMU_FLAGS)

run-irqrevoked: $(ISO_IRQREVOKED)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IRQREVOKED) --expect irqrevoked -- $(QEMU_FLAGS)

run-irqalloc: $(ISO_IRQALLOC)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IRQALLOC) --expect irqalloc -- $(QEMU_FLAGS)

# Add M10 to the acceptance suite without editing the `test:` rule.
#
# A target may be named in several rules as long as only one carries a
# recipe; the prerequisites accumulate. `test`'s recipe is above, so
# this line adds four more runs to it and nothing else.
test: run-irqtest run-irqnocap run-irqrevoked run-irqalloc


# ---- M10: the ring 3 interrupt scenario --------------------------------
#
# Appended separately from the four above because it was written
# after them: scenarios 1-5 and the three negatives were landed and
# run first, and this one needed a new blocking syscall (`Wait`) to
# exist before it could be written at all. Keeping the two additions
# apart leaves that order visible.

ISO_IRQUSER := $(BUILD_DIR)/safrox-irquser.iso

ALL_ISOS += $(ISO_IRQUSER)

.PHONY: run-irquser

$(ISO_IRQUSER): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,50)

run-irquser: $(ISO_IRQUSER)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_IRQUSER) --expect irquser -- $(QEMU_FLAGS)

test: run-irquser



# ---- M11: user-space address space management -------------------------
#
# Five scenarios: the positive one, three negatives and the read-only
# enforcement check. Appended rather than merged into an existing
# list, which is this Makefile's rule (ADR 023, K7) and the reason
# every milestone's additions stay legible as a block.
#
# Entry numbers continue from M10's 50.

ISO_VSUSERTEST    := $(BUILD_DIR)/safrox-vsusertest.iso
ISO_VSUSERKERNEL  := $(BUILD_DIR)/safrox-vsuserkernel.iso
ISO_VSUSERFOREIGN := $(BUILD_DIR)/safrox-vsuserforeign.iso
ISO_VSUSEREXHAUST := $(BUILD_DIR)/safrox-vsuserexhaust.iso
ISO_VSUSERRO      := $(BUILD_DIR)/safrox-vsuserro.iso

ALL_ISOS += $(ISO_VSUSERTEST) $(ISO_VSUSERKERNEL) $(ISO_VSUSERFOREIGN) \
            $(ISO_VSUSEREXHAUST) $(ISO_VSUSERRO)

.PHONY: run-vsusertest run-vsuserkernel run-vsuserforeign
.PHONY: run-vsuserexhaust run-vsuserro

$(ISO_VSUSERTEST): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,51)

$(ISO_VSUSERKERNEL): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,52)

$(ISO_VSUSERFOREIGN): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,53)

$(ISO_VSUSEREXHAUST): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,54)

$(ISO_VSUSERRO): $(KERNEL) boot/limine.conf.in $(LIMINE_EXE)
	$(call build_iso,$@,55)

run-vsusertest: $(ISO_VSUSERTEST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_VSUSERTEST) --expect vsusertest -- $(QEMU_FLAGS)

run-vsuserkernel: $(ISO_VSUSERKERNEL)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_VSUSERKERNEL) --expect vsuserkernel -- $(QEMU_FLAGS)

run-vsuserforeign: $(ISO_VSUSERFOREIGN)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_VSUSERFOREIGN) --expect vsuserforeign -- $(QEMU_FLAGS)

run-vsuserexhaust: $(ISO_VSUSEREXHAUST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_VSUSEREXHAUST) --expect vsuserexhaust -- $(QEMU_FLAGS)

run-vsuserro: $(ISO_VSUSERRO)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_VSUSERRO) --expect vsuserro -- $(QEMU_FLAGS)

test: run-vsusertest run-vsuserkernel run-vsuserforeign run-vsuserexhaust run-vsuserro


# ---- M12: the root task ------------------------------------------------
#
# Appended, as every milestone's block is (ADR 023, K7). The one
# exception is `$(KERNEL)`'s prerequisite list, which gained
# `$(ROOTTASK_BIN)` — a dependency cannot be expressed by appending,
# and an ordering convention would be remembered by whoever edits
# next rather than by make.

# The root task's flat image, embedded in the kernel's .rodata.
ROOTTASK_BIN := $(BUILD_DIR)/roottask.bin

# Its sources. Listed rather than globbed so that a new file is a
# deliberate addition: a glob silently picks up editor backups and
# silently misses a file added in a subdirectory.
ROOTTASK_SRCS := roottask/src/main.rs roottask/link.ld roottask/Cargo.toml \
                 abi/src/lib.rs

# Built by a script rather than inline, because it needs a *replaced*
# RUSTFLAGS and that is too subtle for a recipe line. Cargo merges
# `rustflags` across config files instead of letting the nearer one
# win, so a `roottask/.cargo/config.toml` produces a link line with
# both linker scripts and the kernel's comes first — linking the root
# task at -2 GiB, in the kernel half, where CPL 3 cannot fetch an
# instruction. `CARGO_TARGET_*_RUSTFLAGS` does not replace either.
# Only the blanket variable does, which is exactly the hazard
# .cargo/config.toml warns about at length, used here as the tool.
$(ROOTTASK_BIN): $(ROOTTASK_SRCS)
	@echo "  CARGO   roottask"
	@bash scripts/build-roottask.sh

roottask: $(ROOTTASK_BIN)

# **The kernel depends on the image**, because it embeds it with
# `include_bytes!` (ADR 044): a stale file is a kernel built around
# the wrong program.
#
# Appended as a second rule for the same target rather than edited
# into the original prerequisite list, and that is not only K7
# politeness — the direct edit **silently did nothing**.
#
# Make expands a prerequisite list when it *parses* the rule.
# `$(KERNEL)` is declared around line 200; `ROOTTASK_BIN` is defined
# here, some 570 lines later. At parse time the variable was empty,
# so the list read as though nothing had been added — no error, no
# warning, and `make -n` showing no rebuild even with the source
# newer than the image. The symptom was a scenario failing against
# a root task binary from two edits ago.
#
# A target may appear in several rules; make unions the
# prerequisites as long as only one rule has a recipe. So this line
# does what the edit was meant to do, and does it after the variable
# exists.
#
# The general lesson for an append-structured Makefile: appending a
# *rule* is safe, editing an earlier rule to name a later variable
# is not.
$(KERNEL): $(ROOTTASK_BIN)

# The scenarios. root-test exits 33; root-dies exits 35, because a
# dead root task is a panic (ADR 044) — after it, no capability names
# any resource, so "carry on" would mean "carry on doing nothing".
ISO_ROOTTEST     := $(BUILD_DIR)/safrox-roottest.iso
ISO_ROOTNOUNTYPED := $(BUILD_DIR)/safrox-rootnountyped.iso
ISO_ROOTDIES     := $(BUILD_DIR)/safrox-rootdies.iso

ALL_ISOS += $(ISO_ROOTTEST) $(ISO_ROOTNOUNTYPED) $(ISO_ROOTDIES)

# Entry indices into boot/limine.conf.in, which had 55 entries before
# M12 appended three. Literal numbers because that is what
# `default_entry` takes and what every other milestone's rules pass.
$(ISO_ROOTTEST): $(KERNEL) $(LIMINE_EXE) boot/limine.conf.in
	$(call build_iso,$@,56)

$(ISO_ROOTNOUNTYPED): $(KERNEL) $(LIMINE_EXE) boot/limine.conf.in
	$(call build_iso,$@,57)

$(ISO_ROOTDIES): $(KERNEL) $(LIMINE_EXE) boot/limine.conf.in
	$(call build_iso,$@,58)

run-roottest: $(ISO_ROOTTEST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_ROOTTEST) --expect roottest -- $(QEMU_FLAGS)

run-rootnountyped: $(ISO_ROOTNOUNTYPED)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_ROOTNOUNTYPED) --expect rootnountyped -- $(QEMU_FLAGS)

run-rootdies: $(ISO_ROOTDIES)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_ROOTDIES) --expect rootdies -- $(QEMU_FLAGS)

test: run-roottest run-rootnountyped run-rootdies

.PHONY: roottask run-roottest run-rootnountyped run-rootdies



# ---- M12 follow-up: clippy builds the kernel too -----------------------
#
# `make distclean && make test` failed where a plain `make test` did
# not, and the difference is the whole lesson.
#
# `test: fmt clippy run ...` runs clippy before any rule has built the
# kernel. clippy *compiles* kernel/src/main.rs, which embeds the root
# task image with `include_bytes!` (ADR 044). On a tree that already
# had build/ from an earlier run the file was simply there, so the
# missing edge was invisible. From a clean tree:
#
#     error: couldn't read `kernel/src/../../build/roottask.bin`
#     make: *** [Makefile:449: clippy] Error 101
#
# $(KERNEL) was given this prerequisite when M12 landed; clippy was
# not, because "the thing that builds the kernel" was read as the
# single target named $(KERNEL). clippy builds it as well, and so
# would `cargo check` or `cargo doc` if either were ever wired in.
#
# Appended rather than written into the `clippy:` rule above, and not
# only out of K7 politeness: that rule is parsed some 320 lines before
# ROOTTASK_BIN is defined, and make expands a prerequisite list when
# it parses the rule. Written there it would expand to the empty
# string -- no error, no warning, no dependency -- which is precisely
# the silent failure recorded for $(KERNEL) a few lines above. The
# same trap, caught a second time, in a target nobody thought to look
# at.
#
# `fmt` is deliberately NOT given this prerequisite. The two were
# measured against a tree with build/ removed: `cargo fmt --check`
# passes, `cargo clippy` fails. rustfmt parses the source, it does not
# expand macros, so it never reads the embedded file. Adding the edge
# anyway would be a dependency that can never fire -- indistinguishable
# from its own absence (ADR 023, K5) -- and would make `make fmt`, the
# cheapest check in the tree, build the root task first.
clippy: $(ROOTTASK_BIN)



# ---- M13: the first user-space driver ---------------------------------
#
# Appended, as every milestone's block is (ADR 023, K7).
#
# The build gains a second user component. The dependency chain is
# now three deep and the order is forced rather than chosen:
#
#     serial-driver.bin  ->  roottask.bin  ->  kernel.elf
#
# The root task embeds the driver's flat image with `include_bytes!`,
# because the kernel has no ELF loader and must not grow one --
# loading is a component's job (ADR 043). So the component that
# loads the driver is the one that carries it, and its image has to
# exist before the root task compiles.
#
# `ROOT_CODE_PAGES` went from 2 to 4 for that payload. The figure
# appears in kernel/src/roottask.rs, roottask/link.ld and
# scripts/build-roottask.sh, and all three had to change together --
# the two-page build failed at the link naming the limit, which is
# the guard working rather than an inconvenience.

DRIVER_BIN := $(BUILD_DIR)/serial-driver.bin

# Listed rather than globbed, for the reason ROOTTASK_SRCS is: a glob
# silently picks up editor backups and silently misses a file added
# in a subdirectory.
DRIVER_SRCS := driver/src/main.rs driver/link.ld driver/Cargo.toml \
               abi/src/lib.rs

$(DRIVER_BIN): $(DRIVER_SRCS)
	@echo "  CARGO   serial-driver"
	@bash scripts/build-driver.sh

driver: $(DRIVER_BIN)

# **The root task depends on the driver's image**, because it embeds
# it. A stale file here is a root task that loads the wrong program,
# and the failure would appear as a component behaving like an
# earlier version of itself.
#
# Appended as a second rule for the same target rather than edited
# into the original, which is both K7 and necessary: `$(ROOTTASK_BIN)`
# is declared some 70 lines above and make expands a prerequisite
# list when it *parses* the rule. Written there, `$(DRIVER_BIN)`
# would have been empty -- the silent failure M12 documented and
# M12's own follow-up fix hit a second time.
$(ROOTTASK_BIN): $(DRIVER_BIN)

# And clippy, for the reason the M12 follow-up established: clippy
# compiles kernel/src/main.rs, which reaches the root task image,
# which reaches this one. `make distclean && make test` is what finds
# a missing edge here (ADR 023, K9).
clippy: $(DRIVER_BIN)

ISO_DRIVERTEST := $(BUILD_DIR)/safrox-drivertest.iso
ISO_DRIVERDIES := $(BUILD_DIR)/safrox-driverdies.iso
ISO_DRIVERRESTART := $(BUILD_DIR)/safrox-driverrestart.iso
ISO_DRIVERHOSTILE := $(BUILD_DIR)/safrox-driverhostile.iso

ALL_ISOS += $(ISO_DRIVERTEST) $(ISO_DRIVERDIES) $(ISO_DRIVERRESTART) \
            $(ISO_DRIVERHOSTILE)

.PHONY: driver run-drivertest run-driverdies run-driverrestart
.PHONY: run-driverhostile

# Entry indices into boot/limine.conf.in, which had 58 entries before
# M13 appended four.
$(ISO_DRIVERTEST): $(KERNEL) $(LIMINE_EXE) boot/limine.conf.in
	$(call build_iso,$@,59)

$(ISO_DRIVERDIES): $(KERNEL) $(LIMINE_EXE) boot/limine.conf.in
	$(call build_iso,$@,60)

$(ISO_DRIVERRESTART): $(KERNEL) $(LIMINE_EXE) boot/limine.conf.in
	$(call build_iso,$@,61)

$(ISO_DRIVERHOSTILE): $(KERNEL) $(LIMINE_EXE) boot/limine.conf.in
	$(call build_iso,$@,62)

# **All four exit 33**, including the three negatives, and that is
# the milestone's shape rather than an oversight.
#
# A driver that dies is not a kernel fault — it is the case the
# architecture exists to survive, so a kernel that panicked would be
# failing the test rather than passing it. Same for a component
# refused the hardware: a refusal is a caller-visible outcome, and a
# kernel that stopped would be one any component could halt by
# asking for something it may not have.
#
# Contrast `root-dies`, which exits 35: when the *root* task dies
# every capability in the system becomes unnameable, and "carry on"
# would mean "carry on doing nothing" (ADR 044). The difference
# between the two is the whole content of "a driver is just a
# component".

run-drivertest: $(ISO_DRIVERTEST)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_DRIVERTEST) --expect drivertest -- $(QEMU_FLAGS)

run-driverdies: $(ISO_DRIVERDIES)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_DRIVERDIES) --expect driverdies -- $(QEMU_FLAGS)

run-driverrestart: $(ISO_DRIVERRESTART)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_DRIVERRESTART) --expect driverrestart -- $(QEMU_FLAGS)

run-driverhostile: $(ISO_DRIVERHOSTILE)
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_DRIVERHOSTILE) --expect driverhostile -- $(QEMU_FLAGS)

test: run-drivertest run-driverdies run-driverrestart run-driverhostile
