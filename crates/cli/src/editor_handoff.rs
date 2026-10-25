//! Session-preserving editor handoff shared by `open` and the TUI.
//!
//! The URI and durable run/lease checks mirror the native desktop environment
//! boundary. No shell or provider credentials are involved.

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context};
use codypendent_protocol::{discovery::RuntimePaths, RunId, SessionId};

#[derive(Clone, Copy)]
struct Editor {
    binary: &'static str,
    scheme: &'static str,
    app: &'static str,
    name: &'static str,
}

const EDITORS: &[Editor] = &[
    Editor {
        binary: "code",
        scheme: "vscode",
        app: "Visual Studio Code.app",
        name: "VS Code",
    },
    Editor {
        binary: "cursor",
        scheme: "cursor",
        app: "Cursor.app",
        name: "Cursor",
    },
    Editor {
        binary: "code-insiders",
        scheme: "vscode-insiders",
        app: "Visual Studio Code - Insiders.app",
        name: "VS Code Insiders",
    },
];

fn editor(binary: &str) -> anyhow::Result<Editor> {
    EDITORS
        .iter()
        .copied()
        .find(|editor| editor.binary == binary)
        .context("Choose code, cursor or code-insiders for Codypendent session handoff")
}

fn executable(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(binary))
        .find(|path| {
            let Ok(metadata) = path.metadata() else {
                return false;
            };
            if !metadata.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            true
        })
}

fn application(editor: Editor) -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let system = Path::new("/Applications").join(editor.app);
    if system.is_dir() {
        return Some(system);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Applications").join(editor.app))
        .filter(|path| path.is_dir())
}

fn preferred_editor() -> anyhow::Result<Editor> {
    if let Some(binary) = std::env::var_os("CODYPENDENT_EDITOR") {
        return editor(
            binary
                .to_str()
                .context("CODYPENDENT_EDITOR must be UTF-8")?,
        );
    }
    EDITORS
        .iter()
        .copied()
        .find(|editor| executable(editor.binary).is_some() || application(*editor).is_some())
        .context(
            "Install VS Code or Cursor, or set CODYPENDENT_EDITOR to code, cursor or code-insiders",
        )
}

fn workspace_file(checkout: &Path, file: &str) -> anyhow::Result<PathBuf> {
    if file.is_empty()
        || file.contains('\\')
        || file.chars().any(char::is_control)
        || Path::new(file)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("Choose a file inside the reviewed checkout");
    }
    let root = checkout.canonicalize()?;
    let absolute = root
        .join(file)
        .canonicalize()
        .with_context(|| format!("The file {file} is no longer available in this checkout"))?;
    if !absolute.starts_with(root) || !absolute.is_file() {
        bail!("The file is outside the reviewed checkout or is not a regular file");
    }
    Ok(absolute)
}

fn handoff_uri(
    editor: Editor,
    paths: &RuntimePaths,
    session: SessionId,
    repository: &Path,
    checkout: &Path,
    source: Option<(&str, u32)>,
) -> anyhow::Result<String> {
    if let Some((file, line)) = source {
        if line == 0 {
            bail!("A source location needs a positive line number");
        }
        workspace_file(checkout, file)?;
    }
    let mut uri = reqwest::Url::parse(&format!(
        "{}://codypendent.codypendent/attach",
        editor.scheme
    ))?;
    let mut query = uri.query_pairs_mut();
    query
        .append_pair("session", &session.to_string())
        .append_pair(
            "repository",
            repository
                .to_str()
                .context("Repository path is not UTF-8")?,
        )
        .append_pair(
            "checkout",
            checkout.to_str().context("Checkout path is not UTF-8")?,
        )
        .append_pair(
            "socket",
            paths
                .socket_path
                .to_str()
                .context("Socket path is not UTF-8")?,
        );
    if let Some((file, line)) = source {
        query
            .append_pair("file", file)
            .append_pair("line", &line.to_string());
    }
    drop(query);
    Ok(uri.into())
}

