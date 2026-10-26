//! Exercise the real pre-exec launcher without changing the test process limits.
#![cfg(unix)]

use std::process::Command;

fn launcher() -> Command {
    Command::new(env!("CARGO_BIN_EXE_codypendent-ui-worker-launcher"))
}

#[test]
fn small_worker_budget_can_exec_with_hard_cpu_limit() {
    let output = launcher()
        .args([
            "--memory-bytes",
            "134217728",
            "--cpu-seconds",
            "2",
            "--",
            "/bin/sh",
            "-c",
            "printf '%s %s' \"$(ulimit -St)\" \"$(ulimit -Ht)\"",
        ])
        .output()
        .expect("launcher starts");
    assert!(
        output.status.success(),
        "launcher refused a valid budget: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "2 2");
}

#[test]
fn invalid_resource_budget_never_executes_the_program() {
    let output = launcher()
        .args([
            "--memory-bytes",
            "0",
            "--cpu-seconds",
            "2",
            "--",
            "/bin/echo",
            "must not execute",
        ])
        .output()
        .expect("launcher starts");
    assert_eq!(output.status.code(), Some(78));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("greater than zero"));
}
