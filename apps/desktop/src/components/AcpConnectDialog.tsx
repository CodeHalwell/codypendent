import { useRef, useState } from "react";
import { useFocusTrap } from "../useFocusTrap.js";
import { describeError, type LocalConfigClient, type ProviderRow } from "./localConfig.js";

/** A host-owned, explicit launch/install decision. Nothing runs on selection. */
export function AcpConnectDialog({ provider, repositoryPath, client, onClose, onConnected }: {
  provider: ProviderRow; repositoryPath?: string; client: LocalConfigClient;
  onClose(): void; onConnected(): void;
}) {
  const dialog = useRef<HTMLDivElement>(null);
  const cancel = useRef<HTMLButtonElement>(null);
  const inFlight = useRef(false);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [acceptedRisk, setAcceptedRisk] = useState(false);
  useFocusTrap(dialog, { active: true, initialFocus: cancel, onEscape: () => { if (!inFlight.current) onClose(); } });
  async function connect() {
    if (inFlight.current || !repositoryPath || !client.connectAcp ||
      (provider.community_consent_required && !acceptedRisk)) return;
    inFlight.current = true;
    setPending(true);
    setError(null);
    try {
      await client.connectAcp(provider.id, repositoryPath, acceptedRisk);
      onConnected();
    } catch (error) {
      setError(describeError(error));
    } finally {
      inFlight.current = false;
      setPending(false);
    }
  }
  return <div ref={dialog} role="dialog" aria-modal="true" aria-labelledby="acp-connect-title"
    style={{ borderTop: "1px solid var(--cody-border-strong)", background: "var(--cody-panel-raised)", padding: "16px 24px" }}>
    <h3 id="acp-connect-title" style={{ margin: 0 }}>Connect {provider.name}</h3>
    <p>This starts the agent in the selected repository to check its connection and discover its models. A missing adapter may be downloaded using its pinned distribution.</p>
    <p>{repositoryPath ? `Repository: ${repositoryPath}` : "Choose a repository in the sidebar before connecting an agent."}</p>
    <p>Your existing agent sign-in is used. Model profiles are saved only after a successful handshake.</p>
    {provider.community_consent_required && <label style={{ display: "block", color: "var(--cody-warning)" }}>
      <input type="checkbox" checked={acceptedRisk} disabled={pending} onChange={(event) => setAcceptedRisk(event.target.checked)} />
      {provider.community_consent_detail} I accept this third-party risk.
    </label>}
    {error && <p role="alert">{error}</p>}
    {pending && <p role="status">Connecting and discovering models…</p>}
    <div style={{ display: "flex", gap: 8, marginTop: 12 }}>
      <button ref={cancel} type="button" disabled={pending} onClick={onClose} style={{ padding: "8px 12px", borderRadius: 6, border: "1px solid var(--cody-border-strong)", background: "var(--cody-inset)", color: "var(--cody-text)", font: "inherit" }}>Cancel</button>
      <button type="button" disabled={pending || !repositoryPath || !provider.available || !client.connectAcp ||
        (provider.community_consent_required && !acceptedRisk)} onClick={() => void connect()} style={{ padding: "8px 12px", borderRadius: 6, border: "1px solid var(--cody-border-strong)", background: "var(--cody-accent-strong)", color: "var(--cody-on-accent)", font: "inherit" }}>Connect agent</button>
    </div>
  </div>;
}
