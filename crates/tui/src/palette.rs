//! The command palette's command table and filtering (borrowed design idea: a
//! searchable command surface that scales a large feature set without a permanent
//! pane or a single-key binding per command).
//!
//! This module is pure data: the ordered list of commands the palette offers and
//! a case-insensitive filter over it. Executing a selected command — mapping it
//! onto a state change — lives in [`crate::reduce`], next to the helpers those
//! changes reuse. The palette overlay's own state (the filter query and the
//! selected index) lives in [`crate::state::Overlay::Palette`].

/// A command the palette can run. Each maps, in the reducer, onto the same effect
/// its single-key binding produces — the palette is a discoverable front door to
/// the existing commands, never a second code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteCommand {
    /// Open persistent setup/runtime diagnostics.
    Issues,
    /// Open guided model setup (the first-run triage). Reachable from the
    /// palette at any time, not only from an empty chat — after one failed
    /// run the empty-Enter shortcut is exactly what a user reaches for.
    Setup,
    /// Open the new-run prompt.
    NewRun,
    /// View detailed context usage breakdown.
    Context,
    /// Queue steering text for the selected run.
    Steer,
    /// Pause or resume the selected run.
    PauseResume,
    /// Ask to cancel the selected run.
    Cancel,
    /// Open the Skill Studio browser.
    Skills,
    /// Open the memory browser.
    Memory,
    /// Review useful, governed facts and preferences.
    Journey,
    /// Open the Docs Studio browser.
    Docs,
    /// Open the code-graph edge inspector.
    Edges,
    /// Open the workflow-graph view.
    Workflow,
    /// Open the blackboard view.
    Blackboard,
    /// Open the repository task board.
    Kanban,
    /// Open host-owned installed Remote UI plugin management.
    UiPlugins,
    /// Open the model picker (MP1).
    Model,
    /// Open the provider catalog picker (Task 8).
    Provider,
    /// Refresh the downloadable agent catalogue in the background.
    RefreshAgents,
    /// Open the submission-mode picker (PR C2 — plan mode).
    Mode,
    /// Open the `/keys` overlay (D1): set/replace/remove API keys.
    ApiKeys,
    /// Surface the persisted multi-provider council workflow.
    Council,
    /// Browse or directly retrieve durable council outcomes.
    CouncilResults,
    /// Browse the Unsloth GGUF catalog on Hugging Face and pull one via
    /// `ollama` into a selectable local model.
    UnslothCatalog,
    /// Toggle speaking finalized assistant turns aloud (voice v1, rubric 8).
    VoiceSpeak,
    /// Open the theme picker (7 built-in variants + installed packs).
    Theme,
    /// Flip between the chat and workspace layouts.
    ToggleLayout,
    /// Toggle the help overlay.
    Help,
    /// Detach this client (the run keeps going).
    Detach,
    /// Resume an existing conversation session (Adoption 11 S1).
    Sessions,
    /// Open the Session Library: daemon-ranked search plus the lifecycle verbs.
    SessionLibrary,
    /// Start a fresh, unseeded conversation in place. The harness creates a new
    /// durable session and atomically hands the running TUI to its own socket;
    /// the previous run remains alive in the daemon without leaking events into
    /// the new conversation.
    NewConversation,
    /// Open the backtrack/fork picker over the session's checkpointed runs —
    /// the palette front door to the `Esc Esc` gesture, which was otherwise
    /// undiscoverable.
    Backtrack,
}

/// One palette row: the command plus how it is presented and searched.
#[derive(Debug, Clone, Copy)]
pub struct PaletteEntry {
    /// The command this row runs.
    pub command: PaletteCommand,
    /// The row's title (what the user reads and matches on).
    pub title: &'static str,
    /// A one-line description of what the command does.
    pub description: &'static str,
    /// The single-key equivalent, shown as a hint (kept in sync with
    /// [`crate::input`]) — `"—"` for a palette-only command with no
    /// single-key binding (e.g. the model picker: MP1 deliberately gives it
    /// none, leaving `m` free).
    pub key: &'static str,
    /// The command's group, for the palette's dim group-label rows. Groups
    /// are rendered only while the query is empty (filtering can straddle
    /// groups, so the labels would mislead); see [`crate::render`].
    pub group: &'static str,
}

