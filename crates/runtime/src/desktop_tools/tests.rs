use super::*;

fn host_tool() -> DesktopTool {
    DesktopTool {
        name: "desktop.navigate".into(),
        description: "Open a workspace view".into(),
        parameters: json!({"type":"object","properties":{"view":{"type":"string","enum":["chat","models"]}},"required":["view"],"additionalProperties":false}),
        function_body: None,
        language: None,
        capabilities: Vec::new(),
        version: 0,
    }
}

fn custom_tool() -> Value {
    json!({"name":"desktop.custom.openModels","description":"Open the models view","parameters":{"type":"object","properties":{},"additionalProperties":false},"functionBody":"return await host.call('desktop.navigate', {view:'models'});","capabilities":["desktop.navigate"]})
}

#[test]
fn generated_helper_language_is_optional_and_python_is_persisted() {
    let temporary = tempfile::tempdir().unwrap();
    let tools = DesktopTools::new(temporary.path().join("desktop"));
    let session = SessionId::new();
    tools
        .publish(session, json!({"view":"chat"}), vec![host_tool()])
        .unwrap();
    tools.define(session, &custom_tool()).unwrap();
    assert!(tools
        .generated_definition(session, "desktop.custom.openModels")
        .unwrap()
        .unwrap()
        .language
        .is_none());
    let mut python = custom_tool();
    python["language"] = json!("python");
    python["functionBody"] = json!("return host_call('desktop.navigate', {'view': 'models'})");
    tools.define(session, &python).unwrap();
    let saved = tools
        .generated_definition(session, "desktop.custom.openModels")
        .unwrap()
        .unwrap();
    assert_eq!(saved.language.as_deref(), Some("python"));
    assert_eq!(saved.version, 2);
    python["language"] = json!("shell");
    assert!(tools
        .define(session, &python)
        .unwrap_err()
        .contains("invalid_language"));
}

#[tokio::test]
async fn native_generated_helpers_require_a_claimed_current_approved_definition() {
    let temporary = tempfile::tempdir().unwrap();
    let tools = DesktopTools::new(temporary.path().join("desktop"));
    let session = SessionId::new();
    tools
        .publish(session, json!({"view":"chat"}), vec![host_tool()])
        .unwrap();
    tools.define(session, &custom_tool()).unwrap();
    let agent = tools.clone();
    let execution = tokio::spawn(async move {
        agent
            .invoke(
                session,
                RunId::new(),
                "desktop.custom.openModels",
                json!({}),
            )
            .await
    });
    let call = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(call) = tools.pending(session).unwrap() {
                break call;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        tools
            .pending_generated_call(session, &call.call_id)
            .unwrap()
            .definition
            .version,
        1
    );
    assert!(tools
        .pending_generated_call(SessionId::new(), &call.call_id)
        .is_err());
    tools.define(session, &custom_tool()).unwrap();
    assert!(tools
        .pending_generated_call(session, &call.call_id)
        .unwrap_err()
        .contains("version_conflict"));
    execution.abort();
    let _ = execution.await;
}

#[test]
fn desktop_execute_entry_point_only_resolves_saved_generated_tools() {
    let temporary = tempfile::tempdir().unwrap();
    let tools = DesktopTools::new(temporary.path().join("desktop"));
    let session = SessionId::new();
    tools
        .publish(session, json!({"view":"chat"}), vec![host_tool()])
        .unwrap();
    tools.define(session, &custom_tool()).unwrap();
    assert_eq!(
        tools
            .execution_call(
                session,
                &json!({"name":"desktop.custom.openModels","args":{}})
            )
            .unwrap(),
        ("desktop.custom.openModels".to_string(), json!({}))
    );
    for name in ["desktop.navigate", DEFINE, EXECUTE, "desktop.custom.absent"] {
        assert!(tools
            .execution_call(session, &json!({"name":name,"args":{}}))
            .is_err());
    }
}

#[tokio::test]
async fn desktop_call_results_are_bound_once_to_the_request_and_cleaned_up() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let agent = DesktopTools::new(temporary.path().join("desktop"));
    let host = agent.clone();
    let session = SessionId::new();
    let run = RunId::new();
    host.publish(session, json!({"view":"chat"}), vec![host_tool()])
        .expect("publish");
    let execution = tokio::spawn(async move {
        agent
            .invoke(session, run, "desktop.navigate", json!({"view":"models"}))
            .await
    });
    let request = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(request) = host.pending(session).expect("pending") {
                break request;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("request arrived");
    assert_eq!(request.session_id, session);
    assert_eq!(request.run_id, run);
    assert!(DesktopTools::new(host.root.clone())
        .pending(session)
        .expect("second window poll")
        .is_none());
    assert!(host
        .resolve_call(SessionId::new(), &request.call_id, Some(json!({})), None)
        .is_err());
    host.resolve_call(
        session,
        &request.call_id,
        Some(json!({"view":"models"})),
        None,
    )
    .expect("resolve");
    assert!(host
        .resolve_call(
            session,
            &request.call_id,
            Some(json!({"view":"chat"})),
            None
        )
        .is_err());
    assert_eq!(
        execution.await.expect("agent task").expect("tool result"),
        json!({"view":"models"})
    );
    assert!(host.pending(session).expect("pending").is_none());
    assert!(fs::read_dir(host.directory(session))
        .expect("records")
        .all(|entry| !entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .starts_with("request-")));
}

