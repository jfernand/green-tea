// (c) Javier Fernandez @ JotaSoft 2026
#import "isss-template.typ": *
#import "lib.typ": *

#show: isss-doc.with(
  title: "green-tea",
  subtitle: "Green threads, allocators and kernels, crate by crate",
  author: "Javier Fernandez",
  contact: "jotafernand.casaroja.es",
  date: "2026-11-21",
  docid: "ISSS-TR-0506",
  running: "green-tea — a technical report",
  abstract: [
    green-tea is a Rust workspace of 31 crates and about 39,650 lines. It starts from a
    context switch under Linux and ends with bare-metal kernels for riscv64, aarch64 and
    x86_64 in QEMU, a board for physical PCs, with pool, free-list, buddy, TLSF, slab, region and lock-free
    allocators, a small Lisp on reference-counted and tracing heaps, safe memory
    reclamation, device drivers, and tools that record and replay allocation traces.

    We describe everything in it, one section per crate, in dependency order: what each
    crate offers, how it works, how it is tested, and where it stops. Every listing is
    from commit #raw(pinned), and every quoted output is from a recorded run.
  ],
  meta: (
    ("Status", [Describes commit #raw(pinned)]),
    ("Targets", [Linux on x86_64, aarch64, riscv64; bare metal in QEMU on riscv64, aarch64, x86_64]),
    ("Repository", [#link(repo-url)]),
  ),
)

// Inline code in a heading takes the heading's own font and size.
#show heading: it => { show raw.where(block: false): r => r.text; it }

#outline(title: none, depth: 1, indent: 1em)
#pagebreak()

#include "overview.typ"

#part("Green threads on Linux", [A context switch, its machine layer, and a runtime on top of it.])
#include "crates/v1.typ"
#include "crates/arch.typ"
#include "crates/v2.typ"

#part("Allocators and reclamation", [The libraries the kernels allocate with. All of them run and are tested on the host.])
#include "crates/pool.typ"
#include "crates/fit.typ"
#include "crates/kmem.typ"
#include "crates/region.typ"
#include "crates/smp.typ"
#include "crates/shard.typ"
#include "crates/smr.typ"
#include "crates/lisp.typ"

#part("Drivers", [One crate per device, for QEMU's machines and a physical PC.])
#include "crates/io.typ"
#include "crates/ns16550.typ"
#include "crates/pl011.typ"
#include "crates/fw-cfg.typ"
#include "crates/framebuffer.typ"
#include "crates/ramfb.typ"
#include "crates/pci.typ"
#include "crates/virtio.typ"
#include "crates/virtio-blk.typ"
#include "crates/multiboot2.typ"
#include "crates/acpi.typ"
#include "crates/i8042.typ"
#include "crates/xhci.typ"

#part("Kernels", [The shared bare-metal layer, and the three kernels built on it.])
#include "crates/bare.typ"
#include "crates/coop.typ"
#include "crates/preempt.typ"
#include "crates/shell.typ"

#part("Tooling", [Replaying traces, measuring allocators, and driving all of it.])
#include "crates/trace.typ"
#include "crates/measure.typ"
#include "crates/xtask.typ"

#show: appendices
#include "appendix-kernels.typ"
#include "appendix-xtask.typ"
#include "appendix-issues.typ"
#include "appendix-method.typ"
