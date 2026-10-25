import { useEffect, useRef, useState } from "react";
import type { PendingPromptView } from "@codypendent/protocol";
import { useCommandActions } from "./useCommandActions.js";

type Draft = { id: string; text: string };
type Options = {
  scope: string;
  sessionId: string | null;
  prompts: readonly PendingPromptView[];
  canMutate: boolean;
  onEdit: (id: string, text: string) => Promise<boolean>;
  onPromote: (id: string) => Promise<boolean>;
  onDelete: (id: string) => Promise<boolean>;
};

/** Keep queue drafts and in-flight commands above panel/navigation unmounts. */
export function usePromptQueueActions({ scope, sessionId, prompts, canMutate, onEdit, onPromote, onDelete }: Options) {
  const actions = useCommandActions(scope);
  const owner = useRef({ scope });
  if (owner.current.scope !== scope) owner.current = { scope };
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);
  const [editor, setEditor] = useState<{ sessionId: string | null; draft: Draft | null }>({ sessionId, draft: null });
  const draftId = editor.draft?.id;
  if (editor.sessionId !== sessionId || (draftId !== undefined && !prompts.some((p) => p.id === draftId))) {
    setEditor({ sessionId, draft: null });
  }
  const draft = editor.sessionId === sessionId ? editor.draft : null;
  const setDraft = (next: Draft | null) => setEditor({ sessionId, draft: next });
  const run = (id: string, command: () => Promise<boolean>, accepted?: () => void) => {
    if (!canMutate || !prompts.some((prompt) => prompt.id === id)) return;
    const requestOwner = owner.current;
    actions.run(id, async () => {
      if (!await command()) throw new Error("The queue change was not accepted. Review the queue error and try again.");
      if (mounted.current && owner.current === requestOwner) accepted?.();
    });
  };
  return {
    draft,
    setDraft,
    feedback: actions.feedback,
    isLocked: actions.isLocked,
    edit: (prompt: PendingPromptView) => {
      if (canMutate && !actions.isLocked(prompt.id)) setDraft({ id: prompt.id, text: prompt.text });
    },
    save: () => {
      if (!draft?.text.trim()) return;
      run(draft.id, () => onEdit(draft.id, draft.text), () => {
        // A later keystroke, Cancel/Edit, or another row owns a different
        // object, even if its text returns to the same value (A -> B -> A).
        setEditor((current) => current.draft === draft ? { ...current, draft: null } : current);
      });
    },
    promote: (id: string) => run(id, () => onPromote(id)),
    remove: (id: string) => run(id, () => onDelete(id)),
  };
}

export type PromptQueueActions = ReturnType<typeof usePromptQueueActions>;
