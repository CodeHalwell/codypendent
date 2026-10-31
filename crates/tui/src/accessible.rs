//! Cooked-terminal accessibility projection and line-command mapping.
//!
//! This module deliberately contains no terminal I/O. The CLI's accessible
//! harness prints [`accessible_snapshot`] on an ordinary cooked stdout and feeds
//! complete input lines through [`map_accessible_input`]. Keeping both pieces
//! pure makes the no-alternate-screen path deterministic and script-testable.

use codypendent_protocol::{ApprovalScope, RunDisposition};
use ratatui::{buffer::Buffer, layout::Rect};

use crate::action::Action;
use crate::remote_ui::{project_accessibility, render_remote_ui, RemoteKey, RemoteUiRenderOptions};
use crate::state::{
    filter_council_member_models, filter_model_names, filter_models, filter_modes,
    filter_onboard_providers, filter_providers, AppState, CouncilBuilderStep, InputMode,
    ModelListOrigin, ModelReadiness, OnboardProviderClass, OnboardStep, Overlay, RunActivity,
    TranscriptEntry, MODE_CARDS,
};
use crate::Theme;

/// Render the complete current application state as a stable, linear document.
/// UI chrome is ASCII-only; user/model/extension content retains Unicode but is
/// stripped of terminal and bidi controls before it reaches cooked stdout.
#[must_use]
pub fn accessible_snapshot(state: &AppState) -> String {
    refresh_remote_ui_render_cache(state);
    let mut lines = vec!["Codypendent accessible view".to_owned()];
    lines.push(format!(
        "Session: {}",
        clean(state.session_title.as_deref().unwrap_or("untitled"))
    ));
    if let Some(notice) = state.notice.as_ref().map(|(message, _)| message) {
        lines.push(format!("Notice: {}", clean(notice)));
    }
    for issue in &state.issues {
        lines.push(format!("Issue: {}", clean(issue)));
    }

    if state.runs.is_empty() {
        lines.push("Conversation: no runs yet".to_owned());
        if !state.has_runnable_models() {
            lines.push(if state.models.is_empty() {
                "Setup required: no model is configured. A verified model is required to start a run."
                    .to_owned()
            } else {
                "Setup required: saved models exist, but none is runnable. A credential, endpoint, or supported adapter may be missing."
                    .to_owned()
            });
            lines.push(
                "Setup control: submit an empty composer to open guided model setup, or enter slash for all commands."
                    .to_owned(),
            );
        }
    } else {
        lines.push(format!(
            "Conversation: {} run(s); selected {}",
            state.runs.len(),
            state.selected_run.saturating_add(1).min(state.runs.len())
        ));
        for (index, run) in state.runs.iter().enumerate() {
            let selected = if index == state.selected_run {
                " selected"
            } else {
                ""
            };
            lines.push(format!(
                "Run {}{}: {:?}; mode {:?}; objective {}",
                index + 1,
                selected,
                run.state,
                run.mode,
                clean(&run.objective)
            ));
            if let Some(model) = &run.model {
                lines.push(format!("Model: {}", clean(&model.0)));
            }
            // The run's MEASURED usage. This client used to print
            // "Unsupported event: unsupported event" here instead — the
            // `RunUsage` event with no reducer arm. Only measured dimensions
            // are announced: an absent one means the provider reported none,
            // never zero.
            if let Some(usage) = usage_sentence(run) {
                lines.push(format!("Usage: {usage}"));
            }
            match &run.activity {
                RunActivity::Idle => {}
                RunActivity::Thinking => lines.push("Activity: thinking".to_owned()),
                RunActivity::Streaming => lines.push("Activity: responding".to_owned()),
                RunActivity::RunningTool(tool) => {
                    lines.push(format!("Activity: running tool {}", clean(tool)));
                }
                RunActivity::Retrying {
                    attempt,
                    max_attempts,
                    message,
                    delay_ms,
                } => {
                    lines.push(format!(
                        "Activity: retrying ({attempt}/{max_attempts}): {} — next attempt in {} seconds",
                        clean(message),
                        delay_ms.div_ceil(1000).max(1)
                    ));
                }
            }
            for entry in &run.transcript {
                append_transcript(&mut lines, entry, run.model.as_ref());
            }
        }
    }

    if !state.pending_approvals.is_empty() {
        lines.push(format!(
            "Approvals: {} pending; selected {}",
            state.pending_approvals.len(),
            state
                .selected_approval
                .saturating_add(1)
                .min(state.pending_approvals.len())
        ));
        if let Some(approval) = state.focused_approval() {
            lines.extend(crate::approval::describe_action(&approval.action));
            lines.push(format!("Approval risk: {:?}", approval.risk));
            lines.push(if crate::approval::can_review(&approval.action) {
                "Approval controls: approve, approve-run, reject".to_owned()
            } else {
                "Approval controls: reject".to_owned()
            });
        }
    }

    append_remote_ui(&mut lines, state);
    append_overlay(&mut lines, state);

    if !state.composer.is_empty() {
        lines.push(format!("Composer draft: {}", clean(&state.composer)));
    }
    lines.push(controls_for(state.input_mode()).to_owned());
    // Sanitize the finished projection as well as individual untrusted fields:
    // several protocol enums use `Debug` formatting above and future variants
    // must not accidentally reintroduce terminal controls through that path.
    clean(&lines.join("\n"))
}

/// The normal renderer populates the interaction descriptors consumed by the
/// Remote UI reducer. Cooked-terminal mode has no frame renderer, so build the
/// same bounded metadata here to keep its announced controls operable.
fn refresh_remote_ui_render_cache(state: &AppState) {
    let documents = state.remote_ui.mounted_documents();
    let mut cache = state.remote_ui.last_render.borrow_mut();
    cache.clear();
    for document in documents {
        let projection = project_accessibility(&document.root, &state.remote_ui.capabilities);
        let height = projection.nodes.len().saturating_mul(2).clamp(24, 512) as u16;
        let area = Rect::new(0, 0, 120, height);
        let mut buffer = Buffer::empty(area);
        let output = render_remote_ui(
            &mut buffer,
            area,
            document,
            &Theme::default(),
            &state.remote_ui.capabilities,
            &state.remote_ui.view,
            RemoteUiRenderOptions::default(),
        );
        cache.insert(document.document_id.clone(), output);
    }
}

