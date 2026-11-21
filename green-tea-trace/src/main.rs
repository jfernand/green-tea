//! `green-tea-trace`: what the shell's `replay` does, on the host, for a trace in a file.
//!
//! ```text
//! green-tea-trace stat <file>                      what the trace holds
//! green-tea-trace replay <file> [--region-kib <n>] [--shuffle [<hex seed>]]
//!                                                  what each allocator needed for it
//! green-tea-trace convert <file> <out> [--text|--binary]
//!                                                  the same records in the other form
//! ```
//!
//! A file is in either of green-tea-fit's two forms (`green_tea_fit::tracefile`); `-` is
//! the standard input. The tables are the ones the kernel prints, column for column, so
//! that a trace dumped by the kernel can be replayed here and the two outputs compared.

use green_tea_fit::buddy::{self, Buddy};
use green_tea_fit::tlsf::Tlsf;
use green_tea_fit::trace::{self, Event, Report};
use green_tea_fit::tracefile::{self, Events, MAGIC, Record};
use green_tea_fit::{Fit, Heap, Order, Policy};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::process::ExitCode;
use std::{env, fs, io};

const USAGE: &str = "\
usage: green-tea-trace stat <file>
       green-tea-trace replay <file> [--region-kib <n>] [--shuffle [<hex seed>]]
       green-tea-trace convert <file> <out> [--text|--binary]

stat:     prints what a trace file holds: its records, the sizes asked for, how long the
          blocks lived, and the most bytes the program had allocated at once.
replay:   replays the trace against first fit, next fit and best fit, each with its free
          list in LIFO and in address order, and against the buddy system and TLSF, and
          prints what each needed. --region-kib is the region each replay gets (default:
          the power of two at or above four times the trace's peak of live bytes, and at
          least 512). --shuffle also replays the trace with its sizes and lifetimes dealt
          out at random; a seed, in hexadecimal, goes after the file.
convert:  writes the trace in the other form, or in the one asked for.

A file is in the text form or the binary form; the first bytes say which. `-` is the
standard input, and for convert's <out> the standard output.";

/// The seed the shell's `replay` shuffles with: the default here, so that the two agree.
const SEED: u64 = 0x2545_f491_4f6c_dd1d;
/// A block is long-lived if it outlives more than this many later allocations, as in the shell.
const LONG_LIVED: u32 = 64;
/// The region the shell's `replay` uses, and the least this one does.
const LEAST_REGION: usize = 512 * 1024;
/// The largest region every allocator here can take: the buddy system's table of lists ends there.
const MOST_REGION: usize = buddy::MIN_BLOCK << buddy::ORDERS;

