use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use codypendent_protocol::{
    AgentId, AnalyticsBucket, AnalyticsExportRequest, AnalyticsExportResult, AnalyticsQuery,
    ApprovalId, ArtifactId, ArtifactRef, AutomationBinding, AutomationBindingId,
    AutomationBindingQuery, AutomationBindingRequest, BranchId, BundleExportReceipt,
    BundleExportRequest, BundleImportReceipt, BundleImportRequest, BundleManifest, Catchup,
    ChangeSetId, CheckpointId, ClientHello, ClientId, CodeNodeId, Command, CommandBody, CommandId,
    CorrelationId, CouncilResultId, DaemonInstanceId, DataClassification, Diagnostic,
    DiagnosticSeverity, DiffRequest, DirtyBufferDigest, DocumentId, EditorActionContext,
    EditorNativeAction, EditorSelection, Envelope, EventBody, IdeContextUpdate, IdeRequest,
    InboxEntry, InboxEntryId, InboxListQuery, InboxMutation, InboxPage, InputEnvelope, LearningId,
    Location, MemoryId, MessageId, ModelId, OrganizationId, PageCursor, Payload,
    PendingApprovalProjection, PluginId, Position, PromptId, ProtocolVersion, QuestionId, Range,
    RegistryItemId, RepositoryId, RunId, ServerHello, SessionEvent, SessionExportOptions,
    SessionHistoryPage, SessionId, SessionLifecycleAction, SessionProjection, SessionSearchPage,
    SessionSearchQuery, SessionSearchResult, SessionSummary, SkillId, SourceProvenance, TaskId,
    TextEdit, ToolId, TriggerSource, UserId, WorkflowCatalog, WorkflowId, WorkspaceEdit,
    WorkspaceId,
};
use schemars::gen::SchemaSettings;
use schemars::JsonSchema;
use serde_json::Value;

