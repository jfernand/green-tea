//! A bare-metal kernel for QEMU that runs green-tea's cooperative tasks and draws every
//! context switch on a ramfb display. Runs on riscv64 and aarch64 (`virt`) and x86_64
//! (`q35`): `cargo xtask kernel --kernel coop`.
//!
//! Tasks switch only when they call `yield_now`: no interrupts, one CPU. Everything else
//! (boot, drivers, tasks, scheduler) is green-tea-bare; this is just the demo.
//!
//! With the fw_cfg file `opt/green-tea/test` set to a test's name, it runs that test
//! instead of the demo: `kmain` matches the name. `cargo xtask kernel --kernel <name>` sets
//! the file, and `cargo xtask kernel --help` says what each test does. The first tests
//! (overflow, disk, memmap, pool) are in this file; each later one has a file of its own.

#![no_std]
#![no_main]

use green_tea_bare::ramfb::Framebuffer;
use green_tea_bare::runtime::{Switch, run, spawn};
use green_tea_bare::screen::{COLORS, COLUMNS, Painter, Screen};
use green_tea_bare::task::{current_guard, yield_now};
use green_tea_bare::virtio::{self, Transport};
use green_tea_bare::virtio_blk::{self as blk, Blk, SECTOR_SIZE};
use green_tea_bare::pool::{Bitmap, FreeList, GiveError, Pool};
use green_tea_bare::{Board, Global, Platform, boot, delay_ms, heap, memory, println, stack};

extern crate alloc;

mod fit;
mod frag;
mod safety;
mod shard;
mod slab;
mod pages;
mod smp;
mod lisp;
mod gc;
mod incremental;
mod tlsf;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use core::sync::atomic::AtomicUsize;

/// Called by the platform's `_start`, on the boot stack, with .bss zeroed.
#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    let fb = boot("green-tea-coop");
    let mut test = [0u8; 16];
    let len = Board::fw_cfg().read_file("opt/green-tea/test", &mut test).unwrap_or(0);
    match &test[..len] {
        b"" => demo(fb),
        b"overflow" => overflow_test(fb),
        b"disk" => disk_test(fb),
        b"memmap" => memmap_test(fb),
        b"pool" => pool_test(fb),
        b"fit" => fit::test(fb),
        b"frag" => frag::test(fb),
        b"pages" => pages::test(fb),
        b"lisp" => lisp::test(fb),
        b"gc" => gc::test(fb),
        b"incremental" => incremental::test(fb),
        b"tlsf" => tlsf::test(fb),
        b"slab" => slab::test(fb),
        b"safety" => safety::test(fb),
        b"smp" => smp::test(fb),
        b"shard" => shard::test(fb),
        other => panic!("unknown test {:?}", core::str::from_utf8(other)),
    }
    println!("halting");
    Board::halt()
}

// ---- The demo -------------------------------------------------------------------------
//
// Five columns, one per worker task. Each time a worker runs it grows its bar by one step
// and yields. Just before each resume, the scheduler's hook moves the highlight to the
// task about to run, bumps the switch counter, and waits a moment so the switches are
// slow enough to watch.

const STEP: usize = 20; // bar growth per turn
const SWITCH_DELAY_MS: u64 = 150;

/// Turns per worker. Worker 0 spawns worker 4 after its third turn.
const TURNS: [usize; COLUMNS] = [6, 10, 14, 18, 8];
const SPAWN_AFTER_TURN: usize = 3;

fn demo(fb: Framebuffer) {
    let mut screen = Screen::new(fb);

    // The main task only spawns the first four workers, so it is resumed once.
    let expected = 1 + TURNS.iter().sum::<usize>();
    println!("running {COLUMNS} workers; expecting {expected} switches");

    let painter = screen.painter();
    static SWITCHES: Global<usize> = Global::new(0);
    run(
        move || {
            for column in 0..COLUMNS - 1 {
                spawn(move || worker(painter, column));
            }
        },
        |switch| {
            let Switch::To(id) = switch else { return }; // no task here sleeps, so never idle
            let switches = SWITCHES.get() + 1;
            SWITCHES.set(switches);
            screen.switch_to(id, switches);
            delay_ms(SWITCH_DELAY_MS);
        },
    );

    let switches = SWITCHES.get();
    screen.finish(switches);
    println!("all tasks finished after {switches} switches (expected {expected})");
    // The line cargo xtask kernel looks for.
    if switches == expected {
        println!("PASS: {switches} switches, as expected");
    } else {
        println!("FAIL: {switches} switches, expected {expected}");
    }
}

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