const WAYS: [(Policy, &str, Order, &str); 6] = [
    (Policy::FirstFit, "first fit", Order::Lifo, "LIFO"),
    (Policy::FirstFit, "first fit", Order::Address, "address"),
    (Policy::NextFit, "next fit", Order::Lifo, "LIFO"),
    (Policy::NextFit, "next fit", Order::Address, "address"),
    (Policy::BestFit, "best fit", Order::Lifo, "LIFO"),
    (Policy::BestFit, "best fit", Order::Address, "address"),
];

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(text) => {
            // A reader that stops early (`| head`) is not an error.
            let _ = io::stdout().write_all(text.as_bytes());
            ExitCode::SUCCESS
        }
        Err(Failure::Usage(e)) => {
            eprintln!("error: {e}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Failure::Refused { printed, why }) => {
            let _ = io::stdout().write_all(printed.as_bytes());
            eprintln!("error: {why}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, PartialEq)]
enum Failure {
    /// The command line makes no sense.
    Usage(String),
    /// The trace can't be read, or a replay of it went wrong. `printed` is what there was
    /// to print before that.
    Refused { printed: String, why: String },
}

fn refused(why: impl Into<String>) -> Failure {
    Failure::Refused { printed: String::new(), why: why.into() }
}

fn run(args: &[String]) -> Result<String, Failure> {
    let usage = |e: &str| Failure::Usage(e.into());
    let (command, rest) = args.split_first().ok_or_else(|| usage("no command"))?;
    let mut files = Vec::new();
    let (mut region, mut shuffle, mut form) = (None, None, None);
    let mut rest = rest.iter().peekable();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--region-kib" => {
                let kib = rest.next().and_then(|n| n.parse::<usize>().ok()).filter(|n| (1..=MOST_REGION / 1024).contains(n));
                let why = format!("--region-kib needs a number of KiB from 1 to {}", MOST_REGION / 1024);
                region = Some(kib.ok_or_else(|| usage(&why))? * 1024);
            }
            "--shuffle" => {
                // Before the file, the next word is the file, whatever it looks like.
                let seed = rest.next_if(|next| !files.is_empty() && u64::from_str_radix(next, 16).is_ok());
                shuffle = Some(seed.map_or(SEED, |seed| u64::from_str_radix(seed, 16).unwrap_or(SEED)));
            }
            "--text" => form = Some(Form::Text),
            "--binary" => form = Some(Form::Binary),
            "-h" | "--help" => return Ok(format!("{USAGE}\n")),
            option if option.starts_with("--") => return Err(Failure::Usage(format!("unknown option {option}"))),
            file => files.push(file),
        }
    }
    match (command.as_str(), files.as_slice()) {
        ("-h" | "--help" | "help", _) => Ok(format!("{USAGE}\n")),
        ("stat", [file]) => {
            let (bytes, records) = load(file)?;
            Ok(stat(file, &bytes, &records))
        }
        ("replay", [file]) => {
            let (_, records) = load(file)?;
            replay(&tracefile::events(&records), region, shuffle)
        }
        ("convert", [file, out]) => {
            let (bytes, records) = load(file)?;
            let form = form.unwrap_or(if Form::of(&bytes) == Form::Text { Form::Binary } else { Form::Text });
            let written = form.write(&records);
            let io = |e: io::Error| refused(format!("{out}: {e}"));
            match *out {
                "-" => io::stdout().write_all(&written).map_err(io)?,
                path => fs::write(path, &written).map_err(io)?,
            }
            Ok(String::new())
        }
        ("stat" | "replay" | "convert", _) => Err(usage("wrong number of files")),
        (other, _) => Err(Failure::Usage(format!("unknown command {other}"))),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Form {
    Text,
    Binary,
}

impl Form {
    fn of(bytes: &[u8]) -> Form {
        if bytes.starts_with(&MAGIC) { Form::Binary } else { Form::Text }
    }

    fn name(self) -> &'static str {
        match self {
            Form::Text => "text",
            Form::Binary => "binary",
        }
    }

    fn write(self, records: &[Record]) -> Vec<u8> {
        match self {
            Form::Binary => tracefile::write_binary(records),
            Form::Text => {
                let mut text = String::new();
                tracefile::write_text(records, &mut text).expect("writing to a String can't fail");
                text.into_bytes()
            }
        }
    }
}

/// Read a trace file, in either form.
fn load(file: &str) -> Result<(Vec<u8>, Vec<Record>), Failure> {
    let mut bytes = Vec::new();
    let read = match file {
        "-" => io::stdin().read_to_end(&mut bytes).map(|_| ()),
        path => fs::read(path).map(|read| bytes = read),
    };
    read.map_err(|e| refused(format!("{file}: {e}")))?;
    let records = tracefile::read(&bytes).map_err(|e| {
        let unit = if Form::of(&bytes) == Form::Text { "line" } else { "byte" };
        refused(format!("{file}: {}", e.to_string().replacen("at ", &format!("at {unit} "), 1)))
    })?;
    Ok((bytes, records))
}

/// `part` as a percentage of `whole`, to one decimal place.
fn percent(part: usize, whole: usize) -> String {
    let tenths = (part * 1000 + whole / 2) / whole.max(1);
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// The most bytes the trace has allocated at once.
fn peak_live(events: &[Event]) -> usize {
    let mut blocks = BTreeMap::new();
    let (mut live, mut peak) = (0usize, 0);
    for event in events {
        if event.is_free() {
            live -= blocks.remove(&event.addr).unwrap_or(0);
        } else {
            live += event.size as usize;
            live -= blocks.insert(event.addr, event.size as usize).unwrap_or(0);
            peak = peak.max(live);
        }
    }
    peak
}

/// `stat`: the file, then the three lines the shell's `replay` starts with, then what the
/// kernel's traces never have.
fn stat(file: &str, bytes: &[u8], records: &[Record]) -> String {
    let mut out = String::new();
    let count = |kind: fn(&Record) -> bool| records.iter().filter(|record| kind(record)).count();
    let _ = writeln!(
        out,
        "{file}: the {} form, {} bytes; {} records: {} allocated, {} freed, {} reallocated",
        Form::of(bytes).name(),
        bytes.len(),
        records.len(),
        count(|r| matches!(r, Record::Allocate { .. })),
        count(|r| matches!(r, Record::Free { .. })),
        count(|r| matches!(r, Record::Reallocate { .. })),
    );
    let made = tracefile::events(records);
    describe(&made.events, &mut out);
    let _ = writeln!(
        out,
        "reallocations: {}, of which {} left the block where it was; {} allocations of no bytes; {} of 4 GiB or more",
        made.reallocations, made.in_place, made.empty, made.too_large
    );
    let (unmatched, twice) = mismatches(&made.events);
    let _ = writeln!(
        out,
        "live bytes: {} at the peak; {unmatched} frees are of blocks the trace never allocated; {twice} allocations \
         are at an address that is still allocated",
        peak_live(&made.events)
    );
    out
}

/// What is wrong with the order of a trace: how many frees are of an address that isn't
/// allocated, and how many allocations are at an address that still is. The first happens
/// when the recording starts after the program does. The second can't happen in a trace
/// whose records are in the order of the calls.
fn mismatches(events: &[Event]) -> (usize, usize) {
    let mut live = BTreeMap::new();
    let (mut unmatched, mut twice) = (0, 0);
    for event in events {
        match event.is_free() {
            true => unmatched += live.remove(&event.addr).is_none() as usize,
            false => twice += live.insert(event.addr, ()).is_some() as usize,
        }
    }
    (unmatched, twice)
}

/// What the trace holds, as the shell prints it. Returns how many allocations that is.
fn describe(events: &[Event], out: &mut String) -> usize {
    let mut sizes = BTreeMap::new();
    for event in events.iter().filter(|e| !e.is_free()) {
        *sizes.entry(event.size).or_insert(0usize) += 1;
    }
    let allocations: usize = sizes.values().sum();
    let bytes: usize = sizes.iter().map(|(size, count)| *size as usize * count).sum();
    let _ = writeln!(
        out,
        "the trace: {} events, {allocations} allocations of {bytes} bytes in all, {} frees",
        events.len(),
        events.len() - allocations
    );
    let mut common: Vec<(usize, u32)> = sizes.iter().map(|(size, count)| (*count, *size)).collect();
    common.sort_unstable_by(|a, b| b.cmp(a));
    let mut line = String::new();
    for (count, size) in common.iter().take(6) {
        let _ = write!(line, " {size} ({})", percent(*count, allocations));
    }
    let _ = writeln!(
        out,
        "sizes: {} different ones, from {} to {} bytes; the most common:{line}",
        sizes.len(),
        sizes.keys().next().copied().unwrap_or(0),
        sizes.keys().last().copied().unwrap_or(0)
    );
    let lifetimes = trace::lifetimes(events);
    let within = |limit: u32| lifetimes.iter().filter(|life| life.is_some_and(|life| life <= limit)).count();
    let _ = writeln!(
        out,
        "lifetimes: {} of the blocks are freed before 8 more are allocated, {} before {LONG_LIVED} more; {} are \
         never freed",
        percent(within(7), allocations),
        percent(within(LONG_LIVED - 1), allocations),
        lifetimes.iter().filter(|life| life.is_none()).count()
    );
    allocations
}

/// `replay`: the trace against every allocator, in a region of `region` bytes each.
fn replay(made: &Events, region: Option<usize>, shuffle: Option<u64>) -> Result<String, Failure> {
    let events = &made.events;
    let mut out = String::new();
    let allocations = describe(events, &mut out);
    let fail = |out: &String, why: &str| Failure::Refused { printed: out.clone(), why: why.into() };
    let (_, twice) = mismatches(events);
    if twice != 0 {
        let why = format!(
            "{twice} allocations are at an address that is still allocated: the trace's records are out of order"
        );
        return Err(fail(&out, &why));
    }
    let region = region.unwrap_or_else(|| (4 * peak_live(events)).next_power_of_two().max(LEAST_REGION));
    let mut memory = vec![0u128; region / 16];
    let base = memory.as_mut_ptr() as usize;

    let _ = writeln!(out, "replayed as recorded, each in a region of {} KiB:", region / 1024);
    let recorded = table(events, base, region, &mut out).map_err(|e| fail(&out, e))?;
    let _ = writeln!(out, "as recorded, against the allocators that don't search:");
    let others = others(events, base, region, &mut out).map_err(|e| fail(&out, e))?;
    if recorded.iter().chain(&others).any(|r| r.allocations != allocations || r.peak_live != recorded[0].peak_live) {
        return Err(fail(&out, "a replay didn't make every allocation of the trace"));
    }
    if let Some(seed) = shuffle {
        let _ = writeln!(out, "the same sizes and the same lifetimes, dealt out at random:");
        table(&trace::shuffled(events, seed), base, region, &mut out).map_err(|e| fail(&out, e))?;
    }
    Ok(out)
}

fn row(name: &str, r: &Report, out: &mut String) -> Result<(), &'static str> {
    let _ = writeln!(
        out,
        "  {name:19} {:10} {:10} {:10} {:>15} {:>15} {:>15}",
        r.peak_live,
        r.peak_used,
        r.footprint,
        percent(r.footprint - r.live_at_peak_footprint, r.live_at_peak_footprint),
        percent(r.footprint - r.peak_live, r.peak_live),
        percent(r.footprint - r.peak_used, r.peak_used),
    );
    if r.failed != 0 {
        return Err("a trace didn't fit in the region it was replayed in: give it a larger one with --region-kib");
    }
    if r.peak_live > r.peak_used || r.peak_used > r.footprint {
        return Err("a replay used less memory than the program asked for");
    }
    Ok(())
}

/// Replay `events` each of the six ways in the region at `base`, and print a row for each.
/// The last three columns are the footprint's excess over the live bytes when it last
/// grew, over the most live bytes there ever were, and over the most bytes ever in blocks.
fn table(events: &[Event], base: usize, region: usize, out: &mut String) -> Result<Vec<Report>, &'static str> {
    let _ = writeln!(
        out,
        "  policy     order     peak live  peak used  footprint  over live then  over peak live  over peak used"
    );
    let mut reports: Vec<Report> = Vec::with_capacity(WAYS.len());
    for (policy, policy_name, order, order_name) in WAYS {
        // SAFETY: the region is ours, and this is the only allocator using it.
        let mut fit = unsafe { Fit::new(base, region, policy, order) };
        let r = trace::replay(events, &mut fit);
        fit.check()?;
        row(&format!("{policy_name:10} {order_name}"), &r, out)?;
        if reports.first().is_some_and(|first| {
            (r.peak_live, r.allocations, r.left) != (first.peak_live, first.allocations, first.left)
        }) {
            return Err("two replays of one trace disagree about what the program asked for");
        }
        reports.push(r);
    }
    Ok(reports)
}

