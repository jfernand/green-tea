//! `cargo xtask kernel`: build the bare-metal kernels for each architecture they support
//! and run them all in QEMU at once.
//!
//! Every QEMU gets its serial port written to a log file and its monitor on a pipe. The
//! log is followed to echo the kernel's output (prefixed with kernel and architecture)
//! and to spot its verdict: a line starting with "PASS: " or "FAIL: ", or a panic. The
//! monitor is used to take a screenshot and, when headless, to quit.

use crate::{on_path, workspace_root};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::ptr;
use std::time::{Duration, Instant};
use std::{env, thread};

pub const USAGE: &str = "\
usage: cargo xtask kernel [--kernel coop|preempt|overflow|disk|memmap|shell]... [--target riscv64|aarch64|x86_64]...
                          [--release] [--headless] [--screenshot]

Builds the bare-metal kernels and boots each in QEMU on every architecture it supports,
one window apiece, all at once. Close the windows to finish. The kernels' serial output
is echoed here, and the run passes when every kernel reports PASS.

  coop      green-tea-coop: tasks yield to each other (riscv64, aarch64, x86_64)
  preempt   green-tea-preempt: a timer interrupt preempts tasks that never yield, and one
            that sleeps between steps (riscv64, aarch64, x86_64)
  overflow  green-tea-coop's test that a task overflowing its stack hits its guard page
            (riscv64, x86_64: the boards that page)
  disk      green-tea-coop's test of the virtio-blk driver: reads back a disk image made
            for it, and writes a sector (virtio-mmio on riscv64 and aarch64, virtio-pci
            on x86_64)
  memmap    green-tea-coop's test of its own memory map: prints where the linker put each
            section, and checks their order, alignment and size (riscv64, aarch64, x86_64)
  shell     green-tea-shell: types two commands at the shell on the serial console; the
            second is its `test`, which checks that an arena frees what the heap can't
            (riscv64, aarch64, x86_64). To type at it yourself: cargo xtask shell

  --kernel      only these kernels (default: all)
  --target      only these architectures (default: all three)
  --release     build with optimizations
  --headless    no windows: quit each QEMU as soon as its kernel finishes
  --screenshot  save the final screen to target/kernel/<kernel>-<arch>.png";

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

const KERNELS: &[Kernel] = &[
    Kernel { name: "coop", package: "green-tea-coop", arches: ALL, test: None, disk: false, input: None },
    Kernel { name: "preempt", package: "green-tea-preempt", arches: ALL, test: None, disk: false, input: None },
    // Needs a board whose guard pages fault: not aarch64 (MMU off), so far.
    Kernel { name: "overflow", package: "green-tea-coop", arches: &["riscv64", "x86_64"], test: Some("overflow"), disk: false, input: None },
    Kernel { name: "disk", package: "green-tea-coop", arches: ALL, test: Some("disk"), disk: true, input: None },
    Kernel { name: "memmap", package: "green-tea-coop", arches: ALL, test: Some("memmap"), disk: false, input: None },
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
];

const ALL: &[&str] = &["riscv64", "aarch64", "x86_64"];

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
}

struct Options {
    jobs: Vec<Job>,
    release: bool,
    headless: bool,
    screenshot: bool,
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
    if let Err(problems) = preflight(&arches) {
        eprintln!("missing prerequisites:");
        for p in problems {
            eprintln!("  - {p}");
        }
        return ExitCode::FAILURE;
    }

    let profile = if options.release { "release" } else { "debug" };
    for job in &options.jobs {
        eprintln!("=== build {} {} {profile}", job.kernel.package, job.arch.name);
        if options.jobs.iter().take_while(|j| !ptr::eq(*j, job)).any(|j| j.kernel.package == job.kernel.package && ptr::eq(j.arch, job.arch)) {
            continue; // already built: another kernel runs the same binary
        }
        if !build(job, options.release) {
            return ExitCode::FAILURE;
        }
    }

