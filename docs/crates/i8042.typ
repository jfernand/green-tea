#import "../lib.typ": *

= green-tea-i8042 <crate-i8042>

`green-tea-i8042` reads a PS/2 keyboard through the i8042 controller, which is how most laptops' built-in keyboards still appear to software. The crate has two halves: `Decoder`, plain logic that turns scan codes into ASCII on a US layout, and `Keyboard`, which reads the controller's ports on `x86_64`. `Keyboard` can also hand over a byte undecoded, as a `RawInput`, for the `pc` board's user-process broker (@crate-black).

#spec(
  ("Source", [#repo("drivers/i8042/src/lib.rs"), 244 lines]),
  ("Depends on", [nothing; `no_std`. It has its own `in` instruction rather than `green-tea-io`'s]),
  ("Used by", [`green-tea-bare` (the `pc` board's console input, and its keyboard broker for user processes)]),
  ("Tests", [8 host tests of the decoder; a typed line in QEMU]),
)

#dtable(columns: (auto, auto, 1fr), ("Item", "Where", "What it does"),
  [`Decoder::new()`], [all targets], [a decoder with no key held],
  [`Decoder::feed(code)`], [all targets], [takes one byte, and returns the character it completes, if any],
  [`Keyboard::new()`], [`x86_64`], [the keyboard on the controller],
  [`RawInput`], [all targets], [one byte from the controller: `code`, and `auxiliary`, true for a mouse or touchpad byte],
  [`Keyboard::getc()`], [`x86_64`], [the next character typed, if one has arrived],
  [`Keyboard::read_raw()`], [`x86_64`], [one waiting byte, undecoded, as a `RawInput`],
  [`Keyboard::decode(input)`], [`x86_64`], [feeds a keyboard byte to the decoder; a mouse byte gives `None`],
  [`Keyboard::read_and_decode()`], [`x86_64`], [`read_raw` and then `decode`, for one byte],
  [`Keyboard::enable_interrupts()`], [`x86_64`], [has the controller raise IRQ 1 and IRQ 12],
)

== Scan codes

A keyboard sends a *make* code when a key goes down and a *break* code when it comes up. The controller translates the keyboard's codes to scan code set 1, in which a break code is the make code with bit 7 set. Some keys, the right Ctrl and the arrows among them, send the prefix `0xe0` first.

#srcfile("drivers/i8042/src/lib.rs", lines: ((39, 54),), notes: ((40, [The prefix is only remembered, for the next byte.]), (44, [`take` clears it, so it applies to one byte.]), (47, [Left Ctrl is `1d` and right Ctrl is `e0 1d`, so the arm ignores the prefix.]), (49, [Some keys send a fake shift, `e0 2a`, around themselves. The guard keeps it out of the shift state.]), (50, [Only a plain key press types. Arrows and other extended keys type nothing.]),))[```rust
pub fn feed(&mut self, code: u8) -> Option<u8> {
    if code == EXTENDED {
        self.extended = true;
        return None;
    }
    let extended = core::mem::take(&mut self.extended);
    let (key, pressed) = (code & !RELEASE, code & RELEASE == 0);
    match key {
        CTRL => self.ctrl = pressed, // the right one is the same code, extended
        // An extended 0x2a or 0x36 is a fake shift some keys send around themselves.
        LEFT_SHIFT | RIGHT_SHIFT if !extended => self.shift = pressed,
        _ if pressed && !extended => return self.character(key),
        _ => {}
    }
    None
}
```]

`character` looks the key up in one of two 58-entry tables, `PLAIN` or `SHIFTED`, where 0 means the key types nothing. With Ctrl held, a letter becomes its control code, so Ctrl-C is 3. Enter gives `\r`, and Backspace gives `0x7f`, as a terminal would send them.

== The controller

`Keyboard` wraps a `Decoder` and two ports, data at `0x60` and status at `0x64`.

Reading the controller and decoding what it says are separate steps, so that a caller can take the bytes without the decoder. `read_raw` reads one byte and its source; `getc` loops over it until a byte completes a character:

#srcfile("drivers/i8042/src/lib.rs", lines: ((151, 158), (162, 168), (171, 173),), notes: ((153, [`?`: no byte waiting, no character.]), (164, [On a machine with no controller the port reads as all ones, which would otherwise look like a byte waiting.]), (167, [Bit 5 of the status marks a byte from the mouse or touchpad. It has to be read either way, to clear the controller's one output buffer.]), (172, [`decode` drops the mouse's bytes, so `getc` reads past them.]),))[```rust
pub fn getc(&mut self) -> Option<u8> {
    loop {
        let input = self.read_raw()?;
        if let Some(c) = self.decode(input) {
            return Some(c);
        }
    }
}
// …
pub fn read_raw(&mut self) -> Option<super::RawInput> {
    let status = inb(STATUS);
    if status == 0xff || status & OUTPUT_FULL == 0 {
        return None;
    }
    Some(super::RawInput { code: inb(DATA), auxiliary: status & FROM_AUX != 0 })
}
// …
pub fn decode(&mut self, input: super::RawInput) -> Option<u8> {
    if input.auxiliary { None } else { self.0.feed(input.code) }
}
```]

The `pc` board's interrupt handler calls `read_raw` while a user process holds the keyboard, and queues the keyboard's scan codes for it undecoded (@crate-black). `read_and_decode` is the one-byte form of `getc`, and nothing calls it.

== Interrupts

The controller raises IRQ 1 for a keyboard byte and IRQ 12 for a mouse byte, if two bits of its configuration byte are set. `enable_interrupts` empties the output buffer, so that an old byte isn't taken for the configuration, reads the configuration, and writes it back with both bits set.

The mouse's interrupt is on even though nothing uses a mouse. Its bytes share the controller's one output buffer, so a mouse byte left unread would block the keyboard, and no keyboard interrupt would say so.

#srcfile("drivers/i8042/src/lib.rs", lines: ((134, 147),), notes: ((134, [Ask for the configuration byte.]), (138, [The value the firmware left.]), (146, [The same byte, with the two interrupt enables set.]),))[```rust
outb(STATUS, READ_CONFIG);
if !wait(OUTPUT_FULL, true) {
    return false;
}
let config = inb(DATA);
if !wait(INPUT_FULL, false) {
    return false;
}
outb(STATUS, WRITE_CONFIG);
if !wait(INPUT_FULL, false) {
    return false;
}
outb(DATA, config | IRQ1_AND_IRQ12);
true
```]

== Exercised by

The tests feed the decoder bytes: press and release, shift held and let go, Enter, Backspace and space, Ctrl-C, an extended key, a fake shift, the right Ctrl, and a code past the table.

#output(title: "cargo test -p green-tea-i8042")[
```
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

To exercise the port side we boot the `pc` board's `shell` kernel in QEMU with OVMF, with no USB keyboard attached, and type `echo keys` from QEMU's monitor with `sendkey`, then `irq`. The serial port's input is unused, so the characters reach the shell through this driver, and the count shows that its interrupt woke the shell:

#output(title: "docs/record.d/i8042.sh: QEMU's sendkey, at the pc board's shell")[
```
green-tea-shell on x86_64 (pc)
type `help` for the commands
> echo keys
keys
> irq
keyboard 27, mouse 0, serial 0, backup timer 84, halts 110
```
]

== Limits

- Tested against QEMU's emulated i8042, and once on the laptop's built-in keyboard, where its interrupts arrived.
- No initialization: it sends the controller no commands, and relies on the firmware having left it enabled with translation to set 1 on. With translation off, the codes would decode as the wrong keys.
- The interrupt only says that a byte is there. The byte waits in the controller until the woken task calls `getc`.
- Mouse and touchpad bytes are read and thrown away.
- While a user process holds the keyboard, its bytes bypass the decoder. A Shift or Ctrl that changes state during the lease is seen in its old state when the kernel takes the keyboard back.
- US layout, with no Caps Lock, Num Lock or Alt, and no function keys or arrows.
- The Pause key's `e1` prefix isn't recognized. Its sequence, `e1 1d 45 e1 9d c5`, contains a Ctrl press and release, so a Ctrl the user is holding reads as released afterwards.
- `Keyboard` takes no lock. The `pc` board guards it with a flag.

Two ports, two tables and an interrupt. @crate-xhci is the same job on a modern machine, and takes rather more.
// chapter-status:begin
// stage: research-done
// revision: 4
// body: 3f3ad0aa48ea
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r2 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
