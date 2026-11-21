//! `trace dump` and `trace check`: the recorded trace as a file.
//!
//! The kernel has no disk to write a file to, so `trace dump` writes the trace to the
//! serial console, in green-tea-fit's text form, for whoever is at the other end to keep:
//! `cargo xtask trace` is. `trace check` writes the trace in both forms into memory, reads
//! both back, and compares what it read with what was recorded.

use crate::Shell;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use green_tea_bare::console::Console;
use green_tea_bare::fit::trace::Event;
use green_tea_bare::fit::tracefile::{self, Record, TextWriter};
use green_tea_bare::{Board, Platform, println};

/// How many events `trace check` writes and reads back at a time: a whole trace in both
/// forms, and read back from both, would need more of the heap than the trace's buffer leaves.
const CHUNK: usize = 2048;

impl Shell {
    /// `trace dump`: write the recorded trace to the console.
    pub fn dump(&self) {
        let Some(events) = self.recorded() else { return };
        // Writing to the console can't fail.
        let Ok(mut writer) = TextWriter::new(Console) else { return };
        let _ = writer.comment(&format!("recorded by green-tea-shell on {}", Board::NAME));
        for event in events {
            let _ = writer.record(&Record::from(*event));
        }
        let _ = writer.end();
    }

    /// `trace check`: write the trace as text and as binary, read both back, and judge.
    pub fn check(&self) {
        let Some(events) = self.recorded() else { return };
        match round_trip(events) {
            Ok((text, binary)) => println!(
                "PASS: {} events written as {text} bytes of text and as {binary} bytes of binary, {CHUNK} at a time, \
                 and read back the same",
                events.len()
            ),
            Err(e) => println!("FAIL: {e}"),
        }
    }

    /// The events of a complete recording, or nothing, with the reason printed.
    fn recorded(&self) -> Option<&[Event]> {
        match &self.trace {
            None => println!("nothing is recorded: `trace on`, some commands, `trace off`"),
            Some(recording) if recording.dropped != 0 => {
                println!("FAIL: the trace is incomplete: {} events were dropped", recording.dropped)
            }
            Some(recording) => return Some(&recording.events[..recording.len]),
        }
        None
    }
}

/// Write `events` in both forms, a chunk at a time, and read each chunk back. Returns the
/// bytes of text and the bytes of binary that the chunks came to.
fn round_trip(events: &[Event]) -> Result<(usize, usize), &'static str> {
    let (mut text_bytes, mut binary_bytes) = (0, 0);
    let mut text = String::new();
    for chunk in events.chunks(CHUNK) {
        let records: Vec<Record> = chunk.iter().map(|&event| event.into()).collect();
        text.clear();
        tracefile::write_text(&records, &mut text).map_err(|_| "the text form couldn't be written")?;
        if tracefile::read(text.as_bytes()).map_err(|_| "the text form was refused when read back")? != records {
            return Err("the text form read back differently");
        }
        let binary = tracefile::write_binary(&records);
        if tracefile::read(&binary).map_err(|_| "the binary form was refused when read back")? != records {
            return Err("the binary form read back differently");
        }
        if tracefile::events(&records).events != chunk {
            return Err("the records of a trace didn't turn back into its events");
        }
        text_bytes += text.len();
        binary_bytes += binary.len();
    }
    Ok((text_bytes, binary_bytes))
}
