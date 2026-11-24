#import "../lib.typ": *

= green-tea-trace <crate-trace>

`green-tea-trace` is a host program that reads an allocation trace from a file and does what the shell's `replay` command does in the kernel: it describes the trace and replays it against each allocator of @crate-fit. It has three commands, `stat`, `replay` and `convert`. Its tables match the kernel's column for column, so a trace dumped by the kernel can be replayed on the host and the two outputs compared line by line.

#spec(
  ("Source", [#repo("green-tea-trace/src/main.rs"), 405 lines without its tests]),
  ("Depends on", [`green-tea-fit`; `std`]),
  ("Used by", [nothing: it is a program. Its traces come from `cargo xtask trace` (@crate-xtask)]),
  ("Tests", [11 host tests]),
)

#dtable(columns: (auto, 1fr), ("Command", "What it does"),
  [`stat <file>`], [Prints the form and size of the file, its records, the sizes asked for, how long blocks lived, the peak of live bytes, and two checks on the order of the records.],
  [`replay <file>`], [Replays the trace against first, next and best fit, each with a LIFO and an address-ordered free list, then against the buddy system and TLSF. `--region-kib <n>` sets the region each replay gets, from 1 KiB to 4 GiB. `--shuffle [<hex seed>]` adds a replay with the sizes and lifetimes dealt out at random.],
  [`convert <file> <out>`], [Writes the same records in the other form, or in the one named by `--text` or `--binary`.],
)

A file is in either form of `tracefile` (@crate-fit), and the first bytes say which: `Form::of` compares them with `MAGIC`. For every command `-` means the standard input, and for the `<out>` of `convert` it means the standard output. An exit status of 2 is a command line that makes no sense, and 1 is a trace that was refused. In the second case `Failure::Refused` carries what had been printed up to then, and `main` prints it before the error, so a replay that fails keeps the rows it had already written.

== `stat`

`stat` works on the records as read, and then on the `Event`s that `tracefile::events` makes of them, in which a reallocation is a free and an allocation. The trace here is the shell's `session` script, recorded in the kernel on riscv64 by `cargo xtask trace`:

#output(title: "green-tea-trace stat shell.gtt", size: 6.1pt)[
```
shell.gtt: the text form, 277801 bytes; 20483 records: 10316 allocated, 10167 freed, 0 reallocated
the trace: 20483 events, 10316 allocations of 656120 bytes in all, 10167 frees
sizes: 167 different ones, from 4 to 6144 bytes; the most common: 64 (22.6%) 128 (12.1%) 8 (6.3%) 16 (5.5%) 32 (4.9%) 5 (3.5%)
lifetimes: 85.1% of the blocks are freed before 8 more are allocated, 88.0% before 64 more; 149 are never freed
reallocations: 0, of which 0 left the block where it was; 0 allocations of no bytes; 0 of 4 GiB or more
live bytes: 14902 at the peak; 0 frees are of blocks the trace never allocated; 0 allocations are at an address that is still allocated
```
]

The program allocated 656,120 bytes over the session and never held more than 14,902 of them. `describe` prints the second, third and fourth lines, which are the ones the shell's `replay` starts with. `peak_live` keeps a map from address to size and follows the live total through the events.

The last line reports two faults of order, found by `mismatches`:

#srcfile("green-tea-trace/src/main.rs", lines: ((249, 259),), notes: ((254, [A free of an address that isn't allocated. It happens when the recording starts after the program does.]), (255, [An allocation at an address still allocated. It can't happen in a trace whose records are in the order of the calls.]),))[```rust
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
```]

The two faults are treated differently. An unmatched free is harmless, since `trace::replay` counts it and goes on. An address allocated twice means the records are out of order, and `replay` refuses such a trace before running anything.

== `replay`

#srcfile("green-tea-trace/src/main.rs", lines: ((302, 330),), notes: ((308, [The refusal: no replay of records out of order means anything.]), (314, [The default region: four times the peak of live bytes, rounded up to a power of two, and at least 512 KiB, which is what the shell uses.]), (315, [One allocation of `u128`s, so the region is aligned to 16. Each replay gets all of it, one after another.]), (322, [Every allocator must have made every allocation and seen the same peak of live bytes.]),))[```rust
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
```]

`table` builds a `Fit` for each of the six entries of `WAYS` on the same memory, replays, and runs `check` on the heap afterwards. `others` does the same for `Buddy` and `Tlsf` through `measured`. `row` prints one line and makes two checks of its own:

#srcfile("green-tea-trace/src/main.rs", lines: ((339, 341), (343, 349),), notes: ((339, [Over the live bytes at the moment the footprint last grew.]), (340, [Over the most live bytes there ever were.]), (341, [Over the most bytes ever in blocks, which leaves out the tags and rounding and counts only the holes.]), (343, [A refused allocation makes the row meaningless: the replay stops there and says to use a larger region.]), (346, [Live bytes can't exceed used bytes, nor used bytes the footprint. A row that says so is a bug in an allocator or in the replay.]),))[```rust
    percent(r.footprint - r.live_at_peak_footprint, r.live_at_peak_footprint),
    percent(r.footprint - r.peak_live, r.peak_live),
    percent(r.footprint - r.peak_used, r.peak_used),
