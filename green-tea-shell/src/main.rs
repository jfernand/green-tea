//! A bare-metal kernel with a shell on the serial console: `cargo xtask shell` to type at
//! it, `cargo xtask kernel --kernel shell` to test it.
//!
//! The shell is one task. It reads a line, runs it as a command, and does that forever,
//! which is the point: unlike the demos, it never stops allocating. Each command
//! allocates its working memory (a copy of the line, the words it splits into, the text
//! it prints) and is done with all of it when it returns.
//!
//! Where that memory comes from is switchable, with `use`:
//! - `use heap`: the global heap, green-tea-bare's TLSF allocator. Each of a command's
//!   blocks is allocated, and freed, on its own when the command returns.
//! - `use arena`: an [`Arena`], reset after every command.
//!
//! Either way the shell runs indefinitely. `heap` prints the heap block by block.
//!
//! Commands are generic over the [`Allocator`] they use, so the same code runs both ways.
//!
//! The shell has variables (`set`, `unset`, `vars`, and `$name` in an `echo`). They outlive
//! the command that sets them, so they are always on the heap, whatever `use` says.
//!
//! `trace on` records every allocation and free on the heap, `session` is a long script
//! to record, and `replay` runs what was recorded against each of green-tea-fit's policies
//! and measures the fragmentation (replay.rs). `trace dump` writes the recording to the
//! console as a trace file, for the host to keep (dump.rs).

#![no_std]
#![no_main]

extern crate alloc;

mod harden;
#[cfg(feature = "board-pc")]
mod irq;
#[cfg(feature = "board-pc")]
mod lights;
mod region;
mod dump;
mod dispatch;
mod pages;
mod replay;