fn command(editor: Editor, uri: &str) -> Command {
    // Match the native desktop launcher on macOS. A `code` shim can point to
    // another editor; the installed app identifies the URI recipient exactly.
    if let Some(app) = application(editor) {
        let mut command = Command::new("/usr/bin/open");
        command.arg("-a").arg(app).arg(uri);
        command
    } else if let Some(binary) = executable(editor.binary) {
        let mut command = Command::new(binary);
        command.arg("--open-url").arg(uri);
        command
    } else {
        // Preserve `open`'s existing NotFound/manual-attach behavior.
        let mut command = Command::new(editor.binary);
        command.arg("--open-url").arg(uri);
        command
    }
}

pub(crate) fn session_command(
    paths: &RuntimePaths,
    session: SessionId,
    binary: &str,
    repository: &Path,
) -> anyhow::Result<Command> {
    let editor = editor(binary)?;
    let root = repository_root(repository)?;
    Ok(command(
        editor,
        &handoff_uri(editor, paths, session, &root, &root, None)?,
    ))
}

fn repository_root(repository: &Path) -> anyhow::Result<PathBuf> {
    let canonical = repository
        .canonicalize()
        .context("The repository is no longer available")?;
    Ok(crate::repo_anchor::anchor_repository_path(&canonical))
}

async fn run_checkout(
    pool: &sqlx::SqlitePool,
    repository: &Path,
    session: SessionId,
    run: RunId,
) -> anyhow::Result<PathBuf> {
    type LeaseRow = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row: Option<LeaseRow> = sqlx::query_as(
        "SELECT r.session_id, r.workspace_lease_id, l.repository_path, l.worktree_path, l.owner_run_id \
         FROM runs r LEFT JOIN workspace_leases l ON l.id = r.workspace_lease_id WHERE r.id = ?",
    ).bind(run.to_string()).fetch_optional(pool).await?;
    let (owner_session, lease, lease_repo, worktree, owner_run) =
        row.context("The reviewed run is no longer available")?;
    if owner_session != session.to_string() {
        bail!("The reviewed run belongs to a different session");
    }
    let root = repository_root(repository)?;
    if lease.is_none() {
        return Ok(root);
    }
    if owner_run.as_deref() != Some(run.to_string().as_str())
        || repository_root(Path::new(
            &lease_repo.context("The run's workspace lease is no longer available")?,
        ))? != root
    {
        bail!("The run's workspace lease does not belong to this repository");
    }
    let checkout = PathBuf::from(worktree.context("The run's worktree is no longer available")?)
        .canonicalize()
        .context("The run's worktree is no longer available")?;
    if !checkout.is_dir() {
        bail!("The run's worktree is no longer available");
    }
    Ok(checkout)
}

