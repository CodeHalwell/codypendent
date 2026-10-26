import type { TranscriptItem } from "../types.js";

export type ActivityEntry = { id: string; items: TranscriptItem[]; grouped: boolean };

export function isBlockedAction(item: TranscriptItem) {
  return item.type === "system" && item.tone === "warning" && item.id.startsWith("denied-");
}

function canArchive(item: TranscriptItem) {
  return (item.type === "tool_call" || item.type === "tool_result" || item.type === "thought" || isBlockedAction(item))
    && item.artifact?.media_type !== "application/vnd.codypendent.desktop-tool+json"
    && item.diffPreview === undefined && !item.patchFiles?.length;
}

function sameActivity(a: TranscriptItem, b: TranscriptItem) {
  return canArchive(a) && canArchive(b) && (a.toolRunId ?? a.runId) === (b.toolRunId ?? b.runId);
}

export function activityEntries(items: TranscriptItem[], start = 0, end = items.length): ActivityEntry[] {
  const entries: ActivityEntry[] = [];
  for (let index = start; index < end; index++) {
    const item = items[index];
    const previous = entries.at(-1);
    const last = previous?.items.at(-1);
    if (previous && last && sameActivity(last, item)) {
      previous.items.push(item);
      previous.grouped = true;
    } else {
      let origin = index;
      if (index === start) while (origin > 0 && sameActivity(items[origin - 1], items[origin])) origin--;
      entries.push({ id: items[origin].id, items: [item], grouped: false });
    }
  }
  return entries;
}

export function activityCounts(items: TranscriptItem[]) {
  const calls = items.filter((item) => item.type === "tool_call");
  const counted = calls.length ? calls : items;
  return {
    total: counted.length,
    failed: counted.filter((item) => item.status === "error").length,
    blocked: items.filter(isBlockedAction).length,
    unavailable: counted.filter((item) => item.status === "finished").length,
    running: counted.filter((item) => item.type === "tool_call" && (!item.status || item.status === "running" || item.status === "pending")),
  };
}
