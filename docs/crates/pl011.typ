#import "../lib.typ": *

= green-tea-pl011 <crate-pl011>

`green-tea-pl011` drives Arm's PL011 UART by polling, as the console on `aarch64` `virt`. One type, `Pl011(pub usize)`, holds the base address (`0x0900_0000` on that machine), and its registers are 32 bits wide.

#spec(
  ("Source", [#repo("drivers/pl011/src/lib.rs"), 42 lines]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-bare` (the `aarch64` board)]),
  ("Tests", [no host tests; every kernel on `aarch64` prints through it, and the `shell` kernel reads through it]),
)

#dtable(columns: (auto, 1fr), ("Method", "What it does"),
  [`enable()`], [sets UARTEN, TXE and RXE in the control register, keeping its other bits],
  [`putc(byte)`], [waits while the transmit FIFO is full, then writes the byte],
  [`getc()`], [`Some(byte)` if the receive FIFO has one, `None` if it's empty],
)

== Registers

The driver uses the data register at offset `0x00`, the flag register `FR` at `0x18`, and the control register `CR` at `0x30`.

#bytefield(caption: "The control register bits enable sets",
  ("other", "bits 31–10", 5), ("RXE", "bit 9", 2), ("TXE", "bit 8", 2), ("other", "bits 7–1", 3), ("UARTEN", "bit 0", 2),
)

#srcfile("drivers/pl011/src/lib.rs", lines: ((23, 32),), notes: ((30, [A read-modify-write, so `enable` leaves the rest of the register alone.]),))[```rust
pub fn enable(&self) {
    const CR: usize = 0x30; // control register
    const CR_UARTEN: u32 = 1 << 0;
    const CR_TXE: u32 = 1 << 8;
    const CR_RXE: u32 = 1 << 9;
    unsafe {
        let cr = (self.0 + CR) as *mut u32;
        write_volatile(cr, read_volatile(cr) | CR_UARTEN | CR_TXE | CR_RXE);
    }
}
```]

QEMU resets `CR` to `0x300`, with TXE and RXE set and UARTEN clear, and sends bytes regardless. The hardware needs all three, so `enable` sets them. `putc` spins on `FR_TXFF`, bit 5 of `FR`, and `getc` tests `FR_RXFE`, bit 4. The `aarch64` board calls `enable` in its `init`, before the first `println!`. `getc` reads the data register as 32 bits and casts it to `u8`.

== Exercised by

Every `aarch64` kernel prints through `putc`. The `shell` kernel also reads the script `cargo xtask kernel` types at it:

#output(title: "cargo xtask kernel --kernel shell --target aarch64 --headless")[
```
green-tea-shell on aarch64
ramfb: 640x480 XRGB8888
type `help` for the commands
> echo hello from the serial port
hello from the serial port
```
]

== Limits

- `enable` sets no baud rate or line format, and changes `CR` with the UART running.
- `getc`'s cast discards the data register's error flags, in bits 8 to 11.
- No interrupts, and `putc` can spin forever.

Three registers and a console. @crate-fw-cfg is the first device here that does DMA.
// chapter-status:begin
// stage: research-done
// revision: 2
// body: 9c29d50606dd
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r2 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
