#import "../lib.typ": *

= green-tea-shell <crate-shell>

`green-tea-shell` is a kernel with a shell on the serial console. It is one task that reads a line, runs it as a command, and repeats, and it never stops allocating, which makes it the workspace's source of a real allocation trace. It can take each command's memory from the heap or from an arena, record every allocation and free, replay the recording against each allocator, and write it out as a trace file.

#spec(
  ("Source", [#repo("green-tea-shell/src/"), 6 files, 1,773 lines]),
  ("Depends on", [`green-tea-bare` (@crate-bare), `green-tea-region` (@crate-region)]),
  ("Used by", [nothing: it is a binary. `cargo xtask trace` reads what it writes (@crate-xtask)]),
  ("Tests", [the `shell`, `trace`, `harden`, `region`, `gtt` and `resident` kernels, which type at it]),
)

#dtable(columns: (auto, 1fr), ("Module", "What it holds"),
  [`main`], [`kmain`, the line reader, `Shell`, the commands that run from a command's memory],
  [`replay`], [`session`, `script`, `replay`, `TRACE_EVENTS`],
  [`harden`], [`harden`: the trace against `Hardened`, a defence at a time],
  [`region`], [`regions`: the script with its memory in two regions],
  [`pages`], [`pages`, `pages trim`, `pages replay`],
  [`dump`], [`trace dump` and `trace check`, through `dump` and `check`],
)

== The commands

#dtable(columns: (auto, 1fr), ("Command", "What it does"),
  [`help`], [prints the list],
  [`echo <words>`], [prints the words; `$name` is a variable's value, or nothing],
  [`set <name> <words>`], [gives a variable a value],
  [`unset <name>`], [forgets a variable],
  [`vars`], [lists the variables],
  [`mem`], [commands run, and how much of the heap and arena is in use],
  [`heap`], [the heap block by block, up to 24 blocks],
  [`map`], [where the kernel's sections are],
  [`use heap|arena`], [where the next commands get their memory],
  [`repeat <n> <cmd>`], [runs a command `n` times, without its output, and reports what that did to memory],
  [`test`], [checks that commands free what they allocate, from either source],
  [`trace on|off`], [records the heap's allocations and frees],
  [`trace dump`], [writes the recording to the console as a trace file],
  [`trace check`], [writes it as a file in memory, in both forms, and reads it back],
  [`session`], [runs a script of 1,970 commands, from the heap],
  [`replay`], [replays the recording against each policy],
  [`harden`], [replays it against the hardened allocator],
  [`regions`], [runs the session from regions, and compares],
  [`pages [trim|replay]`], [the heap's pages; gives back the idle ones; replays the recording and counts the pages it touches],
  [`halt`], [stops],
)

A session, typed at the serial port:

#output(title: "typed at the shell, on riscv64")[
```
> set tea green
> set pot a small one
> echo $tea $pot $nothing
green a small one
> vars
tea = green
pot = a small one
> mem
commands run: 10, now from the arena
heap:     65952 of 4194288 bytes used; 1 free blocks, the largest 4128336 bytes
arena:      296 of 65536 bytes used, peak 1373
> heap
0x803f6008       48 bytes allocated
0x803f6038    65552 bytes allocated
0x80406048       80 bytes allocated
0x80406098       32 bytes allocated
0x804060b8       32 bytes allocated
0x804060d8      208 bytes allocated
0x804061a8  4128336 bytes free
> map
.text    0x80200000..0x8027cfde    511966 bytes
.rodata  0x8027cfe0..0x8028fdd3     77299 bytes
.data    0x80290a30..0x80290b00       208 bytes
.bss     0x80291000..0x803e6000   1396736 bytes
.stack   0x803e6000..0x803f6000     65536 bytes
.heap    0x803f6000..0x807f6000   4194304 bytes
.stacks  0x80800000..0x80a00000   2097152 bytes
```
]

The block of 65,552 bytes is the arena: 64 KiB and its two tags.

== The line and the command

`kmain` boots, blanks the display, and runs one task. The task's loop is the whole shell:

#srcfile("green-tea-shell/src/main.rs", lines: ((114, 121),))[```rust
let mut line = [0u8; LINE_MAX];
loop {
    print!("> ");
    let len = read_line(&mut line);
    if shell.line(&line[..len]) == Next::Halt {
        return;
    }
}
```]

`read_line` echoes what it reads into a buffer of `LINE_MAX` (120) bytes. Backspace works, bytes that aren't printable are ignored, and typing past the end does nothing:

#srcfile("green-tea-shell/src/main.rs", lines: ((126, 146),))[```rust
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
```]

`Shell::line` handles the commands that run whole commands or need the shell mutably (`repeat`, `test`, `trace`, `session`, `replay`, `harden`, `regions`, `pages`), and passes the rest to `command`, where the source of memory is chosen:

#srcfile("green-tea-shell/src/main.rs", lines: ((183, 204),), notes: ((186, [`Global` here is `alloc::alloc::Global`, the heap.]), (189, [An arena gets all of a command's memory back by moving one pointer.]),))[```rust
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
```]

`execute` is generic over the `Allocator`, so the same code runs both ways. It makes three allocations of its own before it looks at the command: a copy of the line, the list of words, and a buffer for what it prints.

#srcfile("green-tea-shell/src/main.rs", lines: ((428, 439),), notes: ((428, [`Copy`: both the global allocator and `&Arena` are.]),))[```rust
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
```]

Variables outlive the command that sets them, so they are always on the heap, whatever `use` says. `heap` can't write to `out` while it walks the heap, since `out` may be on the heap, so it copies up to `HEAP_BLOCKS` blocks to the stack first.

`repeat` measures what a command costs. From the heap, `echo a b c` makes three allocations each time, and from the arena none:

#output(title: "typed at the shell")[
```
ran `echo a b c` 100 times from the heap: 300 heap allocations, 300 frees, heap +0 bytes; arena peak 1373 bytes
ran `echo a b c` 100 times from the arena: 0 heap allocations, 0 frees, heap +0 bytes; arena peak 1373 bytes
```
]

`repeat` runs one ordinary command at a time. The eight commands in `WHOLE_LINES`, which `line` handles around a command's memory, are refused, and `repeat 1 halt` halts:

#output(title: "typed at the shell")[
```
> repeat 2 repeat 2 echo x
`repeat` runs one command at a time, and can't run `repeat`
> repeat 1 halt
halting
```
]

`test` is the same measurement with a verdict. It runs a fixed `echo` 1,000 times from each source, and fails if the heap didn't get every byte back, if the arena touched the heap, or if any earlier line wasn't a command, which is how a garbled byte on the serial port shows up. It then checks four behaviours of the shell itself: an arena of no bytes takes nothing from the heap (`empty_arena`), `repeat` halts and refuses to nest (`repeats`), a second `trace on` starts a clean recording (`trace_twice`), and `regions` leaves a recording and the variables alone (`regions_refuse`):

#output(title: "cargo xtask kernel --kernel shell --target riscv64")[
```
> test
1000 commands from the heap:  8000 heap allocations, heap +0 bytes afterwards
1000 commands from the arena: 0 heap allocations; arena peak 368 bytes, 0 in use
PASS: 1000 commands made 8000 heap allocations and freed them all, or none from an arena that peaked at 368
```
]

== The trace recording

`trace on` asks `green-tea-bare`'s heap to record into a buffer of `TRACE_EVENTS` (65,536) events, a mebibyte, taken from the heap *before* the recording starts. A recording already under way is stopped first, and its buffer freed, so that the free isn't in the new trace:

#srcfile("green-tea-shell/src/main.rs", lines: ((257, 268),), notes: ((264, [The runtime allocates its list of sleepers the first time a task sleeps, which is whenever the console first has nothing to read. Sleeping here keeps that allocation out of the trace, so that a trace doesn't depend on timing.]), (265, [What the heap has counted so far. `replay` later checks the trace against the heap's own count.]),))[```rust
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
```]

`session` gives the recording something to record. `script` is a deterministic program of commands, driven by `Random` from a fixed `SEED`, in `ROUNDS` (12) rounds, as a person at a shell might work: set some variables, read them, change some, read them again, and unset all but one in eight:

#srcfile("green-tea-shell/src/replay.rs", lines: ((112, 120), (135, 140),), notes: ((114, [16 to 63 variables in a round.]), (135, [One variable in eight survives its round. These are the long-lived blocks.]),))[```rust
let mut random = Random(SEED);
for round in 0..ROUNDS {
    let count = 16 + random.below(48);
    for i in 0..count {
        scripted(&mut run, |line| {
            let _ = write!(line, "set r{round}v{i}");
            line.words(&mut random, 12);
        });
    }
// …
    for i in (0..count).filter(|i| i % 8 != 0) {
        scripted(&mut run, |line| {
            let _ = write!(line, "unset r{round}v{i}");
        });
    }
}
```]

Its lines are built on the stack in a `Line`, so that the trace holds only what the commands allocate. The result is the same on each run:

#output(title: "cargo xtask kernel --kernel trace --target riscv64")[
```
session: 1970 commands in 12 rounds, 74 variables left set
recorded 20483 events, and dropped 0
the trace: 20483 events, 10316 allocations of 656120 bytes in all, 10167 frees
sizes: 167 different ones, from 4 to 6144 bytes; the most common: 64 (22.6%) 128 (12.1%) 8 (6.3%) 16 (5.5%) 32 (4.9%) 5 (3.5%)
lifetimes: 85.1% of the blocks are freed before 8 more are allocated, 88.0% before 64 more; 149 are never freed
```
]

== Replay

`replay` first checks that the trace holds the allocations and frees the heap counted while it was recording. Then it replays it in a region of 512 KiB, three ways: as recorded, shuffled by `trace::shuffled` (the same sizes and lifetimes dealt out at random, which is how allocators were tested for decades), and split by `trace::by_lifetime`, with the blocks that outlive `LONG_LIVED` (64) allocations in a region of their own. Each runs under the six combinations in `WAYS`, and the first also against `Buddy` and `Tlsf`.

#output(title: "cargo xtask kernel --kernel trace --target riscv64")[
```
replayed as recorded, each in a region of 512 KiB:
  policy     order     peak live  peak used  footprint  over live then  over peak live  over peak used
  first fit  LIFO          14902      20624      83968          468.8%          463.5%          307.1%
  first fit  address       14902      21008      21456           44.6%           44.0%            2.1%
  next fit   LIFO          14902      20400     156608          962.1%          950.9%          667.7%
  next fit   address       14902      20384     131664          812.1%          783.5%          545.9%
  best fit   LIFO          14902      20992      21408           44.6%           43.7%            2.0%
  best fit   address       14902      20992      21408           44.6%           43.7%            2.0%
as recorded, against the allocators that don't search:
  buddy system             14902      27744      28160           91.2%           89.0%            1.5%
  TLSF                     14902      20992      21280           44.5%           42.8%            1.4%
```
]

The same 14,902 live bytes needed 21,280 bytes from TLSF and 156,608 from next fit over a LIFO list. Dealt out at random, the policies move closer together:

#output(title: "cargo xtask kernel --kernel trace --target riscv64")[
```
the same sizes and the same lifetimes, dealt out at random:
  first fit  LIFO          18528      23040      45984          148.9%          148.2%           99.6%
  first fit  address       18528      23520      25216           36.5%           36.1%            7.2%
  next fit   LIFO          18528      23088      52688          324.4%          184.4%          128.2%
as recorded, with the blocks that outlive 64 allocations in a region of their own:
  policy     order      long-lived   the rest      total   one region
  first fit  LIFO            47136       1952      49088       83968
  next fit   LIFO            30448       2688      33136      156608
  best fit   address         20352       1952      22304       21408
```
]

Separating the long-lived blocks takes first fit over LIFO from 83,968 bytes to 49,088, and next fit over LIFO from 156,608 to 33,136. For best fit by address it costs 896 bytes.

`table`, `others` and `split` print these, `describe` the summary of the trace, `measured` replays and checks a heap, and `percent` does its arithmetic in tenths, without floating point.

== Regions

`regions` runs the session's script four times with the heap recording, and compares what each way asked of the heap. There are two regions from `green-tea-region` (@crate-region), and the lifetime of each is a type. The *session's* region holds the variables, and the *command's* region holds what one command works with:

#srcfile("green-tea-shell/src/region.rs", lines: ((33, 37), (47, 53),), notes: ((34, [`'s`: a `Var` can hold only references that live as long as the session, so a command's words can't be stored here. The compiler refuses it, and `set` has to copy them.]),))[```rust
struct Var<'s> {
    name: &'s str,
    value: Cell<&'s str>,
    next: Cell<Option<&'s Var<'s>>>,
}
// …
struct Session<'s> {
    region: &'s Region,
    first: Cell<Option<&'s Var<'s>>>,
    /// The bytes the variables set now need, and the most that has been.
    live: Cell<usize>,
    most_live: Cell<usize>,
}
```]

The script with both regions is a few lines. The command's region is reset after each command:

#srcfile("green-tea-shell/src/region.rs", lines: ((366, 373),))[```rust
fn in_two_regions() -> (Ending, Regions) {
    let session_region = Region::new(CHUNK);
    let mut command_region = Region::new(CHUNK);
    let session = Session::new(&session_region);
    Shell::script(|line| {
        run(line, &session, &command_region, true);
        command_region.reset();
    });
```]

#output(title: "cargo xtask kernel --kernel region --target riscv64")[
```
the session's script, 1970 commands, run four times with the heap recording:
  a command's memory    the variables   allocations      frees  bytes asked for  peak live  footprint
  the heap              the heap              10316      10316           656120      14902      21456
  the arena             the heap               2996       2996            99209      14563      20576
  a region, reset       a region                 16         16            65536      65536      65792
  a region, new         a region               1985       1985          8130560      65536      65792
the command's region: 1970 resets, 1 chunk of 4096 bytes taken from the heap, at most 436 bytes of it in use
the session's region: 15 chunks of 4096 bytes, 57693 bytes in use at the end in 1780 blocks
the variables: 74 set at the end, in 6609 of those bytes; the most ever set at once needed 10650
PASS: 1970 commands asked the heap for 10316 blocks, 2996 with an arena, and 16 with two regions, which held 61440 bytes at the end for 6609 of variables
```
]

`regions` needs the recorder and a session with no variables, and otherwise refuses, leaving both as they were:

#output(title: "typed at the shell")[
```
> regions
`regions` runs with no variables: `unset` the ones in `vars` first
> trace on
recording the heap's allocations and frees, up to 65536 of them
> regions
`regions` needs the recorder: a recording is under way, `trace off` first
```
]

Regions asked the heap for 16 blocks where the heap alone took 10,316, and paid in memory: a peak of 65,536 live bytes against 14,902. The session's region can't free a variable, so it held 61,440 bytes at the end for 6,609 bytes of variables. `Session` keeps those counts, in `live` and `most_live`, with `get`, `set` and `unset` on its list of `Var`s. `run` is the command loop for regions. `Ending` is a hash of the final variables, and the four runs must agree on it. `Recorder`, `Asked` and `Regions` hold the recording and its results.

== Hardening

`harden` replays the recording against `green-tea-fit`'s `Hardened` allocator (@crate-fit), adding one defence each time, and then with quarantines of different lengths:

#srcfile("green-tea-shell/src/harden.rs", lines: ((78, 85),))[```rust
let rows = [
    replayed::<0>(base, events, 16, "no defence")?,
    replayed::<L1>(base, events, 16, "+ safe unlinking")?,
    replayed::<L2>(base, events, 16, "+ encoded links")?,
    replayed::<L3>(base, events, 16, "+ canary")?,
    replayed::<L4>(base, events, 16, "+ quarantine")?,
    replayed::<ALL>(base, events, 16, "+ random")?,
];
```]

#output(title: "cargo xtask kernel --kernel harden --target riscv64")[
```
  allocator          peak live  peak used  peak held  footprint  looked at  words filled
                                                                per alloc  and checked
                                                                           per free
  no defence             14902      20624          0      83968        1.6          0.0
  + safe unlinking       14902      20624          0      83968        1.6          0.0
  + encoded links        14902      20624          0      83968        1.6          0.0
  + canary               14902      22464          0      95760        1.5          0.0
  + quarantine           14902      22288       4864      99984        2.1         20.0
  + random               14902      22320       5264      30160       46.0         20.1
with a quarantine of 0, 4, 16 and 64 blocks, and no random choice:
  0 blocks               14902      22464          0      95760        1.5          0.0
  4 blocks               14902      22224       3504     112320        1.7         19.9
  16 blocks              14902      22288       4864      99984        2.1         20.0
  64 blocks              14902      22208      10096      68928        2.4         20.0
```
]

Safe unlinking and encoded links change nothing about where blocks go, and the kernel checks that they don't. Random placement *lowers* the footprint on this trace, from 99,984 to 30,160 bytes, at 46 free blocks examined per allocation against 2.1. A longer quarantine doesn't grow the footprint steadily either: 64 blocks needed less than 4. `replayed` runs one configuration, `report` checks the rows against each other, `Cost` is one row's result, and `ratio` does the division.

== Pages

`pages` shows the heap's pages as the page tables see them, and on aarch64, which has no paging, says so:

#output(title: "typed at the shell, on aarch64")[
```
aarch64 runs with the MMU off: all 1024 pages of the heap are there, always
```
]

`pages replay` measures a trace's footprint in *pages touched*. An allocator alone touches only the pages its tags are on, so `Written` wraps it and fills every block it hands out, as a program would:

#srcfile("green-tea-shell/src/pages.rs", lines: ((42, 50),))[```rust
impl<H: Heap> Heap for Written<'_, H> {
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize> {
        let addr = self
            .0
            .allocate(size, align)?;
        // SAFETY: the allocator handed out `size` bytes at `addr`.
        unsafe { core::ptr::write_bytes(addr as *mut u8, FILL, size) };
        Some(addr)
    }
```]

`measure` unmaps the region, replays the trace, counts the pages mapped, trims the free blocks and counts again. It then checks that each allocated block's first byte survived the trimming, and `live_at_the_end` gives the floor to compare with. `status` and `print` write the rows, and `Row` holds them.

#output(title: "cargo xtask kernel --kernel resident --target riscv64")[
```
the recorded trace, replayed in a region of 64 pages that starts with none mapped, every block written to:
  allocator            footprint  in pages  pages touched  after trimming
  first fit  LIFO          83968        21             22              22
  first fit  address       21456         6              7               7
  next fit   LIFO         156608        39             40              37
  next fit   address      131664        33             34              32
  best fit   LIFO          21408         6              7               6
  best fit   address       21408         6              7               6
  TLSF                     21280         6              7               7
  at the end 149 blocks are live, of 11362 bytes in all: 3 pages, if they were side by side
PASS: the recorded trace touched 7 pages under the allocator that needed fewest, and 40 under the one that needed most
```
]

Trimming saves little here. The 149 survivors are spread over the pages, and a page with one live byte on it can't be given back.

== The dump

The kernel has no disk to write a file to, so `trace dump` writes the trace to the serial console in `green-tea-fit`'s text form, for `cargo xtask trace` to keep:

#srcfile("green-tea-shell/src/dump.rs", lines: ((23, 32),))[```rust
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
```]

`trace check` proves the round trip without the serial port. `round_trip` writes the events in both forms, `CHUNK` (2,048) at a time, since a whole trace in both forms would need more of the heap than the buffer leaves, and reads each back:

#srcfile("green-tea-shell/src/dump.rs", lines: ((65, 81),))[```rust
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
```]

#output(title: "cargo xtask kernel --kernel gtt --target riscv64")[
```
PASS: 20483 events written as 278076 bytes of text and as 74087 bytes of binary, 2048 at a time, and read back the same
```
]

The binary form is 3.75 times smaller than the text.

== Limits

- Variables are a list searched from the front, and the session's region never reuses the memory of a changed or unset variable.
- A line is at most 120 bytes, and input is polled.
- `pages` on aarch64 reports only that there is no paging. The `resident` kernel doesn't run there.

One task, twenty commands, and a trace of 20,483 events. @crate-trace does the same with programs from outside the kernel.
