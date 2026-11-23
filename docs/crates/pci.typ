#import "../lib.typ": *

= green-tea-pci <crate-pci>

`green-tea-pci` reads PCI configuration space through ECAM, the PCI Express mechanism that maps each function's 4 KiB of configuration registers into memory. It enumerates functions, reads their identity, BARs and capabilities, and turns on memory decoding and DMA. It assigns nothing: on `x86_64` `q35`, SeaBIOS has already placed ECAM at `0xb000_0000`, numbered the buses and given the BARs their addresses, and on the `pc` board the firmware has done the same and ACPI's MCFG table says where.

#spec(
  ("Source", [#repo("drivers/pci/src/lib.rs"), 257 lines]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-virtio` (the `pci` transport), `green-tea-bare` (the `q35` and `pc` boards)]),
  ("Tests", [8 host tests, on a function's configuration space in ordinary memory; the `disk` kernel on `x86_64`, and every kernel on the `pc` board, which looks for its USB controller]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`Ecam(pub usize)`], [a configuration window, by base address],
  [`Function`], [the address of a function: `bus`, `device` (0–31), `function` (0–7)],
  [`read_u8`, `read_u16`, `read_u32`, `write_u16`], [one volatile access to a function's register, at its own width and alignment],
  [`vendor_id`, `device_id`, `subsystem_id`], [the identity registers, at `0x00`, `0x02` and `0x2e`],
  [`for_each_function(f)`], [calls `f` with every function that exists, on all 256 buses],
  [`for_each_function_on(buses, f)`], [the same, on these buses only],
  [`bar(f, index)`], [the memory address in a BAR, or `None`; bounded by the header type],
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

`for_each_function_on` is the same scan over a range of buses, and `for_each_function` calls it with `0..=255`. A window needn't cover all 256: MCFG gives the `pc` board a first and last bus, and reading past what the window maps is undefined.


== BARs

A Base Address Register says where a function's registers are. An endpoint (header type 0) has six and a bridge (type 1) two, and `bar` returns `None` past them and for any other header type. Bit 0 separates I/O space from memory, and bits 2 and 1 give a memory BAR's width:

#srcfile("drivers/pci/src/lib.rs", lines: ((107, 126),), size: 6pt, notes: ((108, [Bit 7 of the header type only says whether the device has more functions, so it is masked off.]), (120, [`0b00` is a 32-bit BAR, `0b10` a 64-bit one that takes this slot and the next, if there is a next. The legacy `0b01` falls through to `None`.]), (125, [Address 0 means the firmware didn't assign this BAR.]),))[```rust
pub fn bar(&self, f: Function, index: usize) -> Option<u64> {
    let slots = match self.read_u8(f, HEADER_TYPE) & !HEADER_MULTI_FUNCTION {
        0 => 6,
        1 => 2,
        _ => 0,
    };
    if index >= slots {
        return None;
    }
    let low = self.read_u32(f, BAR0 + 4 * index);
    if low & 1 != 0 {
        return None; // I/O space
    }
    let address = match (low >> 1) & 0b11 {
        0b00 => u64::from(low & !0xf),
        0b10 if index + 1 < slots => u64::from(low & !0xf) | u64::from(self.read_u32(f, BAR0 + 4 * (index + 1))) << 32,
        _ => return None,
    };
    (address != 0).then_some(address)
}
```]

== Capabilities

If bit 4 of the status register is set, the byte at `0x34` points at a linked list of capabilities in configuration space: each has an id at its first byte and the next offset at its second. The low two bits of a pointer are reserved and masked off. `for_each_capability` follows the list until a pointer of 0, and treats a malformed list as ended: a capability can't point into the 64-byte header, and a table of 64 flags, one per 4-byte slot of the first 256 bytes, stops the walk at the first offset it has seen before.


This list is how `green-tea-virtio` finds a device's registers. `enable` then sets `COMMAND_MEMORY` and `COMMAND_BUS_MASTER`, without which the function ignores accesses to its BARs and can't do DMA.

== Exercised by

The `q35` board's `for_each_virtio` scans the bus with `for_each_function` and hands each function to `virtio::pci::probe`, which uses every other method here. The `pc` board scans the buses MCFG lists with `for_each_function_on`, looking for an xHCI controller (@crate-xhci). The host tests put a function's configuration space in ordinary memory, and check the walk on a cycle and on a pointer into the header, and `bar` at both ends of each header type:

#output(title: "cargo test -p green-tea-pci")[
```
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

The recording shows the device found and working over PCI:

#output(title: "cargo xtask kernel --kernel disk --target x86_64 --headless")[
```
virtio: block device (id 2)
virtio-blk over PCI: 64 sectors (32 KiB), id "green-tea-disk", write cache
PASS: read 64 sectors over virtio-PCI, and wrote one back
```
]

== Limits

- Read-mostly: it assigns no BARs, programs no bridges and sets up no interrupts (no MSI or MSI-X). On `riscv64` and `aarch64` `virt` nothing would have done that work, which is one reason those use virtio over MMIO.
- One ECAM window at a time: the caller says which, and the `pc` board reads only PCI segment 0.
- `for_each_capability` walks only the standard list in the first 256 bytes, not PCI Express's extended capabilities.
- The offset arguments are unchecked, and an unaligned one breaks the contract in the comment on line 50.

A flat scan of a window the firmware set up. @crate-virtio is what the scan is looking for.
// chapter-status:begin
// stage: early-draft
// revision: 2
// body: ee755159408d
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// chapter-status:end
