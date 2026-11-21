//! `cargo xtask trace`: get a trace out of the kernel.
//!
//! The kernel has no disk and no network. It has a serial port, which QEMU writes to a
//! file. So the shell is told to record, to run its script, and to write the recording to
//! its console as a trace file (`trace dump`), and this picks the file out of the log:
//! everything from the line `gtt 1` to the line that starts with `end`.

use super::{Board, ARCHES, Arch, Job, KERNELS, build, preflight, target_dir};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const USAGE: &str = "\
usage: cargo xtask trace [--target riscv64|aarch64|x86_64] [--release] [--out <file>] [--type <command>]...

Boots green-tea-shell in QEMU without a window and types at it: `trace on`, the commands
to record, `trace off`, `replay`, `trace dump`, `halt`. Saves the trace the shell dumps on
its serial console to <file>, in the text form, and the whole serial log to <file>.log.

  --target   the architecture (default: riscv64)
  --release  build with optimizations
  --out      where to write the trace (default: target/kernel/shell-<arch>.gtt)
  --type     a command to record, in place of `session`; may be given several times";

const TIMEOUT: Duration = Duration::from_secs(300);

struct Options {
    arch: &'static Arch,
    release: bool,
    out: Option<PathBuf>,
    commands: Vec<String>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options { arch: &ARCHES[0], release: false, out: None, commands: Vec::new() };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--release" => options.release = true,
            "--target" => {
                let value = value()?;
                options.arch = ARCHES
                    .iter()
                    .find(|a| a.name == value || a.triple == value)
                    .ok_or_else(|| format!("unknown target {value}; known: riscv64, aarch64, x86_64"))?;
            }
            "--out" => options.out = Some(PathBuf::from(value()?)),
            "--type" => options.commands.push(value()?.clone()),
            "-h" | "--help" => return Err(String::new()),
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    if options.commands.is_empty() {
        options.commands.push("session".into());
    }
    Ok(options)
}

/// What to type at the shell. The empty first line is there to be lost: on riscv64 the
/// firmware reads the serial port before the kernel does, and takes the first byte with it.
fn script(commands: &[String]) -> String {
    format!("\ntrace on\n{}\ntrace off\nreplay\ntrace dump\nhalt\n", commands.join("\n"))
}

/// The trace file in a serial log: the lines from `gtt 1` to `end`, with the console's
/// carriage returns removed. Also how many records it holds.
///
/// The count is checked against the `end` line here, so that a dump that was cut short is
/// never saved as a trace. The checksum is left to whoever reads the file: xtask has no
/// dependencies, green-tea-fit included.
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

pub fn main(args: &[String]) -> ExitCode {
    let options = match parse(args) {
        Ok(options) => options,
        Err(e) if e.is_empty() => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match capture(&options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn capture(options: &Options) -> Result<(), String> {
    let arch = options.arch;
    preflight(&[arch], Board::Qemu).map_err(|problems| format!("missing prerequisites: {}", problems.join("; ")))?;
    let kernel = KERNELS.iter().find(|k| k.name == "shell").expect("the shell kernel");
    if !build(&Job { kernel, arch }, options.release, Board::Qemu) {
        return Err("the shell didn't build".into());
    }
    let profile = if options.release { "release" } else { "debug" };
    let elf = target_dir().join(arch.triple).join(profile).join(kernel.package);
    let dir = target_dir().join("kernel");
    let out = options.out.clone().unwrap_or_else(|| dir.join(format!("shell-{}.gtt", arch.name)));
    let io = |e: std::io::Error| e.to_string();
    fs::create_dir_all(&dir).map_err(io)?;
    let typed = dir.join(format!("capture-{}.in", arch.name));
    let log = dir.join(format!("capture-{}.log", arch.name));
    fs::write(&typed, script(&options.commands)).map_err(io)?;
    let _ = fs::remove_file(&log);

    let text = run(arch, &elf, &typed, &log)?;
    let (trace, records) = extract(&text)?;
    fs::write(&out, trace).map_err(io)?;
    let kept = PathBuf::from(format!("{}.log", out.display()));
    fs::write(&kept, text.replace('\r', "")).map_err(io)?;
    println!("{records} records from green-tea-shell on {} written to {}", arch.name, out.display());
    println!("the serial log is in {}", kept.display());
    Ok(())
}

/// Boot the shell with `typed` on its serial port, wait for it to halt, and return the log.
fn run(arch: &Arch, elf: &Path, typed: &Path, log: &Path) -> Result<String, String> {
    let mut qemu = Command::new(arch.qemu)
        .args(arch.machine)
        .args(["-m", "128M", "-device", "ramfb", "-display", "none", "-monitor", "none"])
        .arg("-chardev")
        .arg(format!("file,id=console,path={},input-path={}", log.display(), typed.display()))
        .args(["-serial", "chardev:console"])
        .arg("-kernel")
        .arg(elf)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to start {}: {e}", arch.qemu))?;
    let start = Instant::now();
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
    let _ = qemu.kill();
    let _ = qemu.wait();
    text
}

#[cfg(test)]
mod tests {
    use super::{extract, parse, script};

    const LOG: &str = "> trace off\r\nrecorded 3 events, and dropped 0\r\n> trace dump\r\ngtt 1\r\n# recorded by \
                       green-tea-shell on riscv64\r\na 80212340 64 8\r\na 80212400 5 1\r\nf 80212340\r\nend 3 \
                       0123456789abcdef\r\n> halt\r\nhalting\r\n";

    #[test]
    fn the_trace_is_the_lines_from_gtt_to_end() {
        let (trace, records) = extract(LOG).unwrap();
        assert_eq!(records, 3);
        assert_eq!(
            trace,
            "gtt 1\n# recorded by green-tea-shell on riscv64\na 80212340 64 8\na 80212400 5 1\nf 80212340\nend 3 \
             0123456789abcdef\n"
        );
    }

    #[test]
    fn a_dump_with_a_line_missing_is_not_saved() {
        let short = LOG.replace("a 80212400 5 1\r\n", "");
        assert_eq!(extract(&short).unwrap_err(), "the dump says `end 3 0123456789abcdef`, and has 2 records");
    }

    #[test]
    fn a_dump_cut_short_is_not_saved() {
        let cut = &LOG[..LOG.find("end 3").unwrap()];
        assert_eq!(extract(cut).unwrap_err(), "the dump stops after 3 records, without an `end` line");
        assert!(extract("> trace dump\r\nnothing is recorded\r\n").unwrap_err().contains("no line `gtt 1`"));
    }

    #[test]
    fn the_script_records_the_session_unless_told_what_to_type() {
        let args = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let options = parse(&args(&[])).unwrap();
        assert_eq!(script(&options.commands), "\ntrace on\nsession\ntrace off\nreplay\ntrace dump\nhalt\n");
        let options = parse(&args(&["--type", "set a b", "--type", "echo $a", "--target", "x86_64"])).unwrap();
        assert_eq!(script(&options.commands), "\ntrace on\nset a b\necho $a\ntrace off\nreplay\ntrace dump\nhalt\n");
        assert_eq!(options.arch.name, "x86_64");
        assert!(parse(&args(&["--out"])).is_err());
    }
}
