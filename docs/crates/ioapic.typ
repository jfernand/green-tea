#import "../lib.typ": *

= green-tea-ioapic <crate-ioapic>

`green-tea-ioapic` programs the IO APIC, the chip that takes a PC's device interrupt lines and sends each one to a CPU as a numbered vector. The `pc` board uses it to make the keyboard, the mouse and the serial port interrupt, so that a waiting CPU can halt instead of polling.

#spec(
  ("Source", [#repo("drivers/ioapic/src/lib.rs"), 143 lines]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-bare` (the `pc` board)]),
  ("Tests", [host tests against fake registers; the `interrupts` kernel in QEMU]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`Registers`], [a trait with `read` and `write` by register index, so that the tests can use a fake chip],
  [`Mmio(pub usize)`], [the real registers, at a mapped address],
  [`Route`], [where an interrupt goes and how its line signals: `vector`, `destination`, `active_low`, `level`],
  [`IoApic::new(registers, first)`], [the chip whose first input is global system interrupt `first`; reads how many inputs it has],
  [`covers(gsi)`], [whether this chip has that input],
  [`route(gsi, route)`], [sends that input to a CPU, and unmasks it],
)

== Redirection entries

A machine numbers its interrupt inputs across all its IO APICs, as *global system interrupts* (GSIs). Each input has a 64-bit *redirection entry*, stored as two 32-bit registers from index `0x10`. The chip exposes only two words of address space: one selects a register, and the other is that register's value.

#bytefield(caption: "The parts of a redirection entry the driver sets",
  ("destination", "bits 63–56", 3), ("masked", "bit 16", 1), ("level", "bit 15", 1), ("active low", "bit 13", 1), ("vector", "bits 7–0", 2),
)

The two halves can't be written at once, and a half-written entry could deliver the wrong vector to the wrong CPU. So `route` writes the low half masked, then the destination, and clears the mask last:

#srcfile("drivers/ioapic/src/lib.rs", lines: ((75, 87),), notes: ((76, [An input on another chip is refused, so that the caller can try the next one.]), (81, [Masked: nothing is delivered while the entry is incomplete.]), (83, [An IO APIC has 8 bits for the APIC id, in the top byte of the high half.]), (85, [The same low half, without the mask.]),))[```rust
pub fn route(&mut self, gsi: u32, route: Route) -> bool {
    if !self.covers(gsi) {
        return false;
    }
    let index = REDIRECTION + 2 * (gsi - self.first) as u8;
    self.registers
        .write(index, low_half(route) | MASKED);
    self.registers
        .write(index + 1, u32::from(route.destination) << 24);
    self.registers
        .write(index, low_half(route));
    true
}
```]

== Exercised by

The host tests use a fake chip, and check the order of the writes, the bits for each kind of line, and an input outside the chip's range.

#output(title: "cargo test -p green-tea-ioapic")[
```
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

The `interrupts` kernel (@bare-interrupts) boots the `pc` board in QEMU and types two lines at the serial port. The second arrives while the shell is halted, and the shell reports what woke it:

#output(title: "cargo xtask kernel --board pc --kernel interrupts --target x86_64 --headless")[
```
> irq
keyboard 0, mouse 0, serial 0, backup timer 0, halts 0
> irq test
PASS: 2 serial interrupts woke the shell from 98 halts
```
]

== Limits

- Tested against QEMU's IO APIC and a fake one, and once on the laptop, where it delivered the keyboard's interrupt.
- Fixed delivery to one physical APIC id of at most 255. On a CPU with a larger id the `pc` board stays polled.
- An input is never masked again, and nothing reads an entry back.
- Message-signalled interrupts (MSI and MSI-X), which NVMe and xHCI would use, don't pass through this chip and aren't implemented.

Two registers, three writes, and a CPU that can sleep. @crate-i8042 is the first device to use it.
// chapter-status:begin
// stage: research-done
// revision: 2
// body: 0045c208b90f
// activity: 2026-11-22 | r1 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r1 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
