//! Project automation, run as `cargo xtask <command>` (see the alias in .cargo/config.toml).

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "\
usage: cargo xtask test [--target <triple>]... [--profile debug|release]...

Runs green-tea-v2's tests and demo binaries on the host and, under qemu, on every
cross target, in both profiles. Also checks that the library builds for targets we
can compile for but not run (Apple Silicon). --target and --profile narrow the matrix.";

/// A target in the matrix. `None` triple means the host, built without `--target`.
struct Target {
    triple: Option<&'static str>,
    /// Programs that must be on PATH: the cross linker and the qemu runner. These must
    /// match .cargo/config.toml.
    tools: &'static [&'static str],
    /// Run the tests and binaries, or only build the library.
    run: bool,
}

const TARGETS: &[Target] = &[
    Target { triple: None, tools: &[], run: true },
    Target {
        triple: Some("aarch64-unknown-linux-gnu"),
        tools: &["aarch64-linux-gnu-gcc", "qemu-aarch64"],
        run: true,
    },
    Target {
        triple: Some("riscv64gc-unknown-linux-gnu"),
        tools: &["riscv64-linux-gnu-gcc", "qemu-riscv64"],
        run: true,
    },
    // No Apple linker here, so this only proves the asm assembles for Mach-O.
    Target { triple: Some("aarch64-apple-darwin"), tools: &[], run: false },
];

const PACKAGE: &str = "green-tea-v2";
const BINS: &[&str] = &["coop-2-v2", "coop-n-v2"];
const PROFILES: &[&str] = &["debug", "release"];

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("test") => match parse_test_args(&args[1..]) {
            Ok((targets, profiles)) => test(&targets, &profiles),
            Err(e) => {
                eprintln!("error: {e}\n\n{USAGE}");
                ExitCode::FAILURE
            }
        },
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn parse_test_args(args: &[String]) -> Result<(Vec<&'static Target>, Vec<&'static str>), String> {
    let mut targets = Vec::new();
    let mut profiles = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let value = args.next().ok_or_else(|| format!("{arg} needs a value"))?;
        match arg.as_str() {
            "--target" => targets.push(
                TARGETS
                    .iter()
                    .find(|t| t.triple.unwrap_or("host") == value)
                    .ok_or_else(|| format!("unknown target {value}; known: {}", known_targets()))?,
            ),
            "--profile" => profiles.push(
                *PROFILES
                    .iter()
                    .find(|p| *p == value)
                    .ok_or_else(|| format!("unknown profile {value}; known: debug, release"))?,
            ),
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    if targets.is_empty() {
        targets = TARGETS.iter().collect();
    }
    if profiles.is_empty() {
        profiles = PROFILES.to_vec();
    }
    Ok((targets, profiles))
}

fn known_targets() -> String {
    TARGETS
        .iter()
        .map(|t| t.triple.unwrap_or("host"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn test(targets: &[&Target], profiles: &[&str]) -> ExitCode {
    if let Err(problems) = preflight(targets) {
        eprintln!("missing prerequisites:");
        for p in problems {
            eprintln!("  - {p}");
        }
        eprintln!("\nsee the header of .cargo/config.toml for the install commands");
        return ExitCode::FAILURE;
    }

    let mut results = Vec::new();
    for target in targets {
        let name = target.triple.unwrap_or("host");
        for profile in profiles {
            let steps: Vec<(String, Vec<&str>)> = if target.run {
                let mut steps = vec![("test".to_string(), vec!["test"])];
                for bin in BINS {
                    steps.push((format!("run {bin}"), vec!["run", "--bin", bin]));
                }
                steps
            } else {
                vec![("build lib".to_string(), vec!["build", "--lib"])]
            };
            for (step, args) in steps {
                eprintln!("\n=== {name} {profile}: {step}");
                let ok = cargo(&args, target.triple, profile);
                results.push((name, *profile, step, ok));
            }
        }
    }

    eprintln!("\n=== summary");
    for (target, profile, step, ok) in &results {
        eprintln!("{:4}  {target:28} {profile:7} {step}", if *ok { "ok" } else { "FAIL" });
    }
    if results.iter().all(|r| r.3) { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// Runs `cargo <args> -p green-tea-v2 [--target T] [--release]` from the workspace root.
fn cargo(args: &[&str], triple: Option<&str>, profile: &str) -> bool {
    let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(workspace_root())
        .args(args)
        .args(["-p", PACKAGE]);
    if let Some(triple) = triple {
        cmd.args(["--target", triple]);
    }
    if profile == "release" {
        cmd.arg("--release");
    }
    match cmd.status() {
        Ok(status) => status.success(),
        Err(e) => {
            eprintln!("failed to start cargo: {e}");
            false
        }
    }
}

/// Every problem that would make the run fail for a boring reason, all reported at once.
fn preflight(targets: &[&Target]) -> Result<(), Vec<String>> {
    let installed = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let mut problems = Vec::new();
    for target in targets {
        let Some(triple) = target.triple else { continue };
        if !installed.lines().any(|l| l.trim() == triple) {
            problems.push(format!("rust target {triple}: rustup target add {triple}"));
        }
        for tool in target.tools {
            if !on_path(tool) {
                problems.push(format!("{tool} (for {triple}) is not on PATH"));
            }
        }
    }
    if problems.is_empty() { Ok(()) } else { Err(problems) }
}

fn on_path(program: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf()
}
