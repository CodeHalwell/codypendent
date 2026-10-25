/**
 * What a failed run says to the operator.
 *
 * A run's failure `reason` is whatever the provider, driver, or ACP agent put
 * on the wire: a nested error chain, a JSON-RPC body, terminal escapes, and —
 * from a misconfigured proxy or an agent echoing its own request — the very
 * credential that failed. The TUI never renders that raw
 * (`crates/tui/src/state.rs::sanitize_failure_text`, `render.rs::summarize_error`);
 * this module is the same treatment for the desktop, ported rule for rule so
 * both clients tell the same story about the same failure.
 */

import contract from "../../../testdata/failure-sanitization.json";

const nameCharacter = (c: string | undefined) => c !== undefined && /^[a-z0-9_-]$/i.test(c);
function quoteEnd(chars: string[], start: number): number {
  let i = start + 1;
  while (i < chars.length && chars[i] !== "\n") {
    if (chars[i] === "\\") i += 1;
    else if (chars[i] === chars[start]) return i + 1;
    i += 1;
  }
  return Math.min(i, chars.length);
}
function spaces(chars: string[], i: number): number {
  while (chars[i] === " " || chars[i] === "\t") i += 1;
  return i;
}
function valueEnd(chars: string[], start: number, wholeLine: boolean): number {
  if (start >= chars.length) return start;
  if (!wholeLine && (chars[start] === '"' || chars[start] === "'")) return quoteEnd(chars, start);
  let depth = 0;
  let i = start;
  while (i < chars.length && chars[i] !== "\n") {
    const c = chars[i];
    if (!wholeLine) {
      if ((c === '"' || c === "'") && depth > 0) { i = quoteEnd(chars, i); continue; }
      if (c === "{" || c === "[") depth += 1;
      else if ((c === "}" || c === "]") && depth > 0) { depth -= 1; if (depth === 0) return i + 1; }
      else if (depth === 0 && (/\s/.test(c) || "&,}]\"';#".includes(c))) break;
    }
    i += 1;
  }
  return i;
}

/** Bounded tokenizer. Shared rules and exact-output vectors cover both clients.
 * JSON values, headers and query parameters have their own delimiters; adjacent
 * error details survive redaction. Raw diagnostics must never be copied. */
