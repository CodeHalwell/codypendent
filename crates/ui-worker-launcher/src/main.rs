//! Pre-exec resource boundary for long-lived UI workers.
//!
//! This program is deliberately tiny: parse host-generated numeric limits, set
//! supported inherited kernel rlimits, then `exec` the already-lowered sandbox frontend.
//! No shell, package-controlled path lookup, environment widening, or fallback
//! execution exists. A setup failure exits before any component JavaScript can
//! start.

use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::process::Command;

use nix::sys::resource::{setrlimit, Resource};

#[derive(Debug, PartialEq, Eq)]
struct Launch {
    memory_bytes: u64,
    cpu_seconds: u64,
    program: OsString,
    arguments: Vec<OsString>,
}

fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Launch, String> {
    let mut arguments = arguments.into_iter();
    let mut memory_bytes = None;
    let mut cpu_seconds = None;
    let mut command = Vec::new();
    while let Some(argument) = arguments.next() {
        if argument == "--" {
            command.extend(arguments);
            break;
        }
        if argument == "--memory-bytes" {
            let value = arguments
                .next()
                .ok_or_else(|| "--memory-bytes requires a value".to_string())?;
            memory_bytes = Some(
                value
                    .to_string_lossy()
                    .parse::<u64>()
                    .map_err(|_| "invalid --memory-bytes value".to_string())?,
            );
        } else if argument == "--cpu-seconds" {
            let value = arguments
                .next()
                .ok_or_else(|| "--cpu-seconds requires a value".to_string())?;
            cpu_seconds = Some(
                value
                    .to_string_lossy()
                    .parse::<u64>()
                    .map_err(|_| "invalid --cpu-seconds value".to_string())?,
            );
        } else {
            return Err(format!("unknown launcher argument {argument:?}"));
        }
    }
    if command.is_empty() {
        return Err("launcher requires an absolute program after --".into());
    }
    let program = command.remove(0);
    if !std::path::Path::new(&program).is_absolute() {
        return Err("launcher program must be absolute".into());
    }
    let memory_bytes = memory_bytes.ok_or_else(|| "missing --memory-bytes".to_string())?;
    let cpu_seconds = cpu_seconds.ok_or_else(|| "missing --cpu-seconds".to_string())?;
    if memory_bytes == 0 || cpu_seconds == 0 {
        return Err("resource limits must be greater than zero".into());
    }
    Ok(Launch {
        memory_bytes,
        cpu_seconds,
        program,
        arguments: command,
    })
}

fn set_limits(launch: &Launch) -> Result<(), String> {
    setrlimit(Resource::RLIMIT_CPU, launch.cpu_seconds, launch.cpu_seconds)
        .map_err(|error| format!("cannot set RLIMIT_CPU: {error}"))?;

    // Darwin's RLIMIT_DATA compares the limit with the entire current virtual
    // map, including reserved address space. A resident-memory budget therefore
    // rejects even this tiny launcher before exec (EINVAL), and cannot account
    // for V8's large virtual reservations. On macOS the host's mandatory process-
    // group RSS watchdog and V8 heap ceiling enforce the memory budget; it is a
    // sampled limit, not a kernel RSS cap. Do not clear inherited limits or turn
    // a failed supported setrlimit call into an unsandboxed fallback.
    let memory_resource = cfg!(target_os = "linux").then_some(Resource::RLIMIT_AS);
    if let Some(resource) = memory_resource {
        setrlimit(resource, launch.memory_bytes, launch.memory_bytes)
            .map_err(|error| format!("cannot set RLIMIT_AS: {error}"))?;
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let launch = parse(std::env::args_os().skip(1))?;
    set_limits(&launch)?;
    let error = Command::new(&launch.program).args(&launch.arguments).exec();
    Err(format!("cannot exec sandbox frontend: {error}"))
}

fn main() {
    if let Err(error) = run() {
        eprintln!("codypendent UI worker launcher refused startup: {error}");
        std::process::exit(78);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_arguments_are_required() {
        let parsed = parse([
            "--memory-bytes".into(),
            "134217728".into(),
            "--cpu-seconds".into(),
            "30".into(),
            "--".into(),
            "/usr/bin/sandbox-exec".into(),
            "-p".into(),
            "(deny default)".into(),
        ])
        .unwrap();
        assert_eq!(parsed.memory_bytes, 134_217_728);
        assert_eq!(parsed.cpu_seconds, 30);
        assert_eq!(parsed.program, "/usr/bin/sandbox-exec");
        assert_eq!(parsed.arguments.len(), 2);
        assert!(parse(["--".into(), "relative".into()]).is_err());
        assert!(parse(["--unknown".into()]).is_err());
    }
}
