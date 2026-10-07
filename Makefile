# SPDX-License-Identifier: BSD-2-Clause
#
# PythOS — M0 build system.
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
CARGO_OUT   := target/$(TARGET)/$(PROFILE)/pythos

BUILD_DIR   := build
ISO_ROOT    := $(BUILD_DIR)/iso_root
KERNEL      := $(BUILD_DIR)/kernel.elf
ISO         := $(BUILD_DIR)/pythos.iso
ISO_PANIC   := $(BUILD_DIR)/pythos-panic.iso
# M1 fault-injection images. Same kernel.elf, different Limine default_entry.
ISO_INT3    := $(BUILD_DIR)/pythos-int3.iso
ISO_PF      := $(BUILD_DIR)/pythos-pf.iso
ISO_DF      := $(BUILD_DIR)/pythos-df.iso
# M2 frame allocator images.
ISO_MEMTEST := $(BUILD_DIR)/pythos-memtest.iso
ISO_DFREE   := $(BUILD_DIR)/pythos-doublefree.iso
# M3 virtual memory images.
ISO_VMTEST   := $(BUILD_DIR)/pythos-vmtest.iso
ISO_STALETLB := $(BUILD_DIR)/pythos-staletlb.iso
ISO_GUARD    := $(BUILD_DIR)/pythos-guardpage.iso
ISO_NX       := $(BUILD_DIR)/pythos-nx.iso
ISO_STACKOVF := $(BUILD_DIR)/pythos-stackovf.iso
# M4 kernel heap images.
ISO_HEAPTEST  := $(BUILD_DIR)/pythos-heaptest.iso
ISO_HEAPOOM   := $(BUILD_DIR)/pythos-heapoom.iso
ISO_HEAPGUARD := $(BUILD_DIR)/pythos-heapguard.iso
ISO_CAPTEST := $(BUILD_DIR)/pythos-captest.iso
ISO_CAPREVOKE := $(BUILD_DIR)/pythos-caprevoke.iso
ISO_CAPINTALLOC := $(BUILD_DIR)/pythos-capintalloc.iso
ISO_CAPRETIRED := $(BUILD_DIR)/pythos-capretired.iso
ISO_THREADTEST := $(BUILD_DIR)/pythos-threadtest.iso
ISO_THREADOVF := $(BUILD_DIR)/pythos-threadovf.iso
ISO_THREADINTALLOC := $(BUILD_DIR)/pythos-threadintalloc.iso
ISO_THREADKILL := $(BUILD_DIR)/pythos-threadkill.iso
# M7 address space images.
ISO_VSPACETEST := $(BUILD_DIR)/pythos-vspacetest.iso
ISO_VSKERNEL := $(BUILD_DIR)/pythos-vskernel.iso
ISO_VSISOLATION := $(BUILD_DIR)/pythos-vsisolation.iso
ISO_VSSTALE := $(BUILD_DIR)/pythos-vsstale.iso

ALL_ISOS    := $(ISO) $(ISO_PANIC) $(ISO_INT3) $(ISO_PF) $(ISO_DF) \
               $(ISO_MEMTEST) $(ISO_DFREE) \
               $(ISO_VMTEST) $(ISO_STALETLB) $(ISO_GUARD) $(ISO_NX) \
               $(ISO_STACKOVF) \
               $(ISO_HEAPTEST) $(ISO_HEAPOOM) $(ISO_HEAPGUARD) \
               $(ISO_CAPTEST) $(ISO_CAPREVOKE) $(ISO_CAPINTALLOC) \
               $(ISO_CAPRETIRED) $(ISO_THREADTEST) $(ISO_THREADOVF) \
               $(ISO_THREADINTALLOC) $(ISO_THREADKILL) \
               $(ISO_VSPACETEST) $(ISO_VSKERNEL) $(ISO_VSISOLATION) \
               $(ISO_VSSTALE)

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
      run-vspacetest run-vskernel run-vsisolation run-vsstale
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_PANIC) \
		--expect success --self-test-expect-failure -- $(QEMU_FLAGS)
	@echo "M7: all acceptance tests passed"

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