export function sanitizeFailureText(raw: string): string {
  const input: string[] = [];
  for (const c of raw) { input.push(c); if (input.length > contract.maxInput) break; }
  const truncated = input.length > contract.maxInput || input.length > contract.maxOutput;
  const chars: string[] = [];
  let i = 0;
  while (i < Math.min(input.length, contract.maxInput) && chars.length < contract.maxOutput) {
    const c = input[i];
    if (c === "\u001b" && input[i + 1] === "[") {
      i += 2;
      while (i < input.length) { const end = input[i] >= "@" && input[i] <= "~"; i += 1; if (end) break; }
      continue;
    }
    if (c === "\u001b" && input[i + 1] === "]") {
      i += 2;
      while (i < input.length) {
        if (input[i] === "\u0007") { i += 1; break; }
        if (input[i] === "\u001b" && input[i + 1] === "\\") { i += 2; break; }
        i += 1;
      }
      continue;
    }
    if (c === "\n" || c === "\t" || !/[\u0000-\u001f\u007f-\u009f\u061c\u200e\u200f\u202a-\u202e\u2066-\u2069]/.test(c)) chars.push(c);
    i += 1;
  }
  let safe = "";
  i = 0;
  while (i < chars.length) {
    if (chars[i] === '"') {
      const end = quoteEnd(chars, i);
      const colon = spaces(chars, end);
      if (chars[colon] === ":") {
        let key: unknown;
        try { key = JSON.parse(chars.slice(i, end).join("")); } catch { /* malformed key remains plain text */ }
        if (typeof key === "string" && contract.keys.includes(key.toLowerCase())) {
          const start = spaces(chars, colon + 1);
          safe += chars.slice(i, start).join("");
          const value = valueEnd(chars, start, false);
          if (chars[start] === '"') {
            safe += '"[REDACTED]';
            if (value > start + 1 && chars[value - 1] === '"') safe += '"';
          } else safe += "[REDACTED]";
          i = value; continue;
        }
      }
    }
    if (nameCharacter(chars[i]) && (i === 0 || !nameCharacter(chars[i - 1]))) {
      let end = i;
      while (end < chars.length && nameCharacter(chars[end])) end += 1;
      const key = chars.slice(i, end).join("").toLowerCase();
      if (contract.prefixes.some((prefix) => key.startsWith(prefix))) {
        safe += "[REDACTED]"; i = valueEnd(chars, i, false); continue;
      }
      const separator = spaces(chars, end);
      const delimiter = chars[separator];
      const explicit = delimiter === ":" || delimiter === "=";
      const label = ["authorization", "bearer", "token", "api_key", "apikey", "password", "secret"].includes(key);
      if (contract.keys.includes(key) && (explicit || (label && separator > end))) {
        const start = spaces(chars, separator + Number(explicit));
        safe += chars.slice(i, start).join("");
        const wholeLine = delimiter !== "=" && ["authorization", "proxy-authorization", "password", "secret", "cookie", "set-cookie"].includes(key);
        safe += "[REDACTED]"; i = valueEnd(chars, start, wholeLine); continue;
      }
    }
    safe += chars[i]; i += 1;
  }
  const flat = safe.split(/\s+/).filter(Boolean).join(" ");
  return Array.from(flat).slice(0, contract.maxOutput - Number(truncated)).join("") + (truncated ? "…" : "");
}

/**
 * One line a person can act on, from a raw error chain. Mirrors the TUI's
 * `summarize_error`: an ACP `details` member wins, then the recognised driver
 * categories, then the outermost segment of the chain.
 */
export function summarizeError(raw: string): string {
  const lower = raw.toLowerCase();
  if (lower.includes("acp")) {
    const marker = raw.indexOf('"details"');
    if (marker !== -1) {
      const tail = raw.slice(marker + '"details"'.length);
      const colon = tail.indexOf(":");
      if (colon !== -1) {
        const value = tail.slice(colon + 1).trim();
        if (value.startsWith('"')) {
          const detail = value
            .slice(1)
            .split('"')[0]
            .replace(/\\n/g, " ")
            .trim();
          if (detail.length > 0) {
            return `ACP — ${detail}`;
          }
        }
      }
    }
    if (lower.includes("prompt failed")) {
      return "ACP agent request failed — expand for details";
    }
  }
  const segments = raw.split(": ").map((segment) => segment.trim());
  const outer = segments[0] ?? "";
  if (
    segments.some(
      (segment) =>
        segment === "model driver error" || segment === "model stream failed",
    )
  ) {
    return "model error — the provider request failed";
  }
  if (
    segments.some(
      (segment) => segment === "service error" || segment === "request failed",
    )
  ) {
    return "provider request failed";
  }
  return outer.length === 0 ? "run failed" : outer;
}

/** Where the operator should go next, when the failure text says. */
export type FailureRemedy = "keys" | "models" | "retry" | "daemon" | "none";

export interface FailureDiagnosis {
  /** The one-line summary, safe to render. */
  summary: string;
  /** The full chain, sanitised. Never the raw reason. */
  detail: string;
  /** Whether the text names an authentication problem. */
  authRelated: boolean;
  remedy: FailureRemedy;
  /** A sentence naming the next step, or `null` when there is nothing to add. */
  hint: string | null;
}

/**
 * Read a failure reason and say what to do about it.
 *
 * The classification is by substring, exactly as the TUI's failure card
 * decides whether to offer `Alt-A re-authenticate`. It is a hint, never a
 * verdict: the full sanitised text stays a click away on the card.
 */
