//! `println!` and `print!` over the platform's serial port, and reading from it.

use crate::platform::{Board, Platform};
use crate::task;
use core::fmt;

/// How long a task waiting for input sleeps between looks at the serial port.
const POLL_MS: u64 = 1;

/// The next byte typed at the serial console. The port is polled, not interrupt-driven:
/// while nothing has arrived the calling task sleeps a millisecond at a time, so other
/// tasks run.
///
/// # Panics
/// If called outside a task.
pub fn read_byte() -> u8 {
    loop {
        if let Some(byte) = Board::getc() {
            return byte;
        }
        task::sleep_ms(POLL_MS);
    }
}

pub struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                Board::putc(b'\r');
            }
            Board::putc(byte);
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::console::Console, $($arg)*);
    }};
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::console::Console, $($arg)*);
    }};
}
