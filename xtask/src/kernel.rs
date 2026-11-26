//! `cargo xtask kernel`: build the bare-metal kernels for each architecture they support
//! and run them in QEMU: with windows all at once, headless a few at a time.
//!
//! Every QEMU gets its serial port written to a log file and its monitor on a pipe. The
//! log is followed to echo the kernel's output (prefixed with kernel and architecture)
//! and to spot its verdict: a line starting with "PASS: " or "FAIL: ", or a panic. The
//! monitor is used to take a screenshot and, when headless, to quit.

use crate::{on_path, workspace_root};
use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::ptr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use std::{env, thread};

pub mod capture;

pub const USAGE: &str = "\
usage: cargo xtask kernel [--kernel <name>]... [--target riscv64|aarch64|x86_64]...
                          [--release] [--headless] [--screenshot] [--jobs <n>]
                          [--board qemu|pc]

Builds the bare-metal kernels and boots each in QEMU on every architecture it supports,
one window apiece, all at once. Close the windows to finish. The kernels' serial output
is echoed here, and the run passes when every kernel reports PASS.

--headless runs them without windows, using at most half of this machine's CPUs. --jobs
sets how many virtual CPUs may run at once, with or without windows; a kernel with four
counts as four.

  coop      green-tea-coop: tasks yield to each other (riscv64, aarch64, x86_64)
  protected green-tea-black: x86_64 ring-3 process, mediated exit, and contained fault checks
  rt        green-tea-rt: fixed-priority preemption, timed wakeups, inheritance and bounded locks
  preempt   green-tea-preempt: a timer interrupt preempts tasks that never yield, and one
            that sleeps between steps (riscv64, aarch64, x86_64)
  latency   green-tea-latency: an 8192-sample APIC-countdown timer latency histogram (x86_64)
  latency-deadline  the same histogram using capability-checked TSC-deadline mode (x86_64)
  overflow  green-tea-coop's test that a task overflowing its stack hits its guard page
            (riscv64, x86_64: the boards that page)
  disk      green-tea-coop's test of the virtio-blk driver: reads back a disk image made
            for it, and writes a sector (virtio-mmio on riscv64 and aarch64, virtio-pci
            on x86_64)
  memmap    green-tea-coop's test of its own memory map: prints where the linker put each
            section, and checks their order, alignment and size (riscv64, aarch64, x86_64)
  pool      green-tea-coop's test of pools: a bitmap and a free list given the same takes
            and gives, and a thousand tasks run on one reused stack (riscv64, aarch64,
            x86_64)
  fit       green-tea-coop's test of green-tea-fit, the allocator behind the kernel's heap:
            freed blocks merge, first fit, next fit and best fit choose differently, and a
            thousand tasks leave the heap as they found it (riscv64, aarch64, x86_64)
  frag      green-tea-coop's fragmentation test: a program written to fragment its
            allocator makes every policy need several times what it holds, and a random
            workload is replayed against each (riscv64, aarch64, x86_64)
  tlsf      green-tea-coop's test of the allocators that don't search: a buddy system and
            TLSF, which is the kernel's heap, against first fit and best fit on the same
            requests, and on a heap built to make a search long (riscv64, aarch64, x86_64)
  slab      green-tea-coop's test of the kernel's own allocators: page frames from a buddy
            system, a slab cache of constructed objects on top, and the cache its tasks'
            records come from (riscv64, aarch64, x86_64)
  pages     green-tea-coop's test of demand paging: the heap's pages are mapped as they are
            touched, a megabyte is reserved, written, freed and given back, and small
            blocks pin the pages they are on (riscv64, x86_64: the boards that page)
  smp       green-tea-coop's test of allocation from several CPUs: starts the machine's
            other three, and has all four use the kernel's heap at once, behind its lock,
            and then a heap with a cache for each CPU in front of it (riscv64, aarch64,
            x86_64)
  lisp      green-tea-coop's test of green-tea-lisp: a small Lisp on a heap that counts
            references, with every allocation, free and count operation counted, a list
            freed three ways, and cycles leaked and collected (riscv64, aarch64, x86_64)
  shard     green-tea-coop's test of allocation without a lock: four CPUs allocate through
            a heap of pages each, where every page has its own free list and a second
            list that other CPUs push to, against the caches of the smp test on the same
            work (riscv64, aarch64, x86_64)
  gc        green-tea-coop's test of the heaps that trace: the same Lisp on a heap that
            marks and sweeps, one that marks and compacts and one that copies, with every
            collection counted, and the smallest heap each program runs in (riscv64,
            aarch64, x86_64)
  smr       green-tea-coop's test of safe memory reclamation: a lock-free stack and queue
            with their nodes on the heap, a pop interrupted where freeing a node at once
            corrupts the stack, and four CPUs sharing both with epochs and with hazard
            pointers (riscv64, aarch64, x86_64)
  gen       green-tea-coop's test of a heap with two generations: the same Lisp with a
            nursery in front of an old space, a write barrier and a remembered set, with
            what dies in the nursery and what the barrier records counted (riscv64,
            aarch64, x86_64)
  incremental  green-tea-coop's test of a heap that marks and sweeps in steps between the
            steps of the same Lisp: an object freed in use without a write barrier and
            kept by either of two, the longest step against a whole cycle, and steps
            paced by a timer interrupt (riscv64, aarch64, x86_64)
  shell     green-tea-shell: types two commands at the shell on the serial console; the
            second is its `test`, which checks that commands free what they allocate,
            from the heap or from an arena
            (riscv64, aarch64, x86_64). To type at it yourself: cargo xtask shell
  trace     green-tea-shell again: records the heap while the shell runs a script of
            commands, and replays the recording against each policy (riscv64, aarch64,
            x86_64)
  safety    green-tea-coop's heap-safety test: a task writes to a freed block and another
            is handed memory that isn't free; then the allocator hardened one defence at a
            time, and what each catches and costs (riscv64, aarch64, x86_64)
  harden    green-tea-shell once more: the recorded trace replayed against the hardened
            allocator, one defence more each time (riscv64, aarch64, x86_64)
  region    green-tea-shell again: runs the same script with its commands' memory in a
            region and its variables in another, and compares what each way asks of the
            heap (riscv64, aarch64, x86_64)
  gtt       green-tea-shell again: records the same script, writes the recording as a
            trace file in both forms, and reads it back (riscv64, aarch64, x86_64). To get
            the file out of QEMU: cargo xtask trace
  resident  green-tea-shell again: records the same script, and replays the recording in a
            region with no page mapped, counting the pages each allocator touches
            (riscv64, x86_64: the boards that page)

  --kernel      only these kernels (default: all)
  --target      only these architectures (default: all three)
  --release     build with optimizations
  --headless    no windows: quit each QEMU as soon as its kernel finishes
  --jobs <n>    at most this many virtual CPUs at once
  --screenshot  save the final screen to target/kernel/<kernel>-<arch>.png
  --board pc    build for a physical x86_64 PC instead of QEMU's machine, and test it in
                QEMU with UEFI firmware. Copy target/kernel/pc/<kernel>-x86_64/EFI to a
                FAT USB stick to boot it. The disk kernel uses NVMe, and refuses any disk
                but the test image";

