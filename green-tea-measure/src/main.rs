//! `green-tea-measure`: one recorded trace against every single-threaded allocator of
//! the workspace.
//!
//! ```text
//! green-tea-measure space <trace>          bytes and searches: the same on every run
//! green-tea-measure instructions <trace>   instructions for each event, counted by callgrind
//! green-tea-measure time <trace>           nanoseconds for each event, as a distribution
//! green-tea-measure runs <trace> <a> <b>   two allocators, many runs of each
//! green-tea-measure lisp lifetimes         the Lisp's programs, freed explicitly
//! green-tea-measure lisp instructions      and against its garbage-collected heaps
//! ```
//!
//! `--help` has the options. Every table starts with what it was run on, and a command
//! that checks something ends with a line that starts `PASS: ` or `FAIL: `.

use green_tea_fit::trace::{self, Event};
use green_tea_fit::tracefile;
use green_tea_lisp::gc::Collect;
use green_tea_lisp::mark::{MarkHeap, Reclaim};
use green_tea_lisp::programs::{PROGRAMS, Program};
use green_tea_lisp::rc::{Cycles, Freeing, RcHeap};
use green_tea_lisp::semispace::Semispace;
use green_tea_lisp::{Heap, Lisp};
use green_tea_measure::oracle::{Explicit, Lifetimes, Profile};
use green_tea_measure::replay::{self, Ops, Table};
use green_tea_measure::stats::{Summary, wins};
use green_tea_measure::subjects::{self, NAMES, Subject};
use std::fmt::Write as _;
use std::io::Read as _;
use std::process::{Command, ExitCode};
use std::{env, fs};

const USAGE: &str = "\
usage: green-tea-measure space <trace> [options]
       green-tea-measure instructions <trace> [options]
       green-tea-measure time <trace> [--passes <n>] [--no-warmup] [--touch] [options]
       green-tea-measure runs <trace> <allocator> <allocator> [--runs <n>] [options]
       green-tea-measure count <trace> <allocator> [options]
       green-tea-measure lisp lifetimes
       green-tea-measure lisp instructions [--multiples <a,b,...>]
       green-tea-measure lisp run <program> <heap> [<words>]

space:         replays the trace against each allocator and prints the bytes it needed
               and the free blocks it searched. The same on every run.
instructions:  runs `count` under valgrind's callgrind for each allocator, and prints the
               instructions for each event, less those of the replay itself.
time:          replays the trace --passes times (default 20) with the clock read around
               every event, and as many times with the clock read around the whole pass.
               Prints the mean beside the percentiles. One pass is made first and thrown
               away, unless --no-warmup. With --touch, every block is written to when it
               is allocated, as the program that asked for it would. FAIL if a timed pass
               mapped a page.
runs:          --runs runs (default 30) of a whole pass for each of two allocators, in turn.
count:         replays the trace against one allocator and prints nothing but a count:
               the loop that `instructions` counts.
lisp:          the programs of green-tea-lisp with explicit frees placed by an oracle,
               against the same programs on the garbage-collected heaps, in instructions,
               at heap sizes that are multiples of what the explicit allocator needed.

