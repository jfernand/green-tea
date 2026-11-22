#import "lib.typ": *

= Known issues and fixes <app-issues>

This appendix has two parts: what is open at commit #raw(pinned), and the 73 defects found in the code at `7898396`, with how each was resolved. Of those 73, 71 are fixed and 2 are open.

== Open at #raw(pinned)

=== The two defects left

- *A port to other physical hardware is unproven.* The `pc` board is written for physical x86_64 PCs, and it has run only in QEMU, under OVMF. All 26 of its kernels pass there (@app-kernels), which shows the code works against QEMU's firmware and devices, and says nothing about any particular machine's. QEMU's own boards keep their assumptions on purpose, and on aarch64 a CPU's number is still the low byte of `MPIDR_EL1`.
- *`lisp instructions` measures each heap against its own floor.* The floor of a garbage-collected heap is that heap with room never to collect, and the mark-sweep floor is 2.1 to 2.4 times the explicit one across the five programs, because `MarkHeap` checks every object and field access and a release `Explicit` doesn't. The columns over the floor therefore compare the cost of reclamation, and not the cost of running a program on each heap (@crate-measure).

=== Limits of the pc board and USB

- *No USB hubs.* The xHCI driver brings up devices on the controller's root ports, so a keyboard behind a hub isn't found.
- *No hot-plugging.* The keyboard must be attached at boot.
- *No USB storage.* A USB keyboard is the only device class the driver uses. The stick the kernel boots from is read by the firmware and GRUB.
- *Secure Boot must be off*, because the GRUB that `xtask` builds isn't signed.
- *The NVMe test disk.* The `disk` kernel uses only the controller whose serial is `green-tea-disk`. It resets the others to ask them, and neither reads nor writes their blocks (@crate-nvme).
- *Device interrupts are for input only.* NVMe, xHCI, virtio and the other architectures still poll, and a device interrupt is taken only while the CPU is idle (@bare-limits).
- *The keyboard lights are unconfirmed.* The driver has sent nothing to a real EC (@crate-clevo-lights).

=== Found while describing the pc board

Writing the sections on the `pc` board turned up these problems in the code at `59e8c91`. Commit `1363d86` fixed the first six, and `8762171` the last two. The rest are open.