/// Every command the palette offers, grouped into contiguous sections (`Run` →
/// `Models` → `Workspace` → `Session`) so the palette can render a dim group label
/// whenever the group changes. [`filtered`] preserves this table order, so the
/// selectable index math is unaffected by the grouping.
pub const COMMANDS: &[PaletteEntry] = &[
    // --- Setup: first-run health and model configuration. ---
    PaletteEntry {
        command: PaletteCommand::Setup,
        title: "/setup  Guided setup",
        description: "connect a provider: a hosted API, a local endpoint, or an ACP coding agent",
        key: "—",
        group: "Setup",
    },
    PaletteEntry {
        command: PaletteCommand::Issues,
        title: "Setup & diagnostics",
        description: "review persistent configuration and runtime issues",
        key: "—",
        group: "Setup",
    },
    // --- Run: acting on the selected run. ---
    PaletteEntry {
        command: PaletteCommand::NewRun,
        title: "New run",
        description: "start a new run in this session",
        key: "n",
        group: "Run",
    },
    PaletteEntry {
        command: PaletteCommand::Context,
        title: "/context  Context usage breakdown",
        description: "view detailed token usage breakdown for the active run",
        key: "—",
        group: "Run",
    },
    PaletteEntry {
        command: PaletteCommand::Steer,
        title: "Steer run",
        description: "queue a message for the next safe point",
        key: "s",
        group: "Run",
    },
    PaletteEntry {
        command: PaletteCommand::PauseResume,
        title: "Pause / resume run",
        description: "pause the selected run, or resume it",
        key: "p",
        group: "Run",
    },
    PaletteEntry {
        command: PaletteCommand::Cancel,
        title: "Cancel run",
        description: "cancel the selected run (asks to confirm)",
        key: "c",
        group: "Run",
    },
    // --- Models: choosing what runs the next turn. ---
    PaletteEntry {
        command: PaletteCommand::Model,
        title: "/model  Model picker",
        description: "choose the model pinned to your next and later runs",
        // Palette-only this task: no single-key equivalent (see the field's
        // doc comment).
        key: "—",
        group: "Models",
    },
    PaletteEntry {
        command: PaletteCommand::Provider,
        title: "/provider  Provider catalog",
        description: "browse supported providers and add a usable model",
        // Palette-only (Task 8): no single-key equivalent, mirroring the
        // model picker's own row.
        key: "—",
        group: "Models",
    },
    PaletteEntry {
        command: PaletteCommand::RefreshAgents,
        title: "/refresh-agents  Refresh agent catalogue",
        description: "update downloadable ACP agents from the official registry",
        key: "—",
        group: "Models",
    },
    PaletteEntry {
        command: PaletteCommand::Mode,
        title: "/mode  Mode picker",
        description: "choose the submission mode for the next run (Ask/Explore/Plan/Build/Review)",
        // Palette-only (PR C2): no single-key equivalent, mirroring the
        // model/provider pickers.
        key: "—",
        group: "Models",
    },
    PaletteEntry {
        command: PaletteCommand::ApiKeys,
        title: "/keys  API keys",
        description: "set, replace, or remove API keys (stored locally in auth.json)",
        // Palette-only (D1): no single-key equivalent, mirroring the other
        // pickers. NOTE: the title/description deliberately avoid the words
        // "model", "provider", and "mode" so those pickers' filter queries
        // stay unambiguous.
        key: "—",
        group: "Models",
    },
    PaletteEntry {
        command: PaletteCommand::Council,
        title: "/council  Agent council",
        description: "list, run, and manage persisted multi-model councils; `n` inside creates one",
        key: "C",
        group: "Models",
    },
    PaletteEntry {
        command: PaletteCommand::CouncilResults,
        title: "/council result  Council results",
        description: "open durable outcomes; append a council name or result id for direct lookup",
        key: "—",
        group: "Models",
    },
    PaletteEntry {
        command: PaletteCommand::UnslothCatalog,
        title: "Local models: browse Unsloth catalog",
        description:
            "browse Unsloth GGUF repos on Hugging Face, pick a quant, and pull it via ollama",
        // Palette-only: a rare, deliberate action gets no single-key slot,
        // mirroring the other model-configuration entries in this group.
        key: "—",
        group: "Models",
    },
    // --- Workspace: live studios, workflow controls, and inspectors. ---
    PaletteEntry {
        command: PaletteCommand::Docs,
        title: "/docs  Docs Studio · existing docs",
        description: "edit, review, watch, and publish documents that already exist",
        key: "D",
        group: "Workspace",
    },
    PaletteEntry {
        command: PaletteCommand::Edges,
        title: "/edges  Code-graph edges",
        description: "search and page graph edges with evidence and revision",
        key: "G",
        group: "Workspace",
    },
    PaletteEntry {
        command: PaletteCommand::Workflow,
        title: "/workflow  Executable workflow graph",
        description: "create, open, and run persisted workflow manifests with a live DAG",
        key: "W",
        group: "Workspace",
    },
    PaletteEntry {
        command: PaletteCommand::Blackboard,
        title: "/blackboard  Blackboard evidence stream",
        description: "inspect or post attributed workflow evidence, decisions, and artifacts",
        key: "B",
        group: "Workspace",
    },
    PaletteEntry {
        command: PaletteCommand::Kanban,
        title: "/board  Kanban task board",
        description: "create, assign, and move repository backlog tasks in Kanban columns",
        key: "K",
        group: "Workspace",
    },
    PaletteEntry {
        command: PaletteCommand::Skills,
        title: "/skills  Skill Studio · read only",
        description: "inspect registered skills and their permissions",
        key: "S",
        group: "Workspace",
    },
    PaletteEntry {
        command: PaletteCommand::Memory,
        title: "/memory  Memory",
        description: "browse curated memories and their provenance",
        key: "M",
        group: "Workspace",
    },
    PaletteEntry {
        command: PaletteCommand::Journey,
        title: "/journey  Learning review",
        description: "review, activate, edit, pin, reject, or delete curated learnings",
        key: "J",
        group: "Workspace",
    },
    PaletteEntry {
        command: PaletteCommand::UiPlugins,
        title: "/plugins  Remote UI plugins",
        description: "inspect, smoke-test, scope, approve, reject, or revoke verified UI plugins",
        key: "—",
        group: "Workspace",
    },
    // --- Session: client-level and housekeeping commands. ---
    PaletteEntry {
        command: PaletteCommand::VoiceSpeak,
        title: "Voice: speak replies",
        description:
            "read each finished assistant turn aloud (needs a [speech] entry and a play_command)",
        // Palette-only: speaking aloud is a deliberate, occasional choice, not
        // something to fire from a stray keystroke.
        key: "—",
        group: "Session",
    },
    PaletteEntry {
        command: PaletteCommand::Theme,
        title: "/theme  Theme picker",
        description: "switch the colour theme — previews live as you move",
        // Palette-only, like the model/provider/mode pickers.
        key: "—",
        group: "Session",
    },
    PaletteEntry {
        command: PaletteCommand::ToggleLayout,
        title: "Toggle layout",
        description: "switch between chat and workspace panes",
        key: "F2",
        group: "Session",
    },
    PaletteEntry {
        command: PaletteCommand::Help,
        title: "Help",
        description: "toggle the key-binding help overlay",
        key: "?",
        group: "Session",
    },
    PaletteEntry {
        command: PaletteCommand::Detach,
        title: "Detach",
        description: "leave the TUI; the run keeps going",
        key: "Ctrl-C",
        group: "Session",
    },
    PaletteEntry {
        command: PaletteCommand::Sessions,
        title: "/sessions  Resume session",
        description: "browse and switch to another conversation session",
        key: "—",
        group: "Session",
    },
    PaletteEntry {
        command: PaletteCommand::SessionLibrary,
        title: "/library  Session library",
        description: "search every session and rename, pin, archive, or export one",
        key: "—",
        group: "Session",
    },
    PaletteEntry {
        command: PaletteCommand::NewConversation,
        title: "New conversation",
        description: "start a fresh, unseeded conversation in this TUI",
        // Palette-only: a deliberate, rare action gets no single-key slot.
        key: "—",
        group: "Session",
    },
    PaletteEntry {
        command: PaletteCommand::Backtrack,
        title: "Rewind / fork from an earlier turn",
        description: "pick a checkpointed run and branch the conversation from it",
        key: "Esc Esc",
        group: "Session",
    },
];

