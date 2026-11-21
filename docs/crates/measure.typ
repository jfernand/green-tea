#import "../lib.typ": *

= green-tea-measure <crate-measure>

`green-tea-measure` replays one recorded trace against every single-threaded allocator in the workspace and reports what each needed, in bytes, in searches, in instructions counted by callgrind, and in nanoseconds as a distribution. It also runs the Lisp programs of @crate-lisp with explicit frees placed by an *oracle*, after Hertz and Berger's oracular memory manager, to compare `malloc` and `free` with garbage collection on the same programs. It is a library of four modules and a command that prints the tables.

#spec(
  ("Source", [#repo("green-tea-measure/src/"), 6 files, 2,058 lines, and 314 lines of tests in `tests.rs`]),
  ("Depends on", [`green-tea-fit`, `-pool`, `-kmem`, `-smp`, `-shard`, `-lisp`; `std`. `valgrind` at run time, for the instruction counts]),
  ("Used by", [nothing: it is a program, for the host]),
  ("Tests", [17 host tests]),
)

#dtable(columns: (auto, 1fr), ("Module", "What it holds"),
  [`subjects`], [`Subject`, the interface every allocator is measured through; `build`, `NAMES`, `describe`, `Memory`],
  [`replay`], [`Op`, `Ops`, `Table`, `Space`, and the replays `space`, `plain`, `pass`, `each`, with `clock`],
  [`stats`], [`percentile`, `Summary`, `wins`],
  [`oracle`], [`Lifetimes`, `Profile`, `Explicit`],
  [`main.rs`], [the commands],
)

#dtable(columns: (auto, 1fr), ("Command", "What it prints"),
  [`space <trace>`], [Bytes and searches for each allocator. The same on every run.],
  [`instructions <trace>`], [Instructions for each event, from callgrind, less those of the replay loop.],
  [`time <trace>`], [Nanoseconds for each event as a distribution, and for whole passes. `--passes <n>` (default 20), `--no-warmup`, `--touch`.],
  [`runs <trace> <a> <b>`], [`--runs <n>` (default 30) whole passes of two allocators in turn, and how often the first was faster.],
  [`count <trace> <a>`], [Replays against one allocator and prints a count. This is the program `instructions` runs under valgrind.],
  [`lisp lifetimes`], [What the oracle found for the five programs, and each program's footprint on six allocators.],
  [`lisp instructions`], [The programs on explicit and garbage-collected heaps, in instructions. `--multiples <a,b,...>` sets the heap sizes.],
  [`lisp run <p> <heap> [<words>]`], [One program on one heap: `floor`, `explicit:<allocator>`, `counting`, `mark-sweep`, `mark-compact` or `copying`.],
)

The trace commands take `--region-kib <n>`, `--shuffle` (replay with sizes and lifetimes dealt out at random, with the shell's seed), `--only <a,b,...>`, and `--pins <file>`. A line of the pins file is `<name> <records> <checksum>`, and a trace whose count or checksum differs from its line is refused, so that a table can't silently be about a different recording:

#output(title: "green-tea-measure space shell.gtt --pins wrong.pins", size: 6pt)[
```
FAIL: shell is not the trace that was measured: wrong.pins pins 20483 records with checksum 0000000000000000, and this file has 20483 with f137558e1fe4dcb2
```
]

Every table of a trace starts with a line that says what it was run on, including the checksum. A command that checks a condition ends with a line starting `PASS: ` or `FAIL: `, and its exit status follows it.

== `subjects`: one interface

A `Subject` owns its memory, and is what one replay loop drives:

