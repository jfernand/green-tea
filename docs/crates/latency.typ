#import "../lib.typ": *

= green-tea-latency <crate-latency>

`green-tea-latency` measures how late the timer interrupt arrives. It asks for an interrupt every millisecond, 8192 times, and for each one records how long after its deadline the handler ran, in a histogram of 1 µs bins. It is the tool for phase 0 of the realtime plan in #repo("realtime.md"), "measure first", and the yardstick for phase 1, which replaces the APIC countdown with the CPU's TSC-deadline timer. It runs on x86_64 only, and refuses to compile for anything else.

The plan sets this tool's rule, and this chapter follows it: QEMU can show that the tool works, but its timings mean nothing. Every number below is from QEMU, and none of them is a latency result.

#spec(
  ("Source", [#repo("kernels/latency/src/main.rs"), 207 lines; #repo("kernels/latency/src/histogram.rs"), 120 lines, 87 without its tests]),
  ("Depends on", [`green-tea-bare` (@crate-bare), for the board, its timer and `x86_timer_capabilities`]),
  ("Used by", [nothing: it is a binary]),
  ("Tests", [the `latency` and `latency-deadline` kernels; the 3 tests in `histogram` never run (see Limits)]),
)

xtask builds one binary for two kernels: `latency` passes `test=countdown` on the boot command line, and `latency-deadline` passes `test=deadline`. The kernel reads the name with `Board::test_name` and asks for TSC-deadline mode only for `deadline`. Both kernels run on the QEMU x86_64 board and on the `pc` board. The features are `green` (the default), `black`, `white` and `board-pc`, passed to `green-tea-bare`.

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`histogram` (module)], [the bins and counters, apart from the hardware, so that they can be tested on the host],
  [`HISTOGRAM_BINS`], [64: bins 0 to 62 hold 1 µs each, and bin 63 holds everything 63 µs late or more],
  [`Histogram`], [the bins, and the public counters `samples`, `late_samples`, `overruns`, `missed_ticks`, `gap_events`, `gap_ticks` and `max_lateness_ticks`],
  [`Histogram::new()`], [an empty histogram, `const` so that it can sit in a static],
  [`record(now, deadline, period, ticks_per_ms)`], [counts one interrupt and returns the next deadline],
  [`bin(index)`], [one bin's count],
)

== One measurement

`kmain` initializes the board (without the framebuffer), prints what the CPU's timer can do, and starts the timer in the requested mode through `green-tea-bare`'s `init_x86_timer`. The capabilities come from `cpuid`: a local APIC (leaf 1, EDX bit 9), TSC-deadline mode (leaf 1, ECX bit 24) and an invariant TSC, one that runs at a constant rate through power states (leaf `0x8000_0007`, EDX bit 8). A fourth, `tsc_clock`, says whether the board's own clock is the TSC, which is true only on the `pc` board. TSC-deadline mode is `usable_tsc_deadline` only with all four. If the requested mode is refused, the kernel prints `UNSUPPORTED` with the reason and measures with the APIC countdown instead, so a run always produces a histogram and always says which timer made it.

The kernel then arms the first deadline, one period ahead, turns interrupts on and spins until the handler has counted 8192 samples. The handler, `on_tick`, reads the clock, records the sample and arms the deadline `record` returns. When the count is reached it arms none, and `kmain` prints the report.

== The histogram

#srcfile("kernels/latency/src/histogram.rs", lines: ((35, 44), (57, 57), (66, 81),), notes: ((38, [How late, in the board's ticks; a handler that runs early counts as 0.]), (41, [Whole microseconds, capped at bin 63.]), (57, [How many whole periods the interrupt missed.]), (68, [A gap: more than one period since the previous interrupt.]), (79, [The next deadline follows the original schedule, past any periods already missed.]),))[```rust
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
// …
    let skipped = lateness / period;