#[derive(JsonSchema)]
#[allow(dead_code)]
struct IdCatalog {
    session_id: SessionId,
    run_id: RunId,
    task_id: TaskId,
    agent_id: AgentId,
    artifact_id: ArtifactId,
    change_set_id: ChangeSetId,
    workflow_id: WorkflowId,
    council_result_id: CouncilResultId,
    tool_id: ToolId,
    skill_id: SkillId,
    plugin_id: PluginId,
    document_id: DocumentId,
    workspace_id: WorkspaceId,
    client_id: ClientId,
    message_id: MessageId,
    command_id: CommandId,
    correlation_id: CorrelationId,
    approval_id: ApprovalId,
    question_id: QuestionId,
    checkpoint_id: CheckpointId,
    prompt_id: PromptId,
    daemon_instance_id: DaemonInstanceId,
    inbox_entry_id: InboxEntryId,
    automation_binding_id: AutomationBindingId,
    registry_item_id: RegistryItemId,
    memory_id: MemoryId,
    learning_id: LearningId,
    code_node_id: CodeNodeId,
    repository_id: RepositoryId,
    branch_id: BranchId,
    organization_id: OrganizationId,
    model_id: ModelId,
    user_id: UserId,
    timestamp: DateTime<Utc>,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct SessionCatalog {
    cursor: PageCursor,
    summary: SessionSummary,
    search_query: SessionSearchQuery,
    search_result: SessionSearchResult,
    search_page: SessionSearchPage,
    history_page: SessionHistoryPage,
    lifecycle: SessionLifecycleAction,
    export: SessionExportOptions,
    editor_action: EditorNativeAction,
    editor_context: EditorActionContext,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct InboxCatalog {
    entry: InboxEntry,
    query: InboxListQuery,
    mutation: InboxMutation,
    page: InboxPage,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct AnalyticsCatalog {
    query: AnalyticsQuery,
    bucket: AnalyticsBucket,
    export_request: AnalyticsExportRequest,
    export_result: AnalyticsExportResult,
    page: codypendent_protocol::AnalyticsPage,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct AutomationCatalog {
    source: TriggerSource,
    binding: AutomationBinding,
    query: AutomationBindingQuery,
    request: AutomationBindingRequest,
    page: codypendent_protocol::AutomationBindingPage,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct BundleCatalog {
    manifest: BundleManifest,
    export_request: BundleExportRequest,
    export_receipt: BundleExportReceipt,
    import_request: BundleImportRequest,
    import_receipt: BundleImportReceipt,
}

fn usage() -> &'static str {
    "usage: export_schema --output-dir <directory>"
}

fn output_directory() -> Result<PathBuf, String> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--output-dir")) {
        return Err(usage().to_owned());
    }
    let output = arguments
        .next()
        .ok_or_else(|| "missing value for --output-dir".to_owned())?;
    if arguments.next().is_some() {
        return Err(format!("unexpected argument\n{}", usage()));
    }
    Ok(output.into())
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize).collect()),
        Value::Object(values) => {
            let sorted = values
                .into_iter()
                .map(|(key, value)| (key, canonicalize(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        scalar => scalar,
    }
}

fn write_schema<T: JsonSchema>(output: &Path, filename: &str) -> Result<(), Box<dyn Error>> {
    let generator = SchemaSettings::draft07().into_generator();
    let schema = generator.into_root_schema_for::<T>();
    let canonical = canonicalize(serde_json::to_value(schema)?);
    let mut rendered = serde_json::to_string_pretty(&canonical)?;
    rendered.push('\n');
    fs::write(output.join(filename), rendered)?;
    Ok(())
}

fn export(output: &Path) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(output)?;

    write_schema::<AnalyticsCatalog>(output, "analytics-catalog.schema.json")?;
    write_schema::<ArtifactRef>(output, "artifact-ref.schema.json")?;
    write_schema::<AutomationCatalog>(output, "automation-catalog.schema.json")?;
    write_schema::<BundleCatalog>(output, "bundle-catalog.schema.json")?;
    write_schema::<Catchup>(output, "catchup.schema.json")?;
    write_schema::<ClientHello>(output, "client-hello.schema.json")?;
    write_schema::<CommandBody>(output, "command-body.schema.json")?;
    write_schema::<Command>(output, "command.schema.json")?;
    write_schema::<DataClassification>(output, "data-classification.schema.json")?;
    write_schema::<DiagnosticSeverity>(output, "diagnostic-severity.schema.json")?;
    write_schema::<Diagnostic>(output, "diagnostic.schema.json")?;
    write_schema::<DiffRequest>(output, "diff-request.schema.json")?;
    write_schema::<DirtyBufferDigest>(output, "dirty-buffer-digest.schema.json")?;
    write_schema::<EditorSelection>(output, "editor-selection.schema.json")?;
    write_schema::<Envelope>(output, "envelope.schema.json")?;
    write_schema::<EventBody>(output, "event-body.schema.json")?;
    write_schema::<IdeContextUpdate>(output, "ide-context-update.schema.json")?;
    write_schema::<IdeRequest>(output, "ide-request.schema.json")?;
    write_schema::<IdCatalog>(output, "id-catalog.schema.json")?;
    write_schema::<InputEnvelope>(output, "input-envelope.schema.json")?;
    write_schema::<InboxCatalog>(output, "inbox-catalog.schema.json")?;
    write_schema::<Location>(output, "location.schema.json")?;
    write_schema::<Payload>(output, "payload.schema.json")?;
    write_schema::<PendingApprovalProjection>(output, "pending-approval-projection.schema.json")?;
    write_schema::<Position>(output, "position.schema.json")?;
    write_schema::<ProtocolVersion>(output, "protocol-version.schema.json")?;
    write_schema::<Range>(output, "range.schema.json")?;
    write_schema::<ServerHello>(output, "server-hello.schema.json")?;
    write_schema::<SessionEvent>(output, "session-event.schema.json")?;
    write_schema::<SessionProjection>(output, "session-projection.schema.json")?;
    write_schema::<SessionCatalog>(output, "session-catalog.schema.json")?;
    write_schema::<SourceProvenance>(output, "source-provenance.schema.json")?;
    write_schema::<TextEdit>(output, "text-edit.schema.json")?;
    write_schema::<WorkspaceEdit>(output, "workspace-edit.schema.json")?;
    write_schema::<WorkflowCatalog>(output, "workflow-catalog.schema.json")?;

    Ok(())
}

fn main() {
    let output = output_directory().unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(2);
    });
    if let Err(error) = export(&output) {
        eprintln!("failed to export protocol schemas: {error}");
        std::process::exit(1);
    }
}
