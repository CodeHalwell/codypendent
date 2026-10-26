import type { DesktopToolCall, GeneratedUi, HostCall, Json } from "./types.js";
import { convertFileSrc } from "@tauri-apps/api/core";
import { shellAvailable } from "../components/localConfig.js";

const LIMIT = 256 * 1024;
const CSP = "default-src 'none'; script-src 'unsafe-inline' blob:; style-src 'unsafe-inline'; img-src data: blob:; worker-src blob:; connect-src 'none'; base-uri 'none'; form-action 'none'";
type SandboxPayload = { token: string; widget?: GeneratedUi; body?: string; args?: Json };

export function boundedJson(value: unknown): Json {
  const text = JSON.stringify(value);
  if (text === undefined || new TextEncoder().encode(text).length > LIMIT) throw new Error("Tool result exceeds the 256 KB limit or is not JSON.");
  return JSON.parse(text) as Json;
}

function sandboxMain(payload: SandboxPayload) {
  const send = (kind: string, value: unknown) => parent.postMessage({ token: payload.token, kind, value }, "*");
  if (payload.widget) {
    const ui = payload.widget;
    const style = document.createElement("style");
    style.textContent = `:root{color-scheme:light dark}body{margin:16px;font:14px/1.5 system-ui;overflow-wrap:anywhere}button,input,select{font:inherit}*{box-sizing:border-box}` + (ui.css ?? "");
    document.head.append(style);
    document.body.innerHTML = ui.html;
    const script = document.createElement("script");
    script.textContent = `${ui.jsFunctions ?? ""}\n${(ui.jsExpressions ?? []).join(";\n")}`;
    window.addEventListener("error", (event) => send("error", event.message));
    document.body.append(script);
    new ResizeObserver(() => send("height", Math.ceil(document.body.scrollHeight + 24))).observe(document.body);
    send("ready", null);
    return;
  }
  const source = `const args=${JSON.stringify(payload.args)};const pending=new Map();let seq=0;const host=Object.freeze({call:(name,args)=>new Promise((resolve,reject)=>{const id=++seq;pending.set(id,{resolve,reject});postMessage({kind:'host',id,name,args})})});onmessage=({data})=>{const p=pending.get(data.id);if(p){pending.delete(data.id);data.error?p.reject(new Error(data.error)):p.resolve(data.result)}};(async()=>{${payload.body}\n})().then(value=>postMessage({kind:'result',value}),error=>postMessage({kind:'error',value:String(error?.message??error)}));`;
  const url = URL.createObjectURL(new Blob([source], { type: "text/javascript" }));
  const worker = new Worker(url);
  URL.revokeObjectURL(url);
  worker.onmessage = ({ data }) => send(data.kind, data);
  worker.onerror = (event) => send("error", event.message);
  window.addEventListener("message", ({ source, data }) => {
    if (source !== parent || data?.token !== payload.token) return;
    if (data.kind === "reply") worker.postMessage(data.value);
    if (data.kind === "dispose") worker.terminate();
  });
}

function sandboxScript(payload: SandboxPayload): string {
  const safe = JSON.stringify(payload).replaceAll("<", "\\u003c");
  return `(${sandboxMain.toString()})(${safe})`;
}

export function sandboxUrl(payload: SandboxPayload): string {
  if (shellAvailable()) return convertFileSrc("", "cody-sandbox");
  const html = `<!doctype html><html><head><meta http-equiv="Content-Security-Policy" content="${CSP}"><meta name="referrer" content="no-referrer"></head><body><script>${sandboxScript(payload)}</script></body></html>`;
  return `data:text/html;base64,${btoa(Array.from(new TextEncoder().encode(html), (byte) => String.fromCharCode(byte)).join(""))}`;
}

export function sandboxHandshake(frame: HTMLIFrameElement, payload: SandboxPayload) {
  let sent = false;
  return (event: MessageEvent): boolean => {
    if (event.source !== frame.contentWindow || event.data?.kind !== "cody-sandbox-ready") return false;
    if (!sent) {
      sent = true;
      frame.contentWindow?.postMessage({ kind: "cody-sandbox-init", source: sandboxScript(payload) }, "*");
    }
    return true;
  };
}

export function executeGeneratedTool(call: DesktopToolCall, hostCall: HostCall, signal: AbortSignal): Promise<Json> {
  return new Promise((resolve, reject) => {
    if (signal.aborted) { reject(new Error("The chat changed before this tool could run.")); return; }
    if (!call.definition.functionBody || call.definition.name !== call.name) { reject(new Error("Generated tool definition is missing.")); return; }
    const token = crypto.randomUUID();
    const frame = document.createElement("iframe");
    frame.sandbox.add("allow-scripts");
    frame.hidden = true;
    frame.title = "Agent tool sandbox";
    const payload = { token, body: call.definition.functionBody, args: call.args };
    frame.src = sandboxUrl(payload);
    const handshake = sandboxHandshake(frame, payload);
    let finished = false;
    let requests = 0;
    const finish = (error?: Error, result?: unknown) => {
      if (finished) return;
      finished = true;
      clearTimeout(timer);
      signal.removeEventListener("abort", abort);
      window.removeEventListener("message", receive);
      frame.contentWindow?.postMessage({ token, kind: "dispose" }, "*");
      frame.remove();
      if (error) reject(error);
      else { try { resolve(boundedJson(result ?? null)); } catch (error) { reject(error); } }
    };
    const abort = () => finish(new Error("The chat changed while this tool was running."));
    const receive = (event: MessageEvent) => {
      if (!finished && handshake(event)) return;
      if (event.source !== frame.contentWindow || event.data?.token !== token || finished) return;
      const { kind, value } = event.data;
      if (kind === "result") finish(undefined, value.value);
      else if (kind === "error") finish(new Error(typeof value === "string" ? value : String(value.value)));
      else if (kind === "host") {
        const { id, name, args } = value;
        void (async () => {
          let result: Json | undefined;
          let error: string | undefined;
          try {
            if (++requests > 32 || !call.definition.capabilities.includes(name)) throw new Error("This tool has no permission for that host capability.");
            result = boundedJson(await hostCall(name, boundedJson(args ?? {})));
          } catch (reason) { error = reason instanceof Error ? reason.message : String(reason); }
          if (!finished) frame.contentWindow?.postMessage({ token, kind: "reply", value: { id, result, error } }, "*");
        })();
      }
    };
    const timer = setTimeout(() => finish(new Error("Generated tool exceeded its five second limit.")), 5000);
    window.addEventListener("message", receive);
    signal.addEventListener("abort", abort, { once: true });
    document.body.append(frame);
  });
}
