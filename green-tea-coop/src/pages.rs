//! The pages test: `cargo xtask kernel --kernel pages`, on the boards that page.
//!
//! The heap's 1,024 pages start out unmapped, and the page-fault handler maps each one the
//! first time it is touched (green-tea-bare's `pages`). The test shows what that makes of
//! a megabyte that is reserved, touched in part, touched in full, freed, and given back;
//! and of a megabyte of small blocks most of which are freed, where what can be given back
//! depends on how the survivors are spread over the pages.

use alloc::alloc::{alloc, dealloc};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::fmt::Write;
use core::hint::black_box;
use green_tea_bare::pages::{self, PAGE, PointsTo, Step};
use green_tea_bare::ramfb::Framebuffer;
use green_tea_bare::screen::Screen;
use green_tea_bare::{Board, Platform, heap, println, stack};

const MIB: usize = 1 << 20;
/// The small blocks of the second part: 48 bytes each, which is a block of 64 with its tags.
const SMALL: usize = 48;
const SMALL_BLOCK: usize = 64;
const SMALL_COUNT: usize = MIB / SMALL_BLOCK;

/// The heap's footprint, in pages: how far up the heap the allocator has ever put a block.
fn footprint_pages() -> usize {
    heap::with(|heap| heap.footprint()).div_ceil(PAGE)
}

/// One row of the first table: what has been done, and what that made of the heap's pages.
fn row(what: &str) -> pages::Stats {
    let footprint = footprint_pages(); // the heap's first use, if nothing has used it yet
    let stats = pages::stats();
    let mut runs = String::new();
    pages::for_each_run(|run| {
        let comma = if runs.is_empty() { "" } else { ", " };
        let _ = match run.len() {
            1 => write!(runs, "{comma}{}", run.start),
            _ => write!(runs, "{comma}{}-{}", run.start, run.end - 1),
        };
    });
    println!("  {what:<38} {footprint:9} {:7} {:7} {:9}   {runs}", stats.mapped, stats.faults, stats.released);
    stats
}

pub fn test(fb: Framebuffer) {
    if !Board::DEMAND_PAGING {
        return println!("FAIL: {} doesn't page: every page of the heap is always mapped", Board::NAME);
    }
    match run() {
        Err(e) => println!("FAIL: {e}"),
        Ok(()) => {
            let stats = pages::stats();
            Screen::new(fb).finish(stats.faults); // green, with the fault count
            println!(
                "PASS: of {} pages reserved, {} were the most mapped at once; {} page faults, {} pages released",
                stats.reserved, stats.peak, stats.faults, stats.released
            );
        }
    }
}

