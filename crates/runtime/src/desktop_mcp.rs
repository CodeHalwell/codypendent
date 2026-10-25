use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_framework_core::tools::ToolDefinition;
use codypendent_protocol::Actor;
use serde_json::{json, Value};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::{UnixListener, UnixStream};

use crate::agent::{CancellationToken, FrameworkAgentRuntime, RunContext};
use crate::desktop_tools::DesktopTools;

const MAX_MESSAGE_BYTES: usize = 512 * 1024;

pub struct DesktopMcpServer {
    socket: PathBuf,
    instructions: String,
    worker: tokio::task::JoinHandle<()>,
}

impl DesktopMcpServer {
    pub fn start(
        runtime: FrameworkAgentRuntime,
        tools: DesktopTools,
        run: RunContext,
        actor: Actor,
        cancellation: CancellationToken,
    ) -> Result<Self, String> {
        let socket =
            PathBuf::from("/tmp").join(format!("cpd-ui-{}.sock", uuid::Uuid::new_v4().simple()));
        let listener =
            UnixListener::bind(&socket).map_err(|error| format!("desktop.mcp_bind: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| error.to_string())?;
        }
        let instructions = instructions(&runtime, &tools, &run);
        let runtime = Arc::new(runtime);
        let worker = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    accepted = listener.accept() => accepted,
                    _ = cancellation.cancelled() => break,
                };
                let Ok((stream, _)) = accepted else {
                    break;
                };
                let result =
                    serve_connection(stream, &runtime, &tools, &run, &actor, &cancellation).await;
                if let Err(error) = result {
                    tracing::debug!(%error, "desktop MCP connection closed");
                }
            }
        });
        Ok(Self {
            socket,
            instructions,
            worker,
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }
}

impl Drop for DesktopMcpServer {
    fn drop(&mut self) {
        self.worker.abort();
        let _ = std::fs::remove_file(&self.socket);
    }
}

async fn serve_connection(
    stream: UnixStream,
    runtime: &FrameworkAgentRuntime,
    tools: &DesktopTools,
    run: &RunContext,
    actor: &Actor,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    loop {
        let bytes = tokio::select! {
            bytes = read_message(&mut reader) => bytes?,
            _ = cancellation.cancelled() => return Ok(()),
        };
        if bytes.is_empty() {
            return Ok(());
        }
        let request: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let reply = match method {
            "initialize" => Ok(
                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"codypendent-desktop","version":env!("CARGO_PKG_VERSION")},"instructions":instructions(runtime, tools, run)}),
            ),
            "ping" => Ok(json!({})),
            "tools/list" => {
                let listed: Vec<Value> = definitions(runtime, tools, run).into_iter().map(|tool| json!({"name":crate::tool_names::encode(&tool.name),"description":format!("{} [native tool: {}]",tool.description,tool.name),"inputSchema":tool.parameters})).collect();
                Ok(json!({"tools":listed}))
            }
            "tools/call" => {
                let requested = request
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let canonical = definitions(runtime, tools, run)
                    .into_iter()
                    .find(|tool| {
                        tool.name == requested || crate::tool_names::encode(&tool.name) == requested
                    })
                    .map(|tool| tool.name);
                let name = canonical.as_deref().unwrap_or(requested);
                let args = request
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let (text, failed) = match runtime
                    .call_desktop_tool(run, actor, name, args, cancellation)
                    .await
                {
                    Ok(text) => (text, false),
                    Err(error) => (error, true),
                };
                Ok(json!({"content":[{"type":"text","text":text}],"isError":failed}))
            }
            _ => Err(json!({"code":-32601,"message":"Method not found"})),
        };
        let response = match reply {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(error) => json!({"jsonrpc":"2.0","id":id,"error":error}),
        };
        write_message(&mut writer, &response).await?;
        if method == "tools/call" {
            write_message(
                &mut writer,
                &json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}),
            )
            .await?;
        }
    }
}

fn definitions(
    runtime: &FrameworkAgentRuntime,
    _tools: &DesktopTools,
    run: &RunContext,
) -> Vec<ToolDefinition> {
    runtime.application_tool_definitions(run)
}

fn instructions(runtime: &FrameworkAgentRuntime, tools: &DesktopTools, run: &RunContext) -> String {
    let names = definitions(runtime, tools, run)
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    format!(
        "{}\n\n{}\n\nRun scope: read and search {}. The durable repository identity is {}. Paths outside that read scope need their own explicit grant.\n\nTool names in this guide are canonical. Invoke the advertised MCP name whose description identifies that native tool; do not guess a server prefix.\n\n{}\n\n{}",
        crate::agent_guidance::SYSTEM_PROMPT,
        crate::agent_guidance::mode_guidance(run.mode),
        run.read_root.display(),
        run.repository_identity().display(),
        crate::agent_guidance::for_tools(&names),
        tools.context_text(run.session_id).unwrap_or_default()
    )
}

async fn read_message(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_MESSAGE_BYTES as u64 + 1)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err("desktop.mcp_message_limit".into());
    }
    Ok(bytes)
}

async fn write_message(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &Value,
) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .map_err(|error| error.to_string())?;
    writer.flush().await.map_err(|error| error.to_string())
}

pub async fn proxy_stdio(socket: &Path) -> anyhow::Result<()> {
    let stream = UnixStream::connect(socket).await?;
    let (mut reader, mut writer) = stream.into_split();
    let input = async {
        tokio::io::copy(&mut tokio::io::stdin(), &mut writer).await?;
        writer.shutdown().await
    };
    let output = async {
        let mut stdout = tokio::io::stdout();
        tokio::io::copy(&mut reader, &mut stdout).await?;
        stdout.flush().await
    };
    tokio::select! { result = input => result?, result = output => result?, }
    Ok(())
}
