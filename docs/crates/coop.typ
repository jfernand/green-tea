#import "../lib.typ": *

= green-tea-coop <crate-coop>

`green-tea-coop` is one kernel binary that holds eighteen kernels: a demo of cooperative tasks on the display, and seventeen tests. Each test exercises a crate of the workspace on the bare machine, prints what it measured, and ends with a line that starts `PASS:` or `FAIL:`, which is what `cargo xtask kernel` looks for.

#spec(
  ("Source", [#repo("green-tea-coop/src/"), 14 files, 6,050 lines]),
  ("Depends on", [`green-tea-bare` (@crate-bare), `green-tea-lisp` (@crate-lisp), `green-tea-shard` (@crate-shard), `green-tea-smr` (@crate-smr)]),
  ("Used by", [nothing: it is a binary, run by `cargo xtask kernel` (@crate-xtask)]),
  ("Tests", [it is the tests. All recordings below are from riscv64 unless they say otherwise]),
)

The choice of kernel is made outside the binary. `xtask` passes the name in a `fw_cfg` file, `opt/green-tea/test`, or on the `pc` board as `test=` on GRUB's command line. `Board::test_name` reads it, and `kmain` matches it:

#srcfile("green-tea-coop/src/main.rs", lines: ((54, 65), (77, 81),), notes: ((58, [No name means the demo.]), (60, [The `pc` board has no virtio, so `disk` isn't compiled for it.]), (77, [A name the binary doesn't know is a panic, which `xtask` reports as a failure.]),))[```rust
    let fb = boot("green-tea-coop");
    let mut test = [0u8; 16];
    let len = Board::test_name(&mut test);
    match &test[..len] {
        b"" => demo(fb),
        b"overflow" => overflow_test(fb),
        #[cfg(not(feature = "board-pc"))]
        b"disk" => disk_test(fb),
        b"memmap" => memmap_test(fb),
        b"pool" => pool_test(fb),
        b"fit" => fit::test(fb),
        b"frag" => frag::test(fb),
// …
        other => panic!("unknown test {:?}", core::str::from_utf8(other)),
    }
    println!("halting");
    Board::halt()
}
```]

#dtable(columns: (auto, auto, 1fr), ("Kernel", "File", "What it exercises"),
  [`coop`], [`main.rs`], [tasks that yield, drawn],
  [`overflow`], [`main.rs`], [guard pages (riscv64, x86_64)],
  [`disk`], [`main.rs`], [the virtio-blk driver (not on `pc`)],
  [`memmap`], [`main.rs`], [the linker script],
  [`pool`], [`main.rs`], [`green-tea-pool`, and the stack pool],
  [`fit`], [`fit.rs`], [`Fit`, and the kernel's heap],
  [`frag`], [`frag.rs`], [fragmentation, and the adversary],
  [`tlsf`], [`tlsf.rs`], [`Buddy` and `Tlsf`],
  [`slab`], [`slab.rs`], [page frames, and slab caches],
  [`pages`], [`pages.rs`], [demand paging (riscv64, x86_64)],
  [`smp`], [`smp.rs`], [four CPUs on one heap, and per-CPU caches],
  [`lisp`], [`lisp.rs`], [reference counting],
  [`shard`], [`shard.rs`], [a heap of pages per CPU, with no lock],
  [`gc`], [`gc.rs`], [mark-sweep, mark-compact and copying],
  [`smr`], [`smr.rs`], [safe memory reclamation],
  [`gen`], [`generations.rs`], [a generational heap],
  [`incremental`], [`incremental.rs`], [an incremental heap, paced by a timer],
  [`safety`], [`safety.rs`], [`Hardened`, and heap corruption],
)

Each of the thirteen files after `main.rs` has one public function, `test`, which takes the framebuffer. `frag` also exports `random_trace`, the random workload as a trace, which `tlsf` and `safety` replay too. A passing test turns the status bar green with `Screen::finish`.

== `coop`: tasks that yield

Five workers each grow a bar, one step per turn, and yield. The scheduler's hook moves the highlight to the task about to run and waits 150 ms, so that the switches can be watched. Worker 0 spawns worker 4 during its third turn:

#srcfile("green-tea-coop/src/main.rs", lines: ((132, 144),), notes: ((135, [The only way a task gives up the CPU here.]),))[```rust
fn worker(mut painter: Painter, column: usize) {
    for turn in 1..=TURNS[column] {
        if turn > 1 {
            yield_now(); // the first turn starts as soon as the task is first resumed
        }
        painter.bar(column, (turn - 1) * STEP + 2, turn * STEP, COLORS[column]);
        painter.label(column, turn);

        if column == 0 && turn == SPAWN_AFTER_TURN {
            spawn(move || worker(painter, COLUMNS - 1));
        }
    }
}
```]

The workers take 6, 10, 14, 18 and 8 turns, and the main task is resumed once, so the kernel knows how many switches to expect:

#output(title: "cargo xtask kernel --kernel coop --target riscv64")[
```
running 5 workers; expecting 57 switches
all tasks finished after 57 switches (expected 57)
PASS: 57 switches, as expected
```
]

== `overflow`: the guard page

A task recurses with a frame of 256 bytes, well under a page, so that it can't skip the guard:

#srcfile("green-tea-coop/src/main.rs", lines: ((192, 200),))[```rust
#[inline(never)]
fn recurse(depth: u64) -> u64 {
    DEPTH.set(depth);
    let frame = core::hint::black_box([depth as u8; 256]);
    if depth == u64::MAX {
        return 0;
    }
    recurse(depth + 1) + u64::from(frame[0])
}
```]

The test replaces the overflow reaction with `stack::on_overflow`. It passes only if the fault's address is inside *this* task's guard page. Without guard pages the recursion would run on into the next slot, silently, so the kernel doesn't run on aarch64.

#output(title: "cargo xtask kernel --kernel overflow --target riscv64")[
```
overflowing a 16 KiB task stack
PASS: overflowed after 26 frames, into the task's guard page at 0x80a05ff0
```
]

== `disk`: virtio-blk

`xtask` attaches a 64-sector image in which every byte is a function of where it is, so a sector read from the wrong place doesn't match:

#srcfile("green-tea-coop/src/main.rs", lines: ((210, 212),))[```rust
fn pattern(sector: u64, offset: usize) -> u8 {
    (sector as u8).wrapping_mul(31) ^ (offset as u8) ^ (offset >> 8) as u8
}
```]

The test reads every sector, four at a time, and checks that the driver refuses a read past the end and a read of part of a sector. It overwrites sector 3 with the pattern inverted, flushes, and reads sectors 2 to 4 back, so that a write that strayed into a neighbour would show. The same test covers both transports:

#output(title: "cargo xtask kernel --kernel disk --target riscv64, then x86_64")[
```
virtio-blk over MMIO: 64 sectors (32 KiB), id "green-tea-disk", write cache
PASS: read 64 sectors over virtio-MMIO, and wrote one back
virtio-blk over PCI: 64 sectors (32 KiB), id "green-tea-disk", write cache
PASS: read 64 sectors over virtio-PCI, and wrote one back
```
]

== `memmap`: the linker script from inside

`memmap_test` prints each section (the output is in @crate-bare), and `check_memmap` verifies that each starts on its alignment, that they are in order without overlap, and that the ones the script sizes have that size. Then eight *witnesses*, one address of each kind, must be where they belong:

#srcfile("green-tea-coop/src/main.rs", lines: ((357, 366),))[```rust
let witnesses = [
    ("a function", memmap_test as fn(Framebuffer) as usize, ".text", 1),
    ("a constant static", &raw const CONSTANT as usize, ".rodata", 1),
    ("an initialised static", &raw const INITIALISED as usize, ".data", align_of::<AtomicUsize>()),
    ("a zeroed static", &raw const ZEROED as usize, ".bss", align_of::<AtomicUsize>()),
    ("a local of kmain's", &raw const local as usize, ".stack", 1),
    ("a Box<u64>", &raw const *boxed as usize, ".heap", align_of::<u64>()),
    ("a Box<Page>", &raw const *page as usize, ".heap", align_of::<Page>()),
    ("a local of a task's", TASK_LOCAL.get(), ".stacks", 1),
];
```]

#output(title: "cargo xtask kernel --kernel memmap --target riscv64")[
```
  address    in       what
  0x802950ea .text    a function
  0x803058b0 .rodata  a constant static
  0x8031bd98 .data    an initialised static
  0x8031da78 .bss     a zeroed static
  0x8047cfef .stack   a local of kmain's
  0x80482010 .heap    a Box<u64>
  0x80483000 .heap    a Box<Page>
  0x80a04f46 .stacks  a local of a task's
