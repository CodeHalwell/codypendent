//! The one writer of `models.toml`.
//!
//! `models.toml` is a **shared configuration document**, not a model list.
//! `codypendent-runtime` parses five tables out of it: `[[model]]`,
//! `[embedding]`, `[retrieval]`, `[transcription]` and `[speech]`. Four
//! separate code paths add or remove a model — `models add`, `models pull`, the
//! TUI's add-model flow, and ACP client connect/disconnect — and each one grew
//! its own serializer.
//!
//! Three of the four independently rediscovered the same bug and fixed it in
//! place, leaving a comment behind:
//!
//! * `models_pull.rs` — "would silently delete every one of them, disabling a
//!   user's voice and retrieval setup"
//! * `acp_clients.rs` — "silently erased those tables" (past tense: it shipped)
//! * `tui.rs` — "must survive adding a model from the TUI"
//!
//! The fourth, `models add`, did not, and destroyed a user's embedding,
//! retrieval, STT and TTS configuration on every invocation while printing
//! `added model …`.
//!
//! Patching the fourth site would have left four copies of one invariant and a
//! fifth writer free to reintroduce it. So the invariant lives here instead:
//! **edit the parsed document in place; never serialize the file from a struct
//! that models only one section.** Adding a table to `models.toml` requires no
//! change to this module — an unknown table is carried through untouched.
//!
//! "In place" means `toml_edit`, not `toml::Value`. A `toml::Value` round-trip
//! preserves every unknown TABLE but has nowhere to hold a comment, a blank
//! line, or key order, so re-rendering the document silently deleted every
//! comment the user had written — in the file the documentation tells them to
//! hand-edit. `crates/cli/src/tui.rs::write_remove_model` already used
//! `toml_edit` for exactly this reason; [`write_model_entries_locked`] now does
//! the same. Everything outside the `[[model]]` array survives byte for byte.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::models::{load_models, ModelConfig};
use anyhow::{anyhow, bail, Context};
use fs4::FileExt;

/// Replace the `[[model]]` array in `path` with `configs`, preserving every
/// other table, and install it atomically at mode 0600.
///
/// A missing file is created. A file that is not a TOML table is an error
/// rather than something to overwrite — that is a user's file with content this
/// code does not understand.
pub fn write_model_entries(path: &Path, configs: &[ModelConfig]) -> anyhow::Result<()> {
    update_model_entries(path, |current| {
        current.clear();
        current.extend_from_slice(configs);
        Ok(())
    })
}

/// Serialize one read-modify-write of the shared model list under an advisory
/// lock. The edit closure sees the latest file contents after earlier writers
/// commit, so independent CLI/TUI/ACP updates cannot erase each other.
pub fn update_model_entries<R>(
    path: &Path,
    edit: impl FnOnce(&mut Vec<ModelConfig>) -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{}: has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let lock_path = parent.join(".models.toml.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing {}", lock_path.display()))?;
    }
    lock.lock_exclusive()
        .with_context(|| format!("locking {}", lock_path.display()))?;

    let mut configs = if path.exists() {
        load_models(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        Vec::new()
    };
    let result = edit(&mut configs)?;
    write_model_entries_locked(path, &configs)?;
    FileExt::unlock(&lock).with_context(|| format!("unlocking {}", lock_path.display()))?;
    Ok(result)
}

/// The `[[model]]` array alone, rendered so it can be lifted into an existing
/// document as a single `toml_edit` item.
#[derive(serde::Serialize)]
struct ModelArrayFragment<'a> {
    model: &'a [ModelConfig],
}

/// Render `configs` as a `toml_edit` array-of-tables item, or `None` when there
/// are no models left to write.
///
/// Goes through the `toml` crate's serializer (which `ModelConfig` already
/// derives for) and re-parses the fragment, rather than `toml_edit`'s own
/// `ser` module: the workspace enables `toml_edit` without its `serde` feature,
/// so `toml_edit::ser` is not compiled in.
fn model_array_item(configs: &[ModelConfig]) -> anyhow::Result<Option<toml_edit::Item>> {
    if configs.is_empty() {
        return Ok(None);
    }
    let fragment = toml::to_string_pretty(&ModelArrayFragment { model: configs })
        .context("serializing models.toml")?;
    let mut parsed: toml_edit::DocumentMut = fragment
        .parse()
        .context("re-reading the rendered [[model]] array")?;
    Ok(parsed.remove("model"))
}

