#import "../lib.typ": *

= xtask <crate-xtask>

`xtask` is the workspace's automation program, run as `cargo xtask <command>` through an alias in `.cargo/config.toml`. It has four commands: `test` runs `green-tea-v2` across a matrix of targets, `kernel` builds the bare-metal kernels and judges their runs in QEMU, `shell` puts the shell kernel on the terminal, and `trace` gets an allocation trace out of a running kernel. It has no dependencies, and CI runs its `test` and `kernel` commands.

#spec(
  ("Source", [#repo("xtask/src/"), 3 files: `main.rs` (`test`), `kernel.rs` (`kernel`, `shell`), `kernel/capture.rs` (`trace`); 1,199 lines with their tests]),
  ("Depends on", [nothing but `std`. At run time: `cargo`, `rustup`, QEMU, and the cross linkers]),
  ("Used by", [`.github/workflows/ci.yml`, and by hand]),
  ("Tests", [13 host tests]),
)

`main` dispatches on the first argument to `test`, `kernel::main`, `kernel::shell` or `capture::main`. Each of the last three has its own `--help`, printed from `USAGE` (`SHELL_USAGE` for `shell`). `kernel` and `shell` share the table of kernels and machines, and `capture` borrows `build`, `preflight`, `target_dir` and the same tables.

== `cargo xtask test`

`test` runs `green-tea-v2`'s tests and its two demo binaries (`BINS`: `coop-2-v2`, `coop-n-v2`) on each entry of `TARGETS`, in `debug` and `release`:

#dtable(columns: (auto, auto, 1fr), ("Target", "Runs", "Needs"),
  [`host`], [tests, binaries], [nothing],
  [`aarch64-unknown-linux-gnu`], [tests, binaries], [`aarch64-linux-gnu-gcc`, `qemu-aarch64`],
  [`riscv64gc-unknown-linux-gnu`], [tests, binaries], [`riscv64-linux-gnu-gcc`, `qemu-riscv64`],
  [`aarch64-apple-darwin`], [builds the library only], [nothing: there is no Apple linker here, so this proves the assembly assembles for Mach-O],
)

`--target` and `--profile`, each repeatable, narrow the matrix. The cross targets run under QEMU user-mode emulation, through the `runner` lines of `.cargo/config.toml`. `preflight` first collects every missing Rust target and tool and reports them together, so that a long run doesn't stop at the first absent linker. Each step is then a plain `cargo` invocation, and a summary lists every step with `ok` or `FAIL`.

== `cargo xtask kernel`

`kernel` builds each chosen kernel for each chosen architecture and boots it in QEMU. A *kernel* here is a row of `KERNELS`: a package to build, the architectures it runs on, and how to make that binary do one job.