PASS: 7 sections in order and aligned; 8 addresses where they should be
```
]

== `pool`: bitmap against free list

The same takes and gives go to both of `green-tea-pool`'s pools (@crate-pool). A slot given back twice breaks `give`'s contract on purpose:

#srcfile("green-tea-coop/src/main.rs", lines: ((464, 467),))[```rust
let slot = base + 7 * POOL_SLOT;
let _ = unsafe { pool.give(slot) };
let second_give = unsafe { pool.give(slot) };
let after_second_give = [0; 2].map(|_| pool.take().map(|slot| (slot - base) / POOL_SLOT));
```]

#output(title: "cargo xtask kernel --kernel pool --target riscv64")[
```
a pool of 8 slots:        bitmap                     free list
  taken from new:          [0, 1, 2, 3, 4, 5, 6, 7]   [0, 1, 2, 3, 4, 5, 6, 7]
  a ninth take refused:    true                       true
  after giving 2, 5, 0:    [0, 2, 5]                  [0, 5, 2]
  7 given back twice:      Err(NotTaken)              Ok(())
  the next two takes:      [Some(7), None]            [Some(7), Some(7)]
  after a write to a free slot, hands out a block outside the pool:
                           false                      true
PASS: 1000 tasks ran on one stack; the bitmap caught a double give, the free list didn't
```
]

The free list has no way to notice a second give, and then hands slot 7 to two callers. Its links live in the free slots, so a write after a free can make it hand out an address outside the pool. The second half runs a thousand tasks one after another, all of which must run on one stack and leave the pool full.

== `fit`: the allocator, word by word

`fit` runs six stages: `words` prints a small region a word to a line, `merging` frees blocks one by one, `policies` gives one request to three policies, `mistakes` makes the caller's errors, `workloads` runs 20,000 operations under each policy and order, and `kernel_heap` checks that a thousand tasks leave the kernel's heap as they found it. The block layout that @crate-fit describes, in memory:

#output(title: "cargo xtask kernel --kernel fit --target riscv64")[
```
after free(a), a's block:
  0x80482018  0x0000000000000030
  0x80482020  0x0000000080482078
  0x80482028  0x0000000000000000
  0x80482030  0x6161616161616161
  0x80482038  0x0000000000000000
  0x80482040  0x0000000000000030
