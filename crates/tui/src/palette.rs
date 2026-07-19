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
    /// Open the new-run prompt.
    NewRun,
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
    /// Open the Docs Studio browser.
    Docs,
    /// Open the code-graph edge inspector.
    Edges,
    /// Flip between the chat and workspace layouts.
    ToggleLayout,
    /// Toggle the help overlay.
    Help,
    /// Detach this client (the run keeps going).
    Detach,
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
    /// [`crate::input`]).
    pub key: &'static str,
}

/// Every command the palette offers, ordered by likely usage (not alphabetically)
/// so the common actions surface first when the query is empty.
pub const COMMANDS: &[PaletteEntry] = &[
    PaletteEntry {
        command: PaletteCommand::NewRun,
        title: "New run",
        description: "start a new run in this session",
        key: "n",
    },
    PaletteEntry {
        command: PaletteCommand::Steer,
        title: "Steer run",
        description: "queue a message for the next safe point",
        key: "s",
    },
    PaletteEntry {
        command: PaletteCommand::PauseResume,
        title: "Pause / resume run",
        description: "pause the selected run, or resume it",
        key: "p",
    },
    PaletteEntry {
        command: PaletteCommand::Cancel,
        title: "Cancel run",
        description: "cancel the selected run (asks to confirm)",
        key: "c",
    },
    PaletteEntry {
        command: PaletteCommand::Docs,
        title: "Docs Studio",
        description: "browse documents (tree / editor / review rails)",
        key: "D",
    },
    PaletteEntry {
        command: PaletteCommand::Edges,
        title: "Code-graph edges",
        description: "inspect graph edges (relation, evidence, revision)",
        key: "G",
    },
    PaletteEntry {
        command: PaletteCommand::Skills,
        title: "Skill Studio",
        description: "browse skills and their permissions verbatim",
        key: "S",
    },
    PaletteEntry {
        command: PaletteCommand::Memory,
        title: "Memory",
        description: "browse curated memories and their provenance",
        key: "M",
    },
    PaletteEntry {
        command: PaletteCommand::ToggleLayout,
        title: "Toggle layout",
        description: "switch between chat and workspace panes",
        key: "F2",
    },
    PaletteEntry {
        command: PaletteCommand::Help,
        title: "Help",
        description: "toggle the key-binding help overlay",
        key: "?",
    },
    PaletteEntry {
        command: PaletteCommand::Detach,
        title: "Detach",
        description: "leave the TUI; the run keeps going",
        key: "q",
    },
];

/// The commands matching `query`, in table order. An empty query matches
/// everything; otherwise a command matches when the (case-folded) query is a
/// substring of its title, description, or key.
#[must_use]
pub fn filtered(query: &str) -> Vec<&'static PaletteEntry> {
    let needle = query.trim().to_lowercase();
    COMMANDS
        .iter()
        .filter(|entry| {
            needle.is_empty()
                || entry.title.to_lowercase().contains(&needle)
                || entry.description.to_lowercase().contains(&needle)
                || entry.key.to_lowercase() == needle
        })
        .collect()
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
    fn every_command_has_a_nonempty_title_and_key() {
        for entry in COMMANDS {
            assert!(!entry.title.is_empty());
            assert!(!entry.key.is_empty());
        }
    }
}