options:
  --region-kib <n>   the region each allocator gets (default: the power of two at or
                     above four times the trace's peak of live bytes, at least 512)
  --shuffle          replay the trace with its sizes and lifetimes dealt out at random
  --only <a,b,...>   these allocators only
  --pins <file>      refuse a trace whose checksum isn't the one this file gives for its
                     name. A line of the file is: <name> <records> <checksum>

allocators: bump first-lifo first-addr next-lifo next-addr best-lifo best-addr buddy
            tlsf hardened pools slabs caches shards null
heaps:      floor explicit:<allocator> counting mark-sweep mark-compact copying";

/// The seed `--shuffle` uses: the shell's, and green-tea-trace's.
const SEED: u64 = 0x2545_f491_4f6c_dd1d;
const LEAST_REGION: usize = 512 * 1024;
const MARK_STACK: usize = 1024;
/// The region an allocator gets under a Lisp program, which is far more than any needs.
const LISP_REGION: usize = 1 << 20;
/// A heap so large that no program here makes it collect, in words.
const NEVER: usize = 1 << 22;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(passed) => {
            if passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(Failure::Usage(why)) => {
            eprintln!("error: {why}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Failure::Other(why)) => {
            eprintln!("error: {why}");
            ExitCode::from(2)
        }
    }
}

/// Why a command didn't finish. Only a mistake on the command line is answered with the usage.
enum Failure {
    Usage(String),
    Other(String),
}

impl From<String> for Failure {
    fn from(why: String) -> Failure {
        Failure::Other(why)
    }
}

fn usage(why: impl Into<String>) -> Failure {
    Failure::Usage(why.into())
}

#[derive(Default)]
struct Options {
    region: Option<usize>,
    shuffle: bool,
    only: Option<Vec<String>>,
    pins: Option<String>,
    passes: Option<usize>,
    runs: Option<usize>,
    no_warmup: bool,
    touch: bool,
    multiples: Option<Vec<usize>>,
}

fn run(args: &[String]) -> Result<bool, Failure> {
    let mut words = Vec::new();
    let mut options = Options::default();
    let mut rest = args.iter();
    let number = |value: Option<&String>, what: &str| {
        value.and_then(|n| n.parse::<usize>().ok()).filter(|n| *n >= 1).ok_or_else(|| usage(format!("{what} needs a number")))
    };
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--region-kib" => options.region = Some(number(rest.next(), "--region-kib")? * 1024),
            "--passes" => options.passes = Some(number(rest.next(), "--passes")?),
            "--runs" => options.runs = Some(number(rest.next(), "--runs")?),
            "--shuffle" => options.shuffle = true,
            "--no-warmup" => options.no_warmup = true,
            "--touch" => options.touch = true,
            "--only" => {
                let names = rest.next().ok_or_else(|| usage("--only needs a list of allocators"))?;
                options.only = Some(names.split(',').map(String::from).collect());
            }
            "--multiples" => {
                let list = rest.next().ok_or_else(|| usage("--multiples needs a list"))?;
                let tenths = |m: &str| m.parse::<f64>().ok().map(|m| (m * 10.0).round() as usize).filter(|m| *m >= 1);
                let parsed: Option<Vec<usize>> = list.split(',').map(tenths).collect();
                options.multiples = Some(parsed.ok_or_else(|| usage("--multiples needs numbers, such as 1,1.5,2"))?);
            }
            "--pins" => options.pins = Some(rest.next().ok_or_else(|| usage("--pins needs a file"))?.clone()),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(true);
            }
            option if option.starts_with("--") => return Err(usage(format!("unknown option {option}"))),
            word => words.push(word),
        }
    }
    Ok(match words.as_slice() {
        ["space", file] => space(&Loaded::new(file, &options)?, &options)?,
        ["instructions", file] => instructions(file, &Loaded::new(file, &options)?, &options)?,
        ["time", file] => time(&Loaded::new(file, &options)?, &options)?,
        ["runs", file, a, b] => runs(&Loaded::new(file, &options)?, a, b, &options)?,
        ["count", file, name] => count(&Loaded::new(file, &options)?, name)?,
        ["lisp", "lifetimes"] => lisp_lifetimes()?,
        ["lisp", "instructions"] => lisp_instructions(&options)?,
        ["lisp", "run", program, heap] => lisp_run(program, heap, 0)?,
        ["lisp", "run", program, heap, words] => {
            lisp_run(program, heap, words.parse().map_err(|_| usage("the heap's size is a number of words"))?)?
        }
        [] => return Err(usage("no command")),
        _ => return Err(usage("that isn't a command, or it has the wrong number of arguments")),
    })
}

/// A trace, read from a file and made ready.
struct Loaded {
    ops: Ops,
    region: usize,
    /// What the trace is, in a line.
    header: String,
}

impl Loaded {
    fn new(file: &str, options: &Options) -> Result<Loaded, String> {
        let bytes = fs::read(file).map_err(|e| format!("{file}: {e}"))?;
        let records = tracefile::read(&bytes).map_err(|e| format!("{file}: {e}"))?;
        let checksum = tracefile::checksum(&records);
        let name = file.rsplit('/').next().unwrap_or(file);
        let name = name.strip_suffix(".gtt").unwrap_or(name).to_string();
        if let Some(pins) = &options.pins {
            pinned(pins, &name, records.len(), checksum)?;
        }
        let mut events: Vec<Event> = tracefile::events(&records).events;
        if options.shuffle {
            events = trace::shuffled(&events, SEED);
        }
        let ops = Ops::new(&events);
        let region = options.region.unwrap_or_else(|| (4 * ops.peak_live).next_power_of_two().max(LEAST_REGION));
        let header = format!(
            "{name}{}: {} events, {} allocations of {} bytes in all, {} bytes live at the peak; checksum {checksum:016x}",
            if options.shuffle { ", shuffled" } else { "" },
            ops.len(),
            ops.allocations,
            ops.total,
            ops.peak_live,
        );
        Ok(Loaded { ops, region, header })
    }

