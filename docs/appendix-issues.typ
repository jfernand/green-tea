#import "lib.typ": *

= Known issues <app-issues>

Writing this report turned up 73 defects in the code at commit #raw(pinned), from unsound APIs to stale comments. None was fixed, since the report describes the code as it is. Each was checked a second time, independently of the section that found it, by reading the cited code, and by running a small program wherever that was cheap: the last column says which. The sections' Limits discuss them in context.

== The ones that matter most

- *Three safe APIs are unsound.* Dropping a suspended `green-tea-v2` task unmaps a stack that a `thread::scope` may still be using (@crate-v2). `green-tea-smp`'s `Guard<Cell<u32>>` is `Sync`, and two threads racing through one lost half of 10,000,000 increments (@crate-smp). `green-tea-smr`'s `Rcu::update` can drop an `Rc` on another thread (@crate-smr).
- *A layout that hasn't happened yet would silently remove every guard page.* If the heap ever starts on a 2 MiB boundary, on riscv64 or x86_64, a page table entry is overwritten and stack overflows go undetected. None of the twelve debug and release builds is affected today, and the assertions wouldn't catch one that was (@bare-limits).
- *On aarch64, a fault on the boot CPU hangs without a message* in every kernel but `preempt` and the end of `incremental`, because its exception vectors aren't installed. This one comes from reading the code, and wasn't provoked.
- *A context switch doesn't carry the floating-point control state*, so a rounding mode set by one task applies to the next, on all three architectures (@crate-arch).
- *The virtio driver breaks the specification's status rule*: on failure `negotiate` clears status bits, which virtio 1.2 section 2.1.1 forbids (@crate-virtio).
- *`green-tea-measure time` fails intermittently, for a reason nobody has found.* Its check that no page was mapped during a timed pass failed in between 4 and 10 of every 100 runs (@crate-measure).
- *The x86_64 port assumes QEMU*: PVH entry, fw_cfg, a serial port at `0x3f8`, fixed device addresses, and APIC ids equal to CPU numbers. These stand between green-tea and physical hardware (@crate-bare).

== green-tea-v1

#dtable(columns: (31%, 1fr, auto), ("Where", "Defect", "Checked"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-v1/src/context.rs:87-101".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`run` is not reentrant: a nested `run` overwrites `MAIN` (:94) and on return leaves `CURRENT` null (:95), and the outer task's next `yield_now` segfaults on that null `CURRENT` (fault address 0x10, exit 139), not on the overwritten `MAIN`.], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-v1/src/context.rs:81-83".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`yield_now` outside a task dereferences a null `CURRENT` and segfaults (exit 139, debug and release).], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-v1/src/stack.rs:17-34".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The results of `mmap` and `mprotect` are not checked.], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-v1/src/stack.rs:12".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The comment "16 KiB on Apple Silicon" sits in a crate whose assembly is x86\_64 only.], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-v1/Cargo.toml:16".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`test-log` is a normal dependency, used only by `#[test_log::test]` in the tests of context.rs.], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-v1/src/context.s:24, green-tea-arch/src/x86_64.s:24".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The comment says `call` put the return address there, but on a first switch `spawn` / `Context::new` wrote it.], [read],
)

== green-tea-arch

#dtable(columns: (31%, 1fr, auto), ("Where", "Defect", "Checked"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-arch/src/{x86_64,aarch64,riscv64}.s".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [FP control state (mxcsr and x87 CW, fpcr, fcsr) is not switched, so a rounding mode set in a task is still set after it yields, on all three architectures.], [run],
)

== green-tea-v2

#dtable(columns: (31%, 1fr, auto), ("Where", "Defect", "Checked"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-v2/src/task.rs:68".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Dropping a suspended task from safe code unmaps its stack while a `thread::scope` on it is open: the scoped thread then segfaults (exit 139), or reads 0 from a local holding 7 if a new `Task` has been mapped at that address.], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-v2/src/runtime.rs:30-34".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The documented panic of `run` inside `run` is raised in a task and becomes a process abort (exit 134).], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-v2/src/task.rs:89".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A panic in a task aborts with "failed to initiate panic, error 5" and runs no destructors, which the comment about the `extern "C"` frame does not convey.], [run],
)

