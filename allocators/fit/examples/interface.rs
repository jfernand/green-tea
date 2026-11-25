//! What the three allocators of this crate answer at the edges of the [`Heap`] trait: a
//! request for no bytes, for an alignment above 16, for more than there is, and a free of
//! an address that isn't a live block. Addresses are printed as offsets into the region.
//!
//! cargo run -p green-tea-fit --example interface

use green_tea_fit::buddy::Buddy;
use green_tea_fit::tlsf::Tlsf;
use green_tea_fit::{Fit, Heap, Order, Policy};

const REGION: usize = 4096;

#[repr(align(4096))]
struct Regions([u8; 3 * REGION]);

fn at(base: usize, addr: Option<usize>) -> String {
    match addr {
        Some(addr) => format!("Some(region + {})", addr - base),
        None => "None".to_string(),
    }
}

fn edges(name: &str, base: usize, heap: &mut dyn Heap) {
    println!("{name}");
    let a = heap.allocate(100, 8);
    println!("  allocate(100, 8)        = {}, used {}", at(base, a), heap.used());
    let before = heap.used();
    let zero = heap.allocate(0, 1);
    println!("  allocate(0, 1)          = {}, used +{}", at(base, zero), heap.used() - before);
    let wide = heap.allocate(24, 64);
    let rem = wide.map_or("-".to_string(), |addr| (addr % 64).to_string());
    println!("  allocate(24, 64)        = {}, address % 64 = {rem}", at(base, wide));
    println!("  allocate(8192, 8)       = {}", at(base, heap.allocate(8192, 8)));
    println!("  allocate(usize::MAX, 8) = {}", at(base, heap.allocate(usize::MAX, 8)));
    println!("  stats().failed          = {}", heap.stats().failed);
    let a = a.expect("the first allocation");
    // SAFETY: `a` is a live block for the first call. The other three break `free`'s
    // contract on purpose, with addresses the allocators check before they touch anything.
    unsafe {
        println!("  free(a)                 = {:?}", heap.free(a));
        println!("  free(a), again          = {:?}", heap.free(a));
        println!("  free(a + 8)             = {:?}", heap.free(a + 8));
        println!("  free(region + {REGION})     = {:?}", heap.free(base + REGION));
    }
    println!("  check()                 = {:?}", heap.check());
}

fn main() {
    // One region for each allocator. Page-aligned, so that the offsets printed are the
    // same on every run.
    let mut memory = Box::new(Regions([0; 3 * REGION]));
    let base = memory.0.as_mut_ptr() as usize;
    // SAFETY: the three regions are parts of `memory`, which outlives the allocators.
    unsafe {
        edges("Fit (first fit, address order)", base, &mut Fit::new(base, REGION, Policy::FirstFit, Order::Address));
        edges("Buddy", base + REGION, &mut Buddy::new(base + REGION, REGION));
        edges("Tlsf", base + 2 * REGION, &mut Tlsf::new(base + 2 * REGION, REGION));
    }
}