    fn subject(&self, name: &str) -> Result<Box<dyn Subject>, String> {
        subjects::build(name, self.region, self.ops.total).ok_or(format!("no allocator is called {name}"))
    }
}

/// Check a trace against the line for its name in the file of pins.
fn pinned(pins: &str, name: &str, records: usize, checksum: u64) -> Result<(), String> {
    let text = fs::read_to_string(pins).map_err(|e| format!("{pins}: {e}"))?;
    let line =
        text.lines().map(|line| line.split_whitespace().collect::<Vec<_>>()).find(|line| line.first() == Some(&name));
    let Some(&[_, pinned_records, pinned_checksum]) = line.as_deref() else {
        return Err(format!("{pins} has no line for the trace {name}"));
    };
    if pinned_records != records.to_string() || pinned_checksum != format!("{checksum:016x}") {
        println!(
            "FAIL: {name} is not the trace that was measured: {pins} pins {pinned_records} records with checksum \
             {pinned_checksum}, and this file has {records} with {checksum:016x}"
        );
        return Err(format!("{name} is not the pinned trace"));
    }
    Ok(())
}

/// The allocators a command runs: all of them but `null`, or those of `--only`.
fn chosen(options: &Options) -> Vec<String> {
    match &options.only {
        Some(only) => only.clone(),
        None => NAMES.iter().filter(|name| **name != "null").map(|name| name.to_string()).collect(),
    }
}

/// `part` as a percentage of `whole`, to one decimal place.
fn percent(part: usize, whole: usize) -> String {
    let tenths = (part * 1000 + whole / 2) / whole.max(1);
    format!("{}.{}%", tenths / 10, tenths % 10)
}

fn space(loaded: &Loaded, options: &Options) -> Result<bool, String> {
    println!("{}", loaded.header);
    println!("each allocator in regions of {} KiB:", loaded.region / 1024);
    println!("  allocator    peak used  footprint  over peak live  over peak used  blocks searched  per event  most");
    let mut passed = true;
    for name in chosen(options) {
        let mut subject = loaded.subject(&name)?;
        let space = replay::space(&loaded.ops, &mut *subject).map_err(|e| format!("{name}: {e}"))?;
        if space.refused != 0 {
            println!("  {name:10}   refused an allocation: the trace doesn't fit in its region");
            passed = false;
            continue;
        }
        let mut line = format!("  {name:10} ");
        match space.peak_used {
            Some(used) => write!(line, "{used:11}").unwrap(),
            None => line.push_str("          -"),
        }
        write!(line, " {:10} {:>15}", space.footprint, percent(space.footprint - space.peak_live, space.peak_live))
            .unwrap();
        match space.peak_used {
            Some(used) if used <= space.footprint => {
                write!(line, " {:>15}", percent(space.footprint - used, used)).unwrap()
            }
            _ => line.push_str("               -"),
        }
        if let Some((searched, most)) = space.searched {
            let hundredths = (searched * 100 + loaded.ops.len() / 2) / loaded.ops.len();
            write!(line, " {searched:16} {:7}.{:02} {most:5}", hundredths / 100, hundredths % 100).unwrap();
        }
        println!("{line}");
    }
    Ok(passed)
}

/// The loop that callgrind counts. Its name isn't mangled, so that `--toggle-collect` can name it.
#[unsafe(no_mangle)]
#[inline(never)]
fn green_tea_measure_counted(ops: &Ops, table: &mut Table, subject: &mut Box<dyn Subject>) -> bool {
    replay::plain(ops, table, &mut **subject, 1)
}

fn count(loaded: &Loaded, name: &str) -> Result<bool, String> {
    let mut subject = loaded.subject(name)?;
    let mut table = Table::new(&loaded.ops);
    if !green_tea_measure_counted(&loaded.ops, &mut table, &mut subject) {
        // On standard output, with exit status 1: what `callgrind` looks for.
        println!("{name} refused an allocation");
        return Ok(false);
    }
    println!("{name}: {} events replayed", loaded.ops.len());
    Ok(true)
}

/// Why callgrind gave no count.
enum Uncounted {
    /// The program ran and said it had no room: an allocator refused, or a heap was full.
    NoRoom,
    /// Anything else: valgrind failed, or the program did.
    Failed(String),
}

impl From<Uncounted> for String {
    fn from(why: Uncounted) -> String {
        match why {
            Uncounted::NoRoom => "the program ran out of room under valgrind".into(),
            Uncounted::Failed(why) => why,
        }
    }
}