fn run() -> Result<(), &'static str> {
    let heap = pages::heap_range();
    println!("the heap is {} pages of {PAGE} bytes, at {:#x}..{:#x}", heap.len() / PAGE, heap.start, heap.end);
    println!("  {:<38} footprint  mapped  faults  released   the pages mapped", "");
    let booted = row("after booting");

    // A megabyte asked for and not written to.
    let mut megabyte: Vec<u8> = Vec::with_capacity(MIB);
    let base = megabyte.as_mut_ptr();
    let reserved = row("a megabyte allocated");
    for page in 0..16 {
        // SAFETY: inside the vector's capacity.
        unsafe {
            base.add(page * PAGE + 100)
                .write_volatile(0xab)
        };
    }
    let sixteen = row("a byte written in each of 16 pages");
    megabyte.resize(MIB, 0xab);
    black_box(&megabyte); // or an optimizing compiler drops the writes: nothing reads them
    let written = row("every byte written");
    drop(megabyte);
    let freed = row("freed");
    let trimmed_pages = pages::trim();
    let trimmed = row("trimmed");
    check()?;

    // The same megabyte again: the allocator hands out the same block, and its pages are gone.
    let again: Vec<u8> = Vec::with_capacity(MIB);
    if again.as_ptr() != base.cast_const() {
        return Err("the second megabyte wasn't where the first was");
    }
    // SAFETY: inside the vector's capacity. The byte was 0xab before the page was released.
    let allocated = row("allocated again");
    let read = unsafe {
        again
            .as_ptr()
            .add(64 * PAGE + 100)
            .read_volatile()
    };
    let reread = row("one byte of it read");
    drop(again);
    let zeros = black_box(vec![0u8; MIB]);
    let zeroed = row("a megabyte of zeros (vec![0; n])");
    drop(zeros);
    pages::trim();
    let end = row("freed and trimmed");

    if booted.mapped != booted.faults || booted.released != 0 {
        return Err("pages were mapped without a fault, or released, before the test began");
    }
    if booted.mapped != 2 || reserved.faults - booted.faults != 1 {
        return Err("a new heap isn't on two pages, or allocating a megabyte touched more than the page its end is on");
    }
    if sixteen.faults - reserved.faults != 16 && sixteen.faults - reserved.faults != 15 {
        return Err("writing to 16 pages didn't fault once for each page that was missing");
    }
    if written.mapped - booted.mapped < MIB / PAGE || written.mapped - booted.mapped > MIB / PAGE + 2 {
        return Err("a megabyte written in full isn't on 256 pages, or 257");
    }
    if freed != written {
        return Err("freeing a block changed which pages are mapped");
    }
    if trimmed_pages < MIB / PAGE - 2 || trimmed.mapped != freed.mapped - trimmed_pages {
        return Err("trimming didn't release the freed megabyte's pages");
    }
    if read != 0 {
        return Err("a released page came back with its old contents");
    }
    if allocated.faults - trimmed.faults != 1 {
        return Err("allocating the megabyte again didn't fault once, for the page its end is on");
    }
    if reread.faults - allocated.faults != 1 || reread.mapped - allocated.mapped != 1 {
        return Err("reading a released page didn't fault exactly once");
    }
    if zeroed.mapped - end.mapped < MIB / PAGE - 2 {
        return Err("a megabyte of zeros wasn't written to");
    }
    check()?;
    walks()?;
    boundary()?;
    pinned()
}

/// Two free blocks placed so that each one's header is the last word of a page, and its
/// links are the first two words of the next. Trimming must keep the page the links are
/// on: the blocks are the same size, so they are on one list, and the list runs through
/// those links.
fn boundary() -> Result<(), &'static str> {
    const FREED: usize = 3 * PAGE; // each of the two blocks, tags included
    let layout = |block: usize| Layout::from_size_align(block - 16, 8).expect("a layout");
    // SAFETY (all of this function's allocations): every layout has a size, and every
    // block is freed once, with the layout it was allocated with. `black_box`, because an
    // optimizing compiler may remove an allocation that is freed without being used, and
    // here the blocks that are never written to are there to keep the others apart.
    let alloc = |layout: Layout| black_box(unsafe { alloc(layout) });
    // A block of a page, to see where the heap's large free block is: the block after it
    // starts a page above it. Then a filler that ends on the last word of a page.
    let probe = alloc(layout(PAGE)) as usize;
    let filler_size = (PAGE - probe % PAGE) % PAGE + PAGE;
    let filler = alloc(layout(filler_size));
    let blocks = [FREED, PAGE, FREED, PAGE].map(|size| alloc(layout(size)));
    if blocks
        .iter()
        .any(|block| block.is_null() || !(*block as usize).is_multiple_of(PAGE))
    {
        return Err("the test's blocks aren't where it planned: each header the last word of a page");
    }
    unsafe {
        blocks[0].write_bytes(0xcd, FREED - 16); // in use, so on four pages each
        blocks[2].write_bytes(0xcd, FREED - 16);
        black_box(
            blocks[0].read_volatile()
                ^ blocks[2]
                    .add(FREED - 17)
                    .read_volatile(),
        );
        dealloc(blocks[0], layout(FREED));
        dealloc(blocks[2], layout(FREED));
    }
    let released = pages::trim();
    let checked = check();
    // Both blocks again: each must come back from the list, where it was.
    let again = [alloc(layout(FREED)), alloc(layout(FREED))];
    println!(
        "two free blocks of {FREED} bytes, each with its header in the last word of a page: trimming released \
         {released} pages"
    );
    unsafe {
        for (block, size) in again
            .into_iter()
            .zip([FREED, FREED])
            .chain(
                blocks
                    .into_iter()
                    .zip([0, PAGE, 0, PAGE]),
            )
        {
            if size != 0 && !block.is_null() {
                dealloc(block, layout(size));
            }
        }
        dealloc(filler, layout(filler_size));
        dealloc(probe as *mut u8, layout(PAGE));
    }
    checked?;
    if again != [blocks[2], blocks[0]] {
        return Err("a free block was lost: the page its links are on was released");
    }
    if released != 2 {
        return Err("a free block of three pages that is on four has one page to release");
    }
    pages::trim();
    check()
}

