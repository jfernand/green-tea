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
