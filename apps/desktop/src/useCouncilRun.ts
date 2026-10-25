import { useCallback, useRef, useState } from "react";
import type { CouncilDraft, CouncilProgressFrame, CouncilResultCard, CouncilRunReply } from "./localConfig.js";

export type CouncilRunner = (name: string, objective: string,
  onProgress: (frame: CouncilProgressFrame) => void) => Promise<CouncilRunReply>;

export interface CouncilRunState {
  council: string;
  objective: string;
  progress: CouncilProgressFrame[];
  running: boolean;
  failure: string | null;
  result: CouncilResultCard | null;
  repository: string | null;
  completion: number;
}

export interface CouncilRunController extends CouncilRunState {
  setCouncil: (council: string) => void;
  setObjective: (objective: string) => void;
  start: (repository: string) => Promise<void>;
}

export function newCouncilDraft(): CouncilDraft {
  return { name: "", description: "", chair: "", rounds: 1,
    members: [{ model: "", role: "member" }, { model: "", role: "member" }] };
}

/** Keep one native deliberation alive while its page is elsewhere in the app. */
export function useCouncilRun(onRun?: CouncilRunner, initialCouncil = ""): CouncilRunController {
  const [state, setState] = useState<CouncilRunState>(() => ({ council: initialCouncil, objective: "",
    progress: [], running: false, failure: null, result: null, repository: null, completion: 0 }));
  const live = useRef(state);
  const runner = useRef(onRun);
  runner.current = onRun;
  const attempt = useRef(0);
  const update = useCallback((patch: Partial<CouncilRunState>) => {
    live.current = { ...live.current, ...patch };
    setState(live.current);
  }, []);
  const setCouncil = useCallback((council: string) => {
    if (!live.current.running) update({ council });
  }, [update]);
  const setObjective = useCallback((objective: string) => {
    if (!live.current.running) update({ objective });
  }, [update]);
  const start = useCallback(async (repository: string) => {
    const run = runner.current;
    const { council, objective, running } = live.current;
    if (!run || running || !repository.trim() || !council.trim() || !objective.trim()) return;
    const current = ++attempt.current;
    update({ running: true, progress: [], failure: null, result: null, repository });
    try {
      // Capture the runner and inputs once; later navigation cannot retarget a live run.
      const reply = await run(council.trim(), objective.trim(), (frame) => {
        if (attempt.current === current && live.current.running) {
          update({ progress: [...live.current.progress, frame] });
        }
      });
      update({ result: reply.result ?? null, failure: reply.failure ?? null });
    } catch (error) {
      update({ failure: error instanceof Error ? error.message : String(error) });
    } finally {
      update({ running: false, completion: live.current.completion + 1 });
    }
  }, [update]);
  return { ...state, setCouncil, setObjective, start };
}