/// A run's measured usage as a spoken sentence, or `None` when the provider
/// measured nothing. Words rather than glyphs — this projection is read aloud.
fn usage_sentence(run: &crate::state::RunView) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(tokens) = run.prompt_tokens {
        parts.push(format!("{tokens} prompt tokens"));
    }
    if let Some(tokens) = run.completion_tokens {
        parts.push(format!("{tokens} completion tokens"));
    }
    if let Some(micros) = run.cost_micros {
        parts.push(format!(
            "cost {} US dollars",
            crate::usage::format_cost_micros(micros).trim_start_matches('$')
        ));
    }
    if !parts.is_empty() {
        if let Some(label) = crate::state::usage_coverage_label(
            run.usage_coverage,
            [
                run.prompt_tokens.is_some(),
                run.completion_tokens.is_some(),
                run.cost_micros.is_some(),
            ],
        ) {
            parts.push(format!("measured subtotal; {label}"));
        }
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

fn append_transcript(
    lines: &mut Vec<String>,
    entry: &TranscriptEntry,
    model: Option<&codypendent_protocol::ModelId>,
) {
    match entry {
        TranscriptEntry::User { text } => lines.push(format!("You: {}", clean(text))),
        TranscriptEntry::Model { text, .. } => {
            lines.push(format!("Assistant: {}", clean(text)));
        }
        TranscriptEntry::Tool(tool) => {
            let status = match tool.status {
                crate::state::ToolStatus::Proposed => "awaiting review",
                crate::state::ToolStatus::Running => "running",
                crate::state::ToolStatus::Completed => "completed",
            };
            let label = tool
                .label
                .as_deref()
                .map(|label| format!("; {}", clean(label)))
                .unwrap_or_default();
            lines.push(format!("Tool: {}; {status}{label}", clean(&tool.tool)));
            if let Some(codypendent_protocol::ToolOutcome::Failed { message }) = &tool.outcome {
                lines.push(format!(
                    "Tool failure: {}",
                    crate::state::sanitize_failure_text(message)
                ));
            }
            lines.push("Tool card controls: browse with alt up or alt down; alt Enter expands; alt Y copies its safe projection.".to_owned());
        }
        TranscriptEntry::Patch(patch) => lines.push(format!(
            "Patch: {} file(s), {} additions, {} deletions",
            patch.files.len(),
            patch.additions,
            patch.deletions
        )),
        TranscriptEntry::Steering { applied } => lines.push(if *applied {
            "Steering: applied".to_owned()
        } else {
            "Steering: queued".to_owned()
        }),
        TranscriptEntry::Budget {
            dimension,
            used,
            limit,
        } => lines.push(format!("Budget warning: {dimension:?} {used} of {limit}")),
        TranscriptEntry::Completed { disposition, .. } => match disposition {
            RunDisposition::Completed { summary } => lines.push(format!(
                "Completed: {}",
                clean(summary.as_deref().unwrap_or("success"))
            )),
            RunDisposition::Failed { reason, .. } => {
                if let Some(failure) = crate::state::acp_failure_summary(model, reason) {
                    lines.push(format!(
                        "ACP failure: provider {}; model {}; phase {}; cause {}",
                        clean(&failure.provider),
                        clean(&failure.model),
                        failure.phase,
                        clean(&failure.cause)
                    ));
                    lines.push(format!(
                        "ACP recovery controls: alt R retries; {}alt M chooses another model; diagnostics opens setup diagnostics; alt D disables the configured profile; alt Y copies this safe card.",
                        if failure.auth_related { "alt A explains re-authentication; " } else { "" }
                    ));
                } else {
                    lines.push(format!(
                        "Failed: {}",
                        crate::state::sanitize_failure_text(reason)
                    ));
                    lines.push("Failure controls: alt R retries; alt M chooses another model; diagnostics opens setup diagnostics; alt Y copies this safe card.".to_owned());
                }
            }
            RunDisposition::Cancelled { reason } => lines.push(format!(
                "Cancelled: {}",
                clean(reason.as_deref().unwrap_or("no reason supplied"))
            )),
            RunDisposition::Unknown => lines.push("Completed: unknown outcome".to_owned()),
            _ => lines.push("Completed: unsupported outcome".to_owned()),
        },
        TranscriptEntry::Note { text, .. } => lines.push(format!("Note: {}", clean(text))),
        TranscriptEntry::Reasoning { text, expanded } => lines.push(if *expanded {
            format!("Reasoning: {}", clean(text))
        } else {
            // Folded: announce that reasoning happened and how much, not the
            // body — a screen reader must not have the model's deliberation
            // read at it in full before the answer it was reasoning towards.
            format!("Reasoning, folded: {} character(s)", text.chars().count())
        }),
        TranscriptEntry::Backstage {
            context_lines,
            memory_updates,
            ..
        } => lines.push(format!(
            "Backstage: {} context line(s), {memory_updates} memory update(s)",
            context_lines.unwrap_or(0)
        )),
        TranscriptEntry::Unsupported { label } => {
            lines.push(format!("Unsupported event: {}", clean(label)));
        }
    }
}

fn append_remote_ui(lines: &mut Vec<String>, state: &AppState) {
    let documents = state.remote_ui.mounted_documents();
    if documents.is_empty() {
        return;
    }
    lines.push(format!("Extension documents: {}", documents.len()));
    for document in documents {
        let focused = state.remote_ui.active
            && state.remote_ui.focused_document.as_ref() == Some(&document.document_id);
        let (extension, publisher, trust) = state
            .remote_ui
            .extension_identity_for_document(&document.document_id)
            .unwrap_or(("unknown extension", None, None));
        let mut identity = clean(extension);
        if let Some(publisher) = publisher {
            identity.push_str("; publisher ");
            identity.push_str(&clean(publisher));
        }
        if let Some(trust) = trust {
            identity.push_str("; trust ");
            identity.push_str(&clean(trust));
        }
        lines.push(format!(
            "Extension document {}{}: {}",
            clean(document.document_id.as_str()),
            if focused { " focused" } else { "" },
            identity
        ));
        let projection = project_accessibility(&document.root, &state.remote_ui.capabilities);
        if projection.plain_text.is_empty() {
            lines.push("Extension content: no readable content".to_owned());
        } else {
            lines.push("Extension content:".to_owned());
            lines.extend(
                projection
                    .plain_text
                    .lines()
                    .map(|line| format!("  {}", clean(line))),
            );
        }
        if !projection.nodes.is_empty() {
            lines.push("Extension semantic controls:".to_owned());
            for node in &projection.nodes {
                let selected =
                    focused && node.node_id.as_ref() == state.remote_ui.view.focused_node.as_ref();
                let mut description = format!(
                    "  {}{}: {}",
                    node.role.as_str(),
                    if selected { " focused" } else { "" },
                    clean(&node.label)
                );
                if node.disabled {
                    description.push_str("; disabled");
                }
                if let Some(detail) = &node.description {
                    description.push_str("; ");
                    description.push_str(&clean(detail));
                }
                if let Some(hint) = &node.keyboard_hint {
                    description.push_str("; keyboard ");
                    description.push_str(&clean(hint));
                }
                if let Some(live_region) = &node.live_region {
                    description.push_str("; live region ");
                    description.push_str(&clean(live_region));
                }
                lines.push(description);
            }
        }
        if focused {
            if let Some(focused_node) = state.remote_ui.view.focused_node.as_ref() {
                if let Some(node) = projection
                    .nodes
                    .iter()
                    .find(|node| node.node_id.as_ref() == Some(focused_node))
                {
                    let mut focused_line = format!("Focused control: {}", clean(&node.label));
                    if node.disabled {
                        focused_line.push_str("; disabled");
                    }
                    if let Some(hint) = &node.keyboard_hint {
                        focused_line.push_str("; ");
                        focused_line.push_str(&clean(hint));
                    }
                    lines.push(focused_line);
                }
            }
        }
    }
}

fn append_overlay(lines: &mut Vec<String>, state: &AppState) {
    match &state.overlay {
        Overlay::None => {}
        Overlay::Onboard { step } => match step {
            OnboardStep::Triage { selected } => {
                lines.push("Model setup: choose a connection route. Nothing is saved until a provider and model are selected.".to_owned());
                let rows = vec![
                    "Hosted API; use a provider API key from the environment or save one locally"
                        .to_owned(),
                    "Local endpoint; connect Ollama, LM Studio, or vLLM already running on this machine"
                        .to_owned(),
                    "ACP coding agent; connect an installed agent such as Claude Code, Codex, Kimi, Amp, or Cline"
                        .to_owned(),
                ];
                append_picker_rows(lines, "connection route", rows, *selected);
                lines.push(
                    "Setup controls: up or down selects a route, Enter continues, Esc opens skip choices."
                        .to_owned(),
                );
            }
            OnboardStep::SkipConfirm { selected } => {
                lines.push("Skip model setup? Codypendent cannot start agent runs without a runnable model.".to_owned());
                let rows = vec![
                    "Skip future startup setup; do not open setup automatically again"
                        .to_owned(),
                    "Continue setup; return to the connection routes".to_owned(),
                    "Cancel; return to setup without changing providers, models, or credentials"
                        .to_owned(),
                ];
                append_picker_rows(lines, "skip choice", rows, *selected);
                lines.push(
                    "Skip controls: up or down selects, Enter chooses, Esc returns to setup."
                        .to_owned(),
                );
            }
            OnboardStep::Validating { model_id } => {
                lines.push(format!(
                    "Model setup validating: {}. The profile was saved; credentials, protocol support, and availability are being checked.",
                    clean(&model_id.0)
                ));
                lines.push(
                    "Setup completes only when this exact model can start a run. Please wait; this operation cannot be cancelled safely."
                        .to_owned(),
                );
            }
        },
        Overlay::OnboardProviderPicker {
            class,
            query,
            selected,
        } => {
            let label = onboard_class_accessible_label(*class);
            lines.push(format!(
                "Model setup, {label} providers: query {}. Only providers in this connection route are shown.",
                clean(query)
            ));
            let rows = filter_onboard_providers(&state.providers, *class, query)
                .into_iter()
                .filter_map(|index| state.providers.get(index))
                .map(|card| {
                    format!(
                        "{} ({}); protocol {}; authentication {}; model discovery {}",
                        clean(&card.name),
                        clean(&card.id),
                        clean(&card.protocol),
                        clean(&card.auth),
                        if card.can_list_models {
                            "live listing"
                        } else if card.catalog_models > 0 {
                            "catalog"
                        } else {
                            "manual model name"
                        }
                    )
                })
                .collect();
            append_picker_rows(lines, "setup provider", rows, *selected);
            lines.push(
                "Provider controls: type text to filter, up or down selects, Enter opens model discovery, Esc returns to connection routes."
                    .to_owned(),
            );
        }
        Overlay::Help => {
            lines.push(format!("Shortcut filter: {}", clean(&state.help_query)));
            lines.push("Help:".to_owned());
            lines.extend(crate::input::filtered_key_bindings(&state.help_query).iter().map(|binding| {
                format!(
                    "  {}: {}",
                    ascii_keys(binding.keys),
                    ascii_chrome(binding.description)
                )
            }));
        }
        Overlay::Palette { query, selected } => {
            lines.push(format!("Command palette: query {}", clean(query)));
            let rows = crate::palette::filtered(query)
                .into_iter()
                .map(|entry| {
                    let shortcut = if entry.key == "—" {
                        "no direct shortcut".to_owned()
                    } else {
                        format!("shortcut {}", clean(entry.key))
                    };
                    format!(
                        "{}; {}; {}",
                        clean(entry.title),
                        clean(entry.description),
                        shortcut
                    )
                })
                .collect();
            append_picker_rows(lines, "command", rows, *selected);
        }
        Overlay::ModelPicker { query, selected } => {
            lines.push(format!("Model picker: query {}", clean(query)));
            let rows = filter_models(&state.models, query)
                .into_iter()
                .filter_map(|index| state.models.get(index))
                .map(|card| {
                    let kind = if card.acp_supplier().is_some() {
                        "ACP supplier; Enter connects, tests, and browses its live models"
                    } else {
                        "concrete configured model; Enter stages it"
                    };
                    format!(
                        "{}; provider {}; {}; {}",
                        clean(&card.id.0),
                        clean(&card.provider),
                        model_readiness_label(&card.readiness),
                        kind
                    )
                })
                .collect();
            append_picker_rows(lines, "model", rows, *selected);
        }
        Overlay::ProviderPicker { query, selected } => {
            lines.push(format!("Provider picker: query {}", clean(query)));
            let rows = filter_providers(&state.providers, query)
                .into_iter()
                .filter_map(|index| state.providers.get(index))
                .map(|card| {
                    format!(
                        "{} ({}); protocol {}; {}",
                        clean(&card.name),
                        clean(&card.id),
                        clean(&card.protocol),
                        if card.available {
                            "available"
                        } else {
                            "not yet executable"
                        }
                    )
                })
                .collect();
            append_picker_rows(lines, "provider", rows, *selected);
            lines.push(
                "Provider control: Enter continues to step 2, the concrete model catalogue."
                    .to_owned(),
            );
        }
        Overlay::AddModelPick {
            provider_id,
            models,
            query,
            selected,
            origin,
            refreshing,
            ..
        } => {
            let source = match origin {
                ModelListOrigin::Live => "live supplier listing".to_owned(),
                ModelListOrigin::Cached(age) => format!("cached supplier listing, {age}"),
                ModelListOrigin::Catalog(reason) if reason.is_empty() => {
                    "curated catalogue".to_owned()
                }
                ModelListOrigin::Catalog(reason) => {
                    format!("curated catalogue because {}", clean(reason))
                }
            };
            lines.push(format!(
                "Choose model, step 2 of 2, provider {}; source {}{}; query {}",
                clean(provider_id),
                source,
                if *refreshing { "; retry in progress" } else { "" },
                clean(query)
            ));
            let rows = filter_model_names(models, query)
                .into_iter()
                .filter_map(|index| models.get(index))
                .map(|row| {
                    format!(
                        "{}; {}; context {}; input cost {}; output cost {}",
                        clean(&row.id),
                        if row.live { "live" } else { "catalogue only" },
                        row.context_tokens.map_or("unknown".to_owned(), |value| value.to_string()),
                        row.cost_per_1m_input_usd.map_or("unknown".to_owned(), |value| format!("${value} per million")),
                        row.cost_per_1m_output_usd.map_or("unknown".to_owned(), |value| format!("${value} per million")),
                    )
                })
                .collect();
            append_picker_rows(lines, "supplier model", rows, *selected);
            lines.push("Model controls: arrows, page, Home, or End move; Enter adds the concrete model; control R reconnects and tests the supplier; Escape closes.".to_owned());
        }
        Overlay::ModePicker { query, selected } => {
            lines.push(format!("Mode picker: query {}", clean(query)));
            let rows = filter_modes(query)
                .into_iter()
                .filter_map(|index| MODE_CARDS.get(index))
                .map(|card| format!("{}; {}", card.label, card.summary))
                .collect();
            append_picker_rows(lines, "mode", rows, *selected);
        }
        Overlay::NewRun(buffer) => {
            lines.push(format!("New run prompt: {}", clean(buffer)));
        }
        Overlay::Steering(buffer) => {
            lines.push(format!("Steering prompt: {}", clean(buffer)));
        }
        Overlay::KanbanNew { buffer } => lines.push(format!(
            "Create Kanban task. Title: {}. Example: Add a regression test for ACP reconnects.",
            clean(buffer)
        )),
        Overlay::BlackboardPost {
            workflow_run_id,
            buffer,
        } => lines.push(format!(
            "Post open question to Blackboard run {}: {}",
            clean(workflow_run_id),
            clean(buffer)
        )),
        Overlay::Workflow => {
            lines.push(format!(
                "Executable persisted workflow graph: {} nodes.",
                state.workflow.len()
            ));
            if state.workflow.is_empty() {
                lines.push("No workflow manifests. Press n to draft an inspect, implement, verify example; review it before sending.".to_owned());
            } else {
                lines.push("Press n to run the selected workflow; p pauses, r retries, c cancels.".to_owned());
            }
        }
        Overlay::Blackboard => {
            lines.push(format!(
                "Blackboard evidence, decision, and artifact stream: {} items.",
                state.blackboard.len()
            ));
            lines.push("Press n to post an open question. A workflow run is required; council handoff is never automatic.".to_owned());
        }
        Overlay::Kanban => {
            lines.push(format!(
                "Kanban repository task board: {} cards.",
                state.kanban.len()
            ));
            lines.push("Press n to create a task; left or right moves the selected card between columns.".to_owned());
        }
        Overlay::ConfirmCancel { run_id } => {
            match state.runs.iter().find(|run| run.run_id == *run_id) {
                Some(run) => {
                    lines.push(format!(
                        "Confirmation: cancel run {run_id}: {}? Mode: {:?}. State: {:?}. Its chronicle and artifacts are kept.",
                        clean(&run.objective),
                        run.mode,
                        run.state,
                    ));
                    if matches!(
                        run.state,
                        codypendent_protocol::RunState::Completed
                            | codypendent_protocol::RunState::Failed
                            | codypendent_protocol::RunState::Cancelled
                    ) {
                        lines.push("This run has already finished; no cancellation will be sent.".to_owned());
                    }
                }
                None => lines.push(format!(
                    "Confirmation: run {run_id} is no longer available; no cancellation will be sent."
                )),
            }
        }
        Overlay::ConfirmWorkflowCancel { workflow_run_id } => lines.push(format!(
            "Confirmation: cancel workflow run {}?",
            clean(workflow_run_id)
        )),
        Overlay::CouncilRunObjective { name, buffer } => {
            lines.push(format!(
                "Objective for council {}: {}",
                clean(name),
                clean(buffer)
            ));
        }
        Overlay::ConfirmCouncilDelete { name } => lines.push(format!(
            "Confirmation: remove council {}? Saved run reports remain on disk.",
            clean(name)
        )),
        Overlay::ConfirmModelRemove {
            model_id, provider, ..
        } => lines.push(format!(
            "Confirmation: remove user-configured model {} (provider {}) and its model-specific saved key? Comments and ordering in models.toml, and the provider catalogue, remain available.",
            clean(model_id),
            clean(provider)
        )),
        Overlay::ConfirmCommunityAcpInstall { .. } => {
            lines.push("Confirmation: install the pinned Antigravity community ACP bridge v1.0.0? This bridge is not provided or endorsed by Google. Its maintainer warns that third-party Antigravity OAuth use may violate Google's Terms and risk account suspension. Codypendent verifies the published SHA-256 and does not store Antigravity credentials.".to_owned());
        }
        Overlay::CouncilBrowser => {
            lines.push(format!(
                "Agent councils: {} configured",
                state.councils.len()
            ));
            if let Some(council) = state.focused_council() {
                lines.push(format!(
                    "Focused council: {}; chair {}; {} round(s); {} member(s){}",
                    clean(&council.name),
                    clean(&council.chair),
                    council.rounds,
                    council.members.len(),
                    if council.evidence {
                        "; evidence mode"
                    } else {
                        ""
                    }
                ));
            }
        }
        Overlay::CouncilResults => {
            lines.push(format!(
                "Council results: {} durable outcome(s).",
                state.council_results.len()
            ));
            if let Some(result) = state.focused_council_result() {
                lines.push(format!(
                    "{} council result {}; handle {}; started {}; finished {}; repository {}; origin session {}.",
                    clean(&result.status), clean(&result.council), clean(&result.result_id),
                    clean(&result.started_at), clean(&result.finished_at), clean(&result.repository),
                    clean(result.origin_session_id.as_deref().unwrap_or("none")),
                ));
                lines.push(format!("Objective: {}", clean(&result.objective)));
                lines.push(format!("Chair synthesis: {}", clean(&result.synthesis)));
                for warning in &result.warnings {
                    lines.push(format!("Warning: {}", clean(warning)));
                }
                if state.council_result_expanded {
                    for round in &result.rounds {
                        lines.push(format!("Round {}.", round.round));
                        for member in &round.members {
                            lines.push(format!(
                                "Member {} using {}; session {}; run {}; report: {}",
                                clean(&member.role), clean(&member.model), clean(&member.session_id),
                                clean(&member.run_id), clean(&member.response)
                            ));
                        }
                        for failure in &round.failures {
                            lines.push(format!("Member failure: {}", clean(failure)));
                        }
                    }
                }
                lines.push("Press Enter to expand member reports; type copy to copy the full chair synthesis; Escape closes.".to_owned());
            }
        }
        Overlay::Journey => {
            lines.push(format!("Learning journey: {} useful curated entries; {} awaiting review.", state.learnings.len(), state.pending_learning_review));
            let rows = state.learnings.iter().map(|card| format!(
                "{}; {}; {}; scope {}; confidence {:.2}; provenance {}{}",
                clean(&card.statement), clean(&card.state), clean(&card.kind), clean(&card.scope),
                card.confidence, clean(&card.provenance), if card.pinned { "; pinned" } else { "" }
            )).collect();
            append_picker_rows(lines, "learning", rows, state.selected_learning);
            lines.push("Learning controls: a activates; r rejects; p pins or unpins; e edits; d asks before permanent deletion; Escape closes. Raw logs, tool output, URLs, and secrets are not exposed.".to_owned());
        }
        Overlay::LearningEdit { buffer, .. } => {
            lines.push(format!("Edit curated learning: {}", clean(buffer)));
            lines.push("Edit controls: Enter saves through learning policy; Escape returns without changing it.".to_owned());
        }
        Overlay::ConfirmLearningDelete { label, .. } => {
            lines.push(format!("Permanently delete learning: {}. The learning store has no undo.", clean(label)));
            lines.push("Delete controls: Enter or y confirms; Escape cancels.".to_owned());
        }
        Overlay::CouncilBuilder(builder) => {
            lines.push(format!(
                "Council builder: step {:?}; name {}; {} member(s); chair {}; rounds {}",
                builder.step,
                clean(&builder.name),
                builder.members.len(),
                clean(builder.chair.as_deref().unwrap_or("not selected")),
                builder.rounds
            ));
            for member in &builder.members {
                lines.push(format!(
                    "Council member: {}; role {}",
                    clean(&member.model),
                    clean(&member.role)
                ));
            }
            match builder.step {
                CouncilBuilderStep::MemberModel => {
                    let continue_row =
                        builder.members.len() >= 2 && builder.query.trim().is_empty();
                    let remove_row = !builder.members.is_empty() && builder.query.trim().is_empty();
                    let mut rows = Vec::new();
                    if continue_row {
                        rows.push(format!("Continue with {} members", builder.members.len()));
                    }
                    if builder.members.len() < 8 {
                        rows.extend(
                            filter_council_member_models(
                                &state.models,
                                &builder.query,
                                &builder.members,
                            )
                            .into_iter()
                            .filter_map(|index| state.models.get(index))
                            .map(|card| {
                                format!(
                                    "{}; provider {}; {}",
                                    clean(&card.id.0),
                                    clean(&card.provider),
                                    model_readiness_label(&card.readiness)
                                )
                            }),
                        );
                    }
                    if remove_row {
                        let model = builder
                            .members
                            .last()
                            .map_or("member", |member| member.model.as_str());
                        rows.push(format!("Remove last member; {}", clean(model)));
                    }
                    append_picker_rows(lines, "council member choice", rows, builder.selected);
                }
                CouncilBuilderStep::Chair => {
                    let rows = filter_models(&state.models, &builder.query)
                        .into_iter()
                        .filter_map(|index| state.models.get(index))
                        .map(|card| {
                            format!(
                                "{}; provider {}; {}",
                                clean(&card.id.0),
                                clean(&card.provider),
                                model_readiness_label(&card.readiness)
                            )
                        })
                        .collect();
                    append_picker_rows(lines, "council chair", rows, builder.selected);
                }
                CouncilBuilderStep::Name
                | CouncilBuilderStep::Description
                | CouncilBuilderStep::MemberRole
                | CouncilBuilderStep::Rounds
                | CouncilBuilderStep::Review => {}
            }
        }
        Overlay::SessionLibrary {
            query,
            selected,
            waiting,
        } => {
            lines.push(format!(
                "Session library: query {}; {}",
                if query.is_empty() {
                    "empty, showing the most recent sessions".to_owned()
                } else {
                    clean(query)
                },
                if *waiting {
                    "searching".to_owned()
                } else {
                    format!("{} ranked result(s)", state.session_library.len())
                }
            ));
            let rows = state
                .session_library
                .iter()
                .map(|row| {
                    // Pin/archive are read from the row's own daemon-supplied
                    // flags, and an absent excerpt is simply not spoken.
                    let mut text = format!(
                        "{}; {}; updated {}",
                        clean(&row.title),
                        clean(&row.state),
                        clean(&row.updated_at)
                    );
                    if row.pinned {
                        text.push_str("; pinned");
                    }
                    if row.archived {
                        text.push_str("; archived");
                    }
                    if let Some(excerpt) = row.excerpt.as_deref() {
                        text.push_str(&format!("; match {}", clean(excerpt)));
                    }
                    text
                })
                .collect();
            append_picker_rows(lines, "session library", rows, *selected);
            lines.push("Session library controls: type TEXT searches every session; up and down select and load more; Enter resumes; Alt-P pins or unpins; Alt-A archives or restores; Alt-R renames; Alt-E exports; Ctrl-D asks before deleting; Escape closes.".to_owned());
        }
        Overlay::SessionRename { buffer, .. } => {
            lines.push(format!("Rename session: {}", clean(buffer)));
            lines.push("Rename controls: Enter submits a non-empty single-line title; Escape returns to the session library without changing it.".to_owned());
        }
        Overlay::ConfirmSessionDelete { title, .. } => {
            lines.push(format!(
                "Delete session: {}. The daemon applies its retention policy and may tombstone rather than purge. This cannot be undone from the client.",
                clean(title)
            ));
            lines.push("Delete controls: Enter or y confirms; Escape cancels.".to_owned());
        }
        other => lines.push(format!("Open dialog: {}", overlay_name(other))),
    }
}

/// Announce both the exact highlighted choice and a bounded neighbourhood of
/// useful rows. Cooked-terminal users receive a fresh snapshot after every
/// navigation action, so a window around the cursor is more usable than
/// dumping an arbitrarily large provider/model catalogue each time.
fn append_picker_rows(lines: &mut Vec<String>, kind: &str, rows: Vec<String>, selected: usize) {
    const WINDOW: usize = 9;

    let Some(highlighted) = rows.get(selected) else {
        lines.push(format!("Available {kind} rows: none"));
        return;
    };
    lines.push(format!(
        "Highlighted {kind} {} of {}: {highlighted}",
        selected + 1,
        rows.len()
    ));

    let start = selected
        .saturating_sub(WINDOW / 2)
        .min(rows.len().saturating_sub(WINDOW));
    let end = (start + WINDOW).min(rows.len());
    lines.push(format!(
        "Available {kind} rows {} through {} of {}:",
        start + 1,
        end,
        rows.len()
    ));
    for (index, row) in rows.iter().enumerate().take(end).skip(start) {
        lines.push(format!(
            "  {}{}: {}",
            index + 1,
            if index == selected {
                " highlighted"
            } else {
                ""
            },
            row
        ));
    }
}

fn model_readiness_label(readiness: &ModelReadiness) -> &'static str {
    match readiness {
        ModelReadiness::Ready => "ready",
        ModelReadiness::Unverified => "unverified",
        ModelReadiness::Unavailable(_) => "unavailable",
    }
}

