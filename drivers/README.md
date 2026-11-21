# Drivers

One crate per driver, for green-tea's bare-metal kernels. A driver knows its device, not
the machine: where the device lives (an MMIO base, an I/O port, an ECAM window) is the
caller's to say, and green-tea-bare's platforms say it.

| crate | device | used on |
|---|---|---|
| [`green-tea-ns16550`](ns16550) | 16550 UART, memory-mapped or on I/O ports | riscv64, x86_64 |
| [`green-tea-pl011`](pl011) | Arm PL011 UART | aarch64 |
| [`green-tea-fw-cfg`](fw-cfg) | QEMU's firmware configuration device | all |
| [`green-tea-framebuffer`](framebuffer) | Drawing on a linear XRGB8888 framebuffer | all |
| [`green-tea-ramfb`](ramfb) | QEMU's ramfb display, set up through fw_cfg | all |
| [`green-tea-pci`](pci) | PCI configuration space through ECAM | x86_64 |
| [`green-tea-virtio`](virtio) | virtio: negotiation, split virtqueues, the MMIO and PCI transports | all |
| [`green-tea-virtio-blk`](virtio-blk) | virtio-blk, a disk, over any virtio transport | all |
| [`green-tea-multiboot2`](multiboot2) | The boot information GRUB leaves: memory, framebuffer, command line | x86_64 `pc` |
| [`green-tea-acpi`](acpi) | ACPI tables: the CPUs, the HPET and the power management timer | x86_64 `pc` |
| [`green-tea-i8042`](i8042) | A PS/2 keyboard, polled | x86_64 `pc` |
| [`green-tea-xhci`](xhci) | A polled xHCI (USB 3) controller, and a USB keyboard | x86_64 `pc` |
| [`green-tea-io`](io) | not a driver: what they share (`dma_fence`, x86 port I/O) | |

All are `no_std` and are built for the bare-metal targets; the ones with logic worth
testing (framebuffer, multiboot2, acpi, i8042, xhci, pci, fw-cfg, virtio, virtio-blk) also run host tests, against fake devices
in memory. They assume what green-tea-bare provides: memory mapped one to one, so a
buffer's address is also the physical address a device needs for DMA. They take no locks:
where a kernel starts several CPUs, keeping them from using one device at once is the
caller's job. green-tea-virtio allocates its queues, so a kernel using it needs a global
allocator.
