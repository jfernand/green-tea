//! `println!` over the platform's serial port.

use core::fmt;

pub struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                crate::platform::putc(b'\r');
            }
            crate::platform::putc(byte);
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
