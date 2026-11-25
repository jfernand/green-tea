//! `irq`: what the keyboard and the serial port's interrupts have done, on the `pc` board.

use core::fmt::{self, Write};
use green_tea_bare::{Board, InterruptCounts};

pub const HELP: &str = "irq                how many device interrupts have arrived, and how often we halted\n";

pub fn command(args: &[&str], out: &mut impl Write) -> fmt::Result {
    let InterruptCounts { keyboard, mouse, serial, backup_timer, halts } = Board::interrupt_counts();
    match args {
        [] => writeln!(out, "keyboard {keyboard}, mouse {mouse}, serial {serial}, backup timer {backup_timer}, halts {halts}"),
        ["test"] if serial > 0 && halts > 0 => {
            writeln!(out, "PASS: {serial} serial interrupts woke the shell from {halts} halts")
        }
        ["test"] => writeln!(out, "FAIL: {serial} serial interrupts, {halts} halts"),
        _ => writeln!(out, "usage: irq [test]"),
    }
}
