#import "lib.typ": *

= Every kernel <app-kernels>

`cargo xtask kernel` runs 25 kernels, and a 26th on the `pc` board. Three binaries provide them: `green-tea-coop` selects a test through the fw_cfg file `opt/green-tea/test`, `green-tea-shell` is driven by what `xtask` types at its serial console, and `green-tea-preempt` has one behaviour. Each runs on all three architectures unless the table says otherwise, which makes 72 runs. `smp`, `shard` and `smr` get four CPUs and the rest get one.

#dtable(columns: (auto, auto, 1fr), ("Kernel", "Binary", "What it checks"),
  [`coop`], [coop], [Tasks yield to each other, and the number of switches is as expected],
  [`preempt`], [preempt], [A timer preempts tasks that never yield, and one that sleeps],
  [`overflow`], [coop], [A task overflowing its stack hits its guard page. riscv64, x86_64],
  [`disk`], [coop], [Reads a disk image over virtio-blk (NVMe on `pc`), and writes a sector],
  [`memmap`], [coop], [The order, alignment and size of the linker's sections],
  [`pool`], [coop], [A bitmap and a free list given the same takes and gives, and 1,000 tasks on one reused stack],
  [`fit`], [coop], [Freed blocks merge, the three policies choose differently, and 1,000 tasks leave the heap as they found it],
  [`frag`], [coop], [An adversary and a random workload, against every policy],
  [`tlsf`], [coop], [Buddy and TLSF against first and best fit, on a heap built to make a search long],
  [`slab`], [coop], [Page frames, a slab cache, and the cache task records come from],
  [`pages`], [coop], [Demand paging: a megabyte reserved, written, freed and given back. riscv64, x86_64],
  [`smp`], [coop], [Four CPUs on the kernel's heap behind its lock, then with per-CPU caches],
  [`lisp`], [coop], [The Lisp on a reference-counted heap, with cycles leaked and collected],
  [`shard`], [coop], [Four CPUs allocating with no lock, against the caches of `smp`],
  [`gc`], [coop], [The Lisp on mark-sweep, mark-compact and copying heaps],
  [`smr`], [coop], [A lock-free stack and queue, freeing too early, then epochs and hazard pointers],
  [`gen`], [coop], [The Lisp with a nursery, a write barrier and a remembered set],
  [`incremental`], [coop], [Marking in steps, with and without a write barrier, and steps paced by a timer],
  [`safety`], [coop], [Six memory errors against the allocator, one defence more each time],
  [`shell`], [shell], [Commands free what they allocate, from the heap or an arena],
  [`trace`], [shell], [Records a session and replays it against each policy],
  [`harden`], [shell], [The recorded trace against the hardened allocator],
  [`region`], [shell], [The session with its memory in regions],
  [`gtt`], [shell], [A recording written as a trace file in both forms and read back],
  [`resident`], [shell], [The recording replayed on unmapped pages, counting pages touched. riscv64, x86_64],
  [`interrupts`], [shell], [Bytes at the serial port wake the halted shell through the IO APIC. `pc` only],
)

The whole suite, in release and headless, under the default limit of half the host's CPUs, passes all 72. The first lines of its summary:

#output(title: "cargo xtask kernel --release --headless")[
```
=== summary
ok    coop-riscv64    57 switches, as expected
ok    coop-aarch64    57 switches, as expected
ok    coop-x86_64     57 switches, as expected
```
]

== On the pc board

`cargo xtask kernel --board pc` runs all 26 on x86_64, with `disk` on an NVMe drive. `smp`, `shard` and `smr` again get four CPUs. A test is named on the kernel's command line by GRUB, in place of the fw_cfg file, and the shell kernels get their input over a socket once the banner has appeared (@crate-xtask). All 26 pass, under QEMU with UEFI firmware. The first lines of the summary, in release and with one virtual CPU running at a time:

#output(title: "cargo xtask kernel --release --headless --board pc --jobs 1")[
```
=== summary
ok    coop-x86_64     57 switches, as expected
```
]

@crate-coop, @crate-preempt and @crate-shell describe each in detail.