fn write_model_entries_locked(path: &Path, configs: &[ModelConfig]) -> anyhow::Result<()> {
    // `toml_edit`, not `toml::Value` + `to_string_pretty`.
    //
    // The module header above promises the file is edited IN PLACE and that
    // anything this code does not model is "carried through untouched". Round
    // tripping through `toml::Value` kept every unknown TABLE, which is what the
    // four rediscovered bugs were about — but a `toml::Value` has no comments and
    // no formatting, so re-rendering the whole document silently deleted every
    // comment the user had written, along with their key order and spacing. On
    // `models add`. Which is what `models.toml` is full of, because it is the
    // file the docs tell people to hand-write.
    //
    // `crates/cli/src/tui.rs::write_remove_model` already had this right (and
    // says so in its doc comment); this is that writer's approach applied to the
    // replace-the-array case. Everything outside the `[[model]]` array —
    // `[embedding]`, `[retrieval]`, `[transcription]`, `[speech]`, unknown future
    // tables, top-level comments, blank lines, key order — survives byte for
    // byte. The `[[model]]` array itself is replaced, because replacing it is the
    // operation.
    let mut document = if path.exists() {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        raw.parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("parsing {}", path.display()))?
    } else {
        toml_edit::DocumentMut::new()
    };
    // A file whose root is not a table is a user's file this code does not
    // understand; `DocumentMut` always parses to a table root, so the check that
    // used to live here is now structural.
    // A comment written directly above `[[model]]` is that array's PREFIX decor
    // in `toml_edit`, not a free-floating line, so replacing the array would
    // carry the user's comment out with it. Lift it onto the replacement.
    // (A comment above an array that is being REMOVED entirely does go with it:
    // it documents the model list, and there is no non-arbitrary item left to
    // reattach it to.)
    let preserved_prefix = document
        .get("model")
        .and_then(toml_edit::Item::as_array_of_tables)
        .and_then(|array| array.get(0))
        .and_then(|table| table.decor().prefix().cloned());
    match model_array_item(configs)? {
        Some(mut item) => {
            if let Some(prefix) = preserved_prefix {
                if let Some(first) = item
                    .as_array_of_tables_mut()
                    .and_then(|array| array.get_mut(0))
                {
                    first.decor_mut().set_prefix(prefix);
                }
            }
            // `insert` on an existing key keeps its position in the document.
            document.insert("model", item);
        }
        None => {
            document.remove("model");
        }
    }
    let rendered = document.to_string();

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{}: has no parent directory", path.display()))?;
    // Unique per WRITE, not per process. The previous name was pid-only and its
    // comment claimed that stopped concurrent writers sharing a temp file — it
    // did not: two writes inside one process (the TUI's background model pull
    // alongside an edit) collided on the same path, so one could rename the
    // other's half-written render into place. A counter plus the pid is unique
    // for both cases.
    //
    static WRITE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ticket = WRITE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = parent.join(format!(".models-{}-{ticket}.tmp", std::process::id()));
    let mut temp_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    temp_file
        .write_all(rendered.as_bytes())
        .with_context(|| format!("writing {}", temp.display()))?;
    temp_file
        .sync_all()
        .with_context(|| format!("syncing {}", temp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // An entry names the environment variable holding a key, never the key
        // itself, but the endpoint list is still the user's business alone.
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing {}", temp.display()))?;
    }
    std::fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("syncing {}", parent.display()))?;
    Ok(())
}

