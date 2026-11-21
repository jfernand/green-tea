#import "lib.typ": *

= Overview <overview>

green-tea is a Rust workspace of 26 crates and 34,915 lines, in which every level of a small system is written by hand: a context switch, a scheduler, allocators, device drivers, kernels, a Lisp to allocate for, and the tools that measure the result. It began as green threads under Linux, and the kernels still run the same context switch. All of it is `MIT OR Apache-2.0`, and none of it is published as a crate.

The kernels run in QEMU, on its `virt` machine for riscv64 and aarch64 and on `q35` for x86_64. They have not been booted on physical hardware, and @app-issues lists what x86_64 assumes about QEMU.

== The workspace

Crates fall into five groups, which are the five parts of this report. Each group depends only on the groups above it in @fig-layers, except that the tools of Part V also use the allocators directly.

#figure(layers(
  ("Tooling", [`green-tea-trace`, `green-tea-measure`, `xtask`], "amber"),
  ("Kernels", [`green-tea-coop`, `green-tea-preempt`, `green-tea-shell`, on `green-tea-bare`], "dark"),
  ("Drivers", [`io`, `ns16550`, `pl011`, `fw-cfg`, `ramfb`, `pci`, `virtio`, `virtio-blk`], "light"),
  ("Allocators", [`pool`, `fit`, `kmem`, `region`, `smp`, `shard`, `smr`, `lisp`], "light"),
  ("Green threads", [`green-tea-v1`, `green-tea-arch`, `green-tea-v2`], "light"),
), kind: image, caption: [The workspace, by what builds on what.]) <fig-layers>

#dtable(columns: (auto, auto, 1fr), ("Crate", "Lines", "What it is"),
  [#link(<crate-v1>)[`green-tea-v1`]], [311], [Green threads on x86_64: one global scheduler context],
  [#link(<crate-arch>)[`green-tea-arch`]], [188], [`Context` and `swap_context` for three architectures],
  [#link(<crate-v2>)[`green-tea-v2`]], [563], [Green threads in layers: arch, task, runtime],
  [#link(<crate-pool>)[`green-tea-pool`]], [328], [Fixed-size pools: a bitmap and a free list],
  [#link(<crate-fit>)[`green-tea-fit`]], [4,391], [First, next and best fit, buddy, TLSF, a hardened heap, traces],
  [#link(<crate-kmem>)[`green-tea-kmem`]], [1,231], [Page frames from a buddy system, and slab caches],
  [#link(<crate-region>)[`green-tea-region`]], [726], [Regions, with lifetimes checked by the compiler],
  [#link(<crate-smp>)[`green-tea-smp`]], [697], [A spin lock, and per-CPU caches over a shared heap],
  [#link(<crate-shard>)[`green-tea-shard`]], [1,221], [Lock-free allocation, after mimalloc],
  [#link(<crate-smr>)[`green-tea-smr`]], [1,477], [Lock-free structures, epochs, hazard pointers, RCU],
  [#link(<crate-lisp>)[`green-tea-lisp`]], [6,765], [A Lisp on counted, traced, generational and incremental heaps],
  [#link(<crate-io>)[`green-tea-io`]], [66], [`dma_fence` and x86 port I/O],
  [#link(<crate-ns16550>)[`green-tea-ns16550`]], [54], [16550 UART],
  [#link(<crate-pl011>)[`green-tea-pl011`]], [42], [Arm PL011 UART],
  [#link(<crate-fw-cfg>)[`green-tea-fw-cfg`]], [202], [QEMU's firmware configuration device],
  [#link(<crate-ramfb>)[`green-tea-ramfb`]], [113], [QEMU's `ramfb` display],
  [#link(<crate-pci>)[`green-tea-pci`]], [130], [PCI configuration space through ECAM],
  [#link(<crate-virtio>)[`green-tea-virtio`]], [695], [Virtio: negotiation, split virtqueues, two transports],
  [#link(<crate-virtio-blk>)[`green-tea-virtio-blk`]], [191], [A virtio disk],
  [#link(<crate-bare>)[`green-tea-bare`]], [3,532], [Boot, platforms, heap, paging, CPUs, tasks, scheduler],
  [#link(<crate-coop>)[`green-tea-coop`]], [5,958], [The cooperative kernel and its test kernels],
  [#link(<crate-preempt>)[`green-tea-preempt`]], [240], [A kernel whose tasks are preempted by a timer],
  [#link(<crate-shell>)[`green-tea-shell`]], [1,679], [A shell on the serial console],
  [#link(<crate-trace>)[`green-tea-trace`]], [544], [Replays a trace file on the host],
  [#link(<crate-measure>)[`green-tea-measure`]], [2,372], [One trace against every allocator],
  [#link(<crate-xtask>)[`xtask`]], [1,199], [Builds, boots and judges everything],
)

Line counts include each crate's tests and examples.

== Targets

The workspace has two kinds of code, and they build differently.

*Host code* is everything in the workspace's `default-members`: the green threads, the allocators, the Lisp and the tools. Plain `cargo test` builds and runs it. The allocators are `no_std` and take their memory as an address and a length, so the code a kernel runs is the code the host tests.

*Kernel code* is `green-tea-bare`, the drivers and the three kernels. They build only for `riscv64gc-unknown-none-elf`, `aarch64-unknown-none` and `x86_64-unknown-none`, and run through `cargo xtask kernel`.

The boards differ in what they offer, and the differences recur throughout:

#dtable(columns: (auto, 1fr, 1fr, 1fr), ("", "riscv64", "aarch64", "x86_64"),
  [Machine], [`virt`, OpenSBI], [`virt`, GICv2], [`q35`, PVH boot],
  [Serial port], [16550, memory-mapped], [PL011], [16550, port `0x3f8`],
  [Paging], [identity map], [MMU off], [identity map],
  [Guard pages], [yes], [no], [yes],
  [Demand paging], [yes], [no], [yes],
  [Virtio transport], [MMIO], [MMIO], [PCI],
)

== Building and running

#codepanel(title: "The commands")[```sh
cargo test                                   # every host crate
cargo xtask test                             # green-tea-v2 on three architectures, under qemu-user
cargo xtask kernel --headless                # every kernel, everywhere it runs
cargo xtask kernel --kernel tlsf --target x86_64
cargo xtask shell --target riscv64           # type at the shell
cargo xtask trace --out shell.gtt            # record a trace from the shell
cargo run -p green-tea-trace -- replay shell.gtt  # replay it on the host
```]

At the commit this report describes, the host tests pass:

#output(title: "cargo test")[
```
host tests: 389 passed, 0 failed, 0 ignored
```
]

The kernels judge themselves. Each prints a line starting with `PASS:` or `FAIL:`, and `xtask` reads it from the serial log. @app-kernels lists all 25, which make 72 runs across the three architectures.

== Conventions in this report

Every listing is from commit #raw(pinned), with its file and line numbers in the bar above it. Lines may be cut, which is marked `// …`, and nothing else is changed. Numbered badges refer to the notes under a listing. Every output is from a recorded run, and lines may be left out of it. @app-method says how both are checked.