/// What the kernels are built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Board {
    /// The machine QEMU emulates for each architecture.
    Qemu,
    /// A physical x86_64 PC, booted by GRUB from UEFI.
    Pc,
}

/// UEFI firmware for QEMU, from Debian and Ubuntu's `ovmf` package.
const OVMF_CODE: &str = "/usr/share/OVMF/OVMF_CODE_4M.fd";
const OVMF_VARS: &str = "/usr/share/OVMF/OVMF_VARS_4M.fd";

struct Kernel {
    name: &'static str,
    package: &'static str,
    /// The architectures it runs on, by `Arch::name`.
    arches: &'static [&'static str],
    /// A test for the kernel to run instead of its demo: the fw_cfg file opt/green-tea/test.
    test: Option<&'static str>,
    /// Attach a disk (virtio, or NVMe on the pc board), from a fresh image (see `disk_image`).
    disk: bool,
    /// What to type at the serial console.
    input: Option<&'static str>,
}

const KERNELS: &[Kernel] = &[
    Kernel { name: "coop", package: "green-tea-coop", arches: ALL, test: None, disk: false, input: None },
    Kernel { name: "protected", package: "green-tea-black", arches: &["x86_64"], test: None, disk: false, input: None },
    Kernel { name: "rt", package: "green-tea-rt-kernel", arches: ALL, test: None, disk: false, input: None },
    Kernel { name: "preempt", package: "green-tea-preempt", arches: ALL, test: None, disk: false, input: None },
    Kernel { name: "latency", package: "green-tea-latency", arches: &["x86_64"], test: Some("countdown"), disk: false, input: None },
    Kernel { name: "latency-deadline", package: "green-tea-latency", arches: &["x86_64"], test: Some("deadline"), disk: false, input: None },
    // Needs a board whose guard pages fault: not aarch64 (MMU off), so far.
    Kernel { name: "overflow", package: "green-tea-coop", arches: &["riscv64", "x86_64"], test: Some("overflow"), disk: false, input: None },
    Kernel { name: "disk", package: "green-tea-coop", arches: ALL, test: Some("disk"), disk: true, input: None },
    Kernel { name: "memmap", package: "green-tea-coop", arches: ALL, test: Some("memmap"), disk: false, input: None },
    Kernel { name: "pool", package: "green-tea-coop", arches: ALL, test: Some("pool"), disk: false, input: None },
    Kernel { name: "fit", package: "green-tea-coop", arches: ALL, test: Some("fit"), disk: false, input: None },
    Kernel { name: "frag", package: "green-tea-coop", arches: ALL, test: Some("frag"), disk: false, input: None },
    Kernel { name: "tlsf", package: "green-tea-coop", arches: ALL, test: Some("tlsf"), disk: false, input: None },
    Kernel { name: "slab", package: "green-tea-coop", arches: ALL, test: Some("slab"), disk: false, input: None },
    Kernel { name: "pages", package: "green-tea-coop", arches: &["riscv64", "x86_64"], test: Some("pages"), disk: false, input: None },
    Kernel { name: "smp", package: "green-tea-coop", arches: ALL, test: Some("smp"), disk: false, input: None },
    Kernel { name: "lisp", package: "green-tea-coop", arches: ALL, test: Some("lisp"), disk: false, input: None },
    Kernel { name: "shard", package: "green-tea-coop", arches: ALL, test: Some("shard"), disk: false, input: None },
    Kernel { name: "gc", package: "green-tea-coop", arches: ALL, test: Some("gc"), disk: false, input: None },
    Kernel { name: "smr", package: "green-tea-coop", arches: ALL, test: Some("smr"), disk: false, input: None },
    Kernel { name: "gen", package: "green-tea-coop", arches: ALL, test: Some("gen"), disk: false, input: None },
    Kernel { name: "incremental", package: "green-tea-coop", arches: ALL, test: Some("incremental"), disk: false, input: None },
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
    Kernel {
        name: "trace",
        package: "green-tea-shell",
        arches: ALL,
        test: None,
        disk: false,
        input: Some("\ntrace on\nsession\ntrace off\nreplay\n"),
    },
    Kernel { name: "safety", package: "green-tea-coop", arches: ALL, test: Some("safety"), disk: false, input: None },
    Kernel {
        name: "harden",
        package: "green-tea-shell",
        arches: ALL,
        test: None,
        disk: false,
        input: Some("\ntrace on\nsession\ntrace off\nharden\n"),
    },
    Kernel { name: "region", package: "green-tea-shell", arches: ALL, test: None, disk: false, input: Some("\nregions\n") },
    Kernel { name: "gtt", package: "green-tea-shell", arches: ALL, test: None, disk: false, input: Some("\ntrace on\nsession\ntrace off\ntrace check\n") },
    Kernel { name: "interrupts", package: "green-tea-shell", arches: &["x86_64"], test: None, disk: false, input: Some("\nirq\n\u{1}irq test\n") },
    Kernel {
        name: "resident",
        package: "green-tea-shell",
        arches: &["riscv64", "x86_64"],
        test: None,
        disk: false,
        input: Some("\npages\ntrace on\npages\nsession\ntrace off\npages\npages replay\npages trim\n"),
    },
];

