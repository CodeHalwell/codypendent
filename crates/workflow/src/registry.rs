//! Cross-checking a compiled workflow's references against a live registry
//! (STEP 5.1).
//!
//! The compiler ([`crate::compile`]) validates everything the manifest shape can
//! express on its own — ids, edges, the acyclic graph, budget sanity — but it
//! deliberately cannot answer *does this name resolve?*: whether a step's `tool`,
//! an agent's `skill`, or an agent `role` actually exists is a question only the
//! live registry (and the set of loaded agent profiles) can answer. This module
//! is the seam for that question.
//!
//! [`WorkflowRegistry`] is a narrow lookup interface: the workflow crate stays
//! daemon- and knowledge-free, so the daemon supplies the concrete registry (its
//! `codypendent-knowledge` `Registry` for tools/skills plus the loaded agent
//! profiles for roles) by implementing this trait. [`SetRegistry`] is an
//! in-memory implementation over string sets — used by the crate's own tests and
//! usable by any caller that has already materialised the known names.
//!
//! [`crate::compile::compile_with_registry`] runs [`crate::compile::compile`]
//! first and then this resolution pass, so a caller gets one `Result` that is
//! either a fully validated [`crate::CompiledWorkflow`] or the precise reference
//! that failed to resolve.

use std::collections::BTreeSet;

/// A lookup interface over the names a workflow may reference: tools, skills, and
/// agent roles. Implemented by the daemon over the live registry + loaded agent
/// profiles; [`SetRegistry`] is the in-memory implementation.
///
/// Each method answers *does a usable item with this exact name exist?* — scope
/// resolution and shadowing are the registry's concern, not the caller's. A
/// lookup is a pure, synchronous membership test: the daemon materialises the
/// known names once (the registry list is small and cached) and hands this seam a
/// snapshot, so compiling a workflow never blocks on I/O.
pub trait WorkflowRegistry {
    /// Whether a tool with this name is registered.
    fn has_tool(&self, name: &str) -> bool;
    /// Whether a skill with this name is registered.
    fn has_skill(&self, name: &str) -> bool;
    /// Whether an agent role (profile) with this name is known.
    fn has_agent_role(&self, role: &str) -> bool;
}

/// An in-memory [`WorkflowRegistry`] over three name sets. Cheap to build from the
/// registry's item list and the loaded agent profiles; the crate's tests use it
/// directly, and a caller that already has the names in hand can too.
#[derive(Debug, Clone, Default)]
pub struct SetRegistry {
    tools: BTreeSet<String>,
    skills: BTreeSet<String>,
    roles: BTreeSet<String>,
}

impl SetRegistry {
    /// An empty registry — every lookup fails until names are added.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a known tool name (builder-style).
    #[must_use]
    pub fn with_tool(mut self, name: impl Into<String>) -> Self {
        self.tools.insert(name.into());
        self
    }

    /// Register a known skill name (builder-style).
    #[must_use]
    pub fn with_skill(mut self, name: impl Into<String>) -> Self {
        self.skills.insert(name.into());
        self
    }

    /// Register a known agent role (builder-style).
    #[must_use]
    pub fn with_agent_role(mut self, role: impl Into<String>) -> Self {
        self.roles.insert(role.into());
        self
    }

    /// Register every tool name in `names`.
    pub fn add_tools<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tools.extend(names.into_iter().map(Into::into));
    }

    /// Register every skill name in `names`.
    pub fn add_skills<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.skills.extend(names.into_iter().map(Into::into));
    }

    /// Register every agent role in `roles`.
    pub fn add_agent_roles<I, S>(&mut self, roles: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.roles.extend(roles.into_iter().map(Into::into));
    }
}

impl WorkflowRegistry for SetRegistry {
    fn has_tool(&self, name: &str) -> bool {
        self.tools.contains(name)
    }

    fn has_skill(&self, name: &str) -> bool {
        self.skills.contains(name)
    }

    fn has_agent_role(&self, role: &str) -> bool {
        self.roles.contains(role)
    }
}

/// A `&T` registry defers to the underlying `T`, so a caller can cross-check
/// against a borrowed registry without giving up ownership.
impl<T: WorkflowRegistry + ?Sized> WorkflowRegistry for &T {
    fn has_tool(&self, name: &str) -> bool {
        (**self).has_tool(name)
    }

    fn has_skill(&self, name: &str) -> bool {
        (**self).has_skill(name)
    }

    fn has_agent_role(&self, role: &str) -> bool {
        (**self).has_agent_role(role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_registry_reports_membership() {
        let registry = SetRegistry::new()
            .with_tool("repository.test")
            .with_skill("code.repair")
            .with_agent_role("implementer");
        assert!(registry.has_tool("repository.test"));
        assert!(!registry.has_tool("code.repair")); // a skill, not a tool
        assert!(registry.has_skill("code.repair"));
        assert!(registry.has_agent_role("implementer"));
        assert!(!registry.has_agent_role("ghost"));
    }

    #[test]
    fn borrowed_registry_defers_to_the_owner() {
        let owned = SetRegistry::new().with_tool("t");
        let borrowed = &owned;
        assert!(WorkflowRegistry::has_tool(&borrowed, "t"));
    }
}
