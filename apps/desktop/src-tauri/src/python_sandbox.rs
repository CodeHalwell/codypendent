use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
    time::Duration,
};

use codypendent_protocol::SessionId;
use monty_pool::{on_print_sync, Pool, PoolConfig, ReplConfig, ResumeValue, TurnEvent};
use monty_types::{MontyObject, NameLookupResult, ResourceLimits};
use serde::Serialize;
use serde_json::Value;
use tauri::ipc::Channel;
use tokio::sync::{oneshot, OnceCell};

const MAX_BYTES: usize = 256 * 1024;
const MAX_CALLS: usize = 32;
const DEADLINE: Duration = Duration::from_secs(5);

static POOL: OnceCell<Pool> = OnceCell::const_new();
static JOBS: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new();

struct Job {
    session: SessionId,
    cancel: Option<oneshot::Sender<()>>,
    reply: Option<(u32, oneshot::Sender<Result<Value, String>>)>,
}

struct JobGuard(String);
impl Drop for JobGuard {
    fn drop(&mut self) {
        if let Ok(mut jobs) = jobs().lock() {
            jobs.remove(&self.0);
        }
    }
}

fn jobs() -> &'static Mutex<HashMap<String, Job>> {
    JOBS.get_or_init(Mutex::default)
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostRequest {
    request_id: u32,
    name: String,
    args: Value,
}

fn worker_path() -> Result<PathBuf, String> {
    let name = if cfg!(windows) { "monty.exe" } else { "monty" };
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let sibling = executable
        .parent()
        .ok_or("The app location is unavailable.")?
        .join(name);
    if sibling.is_file() {
        return Ok(sibling);
    }
    #[cfg(debug_assertions)]
    {
        let development = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target/debug")
            .join(name);
        if development.is_file() {
            return Ok(development);
        }
    }
    Err("The bundled Python sandbox is missing. Reinstall Codypendent to restore it.".into())
}

async fn pool() -> Result<&'static Pool, String> {
    POOL.get_or_try_init(|| async { new_pool(worker_path()?).await })
        .await
}

async fn new_pool(binary: PathBuf) -> Result<Pool, String> {
    let mut config = PoolConfig::subprocess(binary);
    config.min_processes = 0;
    config.max_processes = 2;
    config.checkout_timeout = Some(Duration::from_secs(1));
    config.request_timeout = Some(Duration::from_secs(2));
    config.max_checkouts_per_worker = Some(32);
    Pool::new(config)
        .await
        .map_err(|error| format!("Python sandbox could not start: {error}"))
}

fn json_text(value: &Value) -> Result<String, String> {
    let text = serde_json::to_string(value).map_err(|error| error.to_string())?;
    if text.len() > MAX_BYTES {
        return Err("Python helper data exceeds the 256 KB limit.".into());
    }
    Ok(text)
}

