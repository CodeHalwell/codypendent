//! The standalone desktop always owns Codypendent's profile, even when another
//! application's agent runtime launched it with an inherited host selector.

use codypendent_protocol::discovery::{DiscoveryError, RuntimePaths, RuntimeProduct};

pub(crate) fn resolve() -> Result<RuntimePaths, DiscoveryError> {
    RuntimePaths::resolve_for(RuntimeProduct::Codypendent)
}

#[cfg(any(feature = "app", test))]
pub(crate) fn desktop_tools() -> Result<codypendent_runtime::desktop_tools::DesktopTools, String> {
    resolve()
        .map(|paths| {
            codypendent_runtime::desktop_tools::DesktopTools::new(
                paths.data_dir.join("desktop-tools"),
            )
        })
        .map_err(|error| error.to_string())
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

    #[tokio::test]
    async fn desktop_host_tools_share_codypendent_storage_with_inherited_son_profile() {
        use codypendent_protocol::{RunId, SessionId};
        use codypendent_runtime::desktop_tools::{DesktopTool, DesktopTools};
        use serde_json::json;

        const CHILD: &str = "CODYPENDENT_DESKTOP_TOOLS_PROFILE_TEST_CHILD";
        const TEST: &str = "runtime_paths::tests::desktop_host_tools_share_codypendent_storage_with_inherited_son_profile";
        if std::env::var(CHILD).as_deref() != Ok("1") {
            let temporary = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST])
                .env(CHILD, "1")
                .env("AGENT_RUNTIME_PRODUCT", "son-of-anton")
                .env("CODYPENDENT_DATA_DIR", temporary.path().join("cody/data"))
                .env(
                    "CODYPENDENT_CONFIG_DIR",
                    temporary.path().join("cody/config"),
                )
                .env("CODYPENDENT_SOCKET", temporary.path().join("c.sock"))
                .env("SON_OF_ANTON_DATA_DIR", temporary.path().join("anton/data"))
                .env(
                    "SON_OF_ANTON_CONFIG_DIR",
                    temporary.path().join("anton/config"),
                )
                .env("SON_OF_ANTON_SOCKET", temporary.path().join("a.sock"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }

        let session = SessionId::new();
        let paths = resolve().unwrap();
        paths.ensure_directories().unwrap();
        let daemon = DesktopTools::new(paths.data_dir.join("desktop-tools"));
        desktop_tools()
            .unwrap()
            .publish(
                session,
                json!({"view":"chat"}),
                vec![DesktopTool {
                    name: "desktop.navigate".into(),
                    description: "Open a workspace view".into(),
                    parameters: json!({"type":"object"}),
                    function_body: None,
                    language: None,
                    capabilities: vec![],
                    version: 0,
                }],
            )
            .unwrap();
        assert!(daemon.context_text(session).unwrap().contains("chat"));
        daemon
            .define(
                session,
                &json!({"name":"desktop.custom.openModels","description":"Open models",
                    "parameters":{"type":"object"},"functionBody":"return host_call('desktop.navigate', {})",
                    "language":"python","capabilities":["desktop.navigate"]}),
            )
            .unwrap();
        assert_eq!(desktop_tools().unwrap().list(session).unwrap().len(), 1);
        let execution = tokio::spawn(async move {
            daemon
                .invoke(
                    session,
                    RunId::new(),
                    "desktop.custom.openModels",
                    json!({}),
                )
                .await
        });
        let call = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(call) = desktop_tools().unwrap().pending(session).unwrap() {
                    break call;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let helper = desktop_tools()
            .unwrap()
            .pending_generated_call(session, &call.call_id)
            .unwrap();
        assert_eq!(helper.definition.language.as_deref(), Some("python"));
        desktop_tools()
            .unwrap()
            .resolve_call(session, &call.call_id, Some(json!({"opened":true})), None)
            .unwrap();
        assert_eq!(execution.await.unwrap().unwrap(), json!({"opened":true}));
        desktop_tools()
            .unwrap()
            .remove(session, &helper.name, helper.definition.version)
            .unwrap();
        assert!(desktop_tools().unwrap().list(session).unwrap().is_empty());
        assert!(DesktopTools::resolve()
            .unwrap()
            .context_text(session)
            .is_none());
        assert!(!RuntimePaths::resolve()
            .unwrap()
            .data_dir
            .join("desktop-tools")
            .exists());
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
