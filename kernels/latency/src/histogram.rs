pub const HISTOGRAM_BINS: usize = 64;

#[derive(Clone, Copy)]
pub struct Histogram {
    bins: [u64; HISTOGRAM_BINS],
    pub samples: u64,
    pub late_samples: u64,
    pub overruns: u64,
    pub missed_ticks: u64,
    pub gap_events: u64,
    pub gap_ticks: u64,
    pub max_lateness_ticks: u64,
    last_tick: Option<u64>,
}

impl Histogram {
    pub const fn new() -> Self {
        Self {
            bins: [0; HISTOGRAM_BINS],
            samples: 0,
            late_samples: 0,
            overruns: 0,
            missed_ticks: 0,
            gap_events: 0,
            gap_ticks: 0,
            max_lateness_ticks: 0,
            last_tick: None,
        }
    }

    /// Record one interrupt and return the next absolute deadline. Bin 63 represents
    /// lateness of 63 microseconds or more; `ticks_per_ms` converts the platform clock.
    /// An overrun is an interrupt at least one whole period late; missed ticks count how
    /// many complete periods elapsed beyond the intended deadline.
    pub fn record(&mut self, now: u64, deadline: u64, period: u64, ticks_per_ms: u64) -> u64 {
        let period = period.max(1);
        let ticks_per_ms = ticks_per_ms.max(1);
        let lateness = now.saturating_sub(deadline);
        let lateness_us = (u128::from(lateness) * 1000 / u128::from(ticks_per_ms))
            .min(u128::from(u64::MAX)) as u64;
        let bin = usize::try_from(lateness_us)
            .unwrap_or(usize::MAX)
            .min(HISTOGRAM_BINS - 1);
        self.bins[bin] = self.bins[bin].saturating_add(1);
        self.samples = self
            .samples
            .saturating_add(1);
        self.max_lateness_ticks = self
            .max_lateness_ticks
            .max(lateness);
        if lateness > 0 {
            self.late_samples = self
                .late_samples
                .saturating_add(1);
        }

        let skipped = lateness / period;
        self.missed_ticks = self
            .missed_ticks
            .saturating_add(skipped);
        if skipped > 0 {
            self.overruns = self
                .overruns
                .saturating_add(1);
        }
        if let Some(last_tick) = self.last_tick {
            let elapsed = now.saturating_sub(last_tick);
            if elapsed > period {
                self.gap_events = self
                    .gap_events
                    .saturating_add(1);
                self.gap_ticks = self
                    .gap_ticks
                    .saturating_add(elapsed - period);
            }
        }
        self.last_tick = Some(now);

        let advance = (u128::from(skipped) + 1) * u128::from(period);
        u64::try_from(u128::from(deadline) + advance).unwrap_or(u64::MAX)
    }

    pub fn bin(&self, index: usize) -> u64 {
        self.bins[index]
    }
}

#[cfg(test)]
mod tests {
    use super::{HISTOGRAM_BINS, Histogram};

    #[test]
    fn records_on_time_and_sub_microsecond_samples() {
        let mut histogram = Histogram::new();
        assert_eq!(histogram.record(1000, 1000, 1000, 2000), 2000);
        assert_eq!(histogram.record(2001, 2000, 1000, 2000), 3000);
        assert_eq!(histogram.samples, 2);
        assert_eq!(histogram.late_samples, 1);
        assert_eq!(histogram.bin(0), 2);
        assert_eq!(histogram.missed_ticks, 0);
    }

    #[test]
    fn advances_absolute_deadlines_and_accounts_for_misses_and_gaps() {
        let mut histogram = Histogram::new();
        assert_eq!(histogram.record(1000, 1000, 1000, 1000), 2000);
        assert_eq!(histogram.record(4500, 2000, 1000, 1000), 5000);
        assert_eq!(histogram.missed_ticks, 2);
        assert_eq!(histogram.overruns, 1);
        assert_eq!(histogram.gap_events, 1);
        assert_eq!(histogram.gap_ticks, 2500);
        assert_eq!(histogram.bin(HISTOGRAM_BINS - 1), 1);
    }

    #[test]
    fn deadline_arithmetic_saturates_instead_of_wrapping() {
        let mut histogram = Histogram::new();
        assert_eq!(histogram.record(u64::MAX, u64::MAX - 5, 10, 1000), u64::MAX);
    }
}
