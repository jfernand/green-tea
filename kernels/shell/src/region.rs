//! `regions`: the session's script again, with its memory from regions, and what that
//! costs the heap next to the other ways of running it.
//!
//! There are two regions, and the lifetime of each is a type. The *session's* region
//! holds the variables: a list of [`Var`]s, with their names and values. The *command's*
//! region holds what one command works with (a copy of its line, its words, what it
//! prints) and is reset when the command is done. A command may read the session, and
//! what it wants to keep it has to copy there: `Var<'s>` has no room for a reference
//! that lives only as long as a command, and the compiler says so.
//!
//! The script is run four times with the heap recording (see replay.rs): commands and
//! variables on the heap; commands in the arena; commands in a region that is reset, and
//! variables in the session's region; and the same with a new region for every command.

use crate::{Shell, Source, Text};
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::Cell;
use core::fmt::Write;
use green_tea_bare::fit::trace::{self, Event};
use green_tea_bare::fit::{Fit, Order, Policy};
use green_tea_bare::{heap, print, println};
use green_tea_region::{Counts, Region, scope};

/// The size of the chunks both regions take from the heap.
const CHUNK: usize = 4096;
/// The region each recorded trace is replayed in, from the heap.
const REPLAY: usize = 512 * 1024;

/// A variable. It is a node of the session's list, and it, its name and its value are
/// all in the session's region.
struct Var<'s> {
    name: &'s str,
    value: Cell<&'s str>,
    next: Cell<Option<&'s Var<'s>>>,
}

impl Var<'_> {
    /// The bytes of the region this variable needs: itself, its name and its value.
    fn bytes(&self) -> usize {
        size_of::<Self>() + self.name.len() + self.value.get().len()
    }
}

/// What outlives a command: the variables.
struct Session<'s> {
    region: &'s Region,
    first: Cell<Option<&'s Var<'s>>>,
    /// The bytes the variables set now need, and the most that has been.
    live: Cell<usize>,
    most_live: Cell<usize>,
}

impl<'s> Session<'s> {
    fn new(region: &'s Region) -> Session<'s> {
        Session { region, first: Cell::new(None), live: Cell::new(0), most_live: Cell::new(0) }
    }

    fn vars(&self) -> impl Iterator<Item = &'s Var<'s>> {
        core::iter::successors(self.first.get(), |var| var.next.get())
    }

    fn get(&self, name: &str) -> Option<&'s str> {
        self.vars().find(|var| var.name == name).map(|var| var.value.get())
    }

    /// Give `name` the value `words`, joined by spaces. Everything kept is copied into the
    /// session's region: `name` and `words` are in a command's.
    fn set(&self, name: &str, words: &[&str]) {
        let len = words.iter().map(|word| word.len()).sum::<usize>() + words.len().saturating_sub(1);
        let mut text = Vec::with_capacity_in(len, self.region);
        for (i, word) in words.iter().enumerate() {
            text.extend_from_slice(if i == 0 { b"" } else { b" " });
            text.extend_from_slice(word.as_bytes());
        }
        let value: &'s str = core::str::from_utf8(text.leak()).unwrap_or("");

        let mut link = &self.first;
        while let Some(var) = link.get() {
            if var.name == name {
                // The old value stays where it is, unused, until the session ends.
                self.live.set(self.live.get() - var.value.get().len() + value.len());
                self.most_live.set(self.most_live.get().max(self.live.get()));
                return var.value.set(value);
            }
            link = &var.next;
        }
        let var = self.region.alloc(Var {
            name: self.region.alloc_str(name),
            value: Cell::new(value),
            next: Cell::new(None),
        });
        self.live.set(self.live.get() + var.bytes());
        self.most_live.set(self.most_live.get().max(self.live.get()));
        link.set(Some(var));
    }

    /// Take `name` off the list. Its node, its name and its value stay in the region.
    fn unset(&self, name: &str) {
        let mut link = &self.first;
        while let Some(var) = link.get() {
            if var.name == name {
                self.live.set(self.live.get() - var.bytes());
                return link.set(var.next.get());
            }
            link = &var.next;
        }
    }
}

/// Run one command, with its working memory in `command` and the variables in `session`.
/// The commands are the ones the script uses, and `vars`.
fn run(text: &str, session: &Session, command: &Region, quiet: bool) {
    let line: &str = command.alloc_str(text);
    let mut words = Vec::new_in(command);
    words.extend(line.split_ascii_whitespace());
    let words: &[&str] = words.leak();

    let mut out = Text(Vec::new_in(command));
    // Writing to `out` can't fail: it only appends to a Vec.
    let _ = match words {
        [] => Ok(()),
        ["echo", rest @ ..] => {
            for (i, word) in rest.iter().enumerate() {
                let word = match word.strip_prefix('$') {
                    Some(name) => session.get(name).unwrap_or(""),
                    None => word,
                };
                let _ = write!(out, "{}{word}", if i == 0 { "" } else { " " });
            }
            out.write_str("\n")
        }
        ["set", name, value @ ..] => {
            session.set(name, value);
            Ok(())
        }
        ["unset", name] => {
            session.unset(name);
            Ok(())
        }
        ["vars"] => {
            for var in session.vars() {
                let _ = writeln!(out, "{} = {}", var.name, var.value.get());
            }
            Ok(())
        }
        [other, ..] => writeln!(out, "`{other}` is not a command that runs from a region"),
    };
    if !quiet {
        print!("{}", core::str::from_utf8(&out.0).unwrap_or(""));
    }
}

