/** Display names only. Provider IDs and model coordinates remain unchanged. */
const PROVIDERS: Record<string, string> = {
  openai: "OpenAI", anthropic: "Anthropic", google: "Google", gemini: "Gemini", "gemini-cli": "Gemini",
  "codex-acp": "Codex", codex: "Codex", "claude-acp": "Claude", "claude-code": "Claude",
  copilot: "GitHub Copilot", "github-copilot-cli": "GitHub Copilot", github: "GitHub",
  opencode: "OpenCode", cline: "Cline", amp: "Amp", "amp-acp": "Amp", zcode: "ZCode", "zcode-acp": "ZCode",
  "kimi-code": "Kimi Code", kimi: "Kimi", moonshotai: "Moonshot AI", moonshot: "Moonshot AI",
  "antigravity-acp": "Antigravity", antigravity: "Antigravity", cursor: "Cursor", "cursor-agent": "Cursor",
  ollama: "Ollama", "ollama-cloud": "Ollama Cloud", cerebras: "Cerebras", groq: "Groq",
  openrouter: "OpenRouter", azure: "Azure", "azure-openai": "Azure OpenAI", mistral: "Mistral",
  "mistral-vibe": "Mistral Vibe", deepseek: "DeepSeek", nebius: "Nebius", nvidia: "NVIDIA",
  minimax: "MiniMax", minimaxai: "MiniMax", "opencode-go": "OpenCode Go", "openai-compatible": "Compatible API",
  zai: "Z.AI", zhipu: "Z.AI", "account:zai-individual-coding-plan": "Z.AI", "account:zai-enterprise-coding-plan": "Z.AI",
};

export function providerLabel(id: string, fallback?: string): string {
  const key = id.replace(/^acp\//, "").split(/[@#]/)[0].toLowerCase();
  if (fallback && /[A-Z]/.test(fallback) && !/[#]/.test(fallback)) return fallback;
  return PROVIDERS[key] ?? fallback ?? words(key);
}

function words(value: string): string {
  return value.replace(/[_-]+/g, " ").replace(/\b[a-z]/g, (letter) => letter.toUpperCase());
}

/** Keep versions and variants distinct, without exposing routing syntax. */
export function modelVersionLabel(value: string): string {
  const context = value.match(/\[(\d+(?:\.\d+)?[km])\]$/i)?.[1];
  const cloud = /:cloud$/i.test(value);
  let name = value.replace(/\[\d+(?:\.\d+)?[km]\]$/i, "").replace(/:cloud$/i, "");
  if (name.includes("/")) name = name.slice(name.lastIndexOf("/") + 1);
  const suffix = name.match(/:([^:]+)$/)?.[1];
  if (suffix) name = name.slice(0, -(suffix.length + 1));
  name = words(name)
    .replace(/\bGpt\s+(\d)/gi, "GPT-$1").replace(/\bGpt\b/gi, "GPT")
    .replace(/\bDeepseek\b/gi, "DeepSeek").replace(/\bGlm\s+(\d)/gi, "GLM-$1").replace(/\bGlm\b/gi, "GLM")
    .replace(/\bMinimax\b/g, "MiniMax").replace(/\bQwen\b/g, "Qwen").replace(/\bNvfp4\b/g, "NVFP4")
    .replace(/\bOss\b/g, "OSS").replace(/\b(\d+(?:\.\d+)?)b\b/gi, "$1B");
  const hints = [suffix ? words(suffix) : null, context ? `${context.toUpperCase()} context` : null, cloud ? "Cloud" : null].filter(Boolean);
  return [name, ...hints].join(" · ");
}

export function modelLabel(id: string): string {
  if (!id) return "Choose model";
  if (id.startsWith("acp/")) {
    const coordinate = id.slice(4);
    const marker = coordinate.indexOf("#");
    const agent = providerLabel(marker < 0 ? coordinate : coordinate.slice(0, marker));
    if (marker < 0) return `${agent} · Default`;
    let version = modelVersionLabel(coordinate.slice(marker + 1));
    if (agent === "Claude") version = version.replace(/^Claude\s+/i, "");
    if (agent === "Gemini") version = version.replace(/^Gemini\s+/i, "");
    const selector = coordinate.slice(marker + 1);
    const route = ["OpenCode", "Cline"].includes(agent) && selector.includes("/")
      ? providerLabel(selector.slice(0, selector.indexOf("/"))) : null;
    return `${agent} · ${version}${route ? ` · ${route}` : ""}`;
  }
  const slash = id.indexOf("/");
  if (slash > 0 && PROVIDERS[id.slice(0, slash).toLowerCase()]) return modelVersionLabel(id.slice(slash + 1));
  // Custom user names are labels already, rather than model coordinates.
  if (/\s/.test(id)) return id;
  return modelVersionLabel(id);
}

/** Message attribution is the recorded identity, never the current selection. */
export function messageModelLabel(id?: string): string {
  if (!id) return "Model not recorded";
  if (id.startsWith("acp/") && (!id.includes("#") || /#(?:default|auto)$/i.test(id))) {
    return `${providerLabel(id)} · Model not reported`;
  }
  return modelLabel(id);
}

export function modelProviderLabel(id: string, providerId?: string | null, provider?: string): string {
  if (providerId) return providerLabel(providerId);
  if (id.startsWith("acp/")) {
    const coordinate = id.slice(4);
    const selected = coordinate.split("#")[1];
    if (selected?.includes("/")) return providerLabel(selected.slice(0, selected.indexOf("/")));
    const agent = coordinate.split(/[@#]/)[0];
    return ({ "claude-acp": "Anthropic", "claude-code": "Anthropic", "codex-acp": "OpenAI", "gemini-cli": "Google", "antigravity-acp": "Google" } as Record<string, string>)[agent] ?? providerLabel(agent);
  }
  const prefix = id.split("/")[0];
  return PROVIDERS[prefix] ?? providerLabel(provider ?? prefix);
}

export function runDefaultsLabel(label: string | null | undefined): string {
  if (!label) return "Build mode";
  const marker = label.indexOf(" · ");
  return marker < 0 ? label : `${label.slice(0, marker)} · ${modelLabel(label.slice(marker + 3))}`;
}