const ALL: &[&str] = &["riscv64", "aarch64", "x86_64"];

/// The kernels that are given more than one CPU (`-smp`). Every other kernel runs on a
/// machine with one.
const SMP_KERNELS: &[(&str, usize)] = &[("smp", 4), ("shard", 4), ("smr", 4)];

/// The kernels that only make sense on the `pc` board.
const PC_KERNELS: &[&str] = &["interrupts"];

struct Arch {
    name: &'static str,
    triple: &'static str,
    qemu: &'static str,
    /// The machine, plus whatever that machine needs to boot our ELF and show only ramfb.
    machine: &'static [&'static str],
    /// How to plug in a virtio disk, given `-drive id=disk`.
    virtio_blk: &'static [&'static str],
}

/// On `virt`, a virtio-mmio slot. QEMU makes those legacy (virtio 0.9) devices unless
/// told otherwise, and green-tea-virtio speaks only virtio 1.0.
const NVME_DISK: &[&str] = &["-device", "nvme,drive=disk,serial=green-tea-disk"];
const VIRTIO_MMIO_BLK: &[&str] = &["-global", "virtio-mmio.force-legacy=false", "-device", "virtio-blk-device,drive=disk,serial=green-tea-disk"];

const ARCHES: &[Arch] = &[
    Arch {
        name: "riscv64",
        triple: "riscv64gc-unknown-none-elf",
        qemu: "qemu-system-riscv64",
        machine: &["-machine", "virt", "-bios", "default"], // OpenSBI loads the kernel
        virtio_blk: VIRTIO_MMIO_BLK,
    },
    Arch {
        name: "aarch64",
        triple: "aarch64-unknown-none",
        qemu: "qemu-system-aarch64",
        // No firmware: QEMU jumps in at EL1. green-tea-bare drives a GICv2 (the default).
        machine: &["-machine", "virt,gic-version=2", "-cpu", "cortex-a72"],
        virtio_blk: VIRTIO_MMIO_BLK,
    },
    Arch {
        name: "x86_64",
        triple: "x86_64-unknown-none",
        qemu: "qemu-system-x86_64",
        machine: &["-machine", "q35", "-vga", "none"], // PVH boot; no VGA so ramfb is the screen
        // A PCI device, modern interface only.
        virtio_blk: &["-device", "virtio-blk-pci,drive=disk,serial=green-tea-disk,disable-legacy=on"],
    },
];

const HEADLESS_TIMEOUT: Duration = Duration::from_secs(120);

/// One kernel on one architecture.
struct Job {
    kernel: &'static Kernel,
    arch: &'static Arch,
}

impl Job {
    fn label(&self) -> String {
        format!("{}-{}", self.kernel.name, self.arch.name)
    }

    /// How many virtual CPUs its machine has.
    fn cpus(&self) -> usize {
        SMP_KERNELS.iter().find(|(name, _)| *name == self.kernel.name).map_or(1, |(_, cpus)| *cpus)
    }
}

struct Options {
    jobs: Vec<Job>,
    release: bool,
    headless: bool,
    screenshot: bool,
    board: Board,
    /// How many virtual CPUs may run at once, if `--jobs` said.
    limit: Option<usize>,
}

/// With windows, everything runs at once, to be watched. Headless, half of the host's CPUs:
/// the kernels that check their timing need the CPU time they think they are getting.
fn default_limit(headless: bool) -> usize {
    if headless { thread::available_parallelism().map_or(1, |n| n.get() / 2).max(1) } else { usize::MAX }
}

