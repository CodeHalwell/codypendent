import type { ReactNode } from "react";
import type { TranscriptItem } from "../types.js";
import { ToolActivity } from "./ToolActivity.js";

export function WorkActivity({ items, renderItem }: { items: TranscriptItem[]; renderItem: (item: TranscriptItem) => ReactNode }) {
  const progress = items.filter((item) => item.type === "thought");
  const actions = items.filter((item) => item.type !== "thought");
  return <div className="transcript-work-phase">
    {progress.length > 0 && <div className="transcript-work-progress">{progress.map(renderItem)}</div>}
    {actions.length > 0 && <ToolActivity items={actions} renderItem={renderItem} />}
  </div>;
}