/// Run this program again under callgrind with `args`, and return the instructions it
/// counted inside `function`.
fn callgrind(function: &str, args: &[&str]) -> Result<u64, Uncounted> {
    callgrind_with_output(function, args).map(|(counted, _)| counted)
}

/// [`callgrind`], and what the program printed.
fn callgrind_with_output(function: &str, args: &[&str]) -> Result<(u64, String), Uncounted> {
    let exe = env::current_exe().map_err(|e| Uncounted::Failed(e.to_string()))?;
    let output = Command::new("valgrind")
        .args(["--tool=callgrind", "--callgrind-out-file=/dev/null"])
        .arg(format!("--toggle-collect={function}"))
        .arg(exe)
        .args(args)
        .output()
        .map_err(|e| Uncounted::Failed(format!("valgrind: {e}")))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        if output.status.code() == Some(1) && (stdout.contains("refused an allocation") || stdout.contains("HeapFull")) {
            return Err(Uncounted::NoRoom);
        }
        return Err(Uncounted::Failed(format!("under valgrind, `{}` failed: {stdout}{stderr}", args.join(" "))));
    }
    let collected = stderr.lines().find_map(|line| line.split("Collected :").nth(1));
    let counted = collected
        .and_then(|n| n.trim().parse().ok())
        .ok_or_else(|| Uncounted::Failed(format!("valgrind printed no count: {stderr}")))?;
    Ok((counted, stdout.into_owned()))
}

fn instructions(file: &str, loaded: &Loaded, options: &Options) -> Result<bool, String> {
    println!("{}", loaded.header);
    let mut flags = vec!["--region-kib".to_string(), (loaded.region / 1024).to_string()];
    if options.shuffle {
        flags.push("--shuffle".into());
    }
    let counted = |name: &str| {
        let mut args = vec!["count", file, name];
        args.extend(flags.iter().map(String::as_str));
        callgrind("green_tea_measure_counted", &args)
    };
    let null = counted("null")?;
    let again = counted("null")?;
    let events = loaded.ops.len() as u64;
    println!(
        "instructions in the replay loop, counted by callgrind; with no allocator the loop is {null}, {} for each event",
        hundredths(null, events)
    );
    println!("  allocator    instructions  less the loop's  per event");
    for name in chosen(options) {
        let all = match counted(&name) {
            Ok(all) => all,
            Err(Uncounted::NoRoom) => {
                println!("  {name:10}   refused an allocation: the trace doesn't fit in its region");
                continue;
            }
            Err(why) => return Err(why.into()),
        };
        println!("  {name:10} {all:14} {:16} {:>10}", all - null, hundredths(all - null, events));
    }
    if again == null {
        println!("PASS: the loop with no allocator was counted twice, and was {null} instructions both times");
    } else {
        println!("FAIL: the loop with no allocator was {null} instructions, and then {again}");
    }
    Ok(again == null)
}

/// `part / whole`, to two decimal places.
fn hundredths(part: u64, whole: u64) -> String {
    let h = (part as u128 * 100 + whole as u128 / 2) / whole.max(1) as u128;
    format!("{}.{:02}", h / 100, h % 100)
}

/// The machine's load averages over one, five and fifteen minutes, as Linux gives them.
fn load() -> String {
    let text = fs::read_to_string("/proc/loadavg").unwrap_or_default();
    text.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}

/// The pages this process has had mapped at a fault so far: the minor and the major
/// faults of `/proc/self/stat`. It reads into an array on the stack, so that counting
/// faults doesn't allocate, and so cause one.
fn faults() -> u64 {
    let mut buffer = [0u8; 1024];
    let read = fs::File::open("/proc/self/stat").and_then(|mut file| file.read(&mut buffer)).unwrap_or(0);
    let text = core::str::from_utf8(&buffer[..read]).unwrap_or("");
    // The fields after the program's name, which is in brackets and may hold spaces.
    let mut fields = text.rsplit(')').next().unwrap_or("").split_whitespace();
    let minor = fields.nth(7).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    let major = fields.nth(1).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    minor + major
}

