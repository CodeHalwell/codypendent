import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { MAP_HEIGHT, MAP_WIDTH, type GraphData, type PositionedNode } from "./layout.js";
import { beginPhysicsDrag, createPhysics, endPhysicsDrag, movePhysicsNode, physicsNodes, physicsRunning, physicsSignature, reheatPhysics, stepPhysics, type PhysicsState, type Viewport } from "./physics.js";

const DEFAULT_VIEWPORT = { width: MAP_WIDTH, height: MAP_HEIGHT };
const reducedMotion = () => typeof window !== "undefined" && !!window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;

export function useGraphPhysics(data: GraphData, seeds: PositionedNode[], active: boolean, viewport: Viewport = DEFAULT_VIEWPORT) {
  const engine = useRef<PhysicsState | null>(null);
  if (!engine.current) engine.current = createPhysics(data, seeds, viewport);
  const latest = useRef({ data, seeds, viewport }); latest.current = { data, seeds, viewport };
  const [nodes, setNodes] = useState(() => physicsNodes(engine.current!));
  const [enabled, setEnabled] = useState(() => !reducedMotion());
  const [running, setRunning] = useState(false);
  const [revision, setRevision] = useState(0);
  const signature = useMemo(() => physicsSignature(data, seeds, viewport), [data, seeds, viewport.width, viewport.height]);

  useEffect(() => {
    const current = latest.current;
    engine.current = createPhysics(current.data, current.seeds, current.viewport, engine.current!);
    setNodes(physicsNodes(engine.current)); setRevision((value) => value + 1);
  }, [signature]);
  useEffect(() => {
    const media = window.matchMedia?.("(prefers-reduced-motion: reduce)");
    if (!media) return;
    const change = () => { if (media.matches) setEnabled(false); };
    media.addEventListener?.("change", change);
    return () => media.removeEventListener?.("change", change);
  }, []);
  useEffect(() => {
    if (!active || !enabled) { setRunning(false); return; }
    const state = engine.current!; reheatPhysics(state);
    if (!physicsRunning(state)) { setRunning(false); return; }
    let frame: number, last: number | undefined, cancelled = false;
    setRunning(true);
    const tick = (time: number) => {
      if (cancelled) return;
      const continuing = stepPhysics(state, last === undefined ? 1000 / 60 : time - last); last = time;
      setNodes(physicsNodes(state));
      if (continuing) frame = requestAnimationFrame(tick); else setRunning(false);
    };
    frame = requestAnimationFrame(tick);
    return () => { cancelled = true; cancelAnimationFrame(frame); setRunning(false); };
  }, [active, enabled, revision]);

  const beginDrag = useCallback((id: string) => { beginPhysicsDrag(engine.current!, id); setRevision((value) => value + 1); }, []);
  const moveNode = useCallback((id: string, x: number, y: number) => { movePhysicsNode(engine.current!, id, x, y); setNodes(physicsNodes(engine.current!)); }, []);
  const endDrag = useCallback(() => { endPhysicsDrag(engine.current!); setRevision((value) => value + 1); }, []);
  const reset = useCallback(() => {
    const current = latest.current; engine.current = createPhysics(current.data, current.seeds, current.viewport);
    setNodes(physicsNodes(engine.current)); setRevision((value) => value + 1);
  }, []);
  return { nodes, enabled, setEnabled, running, beginDrag, moveNode, endDrag, reset };
}
