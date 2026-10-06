# PythOS

A capability-based microkernel for x86-64, written in Rust, booted by
[Limine](https://github.com/limine-bootloader/limine).

**Current milestone: M0 — Bootstrap.** The kernel boots, identifies itself and
its boot environment on the serial console, reports panics with their source
location, and terminates QEMU with a status the host test runner can decode.

## Quick start

The build runs under Linux (WSL on Windows — see
[ADR 004](docs/adr/004-wsl-build-ortami.md)):

```sh
make toolchain-check   # report missing tools and how to install them
make                   # build the kernel and both ISO images
make test              # positive test, negative test, runner self-test
```

Required: `rustup` (nightly + `rust-src`), `make`, `git`, `xorriso`,
`qemu-system-x86_64`, `python3`.

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- \
    -y --default-toolchain nightly --profile minimal --component rust-src,llvm-tools
sudo apt-get install -y build-essential xorriso qemu-system-x86 python3 git
```

[`rust-toolchain.toml`](rust-toolchain.toml) pins the channel, components and
target, so `cargo` fetches the right toolchain on first build. Limine is
cloned into `third_party/` automatically.

> **Windows:** build inside WSL. `xorriso` and `qemu-system-x86_64` have no
> usable native Windows equivalents in the available package managers.
> ```
> wsl -d Ubuntu -- bash -lc "cd /mnt/c/Users/user/pyth && make test"
> ```

## Make targets

| Target | Description |
| --- | --- |
| `make` / `make all` | Build the kernel and both ISO images |
| `make kernel` | Build and statically verify the kernel ELF only |
| `make run` | Boot the normal ISO; **must** exit with host status 33 |
| `make run-panic` | Boot the panic ISO; **must** exit with host status 35 |
| `make test` | Both of the above, plus the runner's own self-test |
| `make debug` | Boot and wait for GDB on `localhost:1234` |
| `make clippy` | Lint, including the `// SAFETY:` comment check |
| `make fmt` | Check formatting |
| `make toolchain-check` | Report tool availability |
| `make clean` | Remove `target/` and `build/` |
| `make distclean` | Also remove the vendored Limine checkout |

## M0 acceptance criteria

### Positive path — `make run`

Boots `build/pythos.iso` and requires all of the following on COM1:

```
PythOS 0.1.0-M0
[boot] serial: COM1 115200 8N1 (self-test ok)
[boot] bootloader: Limine 9.6.7
[boot] higher-half base: 0xffffffff80000000 (phys 0xfc2c000, slide 0xffffffff703d4000)
[boot] hhdm offset: 0xffff800000000000
[boot] stack: 64 KiB (granted)
[boot] image: 0xffffffff80000000 .. 0xffffffff8000b000 (44 KiB)
[boot] bss:   0xffffffff8000a008 .. 0xffffffff8000a00a
[boot] cmdline: <none>
[boot] M0 checks passed
[boot] exiting QEMU (write 0x10 -> exit status 33)
```

The kernel then writes `0x10` to the `isa-debug-exit` port and QEMU exits with
**host status 33**.

### Negative path — `make run-panic`

Boots `build/pythos-panic.iso`: the *same kernel binary*, booted through a
Limine entry carrying `cmdline: panic-test`. The kernel sees the token and
panics deliberately ([ADR 002](docs/adr/002-tek-binary-iki-iso.md)):

```
*** KERNEL PANIC ***
[PANIC] at kernel/src/main.rs:240:9
[PANIC] deliberate panic requested via cmdline (token=panic-test)
[PANIC] halting; signalling QEMU (write 0x11 -> exit status 35)
```

The message carries **file:line:column**, the kernel writes `0x11`, and QEMU
exits with **host status 35**. `-no-reboot` guarantees a fault can never put
the VM into an endless reset loop.

The source path is project-relative, not absolute: `trim-paths = "all"` keeps
the builder's home directory out of the binary, so the output is identical on
every machine and the test is not asserting on someone's `$HOME`.

### The runner's own negative test

`make test` finally feeds the **panic ISO** to the runner while asking for
**success**. This must FAIL — otherwise a runner that silently accepted
everything would be indistinguishable from a working one:

```
[PASS] runner self-test: 'normal boot' expectations correctly rejected this run (3 violation(s)):
       - exit status 35 (guest wrote 0x11 (panic)); expected 33 (guest wrote 0x10 (success))
       - missing expected serial line: /^\[boot\] M0 checks passed/
       - unexpected serial line present: /KERNEL PANIC/
```

## Why exit status 33 and not 16?

QEMU's `isa-debug-exit` device does **not** let the guest choose its own host
exit status. Writing `VALUE` to the device's port makes QEMU terminate with:

```
host_status = (VALUE << 1) | 1
```

The low bit is hard-wired to 1, so **the guest can never produce status 0** —
QEMU reserves 0 for "the VM powered off normally". A runner that naively
treated "non-zero means failure" would flag every successful kernel run as a
failure.

[`scripts/run.py`](scripts/run.py) decodes the status instead of comparing it
to the raw written value:

| Guest writes | Host status | Meaning |
| --- | --- | --- |
| `0x10` | 33 | Success |
| `0x11` | 35 | Deliberate panic |
| — | 0 | Guest powered off without using the device → failure |
| — | 1 | QEMU itself failed to start → failure |
| — | (timeout) | Kernel hung before reaching the device → failure |

The runner also greps the serial log for the lines the milestone requires, so
a kernel that exits with the right status but prints nothing still fails.

## Layout

```
Cargo.toml              Crate, lints (warnings = deny), profiles
.cargo/config.toml      Target, build-std, linker script
rust-toolchain.toml     Pinned nightly + rust-src + x86_64-unknown-none
build.rs                Re-link when the linker script changes
boot/limine.conf.in     Boot config template; @DEFAULT_ENTRY@ picks the entry
kernel/src/main.rs      Limine requests, kstart() entry point
kernel/src/serial.rs    16550A COM1 driver, print!/println!
kernel/src/panic.rs     #[panic_handler]: file:line, re-entry guard
kernel/src/qemu.rs      isa-debug-exit, (value << 1) | 1
kernel/src/io.rs        in/out instructions, halt_forever
kernel/linker.ld        Higher-half layout, KEEPs .limine_requests
scripts/run.py          QEMU runner: status decoding + log assertions
scripts/check_kernel.py Post-link ELF validation
scripts/check_toolchain.py
docs/adr/               Architecture decision records
third_party/limine/     Vendored bootloader (fetched by make, gitignored)
```

## Implementation notes

**Memory safety comes from the language.** The whole kernel is the TCB and we
are not doing formal proof, so the only practical defence is a compiler that
rejects the mistakes. Every `unsafe` block carries a `// SAFETY:` comment
stating the invariant that makes it sound; this is enforced by
`clippy::undocumented_unsafe_blocks = "deny"` in
[`Cargo.toml`](Cargo.toml), not by review.

**Hardware constants cite their source.** Every port number, register offset
and bit position names the document it came from — PC16550D datasheet, Intel
SDM volume and section, QEMU source file, or the OSDev wiki. A named constant
without a citation is not self-documenting; it just moves the mystery.

**Warnings are denied in `Cargo.toml`, not via `RUSTFLAGS`.** Cargo *replaces*
rather than merges `[target.*.rustflags]` when `RUSTFLAGS` is set in the
environment. Denying warnings that way would silently drop the linker script
and produce a kernel linked at address 0 — which is exactly the failure this
project hit once. `[lints]` composes correctly and needs no environment
variable.

**Limine requests survive the linker.** They sit in `.limine_requests` with
`#[used]` + `#[link_section]`, and the linker script wraps them in `KEEP()`.
Nothing in the kernel references them — the bootloader finds them by scanning
that section — so without both, they would be garbage-collected and every
`response()` would return `None` at runtime. The section gets its own
writable `PT_LOAD` segment, because the bootloader writes response pointers
back into it.

**Post-link verification.** [`scripts/check_kernel.py`](scripts/check_kernel.py)
runs on every link and rejects the image if the entry point is not
higher-half, if any `PT_LOAD` segment is misaligned or not congruent with its
file offset modulo the page size, if `.limine_requests` went missing, or if an
absolute build path leaked in. See
[ADR 003](docs/adr/003-link-sonrasi-elf-denetimi.md).

**Serial.** COM1 at 115200 8N1, polled, with a UART loopback self-test at init
and a bounded spin on the transmit-holding-register — no kernel path may
contain an unbounded loop, and a missing UART must degrade to dropped output
rather than a hang.

**Panic.** The handler masks interrupts, reports `file:line:column` from
`PanicInfo`, and guards against recursive panics so a fault inside panic
handling still terminates the VM with a deterministic status instead of
triple-faulting and destroying the evidence.

## Architecture decisions

- [001 — Limine boot protocol, base revision 3](docs/adr/001-limine-boot-protocol-revision-3.md)
- [002 — Tek kernel binary, iki ISO](docs/adr/002-tek-binary-iki-iso.md)
- [003 — Link sonrası ELF denetimi](docs/adr/003-link-sonrasi-elf-denetimi.md)
- [004 — WSL Ubuntu build ortamı](docs/adr/004-wsl-build-ortami.md)

## Not yet built

Deliberately absent from M0, each belonging to a later milestone: GDT/IDT and
exception handling, physical and virtual memory management, the capability
object model (Untyped, CNode, TCB, Endpoint, Notification, Frame, PageTable,
IRQHandler), user space, and IPC.