fn overlay_name(overlay: &Overlay) -> &'static str {
    match overlay {
        Overlay::Onboard { .. } => "guided model setup",
        Overlay::OnboardProviderPicker { .. } => "guided provider picker",
        Overlay::Issues => "setup and diagnostics",
        Overlay::Skills => "skills",
        Overlay::Memory { .. } => "memory",
        Overlay::Journey => "learning journey",
        Overlay::LearningEdit { .. } => "edit curated learning",
        Overlay::ConfirmLearningDelete { .. } => "delete learning confirmation",
        Overlay::Docs => "documents",
        Overlay::Edges | Overlay::EdgeSearch(_) => "code graph",
        Overlay::Workflow | Overlay::WorkflowInputs { .. } => "workflow",
        Overlay::KanbanNew { .. } => "new Kanban task",
        Overlay::BlackboardPost { .. } => "Blackboard post",
        Overlay::Blackboard => "blackboard",
        Overlay::Kanban => "task board",
        Overlay::UiPlugins => "Remote UI plugins",
        Overlay::ThemePicker { .. } => "theme picker",
        Overlay::SessionPicker { .. } => "resume session",
        Overlay::SessionLibrary { .. } => "session library",
        Overlay::SessionRename { .. } => "rename session",
        Overlay::ConfirmSessionDelete { .. } => "delete session confirmation",
        Overlay::ApiKeys { .. } => "API keys",
        Overlay::ApiKeySet { .. } => "API key entry",
        Overlay::ApiKeyRemoveConfirm { .. } => "remove API key confirmation",
        Overlay::CouncilBuilder(_) => "council builder",
        Overlay::CouncilBrowser => "agent councils",
        Overlay::CouncilResults => "council results",
        Overlay::CouncilRunObjective { .. } => "council objective",
        Overlay::ConfirmCouncilDelete { .. } => "remove council confirmation",
        Overlay::ConfirmModelRemove { .. } => "remove model confirmation",
        Overlay::ConfirmCommunityAcpInstall { .. } => "Antigravity community bridge confirmation",
        Overlay::Backtrack(_) => "backtrack session fork",
        Overlay::Context => "context usage breakdown",
        Overlay::AddModelId { .. }
        | Overlay::AddModelKey { .. }
        | Overlay::AddModelProviderKey { .. }
        | Overlay::AddModelQuerying { .. }
        | Overlay::AddModelPick { .. } => "add model",
        Overlay::UnslothRepos { .. }
        | Overlay::UnslothQuants { .. }
        | Overlay::UnslothConfirmPull { .. }
        | Overlay::UnslothPulling { .. } => "local models: unsloth catalog",
        Overlay::DocEdit { .. } | Overlay::DocInsert { .. } => "document editor",
        Overlay::DocNew { .. } => "new document",
        Overlay::DocDeleteConfirm { .. } => "delete block confirmation",
        Overlay::DocPublishTarget { .. } => "document publish target",
        Overlay::DocPublishPath { .. } => "document publish path",
        Overlay::DocPublishBranch { .. } => "document publish branch",
        Overlay::DocPublishTitle { .. } => "document publish pull request title",
        Overlay::ConfirmUiPluginApprove { .. }
        | Overlay::ConfirmUiPluginReject { .. }
        | Overlay::ConfirmUiPluginRevoke { .. }
        | Overlay::ConfirmUiPluginEnable { .. } => "Remote UI plugin confirmation",
        Overlay::Help
        | Overlay::None
        | Overlay::NewRun(_)
        | Overlay::Steering(_)
        | Overlay::ConfirmCancel { .. }
        | Overlay::ConfirmWorkflowCancel { .. }
        | Overlay::Palette { .. }
        | Overlay::ModelPicker { .. }
        | Overlay::ProviderPicker { .. }
        | Overlay::ModePicker { .. } => "dialog",
    }
}

