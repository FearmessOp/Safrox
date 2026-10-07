# PythOS

A capability-based microkernel for x86-64, written in Rust, booted by
[Limine](https://github.com/limine-bootloader/limine).

**Current milestone: M9 — IPC (architect pass).** No code: the
semantics of synchronous IPC, the message format and the IPC half of
the syscall ABI are now fixed, in eight ADRs
([033](docs/adr/033-rendezvous-semantigi.md)–[040](docs/adr/040-hizli-yol-yok.md)).
The shape of the thing is in [docs/ipc.md](docs/ipc.md); what could
not be settled is in
[docs/m9-acik-sorular.md](docs/m9-acik-sorular.md).

**Last milestone with code: M8 — Ring 3.** Code runs at CPL 3, in its
own address space, with its instructions in a page that is executable
and not writable. It reaches the kernel through one `syscall` and comes
back through `sysret`; the timer preempts it; its XMM registers
survive; and it enters the kernel on a stack no other thread shares.

M7 proved the address space and deliberately stopped there — reaching
ring 3 and reaching a private CR3 are two sources of error, and doing
both at once leaves a first `#GP` with two suspects. That debt is now
paid.

The claim that matters is one line of the acceptance test:

```
[user] ring 3: cs = 0x003b (cpl 3), data page reads 0xa5a5a5a511111111
```

The low two bits of CS are the privilege level. Everything else in
this milestone would pass on a kernel that ran the same code at ring 0.

**The value is in the five negative scenarios.** A kernel that lets
user code run but cannot stop it has gained nothing. `cli` from ring 3
takes `#GP`; a read of the kernel half takes `#PF` with the USER bit;
SMEP refuses the kernel a fetch from the *same page* ring 3 just
executed 101 times; SMAP refuses an unguarded read; and six forged
capability pointers come back as six distinct errors with the kernel
still running.

The design is seL4's, with three departures named in the ADRs: one
PageTable type instead of four ([017](docs/adr/017-nesne-kumesi.md)),
no guards and exactly two levels ([018](docs/adr/018-cptr-ve-cspace.md)),
and a bounded derivation depth ([019](docs/adr/019-derivation-tree.md)).

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
| `make run-vmtest` | Virtual memory suite; **must** exit 33 |
| `make run-staletlb` | Unmap without invalidating (advisory); **must** exit 33 |
| `make run-guardpage` | Write below the IST stack → #PF; **must** exit 35 |
| `make run-nx` | Execute from a data page → #PF; **must** exit 35 |
| `make run-stackovf` | Overflow the main stack → #DF on IST1; **must** exit 35 |
| `make run-heaptest` | Kernel heap suite; **must** exit 33 |
| `make run-heapoom` | The heap runs out and says so; **must** exit 35 |
| `make run-heapguard` | Write above the heap's limit → #PF; **must** exit 35 |
| `make run-captest` | Capability suite; **must** exit 33 |
| `make run-caprevoke` | Times a revoke over the widest, deepest tree; **must** exit 33 |
| `make run-capintalloc` | Allocating from an interrupt is refused; **must** exit 35 |
| `make run-capretired` | Allocating after retirement is refused; **must** exit 35 |
| `make run-threadtest` | Thread and scheduler suite; **must** exit 33 |
| `make run-threadovf` | A thread runs off its own stack → #DF; **must** exit 35 |
| `make run-threadintalloc` | Allocating inside a *real* timer interrupt; **must** exit 35 |
| `make run-threadkill` | Operating on a running thread is refused; **must** exit 35 |
| `make run-vspacetest` | Address space suite; **must** exit 33 |
| `make run-vskernel` | Mapping into the kernel half is *refused*; **must** exit 33 |
| `make run-vsisolation` | Reaching into another VSpace → #PF; **must** exit 35 |
| `make run-vsstale` | Revoking a VSpace in use is refused; **must** exit 35 |
| `make run-usertest` | Ring 3 suite; **must** exit 33 |
| `make run-userpriv` | `cli` at CPL 3 → #GP; **must** exit 35 |
| `make run-userkread` | Ring 3 reads the kernel half → #PF; **must** exit 35 |
| `make run-usersmep` | The kernel fetches from a user page → #PF; **must** exit 35 |
| `make run-usersmap` | The kernel reads a user page unguarded → #PF; **must** exit 35 |
| `make run-userforge` | Six forged CPtrs, six errors, kernel alive; **must** exit 33 |
| `make test` | `fmt`, `clippy`, all twenty-seven scenarios, runner self-test |
| `make debug` | Boot and wait for GDB on `localhost:1234` |
| `make clippy` | Lint, including the `// SAFETY:` comment check |
| `make fmt` | Check formatting |
| `make toolchain-check` | Report tool availability |
| `make clean` | Remove `target/` and `build/` |
| `make distclean` | Also remove the vendored Limine checkout |

## M8 acceptance criteria

Twenty-seven scenarios, all booting the **same** `kernel.elf` through
different Limine entries ([ADR 002](docs/adr/002-tek-binary-iki-iso.md)).

### Normal boot — `make run`, exit 33

Every boot now builds page tables, switches CR3, moves onto a kernel-owned
stack and reclaims the bootloader's memory — before any scenario hook runs:

```
[cpu] nx: supported (efer.nxe set)
[cpu] smep: unsupported, smap: unsupported, pge: enabled
[vm] building kernel address space (pml4 at frame 84 @ 0x0000000000054000)
[vm] pmap: 255 MiB of RAM at 0xffff800000000000, rw+nx+global
[vm] image .text:     0xffffffff80000000..0xffffffff8000f000 (15 pages) READ | EXEC | GLOBAL
[vm] image .rodata:   0xffffffff8000f000..0xffffffff80016000  (7 pages) READ | GLOBAL
[vm] image .writable: 0xffffffff80016000..0xffffffff8002e000 (24 pages) READ | WRITE | GLOBAL
[vm] kstack[0]: 0xffffc80000001000 .. 0xffffc80000005000 (16 KiB), guard page at 0xffffc80000000000 (ist1)
[vm] kstack[1]: 0xffffc80000006000 .. 0xffffc8000000a000 (16 KiB), guard page at 0xffffc80000005000 (main)
[vm] pre-switch check: 46 image pages mapped, stack reachable, .text is rx
[vm] switching cr3 to frame 84 @ 0x0000000000054000 ...
[vm] now running on our own page tables
[vm] switching stack: 0xffff80000ff6edc0 -> 0xffffc80000009ff8 (off the bootloader's stack)
[boot] now on the kernel's own stack
[mem] poisoned 4504 KiB of reclaimable memory with 0xde; boot info still reads back correctly
[mem] reclaimed 1126 frames (4504 KiB) from bootloader-reclaimable
[boot] after reclaim: bootloader still reported as Limine 9.6.7 (1126 frames gained)
[boot] M3 checks passed
```

Three lines carry most of the weight. **`pre-switch check`** walks every
page of the image in the *new* tables before CR3 moves; a missing page there
is a triple fault a few instructions later. **`switching stack`** is the one
the design originally missed — see below. **`poisoned ... still reads back
correctly`** is the kernel re-reading its own copies after filling the
bootloader's memory with `0xDE`.

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

### Virtual memory — `make run-vmtest`, exit 33

Eight checks against the live address space:

```
[vmtest] cpu: efer.nxe set, w^x is meaningful
[vmtest] cr3: own tables; pmap at 0xffff800000000000 is rw+nx and round-trips
[vmtest] stack: rsp 0xffffc80000009e10 in kstack[1] ..., phys 0x0000000000067e10,
         guard page at 0xffffc80000005000 unmapped, not reclaimable
[vmtest] map: 0xffffd80000000000 -> frame 1, wrote and read back 0x50474e4950414d56
[vmtest] translate: 0xffffd80000000000 -> 0x0000000000001000 PageFlags(READ | WRITE)
[vmtest] unmap: returned frame 1 as expected
[vmtest] frames: leaf frame returned; 3 page-table frames remain allocated by design
[vmtest] error: map over a live mapping -> AlreadyMapped
[vmtest] error: unmap an absent page -> NotMapped
[vmtest] error: map at non-canonical 0x0000800000000000 -> NotCanonical
[vmtest] error: map with WRITE|EXEC -> InvalidFlags (w^x)
[vmtest] error: all four cases returned errors, none panicked
[vmtest] memory map: 16 regions, 1 not page aligned; whole-frame arithmetic stays inside every one
[vmtest] reclaim: boot info survived (Limine 9.6.7, 16 map entries, cmdline "vm-test")
```

The error cases are the part that matters for M11: a user component will
drive this API through a capability, and a bad request must return an error,
not take the kernel down.

The "3 page-table frames remain allocated" line is a deliberate design
property, asserted exactly rather than tolerated
([ADR 015](docs/adr/015-ara-tablolar-serbest-birakilmiyor.md)).

### NX enforcement — `make run-nx`, exit 35

A jump into a data page in PMAP. The error code's bit 4 is the proof: the
CPU only sets it when `EFER.NXE` is enabled, so this one line shows both
that PMAP is NX *and* that `cpu::init` did its job.

```
[vmtest] nx test: jumping to 0xffff800000001000 (a data page in PMAP, mapped NX)

*** EXCEPTION: #PF page fault (vector 14) ***
[trap] error_code=0x0000000000000011  rip=0xffff800000001000  cs=0x0008
[trap] cr2=0xffff800000001000  (supervisor instruction fetch protection-violation)
```

Writing this test found a real defect: the M1 page-fault decoder only
looked at three bits, so it reported `0x11` as "supervisor **read**". An NX
violation was indistinguishable from an ordinary permission fault. The
decoder now handles the instruction-fetch and reserved-bit flags too.

### Guard page — `make run-guardpage`, exit 35

A write just below the IST stack. In M1 and M2 this address was mapped dead
space and the write simply succeeded; now the page is genuinely absent:

```
[vmtest] guard page test: writing to 0xffffc80000000000 (guard below the IST stack)

*** EXCEPTION: #PF page fault (vector 14) ***
[trap] cr2=0xffffc80000000ff8  (supervisor write not-present)
```

### Stack overflow — `make run-stackovf`, exit 35

The same protection, reached the way a real overflow would: by recursing
until RSP walks off the bottom of the main kernel stack.

```
[vmtest] depth 192, rsp 0xffffc80000006230

*** EXCEPTION: #DF double fault (vector 8) ***
[trap] rflags=0x0000000000000003  rsp=0xffffc80000006000  ss=0x0010
[trap] IST1 stack: 0xffffc80000001000 .. 0xffffc80000005000, handler rsp=0xffffc80000004e80
[trap] IST1: ok
```

**#DF, not #PF** — and that is correct, though it was not what this test
first expected. RSP descends into the guard page, the CPU wants to deliver
#PF, delivering it means pushing a five-qword frame onto the stack that
just ran out, the push faults, and the fault escalates. IST1 catches it.

This is the first time in the project that a *genuine* double fault — not
one manufactured by corrupting RSP on purpose — has exercised the IST, and
it is the clearest demonstration of why
[ADR 005](docs/adr/005-double-fault-ist-stack.md) exists. Without the stack
switch this would be a triple fault and a silent reset.

### Stale TLB — `make run-staletlb`, exit 33, advisory

A page is mapped, written through, unmapped **without** invalidating, and
read again. On real hardware the stale entry may answer the read; on QEMU
it may not. Either outcome passes, and the kernel says which happened:

```
[vmtest] unmapped it WITHOUT invalidating the TLB
[vmtest] read after unmap returned 0x50474e4950414d56 -- QEMU did not fault;
         this check is advisory
[vmtest] a stale TLB entry survived here; on real hardware this is exactly the
         bug TlbFlush exists to prevent
```

On this run the stale entry **did** survive — QEMU returned the old data
rather than faulting. That is a live demonstration of the hazard
[ADR 012](docs/adr/012-tlb-gecersizlestirme.md) is written against, and the
reason `TlbFlush` is `#[must_use]` with a panicking `Drop`. It is recorded
as advisory because the opposite outcome would also be legitimate; see
*Known gaps*.
### Kernel heap — `make run-heaptest`, exit 33

Five checks, each measuring something rather than merely exercising it:

```
[heap] base 0xffffc00000001000..0xffffc00004001000 (limit 64 MiB), 1 page(s) mapped
[heap] guard pages: 0xffffc00000000000 (below), 0xffffc00004001000 (above)
[heap] 17 size classes, 16 B .. 1024 KiB; alloc and dealloc are O(1) with no search

[heaptest] collections: Box, Vec(1000), String(400), BTreeMap(500) all allocate
           and read back correctly
[heaptest] alloc: 10000 blocks, 1 B .. 3 KiB, every one aligned and inside the heap
[heaptest] canaries: all 10000 blocks intact; no two allocations overlap
[heaptest] fragmentation: 5925046 bytes requested, 9367936 bytes reserved
           (158% -- the size classes guarantee at most 200%)
[heaptest] growth: 524288 bytes requested, heap grew from 2868 to 3071 pages,
           both ends writable
[heaptest] baseline: 0 bytes in use (was 0), 3071 pages still mapped
[heaptest] free lists: 10003 blocks across 12 of 17 classes (9668 KiB held for
           reuse; the heap does not shrink and does not coalesce -- ADR 016)
[heaptest] calls: 10101 alloc, 10101 dealloc, 2753 grow
```

Three of these lines report things the design chose, not things that
happened to work:

**The canary check runs after all ten thousand allocations**, not after
each one. A block that overlaps another is only visible once the second
one has been written, so checking as you go would miss exactly the case
the check exists for.

**12 MiB stays mapped after everything is freed.** The live byte count is
back to zero; the pages are not returned. That is the cost of not
coalescing — a free 64 KiB block cannot serve a 16 B request — and it is
printed rather than hidden, because silently holding 12 MiB and leaking
12 MiB look identical from outside.

**2753 of 10101 allocations grew the heap.** The ADR predicted the slow
path would run "once per size class"; the measurement says 27% of the
time. The reason is the test itself: it holds all ten thousand blocks
live, so nothing ever returns to a free list and almost every allocation
carves from the frontier. The design is unchanged but the claim was
wrong, and [ADR 016](docs/adr/016-kernel-heap-allocator.md) now says so —
with the consequence that M9's IPC path will *need* pre-warmed classes
rather than merely benefiting from them.

### Heap exhaustion — `make run-heapoom`, exit 35

1 MiB at a time until the heap hits its limit. The panic names the size,
the alignment, the size class, and how much is live:

```
[heaptest] 57 MiB allocated and leaked, 14847 pages mapped

*** KERNEL PANIC ***
[PANIC] heap exhausted: allocating 1048576 bytes (align 8, class 1048576) would
        take the heap to 66556 KiB, past its 65536 KiB limit. 66060288 bytes are
        live in 63 allocations.
```

This test failed in an instructive way first. It reported *the opposite*
of what it was checking:

```
[PANIC] the heap handed out 72 MiB without hitting its 64 MiB limit
```

The limit was fine. The allocations had not happened: LLVM recognises
`alloc` as an allocation function and may delete one whose result is never
used — and a null check is not a use. Adding a `write_volatile` at each
end of every block fixed it. The same trap had caught the stack-overflow
test in M3, where an unused array was optimised away and nothing
overflowed. **A test whose effect the optimiser can erase is not a test.**

### Heap guard page — `make run-heapguard`, exit 35

A write to the first byte above the heap's maximum extent:

```
[heaptest] guard page test: writing to 0xffffc00004001000, the unmapped page
           above the heap's 64 MiB limit

*** EXCEPTION: #PF page fault (vector 14) ***
[trap] cr2=0xffffc00004001000  (supervisor write not-present)
```

There is a guard below the heap too, at `0xffffc00000000000`, for pointer
underflow. Neither is ever mapped, so the protection does not depend on

### Capability system — `make run-captest`, exit 33

Fifteen checks. The object sizes and the alignment rule retype depends
on; the rights lattice, all 256 pairs against an independent definition;
retype across three size classes, and the demonstration that a retype
which cannot fit changes *nothing*; copy narrowing rights, and both ways
of trying to widen them; every distinct way a CPtr can fail; the depth
cap; device Untyped; and the difference between `delete` and `revoke`.

```
[captest] rights algebra: all 256 pairs checked against an independent
          definition; 81 subsets as predicted by 3^4
[captest] retype: CNode(16) + Endpoint + Frame; watermark 16 -> 8192
          (+8176 B for 4640 B of objects); the new CNode's 16 slots are all zero
[captest] atomicity: 256 frames (1048576 B) refused by a 65520 B Untyped;
          watermark still 16, all 248 destination slots still empty
[captest] revoke: 8 descendants deleted in 8 visits; Untyped survives,
          watermark 262176 -> 16, a new object reused 0x4400020
          (was 0x4440020), old pointers return errors
```

Two of these deserve a note.

**Atomicity** is checked by carving a deliberately small child Untyped —
64 KiB — and asking it for 256 Frames. The scratch Untyped is 4 MiB and
would have *succeeded*, which would have made the test vacuous. Both
halves of the proof are required: the watermark unmoved and the
destination slots empty. A retype that validated as it went would leave
the first few slots filled and the watermark partly advanced, and either
check alone could pass by luck.

**Revoke returning memory** is observed, not inferred. The watermark
dropping back could be a lie told by a counter; so the test records where
the objects were, revokes, retypes again, and requires the new object to
land at or below the old address.

#### The occupied-slot refusal, and how it was found to be untested

```
[captest] occupied slot: retype, copy and mint all refuse with SlotNotEmpty;
          the Endpoint occupant keeps its object, rights, depth and
          generation, and still resolves
[captest] partial overlap: a 4-object retype over slots 0..4 with slot 2
          occupied wrote nothing, not even the 2 slots before the obstacle;
          watermark still 131232
```

The first version of this suite did not test the emptiness check at all.
Turning it into a no-op — deleting the `return Err(SlotNotEmpty)` and
leaving the `if` — left all nineteen scenarios passing. The atomicity
check above exercises only the `NotEnoughMemory` path, so nothing
distinguished the guard from its own absence.

Each operation is now checked for three things, and the third is the one
that matters: the error, the unmoved watermark, and **the occupant still
being the capability it was** — same object, rights, depth and
generation, and still resolving through a CPtr. Asserting only that an
error came back would not notice an implementation that wrote first and
complained afterwards.

The overlap check puts the obstacle in the **middle** of the range. "Refuse
if the first slot is taken" is easy to satisfy by accident: an
implementation that validated each slot immediately before writing it
would pass that, and would still corrupt the slots preceding a later
obstacle.

Both checks were verified by mutation: each was watched failing against a
deliberately broken kernel before being kept.

#### The error paths nothing else reached

Finding one untested guard raised the obvious question: how many others?
Listing `CapError`'s fourteen variants against the ones `captest`
mentions answered it — three were never produced by anything.

```
[captest] remaining errors: RootSlotMustBeCNode fires for a non-CNode root
          slot; TooManyObjects at 257 but not at 256; InvalidArgument for
          count 0, a sized Endpoint and a radix-0 CNode
```

`RootSlotMustBeCNode` was the serious one. It is the *only* enforcement
of ADR 018's two-level rule, and that rule is half of the CPtr
invariant — the half that does not depend on generation counters. With
the check removed, a pointer through a root slot holding an Endpoint
read the Endpoint's bytes as an array of slots and returned a "Null
capability". The test now says so in those words.

`TooManyObjects` is checked from both sides: 257 objects refused, 256
*not* refused for that reason. A test that only exercised the failing
side would pass against an implementation that refused everything.

This produced a strengthening of ADR 023's rule K5. "At least one test
per milestone verified by breaking it" is a numeric threshold; what is
actually wanted is that *every* guard be distinguishable from its own
absence:

> If a function has N ways of returning `Err`, the acceptance test must
> trigger all N. An error path that is never taken cannot be
> distinguished from one that does not exist.

The variant listing is a cheap way to check it, and it is what found
these three.

One last thing the new checks taught, at the cost of a failing run: the
first version took the *last* root slot for its impostor, seeded it,
deleted it, and so advanced its generation — which turned
`check_cptr_errors`'s expected `EmptySlot` into a `StaleCPtr`. Two
checks had been sharing a slot without saying so. The slot choice is
now asserted rather than assumed.

#### The CPtr invariant

> A CPtr resolves to the same slot every time, or it returns an error.
> It never resolves to a different slot.

```
[captest] cptr invariant: slot 0 was Endpoint gen 2, is now Notification
          gen 3; the old pointer gives StaleCPtr and never the new object;
          a wrong root generation is caught too
```

This check exists because the first draft of ADR 018 broke it, and the
break is worth recording. The design had no generation counter and no
rule about what a root slot may hold. If root slot 5 held an Endpoint
then `CPtr = 5<<16` named it; if that slot was later refilled with a
CNode, the *same* CPtr silently resolved to that CNode's slot 0 — a
different object, no error, no warning.

That is precisely the ambiguity the ADR had criticised seL4's guard
field for, arrived at from the other direction. Two mechanisms now
prevent it: a root slot may hold **only** a CNode, so the index fields
never change meaning; and every slot counts how often it has been
emptied, with both generations carried in the pointer and checked.

The test deliberately refills the slot with a *different type* at a
*different address*, and asserts the addresses differ — otherwise it
could not tell "resolved to the old object" from "resolved to the new
one" and would prove nothing.

### Revoke cost — `make run-caprevoke`, exit 33

Not pass/fail: a measurement, and an uncomfortable one.

```
[captest] tree: 256 capabilities from one Untyped, deepest level 8,
          18 chain restarts
[captest] revoke timing: 256 capabilities in 620592 TSC cycles
          (2424 cycles each, 256 slots visited)
[captest] extrapolated: 8400000 slots would be ~20361600000 cycles,
          ~6787 ms at 3 GHz -- extrapolated from 256 samples under
          emulation, not a measurement of the worst case
```

ADR 019 estimated the worst case at **56 ms** and nothing had ever
checked it. The extrapolation says **6787 ms** — 120 times larger.

Part of that gap is emulation: QEMU's TCG interpreter is an order of
magnitude slow and its `rdtsc` is a virtual counter. Part of it is that
the estimate undercounted — "20 instructions" ignored the two neighbour
updates in `unlink` and the generation bump in `clear`. And part of it
goes the *other* way: 256 slots fit in L1, where eight million would
miss cache on every step, so the real figure could be worse still.

The honest conclusion is that the worst case is not known, and is
certainly larger than 56 ms. ADR 019 now says so, and the decision it
justified — revoke runs to completion with interrupts on but no thread
switch — has moved from "acceptable" to "must be revisited in M10".

### Allocation from an interrupt — `make run-capintalloc`, exit 35

```
[captest] interrupt depth is 1; calling allocate() (must panic)
*** KERNEL PANIC ***
[PANIC] frame allocation from inside an interrupt handler. Handlers must
        not allocate: there is no sensible behaviour when memory runs out
        in an interrupt, and the allocator is not reentrant. See ADR 022.
```

M2's soundness argument for the frame allocator had three legs: one CPU,
interrupts disabled, one non-reentrant entry point. M6 enables interrupts
and removes the second. ADR 022 chose to make "handlers do not allocate"
an enforced rule rather than add a lock that would protect nothing on a
single CPU — this is the tripwire that makes a violation loud at the
moment it happens, instead of leaving a corrupted bitmap to be found
later.

### Allocation after retirement — `make run-capretired`, exit 35

```
[captest] allocator is retired; calling allocate() (must panic)
*** KERNEL PANIC ***
[PANIC] frame allocation after the allocator was retired. From the end of
        boot, memory comes only from retyping an Untyped, so that every
        allocation is charged to a budget. See ADR 022.
```

how far the heap has grown.

### Threads and the scheduler — `make run-threadtest`, exit 33

Nine checks. The TCB's shape; the interrupt flag in each kind of region;
two threads handing off on separate stacks; the timer; preemption;
suspend and resume; `ReadRegisters`; and ADR 026's two-step kill.

```
[sched] boot thread adopted: tcb at 0x000000000ffde800,
        stack 0xffffc80000006000..0xffffc8000000a000
[apic] legacy 8259 PICs masked (all 16 lines)
[apic] lapic id 0, version 0x50014, registers at 0xffffd00000000000
[apic] timer: 100 Hz on vector 32, divisor 16, initial count 166392
       (calibrated to 16639200 ticks/s)
[boot] interrupts enabled; the "IF is always clear" assumption that held
       from M2 to M5 is now replaced by ADR 025's named regions

[thread] tcb: 2048 B, 2048-byte aligned, 64 B context, 1024 B reserved
         for the IPC buffer
[thread] IF discipline: inside an InterruptGuard, IF=0
[thread] IF discipline: nested guards do not re-open the outer region
[thread] IF discipline: inside a PreemptGuard, IF=1 and preemption is off
         -- this is what revoke uses (ADR 019)
[thread] cooperative: 200 rounds over 300 switches; A at 0xffffc8000000efe0
         (ran 100x, kstack slot 2), B at 0xffffc80000013fe0 (ran 100x,
         kstack slot 3); neither RSP is in the other's stack
[thread] timer: 100 interrupts taken, 101 ticks processed, 0 deferred by
         preempt-disabled regions, 0 spurious
[thread] preemption: the spinner started with IF=1, ran 263620 iterations
         without yielding; 1 ticks and 2 switches brought control back here
[thread] ReadRegisters: suspended thread at rip=0xffffffff800086ca,
         rsp=0xffffc8000000efe0 (inside its own stack), 102 switches,
         priority 42 (stored, not used -- ADR 024)
[thread] switch cost: 10000 switches in 3282006 TSC cycles (328 cycles each)
```

**The preemption line is the only one that cannot be faked.** Every other
check here would pass on a purely cooperative kernel. The spinning thread
calls nothing, yields nothing, and hints nothing — so if control ever
returns to the test, something took the CPU away from it, and the only
thing that can is the timer. The tick count is asserted for that reason.

**`revoke` runs with interrupts on.** That looks like an oversight and is
a decision: [ADR 019](docs/adr/019-derivation-tree.md) measured revoke at
up to 6787 ms, and dropping device interrupts for that long is not
acceptable. What must not happen during a revoke is a *thread switch*, so
there are two separate mechanisms —

| | Interrupts taken | Threads switched |
|---|---|---|
| `InterruptGuard` (`cli`) | no | no |
| `PreemptGuard` | **yes** | no |

— and the test observes which one `revoke` is in, rather than asserting it
in the abstract.

### Thread stack overflow — `make run-threadovf`, exit 35

M3 already overflowed a stack, but that was the kernel's main stack,
mapped by `vm::init`. This one is retyped from an Untyped and mapped for
a TCB: a different path to the same guard page.

```
[thread] overflowing: stack 0xffffc8000000b000..0xffffc8000000f000,
         rsp now 0xffffc8000000efa0
[thread] depth 88, rsp 0xffffc8000000d3d0

*** EXCEPTION: #DF double fault (vector 8) ***
[trap] IST1: ok
```

The thread names its own stack **before** overflowing. The #DF handler
cannot do it afterwards: by then the thread's stack is unusable, and
`sched::current()` would have to be trusted on a machine that has just
proved it cannot be.

Writing the driver for this test taught something about the rest of M6.
The first version handed the CPU over once and expected never to get it
back. It got it back immediately — the timer preempts the victim
mid-recursion, long before the guard page is reached. That is preemption
working, and the test had assumed it away.

### Allocation inside a real interrupt — `make run-threadintalloc`, exit 35

```
[thread] armed; waiting for the next timer tick (must panic in the handler)
[thread] inside the timer handler, in_interrupt=true; calling allocate()
*** KERNEL PANIC ***
[PANIC] frame allocation from inside an interrupt handler. Handlers must
        not allocate: there is no sensible behaviour when memory runs out
        in an interrupt, and the allocator is not reentrant. See ADR 022.
```

M5 tested this by driving the depth counter by hand, because there were
no interrupts to be inside of. This is the real path: the LAPIC raises
the interrupt, the CPU dispatches to the handler, and the allocation
happens there. ADR 022's tripwire was written in M5 and runs for the
first time here.

### Operating on a running thread — `make run-threadkill`, exit 35

```
[thread] the victim is Ready; attempting SetRegisters
[thread] refused with ThreadBusy(Ready), as ADR 026 requires
[thread] after Suspend the same call succeeds: the refusal is a
         sequencing requirement, not a dead end
```

What should happen when something tries to destroy a thread the scheduler
is holding? Both answers are defensible —
[ADR 026](docs/adr/026-tcb-yasam-dongusu.md) picks **refuse**, and the
point of the ADR is that "undefined" was not on the list.

Refusing is only acceptable because it cannot trap a caller. `Suspend`
*always* succeeds, in every state, so the two-step sequence always
terminates. The scenario shows both halves: the refusal, then the same
call succeeding after a suspend. A refusal with no path through it would
be a denial-of-service vector rather than a safety property.
### Address spaces — `make run-vspacetest`, exit 33

Nine checks. An address space assembled entirely from capabilities, the
two translate paths agreeing, SMAP, rights narrowing, isolation, unmap,
revoke, and the CR3 measurement.

```
[vspace] kernel half: 5 top-level entries, shared by every address space;
         cr3 is 0x0000000000054000
[vspace] built from capabilities: 0x0000000000400000 -> 0x000000000000d000
         PageFlags(READ | WRITE | USER)
[vspace] translate: the capability path and M3's paging agree
[vspace] smap: enforced; AC is clear in ordinary kernel code, set inside a
         UserAccess guard, and clear again after it
[vspace] rights: WRITE withheld maps read-only (PageFlags(READ | USER));
         no rights at all is refused with InsufficientRights
[vspace] isolation: at 0x0000000000400000, thread A reads 0xaaaaaaaa11111111
         and thread B reads 0xbbbbbbbb22222222; same address, different
         frames, 8 cr3 reloads
[vspace] unmap: the Frame and the PT both come back to their holders
[vspace] revoke: 5 capabilities deleted, watermark 24576 -> 16
[vspace] cr3 cost: 2166 cycles/switch within one address space (10004
         skipped), 48173 cycles/switch across two (10046 reloads)
```

**The isolation line is the one that cannot be faked.** Every other
check here would pass on a kernel with a single address space and
careful bookkeeping. Two threads reading different bytes from the same
virtual address would not.

The two patterns are required literally by the runner, because a test
printing the same value twice — or two zeroes — would satisfy a looser
regex and prove nothing.

**Changing address space costs 22×.** 2166 cycles within one, 48173
across two. The figures are from emulation and the ratio is what
matters, not the magnitudes; on real silicon the gap should be wider
still, because the true cost is the cold TLB afterwards rather than the
`mov cr3` itself. This is what pays for the rule that CR3 is reloaded
only when the address space differs — and the two counters in the line
are what make that rule testable. A kernel reloading on every switch
would report `0 skipped`; one never reloading would report `0 reloads`.
Requiring both to be non-zero rules out both.

**Intermediate tables are supplied, never allocated.** The test maps a
PDPT, then a PD, then a PT, then the Frame — four separate invocations,
because the kernel refuses to spend a frame the caller did not offer
([ADR 027](docs/adr/027-vspace-modeli.md)). M3's `paging::map` is used
unchanged for this, with a `FrameSource` that always returns `None`; a
missing table surfaces as an ordinary error. ADR 011's claim that the
paging API is mechanism rather than policy was tested here and held.

### Mapping into the kernel half — `make run-vskernel`, exit 33

```
[vspace] refused 0xffff800000000000 (the first kernel address): NotUserAddress
[vspace] refused 0xffff800000001000 (inside the physical window): NotUserAddress
[vspace] refused 0xffffc00000001000 (the kernel heap): NotUserAddress
[vspace] refused 0xffffffff80000000 (the kernel image): NotUserAddress
[vspace] and the paging layer's own half check is still there and still
         separate: WrongHalf for a USER mapping at a kernel address
[vspace] all four refused and the kernel's own mappings still work;
         this is an error path, not a fault
```

**Exit 33, not 35.** A rejected request is a caller error, and from M8
the caller is a user component. A kernel that died here would be worse
than one that allowed the mapping — it would be a denial of service
reachable from anywhere.

What is being protected is not isolation but kernel integrity. Every
address space shares the kernel's top-level page-table entries, one
physical copy of each table beneath them. A user mapping placed in the
kernel half would edit tables that *every other* address space is also
walking.

#### The check that was indistinguishable from its own absence

The first version of this scenario passed with the capability layer's
half check deleted.

Both layers refuse a kernel address — `paging::map` with `WrongHalf`,
the capability layer with its own test — and both were being reported
as `NotUserAddress`. The log said "refused" and nothing said by whom.
Mutation testing found it; the same shape as M5's `SlotNotEmpty`, where
a guard turned into a no-op left nineteen scenarios green.

They are now separate error values, because they are checking different
things: ownership (a user VSpace may not edit shared tables) against
consistency (the USER flag disagreeing with the address). Deleting
either one now fails this scenario on the first address.

Provoking the second error took two attempts, both instructive. A
kernel address inside PMAP is already mapped, so `AlreadyMapped` comes
back before the half check runs. And a kernel address with USER
*withheld* is perfectly consistent — both halves of the statement agree
— so there is nothing to report. What triggers it is a USER mapping at
a kernel address, at a location nothing else has touched.

### Address space isolation — `make run-vsisolation`, exit 35

Isolation from the failing side. One address is mapped in B's VSpace
and absent from A's; a thread in A reads it.

```
[vspace] 0x0000008000000000 in B: Ok(PhysAddr(102400)); in A: Err(NotMapped)
[vspace] thread running in its own VSpace (cr3 0x0000000000009000);
         reading 0x0000008000000000

*** EXCEPTION: #PF page fault (vector 14) ***
[trap] error_code=0x0000000000000000
[trap] cr2=0x0000008000000000  (supervisor read not-present)
```

**Error code 0 is required, and that is not pedantry.** The faulting
thread holds a `UserAccess` guard across the read. Without it SMAP
would refuse first, and the scenario would end in #PF at exactly the
same CR2 — looking like a pass while proving nothing about address
spaces. Not-present and protection-violation are the same symptom from
two unrelated causes; only the error code separates them.

### Revoked address space — `make run-vsstale`, exit 35

[ADR 028](docs/adr/028-vspace-yasam-dongusu.md) splits one question
into two, because they have different answers.

```
[vspace] SetSpace on the running thread: ThreadBusy, as ADR 026 requires
[vspace] victim running with cr3 0x0000000000009000
[vspace] a thread tried to revoke the address space it is running in
         (root 0x0000000000009000): VSpaceInUse, as ADR 028 case A requires
[vspace] revoked an idle VSpace: 5 capabilities deleted
[sched] thread at 0x0000000000010800 has a revoked address space;
        suspended rather than switched into (ADR 028)
[vspace] the thread was suspended rather than run: 1 stale detections,
         state Suspended
```

**Case A — the VSpace in CR3 right now: refused.** Revoke resets the
Untyped's watermark, so the live page tables would be handed out again
while the CPU is still walking them. Not a fault but undefined
behaviour, and one no interrupt could report, because reaching the IDT
goes through those same tables.

Reaching this case honestly took some thought. The check is "is this
root in CR3", and by the time control returns to the test thread, CR3
holds the kernel's PML4 again — which has no capability naming it,
because `vm::init` built it rather than retyping it. So the attempt has
to come from *inside*: the victim asks to destroy the address space it
is executing in.

**Case B — an idle thread's VSpace: allowed.** Nothing is walking those
tables. The thread is stopped when something next tries to switch into
it, which costs one comparison there rather than a sweep of every TCB
at revoke time — ADR 019 already measured revoke in seconds.

Staleness is detected by **address and generation together**. The
address alone is not enough: revoke resets the watermark, so the next
retype is *likely* to place a new object at exactly the old address,
and the thread would resume in somebody else's address space with no
fault to mark the moment. Precisely the problem
[ADR 018](docs/adr/018-cptr-ve-cspace.md) solved for CPtrs, wearing a
different hat.

**Falling back silently to the kernel's address space was rejected.** A
thread's address space is the definition of what it can see; changing
it without telling anyone means the component is now looking at the
kernel's memory. A protection mechanism that retreats quietly is worse
than one that was never there — at least nobody trusts the second.

### Ring 3 — `make run-usertest`, exit 33

```
[cpu] gdt: user cs=0x003b, user ss=0x0033 (ring 3, ADR 029)
[cpu] syscall: enabled (efer.sce set), lstar 0xffffffff8001e3c0, star 0x002b00080000000
[cpu] syscall: fmask clears IF|DF|TF|AC -- AC because `stac` is unprivileged
[user] fmask: read back as 0x0000000000040700 from the MSR -- IF, DF, TF and AC all cleared on entry
[user] fpu: osfxsr and osxmmexcpt set, cr0.ts clear, saved mxcsr 0x1f80
[user] address space: code at 0x0000000000400000 -> 0x0000000000022000 r-x, stack rw-, data rw-
[user] w^x: no page is both writable and executable
[user] capability path: ReadRegisters from ring 3 returned 1
[user] ring 3: cs = 0x003b (cpl 3), data page reads 0xa5a5a5a511111111, 3940 syscalls completed
[user] kernel stacks: thread A enters at 0xffffc80000036f80, thread B at 0xffffc8000003bf80
[user] isolation: at 0x0000000000600000, thread A reads 0xa5a5a5a5... and thread B reads 0x5a5a5a5a...
[user] xmm: thread A holds 0xdeadbeef00001111, thread B 0xfeedface00002222;
       204 fxsave over 204 switches -- equal, so nothing is lazy
[user] preemption: 488 ticks while the kernel span without yielding
[syscall] 9810054 calls (0 returned an error), 9810054 via sysret, 0 forced onto iretq
```

**CS is `0x003b`, and its low two bits are 3.** That is the milestone.
The selector is GDT index 7 with RPL 3, and the index is not a choice:
`sysret` computes CS from `STAR[63:48] + 16` and SS from `+ 8`, so the
user descriptors must sit in a particular order — 32-bit code, data,
64-bit code. A `const` assertion in [`gdt`](kernel/src/gdt.rs:1)
reproduces that arithmetic, because getting it wrong loads a wrong
descriptor *silently* and the failure surfaces at the first user
instruction.

**The capability path is exercised, not assumed.** User code invokes
`ReadRegisters` on its own TCB through a CPtr the kernel resolves and
validates. Everything else the program does goes through `Echo`, which
resolves no capability at all — so without this line, "ring 3 reaches
the capability system" would rest on nothing. The first run of this
scenario reported `last capability invocation: <none yet>`, which is
exactly that gap showing.

**Two user threads enter the kernel on different stacks, each inside
its own.** A counter would not do: "RSP0 is being updated" is satisfied
by an implementation that writes it once, because the first value is
already correct. So the two addresses are required to differ *and* each
is checked against its own thread's stack bounds.

**`fxsave` count equals the switch count**, which is what makes "no
lazy switching" testable. Preservation alone proves nothing — a correct
lazy switcher also preserves every register. See
[ADR 031](docs/adr/031-fpu-sse-durumu.md), including what a mutation
test showed this check *cannot* catch.

**The measurement.** 2.1 million syscalls in 2×10⁹ cycles, 934 cycles
per round trip including the user-side loop. Measured under emulation:
QEMU's TCG interpreter does not model the pipeline flush a privilege
transition costs on silicon, and that flush is most of what a syscall
pays for. The number is a floor.

The `iretq` fallback count is **zero**, and that is honest rather than
good: ADR 029's canonical-RCX check is never exercised here. Filed in
[`hardware-todo.md`](docs/hardware-todo.md) item 6.


### Privileged instruction from ring 3 — `make run-userpriv`, exit 35

```
[user] running `cli` at cpl 3; expected #GP, error code 0

*** EXCEPTION: #GP general protection (vector 13) ***
[trap] error_code=0x0000000000000000  rip=0x0000000000400020  cs=0x003b
[trap] rflags=0x0000000000000202  rsp=0x0000000000500ff0  ss=0x0033
```

User code runs `cli`, then `hlt`, `rdmsr` and `lgdt`. Each is
privileged, so the **first** one faults — and `cli` is first
deliberately, because it is the one that looks harmless.

`cs=0x003b` and `rsp` in the user stack: the fault came from ring 3,
not from somewhere else that happened to produce a `#GP`. Error code 0
rather than a selector index, because this is a privilege violation and
not a bad descriptor.


### User reads the kernel half — `make run-userkread`, exit 35

```
[trap] error_code=0x0000000000000005  rip=0x000000000040002a  cs=0x003b
[trap] cr2=0xffffc00000001000  (user read protection-violation)
```

Error code `0x5` is present (bit 0) plus **user** (bit 2). That second
bit is the point: it distinguishes "ring 3 reached for the kernel" from
"a kernel pointer went wrong", and the two mean entirely different
things. The runner forbids the `supervisor read` form, which would mean
the access happened in the kernel after all.


### SMEP — `make run-usersmep`, exit 35

```
[user] ring 3 executed the page 101 times; the same page is now fetched from cpl 0
[trap] error_code=0x0000000000000011  rip=0x0000000000400000  cs=0x0008
[trap] cr2=0x0000000000400000  (supervisor instruction fetch protection-violation)
```

**SMEP has been in CR4 since M3 with nothing to protect.** This is its
first real test, and it closes the QEMU half of
[`hardware-todo.md`](docs/hardware-todo.md) item 4.

Error code `0x11`: instruction fetch (bit 4) on a present page (bit 0),
from supervisor mode. Nothing but SMEP produces that combination.

**The same page** is used for both halves, and that is load-bearing. A
different page would leave the fault explainable by a mapping
difference rather than by the privilege level of the code fetching it —
the test would then be measuring its own setup.

This scenario also caught the milestone's first real bug, before it was
written as a test. `configure_user` replaced R12 with the user entry
point but left RIP pointing at M6's kernel trampoline, which ran
`sti; jmp r12` — so the kernel jumped into a USER page at CPL 0. SMEP
refused the fetch and the `#PF` named the right address with the wrong
CS. Had SMEP been off, the user program would have run happily at ring
0 and every check except the one that reads CS would have passed.


### SMAP — `make run-usersmap`, exit 35

```
[user] smap: with a UserAccess guard, the kernel read 0xa5a5a5a511111111 -- succeeded
[user] smap: now reading 0x0000000000600000 with no guard
[trap] cr2=0x0000000000600000  (supervisor read protection-violation)
```

Both halves in one scenario, because either alone proves half the
claim: that the guard is *necessary* and that it is *sufficient*.

The guarded read runs **first**. The unguarded one ends the VM, so the
other order would make half the claim untestable — an ordering
constraint that is easy to get backwards and impossible to notice
afterwards.


### Forged capability pointers — `make run-userforge`, exit 33

```
[user] cap-forge: all 6 hostile invocations refused, each with its own
       error -- InvalidCPtr, CPtrOutOfRange, StaleCPtr, EmptySlot,
       WrongType, InvalidArgument
[user] the kernel is still running; exiting normally
[syscall] 13 calls (6 returned an error), 13 via sysret
```

The first genuinely hostile input this kernel has taken. Ring 3 invokes
with reserved bits set, an index past the end of the root CNode, a
stale generation, an empty slot, a real object of the wrong type, and
an unknown method number.

**Each must fail for its own reason.** Six `InvalidArgument`s would
satisfy a count and prove nothing — which is precisely the mistake
M7's two half-checks made, found by mutation and recorded in
[ADR 027](docs/adr/027-vspace-modeli.md). The test asserts the specific
error code for each case.

Exit 33, not 35: a refused invocation is a *caller* error. A kernel
that panics on a bad CPtr is a kernel any user component can halt.


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
kernel/src/main.rs      Limine requests, kstart() -> kmain(), fault hooks
kernel/src/cpu.rs       CPUID, EFER.NXE, CR4.{PGE,SMEP,SMAP}, read back and verify
kernel/src/gdt.rs       GDT, TSS, IST1 (bootstrap, then re-pointed by vm::init)
kernel/src/idt.rs       IDT gate descriptors, all 32 exception vectors
kernel/src/interrupts.rs  32 asm stubs, ExceptionFrame, common dispatcher
kernel/src/faults.rs    Deliberate #BP / #PF / #DF triggers
kernel/src/memory/mod.rs          PhysAddr/VirtAddr/PhysFrame, Limine memory map
kernel/src/memory/frame_allocator.rs  Bitmap over all managed physical memory
kernel/src/memory/layout.rs       Address-space constants, canonicality, stack slots
kernel/src/memory/paging.rs       Entry/PageTable/PageFlags, map/unmap/translate, TlbFlush
kernel/src/memory/vm.rs           Builds PMAP + W^X image + stacks, switches CR3 and RSP
kernel/src/memory/bootinfo.rs     Copies Limine's responses out, then reclaims its memory
kernel/src/memory/heap.rs         Size-classed free lists, #[global_allocator], fixed reserve
kernel/src/cap/mod.rs       CapType, Rights, CapError, object sizes, compile-time ceilings
kernel/src/cap/cnode.rs     Slot (32 B), the derivation list, copy/mint/delete/revoke
kernel/src/cap/cptr.rs      CPtr resolution: two levels, two generations, no loop
kernel/src/cap/untyped.rs   retype: two phases, atomic, always zeroes
kernel/src/cap/boot.rs      Root Untypeds, the init CSpace, allocator retirement
kernel/src/cap/tcb.rs       TCB: context, states, SetRegisters/ReadRegisters/SetSpace
kernel/src/cap/vspace.rs    VSpace: Frame and PageTable Map/Unmap, rights to permissions
kernel/src/sched.rs     switch_context, round-robin, CR3 switching, the two guards
kernel/src/apic.rs      LAPIC, timer calibrated against the TSC, legacy PIC masking
kernel/src/memtest.rs   Frame allocator acceptance tests, run in-kernel
kernel/src/vmtest.rs    Virtual memory acceptance tests and negative scenarios
kernel/src/heaptest.rs  Kernel heap acceptance tests and negative scenarios
kernel/src/captest.rs   Capability acceptance tests, the revoke measurement, two tripwires
kernel/src/threadtest.rs  Thread and scheduler tests, the switch measurement, three scenarios
kernel/src/vspacetest.rs  Address space tests, the CR3 measurement, three scenarios
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

M3:

- [009 — Adres uzayı yerleşimi ve user/kernel sınırı](docs/adr/009-adres-uzayi-yerlesimi.md)
- [010 — HHDM'in kaderi: kendi fiziksel pencere eşlememiz](docs/adr/010-hhdm-kaderi.md)
- [011 — Sayfa tablosu API'si: mekanizma, politika değil](docs/adr/011-sayfa-tablosu-api.md)
- [012 — TLB geçersizleştirme disiplini](docs/adr/012-tlb-gecersizlestirme.md)
- [013 — BOOTLOADER_RECLAIMABLE geri kazanımı: beş kapı](docs/adr/013-reclaimable-geri-kazanim.md)
- [014 — CPU özellik bitleri: NXE, SMEP, SMAP, PGE](docs/adr/014-cpu-ozellik-bitleri.md)
- [015 — `unmap` ara sayfa tablolarını serbest bırakmıyor](docs/adr/015-ara-tablolar-serbest-birakilmiyor.md)

ADR 010 ve 013, uygulama sırasında bulunanlarla güncellendi: 010'a EFER.NXE
ön koşulu ve PMAP takma adının W^X'i nasıl deldiği, 013'e tasarımın
kaçırdığı beşinci kapı eklendi.

M4:

- [016 — Kernel heap: boyut sınıflı serbest listeler](docs/adr/016-kernel-heap-allocator.md)

ADR 016 da ölçümlerle güncellendi: yavaş yolun sıklığı tahmin edilenden
yüksek çıktı ve parçalanma rakamları artık varsayım değil, kabul testinin
raporladığı sayılar.

M5:

- [017 — Nesne kümesi: sekiz tip](docs/adr/017-nesne-kumesi.md)
- [018 — CPtr formatı: iki seviye, guard yok](docs/adr/018-cptr-ve-cspace.md)
- [019 — Derivation tree ve revoke'un sınırı](docs/adr/019-derivation-tree.md)
- [020 — Retype: atomiklik, watermark, sıfırlama](docs/adr/020-retype-semantigi.md)
- [021 — Haklar: dört bit, monoton daralma](docs/adr/021-capability-haklari.md)
- [022 — Kernel'in ayrıcalıklı kaynakları](docs/adr/022-kernelin-ayricalikli-kaynaklari.md)
- [023 — Testler optimize edilip silinemez](docs/adr/023-test-disiplini.md)

Dördü uygulama sırasında güncellendi. 018, kabul edilmeden önce bir
tasarım hatası yüzünden yeniden yazıldı (aşağıya bakın). 019'un en kötü
durum tahmini ölçüldü ve **120 kat yanlış** çıktı. 022'ye heap rezervinin
sıralaması ve sayfa tablolarının maliyeti eklendi; 017'ye ölçülen
boyutlar yazıldı.

M6:

- [024 — Zamanlayıcı kernel'de: geçici, ve çıkış planı](docs/adr/024-zamanlayici-kernelde.md)
- [025 — IF disiplini: kesmeler ilk kez açılıyor](docs/adr/025-kesme-disiplini.md)
- [026 — TCB yaşam döngüsü: çalışan bir thread'i yok etmek](docs/adr/026-tcb-yasam-dongusu.md)

024 ve 025 ikisi de uygulama sırasında düzeltildi: 024'e ölçülen bağlam
değiştirme maliyeti (switch başına 328 çevrim) ve testlerin artık
deterministik olmadığı; 025'e iki ayrı kaydedilmiş hata — biri yazarken,
biri çalıştırırken bulundu.

M7:

- [027 — VSpace: user adres uzayı bir capability](docs/adr/027-vspace-modeli.md)
- [028 — Revoke edilmiş bir VSpace'e bağlı thread](docs/adr/028-vspace-yasam-dongusu.md)
- [029 — Ring 3'e giriş ve çıkış: `iretq` ile gir, `sysret` ile dön](docs/adr/029-ring-3-giris-ve-cikis.md)
- [030 — Syscall ABI: tek giriş, hiç yeni çağrı](docs/adr/030-syscall-abi.md)
- [031 — FPU/SSE durumu: her switch'te kaydet, lazy yapma](docs/adr/031-fpu-sse-durumu.md)
- [032 — User tarafında W^X: `EXEC` bir hak değil, bir izin](docs/adr/032-user-tarafinda-wx.md)
- [033 — Rendezvous: senkron IPC ve bekleyen kuyruğu](docs/adr/033-rendezvous-semantigi.md)
- [034 — Mesaj formatı: register, tampon, message tag](docs/adr/034-mesaj-formati.md)
- [035 — Capability transferi: en fazla dört, alıcının yerine](docs/adr/035-capability-transferi.md)
- [036 — Call ve Reply: cevap hakkı sunucunun TCB'sinde](docs/adr/036-call-reply.md)
- [037 — Badge: 32 bit, slot'ta, ve bir kez](docs/adr/037-badge.md)
- [038 — Notification ve bağlı bekleme](docs/adr/038-notification-ve-bagli-bekleme.md)
- [039 — Ölüm ve iptal: bloke bir thread'in başına gelenler](docs/adr/039-ipc-olum-ve-iptal.md)
- [040 — Hızlı yol M9'da yok, ve yavaş yol ölçülecek](docs/adr/040-hizli-yol-yok.md)

033–040 bir **architect pass**: kod yazılmadan önce IPC'nin
semantiği sabitlendi. Üç eski ADR bu sırada düzeltildi —
[021](docs/adr/021-capability-haklari.md)'in badge genişliği
(64 → 32 bit, slot 32 bayta sığmıyor),
[026](docs/adr/026-tcb-yasam-dongusu.md)'nın M9'a bıraktığı
`Blocked` sorusu (kapandı), ve
[034](docs/adr/034-mesaj-formati.md)'ün `reserved` alanı
(12 → 11 bit, bağlı bekleme bir ayırt edici bit istedi).

Hepsinin bir arada ne anlama geldiği [docs/ipc.md](docs/ipc.md)'de:
bit-bit mesaj formatı, durum diyagramı ve bir istemci-sunucu
alışverişinin on bir adımı. Karara bağlanamayan yedi nokta
[docs/m9-acik-sorular.md](docs/m9-acik-sorular.md)'de, üçü
uygulama başlamadan cevaplanmalı.

027 iki kez düzeltildi, ikisi de koşarak bulundu: "kökü revoke et"
hiçbir şey silmiyordu, ve "iki kontrol de test ediliyor" iddiası
mutasyonla yanlışlandı.

İki eski ADR de bu milestone'da değişti. **[015](docs/adr/015-ara-tablolar-serbest-birakilmiyor.md)
kapatıldı** — ara tabloların serbest bırakılması sorusu çözülmedi,
sahipliği devredildi. **[014](docs/adr/014-cpu-ozellik-bitleri.md)
düzeltildi**: SMAP borcunu M8'e yazmıştı, M7'de geldi.

## What M8 cost: a bug SMEP found, and a guard proved by its own output

### SMEP caught a kernel bug before it caught a hostile one

The first run of `user-test` ended in a `#PF` at the user entry point
with `cs=0x0008` and error code `0x11` — instruction fetch, protection
violation, from **ring 0**.

`configure_user` builds on `configure`, which writes M6's kernel
trampoline into RIP and the entry point into R12. The user version
replaced R12 and *forgot RIP*, so the kernel trampoline ran — `sti`,
then `jmp r12` — and the kernel jumped into a page marked USER while
still at CPL 0.

SMEP refused the fetch. The diagnosis was as direct as that mistake
can produce: the right address, the wrong CS.

What makes this worth recording is the counterfactual. With SMEP off,
the user program would have run perfectly well at ring 0. The data
page would have read correctly, the syscalls would have worked, the
XMM registers would have been preserved — every check in the scenario
would have passed **except** the one that reads CS. A protection
mechanism that exists to stop an attacker found a bug in the code that
set it up, two minutes after it first had anything to protect.

### A guard whose only evidence was its own banner

ADR 030 argues that `IA32_FMASK` must clear AC, because `stac` is
unprivileged: user code can open an SMAP window and hand it to the
kernel, where `UserAccess::new` reads "already set" and does nothing —
leaving SMAP off for the whole syscall with no fault and no log.

Mutation testing removed AC from the mask. The test failed, and at
first that looked like success.

It was not. The only thing that broke was this line:

```
[cpu] syscall: fmask clears IF|DF|TF|AC
```

The test's sole evidence was a sentence the kernel printed about
itself. A commit that removed the bit would naturally also fix that
message — it would be *wrong* otherwise — and the mutation would have
passed unnoticed.

This is the same shape as M5's `SlotNotEmpty` and M7's K1, with a new
variant: there the two guards reported the same error, here the
guard's evidence was under the guard author's control.

The fix reads the MSR back. On the second attempt the mutation died on
behaviour:

```
[PANIC] IA32_FMASK does not clear AC on syscall entry: ... The MSR
        holds 0x0000000000000700.
```

ADR 023's K5 gains a clause: **a check's evidence cannot be something
the change that removes the check could also change.**

### What a counter cannot measure

ADR 031 claims eager FPU switching, and the test asserts that the
`fxsave` count equals the switch count — on the reasoning that a lazy
implementation would save fewer times.

A mutation made `save_fpu` skip every second call. Two results, and
both were instructive.

The **value** check caught it, cleanly: thread A read thread B's
pattern out of XMM0.

The **counter** check did not. The mutation incremented the counter
while skipping the save, so the equality held. A counter cannot
measure the correctness of the code that updates it; it can only
measure disagreement between the two.

That does not make the counter useless — an accidental skip takes the
increment with it, and a real lazy implementation would keep an honest
count. But it narrows what the check proves, and the ADR now says so.

A third observation: the value check caught the mutation on some runs
and not others, depending on which thread was preempted when. One
mutation run is not evidence that a guard is tested.

### Measured

| | cycles |
|---|---|
| syscall round trip (incl. user-side loop) | 934 |
| context switch, same address space (M7) | 2166 |
| context switch, across address spaces (M7) | 48173 |

Under emulation. QEMU's TCG interpreter does not model the pipeline
flush a privilege transition costs, and that flush is most of what a
real syscall pays for — so 934 is a floor, not an estimate.

The `sysret`-versus-`iretq` comparison ADR 029 promised is **not**
here: the canonical-RCX fallback was never triggered, so there is
nothing to compare. Recorded as an unpaid debt in
[`hardware-todo.md`](docs/hardware-todo.md) item 6 rather than left as
a claim.

## What M7 cost: a borrowed assumption and a check that checked nothing

### SMAP arrived a milestone early

The known-gaps list had `stac`/`clac` filed under M8, to be written
"once user mappings exist". The phrasing hides an assumption: that user
mappings arrive with ring 3.

**SMAP keys off the page's USER bit, not the privilege level of the
code touching it.** M7 creates USER pages while every thread is still
at CPL 0, so the first user mapping and the first SMAP violation
arrived together — a milestone before the first ring-3 instruction.

The acceptance test found it on its first run: `supervisor read
protection-violation` on a page that was present, readable, and
correctly mapped. Nothing was wrong with the mapping; SMAP was doing
its job.

ADR 014 had already specified the fix, down to preferring a RAII guard
over hand-written pairs, and the reason it gave is the one that
matters: an early `return` between `stac` and `clac` leaves AC set, and
from that point SMAP protects nothing, silently. `cpu::UserAccess` is
that guard. Only the milestone was wrong.

**The general shape:** "feature X will be needed in milestone Y" can
quietly assume that what *triggers* X is what Y *brings*. Here it was
not.

### "A VSpace" is not an object, so it cannot be revoked

ADR 027's first draft said tearing down an address space meant revoking
its root PageTable capability. The code was written that way, ran, and
deleted nothing — `0 capabilities deleted`, watermark unmoved.

Revoke removes a capability's *descendants*. The root, the three
intermediate tables and the Frame are all children of the same
**Untyped** — siblings, not a chain. The root is their peer and reaches
none of them.

The deeper point is that a VSpace is a *shape* built from capabilities,
and nothing records that shape: the derivation tree does not know which
tables belong to which root, because it does not need to. What owns all
of it is the Untyped, and that is what gets revoked.

This turned out to be an unplanned vindication of
[ADR 020](docs/adr/020-retype-semantigi.md). Deciding that memory comes
back by resetting a watermark, rather than object by object, has the
side effect that destroying an address space is a single operation. A
design that freed objects individually would have needed to record the
VSpace's shape somewhere.

### The check that was indistinguishable from its own absence

ADR 027 claimed two independent guards stopped a user mapping landing
in the kernel half, and that both were tested. Mutation testing
disagreed: deleting the capability layer's check left
`vspace-kernel-half` **passing**.

Both layers refused the same four addresses, and both refusals were
being reported as the same error. The log said "refused"; nothing said
by whom.

Exactly M5's `SlotNotEmpty` again, where turning a guard into a no-op
left all nineteen scenarios green. Two milestones apart, same shape:

> If deleting a check does not fail a test, the check is not being
> tested, however many tests exercise the code path it sits on.

The two errors are now distinct values, and the scenario requires both.
Triggering the second one took two failed attempts — the first address
was already mapped, and the second combination was not actually
inconsistent — which is its own small argument for K5. Writing a test
that provokes an error forces you to understand what the error *is*.

## What M6 cost: two mistakes about where interrupts are not

Both were about the same thing, found a day apart, and the second was
the expensive one.

### Writing: "phase 2 is enough" was the wrong unit

ADR 025's first draft said `retype`'s second phase needs `cli` and the
first does not, because phase 1 writes nothing. That is true and
irrelevant.

The phase split is an **atomicity** argument, not a concurrency one.
Phase 1's job is to *prove* phase 2 cannot fail — watermark sufficient,
destinations empty, depth within bounds — and phase 2 then writes
without checking anything. An interrupting thread that advances the
watermark invalidates the proof, and phase 2 never notices. What needs
protecting is not phase 2 but **the lifetime of phase 1's proof**, which
begins at the top of phase 1.

Caught by writing the ADR. The cost was a paragraph.

### Running: a thread that starts with interrupts disabled

Every context switch happens inside a `cli` region, and the `sti` that
ends it sits on the line *after* `switch_context` returns. A thread that
has run before resumes exactly there. A thread that has **never** run
does not: its saved RIP is its entry point, and the switch's `ret` goes
straight there, past every `sti` on the way.

Such a thread runs with IF clear for its whole life. And if it never
yields, nothing can take the CPU back — the timer meant to preempt it is
masked in its own RFLAGS. The scheduler was working perfectly; the
interrupt simply never arrived.

The fix is three instructions. A new thread starts at a trampoline that
executes `sti` and jumps to the real entry point, carried in R12 so the
ordinary context load delivers it.

Two things about this are worth more than the bug.

**Only one test could have found it.** The cooperative hand-off test
passed throughout, because threads that call `yield_now` enter the
region and so reach the `sti`. It took a thread that yields *nothing* —
and that check exists only because the acceptance criteria demanded one.

**The first attempt to test it did not work.** The obvious guard is "did
the thread start with IF=1?", checked after control returns. Mutation
testing showed it never ran: reaching it requires `yield_now` to return,
and the definition of the bug is that it does not. The check was correct
exactly when it was unnecessary.

When a bug's consequence is *having no CPU left to observe it with*, no
observation of the consequence can be a test. The invariant has to be
checked before the CPU is handed over — on the stopped thread, through
`ReadRegisters`, against the trampoline's address. Measured difference:
a thirty-second silent timeout became a 0.9-second panic naming its own
cause and the ADR that explains it.

### And one flaky test, which was neither

`make test` failed once in roughly twenty-eight runs on a line that had
nothing wrong with it: the ping-pong test printed `201 rounds` where
`run.py` demanded `200`.

Each thread increments the counter and only then looks at it. The one
that reaches 200 suspends itself — but its partner may still be on the
ready queue, and a timer tick runs it once more before it stops too.
With two threads the overshoot is exactly one.

The tempting fix is `\d+`, which also accepts 202 — and 202 would mean a
thread ran *after* suspending itself, the one thing `suspend` promises
not to allow. So the bound is asserted instead, in the kernel and in the
runner both: `rounds <= 201`, pattern `20[01]`. Sixty consecutive runs
clean.

This is M6's general tax, and it is worth stating separately from the
bug: **until M5 every acceptance test printed identical numbers on every
boot, because nothing could interrupt the kernel.** That is over. The
question to ask of a printed number is no longer "is this the value?"
but "what are its bounds, and why?"

## The object model

Fixed as of the M5 architect pass: the **eight object types, the CPtr
format and the four rights bits will not change before M13**. The full
reference — every object's size, invariants and invocations, a rights
matrix, and an ASCII walkthrough of an Endpoint being retyped from an
Untyped and then revoked — is in
[docs/object-model.md](docs/object-model.md).

Three decisions depart from seL4, each named where it happens:

**One `PageTable` type instead of four.** seL4 gives PML4, PDPT, PD and
PT separate object types, which catches level confusion at compile time.
We use one type and decide the level by where the table is mapped. Four
nearly identical retype paths and invocation sets were not worth the
type safety, since the error is caught at runtime either way.

**CPtr resolution has no loop.** seL4's guarded radix tree is the most
flexible design and the hardest part of seL4 to understand; resolution
walks a variable number of levels. Ours is fixed at two: 32 reserved bits
that must be zero, 16 bits of L1 index, 16 of L0. Worst case is two
memory accesses and three comparisons — under 20 instructions, no
iteration. The cost is that CSpace hierarchies cannot nest more than two
deep, so multi-level component delegation is closed off. The reserved
bits leave room to open it.

**Revocation is bounded but the bound is large.** The derivation tree
lives in the capability slots as a doubly linked list in preorder, so
revoke is a forward scan rather than a tree walk, and derivation depth is
capped at 8 so the "is this a descendant" test is a single comparison.
Breadth is not capped, though: the worst case on a 256 MiB machine is
**8.4 million slots, about 56 ms**. The number was computed rather than
hoped for, and it is written down in
[ADR 019](docs/adr/019-derivation-tree.md) along with what happens during
those 56 ms — interrupts are serviced, threads are not switched. To be
re-examined in M10 against a measured workload.

### Open questions — answered

The six decisions listed in
[docs/m5-acik-sorular.md](docs/m5-acik-sorular.md) were all settled
before the implementation began:

1. **Revoke cost.** Accept it and measure in M10. The measurement
   arrived earlier than planned, and is worse than the estimate — see
   `make run-caprevoke` above.
2. **Generation counters in CPtrs.** Added. One comparison per level,
   two levels, no loop. They turned out to be load-bearing rather than
   belt-and-braces: without them ADR 018's "two levels" rule alone does
   not give the invariant, because a leaf slot can still be reused.
3. **Device memory.** A device Untyped, with the invariant that it may
   only be retyped to a Frame, and that the Frame stays device-marked so
   it can never be recycled into a kernel object.
4. **Shared memory.** Not in M5. Written into ADR 020 as an explicit
   precondition for M9, so the IPC design cannot quietly assume it.
   M13 will have no filesystem and no network stack.
5. **IPC buffer.** Inside the TCB. The TCB is 2048 bytes and IPC
   messages are at most 1 KiB — a limit chosen here, visible in
   ADR 017's table, rather than discovered in M9.
6. **Syscall surface.** A single `invoke`.

## What M5 cost: a design error, two regressions, and a wrong estimate

### The design error, found before any code was written

ADR 018's first draft let a CPtr resolve to a *different* slot. The
mechanism is described under the CPtr invariant above. What is worth
recording is the shape of the mistake: the ADR spent several paragraphs
arguing that seL4's guard field makes a CPtr's meaning depend on the
tree's shape, and then specified a scheme with exactly that property,
reached from a different direction.

The lesson generalises past this bug: **when a design is defended by
pointing at a flaw in somebody else's, check whether the same flaw
appears in your own in a different guise.** The criticism was correct
and the design was still wrong.

### Two regressions in M4's heap, both found by running it

Neither was a coding error. Both were ADR 022's retirement rule meeting
assumptions that predated it.

**The heap had no budget.** ADR 022 retires the frame allocator at the
end of boot, with the precondition that "every frame the kernel will
ever need has already been taken". A heap that maps pages on demand can
never satisfy that — there is always a frame it will need and has not
taken. The first allocation after boot panicked.

The fix is the budget the heap never had: a contiguous physical reserve,
taken before the Untypeds and before the allocator retires. This is
strictly better than what M4 had, because the heap's physical cost is
now decided once and printed, rather than drifting upward as a side
effect of allocation.

Two details only appeared by running it. The reserve must be taken
before `heap::init`, not after, because init maps the first page — a
tripwire caught that on the first boot. And the reserve must pay for the
**page tables** as well as the pages: sized at exactly `KHEAP_MAX_SIZE`,
the heap ran out 34 frames short of its own 64 MiB limit, which is
precisely what the tables cost. *A budget that covers the data but not
the structure needed to reach it is not a budget.*

**`memtest` and `vmtest` needed a live allocator.** They run after boot,
and boot now hands every frame to the capability system. The fix was not
to weaken the tests but to make the allocator's lifetime visible in
`kmain`: scenarios that allocate directly run *before* `cap::boot::init`,
scenarios that do not run after.

`memtest::run()` also had to start diverging. Its last phase drains the
allocator deliberately — harmless while it was the last thing to run,
and not harmless once `cap::boot::init` followed it.

One check was tempting to loosen and was not. `memtest` bounds the frames
held by boot structures at 64, and the 16384-frame heap reserve blew
straight through it. Raising the bound to 17000 would have hidden a real
500-frame leak behind the reserve's bulk; subtracting the reserve by name
leaves the original check exactly as sharp as it was.

### The estimate that was 120 times wrong

ADR 019's worst-case revoke figure — 56 ms — was computed during the
architect pass and never checked against anything. Measured, it is at
least 6787 ms, and the real number on hardware is unknown.

The ADR's old summary line read "the number was computed, accepted, and
written down". That was true and insufficient; what was missing was "and
never verified". From M6 on, every estimate of this kind gets a scenario
that measures it.

## What M4 cost: three defects, two of them in the tests

| # | Defect | How it surfaced | What it says |
|---|---|---|---|
| 1 | `heap-oom` reported the heap exceeding its own limit | `the heap handed out 72 MiB without hitting its 64 MiB limit` | The limit was fine; LLVM had deleted the allocations, because a null check is not a *use* of the returned pointer. Second time this project has been bitten by an optimised-away test — the first was M3's stack overflow. |
| 2 | The stress phase ran the heap out of memory, three times running | A clean OOM panic where a pass was expected | My arithmetic, not the heap's. Cycling evenly through logarithmic size classes puts 1/n of the allocations in the largest one, and that class alone then dominates: 769 × 64 KiB = 48 MiB against a 64 MiB heap. |
| 3 | `[heap] grew to ...` printed on every growth | Hundreds of lines; serial output dominated the run | Logging that is useful once per boot is noise once per allocation. Now reported at powers of two. |

Defect 2 took *three* wrong guesses before the arithmetic got done: a size
sequence that doubled, then a class index written as 16 when 64 KiB is
class 12, then the realisation that even a correct 64 KiB sweep is too
much. The fix was to compute the index from the byte count instead of
writing it down.

None of the three was in the allocator. Its limit check, OOM message and
guard pages all worked the first time they were exercised — some evidence
that reasoning about bounded `alloc`/`dealloc` in
[ADR 016](docs/adr/016-kernel-heap-allocator.md) before writing the code
was worth the time.

What the ADR got *wrong* was a prediction, not a design: it claimed the
slow path would run roughly once per size class. The measurement says 27%
of allocations. The ADR now carries the number and the reason.

## What M3 cost: seven defects

Each of these was found by running the thing, not by reading it. They are
listed because the *shape* of each is more useful than the fix.

| # | Defect | How it surfaced | What it says |
|---|---|---|---|
| 1 | `.limine_requests` fell outside all three mapped ranges — a one-page hole | `vm::verify_before_switch` refused to switch CR3 | The pre-switch check paid for itself on its first run. Without it: silent reset. |
| 2 | Limine's stack is in `BOOTLOADER_RECLAIMABLE`; reclaim poisoned the live stack | `#GP` with `rip=0xdededededededede` | The poison pattern worked exactly as designed — and caught a gap in the *gate list*, not in the code. |
| 3 | The frame bitmap was sized by the highest *usable* address, but reclaim donates frames above it | `release of out-of-range frame 65349` | An M2 invariant that M3 quietly invalidated. |
| 4 | `RUST_SRCS := $(wildcard kernel/src/*.rs)` missed `kernel/src/memory/` | Edits appeared to have no effect; the ISO held a stale kernel | The worst kind: the build *succeeded*, and lied. |
| 5 | The #PF decoder ignored error-code bits 3 and 4 | The NX test's error code read as "supervisor read" | A test that could not have told success from failure. |
| 6 | `make build` was a silent no-op (`build/` is a directory, not `.PHONY`) | "Nothing to be done for 'build'" | Same class as #4: a command that looks like it worked. |
| 7 | `memtest` asserted the free count had not moved since init | Panic at the first M3 boot | M2's assumption "nothing allocates after init" is false once `vm::init` exists. |

Defects 1, 2 and 7 were caught by checks the project already had. Defects
4 and 6 were caught by accident, and both were build-system lies — the
category this project is least protected against. Defect 5 was caught by
writing a test that demanded the kernel *explain* the fault rather than
just produce one.

## Known gaps

Things this milestone deliberately did not finish, each with the evidence
and the milestone it belongs to.

Claims that are *untestable here* rather than unfinished — the ones QEMU
cannot settle — are collected separately in
[docs/hardware-todo.md](docs/hardware-todo.md). The sharpest of them:
deleting the body of `TlbFlush::flush` entirely, `invlpg` and `mfence`
both, leaves all fifteen scenarios passing. The experiment was run. What
`TlbFlush` buys in this environment is compile-time only.

**The heap has no lock.** Same shape as the frame allocator's gap: one
CPU, one non-reentrant entry point — though "interrupts disabled" is no
longer part of the argument, since M6. What holds it together now is
that nothing on an interrupt path allocates, which
[ADR 022](docs/adr/022-kernelin-ayricalikli-kaynaklari.md)'s tripwire
enforces rather than assumes. There is also an `IN_ALLOC` flag that
panics if the mapping path ever allocates.

**The heap never shrinks.** Freeing everything returns the live byte count
to zero and leaves 12 MiB mapped; the free lists hold it for reuse within
their own size class. Returning pages would mean counting live blocks per
page, which is the same bookkeeping that
[ADR 015](docs/adr/015-ara-tablolar-serbest-birakilmiyor.md) declined for
page tables and for the same reason. **Revisit after M9**, when there is a
real allocation pattern to measure instead of a synthetic one.

**Allocations above 1 MiB panic.** Deliberate: the fallback would be a
search for contiguous free pages, which is the unbounded loop the whole
design exists to avoid. A kernel object that large should come from the
frame allocator directly. See
[ADR 016](docs/adr/016-kernel-heap-allocator.md).

**M9 will need pre-warmed size classes.** Measured, not predicted: 27% of
allocations in the acceptance test took the slow path, because nothing was
being freed. On the IPC path that is the difference between 15
instructions and up to 256 page mappings, decided by whether a free list
happens to be empty. `heap::prewarm(class, count)` does not exist yet; its
place is marked.

**PMAP aliases the kernel image as writable.** No single page is both
writable and executable, but `.text`'s physical frames are reachable
read-write through the physical window. W^X therefore holds per virtual
page, not per physical frame. Harmless while the kernel is the only writer;
a privilege-escalation path once user code can request mappings. Closing it
means mapping the image's physical range read-only in the window, which
costs a few extra page tables. **Due before M8.** See
[ADR 010](docs/adr/010-hhdm-kaderi.md).

**SMAP is now enforcing something, and the `UserAccess` guard exists.**
Both were filed under M8 and both arrived in M7, because SMAP keys off
a page's USER bit rather than the privilege level of the code touching
it: the first user mapping was the first SMAP violation.
`vspace-test` shows AC clear in ordinary kernel code, set inside a
guard, and clear again afterwards. **SMEP still protects nothing** —
nothing executes from a user page until M8. See
[ADR 014](docs/adr/014-cpu-ozellik-bitleri.md).

**The stale-TLB check is advisory, and on QEMU the stale entry survived.**
The read after an uninvalidated unmap returned the old data instead of
faulting. That is the hazard `TlbFlush` exists to prevent, demonstrated
rather than argued — but QEMU's TLB is not real silicon and the opposite
result would also be legitimate, so the scenario reports instead of
asserting. **Re-run on real hardware** to see what the architecture
actually does here.

**`unmap` does not free intermediate page tables — in the kernel half.**
On the user side the question went away in M7: tables are PageTable
capabilities now, `Unmap` hands one back to its holder, and the kernel
never owned it to begin with
([ADR 027](docs/adr/027-vspace-modeli.md) closed
[ADR 015](docs/adr/015-ara-tablolar-serbest-birakilmiyor.md)).

The kernel's own tables still have the original problem, because there
the owner is the kernel. It has no practical instance: kernel mappings
are built at boot and never removed, the heap only grows, and KSTACK
slots are fixed, so nothing calls `unmap` on that path. The first thing
that does — unmapping a dead thread's stack is the likely candidate,
**M9** — brings the question back exactly as ADR 015 left it.

**The frame allocator still has no lock.** Single CPU, interrupts disabled,
every entry point funnelled through one non-reentrant function. Documented
in [ADR 007](docs/adr/007-bitmap-frame-allocator.md); becomes wrong the
moment a second CPU starts.

**`ACPI_RECLAIMABLE` is still untouched.** The tables have not been read
yet, and neither M6 nor M7 needed them: the LAPIC's base comes from the
`IA32_APIC_BASE` MSR rather than the MADT, which is enough for one CPU.
Finding the *other* CPUs needs the MADT, and that is whichever milestone
brings SMP. Unchanged from
[ADR 008](docs/adr/008-bootloader-reclaimable-dokunulmaz.md).

**The serial port has no lock.** Interrupts are enabled now, so a handler
that printed while the kernel was mid-line would interleave with it and
break `run.py`'s line matching. M6 is safe because the timer handler is
silent — but that is a property of this handler, not of the driver. The
first noisy one needs a lock. Named in
[ADR 025](docs/adr/025-kesme-disiplini.md).

**Threads have no FPU or SSE state.** Space is reserved in the TCB and
nothing is saved. Kernel threads do not use SSE — the build passes
`-mno-sse` — so there is nothing to lose yet. **M8's** user threads make
this mandatory.

**A thread's address space cannot be changed while it runs.** `SetSpace`
is refused for a busy thread with `ThreadBusy`, for the same reason
`SetRegisters` is: the scheduler is holding a reference, and moving CR3
under a thread it is about to switch into would put it somewhere it
never agreed to. Suspend first. That is also what keeps
[ADR 028](docs/adr/028-vspace-yasam-dongusu.md)'s refusal to revoke a
live VSpace from being a trap.

**Nothing cleans up a thread's address space when the thread dies.**
The VSpace outlives the TCB, and reclaiming it means revoking the
Untyped it came from — which takes the stack and the TCB with it. That
is correct but coarse, and it is the same gap as "threads cannot be
destroyed" below. **M9.**

**A VSpace's user half cannot be shared between threads.** Two threads
can be given the same root capability and will then share everything,
or different roots and share nothing. Anything in between — a shared
buffer between two otherwise isolated components — needs the same Frame
mapped into two VSpaces, which the capability model allows but nothing
has exercised. ADR 020 named shared memory as a precondition for **M9**
rather than something M7 provides.

**The scheduler is policy, and policy does not belong in a microkernel.**
Round-robin, a 10 ms quantum, and a priority field that is stored but
never read. It is in the kernel because a user-space scheduler needs IPC
(M9) and ring 3 (M8), and M6 has neither.
[ADR 024](docs/adr/024-zamanlayici-kernelde.md) names the three functions
that leave in **M10** and the mechanism that stays.

**Threads cannot be destroyed, only suspended.** `Suspend` works and
revoking a suspended thread's TCB works, but nothing unmaps the stack or
returns the KSTACK slot — a thread's memory is reclaimed only when its
whole Untyped is revoked. Proper teardown needs the zombie state that
[ADR 026](docs/adr/026-tcb-yasam-dongusu.md) deferred, and the IPC
blocking semantics that give it meaning. **M9.**

**No 5-level paging (LA57).** The address map assumes 48-bit virtual
addresses. Enabling LA57 would move the kernel half's base. Deliberate;
see [ADR 009](docs/adr/009-adres-uzayi-yerlesimi.md).

## Address space

The virtual address layout is fixed as of the M3 architect pass — in
particular the **user/kernel boundary at `0xFFFF_8000_0000_0000`, which will
not move when user space arrives in M8**. The full map, with an owner and a
milestone for every range, is in
[docs/address-space.md](docs/address-space.md).

## Not yet built

Deliberately absent, each belonging to a later milestone: **IPC's
implementation** (M9's architect pass fixed the semantics; no code
yet), device interrupts beyond the timer (the legacy PICs are masked
and the IOAPIC is untouched), other CPUs, a loader (M8's user programs
are assembled by hand, byte by byte, because anything longer would
need one), and a user-space scheduler.

Three things arrived in M8 as debts rather than features, and each is
written down where it will be found:

* **The syscall path assumes one CPU.** `KERNEL_RSP` and
  `SAVED_USER_RSP` are single statics reached RIP-relative. Correct
  today, wrong the moment a second CPU exists — the fix is `swapgs`
  and per-CPU data, done properly rather than this made cleverer.
  [ADR 030](docs/adr/030-syscall-abi.md) and
  [`hardware-todo.md`](docs/hardware-todo.md) item 7.
* **AVX is avoided rather than handled.** `fxsave` covers x87, MMX and
  XMM0-15; CR4.OSXSAVE stays clear so the state the kernel cannot save
  is state user code cannot reach. Untested, and silent if wrong.
  [ADR 031](docs/adr/031-fpu-sse-durumu.md), item 8.
* **W^X is simultaneous, not permanent.** No page is writable and
  executable *at once*; write → unmap → remap executable is possible
  and not prevented. [ADR 032](docs/adr/032-user-tarafinda-wx.md) says
  so explicitly rather than letting the term imply more than it
  delivers.

Everything *present* but incomplete is in **Known gaps** above, with the
milestone that owes it. Everything that cannot be verified under QEMU is
in [docs/hardware-todo.md](docs/hardware-todo.md). Everything awaiting a
decision is in [docs/m5-acik-sorular.md](docs/m5-acik-sorular.md).