import { PageShell } from "./PageShell.js";
/**
 * Docs Studio — the TUI's `Overlay::Docs` and its publish wizard.
 *
 * Split surface, as in the TUI: the document tree is read from the daemon's
 * SQLite (there is no `ListDocuments` on the wire), while every edit is a real
 * command — `CreateDocument`, `AcquireDocumentLease` + `MutateDocument` +
 * `ReleaseDocumentLease`, `PublishDocument`.
 *
 * Two things are ported deliberately rather than reinvented:
 *
 * 1. A block edit is a FULL REPLACE — delete exactly the original's character
 *    count, then insert the buffer. It is not a prepend.
 * 2. {@link validPublishPath} and {@link validPublishBranch} are ports of
 *    `crates/tui/src/reduce.rs`. These are SECURITY validation, not polish: a
 *    branch name reaches `git` on the daemon side, so refspec metacharacters
 *    and traversal never leave here.
 */
import React, { useState } from "react";
import type { PublishTarget } from "@codypendent/protocol";
import type { DocCard, Loaded } from "./knowledgeTransport.js";
import { ConfirmPanel, Field, SurfaceBody, surfaceButton, surfaceStyles } from "./surfaceChrome.js";

/** Publish targets in list order, least- to most-privileged (`state.rs`). */
export const DOC_PUBLISH_TARGETS = [
  {
    kind: "repository_file" as const,
    label: "Repository file",
    detail: "writes the file in the working tree and commits it",
    needsBranch: false,
  },
  {
    kind: "docs_branch_commit" as const,
    label: "Docs-branch commit",
    detail: "commits to a dedicated docs branch, worktree-safe",
    needsBranch: true,
  },
  {
    kind: "documentation_pr" as const,
    label: "Documentation pull request",
    detail: "commits, pushes, and opens a PR — approval is rated High",
    needsBranch: true,
  },
];

export type DocPublishTargetKind = (typeof DOC_PUBLISH_TARGETS)[number]["kind"];

export const PUBLISH_PATH_REFUSAL =
  "enter a repository-relative Markdown (.md) path without parent traversal";
export const PUBLISH_BRANCH_REFUSAL =
  "enter a branch name of letters, digits, `.`, `_`, `-` and `/` (no leading dash, no `..`)";
export const PUBLISH_TITLE_REFUSAL = "enter a title for the pull request";

/** Unicode control characters — `char::is_control` in the Rust original. */
const CONTROL_CHARACTERS = /[\u0000-\u001F\u007F-\u009F]/;

/**
 * A port of `valid_publish_branch` (`crates/tui/src/reduce.rs`).
 *
 * A deliberate allowlist, not a re-implementation of `git check-ref-format`:
 * this value ends up in a `git` invocation and a PR head ref on the daemon
 * side, so anything outside letters, digits, `.`, `_`, `-` and `/` is refused
 * here rather than relied on to be rejected there.
 */
export function validPublishBranch(branch: string): boolean {
  return (
    branch.length > 0 &&
    !branch.startsWith("-") &&
    !branch.startsWith("/") &&
    !branch.endsWith("/") &&
    !branch.endsWith(".lock") &&
    !branch.includes("..") &&
    !branch.includes("//") &&
    /^[A-Za-z0-9._/-]+$/.test(branch)
  );
}

/**
 * A port of `valid_publish_path` (`crates/tui/src/reduce.rs`), using the unix
 * path semantics the daemon runs under: non-empty, not absolute, no parent /
 * root component, at least one normal component, a `.md` extension
 * (case-insensitive), and no control characters.
 */
export function validPublishPath(path: string): boolean {
  if (path.length === 0 || path.startsWith("/")) {
    return false;
  }
  if (CONTROL_CHARACTERS.test(path)) {
    return false;
  }
  const normals: string[] = [];
  for (const segment of path.split("/")) {
    if (segment === "..") {
      return false;
    }
    if (segment === "" || segment === ".") {
      // Neither an empty segment (a `//`) nor `.` is a Normal component;
      // Rust's `Path::components` skips both.
      continue;
    }
    normals.push(segment);
  }
  const fileName = normals[normals.length - 1];
  if (fileName === undefined) {
    return false;
  }
  // `Path::extension` yields None when the name has no interior dot, which is
  // why a bare `.md` file is refused rather than accepted.
  const dot = fileName.lastIndexOf(".");
  if (dot <= 0) {
    return false;
  }
  return fileName.slice(dot + 1).toLowerCase() === "md";
}

