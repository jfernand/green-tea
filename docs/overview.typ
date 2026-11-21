#import "lib.typ": *

= Overview <overview>

green-tea is a Rust workspace of 31 crates and 39,650 lines, in which every level of a small system is written by hand: a context switch, a scheduler, allocators, device drivers, kernels, a Lisp to allocate for, and the tools that measure the result. It began as green threads under Linux, and the kernels still run the same context switch. All of it is `MIT OR Apache-2.0`, and none of it is published as a crate.

The kernels run in QEMU, on its `virt` machine for riscv64 and aarch64 and on `q35` for x86_64. A second x86_64 board, `pc`, is written for physical PCs: GRUB boots it from UEFI through Multiboot2, and it finds the machine from the boot information and ACPI. It has been run only in QEMU, under UEFI firmware, and @app-issues says what that leaves unproven.

== The workspace

Crates fall into five groups, which are the five parts of this report. Each group depends only on the groups above it in @fig-layers, except that the tools of Part V also use the allocators directly.

#figure(layers(
  ("Tooling", [`green-tea-trace`, `green-tea-measure`, `xtask`], "amber"),
  ("Kernels", [`green-tea-coop`, `green-tea-preempt`, `green-tea-shell`, on `green-tea-bare`], "dark"),
  ("Drivers", [`io`, `ns16550`, `pl011`, `fw-cfg`, `framebuffer`, `ramfb`, `pci`, `virtio`, `virtio-blk`, `multiboot2`, `acpi`, `i8042`, `xhci`], "light"),
  ("Allocators", [`pool`, `fit`, `kmem`, `region`, `smp`, `shard`, `smr`, `lisp`], "light"),
  ("Green threads", [`green-tea-v1`, `green-tea-arch`, `green-tea-v2`], "light"),
), kind: image, caption: [The workspace, by what builds on what.]) <fig-layers>