#srcfile("green-tea-measure/src/subjects.rs", lines: ((22, 22), (24, 24), (32, 32), (36, 36), (41, 41), (45, 45), (48, 48), (51, 52),), notes: ((32, [`free` receives the size and alignment asked for. `Fit` doesn't need them; a size-class allocator does.]), (36, [`None` for an allocator that keeps no such count.]), (45, [The free blocks looked at, and the most in one call. `None` for an allocator that doesn't search.]), (51, [Start again on the same memory, so a timed pass doesn't touch pages for the first time.]),))[```rust
pub trait Subject {
// …
    fn allocate(&mut self, size: usize, align: usize) -> Option<usize>;
// …
    unsafe fn free(&mut self, addr: usize, size: usize, align: usize);
// …
    fn used(&self) -> Option<usize>;
// …
    fn footprint(&self) -> usize;
// …
    fn searched(&self) -> Option<(usize, usize)>;
// …
    fn check(&self) -> Result<(), &'static str>;
// …
    fn reset(&mut self);
}
```]

`build` makes one from its name, and `NAMES` lists the 15 names in the order the tables print them. `describe` gives each a phrase for a caption.

#dtable(columns: (24%, 1fr, 22%), ("Name", "What it is", "Footprint is"),
  [`bump`], [A pointer that moves, and never frees. Its region is twice the trace's bytes plus 64 KiB.], [bytes passed],
  [`first-lifo`, `first-addr`, `next-lifo`, `next-addr`, `best-lifo`, `best-addr`], [`Fit` with each `Policy` and `Order`.], [highest address],
  [`buddy`, `tlsf`], [`Buddy` and `Tlsf`.], [highest address],
  [`hardened`], [`Hardened<ALL>`: first fit, LIFO, all five defences, seeded with `SEED`.], [highest address],
  [`pools`], [A `FreeList` for each of the twelve size classes of @crate-smp, and TLSF for larger blocks.], [slots ever handed out, plus TLSF],
  [`slabs`], [A slab cache per class over page frames from a buddy system (@crate-kmem). Larger blocks are whole pages.], [most pages held],
  [`caches`], [One CPU's cache in front of a TLSF behind its lock.], [TLSF's],
  [`shards`], [One CPU's heap of 16 KiB pages (@crate-shard), and TLSF for larger blocks.], [arena pages, plus TLSF],
  [`null`], [No allocator: each call returns a new number. What a replay costs by itself.], [0],
)

The size classes are 16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768 and 1,024 bytes. A request for more than 1,024 bytes, or aligned to more than 16, skips them. `pools` counts only the slots it has ever handed out, since each pool has a whole region of address space and touches little of it.

`Memory` is a zeroed region that starts at a multiple of 4,096, whose `base`, `len` and `is_empty` say where it is. A block aligned to 64 is placed by its address, so a region starting wherever `Vec` happened to put it would give a footprint that changed between runs.

== `replay`: a trace made ready

The replay in @crate-fit keeps a map from recorded addresses to new ones, and looks in it at every event. Here the replay itself is being timed, so `Ops::new` does that work once, before any clock is read. It gives every block a *slot*, an index into a table, and reuses a slot when its block is freed:

#srcfile("green-tea-measure/src/replay.rs", lines: ((48, 71),), notes: ((50, [A free the trace never allocated is counted in `unmatched` and left out.]), (59, [The most recently freed slot first, so `slots` is the most blocks the trace has at once.]), (64, [Two allocations of one address mean the records are out of order, and this panics.]),))[```rust
for event in events {
    if event.is_free() {
        let Some((slot, size)) = slot_of.remove(&event.addr) else {
            out.unmatched += 1;
            continue;
        };
        unused.push(slot);
        live -= size;
        out.frees += 1;
        out.ops.push(Op { slot, size: 0, align: 0 });
    } else {
        let slot = unused.pop().unwrap_or_else(|| {
            out.slots += 1;
            out.slots as u32 - 1
        });
        let size = event.size as usize;
        assert!(slot_of.insert(event.addr, (slot, size)).is_none(), "an address was allocated twice");
        live += size;
        out.total += size;
        out.peak_live = out.peak_live.max(live);
        out.allocations += 1;
        out.ops.push(Op { slot, size: event.size, align: event.align });
    }
}
```]

An `Op` is a slot, a size and an alignment, with a size of 0 meaning free. `Ops` also has `len` and `is_empty`. A `Table` is the array of slots, made once and reused by every pass, so a timed pass never writes to memory the operating system hasn't mapped. Replaying an event is then one read or one write of it:

#srcfile("green-tea-measure/src/replay.rs", lines: ((103, 121),))[```rust
#[inline(always)]
fn step(op: Op, table: &mut Table, subject: &mut dyn Subject) -> bool {
    let table = &mut table.0;
    if op.size == 0 {
        let held = table[op.slot as usize];
        // SAFETY: the slot holds a block that `subject` returned and that hasn't been freed:
        // `Ops::new` matched every free with one allocation.
        unsafe { subject.free(held.addr, held.size as usize, held.align as usize) };
        true
    } else {
        match subject.allocate(op.size as usize, op.align as usize) {
            Some(addr) => {
                table[op.slot as usize] = Held { addr, size: op.size, align: op.align };
                true
            }
            None => false,
        }
    }
}
```]

There are four loops over `step`. `space` reads the subject's counters after every allocation and fills a `Space`: `refused`, `peak_live`, `peak_used`, `footprint`, `searched`. `plain` does nothing else, and is what callgrind counts. `pass` reads the clock around a whole pass, and `each` reads it around every event. With `touching`, `touch` writes the first byte of each new block, its last, and one in every page between, as the program that asked for it would.

== Space

#output(title: "green-tea-measure space shell.gtt", size: 6.2pt)[
```
shell: 20483 events, 10316 allocations of 656120 bytes in all, 14902 bytes live at the peak; checksum f137558e1fe4dcb2
each allocator in regions of 512 KiB:
  allocator    peak used  footprint  over peak live  over peak used  blocks searched  per event  most
  bump                 -     663752         4354.1%               -
  first-lifo       20624      83968          463.5%          307.1%            16542       0.81    52
  first-addr       21008      21456           44.0%            2.1%            66965       3.27    49
  next-lifo        20400     156608          950.9%          667.7%            10883       0.53     8
  next-addr        20384     131664          783.5%          545.9%           484878      23.67   143
  best-lifo        20992      21408           43.7%            2.0%            51785       2.53    34
  best-addr        20992      21408           43.7%            2.0%            90446       4.42    43
  buddy            27744      28160           89.0%            1.5%            13288       0.65    13
  tlsf             20992      21280           42.8%            1.4%            10316       0.50     1
  hardened         22320      30160          102.4%           35.1%           474570      23.17    79
  pools                -      24176           62.2%               -
  slabs                -      57344          284.8%               -
  caches           36032      36032          141.8%            0.0%
  shards               -     174640         1071.9%               -