async fn execute<F, Fut>(
    pool: &Pool,
    body: &str,
    args: &Value,
    capabilities: &[String],
    mut host: F,
) -> Result<Value, String>
where
    F: FnMut(String, Value) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    if body.trim().is_empty() || body.len() > 64 * 1024 {
        return Err("Python helper source must contain 1 to 65536 bytes.".into());
    }
    let source = format!(
        "import json\nargs = json.loads(_args_json)\ndef host_call(name, arguments):\n    return json.loads(_cody_host(name, json.dumps(arguments)))\ndef _cody_helper():\n{}\njson.dumps(_cody_helper(), allow_nan=False)",
        body.lines().map(|line| format!("    {line}\n")).collect::<String>()
    );
    let input = json_text(args)?;
    let operation = async {
        let limits = ResourceLimits {
            max_memory: Some(32 * 1024 * 1024),
            max_feed_duration: Some(Duration::from_millis(500)),
            max_turn_duration: Some(Duration::from_millis(250)),
            max_recursion_depth: 64,
            max_suspensions: 64,
            max_total_sleep: Some(Duration::ZERO),
            ..ResourceLimits::default()
        };
        let mut session = pool
            .checkout(&ReplConfig {
                limits: Some(limits),
                ..ReplConfig::default()
            })
            .await
            .map_err(|error| error.to_string())?;
        let printed = AtomicUsize::new(0);
        let mut on_print = on_print_sync(|_, text| {
            printed.fetch_add(text.len(), Ordering::Relaxed);
        });
        let mut event = session
            .feed(
                &source,
                vec![
                    ("_args_json".into(), MontyObject::string(input)),
                    (
                        "_cody_host".into(),
                        MontyObject::function("host_call", None),
                    ),
                ],
                vec![],
                true,
                &mut on_print,
            )
            .await
            .map_err(|error| error.to_string())?;
        let mut calls = 0usize;
        loop {
            if printed.load(Ordering::Relaxed) > MAX_BYTES {
                return Err("Python helper output exceeds the 256 KB limit.".into());
            }
            event = match event {
                TurnEvent::Complete(value) => {
                    let text = value
                        .as_ref()
                        .as_str()
                        .ok_or("Python helper did not return JSON.")?;
                    if text.len() > MAX_BYTES {
                        return Err("Python helper result exceeds the 256 KB limit.".into());
                    }
                    let result: Value = serde_json::from_str(text)
                        .map_err(|error| format!("Python helper returned invalid JSON: {error}"))?;
                    session.finish().await.map_err(|error| error.to_string())?;
                    return Ok(result);
                }
                TurnEvent::FunctionCall {
                    function_name,
                    args,
                    object_id,
                    ..
                } => {
                    calls += 1;
                    if calls > MAX_CALLS
                        || function_name != "host_call"
                        || object_id.is_some()
                        || args.args().len() != 2
                        || args.kwargs().len() != 0
                    {
                        return Err("Python helper exceeded its host-call allowance.".into());
                    }
                    let name = args
                        .arg(0)
                        .and_then(|arg| arg.as_str())
                        .ok_or("Invalid host capability name.")?
                        .to_owned();
                    if !capabilities.contains(&name) {
                        return Err(
                            "This helper has no permission for that workspace capability.".into(),
                        );
                    }
                    let arguments = args
                        .arg(1)
                        .and_then(|arg| arg.as_str())
                        .ok_or("Invalid host capability arguments.")?;
                    if arguments.len() > MAX_BYTES {
                        return Err("Host capability arguments exceed the 256 KB limit.".into());
                    }
                    let arguments: Value =
                        serde_json::from_str(arguments).map_err(|error| error.to_string())?;
                    let result = host(name, arguments).await?;
                    session
                        .resume(
                            ResumeValue::Return(MontyObject::string(json_text(&result)?)),
                            &mut on_print,
                        )
                        .await
                        .map_err(|error| error.to_string())?
                }
                TurnEvent::OsCall { .. } => session
                    .resume(ResumeValue::NotHandled, &mut on_print)
                    .await
                    .map_err(|error| error.to_string())?,
                TurnEvent::NameLookup { .. } => session
                    .resume_name_lookup(NameLookupResult::Undefined, &mut on_print)
                    .await
                    .map_err(|error| error.to_string())?,
                TurnEvent::ResolveFutures { .. } => {
                    return Err("Python helpers cannot create background tasks.".into())
                }
            };
        }
    };
    tokio::time::timeout(DEADLINE, operation)
        .await
        .map_err(|_| "Python helper exceeded its five second limit.".to_owned())?
}

#[tauri::command]
pub async fn desktop_tools_python_execute(
    session_id: SessionId,
    call_id: String,
    on_host: Channel<HostRequest>,
) -> Result<Value, String> {
    let desktop = crate::runtime_paths::desktop_tools()?;
    let call = desktop.pending_generated_call(session_id, &call_id)?;
    if call.definition.language.as_deref() != Some("python") {
        return Err("This helper is not a Python helper.".into());
    }
    let body = call
        .definition
        .function_body
        .as_deref()
        .ok_or("Python helper source is missing.")?;
    let (cancel_tx, cancel_rx) = oneshot::channel();
    {
        let mut active = jobs()
            .lock()
            .map_err(|_| "Python helper state is unavailable.")?;
        if active.contains_key(&call_id) {
            return Err("This Python helper is already running.".into());
        }
        active.insert(
            call_id.clone(),
            Job {
                session: session_id,
                cancel: Some(cancel_tx),
                reply: None,
            },
        );
    }
    let _guard = JobGuard(call_id.clone());
    let mut sequence = 0u32;
    let operation = execute(
        pool().await?,
        body,
        &call.args,
        &call.definition.capabilities,
        |name, args| {
            sequence += 1;
            let request_id = sequence;
            let call_id = call_id.clone();
            let desktop = desktop.clone();
            let on_host = on_host.clone();
            async move {
                desktop.pending_generated_call(session_id, &call_id)?;
                desktop.validate_call(session_id, &name, &args)?;
                let (send, receive) = oneshot::channel();
                {
                    let mut active = jobs()
                        .lock()
                        .map_err(|_| "Python helper state is unavailable.")?;
                    active
                        .get_mut(&call_id)
                        .ok_or("This Python helper was cancelled.")?
                        .reply = Some((request_id, send));
                }
                on_host
                    .send(HostRequest {
                        request_id,
                        name,
                        args,
                    })
                    .map_err(|error| error.to_string())?;
                receive
                    .await
                    .map_err(|_| "This Python helper was cancelled.".to_owned())?
            }
        },
    );
    tokio::select! {
        result = operation => result,
        _ = cancel_rx => Err("The chat changed before this Python helper finished.".into()),
    }
}

