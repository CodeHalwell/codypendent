import { useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import type { UiWireMessage as BrokerMessage } from "@codypendent/protocol";
import type { UiCapabilities, UiCapabilitySelection, UiDocument, UiPatchBatch, UiTheme, UiWireMessage } from "@codypendent/ui";
import {
  RemoteUiRenderer, RemoteUiStore, createHostReactCapabilities,
  applyWireTheme, publishMediatedWire, isMediatedRuntimeWire, REMOTE_UI_WIRE_SEND_EVENT,
  type RemoteUiPlacement, type ApplyResult,
} from "@codypendent/ui/host-react";
import type { DesktopTransport } from "../transport.js";
import { subscribeToFrames } from "../frameBus.js";
import "./remoteUi.css";

/** Ingress is the native protocol decoder and daemon broker. Placement must
 * come from their contribution attestations, never document metadata. */
export function applyBrokerMessage(store: RemoteUiStore, message: BrokerMessage): ApplyResult | undefined {
  const registrations = (message.contributions ?? []).map((entry) => ({
    documentId: entry.documentId,
    placement: {
      point: entry.point,
      extensionId: typeof entry.metadata?.hostExtensionId === "string" ? entry.metadata.hostExtensionId : entry.extensionId,
      ownerScope: entry.extensionId,
      slot: entry.slot,
      ...(typeof entry.metadata?.hostPublisher === "string" ? { publisher: entry.metadata.hostPublisher } : {}),
      ...(typeof entry.metadata?.hostTrust === "string" ? { trust: entry.metadata.hostTrust } : {}),
      ...(entry.priority == null ? {} : { priority: entry.priority }),
    } satisfies RemoteUiPlacement,
  }));
  if (message.type === "contributions") {
    const owner = message.extensions?.contributionOwner;
    if (typeof owner !== "string" || !store.replaceContributions(owner, registrations)) {
      return store.apply({ type: "error", code: "extension.placement", message: "Invalid extension placement received from the broker" });
    }
  } else {
    for (const { documentId, placement } of registrations) store.setPlacement(documentId, placement);
  }
  if (message.type === "snapshot" && message.snapshot) {
    return store.apply({ type: "snapshot", document: message.snapshot.document as unknown as UiDocument });
  }
  if (message.type === "patchBatch" && message.patchBatch) {
    return store.apply({ type: "patch", batch: { ...message.patchBatch, atomic: message.patchBatch.atomic ?? true } as unknown as UiPatchBatch });
  }
  if (message.type === "dispose" && message.dispose) return store.apply({ type: "dispose", ...message.dispose });
  if (message.type === "error" && message.error) return store.apply({
    type: "error", code: message.error.code, message: message.error.message,
    ...(message.error.documentId == null ? {} : { documentId: message.error.documentId }),
  });
  if (message.type === "projection" || message.type === "actionResult") {
    publishMediatedWire(message as unknown as UiWireMessage);
  }
  return undefined;
}

/** One host per confirmed attachment. Reconnecting remounts and renegotiates. */
export function RemoteUiHost({ transport, sessionId }: { transport: DesktopTransport; sessionId: string }) {
  const container = useRef<HTMLElement>(null);
  const hiddenDocuments = useRef(new Set<string>());
  const [theme, setTheme] = useState<UiTheme | null>(null);
  const offer = useMemo(() => createHostReactCapabilities(undefined, "desktop"), []);
  const [capabilities, setCapabilities] = useState(offer);
  const store = useMemo(() => new RemoteUiStore(offer), [offer]);
  const snapshot = useSyncExternalStore(store.subscribe, store.getSnapshot, store.getSnapshot);
  const [error, setError] = useState<string | null>(null);
  const [hasContributions, setHasContributions] = useState(false);
  const send = (message: UiWireMessage) => {
    void transport.sendRemoteUi?.(sessionId, message).catch((error: unknown) => {
      setError(error instanceof Error ? error.message : String(error));
    });
  };
  const id = () => crypto.randomUUID();

  useEffect(() => {
    let active = true;
    let negotiated = false;
    const requestedResync = new Set<string>();
    const contributionOwners = new Map<string, number>();
    const deliver = (message: UiWireMessage) => {
      void transport.sendRemoteUi?.(sessionId, message).catch((error: unknown) => {
        if (active) setError(error instanceof Error ? error.message : String(error));
      });
    };
    const unsubscribe = subscribeToFrames((frame) => {
      if (frame.kind !== "remote_ui" || frame.session_id !== sessionId) return;
      if (frame.message.type === "capabilitySelection" && frame.message.selection) {
        const selection = frame.message.selection as unknown as UiCapabilitySelection;
        if (!offer.protocolVersions.some((version) => version.major === selection.protocolVersion.major && version.minor >= selection.protocolVersion.minor)) {
          negotiated = false;
          store.clear();
          setError("The daemon selected an unsupported extension protocol. Update Codypendent.");
          return;
        }
        const limits = { ...offer.limits! };
        for (const name of Object.keys(limits) as (keyof typeof limits)[]) {
          const selected = selection.limits[name];
          if (!Number.isSafeInteger(selected) || selected < 0) {
            negotiated = false;
            store.clear();
            setError("The daemon selected invalid extension limits.");
            return;
          }
          limits[name] = Math.min(limits[name], selected);
        }
        const selectedCapabilities: UiCapabilities = {
          ...offer,
          primitives: selection.primitives.filter((primitive) => offer.primitives.includes(primitive)),
          capabilities: selection.capabilities.filter((capability) => offer.capabilities?.includes(capability)),
          contributionPoints: selection.contributionPoints.filter((point) => offer.contributionPoints?.includes(point)),
          limits,
        };
        store.setCapabilities(selectedCapabilities);
        setCapabilities(selectedCapabilities);
        negotiated = true;
        setError(null);
        return;
      }
      if (!negotiated) return;
      if (frame.message.type === "contributions") {
        const owner = frame.message.extensions?.contributionOwner;
        if (typeof owner === "string") {
          contributionOwners.set(owner, frame.message.contributions?.length ?? 0);
          setHasContributions([...contributionOwners.values()].some((count) => count > 0));
        }
      }
      if (frame.message.type === "theme" && frame.message.theme) {
        setTheme(frame.message.theme as UiTheme);
        return;
      }
      const documentId = frame.message.snapshot?.document.documentId ?? frame.message.patchBatch?.documentId;
      if (documentId && hiddenDocuments.current.has(documentId)) return;
      const result = applyBrokerMessage(store, frame.message);
      if (result?.applied && result.documentId) requestedResync.delete(result.documentId);
      if (result?.resync && !requestedResync.has(result.resync.documentId)) {
        requestedResync.add(result.resync.documentId);
        deliver({ type: "resync", messageId: id(), resync: result.resync });
      }
    });
    const mediated = (event: Event) => {
      if (event instanceof CustomEvent && isMediatedRuntimeWire(event.detail)) deliver(event.detail);
    };
    const resize = () => deliver({ type: "viewport", messageId: id(), viewport: createHostReactCapabilities(undefined, "desktop").viewport });
    window.addEventListener(REMOTE_UI_WIRE_SEND_EVENT, mediated);
    window.addEventListener("resize", resize);
    // Subscribe before negotiating: a daemon can answer immediately.
    deliver({ type: "capabilities", messageId: id(), capabilities: offer });
    return () => {
      active = false;
      unsubscribe();
      window.removeEventListener(REMOTE_UI_WIRE_SEND_EVENT, mediated);
      window.removeEventListener("resize", resize);
      store.clear();
    };
  }, [transport, sessionId, offer, store]);

  useEffect(() => {
    if (container.current && theme) applyWireTheme(container.current, theme);
  }, [theme, snapshot.mounts.length]);

  const documentError = Object.keys(snapshot.errors).some((documentId) => documentId !== "host");
  if (snapshot.mounts.length === 0 && !documentError && (!hasContributions || (!error && Object.keys(snapshot.errors).length === 0))) return null;
  return <aside ref={container} className="desktop-extension-host" aria-label="Extension surfaces">
    {error && <div role="alert">Extension connection: {error}</div>}
    <header style={{ fontWeight: 600, marginBottom: 12 }}>{snapshot.mounts.length > 0 ? `Live Extensions (${snapshot.mounts.length})` : "Extension needs attention"}</header>
    <RemoteUiRenderer store={store} capabilities={capabilities}
      dispatch={(event) => send({ type: "event", messageId: id(), event })}
      recover={(request) => {
        if (request.action === "retry") {
          send({ type: "resync", messageId: id(), resync: { documentId: request.documentId } });
        } else if (request.action === "disable") {
          hiddenDocuments.current.add(request.documentId);
          const mount = store.getSnapshot().mounts.find((mount) => mount.document.documentId === request.documentId);
          if (mount) store.apply({ type: "dispose", documentId: request.documentId, revision: mount.document.revision });
          setError("Surface hidden for this view. Manage installed extensions in Plugins.");
        } else {
          setError(`${request.documentId}: ${request.message}`);
        }
      }} />
  </aside>;
}
