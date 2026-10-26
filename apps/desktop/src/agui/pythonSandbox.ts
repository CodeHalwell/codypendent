import { Channel, invoke } from "@tauri-apps/api/core";
import type { DesktopToolCall, HostCall, Json } from "./types.js";
import { boundedJson } from "./sandbox.js";

type HostRequest = { requestId: number; name: string; args: Json };

export async function executePythonTool(call: DesktopToolCall, hostCall: HostCall, signal: AbortSignal): Promise<Json> {
  if (signal.aborted) throw new Error("The chat changed before this helper could run.");
  const scope = { sessionId: call.sessionId, callId: call.callId };
  const cancel = () => { void invoke("desktop_tools_python_cancel", scope).catch(() => {}); };
  const channel = new Channel<HostRequest>();
  channel.onmessage = (request) => {
    void (async () => {
      let result: Json | undefined;
      let error: string | undefined;
      try {
        if (signal.aborted || !call.definition.capabilities.includes(request.name)) throw new Error("This helper no longer has access to that workspace capability.");
        result = boundedJson(await hostCall(request.name, boundedJson(request.args)));
      } catch (reason) { error = reason instanceof Error ? reason.message : String(reason); }
      await invoke("desktop_tools_python_reply", { ...scope, requestId: request.requestId, result: result ?? null, error: error ?? null }).catch(() => {});
    })();
  };
  signal.addEventListener("abort", cancel, { once: true });
  try {
    const result = await invoke<Json>("desktop_tools_python_execute", { ...scope, onHost: channel });
    if (signal.aborted) throw new Error("The chat changed while this helper was running.");
    return boundedJson(result);
  } finally {
    signal.removeEventListener("abort", cancel);
  }
}
