//! Explicit, local hand-offs to installed development tools. Paths are taken
//! from the validated repository selection; no command or shell fragment comes
//! from the webview.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceTarget {
    pub id: &'static str,
    pub label: &'static str,
    pub kind: &'static str,
}

struct Target {
    info: WorkspaceTarget,
    app: &'static str,
    binary: &'static str,
}

const EDITORS: &[Target] = &[
    Target {
        info: WorkspaceTarget {
            id: "vscode",
            label: "Visual Studio Code",
            kind: "editor",
        },
        app: "Visual Studio Code.app",
        binary: "code",
    },
    Target {
        info: WorkspaceTarget {
            id: "vscode-insiders",
            label: "VS Code Insiders",
            kind: "editor",
        },
        app: "Visual Studio Code - Insiders.app",
        binary: "code-insiders",
    },
    Target {
        info: WorkspaceTarget {
            id: "cursor",
            label: "Cursor",
            kind: "editor",
        },
        app: "Cursor.app",
        binary: "cursor",
    },
    Target {
        info: WorkspaceTarget {
            id: "zed",
            label: "Zed",
            kind: "editor",
        },
        app: "Zed.app",
        binary: "zed",
    },
    Target {
        info: WorkspaceTarget {
            id: "windsurf",
            label: "Windsurf",
            kind: "editor",
        },
        app: "Windsurf.app",
        binary: "windsurf",
    },
    Target {
        info: WorkspaceTarget {
            id: "rustrover",
            label: "RustRover",
            kind: "editor",
        },
        app: "RustRover.app",
        binary: "rustrover",
    },
    Target {
        info: WorkspaceTarget {
            id: "antigravity",
            label: "Antigravity IDE",
            kind: "editor",
        },
        app: "Antigravity IDE.app",
        binary: "antigravity",
    },
];

fn executable(binary: &str) -> Option<PathBuf> {
    let mut directories: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|dir| dir.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    directories.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    directories
        .into_iter()
        .map(|dir| dir.join(binary))
        .find(|path| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

fn app_path(app: &str) -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let system = PathBuf::from("/Applications").join(app);
    if system.is_dir() {
        return Some(system);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Applications").join(app))
        .filter(|path| path.is_dir())
}

pub fn targets() -> Vec<WorkspaceTarget> {
    let mut targets: Vec<_> = EDITORS
        .iter()
        .filter(|target| app_path(target.app).is_some() || executable(target.binary).is_some())
        .map(|target| target.info.clone())
        .collect();
    if cfg!(target_os = "macos") {
        targets.push(WorkspaceTarget {
            id: "finder",
            label: "Finder",
            kind: "files",
        });
        targets.push(WorkspaceTarget {
            id: "terminal",
            label: "Terminal",
            kind: "terminal",
        });
    } else if executable("xdg-open").is_some() {
        targets.push(WorkspaceTarget {
            id: "files",
            label: "File Manager",
            kind: "files",
        });
    }
    targets
}

fn open_command(target: &str, root: &Path) -> anyhow::Result<Command> {
    if !root.is_absolute() || !root.is_dir() {
        bail!("The selected repository is no longer available");
    }
    if cfg!(target_os = "macos") && matches!(target, "finder" | "terminal") {
        let mut command = Command::new("/usr/bin/open");
        if target == "terminal" {
            command.args(["-b", "com.apple.Terminal"]);
        }
        command.arg(root);
        return Ok(command);
    }
    if !cfg!(target_os = "macos") && target == "files" {
        let mut command =
            Command::new(executable("xdg-open").context("No file manager is installed")?);
        command.arg(root);
        return Ok(command);
    }
    let editor = EDITORS
        .iter()
        .find(|editor| editor.info.id == target)
        .context("Unknown workspace application")?;
    if let Some(app) = app_path(editor.app) {
        let mut command = Command::new("/usr/bin/open");
        command.arg("-a").arg(app).arg(root);
        return Ok(command);
    }
    let mut command = Command::new(
        executable(editor.binary)
            .with_context(|| format!("{} is no longer installed", editor.info.label))?,
    );
    command.arg(root);
    Ok(command)
}

fn workspace_file(root: &Path, file: &str) -> anyhow::Result<PathBuf> {
    use std::path::Component;
    let requested = Path::new(file);
    if file.is_empty()
        || file.contains(['\0', '\r', '\n', '\\'])
        || requested
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::RootDir))
    {
        bail!("Choose a file inside the selected repository");
    }
    let root = root.canonicalize()?;
    let absolute = root
        .join(requested)
        .canonicalize()
        .with_context(|| format!("The file {file} is no longer available in this checkout"))?;
    if !absolute.starts_with(&root) || !absolute.is_file() {
        bail!("The file is outside the selected repository or is not a regular file");
    }
    Ok(absolute)
}

