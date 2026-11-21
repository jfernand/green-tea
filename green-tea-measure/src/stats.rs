//! A distribution, summarized without a mean standing in for it.

/// The `p`th percentile of `sorted`, by the nearest-rank rule: the smallest sample such
/// that at least `p` percent of the samples are no larger. `p` is in thousandths of a
/// percent, so that 99.9 is 99_900 and no floating point decides which sample it is.
///
/// # Panics
/// If `sorted` is empty or `p` is over 100_000.
pub fn percentile<T: Copy>(sorted: &[T], p: u32) -> T {
    assert!(!sorted.is_empty() && p <= 100_000);
    let rank = (sorted.len() as u128 * p as u128).div_ceil(100_000) as usize;
    sorted[rank.max(1) - 1]
}

/// What a set of samples looks like: the mean, and the samples at six ranks.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Summary {
    pub count: usize,
    pub mean: f64,
    pub min: u64,
    pub median: u64,
    pub p90: u64,
    pub p99: u64,
    pub p999: u64,
    pub max: u64,
}

impl Summary {
    /// Sorts `samples`.
    ///
    /// # Panics
    /// If there are none.
    pub fn of<T: Copy + Ord + Into<u64>>(samples: &mut [T]) -> Summary {
        samples.sort_unstable();
        let sum: u128 = samples.iter().map(|&s| s.into() as u128).sum();
        let at = |p| percentile(samples, p).into();
        Summary {
            count: samples.len(),
            mean: sum as f64 / samples.len() as f64,
            min: samples[0].into(),
            median: at(50_000),
            p90: at(90_000),
            p99: at(99_000),
            p999: at(99_900),
            max: samples[samples.len() - 1].into(),
        }
    }
}

/// Of all the pairs of one sample from `a` and one from `b`, how many have the sample
/// from `a` the smaller, and how many there are. If a reader had one run of each to go
/// on, this is how often it would say that `a` is the faster.
pub fn wins(a: &[u64], b: &[u64]) -> (usize, usize) {
    let won = a.iter().map(|x| b.iter().filter(|y| x < *y).count()).sum();
    (won, a.len() * b.len())
}