fn time(loaded: &Loaded, options: &Options) -> Result<bool, String> {
    let passes = options.passes.unwrap_or(20);
    let ops = &loaded.ops;
    println!("{}", loaded.header);
    println!(
        "{passes} passes with the clock read around every event, and {passes} with it read around the pass; {}",
        if options.no_warmup { "no pass thrown away first" } else { "one pass made first and thrown away" }
    );
    if options.touch {
        println!(
            "every block is written to when it is allocated: its first byte, its last, and one in each page between"
        );
    }
    println!("the machine's load average when this began: {}", load());
    let mut clock = replay::clock(100_000);
    let clock = Summary::of(&mut clock);
    println!(
        "reading the clock twice with nothing between: median {} ns, 99% {} ns, 99.9% {} ns, most {} ns",
        clock.median, clock.p99, clock.p999, clock.max
    );
    println!("nanoseconds for each event:");
    println!(
        "  allocator       mean  median     90%     99%   99.9%        most    whole pass: least  median    most  \
         pages mapped"
    );
    let mut table = Table::new(ops);
    let mut samples: Vec<u32> = Vec::with_capacity(ops.len() * passes);
    let mut whole: Vec<u64> = Vec::with_capacity(passes);
    // Written now, because the first write to it is inside a timed pass, and can be to a new page.
    whole.resize(passes, 1);
    let mut mapped = 0;
    let _ = faults(); // the first read of the file maps what reading it needs
    for name in chosen(options) {
        if options.touch && name == "null" {
            return Err("`null` hands out no memory to write to".into());
        }
        let mut subject = loaded.subject(&name)?;
        samples.clear();
        if !options.no_warmup {
            // The warm-up: write to every page that a timed pass will write to. A pass
            // does that for the allocator's regions and the table, and the samples are
            // filled once by hand.
            samples.resize(ops.len() * passes, 1);
            samples.clear();
            if !replay::each(ops, &mut table, &mut *subject, &mut samples, options.touch) {
                return Err(format!("{name} refused an allocation"));
            }
            samples.clear();
            subject.reset();
        }
        whole.clear();
        let before = faults();
        for _ in 0..passes {
            replay::each(ops, &mut table, &mut *subject, &mut samples, options.touch);
            subject.reset();
        }
        for _ in 0..passes {
            let ns = replay::pass(ops, &mut table, &mut *subject, options.touch);
            whole.push(ns.ok_or_else(|| format!("{name} refused an allocation"))?);
            subject.reset();
        }
        let during = faults() - before;
        mapped += during;
        let each = Summary::of(&mut samples);
        let per_event = |ns: u64| hundredths(ns, ops.len() as u64);
        let whole = Summary::of(&mut whole);
        println!(
            "  {name:10} {:9.1} {:7} {:7} {:7} {:7} {:11}    {:>17} {:>7} {:>7} {during:13}",
            each.mean,
            each.median,
            each.p90,
            each.p99,
            each.p999,
            each.max,
            per_event(whole.min),
            per_event(whole.median),
            per_event(whole.max),
        );
    }
    println!("the machine's load average when this ended: {}", load());
    if mapped == 0 {
        println!("PASS: no page was mapped during a timed pass");
    } else {
        println!("FAIL: {mapped} pages were mapped during timed passes: those passes timed the operating system too");
    }
    Ok(mapped == 0)
}

fn runs(loaded: &Loaded, a: &str, b: &str, options: &Options) -> Result<bool, String> {
    let count = options.runs.unwrap_or(30);
    let ops = &loaded.ops;
    println!("{}", loaded.header);
    println!("{count} runs of each of two allocators, in turn; a run is one pass, after one thrown away");
    println!("the machine's load average when this began: {}", load());
    let mut table = Table::new(ops);
    let mut subjects = [loaded.subject(a)?, loaded.subject(b)?];
    let mut times = [Vec::with_capacity(count), Vec::with_capacity(count)];
    for subject in &mut subjects {
        replay::pass(ops, &mut table, &mut **subject, false).ok_or("an allocator refused an allocation")?;
    }
    for _ in 0..count {
        for (subject, times) in subjects.iter_mut().zip(&mut times) {
            subject.reset();
            let ns =
                replay::pass(ops, &mut table, &mut **subject, false).ok_or("an allocator refused an allocation")?;
            times.push(ns);
        }
    }
    let (won, pairs) = wins(&times[0], &times[1]);
    println!("nanoseconds for each event, over a whole pass:");
    println!("  allocator     least  median    most   most over least");
    for (name, times) in [a, b].iter().zip(&mut times) {
        let s = Summary::of(times);
        let per_event = |ns: u64| hundredths(ns, ops.len() as u64);
        println!(
            "  {name:10} {:>7} {:>7} {:>7} {:>18}",
            per_event(s.min),
            per_event(s.median),
            per_event(s.max),
            hundredths(s.max, s.min)
        );
    }
    println!("of the {pairs} pairs of one run of each, {a} was the faster in {won}, {}", percent(won, pairs));
    println!("the machine's load average when this ended: {}", load());
    Ok(true)
}