/// Remove one exact `[[model]]` entry without reserializing the surrounding
/// document. `models.toml` also contains voice, speech, embedding, retrieval,
/// and user-defined future tables; `toml_edit` preserves all of them — plus
/// comments and formatting — while only the matching array-of-tables row is
/// removed.
///
/// The model-specific `auth.json` credential is removed as part of the same
/// operation. Provider-wide credentials remain because other models from that
/// provider may still use them. Both inputs are fully parsed before either is
/// changed, so malformed configuration cannot cause a partial deletion.
pub fn remove_model(data_dir: &Path, model_id: &str) -> anyhow::Result<()> {
    use crate::auth::AuthStore;

    let model_id = model_id.trim();
    if model_id.is_empty() {
        bail!("model id must not be blank");
    }
    let models_path = data_dir.join("models.toml");

    // Held across the WHOLE two-file transaction below, load included: this
    // writes `auth.json` and `models.toml` together and undoes one half when
    // the other fails, so another writer landing between those steps would see
    // — or overwrite — a half-committed pair. Released when this returns.
    let _auth_hold = crate::auth::hold(data_dir)
        .with_context(|| format!("locking {}", data_dir.join("auth.json").display()))?;

    // Validate the key store before touching models.toml. A corrupt auth.json
    // must never be silently overwritten or leave the operator unsure which
    // half of the requested cleanup happened.
    let mut auth = AuthStore::load(data_dir)
        .with_context(|| format!("reading {}", data_dir.join("auth.json").display()))?;
    let original_auth = auth.clone();
    let removed_key = auth.remove(model_id);

    let parent = models_path
        .parent()
        .ok_or_else(|| anyhow!("{}: has no parent directory", models_path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let lock_path = parent.join(".models.toml.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing {}", lock_path.display()))?;
    }
    fs4::FileExt::lock_exclusive(&lock)
        .with_context(|| format!("locking {}", lock_path.display()))?;

    let raw = std::fs::read_to_string(&models_path)
        .with_context(|| format!("reading {}", models_path.display()))?;
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing {}", models_path.display()))?;

    // Remove EVERY entry carrying this id, not just the first: a hand-edited
    // `models.toml` can list the same id twice, and `auth.remove` above is
    // unconditional — leaving one copy behind would leave a listed model with
    // no credential.
    let emptied = {
        let Some(item) = doc.get_mut("model") else {
            bail!("model `{model_id}` is not configured");
        };
        let Some(array) = item.as_array_of_tables_mut() else {
            bail!(
                "`model` entry in {} is not an array of tables",
                models_path.display()
            );
        };
        let matching: Vec<usize> = array
            .iter()
            .enumerate()
            .filter_map(|(index, table)| {
                (table.get("id").and_then(toml_edit::Item::as_str) == Some(model_id))
                    .then_some(index)
            })
            .collect();
        if matching.is_empty() {
            bail!("model `{model_id}` is not configured");
        }
        for index in matching.into_iter().rev() {
            array.remove(index);
        }
        array.is_empty()
    };
    if emptied {
        doc.remove("model");
    }

    // Unique per write, not per process: two removals inside one process
    // (the TUI's own and a background reload) collided on a pid-only name,
    // exactly the hazard `models_file::update_model_entries` documents.
    static REMOVE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ticket = REMOVE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = parent.join(format!(".models-{}-{ticket}.toml.tmp", std::process::id()));
    let write_tmp = || -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(doc.to_string().as_bytes())?;
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        std::fs::write(&tmp_path, doc.to_string())?;
        Ok(())
    };
    if let Err(error) = write_tmp() {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error).with_context(|| format!("writing {}", tmp_path.display()));
    }

    // Commit the secret cleanup first, then make the already-written model temp
    // visible. Either half can still fail, so each failure path undoes the
    // other half: a failed `auth.save` leaves models.toml untouched (the temp is
    // discarded, never renamed), and a failed rename restores the original key
    // store. The operation is all-or-nothing in both directions.
    if removed_key {
        if let Err(error) = auth.save(data_dir) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(error)
                .with_context(|| format!("writing {}", data_dir.join("auth.json").display()));
        }
    }
    if let Err(error) = std::fs::rename(&tmp_path, &models_path) {
        let _ = std::fs::remove_file(&tmp_path);
        if removed_key {
            original_auth.save(data_dir).with_context(|| {
                format!(
                    "restoring {} after models.toml replacement failed",
                    data_dir.join("auth.json").display()
                )
            })?;
        }
        return Err(error).with_context(|| format!("replacing {}", models_path.display()));
    }
    let _ = fs4::FileExt::unlock(&lock);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> ModelConfig {
        ModelConfig {
            id: codypendent_protocol::ModelId(id.to_string()),
            provider: "openai-compatible".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            model: id.to_string(),
            api_key_env: "EXAMPLE_KEY".to_string(),
            provider_id: Some("example".to_string()),
            context_tokens: Some(128_000),
        }
    }

    /// The module header promises the file is edited in place and that anything
    /// this code does not model is carried through untouched. A `toml::Value`
    /// round-trip kept every unknown TABLE but had nowhere to keep a comment, so
    /// `models add` silently deleted every line the user had written to explain
    /// their own configuration — in the one file the docs tell them to hand-edit.
    #[test]
    fn writing_models_preserves_comments_and_formatting_everywhere_else() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("models.toml");
        std::fs::write(
            &path,
            r#"# Codypendent models. Edited by hand; `codypendent models add` also writes here.

[embedding]
# Ollama, because the laptop is offline on the train.
provider = "ollama"
model = "nomic-embed-text"

[retrieval]
builtin_top_k = 0  # deliberately OFF — see the incident on 2026-03-02

# the models themselves; keys come from `codypendent models add`
[[model]]
id = "existing/one"
provider = "openai-compatible"
base_url = "https://api.example.com/v1"
model = "existing-one"
api_key_env = "EXAMPLE_KEY"
"#,
        )
        .expect("seed the file");

        write_model_entries(&path, &[entry("existing/one"), entry("openai/gpt-4o")])
            .expect("write");

        let written = std::fs::read_to_string(&path).expect("read back");
        for comment in [
            "# Codypendent models. Edited by hand",
            "# Ollama, because the laptop is offline on the train.",
            "# deliberately OFF — see the incident on 2026-03-02",
            // Directly above `[[model]]`: this one is the replaced array's own
            // prefix decor, so it survives only because it is carried over.
            "# the models themselves; keys come from `codypendent models add`",
        ] {
            assert!(
                written.contains(comment),
                "comment {comment:?} was stripped by the write:\n{written}"
            );
        }

        // The edit itself still happened, and the file is still valid TOML.
        let reparsed: toml::Value = toml::from_str(&written).expect("valid TOML");
        assert_eq!(
            reparsed["model"].as_array().map(Vec::len),
            Some(2),
            "both models are present"
        );
        assert_eq!(reparsed["retrieval"]["builtin_top_k"].as_integer(), Some(0));
    }

    /// Emptying the model list removes the array rather than writing an empty
    /// one, and still leaves the rest of the user's document alone.
    #[test]
    fn removing_every_model_leaves_the_rest_of_the_document_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("models.toml");
        std::fs::write(
            &path,
            r#"# keep me

[embedding]
provider = "ollama"
model = "nomic-embed-text"

# the models themselves
[[model]]
id = "existing/one"
provider = "openai-compatible"
base_url = "https://api.example.com/v1"
model = "existing-one"
api_key_env = "EXAMPLE_KEY"
"#,
        )
        .expect("seed");

        write_model_entries(&path, &[]).expect("write");

        let written = std::fs::read_to_string(&path).expect("read back");
        assert!(written.contains("# keep me"));
        let reparsed: toml::Value = toml::from_str(&written).expect("valid TOML");
        assert!(reparsed.get("model").is_none(), "the array is removed");
        assert_eq!(reparsed["embedding"]["provider"].as_str(), Some("ollama"));
    }

    /// The bug this module exists to make unrepresentable: `models add` rebuilt
    /// the file from a model-only struct, so adding a model silently turned off
    /// the user's embeddings, retrieval tuning, speech-to-text and
    /// text-to-speech.
    #[test]
    fn adding_a_model_preserves_every_other_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("models.toml");
        std::fs::write(
            &path,
            r#"
[[model]]
id = "existing/one"
provider = "openai-compatible"
base_url = "https://api.example.com/v1"
model = "existing-one"
api_key_env = "EXAMPLE_KEY"

[embedding]
provider = "ollama"
model = "nomic-embed-text"

[retrieval]
mcp_top_k = 12
builtin_top_k = 0

[transcription]
base_url = "http://localhost:9000/v1"
model = "whisper-1"

[speech]
base_url = "http://localhost:9000/v1"
model = "tts-1"
voice = "alloy"

[a-table-a-later-version-adds]
setting = true
"#,
        )
        .expect("seed the file");

        write_model_entries(&path, &[entry("existing/one"), entry("openai/gpt-4o")])
            .expect("write");

        let reparsed: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap())
            .expect("the result is valid TOML");
        let root = reparsed.as_table().expect("a table");
        for table in [
            "embedding",
            "retrieval",
            "transcription",
            "speech",
            "a-table-a-later-version-adds",
        ] {
            assert!(root.contains_key(table), "[{table}] must survive the write");
        }
        // The real `RetrievalSettings` keys, by name: both are `#[serde(default)]`,
        // so a second writer that round-tripped the struct would silently reset
        // `builtin_top_k = 0` (retrieval gating deliberately OFF) back to the
        // default and quietly change what tools the model is shown.
        assert_eq!(root["retrieval"]["mcp_top_k"].as_integer(), Some(12));
        assert_eq!(root["retrieval"]["builtin_top_k"].as_integer(), Some(0));
        assert_eq!(root["speech"]["voice"].as_str(), Some("alloy"));
        assert_eq!(
            root["model"].as_array().map(Vec::len),
            Some(2),
            "both models are present"
        );
    }

    #[test]
    fn a_missing_file_is_created_with_just_the_models() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("models.toml");
        write_model_entries(&path, &[entry("openai/gpt-4o")]).expect("write");
        let models = crate::models::load_models(&path).expect("load");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id.0, "openai/gpt-4o");
    }

    #[test]
    fn concurrent_model_updates_are_serialized_without_lost_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("models.toml");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut workers = Vec::new();
        for index in 0..8 {
            let path = path.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                let model = entry(&format!("concurrent/{index}"));
                update_model_entries(&path, |models| {
                    models.push(model);
                    Ok(())
                })
                .expect("serialized update");
            }));
        }
        for worker in workers {
            worker.join().expect("worker");
        }
        let models = load_models(&path).expect("load");
        assert_eq!(models.len(), 8);
        for index in 0..8 {
            assert!(models
                .iter()
                .any(|model| model.id.0 == format!("concurrent/{index}")));
        }
    }
}
