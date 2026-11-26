#![no_std]
#![no_main]

#[cfg(not(target_arch = "x86_64"))]
compile_error!("green-tea-latency only supports x86_64");

use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, Ordering};
use green_tea_bare::platform::{self, X86TimerMode};
use green_tea_bare::{Board, Global, Platform, TimerInterrupt, println};

mod histogram;

const SAMPLES: u64 = 8192;
const INTERVAL_MS: u64 = 1;

#[derive(Clone, Copy)]
struct Measurement {
    histogram: histogram::Histogram,
    deadline: u64,
    period: u64,
    ticks_per_ms: u64,
}

impl Measurement {
    const fn new() -> Self {
        Self {
            histogram: histogram::Histogram::new(),
            deadline: 0,
            period: 0,
            ticks_per_ms: 0,
        }
    }

    fn record(&mut self, now: u64) -> (u64, bool) {
        let next = self
            .histogram
            .record(now, self.deadline, self.period, self.ticks_per_ms);
        self.deadline = next;
        let done = self
            .histogram
            .samples
            == SAMPLES;
        (if done { u64::MAX } else { next }, done)
    }
}

static MEASUREMENT: Global<Measurement> = Global::new(Measurement::new());
static COMPLETE: AtomicBool = AtomicBool::new(false);

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    Board::init();
    println!("green-tea-latency on {}", Board::NAME);

    let capabilities = platform::x86_timer_capabilities();
    let requested = requested_mode();
    println!(
        "timer capabilities: local_apic={} tsc_deadline={} invariant_tsc={} tsc_clock={} usable_tsc_deadline={}",
        capabilities.local_apic,
        capabilities.tsc_deadline,
        capabilities.invariant_tsc,
        capabilities.tsc_clock,
        capabilities.usable_tsc_deadline,
    );

    let active = match platform::init_x86_timer(requested, on_tick) {
        Ok(()) => requested,
        Err(reason) => {
            println!(
                "UNSUPPORTED: requested {:?}: {reason}; falling back to APIC countdown",
                requested
            );
            platform::init_x86_timer(X86TimerMode::ApicCountdown, on_tick)
                .expect("APIC countdown timer");
            X86TimerMode::ApicCountdown
        }
    };

    let period = INTERVAL_MS
        .saturating_mul(Board::ticks_per_ms())
        .max(1);
    let deadline = Board::ticks().saturating_add(period);
    MEASUREMENT.with(|measurement| {
        *measurement = Measurement {
            histogram: histogram::Histogram::new(),
            deadline,
            period,
            ticks_per_ms: Board::ticks_per_ms(),
        };
    });
    COMPLETE.store(false, Ordering::Relaxed);
    println!(
        "measurement: build={} requested={:?} active={:?} cpu={} apic_id={} samples={} interval_ms={} clock_ticks_per_ms={} smi_counter=unavailable",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        requested,
        active,
        Board::cpu(),
        platform::x86_apic_id(),
        SAMPLES,
        INTERVAL_MS,
        Board::ticks_per_ms(),
    );

    Board::set_deadline(deadline);
    Board::interrupts_on();
    while !COMPLETE.load(Ordering::Acquire) {
        spin_loop();
    }
    Board::interrupts_off();
    Board::set_deadline(u64::MAX);
    let result = MEASUREMENT.with(|measurement| *measurement);
    report(
        requested,
        active,
        capabilities.tsc_clock,
        result.histogram,
        result.ticks_per_ms,
    );
    Board::halt()
}

fn requested_mode() -> X86TimerMode {
    let mut name = [0; 16];
    let len = Board::test_name(&mut name).min(name.len());
    if &name[..len] == b"deadline" {
        X86TimerMode::TscDeadline
    } else {
        X86TimerMode::ApicCountdown
    }
}

fn on_tick() {
    let now = Board::ticks();
    let (deadline, done) = MEASUREMENT.with(|measurement| measurement.record(now));
    Board::set_deadline(deadline);
    if done {
        COMPLETE.store(true, Ordering::Release);
    }
}

fn report(
    requested: X86TimerMode,
    active: X86TimerMode,
    tsc_clock: bool,
    histogram: histogram::Histogram,
    ticks_per_ms: u64,
) {
    println!(
        "summary: samples={} late_samples={} overruns={} missed_ticks={}",
        histogram.samples, histogram.late_samples, histogram.overruns, histogram.missed_ticks,
    );
    println!(
        "lateness: max_us={} gap_events={} gap_us={}",
        ticks_to_us(histogram.max_lateness_ticks, ticks_per_ms),
        histogram.gap_events,
        ticks_to_us(histogram.gap_ticks, ticks_per_ms),
    );
    if tsc_clock {
        println!(
            "unattributed_tsc_gaps={} accumulated_gap_us={} (a gap is not proof of an SMI)",
            histogram.gap_events,
            ticks_to_us(histogram.gap_ticks, ticks_per_ms)
        );
    } else {
        println!("unattributed_tsc_gaps=unavailable (platform timer clock is not the TSC)");
    }
    println!("histogram: bins 0..62 are 1 us each; bin 63 is 63 us or more");
    for group in 0..7 {
        let start = group * 8;
        println!(
            "histogram_us[{start}..{}]={},{},{},{},{},{},{},{}",
            start + 7,
            histogram.bin(start),
            histogram.bin(start + 1),
            histogram.bin(start + 2),
            histogram.bin(start + 3),
            histogram.bin(start + 4),
            histogram.bin(start + 5),
            histogram.bin(start + 6),
            histogram.bin(start + 7),
        );
    }
    println!(
        "histogram_us[56..62,63+]={},{},{},{},{},{},{},{}",
        histogram.bin(56),
        histogram.bin(57),
        histogram.bin(58),
        histogram.bin(59),
        histogram.bin(60),
        histogram.bin(61),
        histogram.bin(62),
        histogram.bin(63),
    );
    println!(
        "PASS: {} samples with requested {:?}, active {:?}; histogram timings are measurements, not guaranteed bounds",
        histogram.samples, requested, active,
    );
}

fn ticks_to_us(ticks: u64, ticks_per_ms: u64) -> u64 {
    (u128::from(ticks) * 1000 / u128::from(ticks_per_ms.max(1))).min(u128::from(u64::MAX)) as u64
}