pub fn main(args: &[String]) -> ExitCode {
    let mut options = match parse(args) {
        Ok(o) => o,
        Err(e) if e.is_empty() => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let arches = options.jobs.iter().fold(Vec::<&Arch>::new(), |mut v, j| {
        if !v.iter().any(|a| a.name == j.arch.name) {
            v.push(j.arch);
        }
        v
    });
    if let Err(problems) = preflight(&arches, options.board) {
        eprintln!("missing prerequisites:");
        for p in problems {
            eprintln!("  - {p}");
        }
        return ExitCode::FAILURE;
    }

    let profile = if options.release { "release" } else { "debug" };
    for job in builds(&options.jobs) {
        eprintln!("=== build {} {} {profile}", job.kernel.package, job.arch.name);
        if !build(job, options.release, options.board) {
            return ExitCode::FAILURE;
        }
    }

    let out_dir = target_dir().join("kernel");
    if let Err(e) = fs::create_dir_all(out_dir.join("pc")) {
        eprintln!("can't create {}: {e}", out_dir.display());
        return ExitCode::FAILURE;
    }
    if options.board == Board::Pc {
        for job in &options.jobs {
            let elf = target_dir().join(job.arch.triple).join(profile).join(job.kernel.package);
            if let Err(e) = make_esp(job, &elf, &out_dir) {
                eprintln!("can't make the boot files of {}: {e}", job.label());
                return ExitCode::FAILURE;
            }
        }
    }
    let mut queue: VecDeque<Job> = options.jobs.drain(..).collect();
    let mut runs = Vec::new();
    if !options.headless {
        eprintln!("=== running; close the QEMU windows to finish");
    }
    if let Err(e) = watch(&mut queue, &mut runs, &options, &out_dir, profile) {
        eprintln!("failed to start QEMU: {e}");
        for run in runs.iter_mut().filter(|r| !r.exited) {
            let _ = run.qemu.kill();
            let _ = run.qemu.wait();
        }
        return ExitCode::FAILURE;
    }

    eprintln!("\n=== summary");
    for run in &runs {
        let status = if run.outcome.as_ref().is_some_and(|o| o.is_ok()) { "ok" } else { "FAIL" };
        let detail = match &run.outcome {
            Some(Ok(d) | Err(d)) => d.as_str(),
            None => "no result",
        };
        eprintln!("{status:4}  {:15} {detail}", run.job.label());
    }
    eprintln!("serial logs: {}", out_dir.display());
    if runs.iter().all(|r| r.outcome.as_ref().is_some_and(|o| o.is_ok())) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The jobs to build for: one for each package on each architecture, since several kernels
/// run the same binary.
fn builds(jobs: &[Job]) -> Vec<&Job> {
    let mut builds: Vec<&Job> = Vec::new();
    for job in jobs {
        if !builds.iter().any(|b| b.kernel.package == job.kernel.package && ptr::eq(b.arch, job.arch)) {
            builds.push(job);
        }
    }
    builds
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options { jobs: Vec::new(), release: false, headless: false, screenshot: false, board: Board::Qemu, limit: None };
    let mut kernels: Vec<&'static Kernel> = Vec::new();
    let mut arches: Vec<&'static Arch> = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--release" => options.release = true,
            "--headless" => options.headless = true,
            "--screenshot" => options.screenshot = true,
            "--board" => {
                options.board = match args.next().map(String::as_str) {
                    Some("qemu") => Board::Qemu,
                    Some("pc") => Board::Pc,
                    Some(other) => return Err(format!("unknown board {other}; known: qemu, pc")),
                    None => return Err("--board needs a value".into()),
                };
            }
            "--jobs" => {
                let value = args.next().ok_or("--jobs needs a value")?;
                let limit = value.parse().ok().filter(|n| *n > 0);
                options.limit = Some(limit.ok_or_else(|| format!("--jobs needs a number above 0, not {value}"))?);
            }
            "--target" => {
                let value = args.next().ok_or("--target needs a value")?;
                let arch = ARCHES
                    .iter()
                    .find(|a| a.name == value || a.triple == value)
                    .ok_or_else(|| format!("unknown target {value}; known: riscv64, aarch64, x86_64"))?;
                if !arches.iter().any(|a| ptr::eq(*a, arch)) {
                    arches.push(arch);
                }
            }
            "--kernel" => {
                let value = args.next().ok_or("--kernel needs a value")?;
                let kernel = KERNELS
                    .iter()
                    .find(|k| k.name == value)
                    .ok_or_else(|| {
                        let known: Vec<&str> = KERNELS.iter().map(|k| k.name).collect();
                        format!("unknown kernel {value}; known: {}", known.join(", "))
                    })?;
                // Twice would be two runs writing to one log.
                if !kernels.iter().any(|k| ptr::eq(*k, kernel)) {
                    kernels.push(kernel);
                }
            }
            "-h" | "--help" => return Err(String::new()),
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    let explicit_arches = !arches.is_empty();
    if kernels.is_empty() {
        kernels = KERNELS.iter().collect();
    }
    if options.board == Board::Pc {
        if arches.iter().any(|a| a.name != "x86_64") {
            return Err("--board pc is for x86_64 only".into());
        }
        arches = ARCHES.iter().filter(|a| a.name == "x86_64").collect();
    }
    if arches.is_empty() {
        arches = ARCHES.iter().collect();
    }
    for kernel in kernels {
        if options.board != Board::Pc && PC_KERNELS.contains(&kernel.name) {
            continue;
        }
        for &arch in &arches {
            if kernel.arches.contains(&arch.name) {
                options.jobs.push(Job { kernel, arch });
            } else if explicit_arches {
                eprintln!("note: skipping {} on {}, which it doesn't support", kernel.name, arch.name);
            }
        }
    }
    if options.jobs.is_empty() {
        return Err("nothing to run: no chosen kernel supports the chosen targets".into());
    }
    Ok(options)
}

fn preflight(arches: &[&Arch], board: Board) -> Result<(), Vec<String>> {
    let installed = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let mut problems = Vec::new();
    for arch in arches {
        if !installed.lines().any(|l| l.trim() == arch.triple) {
            problems.push(format!("rust target {0}: rustup target add {0}", arch.triple));
        }
        if !on_path(arch.qemu) {
            let package = match arch.name {
                "riscv64" => "qemu-system-misc",
                "aarch64" => "qemu-system-arm",
                _ => "qemu-system-x86",
            };
            problems.push(format!("{} is not on PATH: sudo apt install {package}", arch.qemu));
        }
    }
    if board == Board::Pc {
        if !on_path("grub-mkstandalone") {
            problems.push("grub-mkstandalone is not on PATH: sudo apt install grub-efi-amd64-bin".into());
        }
        if !Path::new(OVMF_CODE).exists() {
            problems.push(format!("no UEFI firmware at {OVMF_CODE}: sudo apt install ovmf"));
        }
    }
    if problems.is_empty() { Ok(()) } else { Err(problems) }
}

fn build(job: &Job, release: bool, board: Board) -> bool {
    let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(workspace_root())
        .args(["build", "-p", job.kernel.package, "--target", job.arch.triple]);
    if release {
        cmd.arg("--release");
    }
    if board == Board::Pc {
        cmd.args(["--features", "board-pc"]);
    }
    cmd.status().is_ok_and(|s| s.success())
}

/// What GRUB runs: load the kernel, tell it which test to run, and boot.
fn grub_config(test: Option<&str>) -> String {
    let args = test.map_or(String::new(), |t| format!(" test={t}"));
    format!("set timeout=0\ninsmod efi_gop\nmultiboot2 /boot/kernel{args}\nboot\n")
}

/// The directory a job boots from on the pc board: what goes on a UEFI system partition.
fn esp_dir(job: &Job, out_dir: &Path) -> PathBuf {
    out_dir.join("pc").join(job.label())
}

/// Build the UEFI application that boots `elf`: GRUB, with its configuration and the kernel
/// inside it, at the path firmware looks for on removable media.
fn make_esp(job: &Job, elf: &Path, out_dir: &Path) -> std::io::Result<()> {
    let esp = esp_dir(job, out_dir);
    let boot = esp.join("EFI").join("BOOT");
    fs::create_dir_all(&boot)?;
    let config = out_dir.join("pc").join(format!("{}.cfg", job.label()));
    fs::write(&config, grub_config(job.kernel.test))?;
    let status = Command::new("grub-mkstandalone")
        .args(["--format=x86_64-efi", "--modules=multiboot2 normal efi_gop fat part_gpt part_msdos"])
        .arg(format!("--output={}", boot.join("BOOTX64.EFI").display()))
        .arg(format!("boot/grub/grub.cfg={}", config.display()))
        .arg(format!("boot/kernel={}", elf.display()))
        .status()?;
    if status.success() { Ok(()) } else { Err(std::io::Error::other("grub-mkstandalone failed")) }
}

/// Where cargo puts what it builds, which `build.target-dir` in a config file can move.
/// Asked once: it starts a process, and every job wants it.
fn target_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let mut cargo = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        cargo.current_dir(workspace_root());
        target_dir_from(cargo)
    })
    .clone()
}

