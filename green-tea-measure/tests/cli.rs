//! The command line, run as a program.

use std::process::{Command, Output};

fn measure(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_green-tea-measure")).args(args).output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_mistake_on_the_command_line_prints_the_usage() {
    for args in [&["frobnicate"][..], &["space"], &["space", "x", "--passes"], &["space", "x", "--frobnicate"]] {
        let output = measure(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("usage:"), "{args:?}: {}", stderr(&output));
    }
}

#[test]
fn an_error_that_is_not_about_the_command_line_does_not() {
    let output = measure(&["space", "/nonexistent/trace.gtt"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("/nonexistent/trace.gtt"), "{}", stderr(&output));
    assert!(!stderr(&output).contains("usage:"), "{}", stderr(&output));
}

/// A directory with a `valgrind` that runs `script`, and a small trace.
struct Fake {
    dir: std::path::PathBuf,
}

impl Fake {
    fn new(name: &str, script: &str) -> Fake {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("green-tea-measure-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let valgrind = dir.join("valgrind");
        std::fs::write(&valgrind, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&valgrind, std::fs::Permissions::from_mode(0o755)).unwrap();
        let records = [
            green_tea_fit::tracefile::Record::Allocate { addr: 0xa0, size: 16, align: 16 },
            green_tea_fit::tracefile::Record::Free { addr: 0xa0 },
        ];
        let mut text = String::new();
        green_tea_fit::tracefile::write_text(&records, &mut text).unwrap();
        std::fs::write(dir.join("small.gtt"), text).unwrap();
        Fake { dir }
    }

    fn trace(&self) -> String {
        self.dir.join("small.gtt").to_str().unwrap().to_string()
    }

    fn run(&self, args: &[&str]) -> (Output, String) {
        let path = format!("{}:{}", self.dir.display(), std::env::var("PATH").unwrap());
        let output =
            Command::new(env!("CARGO_BIN_EXE_green-tea-measure")).args(args).env("PATH", path).output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        (output, stdout)
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const COUNTED: &str = "echo '==1== Collected : 100' >&2; exit 0";

#[test]
fn a_valgrind_that_fails_is_not_a_refused_allocation() {
    let script = format!("case \"$*\" in *\" null \"*) {COUNTED};; esac; echo 'valgrind broke' >&2; exit 7");
    let fake = Fake::new("instructions-broken", &script);
    let (output, stdout) = fake.run(&["instructions", &fake.trace(), "--only", "tlsf"]);
    assert!(!stdout.contains("refused"), "{stdout}");
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("valgrind broke"), "{}", stderr(&output));
}

#[test]
fn an_allocator_that_refuses_is_still_reported_as_refusing() {
    let script = format!(
        "case \"$*\" in *\" null \"*) {COUNTED};; esac; echo 'tlsf refused an allocation'; exit 1"
    );
    let fake = Fake::new("instructions-refused", &script);
    let (_, stdout) = fake.run(&["instructions", &fake.trace(), "--only", "tlsf"]);
    assert!(stdout.contains("tlsf         refused an allocation"), "{stdout}");
}

#[test]
fn a_lisp_run_that_fails_is_not_a_program_that_doesnt_fit() {
    // Everything is counted but a garbage-collected heap of a real size.
    let script = format!(
        "case \"$*\" in *\" 4194304\"*|*floor*|*explicit:*) {COUNTED};; esac; echo 'valgrind broke' >&2; exit 7"
    );
    let fake = Fake::new("lisp-broken", &script);
    let (output, stdout) = fake.run(&["lisp", "instructions", "--multiples", "1"]);
    assert!(!stdout.contains("doesn't fit"), "{stdout}");
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("valgrind broke"), "{}", stderr(&output));
}

#[test]
fn a_program_out_of_heap_is_still_reported_as_not_fitting() {
    let script = format!(
        "case \"$*\" in *\" 4194304\"*|*floor*|*explicit:*) {COUNTED};; esac; echo 'list in 9 words: HeapFull'; exit 1"
    );
    let fake = Fake::new("lisp-full", &script);
    let (_, stdout) = fake.run(&["lisp", "instructions", "--multiples", "1"]);
    assert!(stdout.contains("the program doesn't fit"), "{stdout}");
}