a request for 64 bytes, which takes a block of 80:
  first fit  looked at 2 free blocks: (64) 32 80 (176) 32 (96) 32 (3568)
  next fit   looked at 1 free blocks: (64) 32 (256) 32 (96) 32 80 (3488)
  best fit   looked at 4 free blocks: (64) 32 (256) 32 96 32 (3568)
```
]

The tags now read `0x30` where they read `0x31`, the first word of the caller's data has become the link to the next free block, and the second is the back link. The last of the caller's bytes are still there.

== `frag`: fragmentation

`holes` shows the problem in a kilobyte: eight blocks of 128 bytes, every other one freed, and a request for 200 bytes refused with 512 free. `adversaries` runs `green-tea-fit`'s adversary against all six policies, and `random_workload` replays `random_trace` against them:

#srcfile("green-tea-coop/src/frag.rs", lines: ((63, 71),))[```rust
while let Some(addr) = fit.allocate(100, 8) {
    blocks.push(addr);
}
println!("a region of {} bytes, filled with {} blocks of 100 bytes:", fit.capacity(), blocks.len());
println!("  {}", map(&fit));
for addr in blocks.iter().step_by(2) {
    // SAFETY: each address came from `allocate`, and is freed once.
    unsafe { fit.free(*addr) }.map_err(|_| "an allocated block couldn't be freed")?;
}
```]

#output(title: "cargo xtask kernel --kernel frag --target riscv64")[
```
every other block freed:
  (128) 128 (128) 128 (128) 128 (128) 128
  512 bytes free in 4 blocks, the largest 128; 512 bytes in use
  a request for 200 bytes: refused