// ---- The overflow test ----------------------------------------------------------------
//
// The bare-metal twin of green-tea-v2's `overflow_hits_the_guard_page`: a task recurses
// until its stack runs out. Passing needs the fault, and at an address in that task's
// own guard page: without guard pages it would run on into the next slot down, silently.

/// The overflowing task's guard page, and how deep it got.
static GUARD: Global<(usize, usize)> = Global::new((0, 0));
static DEPTH: Global<u64> = Global::new(0);

fn overflow_test(fb: Framebuffer) {
    if !Board::GUARD_PAGES {
        println!("FAIL: {} has no guard pages", Board::NAME);
        return;
    }
    static SCREEN: Global<Option<Framebuffer>> = Global::new(None);
    SCREEN.set(Some(fb));
    stack::on_overflow(|addr| {
        let (start, end) = GUARD.get();
        let depth = DEPTH.get();
        if let Some(fb) = SCREEN.get() {
            Screen::new(fb).finish(depth as usize); // green, with the depth reached
        }
        if (start..end).contains(&addr) {
            println!("PASS: overflowed after {depth} frames, into the task's guard page at {addr:#x}");
        } else {
            println!("FAIL: faulted at {addr:#x}, outside the task's guard page {start:#x}..{end:#x}");
        }
        Board::halt()
    });
    println!("overflowing a {} KiB task stack", stack::STACK_SIZE / 1024);
    run(
        || {
            spawn(|| {
                let guard = current_guard();
                GUARD.set((guard.start, guard.end));
                recurse(0);
                println!("FAIL: recursed without faulting");
            })
        },
        |_| {},
    );
}

/// Recurse with a 256-byte frame (well under a page, so it can't skip the guard page)
/// that stays live across the call, so the recursion can't become a loop.
#[inline(never)]
fn recurse(depth: u64) -> u64 {
    DEPTH.set(depth);
    let frame = core::hint::black_box([depth as u8; 256]);
    if depth == u64::MAX {
        return 0;
    }
    recurse(depth + 1) + u64::from(frame[0])
}

// ---- The disk test --------------------------------------------------------------------
//
// cargo xtask kernel attaches a disk image whose every byte is `pattern(sector, offset)`.
// Read it all back, a few sectors per request; then overwrite one sector, flush, and
// check that it reads back changed and its neighbours didn't.

/// Must match xtask's `disk_pattern`.
fn pattern(sector: u64, offset: usize) -> u8 {
    (sector as u8).wrapping_mul(31) ^ (offset as u8) ^ (offset >> 8) as u8
}

/// The serial number xtask gives the disk.
const DISK_ID: &[u8] = b"green-tea-disk";
/// Sectors per read request.
const CHUNK: usize = 4;
/// The sector the test overwrites.
const SCRATCH: u64 = 3;

fn disk_test(fb: Framebuffer) {
    Board::for_each_virtio(|d| println!("virtio: {} device (id {})", virtio::device_name(d.id), d.id))
        .unwrap_or_else(|e| panic!("{e}"));
    let transport = match Board::find_virtio(virtio::ID_BLOCK) {
        Ok(Some(t)) => t,
        Ok(None) => return println!("FAIL: no virtio block device"),
        Err(e) => return println!("FAIL: {e}"),
    };
    let kind = match transport {
        virtio::AnyTransport::Mmio(_) => "MMIO",
        virtio::AnyTransport::Pci(_) => "PCI",
    };
    let mut disk = match Blk::new(transport) {
        Ok(disk) => disk,
        Err(e) => return println!("FAIL: {e}"),
    };
    let sectors = disk.capacity();
    let id = match disk.id() {
        Ok(id) => id,
        Err(e) => return println!("FAIL: reading the disk's id: {e}"),
    };
    let id = &id[..id.iter().position(|&b| b == 0).unwrap_or(id.len())];
    println!(
        "virtio-blk over {kind}: {sectors} sectors ({} KiB), id {:?}{}{}",
        sectors * SECTOR_SIZE as u64 / 1024,
        core::str::from_utf8(id).unwrap_or("?"),
        if disk.read_only() { ", read-only" } else { "" },
        if disk.write_cache() { ", write cache" } else { "" },
    );
    if id != DISK_ID {
        return println!("FAIL: the disk's id isn't {:?}", core::str::from_utf8(DISK_ID).unwrap());
    }
    if let Err(e) = check_disk(&mut disk) {
        return println!("FAIL: {e}");
    }
    Screen::new(fb).finish(sectors as usize); // green, with the sector count
    println!("PASS: read {sectors} sectors over virtio-{kind}, and wrote one back");
}