fn onboard_class_accessible_label(class: OnboardProviderClass) -> &'static str {
    match class {
        OnboardProviderClass::Hosted => "hosted API",
        OnboardProviderClass::LocalEndpoint => "local endpoint",
        OnboardProviderClass::AcpAgent => "ACP coding agent",
    }
}

fn controls_for(mode: InputMode) -> &'static str {
    match mode {
        InputMode::Help => "Help controls: type TEXT to filter; backspace, clear, up, down, pageup, pagedown, home, end; esc closes; quit detaches.",
        InputMode::Composer => {
            "Controls: type a message and press Enter, Home/End move within a line, or use help, /, F6, Shift-F6, quit"
        }
        InputMode::RemoteUi => {
            "Controls: Tab or backtab move focus, Enter activates, type TEXT edits, Shift-F6 changes document, Esc returns"
        }
        InputMode::Palette => {
            "Controls: type TEXT filters, up/down/pageup/pagedown/home/end select, Enter chooses, Delete removes a saved model/key, Esc closes"
        }
        InputMode::Editing => "Controls: type TEXT, Enter submits, Esc cancels",
        InputMode::Confirm => "Controls: yes or Enter confirms, no or Esc cancels",
        InputMode::Approval => {
            "Controls: approve, approve-run, approve-pattern, approve-repository, reject, up, down, pageup, pagedown"
        }
        InputMode::CouncilResults => {
            "Controls: up/down select a result, pageup/pagedown scroll its synthesis, Enter toggles member reports, copy copies the synthesis, Esc closes"
        }
        InputMode::Question => {
            "Controls: up, down, 1-9 to pick, space to toggle, Enter to select, type TEXT for a custom answer, reject, Esc to cancel"
        }
        InputMode::Normal => {
            "Controls: up, down, Enter, Esc, help, quit, new, steer, pause, cancel-run, skills, memory, journey, docs, edges, workflow, blackboard, board, councils"
        }
    }
}