/// What `cargo metadata` says the target directory is. If cargo can't say, the variable or
/// the default.
fn target_dir_from(mut cargo: Command) -> PathBuf {
    let output = cargo.args(["metadata", "--no-deps", "--format-version", "1"]).stderr(Stdio::null()).output();
    output
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|json| json_path(&json, "target_directory"))
        .or_else(|| env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
        .unwrap_or_else(|| workspace_root().join("target"))
}

/// The string at `key` in cargo's JSON, for a path: `None` if it has an escape other than
/// a quote or a backslash.
fn json_path(json: &str, key: &str) -> Option<PathBuf> {
    let pattern = format!("\"{key}\":\"");
    let start = json.rfind(&pattern)? + pattern.len();
    let mut path = String::new();
    let mut chars = json[start..].chars();
    loop {
        match chars.next()? {
            '"' => return Some(PathBuf::from(path)),
            '\\' => match chars.next()? {
                c @ ('"' | '\\') => path.push(c),
                _ => return None,
            },
            c => path.push(c),
        }
    }
}

/// The disk test's image: 32 KiB.
const DISK_SECTORS: u64 = 64;

/// What the disk test expects to read: every byte a function of where it is, so a sector
/// read from the wrong place, or bytes shuffled within one, don't match. Must match
/// green-tea-coop's `pattern`.
fn disk_pattern(sector: u64, offset: usize) -> u8 {
    (sector as u8).wrapping_mul(31) ^ (offset as u8) ^ (offset >> 8) as u8
}

fn disk_image(sectors: u64) -> Vec<u8> {
    (0..sectors).flat_map(|s| (0..512).map(move |o| disk_pattern(s, o))).collect()
}

pub const SHELL_USAGE: &str = "\
usage: cargo xtask shell [--target riscv64|aarch64|x86_64] [--release]

Builds green-tea-shell and boots it in QEMU with its serial console on this terminal, so
you can type at it. `halt` stops the kernel; ctrl-a x leaves QEMU.";

/// `cargo xtask shell`: one QEMU, in the foreground, with the kernel's serial port on our
/// stdin and stdout.
pub fn shell(args: &[String]) -> ExitCode {
    let (arch, release) = match parse_shell(args) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            println!("{SHELL_USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{SHELL_USAGE}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(problems) = preflight(&[arch], Board::Qemu) {
        eprintln!("missing prerequisites:");
        for p in problems {
            eprintln!("  - {p}");
        }
        return ExitCode::FAILURE;
    }
    let kernel = KERNELS.iter().find(|k| k.name == "shell").expect("the shell kernel");
    let job = Job { kernel, arch };
    if !build(&job, release, Board::Qemu) {
        return ExitCode::FAILURE;
    }
    let profile = if release { "release" } else { "debug" };
    let elf = target_dir().join(arch.triple).join(profile).join(kernel.package);
    eprintln!("=== {} on {}: `halt` stops the kernel, ctrl-a x leaves QEMU", kernel.package, arch.name);
    // mon:stdio puts the terminal in raw mode, so the shell sees each key as it's typed,
    // and keeps ctrl-a as QEMU's escape.
    let status = Command::new(arch.qemu)
        .args(arch.machine)
        .args(["-m", "128M", "-device", "ramfb", "-display", "none", "-serial", "mon:stdio"])
        .arg("-kernel")
        .arg(elf)
        .status();
    match status {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("failed to start {}: {e}", arch.qemu);
            ExitCode::FAILURE
        }
    }
}

/// `shell`'s arguments: the architecture, and whether to optimize. `None` for `--help`.
fn parse_shell(args: &[String]) -> Result<Option<(&'static Arch, bool)>, String> {
    let mut arch = &ARCHES[0];
    let mut release = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.clone().next()) {
            ("--release", _) => release = true,
            ("--target", Some(value)) => {
                arch = ARCHES
                    .iter()
                    .find(|a| a.name == value || a.triple == value)
                    .ok_or_else(|| format!("unknown target {value}"))?;
                args.next();
            }
            ("--target", None) => return Err("--target needs a value".into()),
            ("-h" | "--help", _) => return Ok(None),
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    Ok(Some((arch, release)))
}

