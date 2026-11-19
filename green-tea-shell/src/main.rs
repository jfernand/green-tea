//! A bare-metal kernel with a shell on the serial console: `cargo xtask shell` to type at
//! it, `cargo xtask kernel --kernel shell` to test it.
//!
//! The shell is one task. It reads a line, runs it as a command, and does that forever,
//! which is the point: unlike the demos, it never stops allocating. Each command
//! allocates its working memory (a copy of the line, the words it splits into, the text
//! it prints) and is done with all of it when it returns.
//!
//! Where that memory comes from is switchable, with `use`:
//! - `use heap`: the global heap, green-tea-bare's bump allocator, which never frees. Every
//!   command leaves its memory behind, and after enough commands the kernel panics.
//! - `use arena`: an [`Arena`], reset after every command. The shell runs indefinitely.
//!
//! Commands are generic over the [`Allocator`] they use, so the same code runs both ways.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::alloc::Global;
use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt::{self, Write};
use green_tea_bare::arena::Arena;
use green_tea_bare::runtime::run;
use green_tea_bare::screen::Screen;
use green_tea_bare::{Board, Platform, boot, console, heap, memory, print, println};

/// Called by the platform's `_start`, on the boot stack, with .bss zeroed.
#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    let fb = boot("green-tea-shell");
    Screen::new(fb).finish(0); // the display has nothing to show: the shell is on the serial port
    run(shell, |_| {});
    println!("halting");
    Board::halt()
}

/// The longest line the shell reads. Typing past it does nothing.
const LINE_MAX: usize = 120;
/// The arena's size: far more than any command needs.
const ARENA_SIZE: usize = 64 * 1024;

/// Where a command's memory comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Heap,
    Arena,
}

struct Shell {
    source: Source,
    arena: Arena,
    /// Commands run so far, and how many of them weren't commands.
    commands: usize,
    unknown: usize,
    /// Run commands without printing what they print (`repeat`, `test`).
    quiet: bool,
}

/// What the shell does after a command.
#[derive(PartialEq, Eq)]
enum Next {
    Prompt,
    /// Take the following commands' memory from here.
    Use(Source),
    /// That wasn't a command.
    Unknown,
    Halt,
}

fn shell() {
    let mut shell = Shell { source: Source::Arena, arena: Arena::new(ARENA_SIZE), commands: 0, unknown: 0, quiet: false };
    println!("type `help` for the commands");
    let mut line = [0u8; LINE_MAX];
    loop {
        print!("> ");
        let len = read_line(&mut line);
        if shell.line(&line[..len]) == Next::Halt {
            return;
        }
    }
}

/// Read a line from the console into `line`, echoing it, and return its length. Backspace
/// works; anything else that isn't printable is ignored.
fn read_line(line: &mut [u8]) -> usize {
    let mut len = 0;
    loop {
        match console::read_byte() {
            b'\r' | b'\n' => {
                println!();
                return len;
            }
            0x08 | 0x7f if len > 0 => {
                len -= 1;
                print!("\x08 \x08");
            }
            byte @ b' '..=b'~' if len < line.len() => {
                line[len] = byte;
                len += 1;
                print!("{}", byte as char);
            }
            _ => {}
        }
    }
}

impl Shell {
    /// Run one line. `repeat` and `test` are handled here, outside any command: they run
    /// whole commands, each with memory of its own.
    fn line(&mut self, line: &[u8]) -> Next {
        let text = core::str::from_utf8(line).unwrap_or("").trim_ascii();
        if let Some(rest) = text.strip_prefix("repeat ") {
            self.repeat(rest.trim_ascii_start());
            Next::Prompt
        } else if text == "test" {
            self.test();
            Next::Prompt
        } else {
            self.command(text)
        }
    }

    /// Run one command with memory from the current source, and free that memory if the
    /// source can.
    fn command(&mut self, text: &str) -> Next {
        self.commands += 1;
        let next = match self.source {
            Source::Heap => execute(text, Global, self),
            Source::Arena => {
                let next = execute(text, &self.arena, self);
                self.arena.reset();
                next
            }
        };
        match next {
            Next::Use(source) => {
                self.source = source;
                Next::Prompt
            }
            Next::Unknown => {
                self.unknown += 1;
                Next::Prompt
            }
            next => next,
        }
    }

    /// `repeat <n> <command>`: run a command `n` times without its output, and report
    /// what that did to memory.
    fn repeat(&mut self, args: &str) {
        let (count, command) = args.split_once(' ').unwrap_or((args, ""));
        let (Ok(count), false) = (count.parse::<usize>(), command.trim_ascii().is_empty()) else {
            return println!("usage: repeat <n> <command>");
        };
        let heap_before = heap::used();
        self.quiet = true;
        for _ in 0..count {
            self.command(command);
        }
        self.quiet = false;
        println!(
            "ran `{command}` {count} times from the {}: heap {:+} bytes, arena peak {} bytes",
            self.source.name(),
            heap::used() - heap_before,
            self.arena.peak()
        );
    }

