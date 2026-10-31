use codypendent_protocol::SessionId;
use codypendent_runtime::desktop_tools::{DesktopTool, DesktopToolCall};
use serde_json::Value;

#[tauri::command]
pub fn desktop_tools_publish(
    session_id: SessionId,
    context: Value,
    tools: Vec<DesktopTool>,
) -> Result<(), String> {
    crate::runtime_paths::desktop_tools()?.publish(session_id, context, tools)
}

#[tauri::command]
pub fn desktop_tools_pending(session_id: SessionId) -> Result<Option<DesktopToolCall>, String> {
    crate::runtime_paths::desktop_tools()?.pending(session_id)
}

#[tauri::command]
pub fn desktop_tools_resolve(
    session_id: SessionId,
    call_id: String,
    result: Option<Value>,
    error: Option<String>,
) -> Result<(), String> {
    let result = if error.is_none() {
        Some(result.unwrap_or(Value::Null))
    } else {
        result
    };
    crate::runtime_paths::desktop_tools()?.resolve_call(session_id, &call_id, result, error)
}

#[tauri::command]
pub fn desktop_tools_list(session_id: SessionId) -> Result<Vec<DesktopTool>, String> {
    crate::runtime_paths::desktop_tools()?.list(session_id)
}

#[tauri::command]
pub fn desktop_tools_remove(
    session_id: SessionId,
    name: String,
    expected_version: u64,
) -> Result<(), String> {
    crate::runtime_paths::desktop_tools()?.remove(session_id, &name, expected_version)
}