// ---- the Lisp ----------------------------------------------------------------------------

fn program(name: &str) -> Result<&'static Program, String> {
    PROGRAMS.iter().find(|p| p.name == name).ok_or(format!("no program is called {name}"))
}

/// The lifetimes of the objects of one run of `program`.
fn lifetimes(program: &Program) -> Result<Lifetimes, String> {
    let mut lisp = Lisp::new(Profile::new());
    let value = lisp.run(program.source).map_err(|e| format!("{}: {e:?}", program.name))?;
    if value != program.value {
        return Err(format!("{}: the profiling run computed {value}", program.name));
    }
    Ok(lisp.heap.finish())
}

/// The run that callgrind counts: the program, from its text to its value, on `heap`.
/// The heap is made before, and looked at after.
#[inline(never)]
fn lisp_counted<H: Heap>(heap: H, source: &str) -> (Result<String, green_tea_lisp::Error>, H) {
    let mut lisp = Lisp::new(heap);
    let value = lisp.run(source);
    (value, lisp.heap)
}

/// One name for the five instances of [`lisp_counted`], for `--toggle-collect`.
#[unsafe(no_mangle)]
#[inline(never)]
fn green_tea_measure_lisp(heap: &str, program: &Program, words: usize, out: &mut String) -> bool {
    let report = match heap {
        "counting" => {
            let cycles = Cycles::Collect { every: usize::MAX };
            let (value, heap) = lisp_counted(RcHeap::new(words, Freeing::Recursive, cycles), program.source);
            value.map(|value| {
                let counts = heap.counts();
                (value, format!("{} count operations", counts.increments() + counts.decrements()))
            })
        }
        "mark-sweep" | "mark-compact" => {
            let reclaim = if heap == "mark-sweep" { Reclaim::Sweep } else { Reclaim::Compact };
            let (value, heap) = lisp_counted(MarkHeap::new(words, reclaim, MARK_STACK), program.source);
            value.map(|value| (value, collections(&heap)))
        }
        "copying" => {
            let (value, heap) = lisp_counted(Semispace::new(words), program.source);
            value.map(|value| (value, collections(&heap)))
        }
        _ => unreachable!("lisp_run checks the heap's name"),
    };
    match report {
        Ok((value, counts)) if value == program.value => {
            let _ = write!(out, "{} on {heap} in {words} words: {value}; {counts}", program.name);
            true
        }
        Ok((value, _)) => {
            let _ = write!(out, "{} on {heap} in {words} words computed {value}", program.name);
            false
        }
        Err(e) => {
            let _ = write!(out, "{} on {heap} in {words} words: {e:?}", program.name);
            false
        }
    }
}

fn collections<H: Collect>(heap: &H) -> String {
    let counts = heap.counts();
    format!(
        "{} collections traced {} objects, walked {} and moved {} words",
        counts.collections, counts.traced, counts.walked, counts.moved_words
    )
}

/// The same for a heap of blocks from an allocator. The lifetimes are found before, and
/// are not counted.
#[unsafe(no_mangle)]
#[inline(never)]
fn green_tea_measure_explicit(heap: Explicit, program: &Program) -> (Result<String, green_tea_lisp::Error>, Explicit) {
    lisp_counted(heap, program.source)
}

/// Run `program` on `heap`, with the run counted, and write what happened to `out`. The
/// heap is dropped here, outside what is counted: it holds the oracle's table, and freeing
/// that is no part of the program.
fn explicit_run(heap: Explicit, program: &Program, out: &mut String) -> bool {
    let (value, heap) = green_tea_measure_explicit(heap, program);
    let _ = match &value {
        Ok(value) => write!(
            out,
            "{}: {value}; {} allocated, {} freed, footprint {} bytes",
            program.name,
            heap.stats().allocated,
            heap.stats().freed,
            heap.subject().footprint()
        ),
        Err(e) => write!(out, "{}: {e:?}", program.name),
    };
    value.as_deref() == Ok(program.value) && heap.subject().check().is_ok()
}

/// An explicit heap over the allocator `name`, for `program`.
fn explicit(name: &str, lifetimes: &Lifetimes) -> Result<Explicit, String> {
    // What an allocator that never frees needs: every object the program allocates.
    let total = 32 * lifetimes.allocations();
    let subject = subjects::build(name, LISP_REGION, total).ok_or(format!("no allocator is called {name}"))?;
    Ok(Explicit::new(subject, Some(lifetimes)))
}

