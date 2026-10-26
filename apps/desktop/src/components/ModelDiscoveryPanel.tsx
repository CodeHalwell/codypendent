import React, { useState } from "react";
import { describeError, type LocalConfigClient, type ModelDiscovery } from "./localConfig.js";
import { surfaceButton, surfaceStyles } from "./surfaceChrome.js";
import { providerLabel } from "../modelPresentation.js";

export const ModelDiscoveryPanel: React.FC<{
  client: LocalConfigClient;
  onDiscovered?: () => void;
}> = ({ client, onDiscovered }) => {
  const [scanning, setScanning] = useState(false);
  const [report, setReport] = useState<ModelDiscovery | null>(null);
  const [error, setError] = useState<string | null>(null);
  if (!client.discoverModels) return null;

  const scan = async () => {
    setScanning(true);
    setError(null);
    try {
      setReport(await client.discoverModels!());
      onDiscovered?.();
    } catch (failure) {
      setError(describeError(failure));
    } finally {
      setScanning(false);
    }
  };
  const connected = report?.detected.filter((provider) => ["ready", "connected", "listed"].includes(provider.state)) ?? [];
  return (
    <section aria-label="Automatic model discovery" style={{ ...surfaceStyles.card, margin: "16px 24px", flexShrink: 0 }}>
      <div style={{ display: "flex", alignItems: "center", gap: 16, justifyContent: "space-between" }}>
        <div>
          <strong style={{ fontSize: 13, color: "var(--cody-text)" }}>Your models and agents</strong>
          <p style={{ margin: "5px 0 0", fontSize: 12, color: "var(--cody-text-muted)", lineHeight: 1.5 }}>
            Codypendent finds installed agents, local models and existing provider sign-ins automatically.
          </p>
        </div>
        <button type="button" style={surfaceButton()} disabled={scanning} onClick={() => void scan()}>
          {scanning ? "Finding models…" : "Scan again"}
        </button>
      </div>
      {scanning && <p role="status" style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>Checking installed tools and local servers. Your saved model choice stays selected.</p>}
      {error && <p role="alert" style={{ fontSize: 12, color: "var(--cody-danger-text)" }}>{error}</p>}
      {report && <div role="status" style={{ marginTop: 10, fontSize: 12, lineHeight: 1.6 }}>
        {connected.length > 0
          ? `Found ${connected.map((provider) => providerLabel(provider.id, provider.name)).join(", ")}. ${report.added.length > 0 ? `${report.added.length} model${report.added.length === 1 ? "" : "s"} added.` : "Your models are up to date."}`
          : "No signed-in agents or running local models found yet. Sign in to an installed agent or start a local model server, then scan again."}
        {report.default_model && <div style={{ color: "var(--cody-text-muted)" }}>New tasks can use the default from your connected agent or model server.</div>}
        <details style={{ marginTop: 8 }}>
          <summary style={{ cursor: "pointer", color: "var(--cody-text-muted)" }}>Detected tools</summary>
          <ul style={{ paddingLeft: 18, marginBottom: 0 }}>
            {report.detected.map((provider) => <li key={provider.id}><strong>{providerLabel(provider.id, provider.name)}</strong> — {provider.detail}</li>)}
            {report.warnings.map((warning) => <li key={warning}>{warning}</li>)}
          </ul>
        </details>
      </div>}
    </section>
  );
};
