use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use codypendent_knowledge::{EmbedError, SemanticEmbedder};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tracing::{info, warn};

use super::HttpEmbedder;
use crate::models::EmbeddingConfig;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BundleManifest {
    model: String,
    model_sha256: String,
    server_sha256: String,
    runtime_revision: String,
    dimensions: usize,
    prompt_format: String,
}

pub struct BundledEmbedder {
    manifest: BundleManifest,
    identity: String,
    ready: watch::Receiver<Option<Arc<HttpEmbedder>>>,
    supervisor: tokio::task::JoinHandle<()>,
    verification_cancelled: Arc<AtomicBool>,
}

impl BundledEmbedder {
    pub fn discover() -> Result<Self, EmbedError> {
        let executable = std::env::current_exe().map_err(error)?;
        let directory = bundle_candidates(&executable)
            .into_iter()
            .find(|directory| directory.join("embedding-runtime.json").is_file())
            .ok_or_else(|| EmbedError("bundled embedding model is missing".to_string()))?;
        Self::from_directory(&directory)
    }

    pub fn from_directory(directory: &Path) -> Result<Self, EmbedError> {
        let manifest: BundleManifest = serde_json::from_slice(
            &std::fs::read(directory.join("embedding-runtime.json")).map_err(error)?,
        )
        .map_err(error)?;
        if manifest.model != "embeddinggemma-2"
            || manifest.dimensions != 768
            || manifest.prompt_format != "eg2-v1"
            || manifest.model_sha256.len() != 64
        {
            return Err(EmbedError(
                "unsupported bundled embedding manifest".to_string(),
            ));
        }
        let identity = format!(
            "bundled/{}/{}/{}/{}",
            manifest.model,
            manifest.model_sha256,
            manifest.runtime_revision,
            manifest.prompt_format
        );
        let (ready, receiver) = watch::channel(None);
        let directory = directory.to_path_buf();
        let configuration = manifest.clone();
        let space = identity.clone();
        let verification_cancelled = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&verification_cancelled);
        let runtime = tokio::runtime::Handle::try_current().map_err(error)?;
        let supervisor = runtime.spawn(async move {
            let checked = directory.clone();
            let expected = configuration.clone();
            let verification_cancelled = Arc::clone(&cancelled);
            let verification = tokio::task::spawn_blocking(move || {
                verify_file(
                    &checked.join("model.gguf"),
                    &expected.model_sha256,
                    &verification_cancelled,
                )?;
                verify_file(
                    &server_binary(&checked),
                    &expected.server_sha256,
                    &verification_cancelled,
                )
            })
            .await;
            match verification {
                Ok(Ok(())) => {}
                _ => {
                    if !cancelled.load(Ordering::Relaxed) {
                        warn!("bundled embedding model checksum verification failed");
                    }
                    return;
                }
            }
            loop {
                if let Err(failure) = serve(&directory, &configuration, &space, &ready).await {
                    warn!(%failure, "bundled embedding runtime unavailable; retrying in background");
                }
                ready.send_replace(None);
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        Ok(Self {
            manifest,
            identity,
            ready: receiver,
            supervisor,
            verification_cancelled,
        })
    }

    fn client(&self) -> Result<Arc<HttpEmbedder>, EmbedError> {
        self.ready
            .borrow()
            .clone()
            .ok_or_else(|| EmbedError("bundled embedding model is warming up".to_string()))
    }
}

impl Drop for BundledEmbedder {
    fn drop(&mut self) {
        // Aborting the async supervisor cannot stop an active spawn_blocking
        // hash. Let that task stop between chunks so runtime shutdown does not
        // wait for the entire model file after the daemon has stopped serving.
        self.verification_cancelled.store(true, Ordering::Relaxed);
        self.supervisor.abort();
    }
}

#[async_trait::async_trait]
impl SemanticEmbedder for BundledEmbedder {
    fn model(&self) -> &str {
        &self.manifest.model
    }
    fn space_id(&self) -> &str {
        &self.identity
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.client()?.embed_query(text).await
    }
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.client()?.embed_batch(texts).await
    }
}

fn bundle_candidates(executable: &Path) -> Vec<PathBuf> {
    let parent = executable.parent().unwrap_or_else(|| Path::new("."));
    let mut candidates = vec![
        parent.join("embedding-runtime"),
        parent.join("../Resources/embedding-runtime"),
        parent.join("../embedding-runtime"),
        parent.join("../lib/codypendent/embedding-runtime"),
    ];
    if let Some(library) = std::env::var_os("CODYPENDENT_LIB") {
        candidates.insert(0, PathBuf::from(library).join("embedding-runtime"));
    }
    candidates
}

fn verify_file(path: &Path, expected: &str, cancelled: &AtomicBool) -> Result<(), EmbedError> {
    verify_reader(
        std::fs::File::open(path).map_err(error)?,
        expected,
        cancelled,
    )
}

fn verify_reader(
    mut reader: impl std::io::Read,
    expected: &str,
    cancelled: &AtomicBool,
) -> Result<(), EmbedError> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(EmbedError(
                "bundled embedding verification cancelled".to_string(),
            ));
        }
        let count = reader.read(&mut buffer).map_err(error)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    if hex::encode(hasher.finalize()) != expected {
        return Err(EmbedError(
            "bundled embedding model checksum mismatch".to_string(),
        ));
    }
    Ok(())
}