#dtable(columns: (1fr, 1.1fr), ("Problem", "State"),
  [A CPU the MADT marks Online Capable but not Enabled counted as present, and `start_all` could panic waiting for it.], [Fixed in `1363d86`: only Enabled counts.],
  [A framebuffer that isn't XRGB, or none, stopped the boot.], [Fixed in `1363d86`: the kernels draw into memory.],
  [An XSDT above 4 GiB left no tables, with no fallback.], [Fixed in `1363d86` for the root table. An entry of a readable XSDT that points above 4 GiB still reads as absent.],
  [`pm_timer_port` ignored the FADT's `X_PM_TMR_BLK`.], [Fixed in `1363d86`, for an I/O port. A timer in memory space isn't supported.],
  [Device registers were mapped write-back, relying on the MTRRs.], [Fixed in `1363d86`: `map_device`. The framebuffer stays write-back.],
  [The USB arena ran out at 256 scratchpads, firmware that kept the controller wasn't overridden, and a keyboard error stopped input for good.], [Fixed in `1363d86`: a 6 MiB arena, a forced handoff, and endpoint recovery.],
  [The TSC is assumed invariant and synchronised between CPUs, with no CPUID check.], [Open (@bare-limits).],
  [A report shorter than 8 bytes is decoded with the tail of the previous one.], [Open (@crate-xhci).],
  [A configuration descriptor with a total length of 0 produces a setup TRB with no data, followed by a data TRB.], [Open (@crate-xhci).],
  [A keyboard that stalled the refused `SET_IDLE` leaves endpoint 0 halted, so a later endpoint recovery fails and the keyboard is given up.], [Fixed in `8762171`: endpoint 0 is reset and moved past the request.],
  [An NVMe controller already in a fatal state was reported as an error, with no attempt to reset it.], [Fixed in `8762171`: it is reset.],
)

*What hasn't run.* Nothing here has run on physical hardware. The xHCI firmware handoff, the scratchpad pages and endpoint recovery have run only in host tests, against fake registers and a fake controller, because QEMU's controller exercises none of them. The NVMe driver has run on QEMU's device and a fake one.

=== Smaller items

- *The guard-page regression has no test that CI can run.* The fix for a heap starting on a 2 MiB boundary (`7646655`) was checked with the `pages` and `overflow` kernels in a private build with `.heap` aligned to 2 MiB, on riscv64 and x86\_64. No shipped build has that layout, and CI builds only the shipped ones.
- *A dropped suspended `green-tea-v2` task leaks 32 KiB.* Its destructors can't run, and a `thread::scope` open in one of its frames may still be using the stack, so `Drop` leaks the task's state and its stack of `STACK_SIZE` (@crate-v2).
- *aarch64's `fpsr` is still shared.* `swap_context` switches `fpcr` and not `fpsr`, so the cumulative exception flags one task sets are visible to the next (@crate-arch).
- *`green-tea-measure --region-kib` has no upper limit.* A region of 5,000,000 KiB with `buddy` among the allocators panics in `Buddy::new` (@crate-measure). `green-tea-trace` refuses the same value.
- *Four driver tests allocate an untouched 4 GiB `Vec`*, to check that a length of 4 GiB or more is refused: one in `green-tea-fw-cfg`, two in `green-tea-virtio`'s `queue.rs` and one in `green-tea-virtio-blk`. The memory is zeroed on demand and never touched, so it isn't resident, but the tests need 4 GiB of address space.
- *`cargo xtask --help` doesn't mention `--board`.* `cargo xtask kernel --help` does (@crate-xtask).

== Found at `7898396`, and how each was resolved

Writing the first version of this report turned up these 73 defects, from unsound APIs to stale comments. Each was checked a second time, independently of the section that found it, by reading the cited code, and by running a small program wherever that was cheap. The tables keep the paths and line numbers as they were at `7898396`, so they locate the code in that commit and not in this one. The last column says how each was resolved and gives the commit, from `git log 7898396..59e8c91`. A commit could be identified for all 71 fixes. Of the two left open, the QEMU assumptions list the commits that removed them from the `pc` board, and the Lisp floors have none. A commit whose fix has a test has a `Before the fix:` line in its message, with the failure that test produced on the old code.

=== green-tea-v1

#dtable(columns: (22%, 1fr, 30%), ("Where, at 7898396", "Defect", "How it was resolved"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-v1/src/context.rs:87-101".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`run` is not reentrant: a nested `run` overwrites `MAIN` (:94) and on return leaves `CURRENT` null (:95), and the outer task's next `yield_now` segfaults on that null `CURRENT` (fault address 0x10, exit 139), not on the overwritten `MAIN`.], [`run` puts `MAIN` and `CURRENT` back when it returns. `22bc2cd`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-v1/src/context.rs:81-83".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`yield_now` outside a task dereferences a null `CURRENT` and segfaults (exit 139, debug and release).], [Panics, with the same message as v2. `16b6071`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-v1/src/stack.rs:17-34".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The results of `mmap` and `mprotect` are not checked.], [Both results are checked, and a failure panics. `66f0e51`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-v1/src/stack.rs:12".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The comment "16 KiB on Apple Silicon" sits in a crate whose assembly is x86\_64 only.], [Comment corrected. `cc41e0b`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-v1/Cargo.toml:16".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`test-log` is a normal dependency, used only by `#[test_log::test]` in the tests of context.rs.], [`test-log` is a dev-dependency. `cc41e0b`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-v1/src/context.s:24, green-tea-arch/src/x86_64.s:24".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The comment says `call` put the return address there, but on a first switch `spawn` / `Context::new` wrote it.], [Comment corrected in both crates. `cc41e0b`, `62546fc`],
)

=== green-tea-arch

#dtable(columns: (22%, 1fr, 30%), ("Where, at 7898396", "Defect", "How it was resolved"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-arch/src/{x86_64,aarch64,riscv64}.s".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [FP control state (mxcsr and x87 CW, fpcr, fcsr) is not switched, so a rounding mode set in a task is still set after it yields, on all three architectures.], [`Context` holds this state, and a new context starts in the default mode. aarch64's `fpsr` is still shared (see above). `b2e28fb`],
)

=== green-tea-v2