pub(crate) async fn open_reviewed_source(
    paths: &RuntimePaths,
    session: SessionId,
    run: RunId,
    repository: &Path,
    file: &str,
    line: u32,
) -> anyhow::Result<String> {
    let editor = preferred_editor()?;
    let pool =
        codypendent_knowledge::db::open_read_only(&paths.data_dir.join("codypendent.db")).await?;
    let checkout = run_checkout(&pool, repository, session, run).await;
    pool.close().await;
    let checkout = checkout?;
    let root = repository_root(repository)?;
    let uri = handoff_uri(editor, paths, session, &root, &checkout, Some((file, line)))?;
    let mut command = tokio::process::Command::from(command(editor, &uri));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(Duration::from_secs(10), command.status())
        .await
        .context("The editor launcher did not respond within 10 seconds")??;
    if !status.success() {
        bail!("The editor launcher failed; open the editor and try again");
    }
    Ok(format!(
        "Opened {file}:{line} in {} · same session",
        editor.name
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_handoff_preserves_exact_session_checkout_and_line() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let checkout = root.join("run checkout");
        std::fs::create_dir(&checkout).unwrap();
        std::fs::write(checkout.join("a & b.rs"), "hello\n").unwrap();
        let paths = RuntimePaths::from_data_dir(root.join("data"));
        let session = SessionId::new();
        let uri = handoff_uri(
            EDITORS[0],
            &paths,
            session,
            &root,
            &checkout,
            Some(("a & b.rs", 42)),
        )
        .unwrap();
        let uri = reqwest::Url::parse(&uri).unwrap();
        let pairs: std::collections::HashMap<_, _> = uri.query_pairs().into_owned().collect();
        assert_eq!(uri.scheme(), "vscode");
        assert_eq!(pairs.len(), 6);
        assert_eq!(pairs["session"], session.to_string());
        assert_eq!(pairs["repository"], root.to_str().unwrap());
        assert_eq!(pairs["checkout"], checkout.to_str().unwrap());
        assert_eq!(pairs["socket"], paths.socket_path.to_str().unwrap());
        assert_eq!(pairs["file"], "a & b.rs");
        assert_eq!(pairs["line"], "42");
        assert!(handoff_uri(
            EDITORS[0],
            &paths,
            session,
            &root,
            &checkout,
            Some(("a & b.rs", 0))
        )
        .is_err());
        for file in ["../outside", "/etc/passwd", "a\\b", "a\nb", "missing"] {
            assert!(workspace_file(&checkout, file).is_err(), "{file}");
        }
        #[cfg(unix)]
        {
            std::fs::write(root.join("outside"), "outside").unwrap();
            std::os::unix::fs::symlink(root.join("outside"), checkout.join("escape")).unwrap();
            assert!(workspace_file(&checkout, "escape").is_err());
        }
    }

    #[tokio::test]
    async fn editor_handoff_uses_owned_lease_and_refuses_retired_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert!(Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success());
        let nested = root.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        let checkout = root.join("run");
        std::fs::create_dir(&checkout).unwrap();
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE runs (id TEXT, session_id TEXT, workspace_lease_id TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE workspace_leases (id TEXT, repository_path TEXT, worktree_path TEXT, owner_run_id TEXT)").execute(&pool).await.unwrap();
        let session = SessionId::new();
        let run = RunId::new();
        sqlx::query("INSERT INTO runs VALUES (?, ?, 'lease')")
            .bind(run.to_string())
            .bind(session.to_string())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO workspace_leases VALUES ('lease', ?, ?, ?)")
            .bind(nested.to_str().unwrap())
            .bind(checkout.to_str().unwrap())
            .bind(run.to_string())
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            run_checkout(&pool, &root, session, run).await.unwrap(),
            checkout
        );
        assert_eq!(
            run_checkout(&pool, &nested, session, run).await.unwrap(),
            checkout
        );
        let paths = RuntimePaths::from_data_dir(root.join("data"));
        let command = session_command(&paths, session, "code", &nested).unwrap();
        let uri =
            reqwest::Url::parse(command.get_args().last().unwrap().to_str().unwrap()).unwrap();
        let pairs: std::collections::HashMap<_, _> = uri.query_pairs().into_owned().collect();
        assert_eq!(pairs["repository"], root.to_str().unwrap());
        assert_eq!(pairs["checkout"], root.to_str().unwrap());
        assert!(run_checkout(&pool, &root, SessionId::new(), run)
            .await
            .is_err());
        let other = tempfile::tempdir().unwrap();
        assert!(run_checkout(&pool, other.path(), session, run)
            .await
            .is_err());
        std::fs::remove_dir(&checkout).unwrap();
        assert!(run_checkout(&pool, &root, session, run)
            .await
            .unwrap_err()
            .to_string()
            .contains("worktree"));
        sqlx::query("DELETE FROM workspace_leases")
            .execute(&pool)
            .await
            .unwrap();
        assert!(run_checkout(&pool, &root, session, run).await.is_err());
        pool.close().await;
    }
}