#srcfile("xtask/src/kernel.rs", lines: ((116, 127),), notes: ((122, [The `-fw_cfg` file QEMU exposes to the guest. `green-tea-coop` reads it and runs that test instead of its demo.]), (126, [Typed at the serial console, for the kernels that are a shell.]),))[```rust
struct Kernel {
    name: &'static str,
    package: &'static str,
    /// The architectures it runs on, by `Arch::name`.
    arches: &'static [&'static str],
    /// A test for the kernel to run instead of its demo: the fw_cfg file opt/green-tea/test.
    test: Option<&'static str>,
    /// Attach a virtio disk, from a fresh image (see `disk_image`).
    disk: bool,
    /// What to type at the serial console.
    input: Option<&'static str>,
}
```]

There are three packages behind the table's 25 rows, and the same binary serves many of them:

#dtable(columns: (auto, 1fr, auto), ("Package", "Kernels", "Told what to do by"),
  [`green-tea-coop`], [`coop`], [nothing: its demo],
  [`green-tea-preempt`], [`preempt`], [nothing: its demo],
  [`green-tea-coop`], [`overflow`#super[†], `disk`#super[‡], `memmap`, `pool`, `fit`, `frag`, `tlsf`, `slab`, `pages`#super[†], `smp`#super[§], `lisp`, `shard`#super[§], `gc`, `smr`#super[§], `gen`, `incremental`, `safety`], [`test`, via fw_cfg],
  [`green-tea-shell`], [`shell`, `trace`, `harden`, `region`, `gtt`, `resident`#super[†]], [`input`, typed],
)

#text(size: 8pt)[† riscv64 and x86_64 only: the kernel needs guard pages or demand paging, and aarch64 runs with its MMU off. ‡ With a virtio disk. § On a machine of four CPUs (`SMP_KERNELS`); every other kernel has one.]

Of the 25 rows, 22 run on all three architectures and 3 on two, so a full `cargo xtask kernel` is 72 runs. What the shell kernels type is in the table too, and starts with an empty line:

#srcfile("xtask/src/kernel.rs", lines: ((149, 158),), notes: ((155, [On riscv64 the firmware reads the serial port before the kernel does, and takes the first byte with it. The empty line is there to be lost.]),))[```rust
Kernel {
    name: "shell",
    package: "green-tea-shell",
    arches: ALL,
    test: None,
    disk: false,
    // The empty first line is there to be lost: on riscv64 the firmware reads the serial
    // port before the kernel does, and takes the first byte with it.
    input: Some("\necho hello from the serial port\ntest\n"),
},
```]

The other shell rows type `trace on`, `session`, `trace off` and then one command: `replay` for `trace`, `harden` for `harden`, `trace check` for `gtt`. `region` types `regions`, and `resident` a longer script around the `pages` command.

=== The machines

`ARCHES` holds one entry per architecture. All three get 128 MiB and a `ramfb` device, and a serial port written to a log file.

#dtable(columns: (auto, auto, 1fr), ("Name", "Rust target", "QEMU machine"),
  [`riscv64`], [`riscv64gc-unknown-none-elf`], [`-machine virt -bios default`: OpenSBI loads the kernel. virtio-mmio disk.],
  [`aarch64`], [`aarch64-unknown-none`], [`-machine virt,gic-version=2 -cpu cortex-a72`: no firmware, QEMU enters at EL1. virtio-mmio disk.],
  [`x86_64`], [`x86_64-unknown-none`], [`-machine q35 -vga none`: PVH boot, and ramfb is the screen. virtio-pci disk, modern interface only.],
)

On `virt`, `VIRTIO_MMIO_BLK` sets `force-legacy=false`, because QEMU otherwise makes virtio 0.9 devices and `green-tea-virtio` speaks only 1.0. The `disk` kernel gets a fresh 32 KiB image for each run from `disk_image`, in which every byte is `disk_pattern` of its sector and offset, so that a sector read from the wrong place doesn't match.

`Run::start` assembles the command line. A kernel with `input` gets a chardev with separate input and output paths, so that QEMU hands the UART a byte only when it has room, and nothing is lost if the kernel is slow to start reading. The QEMU monitor is on a pipe, which is how `xtask` takes a screenshot and quits a machine.

=== How a run is watched and judged

`watch` is the loop at the centre of the command. Every 100 ms it starts whatever fits, reads each machine's log, and looks for a verdict:

#srcfile("xtask/src/kernel.rs", lines: ((641, 679),), notes: ((645, [Jobs start strictly in order: a four-CPU machine at the front waits, and nothing behind it overtakes.]), (656, [The first verdict is the outcome, and it is set once.]), (662, [Headless, a verdict quits the machine, after any screendump: the monitor runs commands in order.]), (666, [Each run's 120 seconds count from its own start. With windows there is no limit.]), (673, [A machine that exits with no verdict has failed.]),))[```rust
let limit = options.limit.unwrap_or_else(|| default_limit(options.headless));
while !queue.is_empty() || runs.iter().any(|r| !r.exited) {
    let mut busy: usize = runs.iter().filter(|r| !r.exited).map(|r| r.job.cpus()).sum();
    while let Some(job) = queue.front() {
        if !fits(job.cpus(), busy, limit) {
            break;
        }
        busy += job.cpus();
        let job = queue.pop_front().expect("the queue has a front");
        let elf = target_dir().join(job.arch.triple).join(profile).join(job.kernel.package);
        runs.push(Run::start(job, &elf, out_dir, options.headless)?);
    }
    for run in runs.iter_mut().filter(|r| !r.exited) {
        let had_outcome = run.outcome.is_some();
        run.follow();
        if !had_outcome && run.outcome.is_some() {
            if options.screenshot {
                let png = out_dir.join(format!("{}.png", run.job.label()));
                run.monitor(&format!("screendump {} -f png", png.display()));
                eprintln!("[{:15}] screenshot: {}", run.job.label(), png.display());
            }
            if options.headless {
                run.monitor("quit"); // after the screendump: the monitor runs them in order
            }
        }
        if options.headless && run.outcome.is_none() && run.started.elapsed() > HEADLESS_TIMEOUT {
            run.outcome = Some(Err(format!("no result after {}s", HEADLESS_TIMEOUT.as_secs())));
            let _ = run.qemu.kill();
        }
        if let Ok(Some(status)) = run.qemu.try_wait() {
            run.follow(); // anything written just before exiting
            run.exited = true;
            if run.outcome.is_none() {
                run.outcome = Some(Err(format!("QEMU exited ({status}) before the kernel finished")));
            }
        }
    }
    thread::sleep(Duration::from_millis(100));
}
```]

The kernel judges itself. `xtask` only reads its last word:

#srcfile("xtask/src/kernel.rs", lines: ((623, 631),))[```rust
fn verdict(line: &str) -> Option<Result<String, String>> {
    if let Some(rest) = line.strip_prefix("panic: ") {
        return Some(Err(format!("kernel panicked: {rest}")));
    }
    if let Some(rest) = line.strip_prefix("PASS: ") {
        return Some(Ok(rest.to_string()));
    }
    line.strip_prefix("FAIL: ").map(|rest| Err(rest.to_string()))
}
```]

`Run::follow` re-reads the log each time, takes the complete lines it hasn't seen, and echoes them prefixed with the kernel and architecture. It starts echoing at the first line that begins `green-tea-`, which skips the firmware's banner. Here is a run, with the device listing cut:

#output(title: "cargo xtask kernel --kernel pool --target riscv64 --headless", size: 6.2pt)[
```
=== build green-tea-coop riscv64 debug
[pool-riscv64   ] green-tea-coop on riscv64
…
[pool-riscv64   ] a pool of 8 slots:        bitmap                     free list
[pool-riscv64   ]   taken from new:          [0, 1, 2, 3, 4, 5, 6, 7]   [0, 1, 2, 3, 4, 5, 6, 7]
…
[pool-riscv64   ] PASS: 1000 tasks ran on one stack; the bitmap caught a double give, the free list didn't
[pool-riscv64   ] halting