```
]

The columns after the footprint are its excess over the peak of live bytes and over the peak of used bytes, then the free blocks looked at, per event, and the most in one call. TLSF examines one block per allocation, always: 10,316 for 10,316 allocations. Next fit by address looks at 23.67 per event. `hardened` looks at 23.17, because a random choice among the blocks that fit has to see all of them.

The size-class allocators pay in a different column. `pools` needs 24,176 bytes, close to TLSF's 21,280, but `shards` needs 174,640, because its small blocks come from 16 KiB pages of one size class each. For `caches`, the peak used and the footprint are both 36,032: TLSF counts a block waiting in a cache as allocated, so the two agree and the excess reads 0.0%.

== Instructions

`instructions` runs this same program under callgrind, once per allocator, through the command `count`. `--toggle-collect` restricts the count to one function, which has an unmangled name so that it can be given on the command line:

#srcfile("green-tea-measure/src/main.rs", lines: ((270, 274), (295, 300),))[```rust
#[unsafe(no_mangle)]
#[inline(never)]
fn green_tea_measure_counted(ops: &Ops, table: &mut Table, subject: &mut Box<dyn Subject>) -> bool {
    replay::plain(ops, table, &mut **subject, 1)
}
// …
    let output = Command::new("valgrind")
        .args(["--tool=callgrind", "--callgrind-out-file=/dev/null"])
        .arg(format!("--toggle-collect={function}"))
        .arg(exe)
        .args(args)
        .output()
