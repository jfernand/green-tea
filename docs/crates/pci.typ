#import "../lib.typ": *

= green-tea-pci <crate-pci>

`green-tea-pci` reads PCI configuration space through ECAM, the PCI Express mechanism that maps each function's 4 KiB of configuration registers into memory. It enumerates functions, reads their identity, BARs and capabilities, and turns on memory decoding and DMA. It assigns nothing: on `x86_64` `q35`, SeaBIOS has already placed ECAM at `0xb000_0000`, numbered the buses and given the BARs their addresses.

#spec(
  ("Source", [#repo("drivers/pci/src/lib.rs"), 130 lines]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-virtio` (the `pci` transport), `green-tea-bare` (the `x86_64` board)]),
  ("Tests", [no host tests; the `disk` kernel on `x86_64` is the only kernel that calls it]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`Ecam(pub usize)`], [a configuration window, by base address],
  [`Function`], [the address of a function: `bus`, `device` (0–31), `function` (0–7)],
  [`read_u8`, `read_u16`, `read_u32`, `write_u16`], [one volatile access to a function's register, at its own width and alignment],
  [`vendor_id`, `device_id`, `subsystem_id`], [the identity registers, at `0x00`, `0x02` and `0x2e`],
  [`for_each_function(f)`], [calls `f` with every function that exists],
  [`bar(f, index)`], [the memory address in a BAR, or `None`],
  [`for_each_capability(function, f)`], [calls `f` with the offset and id of each capability],
  [`enable(f)`], [sets the memory-decode and bus-master bits of the command register],
)

== Addressing

ECAM gives each function one 4 KiB page, so 256 buses of 32 devices of 8 functions take 256 MiB:

#bytefield(caption: "An ECAM offset, from the window's base",
  ("bus", "bits 27–20", 8), ("device", "bits 19–15", 5), ("function", "bits 14–12", 3), ("register", "bits 11–0", 12),
)


== Enumeration

A function that doesn't exist reads as all ones, so a vendor id of `0xffff` means nobody's there. Function 0 of a device has to exist for the others to, and bit 7 of its header type says whether to look at the other seven. `for_each_function` scans all 256 buses, 8,192 vendor-id reads before it counts a function, and doesn't walk bridges: the firmware has numbered them, and a flat scan finds everything.


== BARs

A Base Address Register says where a function's registers are. Bit 0 separates I/O space from memory, and bits 2 and 1 give a memory BAR's width:

#srcfile("drivers/pci/src/lib.rs", lines: ((100, 111),), size: 6pt, notes: ((105, [`0b00` is a 32-bit BAR, `0b10` a 64-bit one that takes this slot and the next. The legacy `0b01` falls through to `None`.]), (110, [Address 0 means the firmware didn't assign this BAR.]),))[```rust
pub fn bar(&self, f: Function, index: usize) -> Option<u64> {
    let low = self.read_u32(f, BAR0 + 4 * index);
    if low & 1 != 0 {
        return None; // I/O space
    }
    let address = match (low >> 1) & 0b11 {
        0b00 => u64::from(low & !0xf),
        0b10 if index < 5 => u64::from(low & !0xf) | u64::from(self.read_u32(f, BAR0 + 4 * (index + 1))) << 32,
        _ => return None,
    };
    (address != 0).then_some(address)
}
```]

== Capabilities

If bit 4 of the status register is set, the byte at `0x34` points at a linked list of capabilities in configuration space: each has an id at its first byte and the next offset at its second. The low two bits of a pointer are reserved and masked off. `for_each_capability` follows the list until a pointer of 0.


This list is how `green-tea-virtio` finds a device's registers. `enable` then sets `COMMAND_MEMORY` and `COMMAND_BUS_MASTER`, without which the function ignores accesses to its BARs and can't do DMA.

== Exercised by

The `x86_64` board's `for_each_virtio` scans the bus with `for_each_function` and hands each function to `virtio::pci::probe`, which uses every other method here. The recording shows the device found and working over PCI:

#output(title: "cargo xtask kernel --kernel disk --target x86_64 --headless")[
```
virtio: block device (id 2)
virtio-blk over PCI: 64 sectors (32 KiB), id "green-tea-disk", write cache
PASS: read 64 sectors over virtio-PCI, and wrote one back
```
]

== Limits

- Read-mostly: it assigns no BARs, programs no bridges and sets up no interrupts (no MSI or MSI-X). On `riscv64` and `aarch64` `virt` nothing would have done that work, which is one reason those use virtio over MMIO.
- `bar` doesn't validate `index` beyond the 64-bit case, and doesn't look at the header type. A bridge has two BARs, and an index past them reads other registers as if they were a BAR.
- `for_each_capability` has no guard against a cycle in the list (line 119): a malformed device hangs the scan.
- The offset arguments are unchecked, and an unaligned one breaks the contract in the comment on line 49.

A flat scan of a window the firmware set up. @crate-virtio is what the scan is looking for.