/// Replay `events` against the buddy system and against TLSF, and print a row for each.
fn others(events: &[Event], base: usize, region: usize, out: &mut String) -> Result<Vec<Report>, &'static str> {
    let _ = writeln!(
        out,
        "  allocator            peak live  peak used  footprint  over live then  over peak live  over peak used"
    );
    let mut reports = Vec::with_capacity(2);
    for name in ["buddy system", "TLSF"] {
        // SAFETY: the region is ours, and one allocator uses it at a time.
        let r = unsafe {
            if name == "TLSF" {
                measured(events, &mut Tlsf::new(base, region))
            } else {
                measured(events, &mut Buddy::new(base, region))
            }
        }?;
        row(name, &r, out)?;
        reports.push(r);
    }
    Ok(reports)
}

/// Replay `events` against `heap`, and check the heap afterwards.
fn measured(events: &[Event], heap: &mut impl Heap) -> Result<Report, &'static str> {
    let report = trace::replay(events, heap);
    heap.check()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::{Failure, Form, describe, mismatches, peak_live, percent, replay, run, stat};
    use green_tea_fit::trace::Event;
    use green_tea_fit::tracefile::{self, Record};

    /// a = 100, b = 50, b grows to 80 somewhere else, free a, c = 0, free c.
    const SMALL: [Record; 6] = [
        Record::Allocate { addr: 0xa0, size: 100, align: 16 },
        Record::Allocate { addr: 0xb0, size: 50, align: 16 },
        Record::Reallocate { old: 0xb0, new: 0xd0, size: 80, align: 16 },
        Record::Free { addr: 0xa0 },
        Record::Allocate { addr: 0xc0, size: 0, align: 16 },
        Record::Free { addr: 0xc0 },
    ];

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn stat_counts_records_and_events() {
        let bytes = Form::Text.write(&SMALL);
        let out = stat("small.gtt", &bytes, &SMALL);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            format!("small.gtt: the text form, {} bytes; 6 records: 3 allocated, 2 freed, 1 reallocated", bytes.len())
        );
        // The reallocation is a free and an allocation, and the empty block is one byte.
        assert_eq!(lines[1], "the trace: 7 events, 4 allocations of 231 bytes in all, 3 frees");
        assert!(lines[2].starts_with("sizes: 4 different ones, from 1 to 100 bytes;"));
        assert!(lines[3].ends_with("1 are never freed"));
        assert_eq!(
            lines[4],
            "reallocations: 1, of which 0 left the block where it was; 1 allocations of no bytes; 0 of 4 GiB or more"
        );
        assert_eq!(
            lines[5],
            "live bytes: 180 at the peak; 0 frees are of blocks the trace never allocated; 0 allocations are at an \
             address that is still allocated"
        );
    }

    /// A block with `later` allocations between its allocation and its free.
    fn lived(later: usize) -> Vec<Event> {
        let mut events = vec![Event::allocate(1, 8, 8)];
        events.extend((0..later).map(|n| Event::allocate(16 * (n + 2), 8, 8)));
        events.push(Event::free(1));
        events
    }

    #[test]
    fn before_n_more_means_fewer_than_n() {
        let line = |later| {
            let mut out = String::new();
            describe(&lived(later), &mut out);
            out.lines().find(|l| l.starts_with("lifetimes:")).unwrap().to_string()
        };
        // One block of 64 freed after 63 others: counted. After 64: it wasn't freed before 64 more.
        assert!(line(63).contains(&format!("{} before 64 more", percent(1, 64))), "{}", line(63));
        assert!(line(64).contains(&format!("{} before 64 more", percent(0, 65))), "{}", line(64));
    }

    #[test]
    fn the_peak_is_of_bytes_live_at_once() {
        let events = [Event::allocate(1, 10, 8), Event::allocate(2, 20, 8), Event::free(1), Event::allocate(3, 5, 8)];
        assert_eq!(peak_live(&events), 30);
        // A free of a block the trace never allocated changes nothing.
        assert_eq!(peak_live(&[Event::free(9), Event::allocate(1, 10, 8)]), 10);
    }

    #[test]
    fn percentages_round_to_a_tenth() {
        assert_eq!(percent(1, 3), "33.3%");
        assert_eq!(percent(2, 3), "66.7%");
        assert_eq!(percent(448, 21008), "2.1%");
    }

    #[test]
    fn a_replay_has_a_row_for_each_allocator() {
        let out = replay(&tracefile::events(&SMALL), None, Some(1)).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[3], "replayed as recorded, each in a region of 512 KiB:");
        // 100 and 50 bytes are blocks of 128 and 80. The 50 are freed before the 80 that replace them
        // are allocated, so the block of 96 that those need starts where the block of 80 was.
        assert_eq!(
            lines[6],
            "  first fit  address         180        224        224           24.4%           24.4%            0.0%"
        );
        assert_eq!(
            lines.iter().filter(|line| line.starts_with("  first fit") || line.starts_with("  best fit")).count(),
            8
        );
        assert!(lines.iter().any(|line| line.starts_with("  buddy system")));
        assert!(lines.iter().any(|line| line.starts_with("  TLSF")));
        assert_eq!(lines.len(), 3 + 1 + 7 + 4 + 8);
    }

    #[test]
    fn a_trace_out_of_order_is_not_replayed() {
        // Two threads: one frees a block, the other is given its address, and the allocation is written first.
        let records = [
            Record::Allocate { addr: 0xa0, size: 10, align: 16 },
            Record::Allocate { addr: 0xa0, size: 20, align: 16 },
            Record::Free { addr: 0xa0 },
            Record::Free { addr: 0xb0 },
        ];
        assert_eq!(mismatches(&tracefile::events(&records).events), (1, 1));
        assert!(stat("x", b"", &records).contains("1 allocations are at an address that is still allocated"));
        let Err(Failure::Refused { why, .. }) = replay(&tracefile::events(&records), None, None) else {
            panic!("a trace that allocates an address twice was replayed");
        };
        assert!(why.contains("out of order"), "{why}");
    }

    #[test]
    fn a_region_too_small_for_the_trace_is_an_error() {
        let big = [Record::Allocate { addr: 0xa0, size: 3000, align: 16 }];
        let Err(Failure::Refused { printed, why }) = replay(&tracefile::events(&big), Some(2048), None) else {
            panic!("a 3000-byte block was replayed in 2 KiB");
        };
        assert!(why.contains("--region-kib"), "{why}");
        assert!(printed.contains("each in a region of 2 KiB"));
    }

    #[test]
    fn the_command_line_is_checked() {
        assert!(matches!(run(&args(&[])), Err(Failure::Usage(_))));
        assert!(matches!(run(&args(&["replay"])), Err(Failure::Usage(_))));
        assert!(matches!(run(&args(&["replay", "a", "b"])), Err(Failure::Usage(_))));
        assert!(matches!(run(&args(&["frobnicate", "a"])), Err(Failure::Usage(_))));
        assert!(matches!(run(&args(&["replay", "a", "--region-kib", "x"])), Err(Failure::Usage(_))));
        assert!(matches!(run(&args(&["replay", "a", "--frobnicate"])), Err(Failure::Usage(_))));
        assert!(run(&args(&["--help"])).unwrap().starts_with("usage:"));
        assert!(matches!(run(&args(&["stat", "/nonexistent/trace.gtt"])), Err(Failure::Refused { .. })));
    }

    #[test]
    fn a_region_too_large_is_a_usage_error() {
        let path = std::env::temp_dir().join(format!("green-tea-trace-region-{}.gtt", std::process::id()));
        std::fs::write(&path, Form::Text.write(&SMALL)).unwrap();
        let path = path.to_str().unwrap();
        let outcome = std::panic::catch_unwind(|| run(&args(&["replay", path, "--region-kib", "5000000"])));
        let huge = run(&args(&["replay", path, "--region-kib", "18446744073709551615"]));
        std::fs::remove_file(path).unwrap();
        assert!(matches!(outcome, Ok(Err(Failure::Usage(_)))), "{outcome:?}");
        assert!(matches!(huge, Err(Failure::Usage(_))), "{huge:?}");
    }

    #[test]
    fn a_file_named_in_hex_can_follow_shuffle() {
        // The word after --shuffle is the file when no file has been given: here, one that isn't there.
        let Err(Failure::Refused { why, .. }) = run(&args(&["replay", "--shuffle", "cafe"])) else {
            panic!("`cafe` was not taken for the file");
        };
        assert!(why.starts_with("cafe:"), "{why}");
        // After the file, it is the seed.
        let Err(Failure::Refused { why, .. }) = run(&args(&["replay", "/nonexistent/x", "--shuffle", "cafe"])) else {
            panic!("`cafe` was not taken for the seed");
        };
        assert!(why.starts_with("/nonexistent/x:"), "{why}");
        assert!(super::USAGE.contains("<hex seed>"));
    }

    #[test]
    fn a_file_is_converted_and_reads_the_same() {
        let dir = std::env::temp_dir().join(format!("green-tea-trace-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name).to_str().unwrap().to_string();
        std::fs::write(path("small.gtt"), Form::Text.write(&SMALL)).unwrap();
        run(&args(&["convert", &path("small.gtt"), &path("small.gttb")])).unwrap();
        let binary = std::fs::read(path("small.gttb")).unwrap();
        assert_eq!(Form::of(&binary), Form::Binary);
        assert_eq!(tracefile::read(&binary).unwrap(), SMALL);
        run(&args(&["convert", &path("small.gttb"), &path("back.gtt")])).unwrap();
        assert_eq!(std::fs::read(path("back.gtt")).unwrap(), std::fs::read(path("small.gtt")).unwrap());
        // The same replay from either form.
        let from_text = run(&args(&["replay", &path("small.gtt")])).unwrap();
        assert_eq!(run(&args(&["replay", &path("small.gttb")])).unwrap(), from_text);
        // A file with a line removed is refused, and the error says where.
        let text = String::from_utf8(Form::Text.write(&SMALL)).unwrap().replace("f a0\n", "");
        std::fs::write(path("short.gtt"), text).unwrap();
        let Err(Failure::Refused { why, .. }) = run(&args(&["replay", &path("short.gtt")])) else {
            panic!("a trace with a line missing was replayed");
        };
        assert!(why.ends_with("at line 7: `end` says 6 records, and there are 5"), "{why}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