```]

The loop with the `null` subject is counted twice. If the two counts differ the command ends in `FAIL`, since subtracting one from the others would then mean nothing:

#output(title: "green-tea-measure instructions shell.gtt", size: 6.5pt)[
```
instructions in the replay loop, counted by callgrind; with no allocator the loop is 441116, 21.54 for each event
  allocator    instructions  less the loop's  per event
  bump               637121           196005       9.57
  first-lifo        2603858          2162742     105.59
  first-addr        3217974          2776858     135.57
  next-lifo         2553851          2112735     103.15
  next-addr         5893880          5452764     266.21
  best-lifo         3332471          2891355     141.16
  best-addr         3700204          3259088     159.11
  buddy             2350726          1909610      93.23
  tlsf              3111389          2670273     130.37
  hardened         32437852         31996736    1562.11
  pools             1456048          1014932      49.55
  slabs             4180005          3738889     182.54
  caches            1558617          1117501      54.56
  shards            2162441          1721325      84.04
PASS: the loop with no allocator was counted twice, and was 441116 instructions both times
```
]

The counts are exact and repeat from run to run, which is what they are for. `pools` is the cheapest real allocator at 49.55 instructions per event, and `hardened` the most expensive at 1,562.11. The buddy system, at 93.23, costs less than TLSF at 130.37 on this trace, although its footprint is a third larger.

== Time

`time` measures in two ways, because reading the clock costs more than some allocators do. It makes `--passes` passes with the clock read around every event, for the distribution, and as many with it read only around the pass, for an honest total:

#srcfile("green-tea-measure/src/main.rs", lines: ((408, 431),), notes: ((412, [The warm-up. The samples array is written once by hand, and one pass writes the allocator's regions and the table.]), (421, [Page faults are counted from here, through `/proc/self/stat`.]), (423, [The passes with the clock around every event.]), (427, [The passes with the clock around the whole pass.]),))[```rust
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
    whole.push(ns.ok_or(format!("{name} refused an allocation"))?);
    subject.reset();
}
let during = faults() - before;
```]

`faults` reads `/proc/self/stat` into an array on the stack, so that counting faults doesn't itself allocate. If a timed pass mapped a page, the command says so and fails, since that pass timed the operating system as well as the allocator.

#output(title: "green-tea-measure time shell.gtt", size: 5.7pt)[
```
reading the clock twice with nothing between: median 19 ns, 99% 24 ns, 99.9% 28 ns, most 2933 ns
nanoseconds for each event:
  allocator       mean  median     90%     99%   99.9%        most    whole pass: least  median    most  pages mapped
  bump            21.1      21      25      33      41       14311                 3.66    3.68    4.02             0
  first-lifo      30.2      28      42      63     127        3449                12.26   12.74   14.46             0
  first-addr      43.4      39      69     108     146        7767                26.07   26.51   27.38             0
  next-lifo       30.0      30      40      52      75        4516                 9.47   10.05   11.53             0
  next-addr       67.1      38     148     209     238       70186                46.50   48.07   51.80             0
  best-lifo       42.8      32      79     129     179       60816                25.82   26.83   28.00             0
  best-addr       45.8      39      77     127     210        4607                32.79   34.33   35.44             0
  buddy           28.2      24      40      55      73        6298                10.78   11.06   12.90             0
  tlsf            34.6      32      49      62      72        4406                16.20   16.79   18.75             0
  hardened       234.1     147     476     613     742        6829               223.46  235.66  246.50             0
  pools           24.2      22      32      39      47        3060                 7.83    7.93    9.09             0
  slabs           59.5      38     132     182     220        4185                41.21   41.45   43.87             0
  caches          24.3      21      31      43     222        3824                 7.92    8.06    9.11             0
  shards          32.0      30      41      52      65        5749                10.81   11.03   12.04             0
