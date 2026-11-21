//! What the two pools of this crate answer at the edges of the [`Pool`] trait: a take
//! from an empty pool, and a give of a slot that isn't taken. Slots are printed by number.
//!
//! cargo run -p green-tea-pool --example interface

use green_tea_pool::{Bitmap, FreeList, Pool};

const SLOT: usize = 64;
const SLOTS: usize = 4;

fn slot(base: usize, addr: Option<usize>) -> String {
    match addr {
        Some(addr) => format!("Some(slot {})", (addr - base) / SLOT),
        None => "None".to_string(),
    }
}

fn edges<P: Pool>(name: &str) {
    println!("{name}");
    let mut memory = [0usize; SLOTS * SLOT / size_of::<usize>()];
    let base = memory.as_mut_ptr() as usize;
    // SAFETY: `memory` is the pool's alone, and outlives it. The link goes at the start of a slot.
    let mut pool = unsafe { P::new(base, SLOT, SLOTS, 0) };
    let taken: Vec<String> = (0..=SLOTS).map(|_| slot(base, pool.take())).collect();
    println!("  take(), {} times          = {}", SLOTS + 1, taken.join(", "));
    println!("  free()                   = {}", pool.free());
    // SAFETY: slot 2 is taken for the first call. The second breaks `give`'s contract on
    // purpose: a pool that can't tell puts the slot on its list twice, and stays in `memory`.
    unsafe {
        println!("  give(slot 2)             = {:?}", pool.give(base + 2 * SLOT));
        println!("  give(slot 2), again      = {:?}", pool.give(base + 2 * SLOT));
    }
    println!("  free()                   = {}", pool.free());
    let again = [slot(base, pool.take()), slot(base, pool.take())];
    println!("  take(), twice            = {}", again.join(", "));
}

fn main() {
    edges::<Bitmap>("Bitmap");
    edges::<FreeList>("FreeList");
    // Only the bitmap is asked about an address that is no slot's start: a free list
    // would write its link there.
    let mut memory = [0usize; SLOTS * SLOT / size_of::<usize>()];
    let base = memory.as_mut_ptr() as usize;
    // SAFETY: as in `edges`; a `Bitmap` never touches the slots.
    let mut pool = unsafe { Bitmap::new(base, SLOT, SLOTS, 0) };
    pool.take();
    println!("Bitmap, an address inside slot 0, and one past the last slot");
    unsafe {
        println!("  give(base + 8)           = {:?}", pool.give(base + 8));
        println!("  give(base + {})         = {:?}", SLOTS * SLOT, pool.give(base + SLOTS * SLOT));
    }
}