the footprint at the end, policy by policy:
  first fit  LIFO         388384     5.93
  best fit   address      388384     5.93
PASS: an adversary holding 65536 bytes made every policy need 388384 bytes, 5.93 times as much; a random workload made them need 23.0% to 864.9% over
```
]

== `tlsf`: the allocators that don't search

`buddies` draws a buddy system splitting and merging, `blocks` shows the block each allocator makes for a request, and `lists` shows which TLSF list a free block goes on. `long_searches` builds the worst case for a search, thousands of small holes in front of the one block that fits, and times 100 allocations and frees:

#srcfile("green-tea-coop/src/tlsf.rs", lines: ((227, 233),))[```rust
let before = heap.stats();
let start = Board::ticks();
for _ in 0..PAIRS {
    let addr = heap.allocate(4096, 8).ok_or("the region is too small for the large block")?;
    unsafe { heap.free(addr) }.map_err(|_| "an allocated block couldn't be freed")?;
}
let ticks = Board::ticks() - start;
```]

#output(title: "cargo xtask kernel --kernel tlsf --target riscv64")[
```
  holes  allocator               looked at  walked past  microseconds
   1000  first fit, by address        1001         1000       1114.8
   1000  best fit, by address         1001         1000       1510.3
   1000  buddy system                    5            0         19.4
   1000  TLSF                            1            0         18.7
```
]

The verdict is on the counts, which are exact. The microseconds change from run to run. The test ends by checking that the kernel's own heap, since boot, never looked at more than one block for an allocation.

== `slab`: page frames and caches

The test works up from page frames (@crate-kmem). It frees every other page of the 16 MiB to show memory that is half free with no two pages together, then builds a slab cache of 200-byte objects on top, and ends with the cache of task records:

#output(title: "cargo xtask kernel --kernel slab --target riscv64")[
```
  2042 pages free, 2042 in use; a request for two pages together: refused
a slab is a page: 19 buffers, a record of 40 bytes at the end, and 104 bytes to spare
  slabs:                     full partial empty objects constructed destroyed pages pages free
  40 objects                    2       1     0      40          57         0     3       4081
  all 40 freed                  0       0     3       0          57         0     3       4081
  40 objects again              2       1     0      40          57         0     3       4081
  all freed, and reaped         0       0     0       0          57        57     0       4084
the kernel's cache of task records: a record is 488 bytes, a buffer 496, and a slab holds 8
1000 tasks, one after another, every one with its locals at 0x80a09f2e:
  1001 records allocated, 8 constructed, in 1 slab; 8 stacks taken from the pool
```
]

Objects freed stay constructed: forty allocated twice were constructed 57 times, once per buffer of three slabs. `pinned` keeps ten objects out of 190, once one per slab and once all in one slab. The first keeps ten pages and the second keeps one:

#srcfile("green-tea-coop/src/slab.rs", lines: ((434, 442),))[```rust
// Keep ten: the first of each slab, or the first ten of the first slab.
let mut kept = Vec::new();
for (index, object) in objects.into_iter().enumerate() {
    if if spread { index.is_multiple_of(per_slab) } else { index < 10 } {
        kept.push(object);
    } else {
        free(&mut cache, object)?;
    }
}
```]

#output(title: "cargo xtask kernel --kernel slab --target riscv64")[
```
190 objects in 10 slabs, all freed but 10, one in each slab: slabs 10 partly used, 0 empty
  reaped, the cache gives back 0 pages and keeps 10