// …
if r.failed != 0 {
    return Err("a trace didn't fit in the region it was replayed in: give it a larger one with --region-kib");
}
if r.peak_live > r.peak_used || r.peak_used > r.footprint {
    return Err("a replay used less memory than the program asked for");
}
Ok(())
```]

#output(title: "green-tea-trace replay shell.gtt --shuffle", size: 6.3pt)[
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
  allocator            peak live  peak used  footprint  over live then  over peak live  over peak used
  buddy system             14902      27744      28160           91.2%           89.0%            1.5%
  TLSF                     14902      20992      21280           44.5%           42.8%            1.4%
the same sizes and the same lifetimes, dealt out at random:
  policy     order     peak live  peak used  footprint  over live then  over peak live  over peak used
  first fit  LIFO          18528      23040      45984          148.9%          148.2%           99.6%
  first fit  address       18528      23520      25216           36.5%           36.1%            7.2%
  next fit   LIFO          18528      23088      52688          324.4%          184.4%          128.2%
  next fit   address       18528      23248      41520          234.4%          124.1%           78.6%
  best fit   LIFO          18528      23456      24800           34.2%           33.9%            5.7%
  best fit   address       18528      23344      24784           34.1%           33.8%            6.2%
```
]

*The policy and the order of the list decide the footprint of the same trace by a factor of seven*: from 21,408 bytes for best fit to 156,608 for next fit over a LIFO list. Shuffling changes that picture. Next fit over a LIFO list falls to 52,688 bytes, and first fit by address rises from 21,456 to 25,216. The shuffled trace also has a different peak of live bytes (18,528 against 14,902), because which blocks overlap has changed, so its percentages are against a different base.

The default seed is `SEED`, the one the shell's `replay` uses. With it, the kernel and the host produce the same tables. The serial log of the capture holds the kernel's own `replay` of this trace, and comparing it with the output above, line by line, gives:

#output(title: "the kernel's replay, from the serial log, against green-tea-trace replay --shuffle")[
```
23 of the 23 lines of this replay are identical to the kernel's
```
]

The kernel's `replay` goes on to a fourth table, with blocks that outlive `LONG_LIVED` (64) allocations in a region of their own. `green-tea-trace` doesn't print that table.

The word after `--shuffle` is its seed only if a file has already been named and the word parses in base 16. Before the file, the next word is the file, so a trace called `cafe` can follow the option:

#srcfile("green-tea-trace/src/main.rs", lines: ((108, 112),), notes: ((110, [`files` is empty until the file has been read, so nothing can be taken for a seed before it.]),))[```rust
"--shuffle" => {
    // Before the file, the next word is the file, whatever it looks like.
    let seed = rest.next_if(|next| !files.is_empty() && u64::from_str_radix(next, 16).is_ok());
    shuffle = Some(seed.map_or(SEED, |seed| u64::from_str_radix(seed, 16).unwrap_or(SEED)));
}
```]

The largest region is `MOST_REGION`, where the buddy system's table of lists ends. A larger one is a mistake on the command line, and exit status 2:

#output(title: "green-tea-trace replay shell.gtt --region-kib 5000000")[
```
error: --region-kib needs a number of KiB from 1 to 4194304
```
]

== `convert`

`convert` picks the other form unless told, and writes it through `Form::write`. The binary form of this trace is 73,911 bytes against 277,801 for the text, 27% of the size, and it describes the same records:

#output(title: "green-tea-trace stat shell.gttb", size: 6.3pt)[
```
shell.gttb: the binary form, 73911 bytes; 20483 records: 10316 allocated, 10167 freed, 0 reallocated
```
]

#output(title: "green-tea-trace replay, of the text form and of the binary form")[
```
the replay of the binary form is byte for byte the replay of the text form
```
]

Comments don't survive a conversion, because they aren't records. A trace converted to binary and back has the same records and checksum as the original, without its `# recorded by` line.

== Tests

The 11 tests run the program's functions directly. They cover the counts in `stat` on a six-record trace with a reallocation and an empty block, that "before _n_ more" means fewer than _n_, the peak of live bytes, rounding in `percent`, the number of rows in a replay, the refusal of records out of order, of a region too small and of one too large, the checks on the command line, a file named in hexadecimal after `--shuffle`, and a round trip through `convert` that also removes a line from a file and expects the error to name it.

#output(title: "cargo test -p green-tea-trace")[
```
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

== Limits

- It replays against the six free-list configurations, the buddy system and TLSF. `Hardened`, the pools and the slabs are measured by @crate-measure.
- The whole trace is held in memory, and the standard input is read to its end before anything is printed.

The kernel's `replay`, without the kernel. @crate-measure asks what the same traces cost in instructions and in time.
// chapter-status:begin
// stage: research-done
// revision: 2
// body: f43784850740
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r2 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