#dtable(columns: (22%, 1fr, 30%), ("Where, at 7898396", "Defect", "How it was resolved"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-v2/src/task.rs:68".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Dropping a suspended task from safe code unmaps its stack while a `thread::scope` on it is open: the scoped thread then segfaults (exit 139), or reads 0 from a local holding 7 if a new `Task` has been mapped at that address.], [A suspended task keeps its state and stack, which are leaked (see above). `85b5003`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-v2/src/runtime.rs:30-34".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The documented panic of `run` inside `run` is raised in a task and becomes a process abort (exit 134).], [`task_entry` catches the panic, and `resume` continues it on the resumer's stack. `2fc18f3`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-v2/src/task.rs:89".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A panic in a task aborts with "failed to initiate panic, error 5" and runs no destructors, which the comment about the `extern "C"` frame does not convey.], [The same change: the panic reaches the resumer, and destructors run. `2fc18f3`],
)

=== green-tea-kmem

#dtable(columns: (22%, 1fr, 30%), ("Where, at 7898396", "Defect", "How it was resolved"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-kmem/src/frames.rs:93-95".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Reserved pages can be freed: free pages went from 12 to 13.], [`free` refuses them with `FreeError::Reserved`. `4f47d7e`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-kmem/src/slab.rs:70-72,247-258".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A failed `grow` doesn't count its constructor or destructor calls (3 succeeded and 3 destructed; stats show 0 and 0).], [A failed slab's calls are counted. `e484c9e`],
)

=== green-tea-smp

#dtable(columns: (22%, 1fr, 30%), ("Where, at 7898396", "Defect", "How it was resolved"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-smp/src/lock.rs:44".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`Guard` has no `Sync` bound of its own, so `Guard<Cell<u32>>` is `Sync` and safe code can race: two threads made 10,000,000 increments and the cell ended near 5,000,000.], [A guard is `Sync` only if its value is. `cf425ef`],
)

=== green-tea-smr

#dtable(columns: (22%, 1fr, 30%), ("Where, at 7898396", "Defect", "How it was resolved"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-smr/src/rcu.rs:45".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`Rcu::update` retires `T` with no `T: Send`: a retired `Rc` orphaned in an `Epochs` that is then moved to another thread is dropped on that thread.], [`update` requires `T: Send`. `9a1fba4`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-smr/src/lib.rs:78".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`AddAssign` sums the peak fields, giving an upper bound and not a peak.], [Comment now says the sum is an upper bound. `ab1fccd`],
)

=== green-tea-lisp

#dtable(columns: (22%, 1fr, 30%), ("Where, at 7898396", "Defect", "How it was resolved"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/src/read.rs:101-102, lib.rs:117".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Integers beyond 61 bits wrap: 2^60 reads as -2^60.], [A literal outside 61 bits is a reader error. `53e379e`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/src/eval.rs:258".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`(if #t)` evaluates to nil and not a syntax error.], [A syntax error: only the second branch may be left out. `4b5c1b6`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/src/print.rs:51, read.rs:97-106".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A pair and the three-element list `(1 . 2)` print identically, because `.` reads as a symbol.], [The reader reads a dotted pair. `34b66e1`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/src/generational.rs:578-582".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`stores_old` and `old_to_young` stay 0 under `Barrier::Every`, being incremented only in the `Filter` arm.], [Documented: only the filtering barrier counts them. The behaviour is unchanged. `4631de0`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/src/incremental.rs:133-135,556-564".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`Steps::pauses` also counts stalls (here 0 steps, 41 stalls, 41 pauses), and a two-part stall records two pauses.], [Each step, stall and collection is timed once, and only steps go in the buckets. `01ed5a1`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/src/gc.rs:29-30".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The doc of `walked` omits overflow rescans, which add to it (mark.rs:188).], [Doc corrected. `4631de0`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/src/lib.rs:9-12".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The doc says two heaps can replace `RcHeap`; four in this crate do (mark, semispace, generational, incremental).], [Doc corrected. `4631de0`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/programs/queue.lisp:2-3".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The comment says at most eight wait, but nine do, briefly, between the enqueue and the dequeue from i = 9 on.], [Comment corrected. `4631de0`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-lisp/examples/incremental.rs:74".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The printed header says "Chapter 13".], [Header corrected. `4631de0`],
)

=== The drivers