/// Convert one cooked input line into semantic actions. Commands are ASCII and
/// case-insensitive. In the base composer an otherwise-unrecognised line is sent
/// as the user's message; in an editor/filter/Remote UI field it is inserted.
#[must_use]
pub fn map_accessible_input(line: &str, mode: InputMode) -> Vec<Action> {
    let line = line.trim_end_matches(['\r', '\n']);
    let command = line.trim();
    let lower = command.to_ascii_lowercase();
    match lower.as_str() {
        "" => return vec![submit_action(mode)],
        "help" | "?" => return vec![Action::Help],
        "quit" | "exit" => return vec![Action::Detach],
        "/" | "commands" => return vec![Action::OpenPalette],
        "f6" => return vec![Action::RemoteUiSetActive(true)],
        "shift-f6" | "next-document" => return vec![Action::RemoteUiNextDocument],
        "cancel" | "cancel-run" if mode == InputMode::Normal => return vec![Action::Cancel],
        "esc" | "escape" | "cancel" => return vec![cancel_action(mode)],
        "enter" | "choose" => return vec![submit_action(mode)],
        // "yes" confirms only where there is something to confirm. In a mode
        // that takes free text it is an ordinary word — and one an operator
        // plainly wants to send to an agent — so treating it as a keypress
        // everywhere meant a screen-reader user could not answer "yes" at all:
        // it submitted the composer's draft (usually empty) instead, and
        // silently, since an empty submit does nothing.
        "yes" if !accepts_text(mode) => return vec![submit_action(mode)],
        // The counterpart, and the one `controls_for(InputMode::Confirm)` has
        // always promised: "yes or Enter confirms, no or Esc cancels". "no" was
        // never in this map, so it fell through to "unrecognised accessible
        // command" — the client told an operator which word to use and then
        // refused it. Same text-mode condition as "yes": in the composer it is
        // an ordinary word.
        "no" if !accepts_text(mode) => return vec![cancel_action(mode)],
        "new" | "create" | "run" | "post" if mode == InputMode::Normal => {
            return vec![Action::NewRun];
        }
        "steer" if mode == InputMode::Normal => return vec![Action::Steer],
        "pause" | "resume" | "pause-resume" if mode == InputMode::Normal => {
            return vec![Action::Pause];
        }
        "skills" if mode == InputMode::Normal => return vec![Action::OpenSkills],
        "memory" if mode == InputMode::Normal => return vec![Action::OpenMemory],
        "journey" if mode == InputMode::Normal => return vec![Action::OpenJourney],
        "docs" if mode == InputMode::Normal => return vec![Action::OpenDocs],
        "edges" | "graph" if mode == InputMode::Normal => return vec![Action::OpenEdges],
        "workflow" if mode == InputMode::Normal => return vec![Action::OpenWorkflow],
        "blackboard" if mode == InputMode::Normal => return vec![Action::OpenBlackboard],
        "board" | "kanban" if mode == InputMode::Normal => return vec![Action::OpenKanban],
        "council" | "councils" if mode == InputMode::Normal => {
            return vec![Action::OpenCouncils];
        }
        "space" if mode == InputMode::RemoteUi => {
            return vec![Action::RemoteUiKey {
                key: RemoteKey::Space,
                character: Some(' '),
            }];
        }
        // The question card's multi-select toggle — `controls_for(Question)`
        // has always promised it, but it only ever reached Remote UI above.
        "space" if mode == InputMode::Question => {
            return vec![Action::QuestionToggleOption];
        }
        "tab" => return vec![tab_action(mode, false)],
        "backtab" | "shift-tab" => return vec![tab_action(mode, true)],
        "clear" if mode == InputMode::Help => return vec![Action::DeleteToLineStart],
        "up" => return vec![navigation_action(mode, true)],
        "down" => return vec![navigation_action(mode, false)],
        "pageup" | "page-up" => return vec![page_action(mode, true)],
        "pagedown" | "page-down" => return vec![page_action(mode, false)],
        "home" => return vec![edge_action(mode, true)],
        "end" => return vec![edge_action(mode, false)],
        "alt-up" | "previous-card" => return vec![Action::BrowseFoldPrev],
        "alt-down" | "next-card" => return vec![Action::BrowseFoldNext],
        "alt-enter" | "expand-card" => return vec![Action::Expand],
        "alt-y" | "copy-card" => return vec![Action::CopyFocusedCard],
        "copy" | "copy-result" => return vec![Action::CopyFocusedCard],
        "alt-r" | "retry-card" => return vec![Action::RetryFailedRun],
        "alt-a" | "reauthenticate" => return vec![Action::ReauthenticateFailedModel],
        "alt-m" | "choose-model" => return vec![Action::ChooseFailureModel],
        "diagnostics" => return vec![Action::OpenIssues],
        "alt-d" | "disable-model" => return vec![Action::DisableFailureModel],
        "delete" => {
            return vec![match mode {
                InputMode::RemoteUi => Action::RemoteUiKey {
                    key: RemoteKey::Delete,
                    character: None,
                },
                InputMode::Palette => Action::RemoveSelected,
                InputMode::Normal => Action::ClearIssues,
                InputMode::CouncilResults | InputMode::Help => Action::NoOp,
                InputMode::Composer
                | InputMode::Editing
                | InputMode::Confirm
                | InputMode::Approval
                | InputMode::Question => Action::NoOp,
            }];
        }
        "approve" => return vec![Action::Approve(ApprovalScope::Once)],
        "approve-run" => return vec![Action::Approve(ApprovalScope::Run)],
        // The two always-allow scopes the graphical modal has offered as
        // `p`/`P` all along — previously unreachable from a screen reader.
        "approve-pattern" => return vec![Action::Approve(ApprovalScope::Pattern)],
        "approve-repository" | "approve-repo" => {
            return vec![Action::Approve(ApprovalScope::Repository)];
        }
        // On a question card, "reject" opens the feedback box (the graphical
        // `r`); everywhere else it resolves the pending approval.
        "reject" if mode == InputMode::Question => {
            return vec![Action::QuestionOpenReject];
        }
        "reject" => return vec![Action::Reject],
        "backspace" => {
            return vec![match mode {
                InputMode::RemoteUi => Action::RemoteUiKey {
                    key: RemoteKey::Backspace,
                    character: None,
                },
                InputMode::Composer | InputMode::Editing | InputMode::Palette | InputMode::Help => {
                    Action::InputBackspace
                }
                InputMode::Question => Action::QuestionInputBackspace,
                InputMode::Normal | InputMode::Confirm | InputMode::Approval => Action::NoOp,
                InputMode::CouncilResults => Action::NoOp,
            }];
        }
        _ => {
            // `1`..`9` pick a question option directly, as promised by
            // `controls_for(Question)` and mirrored from the raw-mode digits.
            if mode == InputMode::Question && command.len() == 1 {
                if let Some(digit) = command.chars().next().and_then(|c| c.to_digit(10)) {
                    if (1..=9).contains(&digit) {
                        return vec![Action::QuestionPickDigit(digit as usize)];
                    }
                }
            }
        }
    }

    if lower.starts_with("send ") {
        if !accepts_text(mode) {
            return vec![unsupported_command()];
        }
        let text = &command["send ".len()..];
        if mode == InputMode::Question {
            let mut actions = question_text_actions(text);
            actions.push(submit_action(mode));
            return actions;
        }
        return vec![paste_action(mode, text.to_owned()), submit_action(mode)];
    }
    if lower.starts_with("type ") {
        if !accepts_text(mode) {
            return vec![unsupported_command()];
        }
        let text = &command["type ".len()..];
        if mode == InputMode::Question {
            return question_text_actions(text);
        }
        return vec![paste_action(mode, text.to_owned())];
    }
    match mode {
        InputMode::Composer => vec![Action::InputPaste(line.to_owned()), Action::InputSubmit],
        InputMode::RemoteUi | InputMode::Editing | InputMode::Palette => {
            vec![paste_action(mode, line.to_owned())]
        }
        // A bare line on a question card is the custom answer / feedback
        // text — the very thing `accepts_text` now admits for this mode.
        InputMode::Question => question_text_actions(line),
        _ => vec![unsupported_command()],
    }
}

/// The question card has no paste path: text reaches it one
/// [`Action::QuestionInputChar`] at a time, exactly as raw-mode typing does.
fn question_text_actions(text: &str) -> Vec<Action> {
    text.chars().map(Action::QuestionInputChar).collect()
}

fn accepts_text(mode: InputMode) -> bool {
    matches!(
        mode,
        InputMode::Composer
            | InputMode::RemoteUi
            | InputMode::Editing
            | InputMode::Palette
            | InputMode::Help
            // The custom-answer and reject-feedback fields: `type ...` routes
            // per-character through `QuestionInputChar` (see
            // `map_accessible_input`'s text paths), so a screen-reader user
            // can actually answer.
            | InputMode::Question
    )
}

fn unsupported_command() -> Action {
    Action::Notice("unrecognised accessible command; type help for commands".to_owned())
}

fn submit_action(mode: InputMode) -> Action {
    match mode {
        InputMode::RemoteUi => Action::RemoteUiKey {
            key: RemoteKey::Enter,
            character: None,
        },
        InputMode::Confirm => Action::ConfirmCancel,
        InputMode::Approval | InputMode::Help => Action::NoOp,
        InputMode::Normal | InputMode::CouncilResults => Action::Expand,
        InputMode::Question => Action::QuestionSelectOrConfirm,
        _ => Action::InputSubmit,
    }
}

fn cancel_action(mode: InputMode) -> Action {
    match mode {
        InputMode::RemoteUi => Action::RemoteUiSetActive(false),
        InputMode::Editing | InputMode::Palette | InputMode::Composer => Action::InputCancel,
        InputMode::Approval => Action::NoOp,
        // Steps back out of the feedback/custom field first, exactly like the
        // raw-mode Esc.
        InputMode::Question => Action::QuestionCancelReject,
        _ => Action::Dismiss,
    }
}