190 objects in 10 slabs, all freed but 10, all in one slab: slabs 1 partly used, 9 empty
  reaped, the cache gives back 9 pages and keeps 1
```
]

== `pages`: demand paging

`run` follows a megabyte through allocation, writing, freeing and trimming (the table is in @crate-bare). `walks` prints the page-table entries for six addresses. `boundary` is the delicate case: a free block whose header is the last word of a page, so that its links are on the next page, which `trim` must not release:

#srcfile("green-tea-coop/src/pages.rs", lines: ((163, 166),), notes: ((164, [A filler that ends on the last word of a page, so that the next block's header does too.]),))[```rust
let probe = alloc(layout(PAGE)) as usize;
let filler_size = (PAGE - probe % PAGE) % PAGE + PAGE;
let filler = alloc(layout(filler_size));
let blocks = [FREED, PAGE, FREED, PAGE].map(|size| alloc(layout(size)));
```]

`pinned` then frees most of a megabyte of 48-byte blocks and asks what can be given back:

#output(title: "cargo xtask kernel --kernel pages --target riscv64")[
```
two free blocks of 12288 bytes, each with its header in the last word of a page: trimming released 2 pages
  blocks kept            live bytes   would fill   pages mapped   pages released
  one in 64        256        12288            4            256                1
  one in 128       128         6144            2            128              128
  one in 256        64         3072            1             64               64
  one in 1024       16          768            1             16               48
PASS: of 1024 pages reserved, 290 were the most mapped at once; 813 page faults, 779 pages released
```
]

With one block kept in 64, 12,288 live bytes would fill 4 pages and hold 256.

== `smp`: four CPUs and a lock

The kernel starts the other three CPUs (QEMU is given `-smp 4`), and all four do the same work. First, an addition without a lock and with one:

#srcfile("green-tea-coop/src/smp.rs", lines: ((180, 187),), notes: ((184, [A load and a store, as `counter += 1` compiles to. Another CPU's store can come between them, and is then overwritten.]),))[```rust
cpus::run(|_| {
    for _ in 0..ADDS {
        // A load and a store, as `counter += 1` compiles to. Another CPU's store can
        // come between them, and is then overwritten.
        let seen = PLAIN.load(Ordering::Relaxed);
        PLAIN.store(core::hint::black_box(seen) + 1, Ordering::Relaxed);
    }
});
```]

#output(title: "cargo xtask kernel --kernel smp --target riscv64")[
```
4 CPUs are running: the one that booted, and 3 it started
each CPU adds 1 to a counter 1000000 times:
  with a lock: the counter is 4000000, and the lock was taken 4000000 times
  this run: without a lock the counter is 1878532; with one, 3898024 acquisitions waited
each CPU makes 20000 allocations and frees, of 1 to 512 bytes, holding up to 96:
  the kernel's heap, behind its lock:  80128 operations, and the lock was taken 80128 times
  this run: 64003 of those waited for another CPU; 522 ms
  a cache for each CPU:                80128 operations, and the lock was taken 2680 times
  this run: 754 of those waited for another CPU; 337 ms
```
]

Without a lock, more than half of the four million additions were lost. A cache per CPU (@crate-smp) takes the same 80,128 operations to the lock 2,680 times, in batches of eight. The lines that start `this run:` differ between runs, and the verdict depends only on the others.

== `lisp`: reference counting

`green-tea-lisp`'s interpreter (@crate-lisp) runs on a heap that counts references. `session` types four definitions and follows one pair's count, `overwrite` replaces a list with its tail, `programs` runs the five programs, `lists` frees a long list three ways, and `cycles` runs the two programs that make cycles in a heap too small for what they leak:

#output(title: "cargo xtask kernel --kernel lisp --target riscv64")[
```
  program    value  allocated     freed    left  peak words  words touched  increments  decrements
  list       50500       8190      8190       0         614            614      105712      113902
  fib          610       2002      2002       0         134            143       39455       41457
  alist        250      14309     14309       0         766            811      229932      244241
  ring        7200       9130      8330     800        2772           2772      121037      129367
  closure     5500       2877      2556     321        1326           1326       39921       42279
  recursive: 16 pairs, freed by a recursion 16 deep
    stack: 4320 bytes, 288 for each pair; a task's 16384 would hold 56 pairs
  queued: 10000 pairs; the pop freed 10000, and the recursion was 0 deep
  ring     counts alone:    Err(HeapFull) after 2567 allocations, 2351 freed
  ring     trial deletion:  Ok("7200") after 9130 allocations, 9130 freed; 67 searches freed 800
