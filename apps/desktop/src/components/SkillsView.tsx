import { EmptyState, PageShell } from "./PageShell.js";
/**
 * Skill Studio — read only, exactly as the TUI's `Overlay::Skills` is.
 *
 * The invariant this view exists to keep: requested permissions are shown
 * VERBATIM, one row per capability, never summarised, never truncated, never
 * counted ("skill permissions are visible", `crates/tui/src/state.rs`
 * `SkillCard`). There is no mutation affordance because the TUI has none
 * either — the registry is governed elsewhere.
 */
import React, { useState } from "react";
import type { Loaded, SkillCard } from "./knowledgeTransport.js";
import { Field, SurfaceBody, surfaceButton, surfaceStyles } from "./surfaceChrome.js";

export interface SkillsViewProps {
  skills: Loaded<SkillCard>;
  /** Absent when the shell has no `list_skills` handler; then no refresh. */
  onRefresh?: () => void;
}

export const SkillsView: React.FC<SkillsViewProps> = ({ skills, onRefresh }) => {
  const [query, setQuery] = useState("");
  const needle = query.trim().toLowerCase();
  const rows =
    needle.length === 0
      ? skills.items
      : skills.items.filter(
          (skill) =>
            skill.name.toLowerCase().includes(needle) ||
            skill.description.toLowerCase().includes(needle) ||
            skill.permissions.some((permission) => permission.toLowerCase().includes(needle)),
        );

  return (
    <PageShell style={surfaceStyles.page}>
      <div className="cody-page-header" style={surfaceStyles.header}>
        <div>
          <h1 style={surfaceStyles.title}>Skills</h1>
          <div style={surfaceStyles.subtitle}>
            Reusable knowledge and tools available to your agents.
          </div>
        </div>
        <div style={{ display: "flex", gap: 8 }}>
          <input
            value={query}
            aria-label="Filter skills"
            placeholder="Search skills…"
            onChange={(event) => setQuery(event.target.value)}
            style={{
              background: "var(--cody-canvas)",
              border: "1px solid var(--cody-border-strong)",
              borderRadius: 6,
              color: "var(--cody-text)",
              fontSize: 12,
              padding: "5px 10px",
            }}
          />
          {onRefresh && (
            <button style={surfaceButton()} onClick={onRefresh}>
              Refresh
            </button>
          )}
        </div>
      </div>

      <div className="cody-page-body" style={surfaceStyles.scroll}>
        <SurfaceBody
          emptyTitle="Your agents’ skills"
          onRetry={onRefresh}
          status={skills.status}
          detail={skills.detail}
          count={skills.items.length}
          emptyMessage="No skills are registered in this workspace."
        >
          {rows.length === 0 ? (
            <EmptyState title="No matching skills" description={`No skill matches “${query.trim()}”. ${skills.items.length} registered.`}
              action={<button style={surfaceButton()} onClick={() => setQuery("")}>Clear search</button>} />
          ) : (
            rows.map((skill) => (
              <div key={`${skill.scope}/${skill.name}`} style={surfaceStyles.card}>
                <div style={{ display: "flex", justifyContent: "space-between", gap: 12 }}>
                  <span style={{ fontSize: 13, fontWeight: 600, color: "var(--cody-text)" }}>
                    {skill.name}
                  </span>
                  <span style={{ fontSize: 11, color: "var(--cody-text-muted)" }}>{skill.kind}</span>
                </div>
                <div style={{ fontSize: 12, color: "var(--cody-text-secondary)", marginTop: 6 }}>
                  {skill.description}
                </div>
                <div style={{ marginTop: 8 }}>
                  <Field label="scope" value={skill.scope} />
                  <Field label="trust" value={skill.trust} />
                  <Field label="status" value={skill.status} />
                  <Field label="risk" value={skill.risk} />
                </div>
                <div style={{ marginTop: 10 }}>
                  <div style={{ fontSize: 11, color: "var(--cody-text-muted)", marginBottom: 4 }}>
                    Requested permissions
                  </div>
                  {skill.permissions.length === 0 ? (
                    <div style={{ fontSize: 12, color: "var(--cody-text-faint)" }}>
                      Declares no capabilities.
                    </div>
                  ) : (
                    <ul style={{ margin: 0, paddingLeft: 18 }}>
                      {skill.permissions.map((permission) => (
                        // Verbatim: the exact string the package declared.
                        <li
                          key={permission}
                          style={{ ...surfaceStyles.mono, color: "var(--cody-attention)", lineHeight: 1.7 }}
                        >
                          {permission}
                        </li>
                      ))}
                    </ul>
                  )}
                </div>
              </div>
            ))
          )}
        </SurfaceBody>
      </div>
    </PageShell>
  );
};