fn lisp_run(name: &str, heap: &str, words: usize) -> Result<bool, String> {
    let program = program(name)?;
    let mut out = String::new();
    let passed = if heap == "floor" {
        let made = lifetimes(program)?;
        // Every object, and nothing freed: the allocator is a pointer that moves.
        let bump = subjects::build("bump", LISP_REGION, 32 * made.allocations()).expect("bump is an allocator");
        explicit_run(Explicit::new(bump, None), program, &mut out)
    } else if let Some(allocator) = heap.strip_prefix("explicit:") {
        let made = lifetimes(program)?;
        explicit_run(explicit(allocator, &made)?, program, &mut out)
    } else if ["counting", "mark-sweep", "mark-compact", "copying"].contains(&heap) {
        if words == 0 {
            return Err("a garbage-collected heap needs a size in words".into());
        }
        green_tea_measure_lisp(heap, program, words, &mut out)
    } else {
        return Err(format!("no heap is called {heap}"));
    };
    println!("{out}");
    Ok(passed)
}

/// The allocators the Lisp's objects are given to.
const EXPLICIT: [&str; 6] = ["first-addr", "best-addr", "tlsf", "buddy", "pools", "shards"];

/// What `program` needs of the allocator `name` when its objects are freed as `made`
/// says: the footprint, in bytes. Found by replaying the lifetimes as a trace, and then
/// checked by running the program.
fn explicit_footprint(name: &str, program: &Program, made: &Lifetimes) -> Result<usize, String> {
    let ops = Ops::new(&made.events());
    let mut subject = subjects::build(name, LISP_REGION, ops.total).ok_or("no such allocator")?;
    let space = replay::space(&ops, &mut *subject).map_err(|e| format!("{name}: {e}"))?;
    let mut out = String::new();
    let heap = explicit(name, made)?;
    if !explicit_run(heap, program, &mut out) {
        return Err(format!("with explicit frees on {name}, {out}"));
    }
    let ran: usize =
        out.rsplit("footprint ").next().and_then(|s| s.split(' ').next()).and_then(|n| n.parse().ok()).unwrap_or(0);
    if ran != space.footprint {
        return Err(format!(
            "{}: the replay needed {} bytes of {name} and the run {ran}",
            program.name, space.footprint
        ));
    }
    Ok(space.footprint)
}

fn lisp_lifetimes() -> Result<bool, String> {
    println!("five programs, each run once on a heap that frees nothing and finds, before every allocation,");
    println!("what the root stack no longer reaches:");
    println!("  program  allocated     died  reachable at the end  peak words  peak bytes");
    let mut all = Vec::new();
    for program in &PROGRAMS {
        let made = lifetimes(program)?;
        let died = made.deaths.iter().map(Vec::len).sum::<usize>() + made.died_after_last;
        println!(
            "  {:8} {:9} {:8} {:21} {:11} {:11}",
            program.name,
            made.allocations(),
            died,
            made.left,
            made.peak_words,
            made.peak_words * 8
        );
        if died + made.left != made.allocations() {
            println!("FAIL: {}: the objects that died and those left aren't all the objects", program.name);
            return Ok(false);
        }
        all.push((program, made));
    }
    println!("the same programs with each object in a block from an allocator, freed when it died:");
    print!("  program  footprint in bytes:");
    EXPLICIT.iter().for_each(|name| print!(" {name:>10}"));
    println!();
    for (program, made) in &all {
        print!("  {:8}                    ", program.name);
        for name in EXPLICIT {
            print!(" {:10}", explicit_footprint(name, program, made)?);
        }
        println!();
    }
    println!("PASS: every program computed its value on every allocator, with the footprint its lifetimes replay to");
    Ok(true)
}

