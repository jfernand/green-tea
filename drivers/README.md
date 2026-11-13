# Drivers

One crate per driver, for green-tea's bare-metal kernels. A driver knows its device, not
the machine: where the device lives (an MMIO base, an I/O port, an ECAM window) is the
caller's to say, and green-tea-bare's platforms say it.

| crate | device | used on |
|---|---|---|
| [`green-tea-ns16550`](ns16550) | 16550 UART, memory-mapped or on I/O ports | riscv64, x86_64 |
| [`green-tea-pl011`](pl011) | Arm PL011 UART | aarch64 |
| [`green-tea-fw-cfg`](fw-cfg) | QEMU's firmware configuration device | all |
| [`green-tea-ramfb`](ramfb) | QEMU's ramfb display, set up through fw_cfg | all |
| [`green-tea-pci`](pci) | PCI configuration space through ECAM | x86_64 |
| [`green-tea-virtio`](virtio) | virtio: negotiation, split virtqueues, the MMIO and PCI transports | all |
| [`green-tea-virtio-blk`](virtio-blk) | virtio-blk, a disk, over any virtio transport | all |
| [`green-tea-io`](io) | not a driver: what they share (`dma_fence`, x86 port I/O) | |

All are `no_std` and build only for the bare-metal targets. They assume what
green-tea-bare provides: one CPU, and memory mapped one to one, so a buffer's address is
also the physical address a device needs for DMA. green-tea-virtio allocates its queues,
so a kernel using it needs a global allocator.
