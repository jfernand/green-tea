//! `cargo xtask kernel`: build green-tea-kernel for each architecture and run it in QEMU.
//!
//! Every QEMU gets its serial port written to a log file and its monitor on a pipe. The
//! log is followed to echo the kernel's output (prefixed with the architecture) and to
//! spot the kernel's last line, "all tasks finished after N switches (expected M)". The
//! monitor is used to take a screenshot and, when headless, to quit.

use crate::{on_path, workspace_root};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};
use std::{env, thread};

pub const USAGE: &str = "\
usage: cargo xtask kernel [--target riscv64|aarch64|x86_64]... [--release] [--headless] [--screenshot]

Builds green-tea-kernel and boots it in QEMU, one window per architecture, all at once.
Close the windows to finish. The kernel's serial output is echoed here, and the run
passes when every kernel reports the switch count it expected.

  --target      only these architectures (default: all three)
  --release     build with optimizations
  --headless    no windows: quit each QEMU as soon as its kernel finishes (for CI)
  --screenshot  save the final screen to target/kernel/<arch>.png";

struct Arch {
    name: &'static str,
    triple: &'static str,
    qemu: &'static str,
    /// The machine, plus whatever that machine needs to boot our ELF and show only ramfb.
    machine: &'static [&'static str],
}

const ARCHES: &[Arch] = &[
    Arch {
        name: "riscv64",
        triple: "riscv64gc-unknown-none-elf",
        qemu: "qemu-system-riscv64",
        machine: &["-machine", "virt", "-bios", "default"], // OpenSBI loads the kernel
    },
    Arch {
        name: "aarch64",
        triple: "aarch64-unknown-none",
        qemu: "qemu-system-aarch64",
        machine: &["-machine", "virt", "-cpu", "cortex-a72"], // no firmware: QEMU jumps in at EL1
    },
    Arch {
        name: "x86_64",
        triple: "x86_64-unknown-none",
        qemu: "qemu-system-x86_64",
        machine: &["-machine", "q35", "-vga", "none"], // PVH boot; no VGA so ramfb is the screen
    },
];

const HEADLESS_TIMEOUT: Duration = Duration::from_secs(120);

struct Options {
    arches: Vec<&'static Arch>,
    release: bool,
    headless: bool,
    screenshot: bool,
}

pub fn main(args: &[String]) -> ExitCode {
    let options = match parse(args) {
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
    if let Err(problems) = preflight(&options.arches) {
        eprintln!("missing prerequisites:");
        for p in problems {
            eprintln!("  - {p}");
        }
        return ExitCode::FAILURE;
    }

    let profile = if options.release { "release" } else { "debug" };
    for arch in &options.arches {
        eprintln!("=== build {} {profile}", arch.name);
        if !build(arch, options.release) {
            return ExitCode::FAILURE;
        }
    }

    let out_dir = target_dir().join("kernel");
    if let Err(e) = fs::create_dir_all(&out_dir) {
        eprintln!("can't create {}: {e}", out_dir.display());
        return ExitCode::FAILURE;
    }
    let mut runs = Vec::new();
    for arch in &options.arches {
        let elf = target_dir().join(arch.triple).join(profile).join("green-tea-kernel");
        match Run::start(arch, &elf, &out_dir, options.headless) {
            Ok(run) => runs.push(run),
            Err(e) => {
                eprintln!("failed to start {}: {e}", arch.qemu);
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
        eprintln!("{status:4}  {:8} {detail}", run.arch.name);
    }
    eprintln!("serial logs: {}", out_dir.display());
    if runs.iter().all(|r| r.outcome.as_ref().is_some_and(|o| o.is_ok())) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options { arches: Vec::new(), release: false, headless: false, screenshot: false };
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
                options.arches.push(arch);
            }
            "-h" | "--help" => return Err(String::new()),
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    if options.arches.is_empty() {
        options.arches = ARCHES.iter().collect();
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

fn build(arch: &Arch, release: bool) -> bool {
    let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(workspace_root())
        .args(["build", "-p", "green-tea-kernel", "--target", arch.triple]);
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

/// One QEMU and what we've learned from it so far.
struct Run {
    arch: &'static Arch,
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
    fn start(arch: &'static Arch, elf: &Path, out_dir: &Path, headless: bool) -> std::io::Result<Run> {
        let log = out_dir.join(format!("{}.log", arch.name));
        let _ = fs::remove_file(&log);
        let mut cmd = Command::new(arch.qemu);
        cmd.args(arch.machine)
            .args(["-m", "128M", "-device", "ramfb"])
            .args(["-name", &format!("green-tea-kernel ({})", arch.name)])
            .arg("-serial")
            .arg(format!("file:{}", log.display()))
            .args(["-monitor", "stdio"])
            .arg("-kernel")
            .arg(elf)
            .stdin(Stdio::piped())
            .stdout(Stdio::null()); // the monitor's echo; errors still go to stderr
        if headless {
            cmd.args(["-display", "none"]);
        }
        Ok(Run {
            arch,
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
            if line.starts_with("green-tea-kernel on") {
                self.seen_kernel = true;
            }
            if self.seen_kernel {
                eprintln!("[{:7}] {line}", self.arch.name);
            }
            if self.outcome.is_none() {
                self.outcome = verdict(line);
            }
        }
    }
}

/// The kernel's last word: its switch count against the count it expected, or a panic.
fn verdict(line: &str) -> Option<Result<String, String>> {
    if let Some(rest) = line.strip_prefix("panic: ") {
        return Some(Err(format!("kernel panicked: {rest}")));
    }
    let rest = line.strip_prefix("all tasks finished after ")?;
    let (got, rest) = rest.split_once(" switches (expected ")?;
    let expected = rest.strip_suffix(')')?;
    Some(if got == expected {
        Ok(format!("{got} switches, as expected"))
    } else {
        Err(format!("{got} switches, expected {expected}"))
    })
}

fn watch(runs: &mut [Run], options: &Options, out_dir: &Path) {
    let start = Instant::now();
    while runs.iter().any(|r| !r.exited) {
        for run in runs.iter_mut().filter(|r| !r.exited) {
            let had_outcome = run.outcome.is_some();
            run.follow();
            if !had_outcome && run.outcome.is_some() {
                if options.screenshot {
                    let png = out_dir.join(format!("{}.png", run.arch.name));
                    run.monitor(&format!("screendump {} -f png", png.display()));
                    eprintln!("[{:7}] screenshot: {}", run.arch.name, png.display());
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
    use super::verdict;

    #[test]
    fn matching_counts_pass() {
        assert!(verdict("all tasks finished after 57 switches (expected 57)").unwrap().is_ok());
    }

    #[test]
    fn mismatched_counts_fail() {
        let v = verdict("all tasks finished after 56 switches (expected 57)").unwrap();
        assert_eq!(v, Err("56 switches, expected 57".into()));
    }

    #[test]
    fn a_panic_fails() {
        assert!(verdict("panic: no etc/ramfb (run qemu with -device ramfb)").unwrap().is_err());
    }

    #[test]
    fn other_lines_are_not_a_verdict() {
        assert_eq!(verdict("running 5 workers; expecting 57 switches"), None);
    }
}