fn lisp_instructions(options: &Options) -> Result<bool, String> {
    let multiples = options.multiples.clone().unwrap_or_else(|| vec![10, 12, 15, 20, 30, 40, 50, 80]);
    let allocators = ["bump", "pools", "shards", "first-addr", "tlsf"];
    let heaps = ["mark-sweep", "mark-compact", "copying", "counting"];
    println!("instructions from a program's text to its value, counted by callgrind");
    println!("explicit: each object in a block from the allocator named, freed when an earlier run found it dead;");
    println!("  its floor is the same blocks from a pointer that moves, with nothing freed;");
    println!("  bump is that pointer again with the oracle asked, and frees that do nothing: what asking costs");
    println!("garbage-collected: a heap of green-tea-lisp, in a multiple of the bytes the pools needed;");
    println!("  its floor is the same heap in {NEVER} words, where it never collects");
    println!("  (the counting heap has no floor of its own, and is set against the mark-sweep heap's)");
    let mut passed = true;
    // Instructions over the floor, summed over the programs: for each allocator, and for
    // each heap at each multiple with how many programs fit.
    let mut explicit_sum = vec![0u64; allocators.len()];
    let mut heap_sum = vec![vec![(0u64, 0usize); multiples.len()]; heaps.len()];
    let mut objects = 0;
    for program in &PROGRAMS {
        let made = lifetimes(program)?;
        let allocations = made.allocations() as u64;
        objects += allocations;
        let footprint = explicit_footprint("pools", program, &made)?;
        let per_object = |n: u64| hundredths(n, allocations);
        println!(
            "{}: {allocations} objects; freed explicitly, the pools need {footprint} bytes, {} words",
            program.name,
            footprint / 8
        );
        println!("  explicit        footprint, words  instructions  over the floor  for each object");
        let floor = callgrind("green_tea_measure_explicit", &["lisp", "run", program.name, "floor"])?;
        println!("  nothing freed                  - {floor:13}");
        for (a, name) in allocators.iter().enumerate() {
            let heap = format!("explicit:{name}");
            let counted = callgrind("green_tea_measure_explicit", &["lisp", "run", program.name, &heap])?;
            explicit_sum[a] += counted - floor;
            println!(
                "  {name:14} {:17} {counted:13} {:15} {:>16}",
                explicit_footprint(name, program, &made)? / 8,
                counted - floor,
                per_object(counted - floor)
            );
        }
        println!("  heap          multiple   words  collections  instructions  over its floor  for each object");
        let mut sweep_floor = 0;
        for (h, heap) in heaps.iter().enumerate() {
            let never = NEVER.to_string();
            let heap_floor = if *heap == "counting" {
                sweep_floor
            } else {
                let floor = callgrind("green_tea_measure_lisp", &["lisp", "run", program.name, heap, &never])?;
                println!("  {heap:13}    never {NEVER:7}            0 {floor:13}");
                floor
            };
            if *heap == "mark-sweep" {
                sweep_floor = heap_floor;
            }
            for (m, multiple) in multiples.iter().enumerate() {
                let words = footprint / 8 * multiple / 10;
                let times = format!("{}.{}", multiple / 10, multiple % 10);
                let size = words.to_string();
                match callgrind_with_output("green_tea_measure_lisp", &["lisp", "run", program.name, heap, &size]) {
                    Ok((counted, printed)) => {
                        let collections = printed.split("; ").nth(1).and_then(|s| s.split(' ').next()).unwrap_or("-");
                        let collections = if *heap == "counting" { "-" } else { collections };
                        let over = counted.saturating_sub(heap_floor);
                        heap_sum[h][m].0 += over;
                        heap_sum[h][m].1 += 1;
                        println!(
                            "  {heap:13} {times:>8} {words:7} {collections:>12} {counted:13} {over:15} {:>16}",
                            per_object(over)
                        );
                    }
                    Err(Uncounted::NoRoom) => println!("  {heap:13} {times:>8} {words:7}   the program doesn't fit"),
                    Err(why) => return Err(why.into()),
                }
            }
        }
        let again = callgrind("green_tea_measure_explicit", &["lisp", "run", program.name, "floor"])?;
        if again != floor {
            println!("FAIL: the explicit floor of {} was {floor} instructions, and then {again}", program.name);
            passed = false;
        }
    }
    println!("the five programs together, {objects} objects: instructions over the floor for each object allocated");
    print!("  explicit:");
    allocators.iter().zip(&explicit_sum).for_each(|(name, sum)| print!("  {name} {}", hundredths(*sum, objects)));
    println!();
    print!("  heap, in a multiple of the pools' bytes:");
    multiples.iter().for_each(|m| print!(" {:>9}", format!("{}.{}", m / 10, m % 10)));
    println!();
    for (h, heap) in heaps.iter().enumerate() {
        print!("  {heap:40}");
        for (sum, fit) in &heap_sum[h] {
            if *fit == PROGRAMS.len() {
                print!(" {:>9}", hundredths(*sum, objects));
            } else {
                print!(" {:>9}", format!("{fit} of {} fit", PROGRAMS.len()));
            }
        }
        println!();
    }
    if !multiples.contains(&10) {
        println!(
            "FAIL: no heap here was given the bytes the explicit allocator needed: this table compares nothing at \
             equal footprint"
        );
        passed = false;
    }
    if passed {
        println!("PASS: every program's explicit floor was counted twice, and was the same both times");
    }
    Ok(passed)
}