// …
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
```]

Deadlines are absolute: each is the previous deadline plus one period, not the time the handler ran plus one period. A late interrupt therefore doesn't push the rest of the schedule later, and each sample measures lateness against the same 1 ms grid. If an interrupt is more than a whole period late, the periods it skipped are counted in `missed_ticks`, the interrupt counts as one of the `overruns`, and the next deadline is the first grid point still ahead.

A *gap* is a stretch between two interrupts longer than one period. The plan wants lost time detected, because the laptop's firmware can take the CPU away with a System Management Interrupt (SMI) and the kernel can't prevent it. A gap is time the CPU didn't spend in the handler; it could be an SMI, and it could be anything else, which is why the report calls the gaps "unattributed" and adds that "a gap is not proof of an SMI". The gap total is printed only when the board's clock is the TSC. The CPU's own SMI counter, which the plan also asks for, isn't read: the report prints `smi_counter=unavailable`.

All arithmetic saturates or widens to `u128`, so a clock near `u64::MAX` gives a capped deadline instead of a wrapped one.

== Exercised by

The QEMU x86_64 board, in countdown mode. Its clock is the HPET, at 100000 ticks per millisecond:

#output(title: "cargo xtask kernel --kernel latency --target x86_64 --headless")[
```
green-tea-latency on x86_64
timer capabilities: local_apic=true tsc_deadline=false invariant_tsc=false tsc_clock=false usable_tsc_deadline=false
measurement: build=debug requested=ApicCountdown active=ApicCountdown cpu=0 apic_id=0 samples=8192 interval_ms=1 clock_ticks_per_ms=100000 smi_counter=unavailable
summary: samples=8192 late_samples=8192 overruns=354 missed_ticks=743
lateness: max_us=15487 gap_events=4300 gap_us=1039847
unattributed_tsc_gaps=unavailable (platform timer clock is not the TSC)
histogram: bins 0..62 are 1 us each; bin 63 is 63 us or more
…
PASS: 8192 samples with requested ApicCountdown, active ApicCountdown; histogram timings are measurements, not guaranteed bounds
```
]

The `pc` board in QEMU, asking for TSC-deadline mode. QEMU emulates the CPU in software here, and its emulated CPU reports neither TSC-deadline mode nor an invariant TSC, so the kernel falls back and says why:

#output(title: "cargo xtask kernel --board pc --kernel latency-deadline --target x86_64 --headless")[
```
green-tea-latency on x86_64 (pc)
timer capabilities: local_apic=true tsc_deadline=false invariant_tsc=false tsc_clock=true usable_tsc_deadline=false
UNSUPPORTED: requested TscDeadline: the CPU does not support TSC-deadline mode; falling back to APIC countdown
measurement: build=debug requested=TscDeadline active=ApicCountdown cpu=0 apic_id=0 samples=8192 interval_ms=1 clock_ticks_per_ms=2690449 smi_counter=unavailable
summary: samples=8192 late_samples=8192 overruns=688 missed_ticks=2244
lateness: max_us=103356 gap_events=4346 gap_us=2514747
unattributed_tsc_gaps=4346 accumulated_gap_us=2514747 (a gap is not proof of an SMI)
histogram: bins 0..62 are 1 us each; bin 63 is 63 us or more
histogram_us[0..7]=0,0,0,0,0,0,0,0
histogram_us[8..15]=0,0,0,0,0,0,0,0
histogram_us[16..23]=0,0,0,0,0,0,0,0
histogram_us[24..31]=0,0,0,0,0,0,1,0
histogram_us[32..39]=4,3,1,6,9,5,15,19
histogram_us[40..47]=33,49,84,116,136,156,236,242
histogram_us[48..55]=319,299,317,296,282,289,267,239
histogram_us[56..62,63+]=228,228,197,218,196,196,153,3353
PASS: 8192 samples with requested TscDeadline, active ApicCountdown; histogram timings are measurements, not guaranteed bounds
```
]

These runs show the tool doing its job end to end: the capability check, the fallback and its reason, 8192 samples on an absolute schedule, the counters, the bins and the report. They show nothing about interrupt latency. QEMU's CPU is emulated, it shared the host with other emulators while these ran, and a millisecond of the guest's time is not a millisecond of the host's. No sample was under 30 µs late, and 3353 of the 8192 were 63 µs late or more, which describes the emulator, not the timer.

== Limits

- *No run on the laptop.* Phase 0 passes with a histogram from the physical laptop, and there isn't one yet. Phase 1 passes when TSC-deadline mode narrows that histogram, and TSC-deadline mode has not been active in any run, in QEMU or elsewhere, because QEMU's emulated CPU doesn't offer it.
- *The histogram tests never run.* `histogram.rs` has three `#[test]` functions, but the crate's only target is a binary with `test = false`, and the crate isn't one of the workspace's `default-members`. `cargo test -p green-tea-latency` builds and runs nothing:

#output(title: "cargo test -p green-tea-latency")[
```
Finished `test` profile [unoptimized + debuginfo] target(s)
```
]

- A gap is any interval longer than one period, with no threshold. Two interrupts 50 µs and 52 µs late make a 2 µs gap, so ordinary jitter counts, and the gap counts above are about half the samples. A threshold, or the SMI counter, would be needed before a gap means lost time.
- No SMI counter: the report prints `smi_counter=unavailable`.
- The default build is `debug`, as the report's `build=` field shows. A measurement meant to count should use `--release`.
- The lateness includes everything between the deadline and the handler's first read of the clock: the interrupt's delivery, `green-tea-bare`'s trap entry, and its check that the deadline has passed. It is the latency a task woken by the timer would see, not the hardware's alone.
- One CPU, one period (1 ms), one sample count (8192), all constants. There is no load on other CPUs, which phase 6 asks for.

One period, 8192 samples, and a report that says which timer it used and what it can't see. The tool is ready; the laptop's histogram is the next step.
// chapter-status:begin
// stage: research-done
// revision: 1
// body: 61de738a8a62
// activity: 2026-11-27 | r1 | stage | green-tea | note | tracking started at research-done
// chapter-status:end