/// The commands matching `query`, ranked by intent. An empty query preserves
/// the curated table order. A non-empty query prefers an exact title/word/key,
/// then title prefixes, then title/description substrings. This matters for
/// pairs such as `mode` / `model`: typing `mode` must put **Mode picker** first
/// even though `model` also contains those four characters.
#[must_use]
pub fn filtered(query: &str) -> Vec<&'static PaletteEntry> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return COMMANDS.iter().collect();
    }

    let mut matches: Vec<_> = COMMANDS
        .iter()
        .enumerate()
        .filter_map(|(table_index, entry)| {
            palette_match_score(entry, &needle).map(|score| (score, table_index, entry))
        })
        .collect();
    matches.sort_by_key(|(score, table_index, _)| (*score, *table_index));
    matches.into_iter().map(|(_, _, entry)| entry).collect()
}

/// Lower is a stronger palette match. Kept deliberately small and predictable:
/// command discovery should feel smart without making ordering mysterious.
fn palette_match_score(entry: &PaletteEntry, needle: &str) -> Option<u8> {
    let title = entry.title.to_lowercase();
    let description = entry.description.to_lowercase();
    let key = entry.key.to_lowercase();

    if title == needle {
        return Some(0);
    }
    if title
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word == needle)
    {
        return Some(1);
    }
    if key == needle {
        return Some(2);
    }
    if title.starts_with(needle) {
        return Some(3);
    }
    if title.contains(needle) {
        return Some(4);
    }
    if description.starts_with(needle) {
        return Some(5);
    }
    if description.contains(needle) {
        return Some(6);
    }
    None
}