/**
 * The structured half of a failed run, when the daemon sent one: the
 * protocol's `CodypendentError` beside `RunDisposition::Failed.reason`.
 * Only the fields the diagnosis reads are typed here.
 */
export interface StructuredFailure {
  code?: string;
  message?: string;
  retryable?: boolean;
  user_action?: { type?: string } | null;
}

/**
 * The remedy a structured `user_action` names, or `null` when the daemon sent
 * none (or one this build does not know), in which case the text heuristics
 * below decide — exactly as they did before the field existed.
 */
function structuredRemedy(
  error: StructuredFailure | undefined,
): Pick<FailureDiagnosis, "remedy" | "hint" | "authRelated"> | null {
  switch (error?.user_action?.type) {
    case "Reauthenticate":
      return {
        authRelated: true,
        remedy: "keys",
        hint: "The provider refused the credential. Check the key under API Keys, then retry.",
      };
    case "ReconfigureModel":
      return {
        authRelated: false,
        remedy: "models",
        hint: "The model or its endpoint needs attention. Check it under Models, or choose another.",
      };
    case "Retry":
      return {
        authRelated: false,
        remedy: "retry",
        hint: "The provider could not serve the request just now. Wait a moment and retry.",
      };
    case "AdjustPolicy":
      return {
        authRelated: false,
        remedy: "retry",
        hint: "The run hit a budget. Retry starts a fresh one; widen the policy's budgets for more room.",
      };
    default:
      return null;
  }
}

export function diagnoseFailure(
  reason: string,
  error?: StructuredFailure,
): FailureDiagnosis {
  const detail = sanitizeFailureText(reason);
  const summary = summarizeError(detail);
  if (/\b402\b|payment[ _-]required/i.test(detail)) return {
    summary: "The provider requires billing access",
    detail,
    authRelated: false,
    remedy: "none",
    hint: "This provider requires billing or account access before it can serve this model.",
  };
  const structured = structuredRemedy(error);
  if (structured) {
    // The daemon classified the typed cause; that outranks any reading of
    // the text. Its own message is the better summary when it has one.
    return {
      summary: error?.message ? sanitizeFailureText(error.message) : summary,
      detail,
      ...structured,
    };
  }
  const lower = detail.toLowerCase();
  const authRelated = [
    "auth",
    "login",
    "credential",
    "unauthorized",
    "401",
    "403",
    "invalid x-api-key",
    "api key",
  ].some((needle) => lower.includes(needle));
  if (authRelated) {
    return {
      summary,
      detail,
      authRelated,
      remedy: "keys",
      hint: "The provider refused the credential. Check the key under API Keys, then retry.",
    };
  }
  if (
    [
      "429",
      "rate limit",
      "rate_limit",
      "overloaded",
      "quota",
      "too many requests",
    ].some((needle) => lower.includes(needle))
  ) {
    return {
      summary,
      detail,
      authRelated,
      remedy: "retry",
      hint: "The provider is rate-limiting or overloaded. Wait a moment and retry, or choose another model.",
    };
  }
  if (
    [
      "not registered",
      "no candidate model",
      "not configured",
      "unknown model",
      "no model",
    ].some((needle) => lower.includes(needle))
  ) {
    return {
      summary,
      detail,
      authRelated,
      remedy: "models",
      hint: "No usable model is configured for this run. Add or choose one under Models.",
    };
  }
  if (
    [
      "connection refused",
      "dns",
      "timed out",
      "timeout",
      "could not connect",
      "connect error",
      "network",
    ].some((needle) => lower.includes(needle))
  ) {
    return {
      summary,
      detail,
      authRelated,
      remedy: "daemon",
      hint: "The endpoint could not be reached. Check the base URL, that a local server is running, and your network.",
    };
  }
  return { summary, detail, authRelated, remedy: "none", hint: null };
}