/// One QEMU and what we've learned from it so far.
struct Run {
    job: Job,
    board: Board,
    qemu: Child,
    /// A run's time limit counts from its own start, not from the suite's.
    started: Instant,
    log: PathBuf,
    /// How much of the log has been echoed.
    read: usize,
    /// Echoing starts at the kernel's first line, skipping the firmware's banner.
    seen_kernel: bool,
    /// A socket to the serial port and what to type at it, once the kernel is up.
    late_input: Option<(PathBuf, &'static str)>,
    keyboard_test_stage: u8,
    /// Set once: Ok or Err with a one-line description.
    outcome: Option<Result<String, String>>,
    exited: bool,
}

impl Run {
    fn start(job: Job, elf: &Path, out_dir: &Path, headless: bool, board: Board) -> std::io::Result<Run> {
        let log = out_dir.join(format!("{}.log", job.label()));
        let _ = fs::remove_file(&log);
        let mut cmd = Command::new(job.arch.qemu);
        match board {
            Board::Qemu => {
                cmd.args(job.arch.machine).args(["-m", "128M", "-device", "ramfb"]).arg("-kernel").arg(elf);
            }
            // UEFI firmware, which finds GRUB on a FAT drive made from the directory.
            Board::Pc => {
                let vars = out_dir.join("pc").join(format!("{}.vars", job.label()));
                fs::copy(OVMF_VARS, &vars)?;
                cmd.args(["-machine", "q35", "-cpu", "max", "-m", "512M", "-vga", "std", "-net", "none"])
                    .arg("-device")
                    .arg("qemu-xhci,id=xhci");
                if job.kernel.name != "protected" {
                    cmd.args(["-device", "usb-kbd,bus=xhci.0"]);
                }
                cmd.arg("-drive")
                    .arg(format!("if=pflash,format=raw,readonly=on,file={OVMF_CODE}"))
                    .arg("-drive")
                    .arg(format!("if=pflash,format=raw,file={}", vars.display()))
                    .arg("-drive")
                    .arg(format!("if=none,id=esp,format=raw,readonly=on,file=fat:{}", esp_dir(&job, out_dir).display()))
                    .args(["-device", "virtio-blk-pci,drive=esp"]);
            }
        }
        cmd.args(["-name", &format!("{} ({})", job.kernel.package, job.arch.name)])
            .args(["-monitor", "stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null()); // the monitor's echo; errors still go to stderr
        let mut late_input = None;
        match job.kernel.input {
            None => cmd.arg("-serial").arg(format!("file:{}", log.display())),
            // The serial port's output goes to the log, and its input comes from a file.
            // QEMU hands the UART a byte only when it has room, so nothing is lost if
            // the kernel is slow to start reading.
            // On the pc board the firmware and GRUB read the serial port too, and would eat
            // the input. It goes through a socket instead, once the kernel has said hello.
            Some(input) if board == Board::Pc => {
                let socket = env::temp_dir().join(format!("green-tea-{}-{}.sock", std::process::id(), job.label()));
                let _ = fs::remove_file(&socket);
                let chardev = format!("socket,id=console,path={},server=on,wait=off,logfile={}", socket.display(), log.display());
                late_input = Some((socket, input));
                cmd.arg("-chardev").arg(chardev).args(["-serial", "chardev:console"])
            }
            Some(input) => {
                let script = out_dir.join(format!("{}.in", job.label()));
                fs::write(&script, input)?;
                cmd.arg("-chardev")
                    .arg(format!("file,id=console,path={},input-path={}", log.display(), script.display()))
                    .args(["-serial", "chardev:console"])
            }
        };
        if headless {
            cmd.args(["-display", "none"]);
        }
        if job.cpus() > 1 {
            cmd.args(["-smp", &job.cpus().to_string()]);
        }
        if let Some(test) = job.kernel.test.filter(|_| board == Board::Qemu) {
            cmd.arg("-fw_cfg").arg(format!("name=opt/green-tea/test,string={test}"));
        }
        if job.kernel.disk {
            let image = out_dir.join(format!("{}.img", job.label()));
            fs::write(&image, disk_image(DISK_SECTORS))?; // fresh each run: the test writes to it
            cmd.arg("-drive")
                .arg(format!("if=none,id=disk,format=raw,file={}", image.display()))
                .args(if board == Board::Pc { NVME_DISK } else { job.arch.virtio_blk });
        }
        Ok(Run {
            job,
            board,
            qemu: cmd.spawn()?,
            started: Instant::now(),
            log,
            read: 0,
            seen_kernel: false,
            late_input,
            keyboard_test_stage: 0,
            outcome: None,
            exited: false,
        })
    }

    fn monitor(&mut self, command: &str) {
        if let Some(stdin) = self.qemu.stdin.as_mut() {
            let _ = writeln!(stdin, "{command}");
            let _ = stdin.flush();
        }
    }

    /// Type the input, from a thread since QEMU takes it only as the UART has room.
    fn type_late_input(&mut self) {
        if !self.seen_kernel {
            return;
        }
        let Some((socket, input)) = self.late_input.take() else { return };
        let log = self.log.clone();
        thread::spawn(move || {
            if let Ok(mut stream) = UnixStream::connect(&socket) {
                // What follows a \u{1} waits until the kernel has answered and is at its prompt
                // again, so that it arrives while the kernel is waiting for input.
                for (i, part) in input.split('\u{1}').enumerate() {
                    if i > 0 {
                        wait_for_prompt(&log);
                    }
                    let _ = stream.write_all(part.as_bytes());
                }
                // Held open until QEMU exits: closing it could lose what is still queued.
                let _ = std::io::copy(&mut stream, &mut std::io::sink());
            }
            let _ = fs::remove_file(&socket);
        });
    }

    /// Echo new complete lines from the serial log, and look for the verdict.
    fn follow(&mut self) {
        let Ok(bytes) = fs::read(&self.log) else { return };
        let Some(end) = bytes[self.read.min(bytes.len())..].iter().rposition(|&b| b == b'\n') else {
            return;
        };
        let end = self.read + end + 1;
        let text = String::from_utf8_lossy(&bytes[self.read..end]).into_owned();
        self.read = end;
        for line in text.lines().map(|l| l.trim_end_matches('\r')) {
            if line.starts_with("green-tea-") {
                self.seen_kernel = true;
            }
            if self.seen_kernel {
                eprintln!("[{:15}] {line}", self.job.label());
            }
            if self.board == Board::Pc && self.job.kernel.name == "protected" {
                match (self.keyboard_test_stage, line) {
                    (0, "user keyboard: waiting for A scan code") => {
                        eprintln!("[{:15}] sending PS/2 key A", self.job.label());
                        thread::sleep(Duration::from_millis(100));
                        self.monitor("sendkey a");
                        self.keyboard_test_stage = 1;
                    }
                    (1, "kernel keyboard: waiting for B") => {
                        eprintln!("[{:15}] sending PS/2 key B", self.job.label());
                        thread::sleep(Duration::from_millis(100));
                        self.monitor("sendkey b");
                        self.keyboard_test_stage = 2;
                    }
                    _ => {}
                }
            }
            judge(&mut self.outcome, line);
        }
    }
}

/// Wait until the log ends at a prompt with at least one answered command before it, and
/// a moment more for the kernel to go back to waiting. Gives up after 30 seconds.
fn wait_for_prompt(log: &Path) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        let text = fs::read(log).unwrap_or_default();
        if text.ends_with(b"> ") && text.windows(5).any(|w| w == b"halts") {
            thread::sleep(Duration::from_millis(200));
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// The kernel's last word: it judges its own run, and says PASS or FAIL. Or it panics.
fn verdict(line: &str) -> Option<Result<String, String>> {
    if let Some(rest) = line.strip_prefix("panic: ") {
        return Some(Err(format!("kernel panicked: {rest}")));
    }
    if let Some(rest) = line.strip_prefix("PASS: ") {
        return Some(Ok(rest.to_string()));
    }
    line.strip_prefix("FAIL: ").map(|rest| Err(rest.to_string()))
}

/// Take `line` into the run's outcome. The first failure stands, and a failure after a pass
/// replaces it: a kernel that says PASS and then panics has not passed.
fn judge(outcome: &mut Option<Result<String, String>>, line: &str) {
    if !matches!(outcome, Some(Err(_))) {
        match verdict(line) {
            Some(Ok(_)) if outcome.is_some() => {}
            Some(found) => *outcome = Some(found),
            None => {}
        }
    }
}

/// Whether a machine of `cpus` may start while `busy` virtual CPUs are running. One too
/// big for the limit runs when nothing else does, so that it runs at all.
fn fits(cpus: usize, busy: usize, limit: usize) -> bool {
    busy == 0 || busy.saturating_add(cpus) <= limit
}

/// Start the queued jobs in order as there is room, and follow them until all have exited.
fn watch(queue: &mut VecDeque<Job>, runs: &mut Vec<Run>, options: &Options, out_dir: &Path, profile: &str) -> std::io::Result<()> {
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
            runs.push(Run::start(job, &elf, out_dir, options.headless, options.board)?);
        }
        for run in runs.iter_mut().filter(|r| !r.exited) {
            let had_outcome = run.outcome.is_some();
            run.follow();
            run.type_late_input();
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Board, grub_config, disk_image, fits, judge, parse, verdict};

    #[test]
    fn the_disk_image_differs_from_sector_to_sector() {
        let image = disk_image(4);
        assert_eq!(image.len(), 4 * 512);
        assert_ne!(image[..512], image[512..1024]);
        assert_ne!(image[..256], image[256..512]);
    }

    #[test]
    fn pass_passes() {
        assert_eq!(verdict("PASS: 57 switches, as expected"), Some(Ok("57 switches, as expected".into())));
    }

    #[test]
    fn fail_fails() {
        assert_eq!(verdict("FAIL: 56 switches, expected 57"), Some(Err("56 switches, expected 57".into())));
    }

    #[test]
    fn a_panic_fails() {
        assert!(verdict("panic: no etc/ramfb (run qemu with -device ramfb)").unwrap().is_err());
    }

    #[test]
    fn other_lines_are_not_a_verdict() {
        assert_eq!(verdict("running 5 workers; expecting 57 switches"), None);
    }

    #[test]
    fn a_failure_after_a_pass_fails_the_run() {
        for (line, why) in [("panic: late", "kernel panicked: late"), ("FAIL: late", "late")] {
            let mut outcome = None;
            judge(&mut outcome, "PASS: all done");
            judge(&mut outcome, line);
            assert_eq!(outcome, Some(Err(why.to_string())));
        }
        // The first failure is the one reported, and a pass doesn't undo it.
        let mut outcome = None;
        judge(&mut outcome, "FAIL: first");
        judge(&mut outcome, "FAIL: second");
        judge(&mut outcome, "PASS: all done");
        assert_eq!(outcome, Some(Err("first".to_string())));
    }

    #[test]
    fn a_machine_starts_only_if_it_fits() {
        assert!(fits(1, 9, 10));
        assert!(!fits(1, 10, 10));
        assert!(!fits(4, 8, 10));
        assert!(fits(4, 0, 2), "too big for the limit, but alone");
        assert!(!fits(4, 1, 2));
        assert!(fits(4, 1000, usize::MAX));
    }

    #[test]
    fn jobs_must_be_a_positive_number() {
        let parse_jobs = |v: &str| parse(&["--jobs".to_string(), v.to_string()]).map(|o| o.limit);
        assert_eq!(parse_jobs("3"), Ok(Some(3)));
        assert!(parse_jobs("0").is_err());
        assert!(parse_jobs("many").is_err());
    }

    fn labels(args: &[&str]) -> Vec<String> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        parse(&args).unwrap().jobs.iter().map(|j| j.label()).collect()
    }

    /// What `KERNELS` says should run: every kernel on every architecture it supports,
    /// or on `only`.
    fn expected(only: Option<&str>) -> Vec<String> {
        let mut labels = Vec::new();
        for kernel in super::KERNELS {
            if super::PC_KERNELS.contains(&kernel.name) {
                continue;
            }
            for arch in super::ALL.iter().filter(|a| kernel.arches.contains(a) && only.is_none_or(|only| only == **a)) {
                labels.push(format!("{}-{arch}", kernel.name));
            }
        }
        labels
    }

    #[test]
    fn the_interrupts_kernel_runs_only_on_the_pc_board() {
        assert!(!labels(&[]).contains(&"interrupts-x86_64".to_string()));
        let pc = parse(&["--board".to_string(), "pc".to_string()]).unwrap();
        assert!(pc.jobs.iter().any(|j| j.kernel.name == "interrupts"));
    }

    #[test]
    fn by_default_every_kernel_runs_everywhere_it_can() {
        assert_eq!(labels(&[]), expected(None));
        assert!(labels(&[]).contains(&"overflow-riscv64".to_string()));
        assert!(!labels(&[]).contains(&"overflow-aarch64".to_string()));
    }

    /// A project whose `.cargo/config.toml` moves its target directory, asked with none of
    /// the variables that would override that.
    #[test]
    fn the_target_directory_comes_from_cargos_config() {
        let dir = std::env::temp_dir().join(format!("xtask-target-dir-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"probe\"\nversion = \"0.1.0\"\n[workspace]\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.join(".cargo/config.toml"), "[build]\ntarget-dir = \"elsewhere\"\n").unwrap();
        let mut cargo = std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        cargo.current_dir(&dir).env_remove("CARGO_TARGET_DIR").env_remove("CARGO_BUILD_TARGET_DIR");
        let found = super::target_dir_from(cargo);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(found.ends_with("elsewhere"), "{}", found.display());
    }

    #[test]
    fn a_path_is_read_from_cargos_json() {
        let json = r#"{"packages":[],"target_directory":"/a \\ b/\"c\"","version":1}"#;
        assert_eq!(super::json_path(json, "target_directory"), Some(r#"/a \ b/"c""#.into()));
        assert_eq!(super::json_path(r#"{"target_directory":"/a\u00e9"}"#, "target_directory"), None);
    }

    #[test]
    fn the_board_defaults_to_qemu() {
        assert_eq!(parse(&[]).map(|o| o.board).unwrap(), Board::Qemu);
    }

    #[test]
    fn the_pc_board_runs_only_x86_64_and_its_disk_is_nvme() {
        let options = parse(&["--board".to_string(), "pc".to_string()]).unwrap();
        assert_eq!(options.board, Board::Pc);
        assert!(options.jobs.iter().all(|j| j.arch.name == "x86_64"));
        assert!(options.jobs.iter().any(|j| j.kernel.disk));
        assert!(options.jobs.iter().any(|j| j.kernel.name == "shell"));
    }

    #[test]
    fn the_pc_board_refuses_another_architecture() {
        let args = ["--board", "pc", "--target", "riscv64"].map(String::from);
        assert!(parse(&args).is_err());
    }

    #[test]
    fn an_unknown_board_is_refused() {
        assert!(parse(&["--board".to_string(), "sparc".to_string()]).is_err());
    }

    #[test]
    fn grub_passes_the_test_on_the_command_line() {
        assert!(grub_config(Some("slab")).contains("multiboot2 /boot/kernel test=slab\n"));
        assert!(grub_config(Some("deadline")).contains("multiboot2 /boot/kernel test=deadline\n"));
        assert!(grub_config(None).contains("multiboot2 /boot/kernel\n"));
    }

    #[test]
    fn latency_images_use_the_same_x86_binary_with_distinct_timer_modes() {
        let countdown = parse(&["--kernel".into(), "latency".into()]).unwrap();
        let deadline = parse(&["--kernel".into(), "latency-deadline".into()]).unwrap();
        assert_eq!(countdown.jobs.len(), 1);
        assert_eq!(countdown.jobs[0].kernel.package, "green-tea-latency");
        assert_eq!(countdown.jobs[0].kernel.test, Some("countdown"));
        assert_eq!(deadline.jobs[0].kernel.package, "green-tea-latency");
        assert_eq!(deadline.jobs[0].kernel.test, Some("deadline"));
    }

    #[test]
    fn shell_says_when_target_has_no_value() {
        let parsed = super::parse_shell(&["--target".to_string()]).map(|_| ());
        assert_eq!(parsed, Err("--target needs a value".to_string()));
    }

    #[test]
    fn a_binary_is_built_once_for_each_architecture() {
        let args: Vec<String> = ["--kernel", "coop", "--kernel", "disk", "--kernel", "shell", "--target", "riscv64"]
            .iter()
            .map(|a| a.to_string())
            .collect();
        let options = parse(&args).unwrap();
        let built: Vec<&str> = super::builds(&options.jobs).iter().map(|j| j.kernel.package).collect();
        assert_eq!(built, ["green-tea-coop", "green-tea-shell"]);
    }

    #[test]
    fn a_kernel_or_target_named_twice_runs_once() {
        let once = labels(&["--kernel", "coop", "--target", "riscv64"]);
        assert_eq!(labels(&["--kernel", "coop", "--kernel", "coop", "--target", "riscv64"]), once);
        assert_eq!(labels(&["--kernel", "coop", "--target", "riscv64", "--target", "riscv64"]), once);
    }

    #[test]
    fn unsupported_combinations_are_skipped() {
        assert_eq!(labels(&["--target", "aarch64"]), expected(Some("aarch64")));
        assert!(labels(&["--target", "aarch64"]).contains(&"shell-aarch64".to_string()));
        let args = ["--kernel".to_string(), "overflow".into(), "--target".into(), "aarch64".into()];
        assert!(parse(&args).is_err());
    }
}
