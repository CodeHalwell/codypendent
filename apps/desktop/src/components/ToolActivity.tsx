import { useState, type ReactNode } from "react";
import { ChevronRight, ListChecks } from "lucide-react";
import type { TranscriptItem } from "../types.js";
import { activityCounts } from "./activityEntries.js";
import { jsonData, toolTitle } from "./toolPresentation.js";
import "./ToolActivity.css";

export function ToolActivity({ items, renderItem }: { items: TranscriptItem[]; renderItem: (item: TranscriptItem) => ReactNode }) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const { total, failed, blocked, unavailable, running } = activityCounts(items);
  const latest = running.at(-1);
  const label = latest?.toolLabel ?? latest?.toolName ?? "Working";
  const latestLabel = jsonData(label) === undefined ? toolTitle(label) : "Working";
  const visible = query.trim() ? items.filter((item) =>
    `${item.toolLabel ?? ""} ${item.toolName ?? ""} ${item.text} ${item.status ?? ""}`.toLowerCase().includes(query.trim().toLowerCase())) : items;
  return <details className="tool-activity" onToggle={(event) => setOpen(event.currentTarget.open)}>
    <summary>
      <ChevronRight size={14} className="tool-activity-chevron" aria-hidden="true" />
      <ListChecks size={16} aria-hidden="true" />
      <span className="tool-activity-label"><span className="tool-activity-title">Activity <span>· {total} {total === 1 ? "call" : "calls"}</span></span>
        {latest && !open && <span className="tool-activity-current" role="status">{latestLabel}</span>}</span>
      {" "}
      <span className="tool-activity-counts">
        {failed > 0 && <><span className="tool-activity-failed">{failed} failed</span>{" "}</>}
        {blocked > 0 && <><span className="tool-activity-running">{blocked} blocked</span>{" "}</>}
        {unavailable > 0 && <><span>{unavailable} unavailable</span>{" "}</>}
        {running.length > 0 && <span className="tool-activity-running">{running.length} running</span>}
      </span>
    </summary>
    {open && <div className="tool-activity-body">
      {items.length > 8 && <label className="tool-activity-search">Find an action<input type="search" aria-label="Find an action" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="File, tool or command…" /></label>}
      <div className="tool-activity-items" role="region" aria-label="Tool activity history" tabIndex={0}>
        {visible.map(renderItem)}
        {visible.length === 0 && <p className="tool-activity-empty">No actions match your search.</p>}
      </div>
      <p className="tool-activity-footnote">{query.trim() ? `${visible.length} matching entries · ` : ""}Actions and results are kept with this chat.</p>
    </div>}
  </details>;
}
