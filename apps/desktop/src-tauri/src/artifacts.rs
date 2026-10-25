//! Suggest a filename for the native artifact Save dialog.
use codypendent_protocol::ArtifactRef;

pub(crate) fn suggested_name(artifact: &ArtifactRef) -> String {
    let extension = match artifact.media_type.split(';').next().unwrap_or("").trim() {
        "application/json" => "json",
        "text/csv" => "csv",
        "text/markdown" => "md",
        "text/x-diff" | "text/x-patch" => "patch",
        "text/plain" => "txt",
        _ => "bin",
    };
    format!("codypendent-{}.{}", artifact.id, extension)
}
