#import "../lib.typ": *

= green-tea-clevo-lights <crate-clevo-lights>

`green-tea-clevo-lights` sets the color and brightness of a Clevo laptop's keyboard, by writing to a mailbox in the laptop's embedded controller (EC). @app-wmbb explains where that mailbox comes from. The driver has run once on the laptop, and that run showed the keyboard has one zone of color, not the three the firmware's flags describe. In QEMU it can only refuse.

#spec(
  ("Source", [#repo("drivers/clevo-lights/src/lib.rs"), 287 lines]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-bare` (the `pc` board), for the shell's `lights` command]),
  ("Tests", [host tests against a fake EC; a refusal in QEMU; one run on the laptop]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`EcPorts`], [the EC's two I/O ports: `status`, `command` and `data`],
  [`Firmware`], [what identifies a firmware: its DSDT's `table_id`, `length` and `checksum`],
  [`KNOWN_FIRMWARE`], [the firmwares whose AML we have read: one, a Clevo NP50PN at BIOS 1.07.12],
  [`Color`, `Color::parse`], [red, green and blue; from a name, or six hexadecimal digits],
  [`Lights::new(ports, delay_ms, firmware)`], [the lights, if the firmware is a known one],
  [`set_color`], [the color of the whole keyboard],
  [`set_brightness`], [the whole keyboard, where 0 is off],
  [`Error`], [`UnknownFirmware` or `Timeout`],
)

== The firmware guard

The mailbox's meaning is private to each firmware, and the same mailbox serves other EC functions. The same bytes on another machine could do something else. So no `Lights` exists unless the firmware is one we have read:

#srcfile("drivers/clevo-lights/src/lib.rs", lines: ((109, 114),), notes: ((110, [The table id, the length and the checksum must all match. The `pc` board reads them from the DSDT's header.]),))[```rust
pub fn new(ports: P, delay_ms: fn(u64), firmware: Firmware) -> Result<Lights<P>, Error> {
    if !KNOWN_FIRMWARE.contains(&firmware) {
        return Err(Error::UnknownFirmware);
    }
    Ok(Lights { ports, delay_ms })
}
```]

The known firmware's DSDT has the table id `ADL-P-M`, a length of 480,126 bytes and the checksum `0x97`. If a firmware update changes any of them, the driver refuses until someone has read the new AML.

== The mailbox

A color is five writes to the EC's address space. The value in `FDAT` says what is being set, three bytes carry the values, and `FCMD` goes last, because that write is what makes the EC act:

#srcfile("drivers/clevo-lights/src/lib.rs", lines: ((128, 136),), notes: ((129, [`3` for the color, `6` for the brightness.]), (133, [`0xca`, the only command this driver ever sends.]), (134, [The firmware doesn't wait for the EC to act. We leave it 10 ms between commands.]),))[```rust
fn mailbox(&mut self, what: u8, first: u8, second: u8, third: u8) -> Result<(), Error> {
    self.write(FDAT, what)?;
    self.write(FBUF, first)?;
    self.write(FBF1, second)?;
    self.write(FBF2, third)?;
    self.write(FCMD, SET_LIGHTS)?;
    (self.delay_ms)(SETTLE_MS);
    Ok(())
}
```]

`set_color` passes the color bytes in the firmware's order: blue, red, green. Each write is the ACPI specification's protocol for the EC, three bytes over two ports, waiting up to 100 ms for the EC to take each one:

#srcfile("drivers/clevo-lights/src/lib.rs", lines: ((139, 150),), notes: ((142, [`0x81`, write, to the command port `0x66`.]), (145, [The EC address, to the data port `0x62`.]), (148, [The value, to the same port.]),))[```rust
fn write(&mut self, address: u8, value: u8) -> Result<(), Error> {
    self.wait()?;
    self.ports
        .command(EC_WRITE);
    self.wait()?;
    self.ports
        .data(address);
    self.wait()?;
    self.ports
        .data(value);
    Ok(())
}
```]

If the EC stops taking bytes, `wait` returns `Timeout` and `?` ends the sequence. The command byte is written last, so a mailbox left half full is never acted on.

== One zone, not three

The firmware's `WMBB` accepts three colors, for the left, center and right of the keyboard, and writes them with `FDAT` `3`, `4` and `5`. Its flags word says the keyboard has three zones (@app-wmbb). The first version of this driver believed it, and its shell command took a zone.

On the laptop, booted from a USB drive, `lights left red` changed the color of the whole keyboard, and `lights center` and `lights right` changed nothing. Each of those commands also set the brightness to full first, so the last two sent a brightness the keyboard already had, and a color that nothing showed. The keyboard has one zone, and `FDAT` `3` is its color.

So the driver now sends only `3` and `6`, and the shell's `lights <color>` sets the whole keyboard. Leaving out `4` and `5` also keeps to the rule of sending the fewest mailbox values that work.

== Exercised by

The fake EC decodes the port protocol back into the writes it received, and fails a test if a byte arrives while it is busy. The tests check the bytes of a color and of a brightness, the refusal of an unknown firmware, and a stuck EC.

#output(title: "cargo test -p green-tea-clevo-lights")[
```
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

QEMU's firmware isn't a known one, so the shell's `lights` command is refused there, and nothing is written to the EC's ports:

#output(title: "docs/record.d/clevo-lights.sh: the pc board's shell in QEMU")[
```
> lights
lights: this firmware isn't one whose lights we know, so nothing was sent
> lights red
lights: this firmware isn't one whose lights we know, so nothing was sent
```
]

== Limits

- *One run on the laptop*, with the earlier, three-zone version of the driver: the mailbox worked, and the keyboard has one zone. Brightness, `off` and the colors' byte order weren't checked on the keyboard.
- One firmware only. Any other Clevo machine, or this one after a BIOS update, is refused.
- It doesn't ask the EC for the keyboard's type (`KBTP`). The firmware's `PSF1` flags say three zones, which the keyboard contradicts, so the one zone is known only from the run on the laptop.
- It doesn't take the lock the firmware's AML takes on the EC. Nothing else runs on a green-tea machine to contend for it, but the EC's own firmware is not synchronized with us beyond the status bit.
- Steady colors and brightness only: no animated modes, and nothing is read back.

Five bytes, in the right order, to the one zone the keyboard has. @crate-bare is what puts these drivers on a machine.
// chapter-status:begin
// stage: research-done
// revision: 2
// body: 5f5be4a84b7d
// activity: 2026-11-22 | r1 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r1 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