fn handoff_uri(
    target: &str,
    root: &Path,
    checkout: &Path,
    session: codypendent_protocol::SessionId,
    socket: &Path,
    file: Option<&str>,
    line: Option<u32>,
) -> anyhow::Result<String> {
    let scheme = match target {
        "vscode" => "vscode",
        "vscode-insiders" => "vscode-insiders",
        "cursor" => "cursor",
        _ => bail!("This editor does not support Codypendent session hand-off"),
    };
    let mut uri = tauri::Url::parse(&format!("{scheme}://codypendent.codypendent/attach"))?;
    {
        let mut query = uri.query_pairs_mut();
        query.append_pair("session", &session.to_string());
        query.append_pair(
            "repository",
            root.to_str().context("Repository path is not UTF-8")?,
        );
        query.append_pair(
            "checkout",
            checkout.to_str().context("Checkout path is not UTF-8")?,
        );
        query.append_pair(
            "socket",
            socket.to_str().context("Socket path is not UTF-8")?,
        );
        if let Some(file) = file {
            query.append_pair("file", file);
        }
        if let Some(line) = line {
            query.append_pair("line", &line.to_string());
        }
    }
    Ok(uri.into())
}

fn checkout_relative_file(checkout: &Path, file: &Path) -> anyhow::Result<String> {
    Ok(file
        .strip_prefix(checkout.canonicalize()?)?
        .to_str()
        .context("Source path is not UTF-8")?
        .to_owned())
}

fn editor_command(
    target: &str,
    root: &Path,
    checkout: &Path,
    session: Option<codypendent_protocol::SessionId>,
    socket: &Path,
    file: Option<&str>,
    line: Option<u32>,
) -> anyhow::Result<Command> {
    if line == Some(0) || (line.is_some() && file.is_none()) {
        bail!("A file location needs a positive line number");
    }
    let absolute = file
        .map(|file| workspace_file(checkout, file))
        .transpose()?;
    if let Some(session) =
        session.filter(|_| matches!(target, "vscode" | "vscode-insiders" | "cursor"))
    {
        let relative = absolute
            .as_deref()
            .map(|file| checkout_relative_file(checkout, file))
            .transpose()?;
        let uri = handoff_uri(
            target,
            root,
            checkout,
            session,
            socket,
            relative.as_deref(),
            line,
        )?;
        let editor = EDITORS
            .iter()
            .find(|editor| editor.info.id == target)
            .unwrap();
        if let Some(app) = app_path(editor.app) {
            let mut command = Command::new("/usr/bin/open");
            command.arg("-a").arg(app).arg(uri);
            return Ok(command);
        }
        let mut command =
            Command::new(executable(editor.binary).context("The editor is no longer installed")?);
        command.arg("--open-url").arg(uri);
        return Ok(command);
    }
    if let Some(file) = absolute {
        let editor = EDITORS
            .iter()
            .find(|editor| editor.info.id == target)
            .context("Choose an editor to open a source location")?;
        let mut command = if let Some(binary) = executable(editor.binary) {
            Command::new(binary)
        } else if let Some(app) = app_path(editor.app) {
            let mut command = Command::new("/usr/bin/open");
            command.arg("-a").arg(app).arg("--args");
            command
        } else {
            bail!("The editor is no longer installed");
        };
        if matches!(
            target,
            "vscode" | "vscode-insiders" | "cursor" | "windsurf" | "antigravity"
        ) {
            command.arg("--goto");
        } else if target == "rustrover" {
            command
                .arg("--line")
                .arg(line.unwrap_or(1).to_string())
                .arg(&file);
            return Ok(command);
        }
        command.arg(format!("{}:{}", file.display(), line.unwrap_or(1)));
        return Ok(command);
    }
    open_command(target, checkout)
}

