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

ALL_ISOS    := $(ISO) $(ISO_PANIC) $(ISO_INT3) $(ISO_PF) $(ISO_DF) \
               $(ISO_MEMTEST) $(ISO_DFREE)

LIMINE_DIR  := third_party/limine
LIMINE_EXE  := $(LIMINE_DIR)/limine
LIMINE_REPO := https://github.com/limine-bootloader/limine.git
# Pinned to the v9.x binary branch: the boot protocol revision the kernel
# requests (3) must match what the shipped bootloader blobs implement.
LIMINE_TAG  := v9.x-binary

RUST_SRCS := $(wildcard kernel/src/*.rs)

# ---- QEMU --------------------------------------------------------------
#
# -no-reboot   a triple fault terminates the VM instead of resetting it, so a
#              broken kernel fails fast instead of looping forever
# -no-shutdown leaves the VM paused rather than exiting on guest-initiated
#              shutdown, so the only way out is isa-debug-exit
# isa-debug-exit turns a guest I/O write into a host process exit status;
#              see kernel/src/qemu.rs for the (value << 1) | 1 encoding

QEMU_FLAGS := \
	-M q35 \
	-m 256M \
	-cpu qemu64 \
	-no-reboot \
	-no-shutdown \
	-device isa-debug-exit,iobase=0xf4,iosize=0x04 \
	-serial stdio \
	-display none

.PHONY: all kernel isos run run-panic run-int3 run-pf run-df \
        run-memtest run-doublefree \
        test debug clean distclean limine toolchain-check fmt clippy

# `test` depends on fmt and clippy, and those must finish before any QEMU run
# starts. Prerequisite order is only honoured in a serial build, so disable
# parallelism for this Makefile: a `make -j test` that ran clippy alongside
# the VM would report lint failures after the fact, or interleave their
# output with the serial log the runner is parsing.
.NOTPARALLEL:

all: isos

kernel: $(KERNEL)

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
#   self-test  the runner itself is proven to validate: handed the panic ISO
#              while expecting success, it must FAIL. Without this, a runner
#              that accepted everything would look identical to a working one.
#
# Every scenario additionally fails if the kernel banner appears twice,
# which would mean the machine triple-faulted and rebooted.
test: fmt clippy run run-panic run-int3 run-pf run-df run-memtest run-doublefree
	@$(PYTHON) scripts/run.py --qemu "$(QEMU)" --iso $(ISO_PANIC) \
		--expect success --self-test-expect-failure -- $(QEMU_FLAGS)
	@echo "M2: all acceptance tests passed"

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