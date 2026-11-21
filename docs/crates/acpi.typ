#import "../lib.typ": *

= green-tea-acpi <crate-acpi>

`green-tea-acpi` reads enough of the firmware's ACPI tables to learn a machine's CPUs and clocks: the local APICs from the MADT, the PCI window from the MCFG, the HPET's address and the power management timer's port. It verifies every table's checksum before using it, and it interprets no AML.

#spec(
  ("Source", [#repo("drivers/acpi/src/lib.rs"), 405 lines]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-bare` (the `pc` board)]),
  ("Tests", [17 host tests, on tables in a fake memory; every `pc` boot]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`Memory`], [a trait with one method, `read(addr, out)`, so that the tests can use a fake memory],
  [`Identity4G`], [the real one: physical memory through an identity map of its first 4 GiB],
  [`Acpi::new(memory, rsdp)`], [checks the root pointer's bytes, and picks the root table],
  [`apic_ids(boot, ids)`], [the enabled local APICs, with the boot CPU's first; returns how many],
  [`lapic_address()`], [where the local APICs are],
  [`ecam()`], [PCI segment 0's ECAM window: its base, and the buses it covers],
  [`hpet()`], [the HPET's address],
  [`pm_timer_port()`], [the I/O port of the power management timer],
)

== From the root pointer to a table

The firmware publishes a *root pointer*, the RSDP, and the bootloader passes a copy (@crate-multiboot2). It names a root table, which is a list of pointers to all the others. There are two forms: the 32-bit RSDT, and from ACPI 2.0 the 64-bit XSDT, which we prefer when we can read it.

#srcfile("drivers/acpi/src/lib.rs", lines: ((52, 66),), notes: ((53, [The checksum of the first 20 bytes, which every revision has: they have to sum to 0 modulo 256.]), (56, [The RSDT, unless something better turns up. `entry` is the width of a pointer in the root table.]), (57, [The XSDT needs a revision of 2 or more and the extended 36-byte checksum.]), (61, [And it has to be readable, with a good checksum. An XSDT we can't reach leaves the RSDT in place.]),))[```rust
pub fn new(memory: M, rsdp: &[u8]) -> Option<Acpi<M>> {
    if rsdp.get(..8)? != b"RSD PTR " || sum(rsdp.get(..20)?) != 0 {
        return None;
    }
    let mut acpi = Acpi { memory, root: u64::from(u32_at(rsdp, 16)?), entry: 4 }; // the RSDT
    if rsdp[15] >= 2 && rsdp.len() >= 36 && sum(&rsdp[..36]) == 0 {
        // The XSDT, as long as we can read it: it may be above the memory we map.
        let xsdt = u64_at(rsdp, 24)?;
        let mut buf = [0u8; MAX_TABLE];
        if acpi.table(xsdt, &mut buf).is_some() {
            (acpi.root, acpi.entry) = (xsdt, 8);
        }
    }
    Some(acpi)
}
```]

Every table starts with the same 36-byte header. Its signature is at offset 0 and its length at offset 4, and all its bytes sum to 0.

#srcfile("drivers/acpi/src/lib.rs", lines: ((69, 79),), notes: ((71, [`Memory::read` says `false` for what it can't reach.]), (75, [The length comes from the firmware, so it is bounded before it is used. `MAX_TABLE` is 8 KiB.]), (78, [A table with a bad checksum reads as absent.]),))[```rust
fn table<'b>(&self, addr: u64, buf: &'b mut [u8; MAX_TABLE]) -> Option<&'b [u8]> {
    let mut header = [0u8; HEADER];
    if !self.memory.read(addr, &mut header) {
        return None;
    }
    let len = u32_at(&header, 4)? as usize;
    if !(HEADER..=MAX_TABLE).contains(&len) || !self.memory.read(addr, &mut buf[..len]) {
        return None;
    }
    (sum(&buf[..len]) == 0).then_some(&buf[..len])
}
```]

`find` walks the root table's pointers, reads four bytes at each, and returns the first address with the wanted signature. Each query does the whole walk again, and the two buffers it takes, one for the root table and one for the table it wants, are 8 KiB each, on the stack.

== The MADT: the CPUs

The MADT (signature `APIC`) has the local APICs' address at offset 36, and from offset 44 a run of entries, each starting with its type and length. `apic_ids` reads two types:

#dtable(columns: (auto, auto, auto, 1fr), ("Type", "Entry", "APIC id", "Flags"),
  [0], [Local APIC], [byte 3], [`u32` at byte 4],
  [9], [x2APIC], [`u32` at byte 4], [`u32` at byte 8],
)

#srcfile("drivers/acpi/src/lib.rs", lines: ((99, 121),), notes: ((99, [The boot CPU goes first, whatever its place in the table.]), (103, [A length under 2 would never advance.]), (113, [Bit 0, Enabled. Bit 1, Online Capable, on its own describes a CPU that could be added later, so it doesn't count.]), (114, [Bounded by the caller's slice, and an id seen twice is kept once.]),))[```rust
*first = boot;
let mut count = 1;
let mut at = HEADER + 8;
while let (Some(&kind), Some(&len)) = (madt.get(at), madt.get(at + 1)) {
    if len < 2 {
        break;
    }
    let found = match kind {
        0 => madt.get(at + 3).map(|&id| u32::from(id)).zip(u32_at(madt, at + 4)),
        9 => u32_at(madt, at + 4).zip(u32_at(madt, at + 8)),
        _ => None,
    };
    if let Some((id, flags)) = found {
        // "Online capable" without "enabled" is a CPU that could be added later.
        let usable = flags & ENABLED != 0;
        if usable && id != boot && count < ids.len() && !ids[..count].contains(&id) {
            ids[count] = id;
            count += 1;
        }
    }
    at += len as usize;
}
count
```]

The ids can have gaps, so the `pc` board numbers its CPUs by their position in this list rather than by APIC id. `lapic_address` reads the 32-bit address at offset 36, and lets an entry of type 5, the 64-bit override, replace it.

== MCFG, HPET and FADT

`ecam` reads the MCFG, whose 16-byte entries from offset 44 each hold a base address, a segment, and a first and last bus. It returns segment 0's. The address is where bus 0 would be, even if the first bus is higher, which is how the ECAM formula in @crate-pci expects it.

`hpet` reads a generic address structure at offset 40. Its first byte is the address space, 0 for memory, and the address itself is at offset 44. `pm_timer_port` reads the FADT (signature `FACP`). Its 64-bit `X_PM_TMR_BLK`, a generic address at offset 208, wins if it names a nonzero I/O port. Otherwise the 32-bit `PM_TMR_BLK` at offset 76 is used. Either way, 0 or anything that doesn't fit a port number is refused.

The `pc` board uses the two clocks to measure its TSC over 10 ms: the HPET if there is one, and otherwise the power management timer, which counts at 3.579545 MHz.

== Exercised by

The tests build tables in a fake memory, with real headers and checksums. They cover sparse APIC ids, a disabled CPU, an online-capable CPU that isn't enabled, an x2APIC entry, a caller's slice too small, both clocks, the three cases of the 64-bit timer field, the fallback to the RSDT, the address override, an MCFG with two segments, a machine without an MCFG or an HPET, and a bad checksum on a table and on the root pointer.

#output(title: "cargo test -p green-tea-acpi")[
```
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

On the `pc` board in QEMU, `-smp 4` gives a MADT with four CPUs, and the kernel started the other three from it:

#output(title: "cargo xtask kernel --board pc --kernel smp --target x86_64 --headless")[
```
green-tea-coop on x86_64 (pc)
framebuffer: 1280x800 XRGB8888
4 CPUs are running: the one that booted, and 3 it started
```
]

We also booted the same image on a `q35` with `hpet=off`, which leaves ACPI with no HPET table. The board then has only the power management timer to calibrate against, read through `inl`, and it reaches the prompt:

#output(title: "qemu-system-x86_64 -machine q35,hpet=off ... (the pc board's shell kernel)")[
```
green-tea-shell on x86_64 (pc)
framebuffer: 1280x800 XRGB8888
type `help` for the commands
```
]

== Limits

- Exercised only on QEMU with OVMF firmware and on fake tables in the host tests. It has not read a physical machine's tables.
- `Identity4G` reads only below 4 GiB. An XSDT above that falls back to the RSDT, but the choice is made once, for the root table: an XSDT we can read whose entries point above 4 GiB leaves those tables absent, and the RSDT isn't tried for them.
- A table over 8 KiB reads as absent, so a very large MADT means no CPUs beyond the boot one.
- `find` returns the first table with a signature. If that one has a bad checksum, a second copy isn't tried.
- A power management timer in memory space, which hardware-reduced ACPI allows, isn't supported: `X_PM_TMR_BLK` counts only as an I/O port.
- No AML, so nothing that needs the DSDT: no power-off, no device discovery beyond these tables.

A checksum, a walk and four questions. @crate-i8042 is what the machine's keyboard looks like without USB.