use alloc::alloc::Global;
use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::Allocator;
use core::cell::RefCell;
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
/// The most blocks `heap` prints.
const HEAP_BLOCKS: usize = 24;
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
    /// Run commands without printing what they print (`repeat`, `test`, `session`).
    quiet: bool,
    /// The variables, by name. On the heap whatever the source: they outlive commands.
    vars: RefCell<Vec<(String, String)>>,
    /// What `trace on` and `trace off` recorded, and how many allocations and frees the
    /// heap counted between the two.
    trace: Option<heap::Recording>,
    counted: (usize, usize),
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
    let mut shell = Shell {
        source: Source::Arena,
        arena: Arena::new(ARENA_SIZE),
        commands: 0,
        unknown: 0,
        quiet: false,
        vars: RefCell::new(Vec::new()),
        trace: None,
        counted: (0, 0),
    };
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
        match dispatch::parse(text) {
            dispatch::Line::Repeat(rest) => self.repeat(rest),
            dispatch::Line::Test => {
                self.test();
                Next::Prompt
            }
            dispatch::Line::Trace(arg) => {
                self.trace(arg);
                Next::Prompt
            }
            dispatch::Line::Session => {
                self.session();
                Next::Prompt
            }
            dispatch::Line::Replay => {
                self.replay();
                Next::Prompt
            }
            dispatch::Line::Harden => {
                self.harden();
                Next::Prompt
            }
            dispatch::Line::Regions => {
                self.regions();
                Next::Prompt
            }
            dispatch::Line::Pages(arg) => {
                self.pages(arg);
                Next::Prompt
            }
            dispatch::Line::Command(text) => self.command(text),
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
    fn repeat(&mut self, args: &str) -> Next {
        let (count, command) = args.split_once(' ').unwrap_or((args, ""));
        let (Ok(count), false) = (count.parse::<usize>(), command.trim_ascii().is_empty()) else {
            println!("usage: repeat <n> <command>");
            return Next::Prompt;
        };
        // These are handled by `line`, around a command's memory: `command` doesn't know them.
        if let Some(word) = command
            .split_ascii_whitespace()
            .next()
            .filter(|word| dispatch::is_whole_line_word(word))
        {
            println!("`repeat` runs one command at a time, and can't run `{word}`");
            return Next::Prompt;
        }
        let unknown = self.unknown;
        let (used, before) = (heap::used(), heap::with(|fit| fit.stats()));
        self.quiet = true;
        let mut halted = false;
        for _ in 0..count {
            if self.command(command) == Next::Halt {
                halted = true;
                break;
            }
            if self.unknown != unknown {
                break; // it will be unknown every time
            }
        }
        self.quiet = false;
        if halted {
            return Next::Halt;
        }
        if self.unknown != unknown {
            println!("unknown command `{command}`; try `help`");
            return Next::Prompt;
        }
        let after = heap::with(|fit| fit.stats());
        println!(
            "ran `{command}` {count} times from the {}: {} heap allocations, {} frees, heap {:+} bytes; arena peak {} \
             bytes",
            self.source.name(),
            after.allocations - before.allocations,
            after.frees - before.frees,
            heap::used() as isize - used as isize,
            self.arena.peak()
        );
        Next::Prompt
    }

    /// `trace on`, `trace off`: record what the heap is asked for, into a buffer that is
    /// itself taken from the heap, before the recording starts.
    fn trace(&mut self, arg: &str) {
        match arg {
            "on" => {
                drop(heap::stop_recording()); // one under way: its buffer's free isn't in the new one
                self.trace = None; // frees the last trace's buffer
                let buffer = alloc::vec![Default::default(); replay::TRACE_EVENTS].into_boxed_slice();
                // The runtime allocates its list of sleeping tasks the first time a task
                // sleeps, which is whenever the console first has nothing to read. Sleep
                // now, so that it isn't sometimes in the trace and sometimes not.
                green_tea_bare::task::sleep_ms(1);
                self.counted = heap::with(|fit| (fit.stats().allocations, fit.stats().frees));
                heap::record(buffer);
                println!("recording the heap's allocations and frees, up to {} of them", replay::TRACE_EVENTS);
            }
            "off" => match heap::stop_recording() {
                Some(recording) => {
                    let (allocations, frees) = heap::with(|fit| (fit.stats().allocations, fit.stats().frees));
                    self.counted = (allocations - self.counted.0, frees - self.counted.1);
                    println!("recorded {} events, and dropped {}", recording.len, recording.dropped);
                    self.trace = Some(recording);
                }
                None => println!("nothing was being recorded"),
            },
            "dump" => self.dump(),
            "check" => self.check(),
            _ => println!("usage: trace on|off|dump|check"),
        }
    }

    /// What `repeat` does with a command that isn't an ordinary one.
    fn repeats(&mut self) -> Result<(), &'static str> {
        if self.repeat("1 halt") != Next::Halt {
            return Err("`repeat 1 halt` didn't halt");
        }
        let unknown = self.unknown;
        self.repeat("2 repeat 2 echo nested");
        if self.unknown != unknown {
            self.unknown = unknown;
            return Err("a nested `repeat` counted as an unknown command");
        }
        Ok(())
    }

    /// A second `trace on` starts a clean recording: the old one's buffer is freed before
    /// it starts. Leaves alone a recording that was typed, and the last one kept.
    fn trace_twice(&mut self) -> Result<(), &'static str> {
        if heap::is_recording() {
            return Ok(());
        }
        let kept = (self.trace.take(), self.counted);
        self.trace("on");
        self.trace("on");
        let events = heap::stop_recording().map_or(0, |recording| recording.len);
        (self.trace, self.counted) = kept;
        if events != 0 {
            return Err("a second `trace on` put the old buffer's free in the new trace");
        }
        Ok(())
    }

    /// `regions` leaves a recording under way, and the variables, as they were.
    fn regions_refuse(&mut self) -> Result<(), &'static str> {
        let vars = self.vars.take();
        self.vars.borrow_mut().push((String::from("kept"), String::from("value")));
        self.regions();
        let kept = self.vars.borrow().len() == 1;
        self.vars.replace(vars);
        if !kept {
            return Err("`regions` discarded the variables");
        }
        if !heap::is_recording() {
            heap::record(alloc::vec![Default::default(); 16].into_boxed_slice());
            self.regions();
            if heap::stop_recording().is_none() {
                return Err("`regions` discarded a recording under way");
            }
        }
        Ok(())
    }

    /// `test`: check that commands run from the heap allocate there and free all of it,
    /// and that commands run from the arena don't touch the heap. Also fails if anything
    /// typed before it wasn't a command, which is how a lost or garbled byte on the serial
    /// port shows up.
    fn test(&mut self) {
        const RUNS: usize = 1000;
        const COMMAND: &str = "echo the quick brown fox jumps over the lazy dog";
        let source = self.source;
        self.quiet = true;
        let now = || heap::with(|fit| (fit.used(), fit.stats().allocations));

        self.source = Source::Heap;
        let before = now();
        for _ in 0..RUNS {
            self.command(COMMAND);
        }
        let (left, from_heap) = (now().0 as isize - before.0 as isize, now().1 - before.1);

        self.source = Source::Arena;
        let before = now();
        for _ in 0..RUNS {
            self.command(COMMAND);
        }
        let from_arena = now().1 - before.1;

        self.quiet = false;
        self.source = source;
        let (peak, in_use) = (self.arena.peak(), self.arena.used());
        println!("{RUNS} commands from the heap:  {from_heap} heap allocations, heap {left:+} bytes afterwards");
        println!("{RUNS} commands from the arena: {from_arena} heap allocations; arena peak {peak} bytes, {in_use} in use");
        if self.unknown != 0 {
            // Whatever was typed before `test` didn't arrive as it was typed.
            println!("FAIL: {} unknown command(s) before the test", self.unknown);
        } else if from_heap < 3 * RUNS {
            println!("FAIL: commands run from the heap made {from_heap} allocations, fewer than three each");
        } else if left != 0 {
            println!("FAIL: commands run from the heap left {left} bytes allocated");
        } else if from_arena != 0 {
            println!("FAIL: commands run from the arena made {from_arena} heap allocations");
        } else if in_use != 0 || peak == 0 || peak >= self.arena.capacity() {
            println!("FAIL: the arena has {in_use} bytes in use after a reset, and peaked at {peak}");
        } else if let Err(e) = heap::with(|fit| fit.check()) {
            println!("FAIL: the heap is inconsistent: {e}");
        } else if let Err(e) = empty_arena().and_then(|()| self.repeats()).and_then(|()| self.trace_twice()).and_then(|()| self.regions_refuse()) {
            println!("FAIL: {e}");
        } else {
            println!(
                "PASS: {RUNS} commands made {from_heap} heap allocations and freed them all, or none from an arena \
                 that peaked at {peak}"
            );
        }
    }
}