/// A device's registers, which the boot code maps with the largest page it has.
#[cfg(target_arch = "riscv64")]
const DEVICE: (usize, &str) = (0x1000_0000, "the serial port's registers");
#[cfg(target_arch = "x86_64")]
const DEVICE: (usize, &str) = (0xfee0_0000, "the interrupt controller's registers");
#[cfg(target_arch = "aarch64")]
const DEVICE: (usize, &str) = (0x0900_0000, "the serial port's registers");

/// Between the first part and the second: what the page table holds for six addresses,
/// read from the tables themselves, one line per level.
fn walks() -> Result<(), &'static str> {
    let heap = pages::heap_range();
    let guard = stack::guard_pages()
        .next()
        .expect("the stack pool has a slot");
    let past = stack::region_start() + stack::REGION;
    let addresses = [
        (DEVICE.0, DEVICE.1),
        (past, "the first byte of memory after the kernel"),
        (heap.start, "the heap's first page"),
        (heap.start + PAGE, "the heap's second page"),
        (guard, "the guard page of the first task stack"),
        (guard + PAGE, "the first page of that stack"),
    ];
    println!("the page table's entries for six addresses, from the root table down:");
    let mut ends = [PointsTo::Nothing; 6];
    for ((addr, what), end) in addresses
        .into_iter()
        .zip(&mut ends)
    {
        println!("  {addr:#x}, {what}");
        for step in pages::walk(addr) {
            let Step { index, entry, points_to } = step;
            match points_to {
                PointsTo::Nothing => println!("    entry {index:3}  {entry:#018x}  not valid: an access faults"),
                PointsTo::Table(table) => println!("    entry {index:3}  {entry:#018x}  the next table, at {table:#x}"),
                PointsTo::Page { frame, size } => {
                    let (number, unit) = if size >= 1 << 30 { (size >> 30, "GiB") } else { (size >> 10, "KiB") };
                    let (number, unit) =
                        if unit == "KiB" && number >= 1024 { (number >> 10, "MiB") } else { (number, unit) };
                    println!(
                        "    entry {index:3}  {entry:#018x}  a page of {number} {unit}, in the frame at {frame:#x}"
                    );
                }
            }
            *end = points_to;
        }
    }
    let page = |addr: usize, size: usize| PointsTo::Page { frame: addr & !(size - 1), size };
    if !matches!(ends[0], PointsTo::Page { size, .. } if size >= 2 * MIB) || ends[1] != page(past, 2 * MIB) {
        return Err("memory outside the heap and the stacks isn't mapped with large pages");
    }
    if ends[2] != page(heap.start, PAGE) || ends[5] != page(guard + PAGE, PAGE) {
        return Err("a page that is mapped isn't mapped to the frame at its own address");
    }
    if ends[3] != PointsTo::Nothing || ends[4] != PointsTo::Nothing {
        return Err("an untouched page of the heap, or a guard page, has a valid entry");
    }
    Ok(())
}

