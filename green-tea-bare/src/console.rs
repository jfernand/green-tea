//! `println!` over the platform's serial port.

use crate::platform::{Board, Platform};
use core::fmt;

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
macro_rules! println {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::console::Console, $($arg)*);
    }};
}