fn tab_action(mode: InputMode, reverse: bool) -> Action {
    if mode == InputMode::RemoteUi {
        Action::RemoteUiKey {
            key: if reverse {
                RemoteKey::ShiftTab
            } else {
                RemoteKey::Tab
            },
            character: None,
        }
    } else if reverse {
        Action::NoOp
    } else if matches!(mode, InputMode::Palette | InputMode::Editing) {
        Action::BeginAddModel
    } else if matches!(mode, InputMode::Normal | InputMode::Composer) {
        Action::CyclePane
    } else {
        Action::NoOp
    }
}

fn navigation_action(mode: InputMode, previous: bool) -> Action {
    match mode {
        InputMode::RemoteUi => Action::RemoteUiKey {
            key: if previous {
                RemoteKey::Up
            } else {
                RemoteKey::Down
            },
            character: None,
        },
        InputMode::Composer => {
            if previous {
                Action::HistoryPrev
            } else {
                Action::HistoryNext
            }
        }
        InputMode::Help => {
            if previous {
                Action::ScrollLinesUp
            } else {
                Action::ScrollLinesDown
            }
        }
        InputMode::Normal
        | InputMode::Palette
        | InputMode::Approval
        | InputMode::CouncilResults => {
            if previous {
                Action::SelectPrev
            } else {
                Action::SelectNext
            }
        }
        InputMode::Question => {
            if previous {
                Action::QuestionNavigate(-1)
            } else {
                Action::QuestionNavigate(1)
            }
        }
        InputMode::Editing | InputMode::Confirm => Action::NoOp,
    }
}

fn page_action(mode: InputMode, previous: bool) -> Action {
    match mode {
        InputMode::RemoteUi => Action::RemoteUiKey {
            key: if previous {
                RemoteKey::PageUp
            } else {
                RemoteKey::PageDown
            },
            character: None,
        },
        InputMode::Palette | InputMode::Approval => {
            if previous {
                Action::SelectPagePrev
            } else {
                Action::SelectPageNext
            }
        }
        InputMode::Composer | InputMode::Normal | InputMode::CouncilResults | InputMode::Help => {
            if previous {
                Action::ScrollPageUp
            } else {
                Action::ScrollPageDown
            }
        }
        InputMode::Editing | InputMode::Confirm | InputMode::Question => Action::NoOp,
    }
}

fn edge_action(mode: InputMode, start: bool) -> Action {
    match mode {
        InputMode::RemoteUi => Action::RemoteUiKey {
            key: if start {
                RemoteKey::Home
            } else {
                RemoteKey::End
            },
            character: None,
        },
        InputMode::Palette | InputMode::Help => {
            if start {
                Action::SelectFirst
            } else {
                Action::SelectLast
            }
        }
        InputMode::Composer => {
            if start {
                Action::CursorLineStart
            } else {
                Action::CursorLineEnd
            }
        }
        _ => Action::NoOp,
    }
}

fn paste_action(mode: InputMode, text: String) -> Action {
    if mode == InputMode::RemoteUi {
        Action::RemoteUiPaste(text)
    } else {
        Action::InputPaste(text)
    }
}

fn ascii_keys(keys: &str) -> String {
    ascii_chrome(keys)
}

fn ascii_chrome(text: &str) -> String {
    clean(text)
        .replace('↑', "Up")
        .replace('↓', "Down")
        .replace('⇄', "<->")
        .replace('…', "...")
        .replace('·', "/")
        .replace(['—', '–'], "-")
        .replace('→', "->")
        .replace('←', "<-")
        .replace('⌥', "Alt-")
        .replace('⌫', "Backspace")
}