fn check_disk(disk: &mut Blk<impl Transport>) -> Result<(), String> {
    use alloc::format;
    let sectors = disk.capacity();
    if sectors < SCRATCH + 2 || !sectors.is_multiple_of(CHUNK as u64) {
        return Err(format!("unexpected capacity: {sectors} sectors"));
    }

    // Every sector, CHUNK at a time.
    let mut buf = vec![0u8; CHUNK * SECTOR_SIZE];
    for first in (0..sectors).step_by(CHUNK) {
        disk.read(first, &mut buf).map_err(|e| format!("reading sector {first}: {e}"))?;
        for (i, sector) in buf.chunks(SECTOR_SIZE).enumerate() {
            let n = first + i as u64;
            if let Some(offset) = (0..SECTOR_SIZE).find(|&o| sector[o] != pattern(n, o)) {
                return Err(format!("sector {n} byte {offset} is {:#04x}, expected {:#04x}", sector[offset], pattern(n, offset)));
            }
        }
    }

    // The driver refuses what the device would.
    if disk.read(sectors - 1, &mut buf) != Err(blk::Error::OutOfRange) {
        return Err("a read past the end didn't fail".into());
    }
    if disk.read(0, &mut buf[..100]) != Err(blk::Error::Unaligned) {
        return Err("a read of part of a sector didn't fail".into());
    }

    // Overwrite one sector with the pattern inverted, and read it and its neighbours back.
    let mut sector = vec![0u8; SECTOR_SIZE];
    for (o, byte) in sector.iter_mut().enumerate() {
        *byte = !pattern(SCRATCH, o);
    }
    disk.write(SCRATCH, &sector).map_err(|e| format!("writing sector {SCRATCH}: {e}"))?;
    disk.flush().map_err(|e| format!("flushing: {e}"))?;
    let mut three = vec![0u8; 3 * SECTOR_SIZE];
    disk.read(SCRATCH - 1, &mut three).map_err(|e| format!("reading back: {e}"))?;
    for (i, got) in three.chunks(SECTOR_SIZE).enumerate() {
        let n = SCRATCH - 1 + i as u64;
        let expected = |o| if n == SCRATCH { !pattern(n, o) } else { pattern(n, o) };
        if (0..SECTOR_SIZE).any(|o| got[o] != expected(o)) {
            return Err(format!("sector {n} reads back wrong after writing sector {SCRATCH}"));
        }
    }
    Ok(())
}

// ---- The memory map test --------------------------------------------------------------
//
// Print where the linker put each section, from the symbols linker.ld defines, and check
// the layout from the inside: every section starts on its alignment, they are in order and
// don't overlap, the ones linker.ld sizes have that size, and one address of each kind (a
// function, a constant, a static, a local, a box, a task's local) is in the section it
// should be in.

/// A static with an initial value: in the image, and writable, so in .data.
static INITIALISED: AtomicUsize = AtomicUsize::new(0x6772_6e74);
/// A static that starts as zero: not in the image, so in .bss.
static ZEROED: AtomicUsize = AtomicUsize::new(0);
/// A static that never changes: .rodata.
static CONSTANT: [u8; 16] = *b"green-tea memmap";

