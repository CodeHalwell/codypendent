//! Keep Google account storage while scoping MCP servers to the current host.

use std::io;
use std::path::Path;

pub(crate) struct GoogleHome {
    directory: tempfile::TempDir,
}

impl GoogleHome {
    pub(crate) fn prepare(source: &Path) -> io::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("codypendent-antigravity-")
            .tempdir()?;
        let root = directory.path();
        let auth = source.join("antigravity-acp");
        if !auth.exists() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&auth)?;
        }
        // Vendor refresh uses an atomic rename in this directory. Linking its
        // parent preserves that write at the original account-store location.
        link(&auth, &root.join("antigravity-acp"))?;
        let config = root.join("config");
        std::fs::create_dir(&config)?;
        let original = source.join("config");
        if original.is_dir() {
            for entry in std::fs::read_dir(original)?.take(256) {
                let entry = entry?;
                if entry.file_name() != "mcp_config.json" {
                    link(&entry.path(), &config.join(entry.file_name()))?;
                }
            }
        }
        for name in ["GEMINI.md", "AGENTS.md", "rules", "skills"] {
            let path = source.join(name);
            if path.exists() {
                link(&path, &root.join(name))?;
            }
        }
        Ok(Self { directory })
    }

    pub(crate) fn path(&self) -> &Path {
        self.directory.path()
    }
}

fn link(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, target)
    }
    #[cfg(windows)]
    {
        if source.is_dir() {
            std::os::windows::fs::symlink_dir(source, target)
        } else {
            std::os::windows::fs::symlink_file(source, target)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (source, target);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "isolated Google profiles require filesystem links",
        ))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn google_home_scopes_mcp_and_preserves_atomic_vendor_refresh() {
        let original = tempfile::tempdir().unwrap();
        let source = original.path();
        std::fs::create_dir_all(source.join("antigravity-acp")).unwrap();
        std::fs::write(
            source.join("antigravity-acp/acp_token.json"),
            "old-sentinel",
        )
        .unwrap();
        std::fs::create_dir(source.join("config")).unwrap();
        std::fs::write(source.join("config/config.json"), "public-settings").unwrap();
        std::fs::write(source.join("config/mcp_config.json"), "old-ide-server").unwrap();
        let home = GoogleHome::prepare(source).unwrap();
        assert!(!home.path().join("config/mcp_config.json").exists());
        assert_eq!(
            std::fs::read_to_string(home.path().join("config/config.json")).unwrap(),
            "public-settings"
        );
        let temporary = home.path().join("antigravity-acp/refreshed.tmp");
        std::fs::write(&temporary, "new-sentinel").unwrap();
        std::fs::rename(
            temporary,
            home.path().join("antigravity-acp/acp_token.json"),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(source.join("antigravity-acp/acp_token.json")).unwrap(),
            "new-sentinel"
        );
        let isolated = home.path().to_path_buf();
        drop(home);
        assert!(!isolated.exists());
        assert!(source.join("antigravity-acp/acp_token.json").exists());
        assert_eq!(
            std::fs::read_to_string(source.join("config/mcp_config.json")).unwrap(),
            "old-ide-server"
        );
    }
}