```
]

A recursive free of a list needs 288 bytes of stack per pair, so a 16 KiB task stack holds 56 pairs of it. `ring` leaks 800 objects to counting alone, and trial deletion finds them.

== `shard`: a heap per CPU, with no lock

All four CPUs do the same work twice: through a cache each in front of a locked heap, and through a heap of pages each (@crate-shard). The second kind of work is the hard one, where every block a CPU allocates is freed by another:

#srcfile("green-tea-coop/src/shard.rs", lines: ((188, 195),), notes: ((189, [Of the blocks `from` left, the third that belongs to this CPU.]), (192, [`inspect` checks that the block still holds `from`'s bytes.]),))[```rust
for from in (0..CPUS).filter(|&from| from != cpu) {
    for j in (0..PASSED).filter(|j| (from + 1 + j % 3) % CPUS == cpu) {
        let addr = LEFT[before][from][j].load(Ordering::Acquire);
        let size = SIZES[before][from][j].load(Ordering::Acquire);
        inspect(addr, size, from);
        free(addr, size);
        done += 1;
    }
```]

#output(title: "cargo xtask kernel --kernel shard --target riscv64")[
```
each CPU allocates 48 blocks and leaves them for the other three to free, 200 times:
  a cache for each CPU:          38400 allocations and as many frees, and the lock was taken 1992 times
  a heap of pages for each CPU:  38400 allocations and as many frees, no lock, and 38400 frees of another CPU's block
    when every CPU had collected its pages, 38400 blocks had come back to the CPU that allocated them, and 0 were out
```
]

== `gc`: the heaps that trace

The same Lisp on three heaps: mark-sweep, mark-compact and copying. The test types the session again, runs the cycle programs that broke reference counting, measures the five programs on each heap, varies the heap size, and shows where objects are after a collection:

#output(title: "cargo xtask kernel --kernel gc --target riscv64")[
```
  ring     mark-sweep    Ok("7200") after 9130 allocations and 58 collections; 0 left
  ring     mark-compact  Ok("7200") after 9130 allocations and 55 collections; 0 left
  ring     copying       Ok("7200") after 9130 allocations and 275 collections; 0 left
a list of 4 pairs, made after 4 pairs of garbage, in a heap of 64 words: where its pairs are
  mark-sweep    before: 22 19 16 13; after: 22 19 16 13; the next pair allocated is at 1
  mark-compact  before: 22 19 16 13; after: 10 7 4 1; the next pair allocated is at 13
  copying       before: 21 18 15 12; after: 32 35 38 41; the next pair allocated is at 44
```
]

`missed_root` shows what a tracing heap demands of its user. A reference is kept in a Rust variable, where the heap can't see it, across a collection:

#srcfile("green-tea-coop/src/gc.rs", lines: ((341, 348),))[```rust
let borrowed: Obj = heap.top().as_obj().ok_or("no pair")?;
let before = (heap.field(borrowed, 0), heap.field(borrowed, 1));
heap.pop(); // the variable is now the only reference
heap.collect();
heap.push(Value::int(3));
heap.push(Value::int(4));
heap.alloc(Kind::Pair).map_err(|_| "the heap is full")?;
let after = (heap.field(borrowed, 0), heap.field(borrowed, 1));
```]

#output(title: "cargo xtask kernel --kernel gc --target riscv64")[
```
a reference to the pair (1 . 2), kept in a variable across a collection and one allocation:
  now it is a reference to the pair (3 . 4)
```
]

== `smr`: safe memory reclamation

A lock-free stack (@crate-smr) is stopped in the middle of a pop, between reading the head and its compare-and-swap, and another thread does what it could do there:

#srcfile("green-tea-coop/src/smr.rs", lines: ((126, 135),))[```rust
let popped = stack.pop_paused(mine, || {
    rounds += 1;
    if rounds == 1 {
        stack.pop(theirs);
        stack.pop(theirs);
        look(theirs);
        other_node = other.push(9usize) as usize;
        new_node = stack.push(4) as usize;
    }
});
```]

With nodes freed at once this is the *ABA problem*. The heap reuses the address, and the compare-and-swap succeeds on a stale value:

#output(title: "cargo xtask kernel --kernel smr --target riscv64")[
```
  nodes freed at once:
    the stack is +64 (3) -> +32 (2) -> +0 (1); the pop has read head = +64 and next = +32
    the other thread pops 3 and 2, and both nodes are freed; its 9 goes in a node at +32, and its 4 in one at +64
    the compare-and-swap of head from +64 to +32 succeeds; the pop returns 4
    the stack is +32 (9), which is the other stack's node; 1 is on no stack
  nodes freed by epochs:
    the other thread pops 3 and 2, and both nodes wait in a limbo bag; its 9 goes in a node at +176, and its 4 in one at +208
    the compare-and-swap of head from +64 to +32 fails, and the pop starts again; the pop returns 4
    the stack is +0 (1)
```
]

The test then runs four CPUs on one stack and one queue under both schemes, replaces a shared value while others read it, and runs a queue of jobs that counts primes. One stage separates the two schemes. CPU 3 stops in the middle of a pop while the others work:

#output(title: "cargo xtask kernel --kernel smr --target riscv64")[
```
  freed by epochs          6000 nodes were retired, 0 freed, and 6000 waited: 192000 bytes of nodes
  freed by hazard pointers 6000 nodes were retired, and at most 96 waited: 3072 bytes of nodes
```
]

== `gen`: two generations

The module is `generations`. The Lisp runs on a heap with a nursery in front of an old space, a write barrier and a remembered set. The opening session follows one list as it is promoted, and a store puts a young object into an old one:

#output(title: "cargo xtask kernel --kernel gen --target riscv64")[
```
> (set-cdr! keep (cons 3 nil))
nil
  allocated  7; minor collections 1, objects copied  1; stores recorded 1, remembered set 1
  keep is at word   14, in the old space, and its second pair at word 4048, in the nursery
a list of 10000 pairs that is kept, and 100 pairs of garbage made after it:
  a minor collection:                         1 root slots read,  0 remembered scanned,     0 traced,     0 walked
  a minor and a major collection:             2 root slots read,  0 remembered scanned, 10001 traced, 30003 walked
PASS: seven programs on two generations: 7632 of 47009 objects left the nursery alive, and nothing was left
```
]

A minor collection traces nothing of the 10,000 old pairs. `missed_store` writes an old-to-young reference with the barrier and without it:

#srcfile("green-tea-coop/src/generations.rs", lines: ((355, 361),), notes: ((358, [The store the minor collection won't hear about.]),))[```rust
if barrier {
    heap.set_field(old, 0, heap.top());
} else {
    heap.store_without_barrier(old, 0, heap.top());
}
heap.pop(); // the old pair's field is now the only reference to (1 . 2)
heap.minor();
```]

#output(title: "cargo xtask kernel --kernel gen --target riscv64")[
```
  stored by set_field, the field refers to the pair (1 . 2)
  stored without the barrier, it refers to the pair (3 . 4)
```
]

== `incremental`: marking in steps

The heap marks and sweeps a few objects at a time, between the program's steps. `hidden` is the classic failure, a reference moved from an unmarked object to a marked one while the collector is half way through:

#output(title: "cargo xtask kernel --kernel incremental --target riscv64")[
```
  no barrier:
    after the step:                A black, B grey, C white
    the program stores C in A:     A black, B grey, C white
    and overwrites it in B:        A black, B grey, C white
    the cycle ends:                C is freed; one more pair is allocated, and A refers to (3 . 4)
  insertion:
    the program stores C in A:     A black, B grey, C grey
    the cycle ends:                C is kept; the barrier ran 2 times and shaded 1 object
  deletion:
    and overwrites it in B:        A black, B grey, C grey
    the cycle ends:                C is kept; the barrier ran 2 times and shaded 1 object
```
]

This is the only test that takes interrupts. A timer every 200 µs sets a flag, and `alloc` does a step when it finds the flag set. The handler touches nothing of the heap, which the interrupted code may be half way through changing:

#srcfile("green-tea-coop/src/incremental.rs", lines: ((281, 289),))[```rust
fn on_tick() {
    TICKS.set(TICKS.get() + 1);
    let now = Board::ticks();
    if !PENDING.get() {
        PENDING.set(true);
        TICKED_AT.set(now);
    }
    Board::set_deadline(now + PERIOD_US * Board::ticks_per_ms() / 1000);
}
```]

#output(title: "cargo xtask kernel --kernel incremental --target riscv64")[
```
  a time, not a count: whole                   59 cycles,    59 steps of at most  922 units,  0 stalls of at most    0 units
  a time, not a count: 4 units an allocation   58 cycles, 14798 steps of at most   14 units,  0 stalls of at most    0 units
  a time, not a count:   6747 ticks, 6610 found by an allocation, after 66 us on average and 579 us at most
PASS: 212 cycles in 61497 steps of at most 28 units lost nothing; then 13505 ticks of the timer
```
]

The longest step falls from 922 units to 14.

== `safety`: heap corruption

First the bug, in two tasks that share a `Fit`. One writes to a block after freeing it, with a value that reads as a link to a forged free block inside its own buffer:

#srcfile("green-tea-coop/src/safety.rs", lines: ((110, 120),), notes: ((119, [`forge` lays out what looks like a free block, inside the buffer.]), (120, [The write after the free: the freed block's first word is the allocator's link.]),))[```rust
let line = shared().allocate(48, 8).unwrap();
let buffer = shared().allocate(96, 8).unwrap();
LOGGER.set([line, buffer]);
println!("  logger: a line at {line:#x}, and a buffer of 96 bytes at {buffer:#x}");
unsafe { shared().free(line).unwrap() };
// The write after the free. The line's first word is now where the allocator keeps
// its link to the next free block, and what is written there is the address of the
// buffer's second word. The buffer's contents are the worst they could be: they read
// as a free block of 64 bytes.
forge(buffer + WORD, line - WORD);
put(line, 0, buffer + WORD);
```]

The other task then allocates, and is handed memory that belongs to the first:

#output(title: "cargo xtask kernel --kernel safety --target riscv64")[
```
  logger: frees the line, then writes 0x80482068 to its first word
  clerk:  a record at 0x80482020 holding 1000, and one at 0x80482070 holding 2000
  logger: fills its buffer with the letter a
  clerk:  its records now hold 1000 and 0x6161616161616161
the clerk's second record is inside the logger's buffer: true
```
]

`mistakes` then makes six errors against the allocator with one defence more each time, and prints which call caught each. @crate-fit quotes one of the six. The test also measures when a freed block is reused (`reuse`) and what the defences cost (`costs`).

== Limits

- `overflow` and `pages` print `FAIL` on aarch64, which has neither guard pages nor paging, and `xtask` doesn't run them there.
- `smp`, `shard` and `smr` need four CPUs, and fail with a message on any other number.
- Lines marked `this run:` or `a time, not a count:` differ between runs, and no verdict depends on them, except that `incremental` needs a tick to be seen.
- `disk` isn't built for the `pc` board, which has no virtio (@bare-limits).

Eighteen kernels, one binary, and a line that says `PASS`. @crate-preempt takes the `yield_now` away.