the machine's load average when this ended: 2.90 2.93 4.21
PASS: no page was mapped during a timed pass
```
]

*Two readings of the clock cost a median of 19 ns*, more than the 7.93 ns per event that `pools` takes over a whole pass. The per-event columns include that cost, and nothing is subtracted from them: `bump` shows a median of 21 ns per event and 3.68 ns over a whole pass. The per-event columns are for the shape of the distribution, and the whole-pass columns for the total. `hardened` has a median of 147 ns and 742 ns at the 99.9th percentile.

`Summary::of` sorts the samples and keeps their `count`, the mean, the least and the most, and the 50th, 90th, 99th and 99.9th percentiles. `percentile` is the nearest-rank rule, with `p` in thousandths of a percent so that no floating point decides which sample is the 99.9th:

#srcfile("green-tea-measure/src/stats.rs", lines: ((9, 13),))[```rust
pub fn percentile<T: Copy>(sorted: &[T], p: u32) -> T {
    assert!(!sorted.is_empty() && p <= 100_000);
    let rank = (sorted.len() as u128 * p as u128).div_ceil(100_000) as usize;
    sorted[rank.max(1) - 1]
}
```]

Without the warm-up, the pages are mapped during the timed passes, and the check catches it:

#output(title: "green-tea-measure time shell.gtt --no-warmup --only shards", size: 5.7pt)[
```
  shards          26.0      24      33      43      61       86872                 8.58    8.87   10.42           414
FAIL: 414 pages were mapped during timed passes: those passes timed the operating system too
```
]

=== `runs`

`runs` answers the question a single comparison can't: how often would one run of each have ranked them correctly? It alternates whole passes of two allocators, and `wins` counts the pairs of one run of each in which the first was faster.

#output(title: "green-tea-measure runs shell.gtt tlsf first-addr", size: 6.5pt)[
```
nanoseconds for each event, over a whole pass:
  allocator     least  median    most   most over least
  tlsf         15.71   16.12   20.49               1.30
  first-addr   20.55   21.32   24.08               1.17