    /// `test`: check that commands run from the heap leave their memory behind, and that
    /// commands run from the arena leave nothing. Also fails if anything typed before it
    /// wasn't a command, which is how a lost or garbled byte on the serial port shows up.
    fn test(&mut self) {
        const RUNS: usize = 1000;
        const COMMAND: &str = "echo the quick brown fox jumps over the lazy dog";
        let source = self.source;
        self.quiet = true;

        self.source = Source::Heap;
        let before = heap::used();
        for _ in 0..RUNS {
            self.command(COMMAND);
        }
        let from_heap = heap::used() - before;

        self.source = Source::Arena;
        let before = heap::used();
        for _ in 0..RUNS {
            self.command(COMMAND);
        }
        let from_arena = heap::used() - before;

        self.quiet = false;
        self.source = source;
        let (peak, left) = (self.arena.peak(), self.arena.used());
        println!("{RUNS} commands from the heap:  heap +{from_heap} bytes ({} per command)", from_heap / RUNS);
        println!("{RUNS} commands from the arena: heap +{from_arena} bytes; arena peak {peak} bytes, {left} in use");
        if self.unknown != 0 {
            // Whatever was typed before `test` didn't arrive as it was typed.
            println!("FAIL: {} unknown command(s) before the test", self.unknown);
        } else if from_heap == 0 {
            println!("FAIL: commands run from the heap allocated nothing");
        } else if from_arena != 0 {
            println!("FAIL: commands run from the arena took {from_arena} bytes of heap");
        } else if left != 0 || peak == 0 || peak >= self.arena.capacity() {
            println!("FAIL: the arena has {left} bytes in use after a reset, and peaked at {peak}");
        } else {
            println!(
                "PASS: {RUNS} commands cost {from_heap} bytes of heap, or none from an arena that peaked at {peak}"
            );
        }
    }
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Source::Heap => "heap",
            Source::Arena => "arena",
        }
    }
}

/// Text being built up in a command's memory.
struct Text<A: Allocator>(Vec<u8, A>);

impl<A: Allocator> Write for Text<A> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// Run one command, allocating from `memory`: a copy of the line, its words, and what it
/// prints. All three are dropped on return, which gives the memory back only if `memory`
/// is an allocator that takes it.
fn execute<A: Allocator + Copy>(text: &str, memory: A, shell: &Shell) -> Next {
    let mut line = Vec::new_in(memory);
    line.extend_from_slice(text.as_bytes());
    let line = core::str::from_utf8(&line).unwrap_or("");

    let mut words = Vec::new_in(memory);
    words.extend(line.split_ascii_whitespace());

    let mut out = Text(Vec::new_in(memory));
    let mut next = Next::Prompt;
    // Writing to `out` can't fail: it only appends to a Vec.
    let _ = match words.as_slice() {
        [] => Ok(()),
        ["help"] => out.write_str(HELP),
        ["echo", rest @ ..] => {
            for (i, word) in rest.iter().enumerate() {
                let _ = write!(out, "{}{word}", if i == 0 { "" } else { " " });
            }
            out.write_str("\n")
        }
        ["mem"] => {
            let arena = &shell.arena;
            let _ = writeln!(out, "commands run: {}, now from the {}", shell.commands, shell.source.name());
            let _ = writeln!(out, "heap:  {:8} of {} bytes used", heap::used(), heap::capacity());
            writeln!(
                out,
                "arena: {:8} of {} bytes used, peak {}",
                arena.used(),
                arena.capacity(),
                arena.peak()
            )
        }
        ["map"] => {
            for s in memory::sections() {
                let _ = writeln!(out, "{:8} {:#010x}..{:#010x} {:9} bytes", s.name, s.start, s.end, s.size());
            }
            Ok(())
        }
        ["use", source @ ("heap" | "arena")] => {
            next = Next::Use(if *source == "heap" { Source::Heap } else { Source::Arena });
            writeln!(out, "commands now get their memory from the {source}")
        }
        ["halt"] => {
            next = Next::Halt;
            Ok(())
        }
        [other, ..] => {
            next = Next::Unknown;
            writeln!(out, "unknown command `{other}`; try `help`")
        }
    };
    if !shell.quiet {
        print!("{}", core::str::from_utf8(&out.0).unwrap_or(""));
    }
    next
}

const HELP: &str = "\
help               this text
echo <words>       print the words
mem                how much of the heap and the arena is in use
map                where the kernel's sections are
use heap|arena     where the next commands get their memory
repeat <n> <cmd>   run a command n times, without its output
test               check that the arena frees what the heap can't
halt               stop
";
