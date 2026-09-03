//! Smoke tests of the built binary.

use std::process::Command;

/// Runs the binary and returns success, stdout and stderr separately.
fn krr_streams(args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_krr"))
        .args(args)
        .output()
        .expect("krr runs");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn krr(args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_krr"))
        .args(args)
        .output()
        .expect("krr runs");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text)
}

#[test]
fn prints_its_version() {
    let (ok, output) = krr(&["version"]);
    assert!(ok, "{output}");
    assert_eq!(output.trim(), env!("CARGO_PKG_VERSION"));
}

#[test]
fn lists_the_strategies() {
    let (ok, output) = krr(&["--help"]);
    assert!(ok, "{output}");
    assert!(output.contains("simple"), "{output}");
    assert!(output.contains("simple-limit"), "{output}");
}

#[test]
fn documents_the_strategy_flags() {
    let (ok, output) = krr(&["simple", "--help"]);
    assert!(ok, "{output}");

    for flag in [
        "--cpu-percentile",
        "--history-duration",
        "--timeframe-duration",
        "--prometheus-url",
        "--namespace",
        "--formatter",
        "--allow-hpa",
    ] {
        assert!(
            output.contains(flag),
            "{flag} is missing from the help:\n{output}"
        );
    }
}

#[test]
fn rejects_bad_arguments_before_touching_the_cluster() {
    let (ok, output) = krr(&["simple", "--formatter", "nope"]);
    assert!(!ok);
    assert!(output.contains("Formatter 'nope' not found"), "{output}");

    let (ok, output) = krr(&["simple", "--prometheus-url", "prom:9090"]);
    assert!(!ok);
    assert!(
        output.contains("must start with https:// or http://"),
        "{output}"
    );
}

#[test]
fn logs_never_reach_stdout() {
    // stdout carries the report, so a pipe into jq or a file must stay clean
    let (ok, stdout, stderr) = krr_streams(&["simple", "-p", "http://127.0.0.1:1", "-f", "json"]);

    assert!(!ok, "the scan cannot succeed without a metrics store");
    assert!(stdout.is_empty(), "stdout carried log output: {stdout}");
    assert!(
        stderr.contains("127.0.0.1:1"),
        "the failure was not reported: {stderr}"
    );
}
