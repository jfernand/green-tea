//! Reading from the platform's serial console from a cooperative task.

use crate::task;
use green_tea_bare::{Board, Platform};

/// How long a task waiting for input sleeps between looks at the serial port.
const POLL_MS: u64 = 1;

/// The next byte typed at the console. On a board with input interrupts, the calling task
/// waits for one, and the CPU can halt. On the others the port is polled: while nothing
/// has arrived the task sleeps a millisecond at a time, so other tasks run.
///
/// # Panics
/// If called outside a task.
pub fn read_byte() -> u8 {
    if let Some(input) = Board::input_event() {
        loop {
            input.clear(); // before looking, so that a byte arriving after the look still wakes us
            if let Some(byte) = Board::getc() {
                return byte;
            }
            task::wait_event(input);
        }
    }
    loop {
        if let Some(byte) = Board::getc() {
            return byte;
        }
        task::sleep_ms(POLL_MS);
    }
}
