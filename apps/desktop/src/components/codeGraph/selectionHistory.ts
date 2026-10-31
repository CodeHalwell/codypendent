import type { CodeGraphPage } from "@codypendent/protocol";
import type { GraphNode } from "./layout.js";

/** Selection history belongs to one graph query; it never crosses repositories. */
export interface GraphSelectionHistory {
  scope: CodeGraphPage;
  entries: Array<GraphNode | null>;
  index: number;
  restoring: boolean;
}

type Navigation =
  | { type: "select"; scope: CodeGraphPage; node: GraphNode | null }
  | { type: "reset"; scope: CodeGraphPage; node: GraphNode | null }
  | { type: "back" }
  | { type: "forward" };

const HISTORY_LIMIT = 32;

export function initialGraphSelection({ scope, node }: { scope: CodeGraphPage; node: GraphNode | null }): GraphSelectionHistory {
  return { scope, entries: [node], index: 0, restoring: false };
}

/** A new selection replaces the forward branch, as in an editor's source history. */
export function navigateGraphSelection(history: GraphSelectionHistory, action: Navigation): GraphSelectionHistory {
  if (action.type === "reset") return initialGraphSelection(action);
  if (action.type === "back") return { ...history, index: Math.max(0, history.index - 1), restoring: true };
  if (action.type === "forward") return { ...history, index: Math.min(history.entries.length - 1, history.index + 1), restoring: true };
  if (history.scope !== action.scope) return initialGraphSelection(action);
  if (history.entries[history.index]?.id === action.node?.id) return history;
  const entries = [...history.entries.slice(0, history.index + 1), action.node].slice(-HISTORY_LIMIT);
  return { scope: history.scope, entries, index: entries.length - 1, restoring: false };
}