async fn serve(
    directory: &Path,
    manifest: &BundleManifest,
    identity: &str,
    ready: &watch::Sender<Option<Arc<HttpEmbedder>>>,
) -> Result<(), EmbedError> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(error)?;
    let port = listener.local_addr().map_err(error)?.port();
    drop(listener);
    let token = uuid::Uuid::new_v4().to_string();
    let binary = server_binary(directory);
    let mut child = tokio::process::Command::new(binary)
        .arg("--model")
        .arg(directory.join("model.gguf"))
        .args([
            "--embedding",
            "--pooling",
            "mean",
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--api-key",
            &token,
            "--alias",
            &manifest.model,
            "--ctx-size",
            "8192",
            "--batch-size",
            "8192",
            "--ubatch-size",
            "8192",
            "--parallel",
            "4",
            "--threads",
            "2",
            "--threads-batch",
            "2",
            "--no-webui",
            "--no-warmup",
        ])
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(error)?;
    let embedder = HttpEmbedder::from_config(&EmbeddingConfig {
        provider: "openai-compatible".to_string(),
        base_url: format!("http://127.0.0.1:{port}/v1"),
        model: manifest.model.clone(),
        api_key_env: String::new(),
        dims: Some(manifest.dimensions),
    })?
    .with_identity(identity.to_string(), token);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(status) = child.try_wait().map_err(error)? {
            return Err(EmbedError(format!(
                "bundled embedding runtime exited: {status}"
            )));
        }
        if tokio::time::timeout(Duration::from_secs(2), embedder.embed_query("Find a tool"))
            .await
            .is_ok_and(|result| result.is_ok())
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(EmbedError(
                "bundled embedding runtime startup timed out".to_string(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    ready.send_replace(Some(Arc::new(embedder)));
    info!(model = %manifest.model, "bundled semantic retrieval ready");
    let status = child.wait().await.map_err(error)?;
    Err(EmbedError(format!(
        "bundled embedding runtime exited: {status}"
    )))
}

fn error(error: impl std::fmt::Display) -> EmbedError {
    EmbedError(error.to_string())
}

fn server_binary(directory: &Path) -> PathBuf {
    directory.join(if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_resources_and_cli_sidecar_are_discovered() {
        let candidates = bundle_candidates(Path::new("/App/Contents/MacOS/codypendent"));
        assert!(candidates.contains(&PathBuf::from(
            "/App/Contents/MacOS/../Resources/embedding-runtime"
        )));
        assert!(candidates.contains(&PathBuf::from("/App/Contents/MacOS/embedding-runtime")));
    }

    #[test]
    fn model_checksum_rejects_changed_weights() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("model.gguf"), b"weights").unwrap();
        let expected = hex::encode(Sha256::digest(b"weights"));
        let cancelled = AtomicBool::new(false);
        assert!(verify_file(&directory.path().join("model.gguf"), &expected, &cancelled).is_ok());
        assert!(verify_file(
            &directory.path().join("model.gguf"),
            &"0".repeat(64),
            &cancelled
        )
        .is_err());
    }

    #[test]
    fn model_verification_stops_between_chunks_when_its_owner_is_dropped() {
        struct CancelAfterFirstRead<'a> {
            cancelled: &'a AtomicBool,
            reads: usize,
        }
        impl std::io::Read for CancelAfterFirstRead<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                assert_eq!(
                    self.reads, 1,
                    "cancelled verification must not read another chunk"
                );
                buffer[..7].copy_from_slice(b"weights");
                self.cancelled.store(true, Ordering::Relaxed);
                Ok(7)
            }
        }
        let cancelled = AtomicBool::new(false);
        let mut reader = CancelAfterFirstRead {
            cancelled: &cancelled,
            reads: 0,
        };
        let expected = hex::encode(Sha256::digest(b"weights"));
        let failure = verify_reader(&mut reader, &expected, &cancelled).unwrap_err();
        assert!(failure.to_string().contains("cancelled"));
        assert_eq!(reader.reads, 1);
    }
}