/** A port of `publish_slug` (`crates/tui/src/reduce.rs`). */
export function publishSlug(title: string): string {
  let slug = "";
  let lastDash = false;
  for (const character of title.toLowerCase()) {
    if (/[a-z0-9]/.test(character)) {
      slug += character;
      lastDash = false;
    } else if (!lastDash && slug.length > 0) {
      slug += "-";
      lastDash = true;
    }
  }
  while (slug.endsWith("-")) {
    slug = slug.slice(0, -1);
  }
  return slug.length === 0 ? "document" : slug;
}

export interface DocsViewProps {
  docs: Loaded<DocCard>;
  onRefresh?: () => void;
  onCreateDocument?: (title: string) => void;
  /** Full replace of one block's text. */
  onReplaceBlock?: (
    documentId: string,
    blockId: string,
    original: string,
    replacement: string,
  ) => void;
  onDeleteBlock?: (documentId: string, blockId: string) => void;
  onPublish?: (documentId: string, target: PublishTarget) => void;
  notice?: string | null;
}

type Wizard =
  | { step: "target"; documentId: string }
  | { step: "path"; documentId: string; target: DocPublishTargetKind; buffer: string }
  | {
      step: "branch";
      documentId: string;
      target: DocPublishTargetKind;
      path: string;
      buffer: string;
    }
  | { step: "title"; documentId: string; path: string; branch: string; buffer: string };