/// What a run of the script asked of the heap, and what first fit by address would have
/// needed for it (replay.rs has the columns' meanings).
struct Asked {
    allocations: usize,
    frees: usize,
    bytes: usize,
    peak_live: usize,
    footprint: usize,
}

/// How a run of the script ended: how many variables were set, and a hash of their names
/// and values in order (FNV-1a).
#[derive(Clone, Copy, PartialEq, Eq)]
struct Ending {
    variables: usize,
    hash: u64,
}

impl Ending {
    fn of<'a>(vars: impl Iterator<Item = (&'a str, &'a str)>) -> Ending {
        let mut ending = Ending { variables: 0, hash: 0xcbf2_9ce4_8422_2325 };
        for (name, value) in vars {
            ending.variables += 1;
            for byte in name.bytes().chain(*b"=").chain(value.bytes()).chain(*b"\n") {
                ending.hash = (ending.hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        ending
    }
}

/// What the two regions of a run came to.
struct Regions {
    command: Counts,
    session: Counts,
    /// What the session counted for the variables set at the end, and for the most ever
    /// set; and the first of those again, added up from the list.
    live: usize,
    most_live: usize,
    listed: usize,
}

/// The replayer, and the buffer the heap records into, which each run hands to the next.
struct Recorder {
    buffer: Option<Box<[Event]>>,
    replay_in: Vec<u128>,
    /// The heap's counts of allocations and frees when the recording started.
    before: (usize, usize),
}

impl Recorder {
    /// Start recording what the heap is asked for.
    fn start(&mut self) -> Result<(), &'static str> {
        let buffer = self.buffer.take().ok_or("the recorder lost its buffer")?;
        self.before = heap::with(|fit| (fit.stats().allocations, fit.stats().frees));
        drop(heap::record(buffer));
        Ok(())
    }

    /// Stop recording, and replay what was recorded. (Apart from `start`, so that the
    /// script doesn't run on top of this function's frame: a task's stack is 16 KiB.)
    fn finish(&mut self) -> Result<Asked, &'static str> {
        let recording = heap::stop_recording().ok_or("the heap stopped recording by itself")?;
        let after = heap::with(|fit| (fit.stats().allocations, fit.stats().frees));
        let events = &recording.events[..recording.len];
        let frees = events.iter().filter(|event| event.is_free()).count();
        let counted = (after.0 - self.before.0, after.1 - self.before.1);
        if recording.dropped != 0 {
            return Err("the trace is incomplete: events were dropped");
        }
        if (events.len() - frees, frees) != counted {
            return Err("the trace doesn't have the allocations and frees the heap counted");
        }
        // SAFETY: the memory is ours, and this is the only allocator using it.
        let mut fit =
            unsafe { Fit::new(self.replay_in.as_mut_ptr() as usize, REPLAY, Policy::FirstFit, Order::Address) };
        let report = trace::replay(events, &mut fit);
        fit.check()?;
        if report.failed != 0 || report.unmatched != 0 {
            return Err("a trace couldn't be replayed");
        }
        let asked = Asked {
            allocations: counted.0,
            frees: counted.1,
            bytes: events.iter().map(|event| event.size as usize).sum(),
            peak_live: report.peak_live,
            footprint: report.footprint,
        };
        self.buffer = Some(recording.events);
        Ok(asked)
    }
}

impl Shell {
    /// `regions`: run the script each way, print what each asked of the heap, and judge.
    pub fn regions(&mut self) {
        // The runs record the heap themselves, and a session starts with no variables.
        if heap::is_recording() {
            return println!("`regions` needs the recorder: a recording is under way, `trace off` first");
        }
        if !self.vars.borrow().is_empty() {
            return println!("`regions` runs with no variables: `unset` the ones in `vars` first");
        }
        let in_use = heap::used();
        // The heap is looked at again before the verdict is put together: its text is on the heap.
        match (self.compare(), heap::used()) {
            (Err(e), _) => println!("FAIL: {e}"),
            (Ok(_), now) if now != in_use => {
                println!("FAIL: the heap had {in_use} bytes in use before the four runs, and has {now} after them")
            }
            (Ok([commands, heap, arena, regions, held, live]), _) => println!(
                "PASS: {commands} commands asked the heap for {heap} blocks, {arena} with an arena, and {regions} with \
                 two regions, which held {held} bytes at the end for {live} of variables"
            ),
        }
    }