/// Remove terminal control sequences and Unicode bidi controls from text that
/// will be written to the cooked accessibility stream.
#[must_use]
pub fn sanitize_accessible_text(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        // Strip complete ANSI CSI/OSC sequences, rather than only their ESC
        // introducer (which would leave distracting `[31m` debris in a screen
        // reader). C1 equivalents are treated identically.
        if character == '\u{1b}' {
            match characters.next() {
                Some('[') => {
                    for parameter in characters.by_ref() {
                        if ('@'..='~').contains(&parameter) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(parameter) = characters.next() {
                        if parameter == '\u{7}' {
                            break;
                        }
                        if parameter == '\u{1b}' && characters.peek() == Some(&'\\') {
                            characters.next();
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            }
            continue;
        }
        if character == '\u{9b}' {
            for parameter in characters.by_ref() {
                if ('@'..='~').contains(&parameter) {
                    break;
                }
            }
            continue;
        }
        if character == '\u{9d}' {
            while let Some(parameter) = characters.next() {
                if parameter == '\u{7}' {
                    break;
                }
                if parameter == '\u{1b}' && characters.peek() == Some(&'\\') {
                    characters.next();
                    break;
                }
            }
            continue;
        }
        if matches!(character, '\n' | '\t')
            || (!character.is_control()
                && !matches!(
                    character as u32,
                    0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069
                ))
        {
            cleaned.push(character);
        }
    }
    cleaned
}

fn clean(text: &str) -> String {
    sanitize_accessible_text(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        CouncilBuilderState, CouncilBuilderStep, CouncilMemberDraft, ModelCard, ProviderCard,
    };
    use codypendent_protocol::{
        Actor, EventBody, ModelId, RunId, SessionEvent, UiContributionId, UiContributionPoint,
        UiContributionRegistration, UiDocument, UiDocumentId, UiExtensionId, UiSlotId,
    };

    fn picker_model(id: &str, provider: &str) -> ModelCard {
        ModelCard {
            id: ModelId(id.to_owned()),
            provider: provider.to_owned(),
            readiness: ModelReadiness::Ready,
            location: None,
            cost_per_1k_usd: None,
            context_tokens: None,
        }
    }

    fn picker_provider(id: &str, name: &str, protocol: &str, available: bool) -> ProviderCard {
        ProviderCard {
            id: id.to_owned(),
            name: name.to_owned(),
            protocol: protocol.to_owned(),
            auth: "none".to_owned(),
            local: false,
            requires_key: false,
            can_list_models: false,
            available,
            catalog_models: 0,
            has_key: false,
        }
    }

    #[test]
    fn composer_lines_send_and_remote_lines_edit_without_activation() {
        assert_eq!(
            map_accessible_input("build it", InputMode::Composer),
            vec![
                Action::InputPaste("build it".to_owned()),
                Action::InputSubmit
            ]
        );
        assert_eq!(
            map_accessible_input("type query", InputMode::RemoteUi),
            vec![Action::RemoteUiPaste("query".to_owned())]
        );
        assert_eq!(
            map_accessible_input("shift-f6", InputMode::RemoteUi),
            vec![Action::RemoteUiNextDocument]
        );
    }

    #[test]
    fn cancel_confirmation_announces_the_pinned_target_and_stale_state() {
        let mut state = AppState::new();
        let original = RunId::new();
        state
            .ensure_run(
                original,
                "original reviewed objective".to_owned(),
                codypendent_protocol::AgentMode::Build,
            )
            .state = codypendent_protocol::RunState::Running;
        state.overlay = Overlay::ConfirmCancel { run_id: original };
        state.ensure_run(
            RunId::new(),
            "another selected objective".to_owned(),
            codypendent_protocol::AgentMode::Plan,
        );
        let snapshot = accessible_snapshot(&state);
        let confirmation = snapshot
            .lines()
            .find(|line| line.starts_with("Confirmation:"))
            .unwrap();
        assert!(
            confirmation.contains(&original.to_string()),
            "{confirmation}"
        );
        assert!(
            confirmation.contains("original reviewed objective"),
            "{confirmation}"
        );
        assert!(
            confirmation.contains("Mode: Build. State: Running."),
            "{confirmation}"
        );
        assert!(
            !confirmation.contains("another selected objective"),
            "{confirmation}"
        );

        state.runs[0].state = codypendent_protocol::RunState::Completed;
        let finished = accessible_snapshot(&state);
        assert!(finished.contains("already finished; no cancellation will be sent"));
        state.runs.remove(0);
        let missing = accessible_snapshot(&state);
        assert!(missing.contains(&format!(
            "run {original} is no longer available; no cancellation will be sent"
        )));
    }

    #[test]
    fn cooked_navigation_matches_graphical_modes() {
        assert_eq!(
            map_accessible_input("tab", InputMode::Editing),
            vec![Action::BeginAddModel]
        );
        assert_eq!(
            map_accessible_input("pageup", InputMode::Palette),
            vec![Action::SelectPagePrev]
        );
        assert_eq!(
            map_accessible_input("pagedown", InputMode::Palette),
            vec![Action::SelectPageNext]
        );
        assert_eq!(
            map_accessible_input("home", InputMode::Palette),
            vec![Action::SelectFirst]
        );
        assert_eq!(
            map_accessible_input("end", InputMode::Composer),
            vec![Action::CursorLineEnd]
        );
        assert_eq!(
            map_accessible_input("home", InputMode::RemoteUi),
            vec![Action::RemoteUiKey {
                key: RemoteKey::Home,
                character: None,
            }]
        );
        assert_eq!(
            map_accessible_input("delete", InputMode::Palette),
            vec![Action::RemoveSelected]
        );
        assert_eq!(
            map_accessible_input("delete", InputMode::RemoteUi),
            vec![Action::RemoteUiKey {
                key: RemoteKey::Delete,
                character: None,
            }]
        );
    }

    /// Every control word the client announces must be a word it accepts.
    ///
    /// `controls_for(InputMode::Confirm)` has always read "yes or Enter
    /// confirms, no or Esc cancels", but "no" was never in the input map: it
    /// fell through to "unrecognised accessible command". The client named the
    /// word and then refused it, which for a screen-reader user is the whole
    /// interface for that dialog.
    #[test]
    fn a_confirmation_accepts_both_words_it_announces() {
        let announced = controls_for(InputMode::Confirm);
        assert!(
            announced.contains("yes") && announced.contains("no"),
            "the announcement under test changed: {announced}"
        );
        assert_eq!(
            map_accessible_input("yes", InputMode::Confirm),
            vec![Action::ConfirmCancel],
            "the announced confirming word must confirm"
        );
        assert_eq!(
            map_accessible_input("no", InputMode::Confirm),
            vec![Action::Dismiss],
            "the announced cancelling word must cancel, not be refused"
        );
        // Case and surrounding space are how a person types, not a different
        // command.
        assert_eq!(
            map_accessible_input("  No  ", InputMode::Confirm),
            vec![Action::Dismiss],
        );
    }

    /// "yes" and "no" are ordinary words wherever text is accepted.
    ///
    /// Reserving them as keypresses everywhere meant an operator on the cooked
    /// client could not answer an agent's question with "yes": it submitted the
    /// composer's draft instead, which is normally empty, so nothing happened
    /// and nothing said why.
    #[test]
    fn yes_and_no_are_sendable_text_in_the_composer() {
        assert_eq!(
            map_accessible_input("yes", InputMode::Composer),
            vec![Action::InputPaste("yes".to_owned()), Action::InputSubmit],
            "in the composer, `yes` is a message"
        );
        assert_eq!(
            map_accessible_input("no", InputMode::Composer),
            vec![Action::InputPaste("no".to_owned()), Action::InputSubmit],
        );
        // The explicit key names stay keys everywhere: nobody types "enter"
        // meaning to send the word.
        assert_eq!(
            map_accessible_input("enter", InputMode::Composer),
            vec![Action::InputSubmit],
        );
    }

    #[test]
    fn cooked_normal_mode_reaches_the_run_and_workspace_commands_it_announces() {
        for (command, expected) in [
            ("cancel-run", Action::Cancel),
            ("steer", Action::Steer),
            ("pause", Action::Pause),
            ("skills", Action::OpenSkills),
            ("memory", Action::OpenMemory),
            ("journey", Action::OpenJourney),
            ("docs", Action::OpenDocs),
            ("edges", Action::OpenEdges),
            ("workflow", Action::OpenWorkflow),
            ("blackboard", Action::OpenBlackboard),
            ("board", Action::OpenKanban),
            ("councils", Action::OpenCouncils),
        ] {
            assert_eq!(
                map_accessible_input(command, InputMode::Normal),
                vec![expected],
                "accessible command {command} must match its keyboard action"
            );
        }
        assert!(controls_for(InputMode::Normal).contains("cancel-run"));
    }

    #[test]
    fn cooked_onboarding_announces_routes_scope_and_keyboard_controls() {
        let mut state = AppState::new();
        state.overlay = Overlay::Onboard {
            step: OnboardStep::Triage { selected: 2 },
        };
        let triage = accessible_snapshot(&state);
        assert!(triage.contains("Highlighted connection route 3 of 3: ACP coding agent"));
        assert!(triage.contains("Enter continues, Esc opens skip choices"));
        assert_eq!(
            map_accessible_input("up", state.input_mode()),
            vec![Action::SelectPrev]
        );
        assert_eq!(
            map_accessible_input("enter", state.input_mode()),
            vec![Action::InputSubmit]
        );

        state.providers = vec![
            ProviderCard {
                id: "openai".to_owned(),
                name: "OpenAI".to_owned(),
                protocol: "openai-chat".to_owned(),
                auth: "api-key: OPENAI_API_KEY".to_owned(),
                local: false,
                requires_key: true,
                can_list_models: true,
                available: true,
                catalog_models: 3,
                has_key: false,
            },
            ProviderCard {
                id: "kimi-code".to_owned(),
                name: "Kimi Code".to_owned(),
                protocol: "acp".to_owned(),
                auth: "acp: installed executable".to_owned(),
                local: true,
                requires_key: false,
                can_list_models: false,
                available: true,
                catalog_models: 0,
                has_key: false,
            },
        ];
        state.overlay = Overlay::OnboardProviderPicker {
            class: OnboardProviderClass::Hosted,
            query: String::new(),
            selected: 0,
        };
        let providers = accessible_snapshot(&state);
        assert!(providers.contains("Highlighted setup provider 1 of 1: OpenAI"));
        assert!(!providers.contains("Kimi Code"));
        assert!(providers.contains("Enter opens model discovery"));
    }

    #[test]
    fn cooked_empty_chat_distinguishes_configured_but_unrunnable_models() {
        let mut state = AppState::new();
        state.models = vec![ModelCard {
            id: ModelId("saved-but-unavailable".to_owned()),
            provider: "openai-compatible".to_owned(),
            readiness: ModelReadiness::Unavailable("missing key".to_owned()),
            location: None,
            cost_per_1k_usd: None,
            context_tokens: None,
        }];
        let snapshot = accessible_snapshot(&state);
        assert!(snapshot.contains("saved models exist, but none is runnable"));
        assert!(snapshot.contains("submit an empty composer to open guided model setup"));
    }

    #[test]
    fn cooked_modal_commands_do_not_mutate_hidden_state() {
        let no_op = vec![Action::NoOp];
        let unsupported = vec![Action::Notice(
            "unrecognised accessible command; type help for commands".to_owned(),
        )];

        assert_eq!(
            map_accessible_input("pageup", InputMode::Approval),
            vec![Action::SelectPagePrev]
        );
        assert_eq!(
            map_accessible_input("pagedown", InputMode::Approval),
            vec![Action::SelectPageNext]
        );
        for mode in [InputMode::Editing, InputMode::Confirm] {
            assert_eq!(map_accessible_input("pageup", mode), no_op);
            assert_eq!(map_accessible_input("up", mode), no_op);
        }
        for mode in [InputMode::Approval, InputMode::Confirm] {
            assert_eq!(map_accessible_input("tab", mode), no_op);
            assert_eq!(map_accessible_input("backtab", mode), no_op);
            assert_eq!(map_accessible_input("backspace", mode), no_op);
            assert_eq!(map_accessible_input("type hidden", mode), unsupported);
            assert_eq!(map_accessible_input("send hidden", mode), unsupported);
        }
        assert_eq!(map_accessible_input("enter", InputMode::Approval), no_op);
        assert_eq!(map_accessible_input("esc", InputMode::Approval), no_op);
        assert_eq!(map_accessible_input("home", InputMode::Editing), no_op);
        assert_eq!(map_accessible_input("end", InputMode::Confirm), no_op);
    }

    #[test]
    fn cooked_composer_navigation_matches_history_and_cursor_keys() {
        assert_eq!(
            map_accessible_input("up", InputMode::Composer),
            vec![Action::HistoryPrev]
        );
        assert_eq!(
            map_accessible_input("down", InputMode::Composer),
            vec![Action::HistoryNext]
        );
        assert_eq!(
            map_accessible_input("home", InputMode::Composer),
            vec![Action::CursorLineStart]
        );
        assert_eq!(
            map_accessible_input("backtab", InputMode::Composer),
            vec![Action::NoOp]
        );
    }

    #[test]
    fn council_builder_is_announced_with_attributed_members() {
        let mut state = AppState::new();
        state.overlay = Overlay::CouncilBuilder(CouncilBuilderState {
            step: CouncilBuilderStep::Review,
            name: "design-board".to_owned(),
            description: "Architecture".to_owned(),
            members: vec![CouncilMemberDraft {
                model: "kimi-code".to_owned(),
                role: "systems architect".to_owned(),
            }],
            chair: Some("amp".to_owned()),
            rounds: 2,
            query: String::new(),
            selected: 0,
            pending_member_model: None,
            role: String::new(),
        });
        let snapshot = accessible_snapshot(&state);
        assert!(snapshot.contains("Council builder: step Review"));
        assert!(snapshot.contains("design-board"));
        assert!(snapshot.contains("Council member: kimi-code; role systems architect"));
        assert!(snapshot.contains("chair amp; rounds 2"));
    }

    #[test]
    fn model_provider_and_mode_pickers_announce_highlighted_and_available_rows() {
        let mut state = AppState::new();
        state.models = vec![
            picker_model("alpha", "ollama"),
            picker_model("beta", "claude-code"),
            picker_model("gamma", "kimi-code"),
        ];
        state.overlay = Overlay::ModelPicker {
            query: String::new(),
            selected: 1,
        };
        let models = accessible_snapshot(&state);
        assert!(
            models.contains("Highlighted model 2 of 3: beta; provider claude-code; ready"),
            "{models}"
        );
        assert!(
            models.contains("1: alpha; provider ollama; ready"),
            "{models}"
        );
        assert!(
            models.contains("3: gamma; provider kimi-code; ready"),
            "{models}"
        );

        state.providers = vec![
            picker_provider("ollama", "Ollama", "openai-chat", true),
            picker_provider("claude", "Claude Code", "acp", false),
        ];
        state.overlay = Overlay::ProviderPicker {
            query: String::new(),
            selected: 1,
        };
        let providers = accessible_snapshot(&state);
        assert!(
            providers.contains(
                "Highlighted provider 2 of 2: Claude Code (claude); protocol acp; not yet executable"
            ),
            "{providers}"
        );
        assert!(
            providers.contains("1: Ollama (ollama); protocol openai-chat; available"),
            "{providers}"
        );

        state.overlay = Overlay::ModePicker {
            query: String::new(),
            selected: 2,
        };
        let modes = accessible_snapshot(&state);
        assert!(
            modes.contains("Highlighted mode 3 of 5: Plan; investigate read-only"),
            "{modes}"
        );
        assert!(modes.contains("1: Ask; read-only Q&A"), "{modes}");
        assert!(
            modes.contains("5: Review; read-only verification"),
            "{modes}"
        );
    }

    #[test]
    fn council_member_and_chair_steps_announce_the_highlighted_candidate() {
        let mut state = AppState::new();
        state.models = vec![
            picker_model("alpha", "ollama"),
            picker_model("beta", "claude-code"),
            picker_model("gamma", "kimi-code"),
        ];
        let members = vec![
            CouncilMemberDraft {
                model: "alpha".to_owned(),
                role: "planner".to_owned(),
            },
            CouncilMemberDraft {
                model: "beta".to_owned(),
                role: "reviewer".to_owned(),
            },
        ];
        state.overlay = Overlay::CouncilBuilder(CouncilBuilderState {
            step: CouncilBuilderStep::MemberModel,
            members: members.clone(),
            selected: 1,
            ..CouncilBuilderState::default()
        });
        let member_picker = accessible_snapshot(&state);
        assert!(
            member_picker.contains(
                "Highlighted council member choice 2 of 3: gamma; provider kimi-code; ready"
            ),
            "{member_picker}"
        );
        assert!(
            member_picker.contains("1: Continue with 2 members"),
            "{member_picker}"
        );
        assert!(
            member_picker.contains("3: Remove last member; beta"),
            "{member_picker}"
        );

        state.overlay = Overlay::CouncilBuilder(CouncilBuilderState {
            step: CouncilBuilderStep::Chair,
            members,
            selected: 2,
            ..CouncilBuilderState::default()
        });
        let chair_picker = accessible_snapshot(&state);
        assert!(
            chair_picker
                .contains("Highlighted council chair 3 of 3: gamma; provider kimi-code; ready"),
            "{chair_picker}"
        );
        assert!(
            chair_picker.contains("1: alpha; provider ollama; ready"),
            "{chair_picker}"
        );
    }

    #[test]
    fn cooked_home_and_end_drive_council_picker_steps() {
        let mut state = AppState::new();
        state.overlay = Overlay::CouncilBuilder(CouncilBuilderState {
            step: CouncilBuilderStep::Rounds,
            ..CouncilBuilderState::default()
        });
        assert_eq!(state.input_mode(), InputMode::Palette);
        for action in map_accessible_input("end", state.input_mode()) {
            crate::reduce(&mut state, action);
        }
        assert!(matches!(
            state.overlay,
            Overlay::CouncilBuilder(CouncilBuilderState {
                selected: 2,
                rounds: 3,
                ..
            })
        ));
        for action in map_accessible_input("home", state.input_mode()) {
            crate::reduce(&mut state, action);
        }
        assert!(matches!(
            state.overlay,
            Overlay::CouncilBuilder(CouncilBuilderState {
                selected: 0,
                rounds: 1,
                ..
            })
        ));
    }

    #[test]
    fn snapshot_is_linear_ascii_chrome_and_strips_terminal_controls() {
        let mut state = AppState::new();
        let run_id = RunId::new();
        crate::reduce(
            &mut state,
            Action::daemon_event(SessionEvent {
                sequence: 1,
                occurred_at: chrono::Utc::now(),
                causation_id: None,
                correlation_id: None,
                actor: Actor::System,
                body: EventBody::RunStarted {
                    run_id,
                    objective: "build\u{1b}[31m safely".to_owned(),
                    mode: codypendent_protocol::AgentMode::Build,
                },
            }),
        );
        let snapshot = accessible_snapshot(&state);
        assert!(snapshot.contains("Codypendent accessible view"));
        assert!(snapshot.contains("build safely"));
        assert!(!snapshot.contains('\u{1b}'));
        for glyph in ['│', '┌', '┐', '⌕', '▏'] {
            assert!(!snapshot.contains(glyph));
        }
    }

    #[test]
    fn snapshot_includes_remote_ui_plain_text_and_semantic_metadata() {
        let mut state = AppState::new();
        crate::reduce(
            &mut state,
            Action::RemoteUiMessage(Box::new(
                crate::remote_ui_host::accessible_terminal_capabilities_message(80, 24),
            )),
        );
        let document: UiDocument = serde_json::from_value(serde_json::json!({
            "protocolVersion": {"major": 1, "minor": 0},
            "documentId": "accessible-extension",
            "revision": 1,
            "root": {
                "kind": "element", "id": "launch", "type": "Button",
                "props": {
                    "label": "Launch",
                    "accessibility": {
                        "description": "Starts the workflow",
                        "keyboardHint": "Enter",
                        "liveRegion": "polite"
                    }
                },
                "children": []
            }
        }))
        .expect("accessible document");
        let mut snapshot = crate::remote_ui_host::empty_message("snapshot", "accessible-snapshot");
        snapshot.snapshot = Some(codypendent_protocol::UiSnapshot {
            document,
            reason: None,
        });
        crate::reduce(&mut state, Action::RemoteUiMessage(Box::new(snapshot)));
        let mut contributions =
            crate::remote_ui_host::empty_message("contributions", "accessible-contribution");
        contributions
            .contributions
            .push(UiContributionRegistration {
                id: UiContributionId::from("accessible-registration"),
                extension_id: UiExtensionId::from("accessible-extension"),
                point: UiContributionPoint::from("panel"),
                slot: UiSlotId::from("panel"),
                document_id: UiDocumentId::from("accessible-extension"),
                priority: 0,
                when: None,
                requires: Vec::new(),
                metadata: Default::default(),
            });
        crate::reduce(&mut state, Action::RemoteUiMessage(Box::new(contributions)));

        let output = accessible_snapshot(&state);
        assert!(output.contains("Extension document accessible-extension"));
        assert!(output.contains("Launch"));
        assert!(output.contains("Starts the workflow"));
        assert!(output.contains("keyboard Enter"));
        assert!(output.contains("live region polite"));
        let cache = state.remote_ui.last_render.borrow();
        let rendered = cache
            .get(&UiDocumentId::from("accessible-extension"))
            .expect("cooked snapshot populated Remote UI interaction metadata");
        assert!(rendered
            .focus_order
            .iter()
            .any(|control| control.node_id.as_str() == "launch"));
    }

    #[test]
    fn help_chrome_has_a_strict_ascii_fallback() {
        let mut state = AppState::new();
        state.overlay = Overlay::Help;
        let output = accessible_snapshot(&state);
        assert!(output.is_ascii(), "help chrome was not ASCII: {output}");
        assert!(output.contains("F6 / Shift-F6 / Esc"));
    }

    /// The accessible client announced "Unsupported event: unsupported event"
    /// once per run — the `RunUsage` measurement, read aloud as an error.
    #[test]
    fn a_measured_run_is_announced_in_words_not_as_an_unsupported_event() {
        let mut state = AppState::new();
        let run_id = RunId::new();
        let event = |body| {
            Action::daemon_event(SessionEvent {
                sequence: 1,
                occurred_at: chrono::Utc::now(),
                causation_id: None,
                correlation_id: None,
                actor: Actor::System,
                body,
            })
        };
        crate::reduce(
            &mut state,
            event(EventBody::RunStarted {
                run_id,
                objective: "summarise".to_owned(),
                mode: codypendent_protocol::AgentMode::Build,
            }),
        );
        crate::reduce(
            &mut state,
            event(EventBody::RunUsage {
                run_id,
                prompt_tokens: Some(10_000),
                completion_tokens: Some(642),
                cost_micros: Some(3_400),
                coverage: None,
            }),
        );
        let snapshot = accessible_snapshot(&state);
        assert!(
            !snapshot.contains("Unsupported event"),
            "an event this build produces is never announced as unsupported:\n{snapshot}"
        );
        assert!(
            snapshot.contains("Usage: 10000 prompt tokens; 642 completion tokens"),
            "the measurement is announced in words:\n{snapshot}"
        );
        assert!(
            snapshot.contains("cost 0.0034 US dollars"),
            "a measured cost is announced too:\n{snapshot}"
        );
    }

    /// The question card is fully drivable from the cooked client: digits
    /// pick, `space` toggles, `reject` opens feedback, and free text becomes
    /// the custom answer one `QuestionInputChar` at a time — everything
    /// `controls_for(Question)` promises.
    #[test]
    fn question_card_commands_map_in_accessible_mode() {
        use crate::action::Action;
        assert_eq!(
            map_accessible_input("3", InputMode::Question),
            vec![Action::QuestionPickDigit(3)]
        );
        assert_eq!(
            map_accessible_input("space", InputMode::Question),
            vec![Action::QuestionToggleOption]
        );
        assert_eq!(
            map_accessible_input("reject", InputMode::Question),
            vec![Action::QuestionOpenReject]
        );
        assert_eq!(
            map_accessible_input("type ok", InputMode::Question),
            vec![
                Action::QuestionInputChar('o'),
                Action::QuestionInputChar('k'),
            ]
        );
        // A bare line is the custom answer too, and Enter confirms.
        assert_eq!(
            map_accessible_input("ok", InputMode::Question),
            vec![
                Action::QuestionInputChar('o'),
                Action::QuestionInputChar('k'),
            ]
        );
        assert_eq!(
            map_accessible_input("enter", InputMode::Question),
            vec![Action::QuestionSelectOrConfirm]
        );
        assert_eq!(
            map_accessible_input("esc", InputMode::Question),
            vec![Action::QuestionCancelReject]
        );
    }

    /// The always-allow approval scopes the graphical modal has offered as
    /// `p`/`P` are reachable by name from the cooked client.
    #[test]
    fn approval_scopes_map_in_accessible_mode() {
        use crate::action::Action;
        use codypendent_protocol::ApprovalScope;
        assert_eq!(
            map_accessible_input("approve-pattern", InputMode::Approval),
            vec![Action::Approve(ApprovalScope::Pattern)]
        );
        assert_eq!(
            map_accessible_input("approve-repository", InputMode::Approval),
            vec![Action::Approve(ApprovalScope::Repository)]
        );
        assert_eq!(
            map_accessible_input("approve-repo", InputMode::Approval),
            vec![Action::Approve(ApprovalScope::Repository)]
        );
        // The announced control list names them all.
        let controls = controls_for(InputMode::Approval);
        for verb in [
            "approve",
            "approve-run",
            "approve-pattern",
            "approve-repository",
        ] {
            assert!(controls.contains(verb), "{verb} missing from {controls}");
        }
    }
}