/// An arena of no bytes asks the heap for nothing, and has nothing to give.
fn empty_arena() -> Result<(), &'static str> {
    let counts = || heap::with(|fit| (fit.stats().allocations, fit.stats().frees));
    let before = counts();
    let arena = Arena::new(0);
    let refused = arena.allocate(core::alloc::Layout::new::<u8>()).is_err();
    let capacity = arena.capacity();
    drop(arena);
    if counts() != before {
        return Err("an arena of no bytes used the heap");
    }
    if capacity != 0 || !refused {
        return Err("an arena of no bytes handed out a byte");
    }
    Ok(())
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
fn execute<A: Allocator + Copy>(text: &str, allocator: A, shell: &Shell) -> Next {
    let mut line = Vec::new_in(allocator);
    line.extend_from_slice(text.as_bytes());
    let line = core::str::from_utf8(&line).unwrap_or("");

    let mut words = Vec::new_in(allocator);
    words.extend(line.split_ascii_whitespace());

    let mut out = Text(Vec::new_in(allocator));
    let mut next = Next::Prompt;
    // Writing to `out` can't fail: it only appends to a Vec.
    let _ = match words.as_slice() {
        [] => Ok(()),
        #[cfg(not(feature = "board-pc"))]
        ["help"] => out.write_str(HELP),
        #[cfg(feature = "board-pc")]
        ["help"] => out.write_str(HELP).and_then(|()| out.write_str(irq::HELP)).and_then(|()| out.write_str(lights::HELP)),
        #[cfg(feature = "board-pc")]
        ["irq", args @ ..] => irq::command(args, &mut out),
        #[cfg(feature = "board-pc")]
        ["lights", args @ ..] => lights::command(args, &mut out),
        ["echo", rest @ ..] => {
            let vars = shell.vars.borrow();
            for (i, word) in rest.iter().enumerate() {
                // `$name` is the variable's value, or nothing if there is no such variable.
                let word = match word.strip_prefix('$') {
                    Some(name) => vars.iter().find(|(n, _)| n == name).map_or("", |(_, value)| value),
                    None => word,
                };
                let _ = write!(out, "{}{word}", if i == 0 { "" } else { " " });
            }
            out.write_str("\n")
        }
        ["set", name, value @ ..] => {
            // The value is built on the heap, not in `memory`: it has to outlive the command.
            let mut text = String::new();
            for (i, word) in value.iter().enumerate() {
                text.push_str(if i == 0 { "" } else { " " });
                text.push_str(word);
            }
            let mut vars = shell.vars.borrow_mut();
            match vars.iter_mut().find(|(n, _)| n == name) {
                Some((_, old)) => *old = text,
                None => vars.push((String::from(*name), text)),
            }
            Ok(())
        }
        ["unset", name] => {
            shell.vars.borrow_mut().retain(|(n, _)| n != name);
            Ok(())
        }
        ["vars"] => {
            for (name, value) in shell.vars.borrow().iter() {
                let _ = writeln!(out, "{name} = {value}");
            }
            Ok(())
        }
        ["mem"] => {
            let arena = &shell.arena;
            let _ = writeln!(out, "commands run: {}, now from the {}", shell.commands, shell.source.name());
            let (free_blocks, largest) = heap::with(|fit| (fit.free_list().count(), fit.largest_free()));
            let _ = writeln!(
                out,
                "heap:  {:8} of {} bytes used; {free_blocks} free blocks, the largest {largest} bytes",
                heap::used(),
                heap::capacity()
            );
            writeln!(
                out,
                "arena: {:8} of {} bytes used, peak {}",
                arena.used(),
                arena.capacity(),
                arena.peak()
            )
        }
        ["heap"] => {
            // The walk can't write to `out` as it goes: `out` may be on the heap, and the
            // heap can't be used while it is being walked. Copy the blocks out first.
            let mut blocks = [None; HEAP_BLOCKS];
            let total = heap::with(|fit| {
                fit.blocks().zip(&mut blocks).for_each(|(block, slot)| *slot = Some(block));
                fit.blocks().count()
            });
            for block in blocks.iter().flatten() {
                let state = if block.used { "allocated" } else { "free" };
                let _ = writeln!(out, "{:#010x} {:8} bytes {state}", block.start, block.size);
            }
            if total > HEAP_BLOCKS {
                let _ = writeln!(out, "and {} more blocks", total - HEAP_BLOCKS);
            }
            Ok(())
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
heap               the heap, block by block
map                where the kernel's sections are
use heap|arena     where the next commands get their memory
set <name> <words> give a variable a value; `echo $name` prints it
unset <name>       forget a variable
vars               the variables
trace on|off       record the heap's allocations and frees
trace dump         write what was recorded to the console, as a trace file
trace check        write it as a file in memory, and read it back
session            run a script of a few thousand commands, from the heap
replay             replay what was recorded against each policy
harden             replay it against the hardened allocator, a defence at a time
regions            run the session with its memory from regions, and compare (with no
                   variables and no recording under way)
pages [trim|replay] the heap's pages: how many are mapped; give back the idle ones;
                   replay what was recorded and count the pages it touches
repeat <n> <cmd>   run a command n times, without its output
test               check that commands free what they allocate, either way
halt               stop
";
