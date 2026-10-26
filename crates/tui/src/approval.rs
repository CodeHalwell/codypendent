//! Approval evidence shared by the graphical and accessible clients.
use codypendent_protocol::ProposedAction;

pub(crate) const UNREVIEWABLE: &str =
    "This client cannot review this action. Reject it or update Codypendent before approving.";

pub(crate) fn can_review(action: &ProposedAction) -> bool {
    details(action).is_some()
}

pub(crate) fn describe_action(action: &ProposedAction) -> Vec<String> {
    details(action)
        .unwrap_or_else(|| vec![UNREVIEWABLE.to_owned()])
        .into_iter()
        .map(|line| crate::remote_ui::sanitize_terminal_text(&line))
        .collect()
}

/// Verbatim rendering of a proposed action's fields (approval modal).
fn details(action: &ProposedAction) -> Option<Vec<String>> {
    Some(match action {
        ProposedAction::ReadFiles { paths } => {
            let mut v = vec!["read files:".to_owned()];
            v.extend(paths.iter().map(|p| format!("  {p}")));
            v
        }
        ProposedAction::WritePatch { patch } => vec![format!("apply patch: {patch}")],
        ProposedAction::ExecuteCommand {
            program,
            args,
            environment,
            cwd,
        } => {
            // Render the FULL environment and cwd: an unshown binding could
            // smuggle an execution-hijacking variable past a benign-looking
            // command line, so the approver must see every one verbatim.
            let mut v = vec![format!("command: {program} {}", args.join(" "))];
            if let Some(cwd) = cwd {
                v.push(format!("cwd: {cwd}"));
            }
            for (name, value) in environment {
                v.push(format!("env: {name}={value}"));
            }
            v
        }
        ProposedAction::NetworkRequest { destination } => {
            vec![format!("network request: {destination}")]
        }
        ProposedAction::GitCommit { repository } => vec![format!("git commit: {repository}")],
        ProposedAction::GitHubMutation {
            repository,
            summary,
        } => vec![
            format!("GitHub repository: {repository}"),
            format!("write: {summary}"),
        ],
        ProposedAction::RestoreCheckpoint {
            run_id,
            ordinal,
            worktree,
            commit,
        } => vec![
            format!("restore checkpoint: turn {ordinal} of run {run_id}"),
            format!("worktree: {worktree}"),
            format!("commit: {commit}"),
            "Changes after this checkpoint will be discarded.".to_owned(),
        ],
        ProposedAction::GitPush { remote, branch } => {
            vec![format!("git push: {remote} {branch}")]
        }
        // STEP 4.4.2: every publish displays target, changed files, and the
        // resulting Git action before approval — render all three verbatim
        // from the plan, exactly as computed (never re-derived here).
        ProposedAction::PublishDocument {
            target,
            changed_files,
            git_action,
            ..
        } => {
            let mut v = vec![format!("publish document: {target}")];
            v.push("changed files:".to_owned());
            v.extend(changed_files.iter().map(|f| format!("  {f}")));
            v.push(format!("git action: {git_action}"));
            v
        }
        // PR B (MCP client): the approver must see WHICH server and tool, the
        // human summary, and the `args` string VERBATIM — it is canonical JSON,
        // already auditable — exactly as ExecuteCommand renders program/args/env
        // verbatim.
        ProposedAction::McpToolCall {
            server,
            tool,
            summary,
            args,
        } => vec![
            format!("mcp tool: {server}.{tool}"),
            format!("summary: {summary}"),
            format!("args: {args}"),
        ],
        ProposedAction::AcpToolCall {
            agent,
            title,
            details,
        } => vec![
            format!("ACP agent: {agent}"),
            format!("tool: {title}"),
            format!("details: {details}"),
        ],
        ProposedAction::CouncilCreate { name, summary } => vec![
            format!("create council: {name}"),
            format!("preview: {summary}"),
        ],
        ProposedAction::CouncilRun { name, summary } => vec![
            format!("run council: {name}"),
            format!("preview: {summary}"),
        ],
        ProposedAction::WorkflowCreate {
            workflow_id,
            summary,
        } => vec![
            format!("create workflow: {workflow_id}"),
            format!("preview: {summary}"),
        ],
        ProposedAction::WorkflowRun {
            workflow_id,
            kind,
            summary,
        } => vec![
            format!("run {kind} workflow: {workflow_id}"),
            format!("preview: {summary}"),
        ],
        // Adoption 09: stdin to a live process carries the same authority as
        // spawning a command, so the approver must see which process and how
        // many bytes. The bytes themselves are never carried (no echoed secret).
        ProposedAction::WriteProcessStdin {
            process_id,
            byte_len,
        } => vec![format!(
            "write to process {process_id}: {byte_len} bytes to stdin"
        )],
        _ => return None,
    })
}
