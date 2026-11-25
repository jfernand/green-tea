//! A program written to fragment its allocator.
//!
//! It never has more than `budget` bytes allocated, and it asks for blocks of `smallest`
//! bytes, then twice that, and so on up to `largest`. Before each doubling it looks at the
//! addresses of the blocks it holds and frees as many as it can without leaving a hole
//! large enough for a block of the next size. The blocks it keeps pin the memory around
//! them: the holes between them are free, and useless to everything the program will ask
//! for from then on. So each round of larger blocks has to go in memory that has never
//! been used.
//!
//! This is the argument behind J. M. Robson's lower bound on the memory any allocator
//! needs, run as a program. It works on any allocator that can't move a block, because it
//! decides what to free after it has seen where the allocator put things.

use crate::{ALIGN, Fit, MIN_BLOCK, OVERHEAD};
use alloc::vec::Vec;

/// One round: the size asked for, and where things stood after its allocations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Round {
    pub size: usize,
    /// Blocks of smaller sizes kept from the rounds before, and their bytes.
    pub kept: usize,
    pub kept_bytes: usize,
    /// Blocks of this size allocated.
    pub allocated: usize,
    /// Bytes the program holds, by its own count: what it asked for.
    pub live: usize,
    pub footprint: usize,
}

/// Run the adversary against `fit`, which should be new, and return one [`Round`] per size.
/// Stops early if the allocator refuses an allocation.
pub fn run(fit: &mut Fit, budget: usize, smallest: usize, largest: usize) -> Vec<Round> {
    let mut rounds = Vec::new();
    let mut held: Vec<(usize, usize)> = Vec::new(); // address and size asked for, in address order
    let mut live = 0;
    let mut size = smallest;
    while size <= largest {
        // The block the allocator will need for this size: no hole may be that large.
        let need = block_size(size);
        let mut kept = Vec::with_capacity(held.len());
        let mut hole = fit.first(); // where the hole being built up begins
        for (i, &(addr, held_size)) in held.iter().enumerate() {
            let block = addr - OVERHEAD / 2;
            // Without this block, the hole would run to the next block we hold.
            let next = held.get(i + 1).map_or(usize::MAX, |&(next, _)| next - OVERHEAD / 2);
            if next - hole >= need {
                kept.push((addr, held_size));
                hole = block + block_size(held_size);
            } else {
                // SAFETY: `addr` came from `fit.allocate` and is freed once.
                unsafe { fit.free(addr) }.expect("the adversary freed a block twice");
                live -= held_size;
            }
        }
        let (kept_blocks, kept_bytes) = (kept.len(), live);
        held = kept;
        let mut allocated = 0;
        while live + size <= budget {
            let Some(addr) = fit.allocate(size, 1) else {
                return rounds;
            };
            held.push((addr, size));
            live += size;
            allocated += 1;
        }
        held.sort_unstable();
        rounds.push(Round { size, kept: kept_blocks, kept_bytes, allocated, live, footprint: fit.footprint() });
        size *= 2;
    }
    rounds
}

/// The block [`Fit`] makes for a request of `size` bytes.
pub fn block_size(size: usize) -> usize {
    ((size + OVERHEAD + ALIGN - 1) & !(ALIGN - 1)).max(MIN_BLOCK)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::{block_size, run};
    use crate::{Fit, Order, Policy};
    use std::vec;

    #[test]
    fn every_round_of_larger_blocks_goes_in_new_memory() {
        for policy in [Policy::FirstFit, Policy::NextFit, Policy::BestFit] {
            for order in [Order::Lifo, Order::Address] {
                let mut memory = vec![0u128; (1 << 20) / 16];
                let mut fit = unsafe { Fit::new(memory.as_mut_ptr() as usize, 1 << 20, policy, order) };
                let rounds = run(&mut fit, 16 * 1024, 16, 1024);
                fit.check().unwrap();
                assert_eq!(rounds.len(), 7, "{policy:?} {order:?}");
                for pair in rounds.windows(2) {
                    let grew = pair[1].footprint - pair[0].footprint;
                    assert!(pair[1].live <= 16 * 1024);
                    assert_eq!(grew, pair[1].allocated * block_size(pair[1].size), "{policy:?} {order:?}: {pair:?}");
                }
                assert!(rounds[6].footprint > 3 * 16 * 1024, "{policy:?} {order:?}: {rounds:?}");
            }
        }
    }
}