== green-tea-kmem

#dtable(columns: (31%, 1fr, auto), ("Where", "Defect", "Checked"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-kmem/src/frames.rs:93-95".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Reserved pages can be freed: free pages went from 12 to 13.], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-kmem/src/slab.rs:70-72,247-258".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A failed `grow` doesn't count its constructor or destructor calls (3 succeeded and 3 destructed; stats show 0 and 0).], [run],
)

== green-tea-smp

#dtable(columns: (31%, 1fr, auto), ("Where", "Defect", "Checked"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-smp/src/lock.rs:44".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`Guard` has no `Sync` bound of its own, so `Guard<Cell<u32>>` is `Sync` and safe code can race: two threads made 10,000,000 increments and the cell ended near 5,000,000.], [run],
)

== green-tea-smr

#dtable(columns: (31%, 1fr, auto), ("Where", "Defect", "Checked"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-smr/src/rcu.rs:45".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`Rcu::update` retires `T` with no `T: Send`: a retired `Rc` orphaned in an `Epochs` that is then moved to another thread is dropped on that thread.], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-smr/src/lib.rs:78".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`AddAssign` sums the peak fields, giving an upper bound and not a peak.], [read],
)

== green-tea-lisp

#dtable(columns: (31%, 1fr, auto), ("Where", "Defect", "Checked"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/src/read.rs:101-102, lib.rs:117".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Integers beyond 61 bits wrap: 2^60 reads as -2^60.], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/src/eval.rs:258".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`(if #t)` evaluates to nil and not a syntax error.], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/src/print.rs:51, read.rs:97-106".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A pair and the three-element list `(1 . 2)` print identically, because `.` reads as a symbol.], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/src/generational.rs:578-582".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`stores_old` and `old_to_young` stay 0 under `Barrier::Every`, being incremented only in the `Filter` arm.], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/src/incremental.rs:133-135,556-564".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`Steps::pauses` also counts stalls (here 0 steps, 41 stalls, 41 pauses), and a two-part stall records two pauses.], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/src/gc.rs:29-30".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The doc of `walked` omits overflow rescans, which add to it (mark.rs:188).], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/src/lib.rs:9-12".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The doc says two heaps can replace `RcHeap`; four in this crate do (mark, semispace, generational, incremental).], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/programs/queue.lisp:2-3".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The comment says at most eight wait, but nine do, briefly, between the enqueue and the dequeue from i = 9 on.], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/examples/incremental.rs:74".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The printed header says "Chapter 13".], [read],
)

== The drivers

