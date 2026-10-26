import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { shellAvailable } from "../components/localConfig.js";
import { createHostCall, HOST_TOOLS, type HostBindings } from "./hostTools.js";
import { executeGeneratedTool } from "./sandbox.js";
import { executePythonTool } from "./pythonSandbox.js";
import { setDesktopManifest, type DesktopContext, type DesktopToolCall } from "./types.js";

export function useDesktopTools(enabled: boolean, sessionId: string | null, context: DesktopContext, bindings: HostBindings) {
  const current = useRef(bindings);
  current.current = bindings;
  const [error, setError] = useState<string | null>(null);
  const contextKey = JSON.stringify(context);
  const latestContext = useRef(contextKey);
  latestContext.current = contextKey;
  useLayoutEffect(() => {
    setDesktopManifest(enabled && shellAvailable() ? { context: JSON.parse(contextKey) as DesktopContext, tools: HOST_TOOLS } : null);
    return () => setDesktopManifest(null);
  }, [enabled, contextKey]);
  const scope = useRef<AbortController | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    scope.current = controller;
    return () => { controller.abort(); };
  }, [enabled, sessionId]);
  useEffect(() => {
    if (!enabled || !sessionId || !shellAvailable()) return;
    let stopped = false;
    const publish = () => void invoke("desktop_tools_publish", { sessionId, context: JSON.parse(latestContext.current), tools: HOST_TOOLS })
      .then(() => { if (!stopped) setError(null); })
      .catch((reason) => { if (!stopped) setError(String(reason)); });
    publish();
    const timer = setInterval(publish, 15000);
    return () => { stopped = true; clearInterval(timer); };
  }, [enabled, sessionId]);
  useEffect(() => {
    if (!enabled || !sessionId || !shellAvailable()) return;
    let stopped = false;
    const timer = setTimeout(() => void invoke("desktop_tools_publish", { sessionId, context: JSON.parse(contextKey), tools: HOST_TOOLS })
      .catch((reason) => { if (!stopped) setError(String(reason)); }), 200);
    return () => { stopped = true; clearTimeout(timer); };
  }, [enabled, sessionId, contextKey]);
  useEffect(() => {
    if (!enabled || !sessionId || !shellAvailable()) return;
    let stopped = false;
    let timer: ReturnType<typeof setTimeout>;
    const completed = new Set<string>();
    const hostCall = createHostCall(() => current.current);
    const poll = async () => {
      try {
        const call = await invoke<DesktopToolCall | null>("desktop_tools_pending", { sessionId });
        if (stopped) return;
        if (call && call.sessionId === sessionId && !completed.has(call.callId)) {
          completed.add(call.callId);
          let result: unknown;
          let failure: string | undefined;
          try {
            if (call.expiresAt <= Date.now()) throw new Error("This tool request expired.");
            if (call.definition.language === "python") result = await executePythonTool(call, hostCall, scope.current!.signal);
            else if (call.definition.functionBody) result = await executeGeneratedTool(call, hostCall, scope.current!.signal);
            else result = await hostCall(call.name, call.args);
          } catch (reason) { failure = reason instanceof Error ? reason.message : String(reason); }
          await invoke("desktop_tools_resolve", { sessionId, callId: call.callId, result: result ?? null, error: failure ?? null });
        }
      } catch (reason) { if (!stopped) setError(String(reason)); }
      finally { if (!stopped) timer = setTimeout(() => void poll(), 500); }
    };
    void poll();
    return () => { stopped = true; clearTimeout(timer); };
  }, [enabled, sessionId]);
  return { error, tools: HOST_TOOLS };
}