export const DocsView: React.FC<DocsViewProps> = ({
  docs,
  onRefresh,
  onCreateDocument,
  onReplaceBlock,
  onDeleteBlock,
  onPublish,
  notice,
}) => {
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [newTitle, setNewTitle] = useState<string | null>(null);
  const [editing, setEditing] = useState<{
    blockId: string;
    original: string;
    buffer: string;
  } | null>(null);
  const [deleting, setDeleting] = useState<{ blockId: string; label: string } | null>(null);
  const [wizard, setWizard] = useState<Wizard | null>(null);
  const [refusal, setRefusal] = useState<string | null>(null);

  const selected =
    docs.items.find((doc) => doc.document_id === selectedId) ?? docs.items[0] ?? null;

  const chooseTarget = (target: DocPublishTargetKind) => {
    if (wizard?.step !== "target") {
      return;
    }
    const doc = docs.items.find((candidate) => candidate.document_id === wizard.documentId);
    const slug = doc ? publishSlug(doc.title) : "document";
    setWizard({
      step: "path",
      documentId: wizard.documentId,
      target,
      buffer: `docs/${slug}.md`,
    });
  };

  const submitPath = () => {
    if (wizard?.step !== "path") {
      return;
    }
    const path = wizard.buffer.trim();
    if (!validPublishPath(path)) {
      setRefusal(PUBLISH_PATH_REFUSAL);
      return;
    }
    setRefusal(null);
    const definition = DOC_PUBLISH_TARGETS.find((entry) => entry.kind === wizard.target);
    if (definition?.needsBranch) {
      // Defaulting the branch from the path keeps the common case Enter-Enter
      // rather than making the operator invent a name.
      const slug = publishSlug(path.replace(/\.md$/i, ""));
      setWizard({
        step: "branch",
        documentId: wizard.documentId,
        target: wizard.target,
        path,
        buffer: `docs/${slug}`,
      });
      return;
    }
    onPublish?.(wizard.documentId, { kind: "repository_file", path });
    setWizard(null);
  };

  const submitBranch = () => {
    if (wizard?.step !== "branch") {
      return;
    }
    const branch = wizard.buffer.trim();
    if (!validPublishBranch(branch)) {
      setRefusal(PUBLISH_BRANCH_REFUSAL);
      return;
    }
    setRefusal(null);
    if (wizard.target === "documentation_pr") {
      const doc = docs.items.find((candidate) => candidate.document_id === wizard.documentId);
      setWizard({
        step: "title",
        documentId: wizard.documentId,
        path: wizard.path,
        branch,
        buffer: doc ? `docs: ${doc.title}` : "",
      });
      return;
    }
    onPublish?.(wizard.documentId, {
      kind: "docs_branch_commit",
      branch,
      path: wizard.path,
    });
    setWizard(null);
  };

  const submitTitle = () => {
    if (wizard?.step !== "title") {
      return;
    }
    const title = wizard.buffer.trim();
    if (title.length === 0) {
      setRefusal(PUBLISH_TITLE_REFUSAL);
      return;
    }
    setRefusal(null);
    onPublish?.(wizard.documentId, {
      kind: "documentation_pr",
      branch: wizard.branch,
      path: wizard.path,
      title,
    });
    setWizard(null);
  };

  const promptStyle: React.CSSProperties = {
    width: "100%",
    boxSizing: "border-box",
    background: "var(--cody-canvas)",
    border: "1px solid var(--cody-border-strong)",
    borderRadius: 6,
    color: "var(--cody-text)",
    fontSize: 12,
    padding: 8,
  };

  return (
    <PageShell style={surfaceStyles.page}>
      <div className="cody-page-header" style={surfaceStyles.header}>
        <div>
          <h1 style={surfaceStyles.title}>Documents</h1>
          <div style={surfaceStyles.subtitle}>
            Keep plans, notes and agent suggestions together. Review changes before publishing.
          </div>
        </div>
        <div style={{ display: "flex", gap: 8 }}>
          {onCreateDocument && (
            <button style={surfaceButton("primary")} onClick={() => setNewTitle("")}>
              New document…
            </button>
          )}
          {onRefresh && (
            <button style={surfaceButton()} onClick={onRefresh}>
              Refresh
            </button>
          )}
        </div>
      </div>

      <div className="cody-page-body" style={surfaceStyles.scroll}>
        {notice && (
          <div
            role="status"
            style={{ ...surfaceStyles.card, color: "var(--cody-success-text)", borderColor: "var(--cody-success-strong)" }}
          >
            {notice}
          </div>
        )}
        {refusal && (
          <div
            role="alert"
            style={{ ...surfaceStyles.card, color: "var(--cody-danger-text)", borderColor: "var(--cody-danger)" }}
          >
            {refusal}
          </div>
        )}

        {newTitle !== null && onCreateDocument && (
          <div style={surfaceStyles.card}>
            <div style={{ fontSize: 12, color: "var(--cody-text-muted)", marginBottom: 6 }}>Document title</div>
            <input
              value={newTitle}
              aria-label="New document title"
              autoFocus
              onChange={(event) => setNewTitle(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  const title = newTitle.trim();
                  if (title.length === 0) {
                    setRefusal("a document title must not be empty");
                    return;
                  }
                  setRefusal(null);
                  onCreateDocument(title);
                  setNewTitle(null);
                }
              }}
              style={promptStyle}
            />
            <div style={{ display: "flex", gap: 8, marginTop: 8 }}>
              <button
                style={surfaceButton("primary")}
                onClick={() => {
                  const title = newTitle.trim();
                  if (title.length === 0) {
                    setRefusal("a document title must not be empty");
                    return;
                  }
                  setRefusal(null);
                  onCreateDocument(title);
                  setNewTitle(null);
                }}
              >
                Create
              </button>
              <button style={surfaceButton()} onClick={() => setNewTitle(null)}>
                Cancel
              </button>
            </div>
          </div>
        )}

        {wizard?.step === "target" && (
          <div style={surfaceStyles.card}>
            <div style={{ fontSize: 13, color: "var(--cody-text)", marginBottom: 8 }}>
              Publish — choose a target
            </div>
            {DOC_PUBLISH_TARGETS.map((entry) => (
              <button
                key={entry.kind}
                style={{
                  ...surfaceButton(),
                  display: "block",
                  width: "100%",
                  textAlign: "left",
                  marginBottom: 6,
                }}
                onClick={() => chooseTarget(entry.kind)}
              >
                <span style={{ fontWeight: 600 }}>{entry.label}</span>
                <span style={{ display: "block", fontSize: 11, color: "var(--cody-text-muted)", marginTop: 2 }}>
                  {entry.detail}
                </span>
              </button>
            ))}
            <button style={surfaceButton()} onClick={() => setWizard(null)}>
              Cancel
            </button>
          </div>
        )}

        {wizard?.step === "path" && (
          <div style={surfaceStyles.card}>
            <div style={{ fontSize: 12, color: "var(--cody-text-muted)", marginBottom: 6 }}>
              Repository-relative Markdown path
            </div>
            <input
              value={wizard.buffer}
              aria-label="Publish path"
              autoFocus
              onChange={(event) => setWizard({ ...wizard, buffer: event.target.value })}
              onKeyDown={(event) => event.key === "Enter" && submitPath()}
              style={promptStyle}
            />
            <div style={{ display: "flex", gap: 8, marginTop: 8 }}>
              <button style={surfaceButton("primary")} onClick={submitPath}>
                Continue
              </button>
              <button style={surfaceButton()} onClick={() => setWizard(null)}>
                Cancel
              </button>
            </div>
          </div>
        )}

        {wizard?.step === "branch" && (
          <div style={surfaceStyles.card}>
            <div style={{ fontSize: 12, color: "var(--cody-text-muted)", marginBottom: 6 }}>Branch</div>
            <input
              value={wizard.buffer}
              aria-label="Publish branch"
              autoFocus
              onChange={(event) => setWizard({ ...wizard, buffer: event.target.value })}
              onKeyDown={(event) => event.key === "Enter" && submitBranch()}
              style={promptStyle}
            />
            <div style={{ display: "flex", gap: 8, marginTop: 8 }}>
              <button style={surfaceButton("primary")} onClick={submitBranch}>
                Continue
              </button>
              <button style={surfaceButton()} onClick={() => setWizard(null)}>
                Cancel
              </button>
            </div>
          </div>
        )}

        {wizard?.step === "title" && (
          <div style={surfaceStyles.card}>
            <div style={{ fontSize: 12, color: "var(--cody-text-muted)", marginBottom: 6 }}>
              Pull request title
            </div>
            <input
              value={wizard.buffer}
              aria-label="Pull request title"
              autoFocus
              onChange={(event) => setWizard({ ...wizard, buffer: event.target.value })}
              onKeyDown={(event) => event.key === "Enter" && submitTitle()}
              style={promptStyle}
            />
            <div style={{ display: "flex", gap: 8, marginTop: 8 }}>
              <button style={surfaceButton("primary")} onClick={submitTitle}>
                Open pull request
              </button>
              <button style={surfaceButton()} onClick={() => setWizard(null)}>
                Cancel
              </button>
            </div>
          </div>
        )}

        {deleting && selected && onDeleteBlock && (
          <ConfirmPanel
            title="Delete this block?"
            evidence={deleting.label}
            confirmLabel="Delete block"
            onConfirm={() => {
              onDeleteBlock(selected.document_id, deleting.blockId);
              setDeleting(null);
            }}
            onCancel={() => setDeleting(null)}
          />
        )}

        <SurfaceBody
          emptyTitle="Create your first document"
          emptyAction={onCreateDocument && <button style={surfaceButton("primary")} onClick={() => setNewTitle("")}>New document</button>}
          onRetry={onRefresh}
          status={docs.status}
          detail={docs.detail}
          count={docs.items.length}
          emptyMessage="No documents exist in this workspace yet."
        >
          <div className="cody-page-columns">
            <div style={{ width: 240, flexShrink: 0 }}>
              {docs.items.map((doc) => {
                const active = selected?.document_id === doc.document_id;
                return (
                  <button
                    key={doc.document_id}
                    onClick={() => {
                      setSelectedId(doc.document_id);
                      setEditing(null);
                      setDeleting(null);
                    }}
                    style={{
                      ...surfaceButton(),
                      display: "block",
                      width: "100%",
                      textAlign: "left",
                      marginBottom: 6,
                      background: active ? "var(--cody-panel-hover)" : "var(--cody-inset)",
                      borderColor: active ? "var(--cody-accent)" : "var(--cody-border-strong)",
                    }}
                  >
                    <span style={{ fontWeight: active ? 600 : 400 }}>{doc.title}</span>
                    <span
                      style={{ display: "block", fontSize: 11, color: "var(--cody-text-muted)", marginTop: 2 }}
                    >
                      {doc.status} · {doc.revision}
                    </span>
                  </button>
                );
              })}
            </div>

            <div style={{ flex: 1, minWidth: 0 }}>
              {selected && (
                <>
                  <div style={surfaceStyles.card}>
                    <div style={{ fontSize: 14, fontWeight: 600, color: "var(--cody-text)" }}>
                      {selected.title}
                    </div>
                    <div style={{ marginTop: 8 }}>
                      <Field label="scope" value={selected.scope} />
                      <Field label="status" value={selected.status} />
                      <Field label="mode" value={selected.mode} />
                      <Field label="revision" value={selected.revision} />
                    </div>
                    {onPublish && (
                      <div style={{ marginTop: 10 }}>
                        <button
                          style={surfaceButton()}
                          onClick={() => {
                            setRefusal(null);
                            setWizard({ step: "target", documentId: selected.document_id });
                          }}
                        >
                          Publish…
                        </button>
                      </div>
                    )}
                  </div>

                  <div style={{ fontSize: 11, color: "var(--cody-text-muted)", margin: "4px 0 6px" }}>Blocks</div>
                  {selected.blocks.length === 0 ? (
                    <div style={{ color: "var(--cody-text-faint)", fontSize: 13, marginBottom: 12 }}>
                      This document has no blocks.
                    </div>
                  ) : (
                    selected.blocks.map((block) => (
                      <div key={block.id} style={surfaceStyles.card}>
                        <div style={{ fontSize: 11, color: "var(--cody-text-muted)" }}>{block.kind}</div>
                        <div
                          style={{
                            fontSize: 13,
                            color: "var(--cody-text-secondary)",
                            marginTop: 4,
                            whiteSpace: "pre-wrap",
                            // Bounded like the blackboard payloads: one long
                            // block must not push every sibling off screen.
                            maxHeight: 320,
                            overflowY: "auto",
                          }}
                        >
                          {block.text}
                        </div>
                        <div style={{ display: "flex", gap: 8, marginTop: 8 }}>
                          {onReplaceBlock && block.editable !== null && (
                            <button
                              style={surfaceButton()}
                              onClick={() =>
                                setEditing({
                                  blockId: block.id,
                                  original: block.editable ?? "",
                                  buffer: block.editable ?? "",
                                })
                              }
                            >
                              Edit
                            </button>
                          )}
                          {onReplaceBlock && block.editable === null && (
                            <span style={{ fontSize: 11, color: "var(--cody-text-faint)" }}>
                              Structured block — no single editable text container.
                            </span>
                          )}
                          {onDeleteBlock && (
                            <button
                              style={surfaceButton("danger")}
                              onClick={() => setDeleting({ blockId: block.id, label: block.text })}
                            >
                              Delete…
                            </button>
                          )}
                        </div>
                        {editing?.blockId === block.id && onReplaceBlock && (
                          <div style={{ marginTop: 8 }}>
                            <textarea
                              value={editing.buffer}
                              aria-label="Block text"
                              rows={5}
                              onChange={(event) =>
                                setEditing({ ...editing, buffer: event.target.value })
                              }
                              style={promptStyle}
                            />
                            <div style={{ display: "flex", gap: 8, marginTop: 6 }}>
                              <button
                                style={surfaceButton("primary")}
                                onClick={() => {
                                  // A FULL REPLACE: the handler deletes exactly
                                  // `original`'s characters, then inserts the
                                  // buffer. That is only safe while the block
                                  // still reads as the snapshot taken when the
                                  // edit was opened — a document that changed
                                  // in between would be corrupted by the
                                  // delete, so the replace is refused and the
                                  // view re-read instead.
                                  const current = selected.blocks.find(
                                    (candidate) => candidate.id === editing.blockId,
                                  );
                                  if (current?.editable !== editing.original) {
                                    setRefusal(
                                      "document changed since edit started — re-open the edit",
                                    );
                                    setEditing(null);
                                    onRefresh?.();
                                    return;
                                  }
                                  onReplaceBlock(
                                    selected.document_id,
                                    editing.blockId,
                                    editing.original,
                                    editing.buffer,
                                  );
                                  setEditing(null);
                                }}
                              >
                                Replace block text
                              </button>
                              <button style={surfaceButton()} onClick={() => setEditing(null)}>
                                Cancel
                              </button>
                            </div>
                          </div>
                        )}
                      </div>
                    ))
                  )}

                  <div style={{ fontSize: 11, color: "var(--cody-text-muted)", margin: "10px 0 6px" }}>
                    Pending suggestions
                  </div>
                  {selected.suggestions.length === 0 ? (
                    <div style={{ color: "var(--cody-text-faint)", fontSize: 13 }}>No pending suggestions.</div>
                  ) : (
                    selected.suggestions.map((suggestion) => (
                      <div key={suggestion.id} style={surfaceStyles.card}>
                        <div>
                          <Field label="author" value={suggestion.author} />
                          <Field label="range" value={suggestion.range} />
                          <Field label="reviewed at" value={`r${suggestion.source_revision}`} />
                          <Field label="status" value={suggestion.status} />
                        </div>
                        <pre
                          style={{
                            ...surfaceStyles.mono,
                            whiteSpace: "pre-wrap",
                            wordBreak: "break-word",
                            color: "var(--cody-danger-text)",
                            margin: "8px 0 0",
                          }}
                        >
                          - {suggestion.original}
                        </pre>
                        <pre
                          style={{
                            ...surfaceStyles.mono,
                            whiteSpace: "pre-wrap",
                            wordBreak: "break-word",
                            color: "var(--cody-success-text)",
                            margin: "4px 0 0",
                          }}
                        >
                          + {suggestion.replacement}
                        </pre>
                        {suggestion.rationale && (
                          <div style={{ fontSize: 12, color: "var(--cody-text-muted)", marginTop: 6 }}>
                            {suggestion.rationale}
                          </div>
                        )}
                      </div>
                    ))
                  )}
                </>
              )}
            </div>
          </div>
        </SurfaceBody>
      </div>
    </PageShell>
  );
};
