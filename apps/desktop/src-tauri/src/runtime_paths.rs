//! The standalone desktop always owns Codypendent's profile, even when another
//! application's agent runtime launched it with an inherited host selector.

use codypendent_protocol::discovery::{DiscoveryError, RuntimePaths, RuntimeProduct};

pub(crate) fn resolve() -> Result<RuntimePaths, DiscoveryError> {
    RuntimePaths::resolve_for(RuntimeProduct::Codypendent)
}

/// Launch the daemon with the same paths the desktop reads and connects to.
pub(crate) fn configure_command(command: &mut std::process::Command, paths: &RuntimePaths) {
    command
        .env("AGENT_RUNTIME_PRODUCT", "codypendent")
        .env("CODYPENDENT_DATA_DIR", &paths.data_dir)
        .env("CODYPENDENT_CONFIG_DIR", &paths.config_dir)
        .env("CODYPENDENT_SOCKET", &paths.socket_path);
    for suffix in ["DATA_DIR", "CONFIG_DIR", "SOCKET"] {
        command.env_remove(format!("SON_OF_ANTON_{suffix}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn desktop_paths_ignore_inherited_son_selector() {
        const CHILD: &str = "CODYPENDENT_DESKTOP_PROFILE_TEST_CHILD";
        if std::env::var(CHILD).as_deref() == Ok("1") {
            let paths = resolve().expect("desktop paths");
            assert_eq!(paths.data_dir, PathBuf::from("/desktop-cody/data"));
            assert_eq!(paths.config_dir, PathBuf::from("/desktop-cody/config"));
            assert_eq!(
                paths.socket_path,
                PathBuf::from("/desktop-cody/daemon.sock")
            );
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime_paths::tests::desktop_paths_ignore_inherited_son_selector",
            ])
            .env(CHILD, "1")
            .env("AGENT_RUNTIME_PRODUCT", "son-of-anton")
            .env("SON_OF_ANTON_DATA_DIR", "/desktop-anton/data")
            .env("SON_OF_ANTON_CONFIG_DIR", "/desktop-anton/config")
            .env("SON_OF_ANTON_SOCKET", "/desktop-anton/daemon.sock")
            .env("CODYPENDENT_DATA_DIR", "/desktop-cody/data")
            .env("CODYPENDENT_CONFIG_DIR", "/desktop-cody/config")
            .env("CODYPENDENT_SOCKET", "/desktop-cody/daemon.sock")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    #[test]
    fn daemon_child_uses_desktop_paths_without_foreign_overrides() {
        let mut paths = RuntimePaths::from_data_dir(PathBuf::from("/desktop-cody/data"));
        paths.config_dir = PathBuf::from("/desktop-cody/config");
        paths.socket_path = PathBuf::from("/desktop-cody/daemon.sock");
        let mut command = std::process::Command::new("fixture-daemon");
        configure_command(&mut command, &paths);
        let environment: BTreeMap<String, Option<OsString>> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(OsString::from),
                )
            })
            .collect();
        assert_eq!(
            environment,
            BTreeMap::from([
                ("AGENT_RUNTIME_PRODUCT".into(), Some("codypendent".into())),
                (
                    "CODYPENDENT_DATA_DIR".into(),
                    Some(paths.data_dir.into_os_string())
                ),
                (
                    "CODYPENDENT_CONFIG_DIR".into(),
                    Some(paths.config_dir.into_os_string())
                ),
                (
                    "CODYPENDENT_SOCKET".into(),
                    Some(paths.socket_path.into_os_string())
                ),
                ("SON_OF_ANTON_DATA_DIR".into(), None),
                ("SON_OF_ANTON_CONFIG_DIR".into(), None),
                ("SON_OF_ANTON_SOCKET".into(), None),
            ])
        );
    }
}
