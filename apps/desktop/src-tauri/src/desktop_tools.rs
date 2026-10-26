use codypendent_protocol::SessionId;
use codypendent_runtime::desktop_tools::{DesktopTool, DesktopToolCall, DesktopTools};
use serde_json::Value;

#[tauri::command]
pub fn desktop_tools_publish(
    session_id: SessionId,
    context: Value,
    tools: Vec<DesktopTool>,
) -> Result<(), String> {
    DesktopTools::resolve()?.publish(session_id, context, tools)
}

#[tauri::command]
pub fn desktop_tools_pending(session_id: SessionId) -> Result<Option<DesktopToolCall>, String> {
    DesktopTools::resolve()?.pending(session_id)
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
    DesktopTools::resolve()?.resolve_call(session_id, &call_id, result, error)
}

#[tauri::command]
pub fn desktop_tools_list(session_id: SessionId) -> Result<Vec<DesktopTool>, String> {
    DesktopTools::resolve()?.list(session_id)
}

#[tauri::command]
pub fn desktop_tools_remove(
    session_id: SessionId,
    name: String,
    expected_version: u64,
) -> Result<(), String> {
    DesktopTools::resolve()?.remove(session_id, &name, expected_version)
}
