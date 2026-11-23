#import "lib.typ": *

= The keyboard lights and `WMBB` <app-wmbb>

The keyboard of our test laptop, a Clevo NP50PN, is lit by colored LEDs. @crate-clevo-lights is the driver for them. This appendix explains how they are controlled, starting from nothing, because the path runs through four pieces of PC firmware that the rest of this report barely mentions. `WMBB` is the name of one of them: the function in the laptop's firmware that the operating system calls to change the lights.

Everything here comes from reading the firmware's tables on that laptop, and the open-source `tuxedo-drivers` for Clevo machines. The driver follows the procedure below, and has been tested against a fake EC. It has not yet sent a command to the real one, so the procedure is still a reading of the firmware.

== The pieces

#dtable(columns: (auto, 1fr), ("Name", "What it is"),
  [Embedded controller (EC)], [A small separate computer on the motherboard. It runs its own firmware, and looks after the keyboard, fans, battery and lights, even while the main CPU is off.],
  [ACPI], [The standard way firmware describes a PC to an operating system. green-tea already reads its tables to find the CPUs and timers (@crate-acpi).],
  [AML], [ACPI Machine Language, a bytecode. Besides data, the ACPI tables contain small programs in AML, which the operating system runs to operate the hardware without knowing its details.],
  [WMI], [Windows Management Instrumentation. Vendors use its ACPI form to publish private functions, each identified by a long number called a GUID.],
  [`WMBB`], [The AML function behind this laptop's WMI GUID `ABBC0F6D-8EA1-11D1-00A0-C90629100000`. The letters `BB` are the id WMI gives the function, and `WM` is the prefix WMI requires. It takes a command number and an argument.],
)

== The path of one command

An operating system that wants to change a color goes through these steps:

```
 operating system     "run command 0x67 with this 32-bit argument"
        |
        v
 WMBB  (AML)          picks the handler for the command number
        |
        v
 SCMD  (AML)          unpacks the argument into EC registers
        |
        v
 EC registers         0xF8 to 0xFC, a small mailbox
        |
        v
 EC firmware          reads the mailbox, and sets the LEDs
```

The first part of `WMBB` shows its shape. It takes a lock on the EC, and dispatches on the command number. Commands such as `0x67` go to `SCMD`, the firmware's handler for commands that set something:

```
Method (WMBB, 3, Serialized)
{
    P8XH (Zero, Arg1)
    Local1 = Acquire (^^PC00.LPCB.EC.PATM, 0x0064)
    If ((Local1 == Zero))
    {
        Switch (Arg1)
        ...
```

The firmware has a second door to the same room. A device named `CLV0001` has a standard ACPI method, `_DSM`, that sends command `0x67` to the same `SCMD`. That is the door `tuxedo-drivers` prefers.

== What command `0x67` does

`SCMD` looks at the top four bits of the argument to decide what is being set. The ones that matter for steady colors are these:

#dtable(columns: (auto, 1fr), ("Argument", "Meaning"),
  [`0xF0BBRRGG`], [The color of zone 0, the left of the keyboard],
  [`0xF1BBRRGG`], [The color of zone 1, the center],
  [`0xF2BBRRGG`], [The color of zone 2, the right],
  [`0xF40000nn`], [The brightness of the whole keyboard, from `00` to `FF`],
)

The color bytes are in the order blue, red, green. The firmware then writes them to the EC's mailbox, and writes the mailbox's command register last, because that write is what tells the EC to act:

#dtable(columns: (auto, auto, 1fr), ("EC address", "Name", "Holds"),
  [`0xF9`], [`FDAT`], [What to set: `03`, `04` or `05` for zones 0, 1 or 2, and `06` for brightness],
  [`0xFA`], [`FBUF`], [Blue, or the brightness],
  [`0xFB`], [`FBF1`], [Red (0 for brightness)],
  [`0xFC`], [`FBF2`], [Green (0 for brightness)],
  [`0xF8`], [`FCMD`], [`0xCA`, written last],
)

As an example, take the left zone in orange, with red `FF`, green `80` and blue `00`. The argument to `WMBB` is `0xF000FF80`, and the firmware turns it into five writes:

```
EC[0xF9] = 0x03    left zone
EC[0xFA] = 0x00    blue
EC[0xFB] = 0xFF    red
EC[0xFC] = 0x80    green
EC[0xF8] = 0xCA    go
```

== What this means for green-tea

We expected to need an AML interpreter, a large piece of software, to run `WMBB`. For the lights we don't. The AML does no more than compute those five values, so the green-tea driver writes them to the EC itself.

Writing one byte to the EC is a standard ACPI protocol over two I/O ports, `0x66` for commands and status and `0x62` for data. The firmware's tables confirm these ports on this laptop. The driver waits until bit 1 of the status port is clear, meaning the EC has taken the last byte, before each of three writes:

+ the command `0x81` (write) to port `0x66`
+ the EC address to port `0x62`
+ the value to port `0x62`

The firmware also says what kind of keyboard this is. Its flags word, `PSF1`, reads `0x04690025` on this laptop. Bit `0x00400000`, which `tuxedo-drivers` reads as "three-zone RGB", is set, and bit `0x40000000`, "white only", is clear.

#callout(kind: "note", [What we don't know yet])[
  - *Whether it works.* The driver hasn't been run on the laptop, so the zones, the colors and the byte order are unconfirmed on the keyboard itself.
  - *The keyboard type, from the EC.* `tuxedo-drivers` first asks the EC (its `KBTP` register), and uses `PSF1` as a fallback. We read only `PSF1`.
  - *Timing.* The firmware writes the mailbox without waiting for the EC to finish, while holding a lock. Our driver leaves 10 ms between commands, and doesn't check that `FCMD` has cleared.
  - *Safety of other values.* The same mailbox serves other EC functions, so the driver sends only the combinations in the tables above, and refuses any firmware but this one.
]

Running the driver on the laptop, one zone first, settles the first and the third.
// chapter-status:begin
// stage: early-draft
// revision: 2
// body: b45649b7eeaa
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// chapter-status:end