=== summary
ok    pool-riscv64    1000 tasks ran on one stack; the bitmap caught a double give, the free list didn't
```
]

The command succeeds only if every run has an `Ok` outcome. The logs stay in `target/kernel/<kernel>-<arch>.log`, and `--screenshot` saves the framebuffer next to them as a PNG when the verdict arrives.

=== The `--jobs` cap

`--jobs <n>` limits the *virtual CPUs* running at once, and a machine of four counts as four. Without it, windowed runs all start together, and headless runs use half of the host's CPUs:

#srcfile("xtask/src/kernel.rs", lines: ((264, 266), (635, 637),), notes: ((265, [The kernels that check their own timing need the CPU time they think they are getting. This is the reason for the default.]), (636, [A machine too big for the limit still runs, alone, so that a limit of 2 doesn't make a four-CPU kernel impossible.]),))[```rust
fn default_limit(headless: bool) -> usize {
    if headless { thread::available_parallelism().map_or(1, |n| n.get() / 2).max(1) } else { usize::MAX }
}
// …
fn fits(cpus: usize, busy: usize, limit: usize) -> bool {
    busy == 0 || busy.saturating_add(cpus) <= limit
}
```]

With windows, a cap means the next kernel starts when a window is closed.

== `cargo xtask shell`

`shell` builds `green-tea-shell` for one architecture (riscv64 unless `--target` says otherwise) and runs one QEMU in the foreground with `-serial mon:stdio`. That puts the terminal in raw mode, so the shell sees each key as it is typed, and keeps ctrl-a as QEMU's escape: `halt` stops the kernel, and ctrl-a x leaves QEMU. It has no verdict and no timeout, and its exit status is QEMU's.

== `cargo xtask trace`: the capture path

The kernel has no disk and no network to write a trace to. It has a serial port, and QEMU writes that to a file. `capture` types a script at the shell, waits for it to halt, and picks the trace out of the log. The script is an empty line, `trace on`, the commands, `trace off`, `replay`, `trace dump` and `halt`. The commands recorded are `session` unless `--type` gives others. The shell's `trace dump` writes the recording as a text trace file (@crate-fit) between a line `gtt 1` and a line `end`.

#srcfile("xtask/src/kernel/capture.rs", lines: ((161, 177),), notes: ((165, [The last command typed is `halt`, and the kernel's last word is `halting`.]), (168, [A panic ends the wait at once, with its message.]), (174, [300 seconds, against the 120 a `kernel` run gets.]),))[```rust
let text = loop {
    thread::sleep(Duration::from_millis(100));
    let text = fs::read(log).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).unwrap_or_default();
    // `halt` is the last command typed, and the kernel's last word is "halting".
    if text.lines().any(|line| line.trim_end_matches('\r') == "halting") {
        break Ok(text);
    }
    if let Some(panic) = text.lines().find(|line| line.starts_with("panic: ")) {
        break Err(format!("the kernel panicked: {}", panic.trim_end()));
    }
    if qemu.try_wait().ok().flatten().is_some() {
        break Err("QEMU exited before the shell halted".into());
    }
    if start.elapsed() > TIMEOUT {
        break Err(format!("the shell hasn't halted after {}s", TIMEOUT.as_secs()));
    }
};
```]

`extract` then takes the lines from `gtt 1` to `end`, strips the console's carriage returns, and counts the records:

#srcfile("xtask/src/kernel/capture.rs", lines: ((74, 95),), notes: ((83, [The count on the `end` line must be the records seen, so that a dump cut short is never saved as a trace.]), (88, [Comments and the header aren't records.]),))[```rust
pub fn extract(log: &str) -> Result<(String, usize), String> {
    let mut lines = log.lines().map(|line| line.trim_end_matches('\r')).skip_while(|line| *line != "gtt 1");
    let mut trace = String::new();
    let mut records = 0;
    for line in lines.by_ref() {
        trace.push_str(line);
        trace.push('\n');
        if let Some(end) = line.strip_prefix("end ") {
            let said = end.split(' ').next().and_then(|count| count.parse::<usize>().ok());
            if said != Some(records) {
                return Err(format!("the dump says `{line}`, and has {records} records"));
            }
            return Ok((trace, records));
        }
        records += matches!(line.as_bytes().first(), Some(b'a' | b'f' | b'r')) as usize;
    }
    if trace.is_empty() {
        Err("there is no trace in the serial log: no line `gtt 1`".into())
    } else {
        Err(format!("the dump stops after {records} records, without an `end` line"))
    }
}
```]

The checksum on that line is left to whoever reads the file, because `xtask` has no dependencies, `green-tea-fit` included. The trace goes to `--out` (by default `target/kernel/shell-<arch>.gtt`) and the whole serial log to the same path with `.log` added. The trace measured in @crate-trace and @crate-measure was made this way:

#output(title: "cargo xtask trace --target riscv64 --out shell.gtt")[
```
20483 records from green-tea-shell on riscv64 written to shell.gtt
the serial log is in shell.gtt.log
```
]

#output(title: "the file: its first lines, and its last")[
```
gtt 1
# recorded by green-tea-shell on riscv64
a 804e80b0 33 1
a 804e80f0 64 8
…
end 20483 f137558e1fe4dcb2
```
]

== Tests

The 13 tests are all in `kernel.rs` and `capture.rs`. They check the verdict of each kind of line, that `fits` respects the limit and lets an oversized machine run alone, that `--jobs` wants a positive number, that the default selection is what `KERNELS` says, that unsupported combinations are skipped, that the disk image differs between sectors, that `extract` refuses a dump with a line missing or cut short, and the script typed at the shell. Nothing tests `main.rs`, and the QEMU paths are exercised by running them.

#output(title: "cargo test -p xtask")[
```
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