#[tauri::command]
pub fn desktop_tools_python_reply(
    session_id: SessionId,
    call_id: String,
    request_id: u32,
    result: Option<Value>,
    error: Option<String>,
) -> Result<(), String> {
    let result = if error.is_none() {
        Some(result.unwrap_or(Value::Null))
    } else {
        result
    };
    if result.is_some() == error.is_some() {
        return Err("Provide one helper result or error.".into());
    }
    if let Some(value) = &result {
        json_text(value)?;
    }
    let mut active = jobs()
        .lock()
        .map_err(|_| "Python helper state is unavailable.")?;
    let job = active
        .get_mut(&call_id)
        .filter(|job| job.session == session_id)
        .ok_or("This Python helper is no longer running.")?;
    if job.reply.as_ref().is_none_or(|(id, _)| *id != request_id) {
        return Err("This helper response is stale.".into());
    }
    let (_, reply) = job.reply.take().ok_or("This helper response is stale.")?;
    reply
        .send(match error {
            Some(error) => Err(error.chars().take(4096).collect()),
            None => Ok(result.unwrap_or(Value::Null)),
        })
        .map_err(|_| "This Python helper is no longer running.".into())
}

#[tauri::command]
pub fn desktop_tools_python_cancel(session_id: SessionId, call_id: String) -> Result<(), String> {
    let mut active = jobs()
        .lock()
        .map_err(|_| "Python helper state is unavailable.")?;
    if let Some(job) = active
        .get_mut(&call_id)
        .filter(|job| job.session == session_id)
    {
        if let Some(cancel) = job.cancel.take() {
            let _ = cancel.send(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn test_pool() -> Pool {
        new_pool(worker_path().expect("prepare the bundled Monty runtime before native tests"))
            .await
            .expect("Monty worker pool")
    }

    #[tokio::test]
    async fn monty_calls_only_declared_host_functions_and_returns_json() {
        let pool = test_pool().await;
        let result = execute(&pool, "workspace = host_call('desktop.getWorkspace', {})\nreturn {'title': workspace['title'], 'double': args['value'] * 2}", &json!({"value":21}), &["desktop.getWorkspace".into()], |name, args| async move {
            assert_eq!(name, "desktop.getWorkspace");
            assert_eq!(args, json!({}));
            Ok(json!({"title":"Monty acceptance"}))
        }).await.unwrap();
        assert_eq!(result, json!({"title":"Monty acceptance","double":42}));
        pool.close().await;
    }

    #[tokio::test]
    async fn monty_rejects_undeclared_host_access_before_callback() {
        let pool = test_pool().await;
        let error = execute(
            &pool,
            "return host_call('desktop.setDraft', {})",
            &json!({}),
            &[],
            |_, _| async { panic!("denied capability must not reach host") },
        )
        .await
        .unwrap_err();
        assert!(error.contains("no permission"), "{error}");
        pool.close().await;
    }

    #[tokio::test]
    async fn monty_has_no_host_files_environment_processes_or_network() {
        let pool = test_pool().await;
        let secret = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(secret.path(), "private-host-sentinel").unwrap();
        let cases = [
            format!(
                "return open({}).read()",
                serde_json::to_string(&secret.path().to_string_lossy()).unwrap()
            ),
            "import os\nreturn os.getenv('HOME')".into(),
            "import subprocess\nreturn subprocess.run(['sh', '-c', 'echo forbidden'])".into(),
            "import socket\nreturn socket.socket()".into(),
        ];
        for source in cases {
            let error = execute(&pool, &source, &json!({}), &[], |_, _| async {
                panic!("OS access must not invoke host")
            })
            .await
            .unwrap_err();
            assert!(!error.contains("private-host-sentinel"));
        }
        pool.close().await;
    }

    #[tokio::test]
    async fn monty_enforces_cpu_memory_output_and_host_call_limits_then_recovers() {
        let pool = test_pool().await;
        let cases = [
            "while True:\n    pass",
            "return 'x' * (64 * 1024 * 1024)",
            "return 'x' * (256 * 1024 + 1)",
            "for i in range(128):\n    print('x' * 4096)\nreturn True",
            "for i in range(33):\n    host_call('desktop.getWorkspace', {})\nreturn True",
        ];
        for source in cases {
            let started = std::time::Instant::now();
            let error = execute(
                &pool,
                source,
                &json!({}),
                &["desktop.getWorkspace".into()],
                |_, _| async { Ok(json!({})) },
            )
            .await
            .expect_err("resource exhaustion must stop");
            eprintln!("Monty bounded resource case rejected: {error}");
            assert!(started.elapsed() < Duration::from_secs(6));
            assert_eq!(
                execute(&pool, "return 42", &json!({}), &[], |_, _| async {
                    panic!("no host calls")
                })
                .await
                .unwrap(),
                json!(42)
            );
        }
        pool.close().await;
    }
}