    /// The script, run by the shell's own commands with their memory from `source` and the
    /// variables on the heap. The session ends inside the run: the variables are dropped.
    fn on_the_heap(&mut self, source: Source) -> Ending {
        let was = (self.source, self.quiet);
        (self.source, self.quiet) = (source, true);
        Shell::script(|line| _ = self.command(line));
        (self.source, self.quiet) = was;
        let vars = self.vars.take();
        Ending::of(vars.iter().map(|(name, value)| (name.as_str(), value.as_str())))
    }

    /// Returns the numbers of the verdict.
    fn compare(&mut self) -> Result<[usize; 6], &'static str> {
        let mut recorder = Recorder {
            buffer: Some(vec![Event::default(); crate::replay::TRACE_EVENTS].into_boxed_slice()),
            replay_in: vec![0u128; REPLAY / 16],
            before: (0, 0),
        };
        let mut commands = 0;
        Shell::script(|_| commands += 1);

        recorder.start()?;
        let on_heap = self.on_the_heap(Source::Heap);
        let heap = recorder.finish()?;

        recorder.start()?;
        let on_arena = self.on_the_heap(Source::Arena);
        let arena = recorder.finish()?;

        recorder.start()?;
        let (in_regions, regions) = in_two_regions();
        let reset = recorder.finish()?;

        recorder.start()?;
        let in_fresh = in_a_region_each();
        let fresh = recorder.finish()?;

        println!("the session's script, {commands} commands, run four times with the heap recording:");
        println!(
            "  a command's memory    the variables   allocations      frees  bytes asked for  peak live  footprint"
        );
        for (command, variables, asked) in [
            ("the heap", "the heap", &heap),
            ("the arena", "the heap", &arena),
            ("a region, reset", "a region", &reset),
            ("a region, new", "a region", &fresh),
        ] {
            println!(
                "  {command:21} {variables:13} {:13} {:10} {:16} {:10} {:10}",
                asked.allocations, asked.frees, asked.bytes, asked.peak_live, asked.footprint
            );
        }
        let (command, session) = (regions.command, regions.session);
        println!(
            "the command's region: {} resets, {} chunk of {CHUNK} bytes taken from the heap, at most {} bytes of it in use",
            command.resets, command.taken, command.peak
        );
        println!(
            "the session's region: {} chunks of {CHUNK} bytes, {} bytes in use at the end in {} blocks",
            session.chunks, session.used, session.blocks
        );
        println!(
            "the variables: {} set at the end, in {} of those bytes; the most ever set at once needed {}",
            in_regions.variables, regions.live, regions.most_live
        );

        heap::with(|fit| fit.check())?;
        if [on_arena, in_regions, in_fresh].iter().any(|ending| *ending != on_heap) || on_heap.variables == 0 {
            return Err("the four runs didn't end with the same variables");
        }
        if [&heap, &arena, &reset, &fresh].iter().any(|asked| asked.allocations != asked.frees) {
            return Err("a run ended with blocks of the heap still allocated");
        }
        if command.resets != commands || command.taken != 1 || command.most_chunks != 1 {
            return Err("the command's region wasn't one chunk, reset after every command");
        }
        if reset.allocations != command.taken + session.taken || fresh.allocations != commands + session.taken {
            return Err("the regions didn't take from the heap what they say they took");
        }
        if regions.live != regions.listed {
            return Err("the session miscounted the bytes of its variables");
        }
        if regions.live > session.used || regions.most_live > session.used || session.used > session.held {
            return Err("the session's region holds less than the variables in it");
        }
        if !(reset.allocations < arena.allocations && arena.allocations < heap.allocations) {
            return Err("the regions didn't spare the heap any allocations");
        }
        Ok([commands, heap.allocations, arena.allocations, reset.allocations, session.held, regions.live])
    }
}

/// The script with two regions: one for the session, and one for the command being run,
/// reset after each. Both are dropped on return, which is the session ending.
fn in_two_regions() -> (Ending, Regions) {
    let session_region = Region::new(CHUNK);
    let mut command_region = Region::new(CHUNK);
    let session = Session::new(&session_region);
    Shell::script(|line| {
        run(line, &session, &command_region, true);
        command_region.reset();
    });
    let ending = Ending::of(session.vars().map(|var| (var.name, var.value.get())));
    let regions = Regions {
        command: command_region.counts(),
        session: session_region.counts(),
        live: session.live.get(),
        most_live: session.most_live.get(),
        listed: session.vars().map(|var| var.bytes()).sum(),
    };
    (ending, regions)
}

/// The script with a region for the session, and a new region for every command.
fn in_a_region_each() -> Ending {
    let session_region = Region::new(CHUNK);
    let session = Session::new(&session_region);
    Shell::script(|line| scope(CHUNK, |command_region| run(line, &session, command_region, true)));
    Ending::of(session.vars().map(|var| (var.name, var.value.get())))
}
