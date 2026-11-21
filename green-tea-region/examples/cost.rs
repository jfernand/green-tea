//! What a shell command costs with its memory from each of five places, for counting the
//! instructions with: `valgrind --tool=callgrind target/release/examples/cost region 100000`.
//!
//! The command is green-tea-shell's `echo`: a copy of the line, a vector of its words, and
//! the text it prints, each in a `Vec` that grows as it is filled, all three dropped at
//! the end. It is run `count` times with its memory from:
//!
//! - `system`: the C library's `malloc`, which is Rust's global allocator on Linux;
//! - `fit`: green-tea-fit's first fit over an address-ordered free list;
//! - `tlsf`: green-tea-fit's TLSF, which is the kernels' heap;
//! - `region`: a [`Region`], reset after every command;
//! - `scope`: a new region for every command, its chunk from `malloc`.
//!
//! Run a count of 0 as well, and subtract: what is left is the commands.

use core::alloc::{AllocError, Allocator, Layout};
use core::cell::RefCell;
use core::fmt::Write;
use core::hint::black_box;
use core::ptr::NonNull;
use green_tea_fit::tlsf::Tlsf;
use green_tea_fit::{Fit, Heap, Order, Policy};
use green_tea_region::{Region, scope};
use std::alloc::Global;

const LINE: &str = "echo the quick brown fox jumps over the lazy dog";
/// The memory `fit` and `tlsf` allocate from.
const HEAP: usize = 1 << 20;
const CHUNK: usize = 4096;

/// One of green-tea-fit's allocators as an `Allocator`. `grow` is the trait's default:
/// allocate, copy, free.
struct FromHeap<H>(RefCell<H>);

unsafe impl<H: Heap> Allocator for FromHeap<H> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let addr = self.0.borrow_mut().allocate(layout.size(), layout.align()).ok_or(AllocError)?;
        Ok(NonNull::slice_from_raw_parts(NonNull::new(addr as *mut u8).ok_or(AllocError)?, layout.size()))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, _layout: Layout) {
        // SAFETY: the caller's promise is ours to the allocator.
        unsafe { self.0.borrow_mut().free(ptr.as_ptr() as usize) }.expect("a free was refused");
    }
}

/// Text being built up in a command's memory, as in the shell.
struct Text<A: Allocator>(Vec<u8, A>);

impl<A: Allocator> Write for Text<A> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// The shell's `execute`, for an `echo`. Returns how many bytes the command would print.
#[inline(never)]
fn command<A: Allocator + Copy>(text: &str, memory: A) -> usize {
    let mut line = Vec::new_in(memory);
    line.extend_from_slice(text.as_bytes());
    let line = core::str::from_utf8(&line).unwrap_or("");

    let mut words = Vec::new_in(memory);
    words.extend(line.split_ascii_whitespace());

    let mut out = Text(Vec::new_in(memory));
    if let ["echo", rest @ ..] = words.as_slice() {
        for (i, word) in rest.iter().enumerate() {
            let _ = write!(out, "{}{word}", if i == 0 { "" } else { " " });
        }
        let _ = out.write_str("\n");
    }
    black_box(&out.0).len()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(way), Some(Ok(count))) = (args.get(1), args.get(2).map(|n| n.parse::<usize>())) else {
        eprintln!("usage: cost system|fit|tlsf|region|scope <count>");
        std::process::exit(2);
    };
    let mut memory = vec![0u128; HEAP / 16];
    let base = memory.as_mut_ptr() as usize;
    let mut printed = 0;
    match way.as_str() {
        "system" => (0..count).for_each(|_| printed += command(black_box(LINE), Global)),
        "fit" => {
            // SAFETY: the memory is ours, and this is the only allocator using it.
            let heap = FromHeap(RefCell::new(unsafe { Fit::new(base, HEAP, Policy::FirstFit, Order::Address) }));
            (0..count).for_each(|_| printed += command(black_box(LINE), &heap));
            assert_eq!(heap.0.borrow().used(), 0);
        }
        "tlsf" => {
            // SAFETY: as above.
            let heap = FromHeap(RefCell::new(unsafe { Tlsf::new(base, HEAP) }));
            (0..count).for_each(|_| printed += command(black_box(LINE), &heap));
            assert_eq!(heap.0.borrow().used(), 0);
        }
        "region" => {
            let mut region = Region::new(CHUNK);
            for _ in 0..count {
                printed += command(black_box(LINE), &region);
                region.reset();
            }
            assert!(region.counts().taken <= 1);
        }
        "scope" => (0..count).for_each(|_| printed += scope(CHUNK, |region| command(black_box(LINE), region))),
        _ => {
            eprintln!("usage: cost system|fit|tlsf|region|scope <count>");
            std::process::exit(2);
        }
    }
    println!("{way}: {count} commands, {printed} bytes they would have printed");
}
