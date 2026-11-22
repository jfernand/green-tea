#import "../lib.typ": *

= green-tea-ns16550 <crate-ns16550>

`green-tea-ns16550` drives a 16550 UART by polling, as a console: it writes a byte when the transmitter has room, and reads one if one has arrived. The serial console on `riscv64` and `x86_64` is this driver.

#spec(
  ("Source", [#repo("drivers/ns16550/src/lib.rs"), 67 lines]),
  ("Depends on", [`green-tea-io`, on `x86_64` only]),
  ("Used by", [`green-tea-bare` (the `riscv64`, `q35` and `pc` boards)]),
  ("Tests", [no host tests; every kernel on `riscv64` and `x86_64` prints through it, and the `shell` kernel reads through it]),
)

#dtable(columns: (auto, auto, 1fr), ("Type", "Reaches the registers by", "Used at"),
  [`Ns16550Mmio(pub usize)`], [volatile byte accesses], [`0x1000_0000` on `riscv64` `virt`],
  [`Ns16550Port(pub u16)`], [`inb` and `outb`; `x86_64` only], [`0x3f8` (COM1) on `q35` and `pc`],
)

Both types have the same two methods: `putc` waits for room and writes a byte, and `getc` returns `Some(byte)` if one has arrived and `None` if not. `Ns16550Port` also has `enable_receive_interrupt`, which sets the receive bit of the interrupt enable register and the modem control register's `OUT2`, without which a PC's UART line never reaches the interrupt controller.

== Registers

The driver touches two registers. Offset 0 is the data register: a write puts a byte in the transmitter, a read takes one from the receiver. Offset 5 is the line status register, with bit 5 (`LSR_THR_EMPTY`) set when the transmit holding register is empty and bit 0 (`LSR_DATA_READY`) set when a received byte is waiting.

#bytefield(caption: "The line status register as the driver reads it",
  ("other", "bits 7–6", 2), ("THR_EMPTY", "bit 5", 2), ("other", "bits 4–1", 4), ("DATA_READY", "bit 0", 2),
)

#srcfile("drivers/ns16550/src/lib.rs", lines: ((19, 25),), notes: ((22, [Both accesses are volatile: the compiler may neither cache the status nor drop the loop.]),))[```rust
impl Ns16550Mmio {
    pub fn putc(&self, byte: u8) {
        unsafe {
            while read_volatile((self.0 + 5) as *const u8) & LSR_THR_EMPTY == 0 {}
            write_volatile(self.0 as *mut u8, byte);
        }
    }
```]

`Ns16550Port` has the same logic with `inb(self.0 + 5)` and `outb(self.0, byte)`. The driver writes no divisor, line format or FIFO control register, because QEMU's UART works without them. The `pc` board, which can't count on that, programs them itself in `init_serial` (115200 baud, 8N1, FIFOs on) after checking that a UART answers at COM1.

== Exercised by

Every line of every kernel's log has come through `putc`. `getc` runs when a task waits for input, in `green-tea-bare`'s `console::read_byte`, which the shell kernels call. `cargo xtask kernel` types a script into the serial port, and the shell echoes it:

#output(title: "cargo xtask kernel --kernel shell --target riscv64 --headless")[
```
green-tea-shell on riscv64
ramfb: 640x480 XRGB8888
type `help` for the commands
> echo hello from the serial port
hello from the serial port
```
]

The same exchange runs over I/O ports on `x86_64`:

#output(title: "cargo xtask kernel --kernel shell --target x86_64 --headless")[
```
green-tea-shell on x86_64
> echo hello from the serial port
hello from the serial port
```
]

== Limits

- No initialization in the driver: a board on hardware has to set the baud rate and line format, as `pc` does.
- Only the `pc` board uses the receive interrupt. Elsewhere a task waiting for input polls. A byte that overruns the receiver before a task reads it is lost.
- `getc` ignores the line status error bits (overrun, parity, framing).
- `putc` can spin forever if the transmitter never empties.

A console in 67 lines. @crate-pl011 is the same job on Arm's UART.
