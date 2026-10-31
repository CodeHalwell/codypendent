#![cfg(feature = "schema-export")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn export_directory(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "codypendent-protocol-schema-{label}-{}-{nonce}",
        std::process::id(),
    ))
}

fn exported_files(directory: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = fs::read_dir(directory)
        .expect("read export directory")
        .map(|entry| {
            let entry = entry.expect("read export entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path()).expect("read exported schema"),
            )
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

const EXPECTED_SCHEMAS: &[&str] = &[
    "analytics-catalog.schema.json",
    "artifact-ref.schema.json",
    "automation-catalog.schema.json",
    "bundle-catalog.schema.json",
    "catchup.schema.json",
    "client-hello.schema.json",
    "command-body.schema.json",
    "command.schema.json",
    "data-classification.schema.json",
    "diagnostic-severity.schema.json",
    "diagnostic.schema.json",
    "diff-request.schema.json",
    "dirty-buffer-digest.schema.json",
    "editor-selection.schema.json",
    "envelope.schema.json",
    "event-body.schema.json",
    "id-catalog.schema.json",
    "ide-context-update.schema.json",
    "ide-request.schema.json",
    "inbox-catalog.schema.json",
    "input-envelope.schema.json",
    "location.schema.json",
    "payload.schema.json",
    "pending-approval-projection.schema.json",
    "position.schema.json",
    "protocol-version.schema.json",
    "range.schema.json",
    "server-hello.schema.json",
    "session-catalog.schema.json",
    "session-event.schema.json",
    "session-projection.schema.json",
    "source-provenance.schema.json",
    "text-edit.schema.json",
    "workflow-catalog.schema.json",
    "workspace-edit.schema.json",
];

#[test]
fn schema_export_is_byte_identical_across_runs() {
    let first = export_directory("first");
    let second = export_directory("second");
    let exporter = env!("CARGO_BIN_EXE_export_schema");

    for output in [&first, &second] {
        let result = Command::new(exporter)
            .args(["--output-dir", output.to_str().expect("UTF-8 temp path")])
            .output()
            .expect("run schema exporter");
        assert!(
            result.status.success(),
            "schema exporter failed with {}\nstdout:\n{}\nstderr:\n{}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr),
        );
    }

    let first_files = exported_files(&first);
    let second_files = exported_files(&second);
    assert!(!first_files.is_empty(), "exporter produced no schemas");
    assert_eq!(
        first_files, second_files,
        "two clean exports must have byte-identical names and contents",
    );
    assert_eq!(
        first_files
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        EXPECTED_SCHEMAS,
    );
    for (name, bytes) in &first_files {
        assert!(bytes.ends_with(b"\n"), "{name} has no final newline");
        serde_json::from_slice::<serde_json::Value>(bytes)
            .unwrap_or_else(|error| panic!("{name} is not valid JSON: {error}"));
    }

    fs::remove_dir_all(first).expect("remove first export");
    fs::remove_dir_all(second).expect("remove second export");
}