/// Follow the daemon's durable run-to-lease link; never derive a worktree name.
pub async fn run_checkout(
    repository: &str,
    session: codypendent_protocol::SessionId,
    run: codypendent_protocol::RunId,
) -> anyhow::Result<PathBuf> {
    let paths = crate::runtime_paths::resolve()?;
    let pool =
        codypendent_knowledge::db::open_read_only(&paths.data_dir.join("codypendent.db")).await?;
    let result = run_checkout_in(&pool, repository, session, run).await;
    pool.close().await;
    result
}

async fn run_checkout_in(
    pool: &sqlx::SqlitePool,
    repository: &str,
    session: codypendent_protocol::SessionId,
    run: codypendent_protocol::RunId,
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
    let root = Path::new(repository).canonicalize()?;
    if lease.is_none() {
        return Ok(root);
    } // A read-only run has no isolated lease.
    if owner_run.as_deref() != Some(run.to_string().as_str())
        || Path::new(&lease_repo.context("The run's workspace lease is no longer available")?)
            .canonicalize()?
            != root
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

pub fn open_workspace(
    target: &str,
    expected_repository: &str,
    checkout: Option<&Path>,
    session: Option<codypendent_protocol::SessionId>,
    socket: &Path,
    file: Option<&str>,
    line: Option<u32>,
) -> anyhow::Result<()> {
    let selected =
        crate::repository::selected_repository()?.context("Choose a repository first")?;
    if selected.path != expected_repository {
        bail!("The selected repository changed in another window. Choose the repository again before opening it.");
    }
    let mut command = editor_command(
        target,
        Path::new(&selected.path),
        checkout.unwrap_or_else(|| Path::new(&selected.path)),
        session,
        socket,
        file,
        line,
    )?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // macOS's `open` exits as soon as LaunchServices accepts the hand-off.
    // Do not block the webview or a Tokio reactor while waiting for it.
    let mut child = command
        .spawn()
        .context("Opening the workspace application")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("The workspace application could not be opened ({status})");
            }
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            // Some editor launchers remain attached to their GUI. Reap those in
            // a background thread without killing the editor the user opened.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_commands_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        assert!(open_command("sh -c touch /tmp/unwanted", root.path()).is_err());
        assert!(open_command("https://example.com", root.path()).is_err());
    }

    #[test]
    fn source_locations_cannot_escape_the_checkout() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file.rs"), "fn main() {}\n").unwrap();
        std::fs::write(parent.path().join("outside"), "outside").unwrap();
        std::os::unix::fs::symlink(parent.path().join("outside"), root.join("escape")).unwrap();
        assert!(workspace_file(&root, "file.rs")
            .unwrap()
            .ends_with("file.rs"));
        assert_eq!(
            workspace_file(&root, root.join("file.rs").to_str().unwrap()).unwrap(),
            root.join("file.rs").canonicalize().unwrap()
        );
        for file in [
            "../outside",
            "/etc/passwd",
            "escape",
            "missing",
            "bad\nfile",
        ] {
            assert!(workspace_file(&root, file).is_err(), "{file}");
        }
    }

    #[test]
    fn absolute_source_locations_use_only_the_authorised_checkout() {
        let parent = tempfile::tempdir().unwrap();
        let checkout = parent.path().join("run-authorised");
        let other = parent.path().join("run-other");
        for root in [&checkout, &other] {
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(root.join("src/agent.py"), "def create_agent(): pass\n").unwrap();
        }
        std::os::unix::fs::symlink(other.join("src/agent.py"), checkout.join("escape.py")).unwrap();
        for file in [
            other.join("src/agent.py"),
            checkout.join("escape.py"),
            checkout.join("../run-other/src/agent.py"),
            checkout.join("src"),
            checkout.join("missing.py"),
        ] {
            assert!(workspace_file(&checkout, file.to_str().unwrap()).is_err());
        }
        let absolute =
            workspace_file(&checkout, checkout.join("src/agent.py").to_str().unwrap()).unwrap();
        let relative = checkout_relative_file(&checkout, &absolute).unwrap();
        assert_eq!(relative, "src/agent.py");
        let uri = handoff_uri(
            "cursor",
            parent.path(),
            &checkout,
            codypendent_protocol::SessionId::new(),
            &parent.path().join("d.sock"),
            Some(&relative),
            Some(68),
        )
        .unwrap();
        let uri = tauri::Url::parse(&uri).unwrap();
        let fields: std::collections::HashMap<_, _> = uri.query_pairs().collect();
        assert_eq!(fields["file"], "src/agent.py");
        assert_eq!(fields["line"], "68");
    }

    #[test]
    fn session_uri_encodes_paths_and_an_exact_line_without_secrets() {
        let session = codypendent_protocol::SessionId::new();
        let uri = handoff_uri(
            "cursor",
            Path::new("/tmp/repo & café"),
            Path::new("/tmp/run"),
            session,
            Path::new("/tmp/d.sock"),
            Some("src/hello world.rs"),
            Some(42),
        )
        .unwrap();
        let uri = tauri::Url::parse(&uri).unwrap();
        let fields: std::collections::HashMap<_, _> = uri.query_pairs().collect();
        assert_eq!(uri.scheme(), "cursor");
        assert_eq!(fields["session"], session.to_string());
        assert_eq!(fields["repository"], "/tmp/repo & café");
        assert_eq!(fields["checkout"], "/tmp/run");
        assert_eq!(fields["file"], "src/hello world.rs");
        assert_eq!(fields["line"], "42");
        assert_eq!(fields.len(), 6);
    }

    #[tokio::test]
    async fn reviewed_run_uses_its_own_worktree_and_rejects_other_sessions() {
        let root = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE runs (id TEXT, session_id TEXT, workspace_lease_id TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE workspace_leases (id TEXT, repository_path TEXT, worktree_path TEXT, owner_run_id TEXT)").execute(&pool).await.unwrap();
        let session = codypendent_protocol::SessionId::new();
        let run = codypendent_protocol::RunId::new();
        sqlx::query("INSERT INTO runs VALUES (?, ?, 'lease')")
            .bind(run.to_string())
            .bind(session.to_string())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO workspace_leases VALUES ('lease', ?, ?, ?)")
            .bind(root.path().to_string_lossy().as_ref())
            .bind(worktree.path().to_string_lossy().as_ref())
            .bind(run.to_string())
            .execute(&pool)
            .await
            .unwrap();
        let checkout = run_checkout_in(&pool, root.path().to_str().unwrap(), session, run)
            .await
            .unwrap();
        assert_eq!(checkout, worktree.path().canonicalize().unwrap());
        assert!(run_checkout_in(
            &pool,
            root.path().to_str().unwrap(),
            codypendent_protocol::SessionId::new(),
            run
        )
        .await
        .is_err());
        sqlx::query("DELETE FROM workspace_leases")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            run_checkout_in(&pool, root.path().to_str().unwrap(), session, run)
                .await
                .is_err()
        );
        pool.close().await;
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn repository_names_are_one_literal_argument() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("project $(touch nope); 'quoted'");
        std::fs::create_dir(&root).unwrap();
        let command = open_command("terminal", &root).unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(command.get_program(), "/usr/bin/open");
        assert_eq!(
            args,
            [
                std::ffi::OsStr::new("-b"),
                std::ffi::OsStr::new("com.apple.Terminal"),
                root.as_os_str()
            ]
        );
    }
}