/// A type that asks for more than the usual alignment.
#[repr(align(4096))]
struct Page(#[allow(dead_code)] [u8; 4096]);

/// The address of a local variable of the task that `run` starts.
static TASK_LOCAL: Global<usize> = Global::new(0);

fn memmap_test(fb: Framebuffer) {
    let kernel = memory::kernel();
    println!("kernel: {:#x}..{:#x}, {} KiB", kernel.start, kernel.end, kernel.len() / 1024);
    println!("  section      start        end      bytes    align");
    let mut previous = kernel.start;
    for s in memory::sections() {
        if s.start > previous {
            println!("  {:8}                       {:9}", "(gap)", s.start - previous);
        }
        println!("  {:8} {:#010x} {:#010x} {:9} {:8}", s.name, s.start, s.end, s.size(), s.align);
        previous = s.end;
    }

    let local = 0u8;
    let boxed = Box::new(0u64);
    let page = Box::new(Page([0; 4096]));
    run(
        || {
            let task_local = 0u8;
            TASK_LOCAL.set(&raw const task_local as usize);
        },
        |_| {},
    );
    // What each address is, where it is, where it should be, and the alignment it needs.
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
    println!("  address    in       what");
    for (what, addr, ..) in witnesses {
        let name = memory::section_of(addr).map_or("nowhere", |s| s.name);
        println!("  {addr:#010x} {name:8} {what}");
    }

    if let Err(e) = check_memmap(&witnesses) {
        return println!("FAIL: {e}");
    }
    let sections = memory::sections().len();
    Screen::new(fb).finish(sections); // green, with the section count
    println!("PASS: {sections} sections in order and aligned; {} addresses where they should be", witnesses.len());
}

fn check_memmap(witnesses: &[(&str, usize, &str, usize)]) -> Result<(), String> {
    use alloc::format;
    let kernel = memory::kernel();
    let mut previous = kernel.start;
    for s in memory::sections() {
        if !s.start.is_multiple_of(s.align) {
            return Err(format!("{} starts at {:#x}, which isn't a multiple of {}", s.name, s.start, s.align));
        }
        if s.start < previous || s.end < s.start {
            return Err(format!("{} ({:#x}..{:#x}) overlaps the section before it, which ends at {previous:#x}", s.name, s.start, s.end));
        }
        if let Some(reserved) = s.reserved
            && s.size() != reserved
        {
            return Err(format!("{} is {} bytes; linker.ld reserves {reserved}", s.name, s.size()));
        }
        previous = s.end;
    }
    if previous != kernel.end {
        return Err(format!("the last section ends at {previous:#x}, the kernel at {:#x}", kernel.end));
    }
    // The boot stack's top is where sp starts, and every ABI here wants sp 16-byte aligned.
    let stack = memory::sections()[4];
    if stack.name != ".stack" || !stack.end.is_multiple_of(16) {
        return Err(format!("the boot stack's top, {:#x}, isn't 16-byte aligned", stack.end));
    }
    for &(what, addr, expected, align) in witnesses {
        let found = memory::section_of(addr).map_or("no section", |s| s.name);
        if found != expected {
            return Err(format!("{what} is at {addr:#x}, in {found}, not {expected}"));
        }
        if !addr.is_multiple_of(align) {
            return Err(format!("{what} is at {addr:#x}, which isn't a multiple of its alignment, {align}"));
        }
    }
    Ok(())
}

// ---- The pool test --------------------------------------------------------------------
//
// Two parts. First, the same sequence of takes and gives on each of green-tea-pool's two
// pools, over eight small slots from the heap: which slot each hands out next, what
// happens when the pool is empty, and what happens when a slot is given back twice.
// Second, the stack pool at work: a thousand tasks, one after another, must all run on the
// same stack, and leave the pool as full as they found it.

const POOL_SLOT: usize = 64;
const POOL_SLOTS: usize = 8;

/// What one pool did with the sequence.
struct PoolRun {
    /// The slots it handed out from new, as indices.
    fresh: [usize; POOL_SLOTS],
    /// Whether a ninth `take` got nothing.
    empty_refuses: bool,
    /// The three it handed out after 2, 5 and 0 were given back, in that order.
    reused: [usize; 3],
    /// What it said to a slot given back a second time.
    second_give: Result<(), GiveError>,
    /// The next two it handed out after that: `None` where it had nothing left.
    after_second_give: [Option<usize>; 2],
    /// In a new pool: slot 0 is taken and given back, and then its first word is
    /// overwritten with the address of `VICTIM`, as a task still using the slot after
    /// giving it back might. Whether one of the next two takes returned that address.
    hands_out_victim: bool,
}

/// Not part of any pool.
static VICTIM: Global<usize> = Global::new(0);

fn pool_run<P: Pool>() -> PoolRun {
    // Never freed: the heap doesn't, and the test runs once.
    let base = Box::leak(Box::new([0usize; POOL_SLOTS * POOL_SLOT / size_of::<usize>()])).as_mut_ptr() as usize;
    let mut pool = unsafe { P::new(base, POOL_SLOT, POOL_SLOTS, 0) };
    let take = |pool: &mut P| pool.take().map_or(usize::MAX, |slot| (slot - base) / POOL_SLOT);
    let fresh = [0; POOL_SLOTS].map(|_| take(&mut pool));
    let empty_refuses = pool.take().is_none();
    for index in [2, 5, 0] {
        let _ = unsafe { pool.give(base + index * POOL_SLOT) };
    }
    let reused = [0; 3].map(|_| take(&mut pool));
    // Slot 7 is taken. Give it back twice: the second time breaks `give`'s contract, on
    // purpose, to see what each pool makes of it.
    let slot = base + 7 * POOL_SLOT;
    let _ = unsafe { pool.give(slot) };
    let second_give = unsafe { pool.give(slot) };
    let after_second_give = [0; 2].map(|_| pool.take().map(|slot| (slot - base) / POOL_SLOT));

    let base = Box::leak(Box::new([0usize; POOL_SLOTS * POOL_SLOT / size_of::<usize>()])).as_mut_ptr() as usize;
    let mut pool = unsafe { P::new(base, POOL_SLOT, POOL_SLOTS, 0) };
    let victim = &raw const VICTIM as usize;
    let slot = pool.take().expect("a new pool has a slot");
    let _ = unsafe { pool.give(slot) };
    unsafe { (slot as *mut usize).write(victim) }; // a use after free
    let hands_out_victim = [pool.take(), pool.take()].contains(&Some(victim));
    PoolRun { fresh, empty_refuses, reused, second_give, after_second_give, hands_out_victim }
}

/// How many tasks the second part runs.
const TASKS: usize = 1000;

/// The lowest and highest address of a local variable in any of the tasks, and how many
/// tasks have run.
static STACK_SEEN: Global<(usize, usize, usize)> = Global::new((usize::MAX, 0, 0));

fn pool_test(fb: Framebuffer) {
    println!("a pool of {POOL_SLOTS} slots:        bitmap                     free list");
    let (bitmap, list) = (pool_run::<Bitmap>(), pool_run::<FreeList>());
    println!("  taken from new:          {:?}   {:?}", bitmap.fresh, list.fresh);
    println!("  a ninth take refused:    {:<24}   {}", bitmap.empty_refuses, list.empty_refuses);
    println!("  after giving 2, 5, 0:    {:<24}   {:?}", alloc::format!("{:?}", bitmap.reused), list.reused);
    println!(
        "  7 given back twice:      {:<24}   {:?}",
        alloc::format!("{:?}", bitmap.second_give),
        list.second_give
    );
    println!(
        "  the next two takes:      {:<24}   {:?}",
        alloc::format!("{:?}", bitmap.after_second_give),
        list.after_second_give
    );
    println!(
        "  after a write to a free slot, hands out a block outside the pool:\n                           {:<24}   {}",
        bitmap.hands_out_victim, list.hands_out_victim
    );

    let free_before = stack::free_slots();
    let heap_before = heap::used();
    run(
        || {
            for _ in 0..TASKS {
                spawn(|| {
                    let local = 0u8;
                    let addr = &raw const local as usize;
                    let (low, high, count) = STACK_SEEN.get();
                    STACK_SEEN.set((low.min(addr), high.max(addr), count + 1));
                });
                yield_now(); // the new task runs, and finishes, before the next is spawned
            }
        },
        |_| {},
    );
    let (low, high, count) = STACK_SEEN.get();
    let heap_per_task = (heap::used() - heap_before) / TASKS;
    println!(
        "{count} tasks, one after another: their locals were at {low:#x}..={high:#x}; the pool has {} of {} \
         stacks free, as before; each task cost {heap_per_task} bytes of heap",
        stack::free_slots(),
        stack::SLOTS
    );

    let ascending: [usize; POOL_SLOTS] = core::array::from_fn(|i| i);
    let verdict = if bitmap.fresh != ascending || list.fresh != ascending {
        Err("a new pool didn't hand out its slots in address order")
    } else if !bitmap.empty_refuses || !list.empty_refuses {
        Err("an empty pool handed out a slot")
    } else if bitmap.reused != [0, 2, 5] {
        Err("the bitmap didn't reuse the lowest slot first")
    } else if list.reused != [0, 5, 2] {
        Err("the free list didn't reuse the last slot given back first")
    } else if bitmap.second_give != Err(GiveError::NotTaken) {
        Err("the bitmap accepted a slot given back twice")
    } else if list.second_give != Ok(()) || list.after_second_give != [Some(7), Some(7)] {
        Err("the free list didn't hand out twice the slot it was given twice")
    } else if bitmap.hands_out_victim || !list.hands_out_victim {
        Err("a write to a free slot should mislead the free list, and only the free list")
    } else if count != TASKS || low != high {
        Err("the tasks didn't all run on the same stack")
    } else if stack::free_slots() != free_before {
        Err("the stack pool lost a stack")
    } else {
        Ok(())
    };
    match verdict {
        Err(e) => println!("FAIL: {e}"),
        Ok(()) => {
            Screen::new(fb).finish(TASKS); // green, with the task count
            println!("PASS: {TASKS} tasks ran on one stack; the bitmap caught a double give, the free list didn't");
        }
    }
}