#dtable(columns: (22%, 1fr, 30%), ("Where, at 7898396", "Defect", "How it was resolved"),
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio/src/lib.rs:113,121".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`negotiate` writes `STATUS_FAILED` alone, clearing bits already set, against virtio 1.2 section 2.1.1 "The driver MUST NOT clear a device status bit".], [A new `failed()` sets the bit on top of the others. `34f3a8d`],
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio-blk/src/lib.rs:94".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A `Queue::new` failure returns without setting `STATUS_FAILED`, leaving the device part-initialized.], [`Blk::new` marks the device failed. `b9b3035`],
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio-blk/src/lib.rs:175".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The used ring's byte count is ignored, so a short read reported OK goes undetected.], [A read must cover the data and the status byte. `189f66d`],
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio/src/queue.rs:84,88".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The buffer length is cast to `u32`, silently shortening a buffer of 4 GiB or more.], [`Buffer` panics on one, and virtio-blk returns `Error::TooLong` first. `017d3df`],
  [#text(font: font-mono, size: 6.6pt, "drivers/fw-cfg/src/lib.rs:136".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The DMA length has the same unchecked `u32` cast.], [Returns an error. `dae3763`],
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio/src/queue.rs:111".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A queue already in use is reported as "no such queue", because `max_queue_size` returns 0 for both on PCI and MMIO.], [`Transport::queue_in_use` tells the two apart. `e80de3d`],
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio/src/pci.rs:92".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`probe` enables memory decoding and bus mastering before checking for the common and notify capabilities, and leaves them on when it returns `None`.], [The device is enabled only once `probe` accepts it. `ca49caa`],
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio/src/pci.rs:167".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`notify` on a queue index below 4 that was never set up writes to address 0 (the volatile write is not trapped in debug or release).], [Panics. `d7a9ff6`],
  [#text(font: font-mono, size: 6.6pt, "drivers/virtio/src/pci.rs:162".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`setup_queue` with an index of 4 or more panics after writing the queue registers; `Queue::new` cannot reach this, since `max_queue_size` returns 0 first, so it takes a direct call.], [The index is checked before any register is written. `0ace158`],
  [#text(font: font-mono, size: 6.6pt, "drivers/pci/src/lib.rs:119".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`for_each_capability` has no cycle guard, so a malformed list hangs.], [Each slot is visited once, and only past the header. `59dbc40`],
  [#text(font: font-mono, size: 6.6pt, "drivers/pci/src/lib.rs:101".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`bar` neither bounds `index` nor checks the header type (a bridge has two BARs).], [Bounded by the function's header type. `9c865ea`],
  [#text(font: font-mono, size: 6.6pt, "drivers/ramfb/src/lib.rs:66,68".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`outline` underflows when `thickness` exceeds `y + h` or `x + w`.], [The bands are limited to the rectangle. `d035846`],
  [#text(font: font-mono, size: 6.6pt, "drivers/pl011/src/lib.rs:21-22".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The comment says QEMU's UART starts enabled, but QEMU 8.2.2 resets CR to 0x300 (TXE and RXE set, UARTEN clear); the kernel works because `enable()` sets UARTEN at boot (aarch64.rs:89) before the first output, although QEMU 8.2.2 does not check the bit either.], [Comment corrected. `b7a016e`],
  [#text(font: font-mono, size: 6.6pt, "drivers/ramfb/src/lib.rs:19-20".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The comment says there is no paging, but riscv64 and x86\_64 page through an identity map.], [Comment corrected. `e22b4ae`],
  [#text(font: font-mono, size: 6.6pt, "drivers/ramfb/src/lib.rs:23-24".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The comment says one handle on a single hart, but the type is `Copy`, `init` can be called again, and the smp kernels run four CPUs.], [Comment corrected. `e22b4ae`],
  [#text(font: font-mono, size: 6.6pt, "drivers/io/src/lib.rs:4, drivers/README.md:19".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Both say the drivers assume one CPU, but the smp, shard and smr kernels start four and no driver takes a lock.], [Both now say that serializing a device is the caller's job. `ecaaafc`],
)

=== green-tea-bare

#dtable(columns: (22%, 1fr, 30%), ("Where, at 7898396", "Defect", "How it was resolved"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-bare/src/platform/aarch64.rs:88,355".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The boot CPU's only VBAR\_EL1 write is in `init_timer`, called only by preempt and the last stage of incremental, while started CPUs install vectors (:80), so in the SMP kernels a fault is reported on CPUs 1-3 but not CPU 0 (not provoked).], [`init` installs the vectors on the boot CPU. `c2b36e4`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-bare/src/platform/x86_64.rs:45,264,289,297,314,318; aarch64.rs:123".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The boards assume QEMU: PVH, fw\_cfg CPU count, COM1 at 0x3f8, fixed HPET, LAPIC and ECAM, APIC ID equal to CPU number, one STARTUP, and on aarch64 the low byte of MPIDR\_EL1.], [*Open in part* (see above). x86\_64 has a `pc` board that looks these up; `q35` keeps them by design, and aarch64 still reads the low byte of `MPIDR_EL1`. `cabf744`, `f5f74c9`, `bf0f380`, `871a298`, `d8515f0`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-bare/src/platform/x86_64.rs:757, riscv64.rs:480".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [If `__heap_start` is a multiple of 2 MiB, the third `HEAP_4K` table replaces the stack pool's entry and maps every guard page, and the assertions allow it; no shipped build is affected (table below), and a private build with the heap aligned to 2 MiB lost the guard pages on both architectures.], [Only the tables the heap is in are installed. `7646655`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-bare/src/cpus.rs:70".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`run` after `park` waits forever if any other CPU was started (it spins in `together`, :87, for CPUs that have halted); with only the boot CPU it returns normally.], [`park` takes the CPUs out of the count. `dd373fd`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-bare/src/platform/riscv64.rs:277, x86_64.rs:546".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [All CPUs share one fault stack, so simultaneous faults overwrite each other's frames; x86\_64.rs:524-525 documents the sharing.], [Each CPU has its own fault stack. `c5a6997`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-bare/src/arena.rs:35".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`Arena::new(0)` passes a zero-sized layout to `alloc`, breaking the `GlobalAlloc` contract.], [An empty arena takes nothing from the heap. `77cca6d`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-bare/src/kmem.rs:32".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The kernel assumes, unchecked, that RAM extends `REGION` (16 MiB) past `__kernel_end`; 128 MiB is only what xtask gives QEMU, as kmem.rs:5-7 documents.], [x86\_64 reads the RAM's size from fw\_cfg, and the page allocator panics if the region doesn't fit. `a0ea1b3`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-shell/src/main.rs:10, green-tea-coop/src/main.rs:5,439, green-tea-bare/src/lib.rs:1".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Stale comments: the shell's heap is called first-fit (it is TLSF), coop says the heap doesn't free, coop's "no interrupts, one CPU" is true only of the demo, and bare's crate doc omits the shell.], [Comments corrected, one commit each. `74bf294`, `8faa5e1`, `abe165f`],
)

=== green-tea-shell

#dtable(columns: (22%, 1fr, 30%), ("Where, at 7898396", "Defect", "How it was resolved"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-shell/src/main.rs:214".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`repeat` ignores the command's result, so `repeat 1 halt` does not halt.], [`repeat` stops and halts. `30149c1`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-shell/src/main.rs:214".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A nested `repeat` is an unknown command whose error quiet mode hides, and it increments `unknown`, so a later `test` prints "FAIL: 1 unknown command(s)".], [`repeat` refuses it and says so. `da6de36`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-shell/src/main.rs:241".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A second `trace on` frees the old buffer after the new recording starts, putting a stray free in the trace (`f 803e80a0` as its only event).], [The old recording is stopped first. `f104cde`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-shell/src/region.rs:251-252".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`regions` discards a recording in progress and all variables without telling the user.], [`regions` refuses while either exists. `718c1ba`],
)

=== green-tea-trace

#dtable(columns: (22%, 1fr, 30%), ("Where, at 7898396", "Defect", "How it was resolved"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-trace/src/main.rs:291, green-tea-shell/src/replay.rs:259".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Off by one: "before 64 more" counts lifetimes up to and including 64, where the "before 8" beside it uses 7.], [Counts lifetimes below 64. `344ebf9`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-trace/src/main.rs:106".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`--shuffle` swallows a following file name that parses as hex (`cafe` gives "wrong number of files"), and the usage doesn't say the seed is hex.], [A seed must follow the file, and the usage says it is hexadecimal. `d1c31c6`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-trace/src/main.rs:102".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`--region-kib` has no upper limit: 5,000,000 panics in `Buddy::new` (buddy.rs:52, exit 101).], [A usage error above 4 GiB. `b470f46`],
)

=== green-tea-measure

#dtable(columns: (22%, 1fr, 30%), ("Where, at 7898396", "Defect", "How it was resolved"),
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/main.rs:450".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The `time` page check is flaky: FAIL in 7 and then 4 of 100 identical runs (10 and 8 of 100 in the batches recorded for this report); in the second batch all 4 charged one page to bump, the first allocator, and a failure in the first batch mapped 2 pages; cause not found.], [Two first writes moved out of the counted window: 29 of 400 runs failed before, 0 of 400 after. `d5078b5`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/replay.rs:163".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The instruction count includes one `reset` inside the measured function, worth at most 0.64% of an allocator's count (0.72% net of the loop; slabs) on shell.gtt.], [The reset is left out of the counted loop. `49945f6`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/main.rs:692-803".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`lisp instructions` floors differ by heap, and the mark-sweep floor is 2.04-2.34x the explicit floor across the five programs.], [*Open* (see above).],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/oracle.rs:31, main.rs:657".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`left` / "reachable at the end" also counts objects that died after the last allocation, since the dead are found only before an allocation.], [`finish` marks once more, and keeps those apart in `died_after_last`. `f7bb63e`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/main.rs:332,764".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Any valgrind or run failure is reported as a refused allocation or "doesn't fit".], [Only a run that says it had no room is reported that way. `7999d23`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/main.rs:92".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [Every error prints the full usage.], [Only a mistake on the command line prints the usage. `a34b813`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/subjects.rs:74".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`describe` has no caller.], [Removed. `0cb3f2e`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/subjects.rs:623".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`Shards::reset` leaks one `Arena` per reset (`Box::leak` at :583, reached from :624).], [The old arena is freed. `536f6e4`],
  [#text(font: font-mono, size: 6.6pt, "green-tea-measure/src/stats.rs:15".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The comment says five ranks, and `Summary` keeps six.], [Comment corrected. `9465fea`],
)

=== xtask and the workspace

#dtable(columns: (22%, 1fr, 30%), ("Where, at 7898396", "Defect", "How it was resolved"),
  [#text(font: font-mono, size: 6.6pt, "xtask/src/main.rs:9-24, xtask/src/kernel.rs:110-114".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`cargo xtask --help` omits `--jobs` and has no usage line for `shell`, and its `kernel` paragraph names 6 of the 25 kernels (green-tea-shell appears only under `shell` and `trace`); the option list closing `kernel --help` omits `--jobs`, which that help's usage line and text do give.], [Usage rewritten. `--board`, added later, is missing from it (see above). `fe558ce`],
  [#text(font: font-mono, size: 6.6pt, "xtask/Cargo.toml:3".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The description mentions only `cargo xtask test`.], [Description rewritten. `cb7560d`],
  [#text(font: font-mono, size: 6.6pt, "Cargo.toml:25".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`green-tea-measure` is listed twice in `default-members` (harmless).], [Listed once. `c8e8341`],
  [#text(font: font-mono, size: 6.6pt, "xtask/src/main.rs:85".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`cargo xtask test --help` fails with "--help needs a value", exit 1.], [Prints the usage and exits 0. `f71599f`],
  [#text(font: font-mono, size: 6.6pt, "xtask/src/kernel.rs:540-541".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [A kernel named twice is queued twice on one log path: the second start unlinks the first's log and creates a new one, and both runs then follow that one file, so the first's verdict can come from the second's output (run: both reported ok, one log).], [Each is kept once. `2cc9d4a`],
  [#text(font: font-mono, size: 6.6pt, "xtask/src/kernel.rs:296".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`=== build` is printed for builds that are then skipped.], [Only builds it makes are announced. `322d245`],
  [#text(font: font-mono, size: 6.6pt, "xtask/src/kernel.rs:434-438".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`target_dir` honours only `CARGO_TARGET_DIR`, not `build.target-dir`.], [Asks `cargo metadata`. `3a14ed0`],
  [#text(font: font-mono, size: 6.6pt, "xtask/src/kernel.rs:467-487".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [`shell --target` with no value says "unknown option --target".], [Says `--target needs a value`. `5a4c11a`],
  [#text(font: font-mono, size: 6.6pt, "xtask/src/kernel.rs:615-617".replace("/", "/\u{200B}").replace(",", ",\u{200B}"))], [The first verdict is final, so a PASS followed by a panic counts as passed.], [A later failure replaces a pass. `b6c6211`],
)