#dtable(columns: (auto, auto, 1fr), ("Crate", "Lines", "What it is"),
  [#link(<crate-v1>)[`green-tea-v1`]], [390], [Green threads on x86_64: one global scheduler context],
  [#link(<crate-arch>)[`green-tea-arch`]], [217], [`Context` and `swap_context` for three architectures],
  [#link(<crate-v2>)[`green-tea-v2`]], [730], [Green threads in layers: arch, task, runtime],
  [#link(<crate-pool>)[`green-tea-pool`]], [328], [Fixed-size pools: a bitmap and a free list],
  [#link(<crate-fit>)[`green-tea-fit`]], [4,391], [First, next and best fit, buddy, TLSF, a hardened heap, traces],
  [#link(<crate-kmem>)[`green-tea-kmem`]], [1,257], [Page frames from a buddy system, and slab caches],
  [#link(<crate-region>)[`green-tea-region`]], [726], [Regions, with lifetimes checked by the compiler],
  [#link(<crate-smp>)[`green-tea-smp`]], [718], [A spin lock, and per-CPU caches over a shared heap],
  [#link(<crate-shard>)[`green-tea-shard`]], [1,221], [Lock-free allocation, after mimalloc],
  [#link(<crate-smr>)[`green-tea-smr`]], [1,496], [Lock-free structures, epochs, hazard pointers, RCU],
  [#link(<crate-lisp>)[`green-tea-lisp`]], [6,878], [A Lisp on counted, traced, generational and incremental heaps],
  [#link(<crate-io>)[`green-tea-io`]], [76], [`dma_fence` and x86 port I/O],
  [#link(<crate-ns16550>)[`green-tea-ns16550`]], [54], [16550 UART],
  [#link(<crate-pl011>)[`green-tea-pl011`]], [42], [Arm PL011 UART],
  [#link(<crate-fw-cfg>)[`green-tea-fw-cfg`]], [245], [QEMU's firmware configuration device],
  [#link(<crate-framebuffer>)[`green-tea-framebuffer`]], [294], [Drawing on a linear framebuffer, and a text console],
  [#link(<crate-ramfb>)[`green-tea-ramfb`]], [43], [QEMU's `ramfb` display],
  [#link(<crate-pci>)[`green-tea-pci`]], [257], [PCI configuration space through ECAM],
  [#link(<crate-virtio>)[`green-tea-virtio`]], [991], [Virtio: negotiation, split virtqueues, two transports],
  [#link(<crate-virtio-blk>)[`green-tea-virtio-blk`]], [339], [A virtio disk],
  [#link(<crate-multiboot2>)[`green-tea-multiboot2`]], [245], [The boot information GRUB hands over],
  [#link(<crate-acpi>)[`green-tea-acpi`]], [342], [The CPUs and clocks in the firmware's ACPI tables],
  [#link(<crate-i8042>)[`green-tea-i8042`]], [176], [A polled PS/2 keyboard controller],
  [#link(<crate-xhci>)[`green-tea-xhci`]], [1,095], [A polled xHCI (USB 3) controller, and a USB keyboard],
  [#link(<crate-bare>)[`green-tea-bare`]], [4,262], [Boot, platforms, heap, paging, CPUs, tasks, scheduler],
  [#link(<crate-coop>)[`green-tea-coop`]], [6,057], [The cooperative kernel and its test kernels],
  [#link(<crate-preempt>)[`green-tea-preempt`]], [240], [A kernel whose tasks are preempted by a timer],
  [#link(<crate-shell>)[`green-tea-shell`]], [1,780], [A shell on the serial console],
  [#link(<crate-trace>)[`green-tea-trace`]], [595], [Replays a trace file on the host],
  [#link(<crate-measure>)[`green-tea-measure`]], [2,639], [One trace against every allocator],
  [#link(<crate-xtask>)[`xtask`]], [1,526], [Builds, boots and judges everything],
)

Line counts include each crate's tests and examples.

== Targets

The workspace has two kinds of code, and they build differently.

*Host code* is everything in the workspace's `default-members`: the green threads, the allocators, the Lisp, the tools, and the nine drivers with host tests, which run against fake devices in memory. Plain `cargo test` builds and runs it. The allocators are `no_std` and take their memory as an address and a length, so the code a kernel runs is the code the host tests.

*Kernel code* is `green-tea-bare`, all the drivers and the three kernels. As kernel code they build for `riscv64gc-unknown-none-elf`, `aarch64-unknown-none` and `x86_64-unknown-none`, and run through `cargo xtask kernel`.

The boards differ in what they offer, and the differences recur throughout:

#dtable(columns: (auto, 1fr, 1fr, 1fr, 1fr), ("", "riscv64", "aarch64", "x86_64 `q35`", "x86_64 `pc`"),
  [Machine], [`virt`, OpenSBI], [`virt`, GICv2], [`q35`, PVH boot], [a PC: UEFI, GRUB, Multiboot2],
  [Serial port], [16550, memory-mapped], [PL011], [16550, port `0x3f8`], [16550 at `0x3f8`, if there is one],
  [Screen], [`ramfb`], [`ramfb`], [`ramfb`], [GRUB's framebuffer],
  [Input], [serial], [serial], [serial], [serial, PS/2, USB],
  [Paging], [identity map], [MMU off], [identity map], [identity map],
  [Guard pages], [yes], [no], [yes], [yes],
  [Demand paging], [yes], [no], [yes], [yes],
  [Virtio transport], [MMIO], [MMIO], [PCI], [none],
)

== Building and running

#codepanel(title: "The commands")[```sh
cargo test                                   # every host crate
cargo xtask test                             # green-tea-v2 on three architectures, under qemu-user
cargo xtask kernel --headless                # every kernel, everywhere it runs
cargo xtask kernel --kernel tlsf --target x86_64
cargo xtask kernel --board pc --headless     # every kernel but disk, for a physical PC, in QEMU
cargo xtask shell --target riscv64           # type at the shell
cargo xtask trace --out shell.gtt            # record a trace from the shell
cargo run -p green-tea-trace -- replay shell.gtt  # replay it on the host
```]

At the commit this report describes, the host tests pass:

#output(title: "cargo test")[
```
host tests: 524 passed, 0 failed, 0 ignored
```
]

The kernels judge themselves. Each prints a line starting with `PASS:` or `FAIL:`, and `xtask` reads it from the serial log. @app-kernels lists all 25, which make 72 runs across the three architectures, and 24 more on the `pc` board, where all but `disk` run.

== Conventions in this report

Every listing is from commit #raw(pinned), with its file and line numbers in the bar above it. Lines may be cut, which is marked `// …`, and nothing else is changed. Numbered badges refer to the notes under a listing. Every output is from a recorded run, and lines may be left out of it. @app-method says how both are checked.