/// The second part: a megabyte of small blocks, and what trimming can give back as more
/// and more of them are freed.
fn pinned() -> Result<(), &'static str> {
    let layout = Layout::from_size_align(SMALL, 8).expect("a layout of 48 bytes");
    let mut blocks: Vec<*mut u8> = Vec::with_capacity(SMALL_COUNT);
    for i in 0..SMALL_COUNT {
        // SAFETY: the layout has a size. Each block is written to, as a block in use is.
        let block = unsafe { alloc(layout) };
        if block.is_null() {
            return Err("the heap refused a small block");
        }
        unsafe { block.write_bytes(i as u8, SMALL) };
        blocks.push(block);
    }
    let first = blocks[0] as usize - 8;
    let range = first..first + SMALL_COUNT * SMALL_BLOCK;
    if blocks
        .iter()
        .enumerate()
        .any(|(i, block)| *block as usize != first + 8 + i * SMALL_BLOCK)
    {
        return Err("the small blocks aren't one after another");
    }
    println!(
        "{SMALL_COUNT} blocks of {SMALL} bytes, one after another from {:#x}, on {} pages",
        range.start,
        pages::mapped_in(range.clone())
    );
    println!("  blocks kept            live bytes   would fill   pages mapped   pages released");
    // Keep one block in every `keep`: block 32 of each 64, then of each 128, and so on. With
    // 64 blocks to a page, that is a block half way down every page, then every second page.
    let mut previous = SMALL_COUNT;
    let mut results = Vec::new();
    for keep in [64, 128, 256, 1024, 4096] {
        for (i, block) in blocks
            .iter_mut()
            .enumerate()
        {
            if !block.is_null() && i % keep != 32 {
                // SAFETY: allocated above with this layout, and freed once.
                unsafe { dealloc(*block, layout) };
                *block = core::ptr::null_mut();
            }
        }
        let released = pages::trim();
        let live = blocks
            .iter()
            .filter(|block| !block.is_null())
            .count();
        let mapped = pages::mapped_in(range.clone());
        println!(
            "  one in {keep:<6} {live:6} {:12} {:12} {mapped:14} {released:16}",
            live * SMALL,
            (live * SMALL_BLOCK).div_ceil(PAGE)
        );
        check()?;
        if live != SMALL_COUNT / keep || live >= previous {
            return Err("the test kept the wrong blocks");
        }
        previous = live;
        results.push((keep, mapped));
    }
    for (i, block) in blocks
        .iter()
        .enumerate()
    {
        if !block.is_null() {
            // SAFETY: still allocated, and inside the block.
            if unsafe {
                block
                    .add(SMALL - 1)
                    .read_volatile()
            } != i as u8
            {
                return Err("a block that was kept lost its contents");
            }
            unsafe { dealloc(*block, layout) };
        }
    }
    let released = pages::trim();
    println!("  none {:27} {:12} {:14} {released:16}", 0, 0, pages::mapped_in(range.clone()));
    check()?;
    // A block in the middle of each page pins the page. One in the middle of every second
    // page leaves the page between two of them free whole.
    if results[0].1 != MIB / PAGE {
        return Err("one block to a page didn't keep every page");
    }
    if results[1].1 > MIB / PAGE / 2 + 1 || results[2].1 > MIB / PAGE / 4 + 1 {
        return Err("trimming left pages mapped that hold no block");
    }
    Ok(())
}

/// The allocator's own check, and ours: the count of mapped pages is the number of pages
/// the page table has.
fn check() -> Result<(), &'static str> {
    heap::with(|heap| heap.check())?;
    let mut counted = 0;
    pages::for_each_run(|run| counted += run.len());
    if counted != pages::stats().mapped {
        return Err("the count of mapped pages isn't what the page table says");
    }
    Ok(())
}