    let out_dir = target_dir().join("kernel");
    if let Err(e) = fs::create_dir_all(&out_dir) {
        eprintln!("can't create {}: {e}", out_dir.display());
        return ExitCode::FAILURE;
    }
    let mut runs = Vec::new();
    for job in options.jobs.drain(..) {
        let elf = target_dir().join(job.arch.triple).join(profile).join(job.kernel.package);
        let qemu = job.arch.qemu;
        match Run::start(job, &elf, &out_dir, options.headless) {
            Ok(run) => runs.push(run),
            Err(e) => {
                eprintln!("failed to start {qemu}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    if !options.headless {
        eprintln!("=== running; close the QEMU windows to finish");
    }
    watch(&mut runs, &options, &out_dir);

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

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options { jobs: Vec::new(), release: false, headless: false, screenshot: false };
    let mut kernels: Vec<&'static Kernel> = Vec::new();
    let mut arches: Vec<&'static Arch> = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--release" => options.release = true,
            "--headless" => options.headless = true,
            "--screenshot" => options.screenshot = true,
            "--target" => {
                let value = args.next().ok_or("--target needs a value")?;
                let arch = ARCHES
                    .iter()
                    .find(|a| a.name == value || a.triple == value)
                    .ok_or_else(|| format!("unknown target {value}; known: riscv64, aarch64, x86_64"))?;
                arches.push(arch);
            }
            "--kernel" => {
                let value = args.next().ok_or("--kernel needs a value")?;
                let kernel = KERNELS
                    .iter()
                    .find(|k| k.name == value)
                    .ok_or_else(|| format!("unknown kernel {value}; known: coop, preempt, overflow, disk, memmap, shell"))?;
                kernels.push(kernel);
            }
            "-h" | "--help" => return Err(String::new()),
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    let explicit_arches = !arches.is_empty();
    if kernels.is_empty() {
        kernels = KERNELS.iter().collect();
    }
    if arches.is_empty() {
        arches = ARCHES.iter().collect();
    }
    for kernel in kernels {
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

fn preflight(arches: &[&Arch]) -> Result<(), Vec<String>> {
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
    if problems.is_empty() { Ok(()) } else { Err(problems) }
}

fn build(job: &Job, release: bool) -> bool {
    let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(workspace_root())
        .args(["build", "-p", job.kernel.package, "--target", job.arch.triple]);
    if release {
        cmd.arg("--release");
    }
    cmd.status().is_ok_and(|s| s.success())
}

fn target_dir() -> PathBuf {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"))
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
    let mut arch = &ARCHES[0];
    let mut release = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.clone().next()) {
            ("--release", _) => release = true,
            ("--target", Some(value)) => match ARCHES.iter().find(|a| a.name == value || a.triple == value) {
                Some(found) => {
                    arch = found;
                    args.next();
                }
                None => {
                    eprintln!("error: unknown target {value}\n\n{SHELL_USAGE}");
                    return ExitCode::FAILURE;
                }
            },
            ("-h" | "--help", _) => {
                println!("{SHELL_USAGE}");
                return ExitCode::SUCCESS;
            }
            _ => {
                eprintln!("error: unknown option {arg}\n\n{SHELL_USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }
    if let Err(problems) = preflight(&[arch]) {
        eprintln!("missing prerequisites:");
        for p in problems {
            eprintln!("  - {p}");
        }
        return ExitCode::FAILURE;
    }
    let kernel = KERNELS.iter().find(|k| k.name == "shell").expect("the shell kernel");
    let job = Job { kernel, arch };
    if !build(&job, release) {
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

/// One QEMU and what we've learned from it so far.
struct Run {
    job: Job,
    qemu: Child,
    log: PathBuf,
    /// How much of the log has been echoed.
    read: usize,
    /// Echoing starts at the kernel's first line, skipping the firmware's banner.
    seen_kernel: bool,
    /// Set once: Ok or Err with a one-line description.
    outcome: Option<Result<String, String>>,
    exited: bool,
}

impl Run {
    fn start(job: Job, elf: &Path, out_dir: &Path, headless: bool) -> std::io::Result<Run> {
        let log = out_dir.join(format!("{}.log", job.label()));
        let _ = fs::remove_file(&log);
        let mut cmd = Command::new(job.arch.qemu);
        cmd.args(job.arch.machine)
            .args(["-m", "128M", "-device", "ramfb"])
            .args(["-name", &format!("{} ({})", job.kernel.package, job.arch.name)])
            .args(["-monitor", "stdio"])
            .arg("-kernel")
            .arg(elf)
            .stdin(Stdio::piped())
            .stdout(Stdio::null()); // the monitor's echo; errors still go to stderr
        match job.kernel.input {
            None => cmd.arg("-serial").arg(format!("file:{}", log.display())),
            // The serial port's output goes to the log, and its input comes from a file.
            // QEMU hands the UART a byte only when it has room, so nothing is lost if
            // the kernel is slow to start reading.
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
        if let Some(test) = job.kernel.test {
            cmd.arg("-fw_cfg").arg(format!("name=opt/green-tea/test,string={test}"));
        }
        if job.kernel.disk {
            let image = out_dir.join(format!("{}.img", job.label()));
            fs::write(&image, disk_image(DISK_SECTORS))?; // fresh each run: the test writes to it
            cmd.arg("-drive")
                .arg(format!("if=none,id=disk,format=raw,file={}", image.display()))
                .args(job.arch.virtio_blk);
        }
        Ok(Run {
            job,
            qemu: cmd.spawn()?,
            log,
            read: 0,
            seen_kernel: false,
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
            if self.outcome.is_none() {
                self.outcome = verdict(line);
            }
        }
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

fn watch(runs: &mut [Run], options: &Options, out_dir: &Path) {
    let start = Instant::now();
    while runs.iter().any(|r| !r.exited) {
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
            if options.headless && run.outcome.is_none() && start.elapsed() > HEADLESS_TIMEOUT {
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
}

#[cfg(test)]
mod tests {
    use super::{disk_image, parse, verdict};

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

    fn labels(args: &[&str]) -> Vec<String> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        parse(&args).unwrap().jobs.iter().map(|j| j.label()).collect()
    }

    #[test]
    fn by_default_every_kernel_runs_everywhere_it_can() {
        assert_eq!(
            labels(&[]),
            [
                "coop-riscv64",
                "coop-aarch64",
                "coop-x86_64",
                "preempt-riscv64",
                "preempt-aarch64",
                "preempt-x86_64",
                "overflow-riscv64",
                "overflow-x86_64",
                "disk-riscv64",
                "disk-aarch64",
                "disk-x86_64",
                "memmap-riscv64",
                "memmap-aarch64",
                "memmap-x86_64",
                "shell-riscv64",
                "shell-aarch64",
                "shell-x86_64"
            ]
        );
    }

    #[test]
    fn unsupported_combinations_are_skipped() {
        assert_eq!(labels(&["--target", "aarch64"]), ["coop-aarch64", "preempt-aarch64", "disk-aarch64", "memmap-aarch64", "shell-aarch64"]);
        let args = ["--kernel".to_string(), "overflow".into(), "--target".into(), "aarch64".into()];
        assert!(parse(&args).is_err());
    }
}