of the 900 pairs of one run of each, tlsf was the faster in 900, 100.0%
```
]

== `oracle`: explicit frees for a program without any

The Lisp of @crate-lisp never frees an object. To ask what the same program would cost with `malloc` and `free`, something has to decide where the calls to `free` go, and two runs do it.

The first is on a `Profile` heap, which frees nothing and, before every allocation, marks what the root stack reaches. Whatever was alive and isn't reached has died since the last allocation:

#srcfile("green-tea-measure/src/oracle.rs", lines: ((77, 101),), notes: ((78, [A stamp for this allocation, so that no mark bits need clearing.]), (91, [Objects not reached move out of `alive` into this allocation's deaths.]),))[```rust
fn find_the_dead(&mut self) {
    let stamp = self.objects.len() as u32 + 1;
    self.work.extend(self.roots.iter().filter_map(|v| v.as_obj()).map(|obj| obj.0));
    while let Some(n) = self.work.pop() {
        if self.reached[n as usize] == stamp {
            continue;
        }
        self.reached[n as usize] = stamp;
        let (kind, fields) = &self.objects[n as usize];
        self.work.extend(fields[..kind.fields()].iter().filter_map(|v| v.as_obj()).map(|obj| obj.0));
    }
    let mut dead = Vec::new();
    let (reached, words) = (&self.reached, &self.lifetimes.words);
    let mut freed = 0;
    self.alive.retain(|&n| {
        let kept = reached[n as usize] == stamp;
        if !kept {
            dead.push(n);
            freed += words[n as usize] as usize;
        }
        kept
    });
    self.live_words -= freed;
    self.lifetimes.deaths.push(dead);
}
```]

`finish` returns the `Lifetimes`: for each allocation the objects to free before it (`deaths`), the `words` of each, how many are `left`, and `peak_words`. `allocations` is the number of objects, and `events` writes the run out as an ordinary trace, with an object's number plus one as its address.

The second run is on an `Explicit` heap, which puts each object in a block from a `Subject` (reachable through `subject`) and frees what the lifetimes name, when they name it:

#srcfile("green-tea-measure/src/oracle.rs", lines: ((256, 272),), notes: ((259, [The oracle: this allocation's deaths, freed before it.]), (262, [In a debug build, an object freed twice panics, and so does any use of a freed object.]),))[```rust
fn alloc(&mut self, kind: Kind) -> Result<(), Full> {
    let n = self.blocks.len();
    // The oracle: free what died since the last allocation.
    if let Some(deaths) = self.deaths.get(n) {
        for &dead in deaths {
            let block = core::mem::take(&mut self.blocks[dead as usize]);
            #[cfg(debug_assertions)]
            assert!(self.live.remove(&block), "the oracle freed an object twice");
            // SAFETY: the block came from `allocate` below, with the size its header
            // gives, and is freed once: its entry is now 0.
            let words = Explicit::kind_of(unsafe { *(block as *const u64) }).fields() + 1;
            unsafe { self.subject.free(block, words * 8, 8) };
            self.stats.freed += 1;
            self.stats.live_words -= words;
        }
    }
    let words = kind.fields() + 1;
```]

The interpreter is deterministic, so the second run makes the same allocations in the same order as the first. A reference is 32 bits, counted in words from an anchor 16 GiB below the first block (`REACH`), and an assertion refuses a block further than that.

The debug build checks every object access against the set of live blocks. One test moves every death one allocation early and expects the panic, `lifetimes_that_free_an_object_too_early_are_caught`. Another requires the footprint of a real run to equal that of replaying the same lifetimes as a trace, and `lisp lifetimes` makes the same check for every program:

#output(title: "green-tea-measure lisp lifetimes", size: 6.3pt)[
```
  program  allocated     died  reachable at the end  peak words  peak bytes
  list          8190     8093                    97         614        4912
  fib           2002     1970                    32         134        1072
  alist        14309    14126                   183         766        6128
  ring          9130     9016                   114         396        3168
  closure       2877     2807                    70         237        1896
the same programs with each object in a block from an allocator, freed when it died:
  program  footprint in bytes: first-addr  best-addr       tlsf      buddy      pools     shards
  list                               9600       9600       9600       6848       6400      16384
  fib                                1872       1872       1872       1856       1248      16384
  alist                             11328      11328      11328       9856       7552      16384
  ring                               6048       6048       6048       4608       4032      16384
  closure                            3600       3600       3600       2816       2400      16384
```
]

`lisp run` is one cell of the next table, and the program it runs must compute the value it is known to have.

== Explicit frees against garbage collection

`lisp instructions` counts each program from its text to its value on 41 configurations: the floor and five allocators for the explicit heaps, and for the garbage-collected heaps three floors and four heaps at eight sizes each. The *floor* of the explicit heaps is the same blocks from a bump pointer with nothing freed. The garbage-collected heaps run at multiples of the bytes the pools needed, and the floor of each is the same heap with $2^22$ words, where it never collects.

#output(title: "green-tea-measure lisp instructions: a program, then all five together", size: 5.8pt)[
```
fib: 2002 objects; freed explicitly, the pools need 1248 bytes, 156 words
  explicit        footprint, words  instructions  over the floor  for each object
  nothing freed                  -       6528674
  bump                        7981       6592671           63997            31.97
  pools                        156       6733771          205097           102.45
  shards                      2048       6860853          332179           165.92
  first-addr                   234       6938445          409771           204.68
  tlsf                         234       7220164          691490           345.40
  heap          multiple   words  collections  instructions  over its floor  for each object
  mark-sweep       never 4194304            0      13329484
…
the five programs together, 36508 objects: instructions over the floor for each object allocated
  explicit:  bump 31.23  pools 101.78  shards 163.84  first-addr 204.28  tlsf 276.12
  heap, in a multiple of the pools' bytes:       1.0       1.2       1.5       2.0       3.0       4.0       5.0       8.0
  mark-sweep                               3 of 5 fit 3 of 5 fit 3 of 5 fit    186.96    102.23     86.20     73.53     62.58
  mark-compact                                675.79    434.15    286.94    196.67    139.15    115.25    101.28     79.43
  copying                                  0 of 5 fit 0 of 5 fit 0 of 5 fit    354.21    130.55     79.45     58.41     34.71
  counting                                 3 of 5 fit 3 of 5 fit 3 of 5 fit   2798.25   2649.78   2615.61   2596.17   2570.75
PASS: every program's explicit floor was counted twice, and was the same both times
```
]

The last table is in instructions per object, over each heap's own floor. The `bump` figure, 31.23, is the price of asking the oracle, and every explicit figure includes it. With the pools' own bytes (the column 1.0) only `mark-compact` runs all five programs. With twice as many, all four do. A copying heap with 5 times the bytes spends 58.41 instructions per object on reclamation, against 101.78 for explicit frees through the pools. This is Hertz and Berger's trade-off: a tracing heap gets cheaper as it gets more room, and the explicit figure doesn't move.

== Tests

The 17 tests check `Ops` against hand-made traces, compare `space` with `green-tea-fit`'s own replay, and run all 15 subjects through a workload. For every subject but `null`, which hands out no memory, a pattern is written into each block and read back before its free, so that overlapping blocks are caught. Others check that a reset subject replays to the same numbers, that a region starts on a page, that a region too small is reported, that `space`, `plain`, `pass` and `each` make the same calls, the percentile rule, and that every program computes its value with explicit frees, cycles included.

#output(title: "cargo test -p green-tea-measure")[
```
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

== Limits

- *The `time` command's page check fails intermittently.* The same command, run 100 times, ended in `FAIL` 10 times, and in three more batches of 100 it failed 8, 7 and 4 times. In most failures inspected, one page was charged to the first allocator in the table, and one failure mapped two. Its cause wasn't found: touching the `whole` array before the count and adding a `pass` to the warm-up (both tried on a private copy) didn't remove it. A recorded failure:

#output(title: "green-tea-measure time shell.gtt --touch --only tlsf,pools", size: 5.7pt)[
```
  tlsf            35.3      33      50      64      75        3502                18.04   18.46   20.78             1
  pools           25.7      23      34      42      51        3938                 7.68    7.93    9.05             0
FAIL: 1 pages were mapped during timed passes: those passes timed the operating system too
```
]