#[test]
fn generated_tools_reload_with_versions_and_cannot_expand_host_capabilities() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let tools = DesktopTools::new(temporary.path().join("desktop"));
    let session = SessionId::new();
    tools
        .publish(
            session,
            json!({"view":"chat","token":"secret","repository":{"apiKey":"secret"}}),
            vec![host_tool()],
        )
        .expect("publish");
    assert!(!tools
        .context_text(session)
        .expect("context")
        .contains("secret"));
    assert_eq!(
        tools.define(session, &custom_tool()).expect("define")["version"],
        1
    );
    let reload = DesktopTools::new(tools.root.clone());
    assert!(reload
        .definitions(session)
        .iter()
        .any(|tool| tool.name == "desktop.custom.openModels"));
    assert_eq!(
        reload.define(session, &custom_tool()).expect("update")["version"],
        2
    );
    assert!(reload
        .remove(session, "desktop.custom.openModels", 1)
        .is_err());
    let mut denied = custom_tool();
    denied["capabilities"] = json!(["shell.run"]);
    assert!(reload
        .define(session, &denied)
        .expect_err("denied capability")
        .contains("capability_denied"));
    assert!(reload
        .validate_call(session, "desktop.navigate", &json!({"view":"secrets"}))
        .is_err());
    reload
        .remove(session, "desktop.custom.openModels", 2)
        .expect("delete latest");
    assert!(reload.list(session).expect("saved tools").is_empty());
}

#[test]
fn stale_desktop_leases_and_unsupported_schemas_fail_closed() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let tools = DesktopTools::new(temporary.path().join("desktop"));
    let session = SessionId::new();
    assert!(tools.definitions(session).is_empty());
    tools
        .publish(session, json!({"view":"chat"}), vec![host_tool()])
        .expect("publish");
    let mut unsupported = custom_tool();
    unsupported["parameters"]["$ref"] = json!("file:///etc/passwd");
    assert!(tools.define(session, &unsupported).is_err());
    for schema in [
        json!({"type":"object","required":"name"}),
        json!({"type":"object","enum":42}),
        json!({"type":"object","properties":{"n":{"type":"integer","minimum":"zero"}}}),
        json!({"type":"object","properties":{"s":{"type":"string","minLength":8,"maxLength":2}}}),
        json!({"type":"object","required":["absent"],"properties":{}}),
        json!({"type":"object","description":5}),
        json!({"type":"object","properties":{"values":{"type":"array","items":"string"}}}),
    ] {
        let mut invalid = custom_tool();
        invalid["parameters"] = schema;
        assert!(tools.define(session, &invalid).is_err());
    }
    tools
        .write(
            &tools.directory(session).join("context.json"),
            &DesktopContext {
                updated_at: 0,
                context: json!({}),
                tools: vec![host_tool()],
            },
            false,
        )
        .expect("expire lease");
    assert!(tools.definitions(session).is_empty());
    assert!(tools
        .validate_call(session, "desktop.navigate", &json!({"view":"chat"}))
        .expect_err("expired")
        .contains("host_offline"));
}

#[tokio::test]
async fn changed_generated_source_cannot_execute_with_an_older_approval() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let tools = DesktopTools::new(temporary.path().join("desktop"));
    let session = SessionId::new();
    tools
        .publish(session, json!({}), vec![host_tool()])
        .expect("publish");
    tools.define(session, &custom_tool()).expect("define");
    let approved = tools
        .generated_definition(session, "desktop.custom.openModels")
        .expect("registry")
        .expect("tool");
    let mut changed = custom_tool();
    changed["functionBody"] = json!("return 'different action';");
    tools.define(session, &changed).expect("update");
    assert!(tools
        .invoke_checked(
            session,
            RunId::new(),
            &approved.name,
            json!({}),
            Some(&approved)
        )
        .await
        .expect_err("source changed")
        .contains("version_conflict"));
    assert!(tools.pending(session).expect("pending").is_none());
}

#[tokio::test]
async fn cancelling_an_inflight_desktop_call_removes_its_pending_request() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let tools = DesktopTools::new(temporary.path().join("desktop"));
    let session = SessionId::new();
    tools
        .publish(session, json!({}), vec![host_tool()])
        .expect("publish");
    let agent = tools.clone();
    let execution = tokio::spawn(async move {
        agent
            .invoke(
                session,
                RunId::new(),
                "desktop.navigate",
                json!({"view":"chat"}),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while tools.pending(session).expect("pending").is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("request arrived");
    execution.abort();
    let _ = execution.await;
    assert!(tools.pending(session).expect("pending").is_none());
}