#dtable(columns: (31%, 1fr, auto), ("Where", "Defect", "Checked"),
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio/src/lib.rs:113,121".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`negotiate` writes `STATUS_FAILED` alone, clearing bits already set, against virtio 1.2 section 2.1.1 "The driver MUST NOT clear a device status bit".], [read],
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio-blk/src/lib.rs:94".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A `Queue::new` failure returns without setting `STATUS_FAILED`, leaving the device part-initialized.], [read],
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio-blk/src/lib.rs:175".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The used ring's byte count is ignored, so a short read reported OK goes undetected.], [read],
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio/src/queue.rs:84,88".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The buffer length is cast to `u32`, silently shortening a buffer of 4 GiB or more.], [read],
  [#text(font: font-mono, size: 6.6pt, "drivers/fw-cfg/src/lib.rs:136".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The DMA length has the same unchecked `u32` cast.], [read],
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio/src/queue.rs:111".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A queue already in use is reported as "no such queue", because `max_queue_size` returns 0 for both on PCI and MMIO.], [read],
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio/src/pci.rs:92".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`probe` enables memory decoding and bus mastering before checking for the common and notify capabilities, and leaves them on when it returns `None`.], [read],
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio/src/pci.rs:167".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`notify` on a queue index below 4 that was never set up writes to address 0 (the volatile write is not trapped in debug or release).], [run],
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio/src/pci.rs:162".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`setup_queue` with an index of 4 or more panics after writing the queue registers; `Queue::new` cannot reach this, since `max_queue_size` returns 0 first, so it takes a direct call.], [read],
  [#text(font: font-mono, size: 6.6pt, "drivers/pci/src/lib.rs:119".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`for_each_capability` has no cycle guard, so a malformed list hangs.], [read],
  [#text(font: font-mono, size: 6.6pt, "drivers/pci/src/lib.rs:101".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`bar` neither bounds `index` nor checks the header type (a bridge has two BARs).], [read],
  [#text(font: font-mono, size: 6.6pt, "drivers/ramfb/src/lib.rs:66,68".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`outline` underflows when `thickness` exceeds `y + h` or `x + w`.], [read],
  [#text(font: font-mono, size: 6.6pt, "drivers/pl011/src/lib.rs:21-22".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The comment says QEMU's UART starts enabled, but QEMU 8.2.2 resets CR to 0x300 (TXE and RXE set, UARTEN clear); the kernel works because `enable()` sets UARTEN at boot (aarch64.rs:89) before the first output, although QEMU 8.2.2 does not check the bit either.], [read],
  [#text(font: font-mono, size: 6.6pt, "drivers/ramfb/src/lib.rs:19-20".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The comment says there is no paging, but riscv64 and x86\_64 page through an identity map.], [read],
  [#text(font: font-mono, size: 6.6pt, "drivers/ramfb/src/lib.rs:23-24".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The comment says one handle on a single hart, but the type is `Copy`, `init` can be called again, and the smp kernels run four CPUs.], [read],
  [#text(font: font-mono, size: 6.6pt, "drivers/io/src/lib.rs:4, drivers/README.md:19".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Both say the drivers assume one CPU, but the smp, shard and smr kernels start four and no driver takes a lock.], [read],
)

== green-tea-bare

#dtable(columns: (31%, 1fr, auto), ("Where", "Defect", "Checked"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-bare/src/platform/aarch64.rs:88,355".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The boot CPU's only VBAR\_EL1 write is in `init_timer`, called only by preempt and the last stage of incremental, while started CPUs install vectors (:80), so in the SMP kernels a fault is reported on CPUs 1-3 but not CPU 0 (not provoked).], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-bare/src/platform/x86_64.rs:45,264,289,297,314,318; aarch64.rs:123".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The boards assume QEMU: PVH, fw\_cfg CPU count, COM1 at 0x3f8, fixed HPET, LAPIC and ECAM, APIC ID equal to CPU number, one STARTUP, and on aarch64 the low byte of MPIDR\_EL1.], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-bare/src/platform/x86_64.rs:757, riscv64.rs:480".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [If `__heap_start` is a multiple of 2 MiB, the third `HEAP_4K` table replaces the stack pool's entry and maps every guard page, and the assertions allow it; no shipped build is affected (table below), and a private build with the heap aligned to 2 MiB lost the guard pages on both architectures.], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-bare/src/cpus.rs:70".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`run` after `park` waits forever if any other CPU was started (it spins in `together`, :87, for CPUs that have halted); with only the boot CPU it returns normally.], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-bare/src/platform/riscv64.rs:277, x86_64.rs:546".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [All CPUs share one fault stack, so simultaneous faults overwrite each other's frames; x86\_64.rs:524-525 documents the sharing.], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-bare/src/arena.rs:35".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`Arena::new(0)` passes a zero-sized layout to `alloc`, breaking the `GlobalAlloc` contract.], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-bare/src/kmem.rs:32".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The kernel assumes, unchecked, that RAM extends `REGION` (16 MiB) past `__kernel_end`; 128 MiB is only what xtask gives QEMU, as kmem.rs:5-7 documents.], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-shell/src/main.rs:10, green-tea-coop/src/main.rs:5,439, green-tea-bare/src/lib.rs:1".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Stale comments: the shell's heap is called first-fit (it is TLSF), coop says the heap doesn't free, coop's "no interrupts, one CPU" is true only of the demo, and bare's crate doc omits the shell.], [read],
)

== green-tea-shell

#dtable(columns: (31%, 1fr, auto), ("Where", "Defect", "Checked"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-shell/src/main.rs:214".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`repeat` ignores the command's result, so `repeat 1 halt` does not halt.], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-shell/src/main.rs:214".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A nested `repeat` is an unknown command whose error quiet mode hides, and it increments `unknown`, so a later `test` prints "FAIL: 1 unknown command(s)".], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-shell/src/main.rs:241".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A second `trace on` frees the old buffer after the new recording starts, putting a stray free in the trace (`f 803e80a0` as its only event).], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-shell/src/region.rs:251-252".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`regions` discards a recording in progress and all variables without telling the user.], [run],
)

== green-tea-trace

#dtable(columns: (31%, 1fr, auto), ("Where", "Defect", "Checked"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-trace/src/main.rs:291, green-tea-shell/src/replay.rs:259".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Off by one: "before 64 more" counts lifetimes up to and including 64, where the "before 8" beside it uses 7.], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-trace/src/main.rs:106".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`--shuffle` swallows a following file name that parses as hex (`cafe` gives "wrong number of files"), and the usage doesn't say the seed is hex.], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-trace/src/main.rs:102".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`--region-kib` has no upper limit: 5,000,000 panics in `Buddy::new` (buddy.rs:52, exit 101).], [run],
)

== green-tea-measure

#dtable(columns: (31%, 1fr, auto), ("Where", "Defect", "Checked"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/main.rs:450".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The `time` page check is flaky: FAIL in 7 and then 4 of 100 identical runs (10 and 8 of 100 in the batches recorded for this report); in the second batch all 4 charged one page to bump, the first allocator, and a failure in the first batch mapped 2 pages; cause not found.], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/replay.rs:163".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The instruction count includes one `reset` inside the measured function, worth at most 0.64% of an allocator's count (0.72% net of the loop; slabs) on shell.gtt.], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/main.rs:692-803".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`lisp instructions` floors differ by heap, and the mark-sweep floor is 2.04-2.34x the explicit floor across the five programs.], [run],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/oracle.rs:31, main.rs:657".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`left` / "reachable at the end" also counts objects that died after the last allocation, since the dead are found only before an allocation.], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/main.rs:332,764".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Any valgrind or run failure is reported as a refused allocation or "doesn't fit".], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/main.rs:92".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Every error prints the full usage.], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/subjects.rs:74".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`describe` has no caller.], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/subjects.rs:623".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`Shards::reset` leaks one `Arena` per reset (`Box::leak` at :583, reached from :624).], [read],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/stats.rs:15".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The comment says five ranks, and `Summary` keeps six.], [read],
)

== xtask and the workspace

#dtable(columns: (31%, 1fr, auto), ("Where", "Defect", "Checked"),
  [#text(font: font-mono, size: 6.6pt, "xtask/src/main.rs:9-24, xtask/src/kernel.rs:110-114".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`cargo xtask --help` omits `--jobs` and has no usage line for `shell`, and its `kernel` paragraph names 6 of the 25 kernels (green-tea-shell appears only under `shell` and `trace`); the option list closing `kernel --help` omits `--jobs`, which that help's usage line and text do give.], [run],
  [#text(font: font-mono, size: 6.6pt, "xtask/Cargo.toml:3".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The description mentions only `cargo xtask test`.], [read],
  [#text(font: font-mono, size: 6.6pt, "Cargo.toml:25".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`green-tea-measure` is listed twice in `default-members` (harmless).], [read],
  [#text(font: font-mono, size: 6.6pt, "xtask/src/main.rs:85".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`cargo xtask test --help` fails with "--help needs a value", exit 1.], [run],
  [#text(font: font-mono, size: 6.6pt, "xtask/src/kernel.rs:540-541".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A kernel named twice is queued twice on one log path: the second start unlinks the first's log and creates a new one, and both runs then follow that one file, so the first's verdict can come from the second's output (run: both reported ok, one log).], [run],
  [#text(font: font-mono, size: 6.6pt, "xtask/src/kernel.rs:296".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`=== build` is printed for builds that are then skipped.], [run],
  [#text(font: font-mono, size: 6.6pt, "xtask/src/kernel.rs:434-438".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`target_dir` honours only `CARGO_TARGET_DIR`, not `build.target-dir`.], [read],
  [#text(font: font-mono, size: 6.6pt, "xtask/src/kernel.rs:467-487".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`shell --target` with no value says "unknown option --target".], [run],
  [#text(font: font-mono, size: 6.6pt, "xtask/src/kernel.rs:615-617".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The first verdict is final, so a PASS followed by a panic counts as passed.], [read],
)
