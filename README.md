# PythOS

A capability-based microkernel for x86-64, written in Rust, booted by
[Limine](https://github.com/limine-bootloader/limine).

**Current milestone: M2 — Physical memory.** The kernel reads Limine's memory
map and manages physical frames with a bitmap allocator that never hands out
frame 0, the kernel image, or its own metadata — and that detects a double
free instead of corrupting itself quietly.

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
| `make run` | Normal boot; **must** exit 33 |
| `make run-panic` | Deliberate panic; **must** exit 35 |
| `make run-int3` | `int3` → #BP, handler *returns*; **must** exit 33 |
| `make run-pf` | Unmapped write → #PF with CR2; **must** exit 35 |
| `make run-df` | Corrupt RSP → #DF on IST1; **must** exit 35 |
| `make run-memtest` | Frame allocator suite; **must** exit 33 |
| `make run-doublefree` | Double free is detected; **must** exit 35 |
| `make test` | `fmt`, `clippy`, all seven scenarios, runner self-test |
| `make debug` | Boot and wait for GDB on `localhost:1234` |
| `make clippy` | Lint, including the `// SAFETY:` comment check |
| `make fmt` | Check formatting |
| `make toolchain-check` | Report tool availability |
| `make clean` | Remove `target/` and `build/` |
| `make distclean` | Also remove the vendored Limine checkout |

## M2 acceptance criteria

Seven scenarios, all booting the **same** `kernel.elf` through different
Limine entries ([ADR 002](docs/adr/002-tek-binary-iki-iso.md)).

### Normal boot — `make run`, exit 33

```
PythOS 0.1.0-M1
[boot] serial: COM1 115200 8N1 (self-test ok)
[boot] bootloader: Limine 9.6.7
[boot] higher-half base: 0xffffffff80000000 (phys 0xfc12000, slide 0xffffffff703ee000)
[boot] cmdline: <none>
[cpu] gdt: loaded (cs=0x0008, ds=0x0010, tss armed)
[cpu] ist1: 0xffffffff8000b070 .. 0xffffffff8000f070 (16 KiB) for #DF
[cpu] idt: 32 exception vectors installed, #DF on IST1
[boot] M1 checks passed
[boot] exiting QEMU (write 0x10 -> exit status 33)
```

### Breakpoint — `make run-int3`, exit 33

#BP is a *trap*: the CPU saves the address of the instruction *after*
`int3`, so the handler can `iretq` and execution simply continues. This is
the scenario that proves the dispatcher returns correctly, rather than
merely surviving.

```
[fault] executing int3 ...

*** EXCEPTION: #BP breakpoint (vector 3) ***
[trap] error_code=0x0000000000000000  rip=0xffffffff8000239a  cs=0x0008
[trap] rflags=0x0000000000000002  rsp=0xffff80000ff82f20  ss=0x0010
[trap] #BP is a trap; resuming at rip=0xffffffff8000239a
[fault] returned from int3 -- handler resumed execution
[boot] execution continued past the breakpoint
[boot] M1 checks passed
```

### Page fault — `make run-pf`, exit 35

A write to a canonical but unmapped address. The error code decodes to
write / not-present / supervisor, and CR2 holds exactly the address:

```
[fault] writing to unmapped canonical address 0x0000600000000000 ...

*** EXCEPTION: #PF page fault (vector 14) ***
[trap] error_code=0x0000000000000002  rip=0xffffffff80000fee  cs=0x0008
[trap] cr2=0x0000600000000000  (supervisor write not-present)

*** KERNEL PANIC ***
[PANIC] unhandled exception: #PF page fault (vector 14)
```

### Double fault — `make run-df`, exit 35

RSP is pointed at unmapped memory and an exception is raised. The CPU cannot
push the frame, so the fault escalates to #DF — and #DF is delivered on the
IST1 stack regardless of what RSP contained. The kernel checks its own RSP
against the IST1 range and says so ([ADR 005](docs/adr/005-double-fault-ist-stack.md)):

```
[fault] corrupting rsp and faulting to force #DF ...

*** EXCEPTION: #DF double fault (vector 8) ***
[trap] error_code=0x0000000000000000  rip=0xffffffff8000104f  cs=0x0008
[trap] rflags=0x0000000000000046  rsp=0x0000600000000000  ss=0x0010
[trap] IST1 stack: 0xffffffff8000b070 .. 0xffffffff8000f070, handler rsp=0xffffffff8000ef00
[trap] IST1: ok
[trap] unrecoverable; signalling QEMU (write 0x11 -> exit status 35)
```

Note the two stack pointers: `rsp=0x0000600000000000` in the frame is the
broken one that caused the fault; `handler rsp=0xffffffff8000ef00` is inside
IST1. Without the stack switch this would have been a triple fault.

### Frame allocator — `make run-memtest`, exit 33

The memory map is dumped in full, then four phases run against the real
memory of the machine. The kernel checks its own invariants and reports the
verdict; the runner asserts on that.

```
[mem] memory map: 16 entries
[mem]   0x0000000000001000..0x0000000000052000        324 KiB  bootloader-reclaimable
[mem]   0x0000000000052000..0x000000000009f000        308 KiB  usable
[mem]   0x0000000000100000..0x000000000fbd5000     256852 KiB  usable
[mem]   0x000000000fbdb000..0x000000000fc03000        160 KiB  executable-and-modules
[mem] total 12803 MiB, usable 251 MiB (64317 frames of 4 KiB)
[mem] bitmap: 7 KiB for 65361 frames, at phys 0x0000000000052000 (virt 0xffff800000052000)
[mem] image frames verified in use: 64473..=64512
[mem] allocator ready: 64315 free frames (251 MiB)
[memtest] alloc: 10000 unique, aligned, in-range (frames 84..=10180, 39 MiB span)
[memtest] alloc: none in the kernel image, none is frame 0
[memtest] free: count restored (64315)
[memtest] exhausted after 64315 frames; allocate() returned None cleanly
[memtest] recovery: freed frame 65360 @ 0x000000000ff50000 and got it straight back
```

Note the bitmap's address: it lives in the memory it manages, not in a
static array ([ADR 007](docs/adr/007-bitmap-frame-allocator.md)). The
~4 MiB of `bootloader-reclaimable` is deliberately left alone — Limine's
response structures, including the memory map being walked, are still in
there ([ADR 008](docs/adr/008-bootloader-reclaimable-dokunulmaz.md)).

### Double free — `make run-doublefree`, exit 35

A frame is allocated, freed, then freed again. Accepting the second free
quietly would put the same frame on the free list twice and later hand it
to two owners at once, so the allocator stops where the evidence is:

```
[memtest] got frame 84 @ 0x0000000000054000
[memtest] first free (legitimate) ...
[memtest] second free of the same frame -- this must panic

*** KERNEL PANIC ***
[PANIC] at kernel/src/memory/frame_allocator.rs:522:17
[PANIC] double free detected: frame 84 @ 0x0000000000054000 is already free
```

### Deliberate panic — `make run-panic`, exit 35

```
*** KERNEL PANIC ***
[PANIC] at kernel/src/main.rs:240:9
[PANIC] deliberate panic requested via cmdline (token=panic-test)
[PANIC] halting; signalling QEMU (write 0x11 -> exit status 35)
```

The message carries **file:line:column**. The path is project-relative, not
absolute: `trim-paths = "all"` keeps the builder's home directory out of the
binary, so the output is identical on every machine and the test is not
asserting on someone's `$HOME`.

### No resets, in any scenario

`-no-reboot` turns a triple fault into a clean QEMU exit rather than a
restart. To prove no scenario resets anyway, the runner counts the kernel
banner: the kernel prints it exactly once per boot, so **two occurrences
fail the test**. This is what gives the #DF scenario its teeth — a broken
IST would reboot, and rebooting is the only way that run could still print
everything else the test expects.

### The runner's own negative test

`make test` finally feeds the **panic ISO** to the runner while asking for
**success**. This must FAIL — otherwise a runner that silently accepted
everything would be indistinguishable from a working one:

```
[PASS] runner self-test: 'normal boot' expectations correctly rejected this run (3 violation(s)):
       - exit status 35 (guest wrote 0x11 (panic)); expected 33 (guest wrote 0x10 (success))
       - missing expected serial line: /^\[boot\] M1 checks passed/
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
kernel/src/main.rs      Limine requests, kstart() entry point, fault hooks
kernel/src/gdt.rs       GDT, TSS, IST1 stack for #DF
kernel/src/idt.rs       IDT gate descriptors, all 32 exception vectors
kernel/src/interrupts.rs  32 asm stubs, ExceptionFrame, common dispatcher
kernel/src/faults.rs    Deliberate #BP / #PF / #DF triggers
kernel/src/memory/      PhysAddr/PhysFrame, Limine memory map, frame allocator
kernel/src/memtest.rs   Frame allocator acceptance tests, run in-kernel
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
- [005 — Double fault'u IST1 üzerinde ayrı bir stack'te karşılamak](docs/adr/005-double-fault-ist-stack.md)
- [006 — Assembly stub'ları + tek ortak handler](docs/adr/006-exception-stub-ortak-handler.md)
- [007 — Fiziksel çerçeve ayırıcı olarak bitmap](docs/adr/007-bitmap-frame-allocator.md)
- [008 — BOOTLOADER_RECLAIMABLE bölgelerine M2'de dokunulmuyor](docs/adr/008-bootloader-reclaimable-dokunulmaz.md)

## Not yet built

Deliberately absent, each belonging to a later milestone: external interrupts
(the PIC is still masked as Limine left it), user-mode segments and ring 3,
physical and virtual memory management, the capability
object model (Untyped, CNode, TCB, Endpoint, Notification, Frame, PageTable,
IRQHandler), user space, and IPC.