/// The number of commands matching `query` (the length of the navigable list).
#[must_use]
pub fn filtered_len(query: &str) -> usize {
    filtered(query).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_matches_every_command() {
        assert_eq!(filtered("").len(), COMMANDS.len());
    }

    #[test]
    fn filters_case_insensitively_on_title_and_description() {
        // Title match.
        let docs = filtered("docs");
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].command, PaletteCommand::Docs);
        // Description match ("provenance" is only in Memory's description).
        let prov = filtered("PROVENANCE");
        assert_eq!(prov.len(), 1);
        assert_eq!(prov[0].command, PaletteCommand::Memory);
    }

    #[test]
    fn a_nonsense_query_matches_nothing() {
        assert!(filtered("zzzzz").is_empty());
    }

    #[test]
    fn filters_to_the_model_picker_command() {
        // MP1: "/model" opens the picker via the palette front door.
        let model = filtered("model");
        // Other commands may legitimately mention models in their richer
        // descriptions; the exact title match must still rank first.
        assert_eq!(model[0].command, PaletteCommand::Model);
    }

    #[test]
    fn filters_to_the_provider_picker_command() {
        // Task 8: "/provider" opens the catalog picker via the palette front door.
        let provider = filtered("provider");
        assert_eq!(provider[0].command, PaletteCommand::Provider);
    }

    #[test]
    fn filters_to_the_agent_council_command() {
        assert_eq!(filtered("council")[0].command, PaletteCommand::Council);
    }

    #[test]
    fn filters_to_the_unsloth_catalog_command() {
        assert_eq!(
            filtered("unsloth")[0].command,
            PaletteCommand::UnslothCatalog
        );
        assert_eq!(
            filtered("Local models")[0].command,
            PaletteCommand::UnslothCatalog
        );
    }

    #[test]
    fn filters_to_the_mode_picker_command() {
        // PR C2: `/mode` opens the mode picker via the palette front door even
        // though the letters also prefix "model". Intent ranking keeps the
        // exact word match first.
        let mode = filtered("mode");
        assert_eq!(mode[0].command, PaletteCommand::Mode);
    }

    #[test]
    fn exact_word_and_key_matches_beat_incidental_substrings() {
        assert_eq!(filtered("mode")[0].command, PaletteCommand::Mode);
        assert_eq!(filtered("model")[0].command, PaletteCommand::Model);
        assert_eq!(filtered("?")[0].command, PaletteCommand::Help);
    }

    #[test]
    fn every_command_has_a_nonempty_title_and_key() {
        for entry in COMMANDS {
            assert!(!entry.title.is_empty());
            assert!(!entry.key.is_empty());
        }
    }
}
