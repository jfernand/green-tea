#import "lib.typ": *

= Overview <overview>

green-tea is a Rust workspace of 44 crates and 47,094 lines, in which every level of a small system is written by hand: a context switch, a scheduler, allocators, device drivers, kernels, a Lisp to allocate for, and the tools that measure the result. It began as green threads under Linux, and the kernels still run the same context switch. All of it is `MIT OR Apache-2.0`, and none of it is published as a crate.

The kernels run in QEMU, on its `virt` machine for riscv64 and aarch64 and on `q35` for x86_64. A second x86_64 board, `pc`, is written for physical PCs: GRUB boots it from UEFI through Multiboot2, and it finds the machine from the boot information and ACPI. It has been run only in QEMU, under UEFI firmware, and @app-issues says what that leaves unproven.

== The workspace

Crates fall into seven groups, which are the seven parts of this report. Each group depends only on the groups below it in @fig-layers, except that the tools of Part VII also use the allocators directly, and the realtime policy depends on nothing.

#figure(layers(
  ("Tooling", [`green-tea-trace`, `green-tea-measure`, `xtask`], "amber"),
  ("Realtime", [`green-tea-rt`, `green-tea-rt-bare`, and the `rt` and `latency` kernels], "dark"),
  ("Kernels", [`green-tea-coop`, `green-tea-preempt`, `green-tea-shell`, `green-tea-black`, on the schedulers `scheduler-coop` and `scheduler-preempt`, on `green-tea-bare`], "dark"),
  ("Drivers", [`io`, `ns16550`, `pl011`, `fw-cfg`, `framebuffer`, `ramfb`, `pci`, `virtio`, `virtio-blk`, `multiboot2`, `acpi`, `ioapic`, `i8042`, `xhci`, `nvme`, `clevo-lights`], "light"),
  ("Protection", [`capabilities`, `memory`, `dma`], "light"),
  ("Allocators", [`pool`, `fit`, `kmem`, `region`, `smp`, `shard`, `smr`, `lisp`], "light"),
  ("Green threads", [`green-tea-v1`, `green-tea-arch`, `green-tea-v2`], "light"),
), kind: image, caption: [The workspace, by what builds on what.]) <fig-layers>

