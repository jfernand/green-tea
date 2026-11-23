#import "../lib.typ": *

= green-tea-i8042 <crate-i8042>

`green-tea-i8042` reads a PS/2 keyboard through the i8042 controller, which is how most laptops' built-in keyboards still appear to software. The crate has two halves: `Decoder`, plain logic that turns scan codes into ASCII on a US layout, and `Keyboard`, which reads the controller's ports on `x86_64`.

#spec(
  ("Source", [#repo("drivers/i8042/src/lib.rs"), 224 lines]),
  ("Depends on", [nothing; `no_std`. It has its own `in` instruction rather than `green-tea-io`'s]),
  ("Used by", [`green-tea-bare` (the `pc` board)]),
  ("Tests", [8 host tests of the decoder; a typed line in QEMU]),
)

#dtable(columns: (auto, auto, 1fr), ("Item", "Where", "What it does"),
  [`Decoder::new()`], [all targets], [a decoder with no key held],
  [`Decoder::feed(code)`], [all targets], [takes one byte, and returns the character it completes, if any],
  [`Keyboard::new()`], [`x86_64`], [the keyboard on the controller],
  [`Keyboard::getc()`], [`x86_64`], [the next character typed, if one has arrived],
  [`Keyboard::enable_interrupts()`], [`x86_64`], [has the controller raise IRQ 1 and IRQ 12],
)

== Scan codes

A keyboard sends a *make* code when a key goes down and a *break* code when it comes up. The controller translates the keyboard's codes to scan code set 1, in which a break code is the make code with bit 7 set. Some keys, the right Ctrl and the arrows among them, send the prefix `0xe0` first.

#srcfile("drivers/i8042/src/lib.rs", lines: ((32, 47),), notes: ((33, [The prefix is only remembered, for the next byte.]), (37, [`take` clears it, so it applies to one byte.]), (40, [Left Ctrl is `1d` and right Ctrl is `e0 1d`, so the arm ignores the prefix.]), (42, [Some keys send a fake shift, `e0 2a`, around themselves. The guard keeps it out of the shift state.]), (43, [Only a plain key press types. Arrows and other extended keys type nothing.]),))[```rust
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

#srcfile("drivers/i8042/src/lib.rs", lines: ((144, 158),), notes: ((147, [On a machine with no controller the port reads as all ones, which would otherwise look like a byte waiting.]), (151, [Bit 5 marks a byte from the mouse or touchpad. It has to be read, to clear the way, and is then discarded.]), (154, [The loop drains what's waiting until a byte completes a character.]),))[```rust
pub fn getc(&mut self) -> Option<u8> {
    loop {
        let status = inb(STATUS);
        if status == 0xff || status & OUTPUT_FULL == 0 {
            return None; // 0xff: no controller on this machine
        }
        let code = inb(DATA);
        if status & FROM_AUX != 0 {
            continue;
        }
        if let Some(c) = self.0.feed(code) {
            return Some(c);
        }
    }
}
```]

== Interrupts

The controller raises IRQ 1 for a keyboard byte and IRQ 12 for a mouse byte, if two bits of its configuration byte are set. `enable_interrupts` empties the output buffer, so that an old byte isn't taken for the configuration, reads the configuration, and writes it back with both bits set.

The mouse's interrupt is on even though nothing uses a mouse. Its bytes share the controller's one output buffer, so a mouse byte left unread would block the keyboard, and no keyboard interrupt would say so.

#srcfile("drivers/i8042/src/lib.rs", lines: ((127, 140),), notes: ((127, [Ask for the configuration byte.]), (131, [The value the firmware left.]), (139, [The same byte, with the two interrupt enables set.]),))[```rust
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
keyboard 27, mouse 0, serial 0, backup timer 84, halts 111
```
]

== Limits

- Tested only against QEMU's emulated i8042, never on physical hardware.
- No initialization: it sends the controller no commands, and relies on the firmware having left it enabled with translation to set 1 on. With translation off, the codes would decode as the wrong keys.
- The interrupt only says that a byte is there. The byte waits in the controller until the woken task calls `getc`.
- Mouse and touchpad bytes are read and thrown away.
- US layout, with no Caps Lock, Num Lock or Alt, and no function keys or arrows.
- The Pause key's `e1` prefix isn't recognized. Its sequence, `e1 1d 45 e1 9d c5`, contains a Ctrl press and release, so a Ctrl the user is holding reads as released afterwards.
- `Keyboard` takes no lock. The `pc` board guards it with a flag.

Two ports, two tables and an interrupt. @crate-xhci is the same job on a modern machine, and takes rather more.
// chapter-status:begin
// stage: early-draft
// revision: 2
// body: 61f22577cb6f
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// chapter-status:end