- *`instructions` counts one `reset`.* `plain` resets the subject before its pass (`replay.rs:163`), inside the counted function, so building the allocator's tables is charged to the events. On a trace of one allocation, the charge shows:

#output(title: "green-tea-measure instructions one.gtt", size: 6.5pt)[
```
  allocator    instructions  less the loop's  per event
  bump                  107               20      20.00
  tlsf                 4319             4232    4232.00
  slabs               28162            28075   28075.00
  shards              18130            18043   18043.00
```
]

  On the 20,483 events of the session this is at most 0.64% of a subject's count (`slabs`). On a short trace it is most of the number.
- *The floors in `lisp instructions` differ between heaps.* The explicit floor for `list` is 14,152,095 instructions, and the mark-sweep floor is 31,985,637; over the five programs the second is 2.0 to 2.3 times the first. `MarkHeap` checks every object and field access, and a release `Explicit` doesn't. The "over the floor" columns leave that difference out, so they compare the cost of reclamation and not the cost of running the program on each heap.
- *`left` isn't what its comment says.* `Lifetimes::left` is documented as the objects "still reachable when the run ended" (`oracle.rs:31`), and the table's column is "reachable at the end". `finish` counts those not yet found dead, and deaths are found only before an allocation, so objects that die after the last one are counted, and the last object allocated always is.
- *Failures are reported as a particular failure.* `instructions` prints "refused an allocation" for any error from valgrind (`main.rs:332`), and `lisp instructions` prints "the program doesn't fit" for any failed run (`main.rs:764`).
- The comment on `Summary` says it keeps "the samples at five ranks" (`stats.rs:15`), and it keeps six.
- `subjects::describe` has no caller, though its comment says it is for a table's caption (`subjects.rs:74`).
- `Shards::reset` leaks an `Arena` each time (`subjects.rs:623`), and `Slabs` leaks its frame table once. A subject lives as long as the program.
- Any error prints the whole usage after it, including a trace refused by `--pins` (`main.rs:92`).
- The subjects run from one thread. `caches` and `shards` are measured with their locks and remote-free lists uncontended, so this says nothing about what they do across CPUs (@crate-smp, @crate-shard).
- Timings are of one machine under whatever load it had, which is why each table prints the load average.

Bytes, instructions and nanoseconds, on the same trace. @crate-xtask is what boots the kernel that recorded it.