#dtable(columns: (auto, auto, 1fr), ("Crate", "Lines", "What it is"),
  [#link(<crate-v1>)[`green-tea-v1`]], [396], [Green threads on x86_64: one global scheduler context],
  [#link(<crate-arch>)[`green-tea-arch`]], [217], [`Context` and `swap_context` for three architectures],
  [#link(<crate-v2>)[`green-tea-v2`]], [730], [Green threads in layers: arch, task, runtime],
  [#link(<crate-pool>)[`green-tea-pool`]], [366], [Fixed-size pools: a bitmap and a free list],
  [#link(<crate-fit>)[`green-tea-fit`]], [4,393], [First, next and best fit, buddy, TLSF, a hardened heap, traces],
  [#link(<crate-kmem>)[`green-tea-kmem`]], [1,402], [Page frames from a buddy system, and slab caches],
  [#link(<crate-region>)[`green-tea-region`]], [726], [Regions, with lifetimes checked by the compiler],
  [#link(<crate-smp>)[`green-tea-smp`]], [718], [A spin lock, and per-CPU caches over a shared heap],
  [#link(<crate-shard>)[`green-tea-shard`]], [1,221], [Lock-free allocation, after mimalloc],
  [#link(<crate-smr>)[`green-tea-smr`]], [1,496], [Lock-free structures, epochs, hazard pointers, RCU],
  [#link(<crate-lisp>)[`green-tea-lisp`]], [6,914], [A Lisp on counted, traced, generational and incremental heaps],
  [#link(<crate-capabilities>)[`green-tea-capabilities`]], [221], [Build-time profiles, and the capabilities each board implements],
  [#link(<crate-memory>)[`green-tea-memory`]], [366], [Checked physical ranges, ownership tokens, and requests checked against a board],
  [#link(<crate-dma>)[`green-tea-dma`]], [633], [A fail-closed contract for DMA domains over an IOMMU; no backend yet],
  [#link(<crate-io>)[`green-tea-io`]], [76], [`dma_fence` and x86 port I/O],
  [#link(<crate-ns16550>)[`green-tea-ns16550`]], [67], [16550 UART],
  [#link(<crate-pl011>)[`green-tea-pl011`]], [42], [Arm PL011 UART],
  [#link(<crate-fw-cfg>)[`green-tea-fw-cfg`]], [245], [QEMU's firmware configuration device],
  [#link(<crate-framebuffer>)[`green-tea-framebuffer`]], [294], [Drawing on a linear framebuffer, and a text console],
  [#link(<crate-ramfb>)[`green-tea-ramfb`]], [43], [QEMU's `ramfb` display],
  [#link(<crate-pci>)[`green-tea-pci`]], [276], [PCI configuration space through ECAM],
  [#link(<crate-virtio>)[`green-tea-virtio`]], [1,020], [Virtio: negotiation, split virtqueues, two transports],
  [#link(<crate-virtio-blk>)[`green-tea-virtio-blk`]], [339], [A virtio disk],
  [#link(<crate-multiboot2>)[`green-tea-multiboot2`]], [245], [The boot information GRUB hands over],
  [#link(<crate-acpi>)[`green-tea-acpi`]], [540], [The CPUs, clocks and interrupt routes in the firmware's ACPI tables],
  [#link(<crate-ioapic>)[`green-tea-ioapic`]], [143], [The IO APIC, which routes device interrupts to a CPU],
  [#link(<crate-i8042>)[`green-tea-i8042`]], [244], [A PS/2 keyboard controller, read when its interrupt says so],
  [#link(<crate-xhci>)[`green-tea-xhci`]], [1,443], [A polled xHCI (USB 3) controller, and a USB keyboard],
  [#link(<crate-nvme>)[`green-tea-nvme`]], [721], [A polled NVMe disk],
  [#link(<crate-clevo-lights>)[`green-tea-clevo-lights`]], [287], [A Clevo laptop's keyboard lights],
  [#link(<crate-bare>)[`green-tea-bare`]], [5,411], [Boot, platforms, heap, paging, CPUs, interrupts, user processes],
  [#link(<crate-scheduler-coop>)[`green-tea-scheduler-coop`]], [386], [Task records, cooperative switching and a round-robin runtime],
  [#link(<crate-scheduler-preempt>)[`green-tea-scheduler-preempt`]], [82], [Timer preemption, on the cooperative scheduler],
  [#link(<crate-coop>)[`green-tea-coop`]], [6,142], [The cooperative kernel and its test kernels],
  [#link(<crate-preempt>)[`green-tea-preempt`]], [197], [A kernel whose tasks are preempted by a timer],
  [#link(<crate-shell>)[`green-tea-shell`]], [1,960], [A shell on the serial console],
  [#link(<crate-black>)[`green-tea-black`]], [204], [User processes in ring 3 on x86_64, isolated from the kernel and each other],
  [#link(<crate-rt>)[`green-tea-rt`]], [1,002], [A fixed-priority policy: ready queues, timers, events, inheritance, admission],
  [#link(<crate-rt-bare>)[`green-tea-rt-bare`]], [534], [The policy's preallocated bare-metal runtime],
  [#link(<crate-rt-kernel>)[`green-tea-rt-kernel`]], [171], [A kernel that runs the policy's scenarios],
  [#link(<crate-latency>)[`green-tea-latency`]], [334], [A histogram of timer latency on x86_64],
  [#link(<crate-trace>)[`green-tea-trace`]], [595], [Replays a trace file on the host],
  [#link(<crate-measure>)[`green-tea-measure`]], [2,643], [One trace against every allocator],
  [#link(<crate-xtask>)[`xtask`]], [1,609], [Builds, boots and judges everything],
)

Line counts include each crate's tests and examples.

== Targets

The workspace has two kinds of code, and they build differently.

*Host code* is everything in the workspace's `default-members`: the green threads, the allocators, the Lisp, the tools, and the nine drivers with host tests, which run against fake devices in memory. Plain `cargo test` builds and runs it. The allocators are `no_std` and take their memory as an address and a length, so the code a kernel runs is the code the host tests.

*Kernel code* is `green-tea-bare`, the schedulers, all the drivers and the six kernels. As kernel code they build for `riscv64gc-unknown-none-elf`, `aarch64-unknown-none` and `x86_64-unknown-none`, and run through `cargo xtask kernel`.

The boards differ in what they offer, and the differences recur throughout:

#dtable(columns: (auto, 1fr, 1fr, 1fr, 1fr), ("", "riscv64", "aarch64", "x86_64 `q35`", "x86_64 `pc`"),
  [Machine], [`virt`, OpenSBI], [`virt`, GICv2], [`q35`, PVH boot], [a PC: UEFI, GRUB, Multiboot2],
  [Serial port], [16550, memory-mapped], [PL011], [16550, port `0x3f8`], [16550 at `0x3f8`, if there is one],
  [Screen], [`ramfb`], [`ramfb`], [`ramfb`], [GRUB's framebuffer],
  [Input], [serial], [serial], [serial], [serial, PS/2, USB],
  [Waiting for input], [polled], [polled], [polled], [halted, until an interrupt],
  [Paging], [identity map], [MMU off], [identity map], [identity map],
  [Guard pages], [yes], [no], [yes], [yes],
  [Demand paging], [yes], [no], [yes], [yes],
  [Disk], [virtio MMIO], [virtio MMIO], [virtio PCI], [NVMe],
)

== Building and running

#codepanel(title: "The commands")[```sh
cargo test                                   # every host crate
cargo xtask test                             # green-tea-v2 on three architectures, under qemu-user
cargo xtask kernel --headless                # every kernel, everywhere it runs
cargo xtask kernel --kernel tlsf --target x86_64
cargo xtask kernel --board pc --headless     # every kernel, for a physical PC, in QEMU
cargo xtask shell --target riscv64           # type at the shell
cargo xtask trace --out shell.gtt            # record a trace from the shell
cargo run -p green-tea-trace -- replay shell.gtt  # replay it on the host
```]

At the commit this report describes, the host tests pass:

#output(title: "cargo test")[
```
host tests: 605 passed, 0 failed, 0 ignored
```
]

The kernels judge themselves. Each prints a line starting with `PASS:` or `FAIL:`, and `xtask` reads it from the serial log. @app-kernels lists all 30. Twenty-nine of them make 78 runs across the three architectures, and all 30 run on the `pc` board. In release builds, `overflow` fails on riscv64 and x86_64 at #raw(pinned) (@app-issues).

== Conventions in this report

Every listing is from commit #raw(pinned), with its file and line numbers in the bar above it. Lines may be cut, which is marked `// …`, and nothing else is changed. Numbered badges refer to the notes under a listing. Every output is from a recorded run, and lines may be left out of it. @app-method says how both are checked.
// chapter-status:begin
// stage: research-done
// revision: 6
// body: 8d46ba866b37
// activity: 2026-11-22 | r4 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r4 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