== Limits

- *`cargo xtask --help` is stale.* Its synopsis of `kernel` has no `--jobs`, it gives `shell` no usage line, and its paragraph on `kernel` names the two kernel packages and four of coop's tests, six kernels of the 25:

#output(title: "cargo xtask --help", size: 6.4pt)[
```
       cargo xtask kernel [--kernel <name>]... [--target <arch>]... [--release] [--headless] [--screenshot]
…
kernel: builds the bare-metal kernels (green-tea-coop, green-tea-preempt) and boots them,
        and coop's stack overflow, disk, memory map and pool tests, in QEMU on riscv64,
        aarch64 and x86_64 (see cargo xtask kernel --help).
```
]

  `cargo xtask kernel --help` is current and lists all 25, though its closing list of options leaves out `--jobs`, which it explains above that list.
- *The package description is stale.* It reads "Project automation: cargo xtask test runs green-tea-v2 across targets." and mentions only the first of the four commands (`xtask/Cargo.toml:3`).

- *`green-tea-measure` is listed twice in `default-members`* (`Cargo.toml:25`). It is harmless: `cargo metadata` reports both entries, and `cargo test` still builds and runs the crate's test binaries once.

#output(title: "cargo metadata, counted")[
```
15 default members, 14 different; green-tea-measure is listed 2 times
```
]

- *`cargo xtask test --help` is an error.* `parse_test_args` reads every argument as an option with a value, so `--help` is told it needs one (`main.rs:85`), and the command exits with status 1:

#output(title: "cargo xtask test --help")[
```
error: --help needs a value
exit status 1
```
]

- *A kernel named twice runs twice, on one log.* `parse` doesn't remove duplicates, and the log path comes from the label. With `--jobs 1` the two runs follow each other and the second overwrites the first's log. Without it they run together: the second start unlinks the first's log and makes a new one, and both runs then follow that file, so the first's verdict can come from the second's output (`kernel.rs:540-541`).

- The first verdict is final. A kernel that prints `PASS:` and then panics is reported as passed, since later lines aren't judged, and headless it has been told to quit.
- A run with windows has no time limit: a kernel that never reports keeps the command waiting until its window is closed.
- `=== build` is printed for every job, including those whose binary is already built and is skipped (`kernel.rs:296`).
- `target_dir` honours `CARGO_TARGET_DIR` and nothing else. With a `build.target-dir` in a Cargo configuration, `xtask` would look for the kernels in the wrong place.
- `capture` checks the count of records and not the checksum.
- `cargo xtask shell --target` with no value says `unknown option --target`.

Four commands, and one rule: the kernel says PASS or FAIL, and `xtask` believes it. @app-xtask lists every command and option.
