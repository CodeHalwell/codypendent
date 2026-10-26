import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { RemoteUiStore, createHostReactCapabilities } from "@codypendent/ui/host-react";
import type { UiDocument } from "@codypendent/ui";
import type { UiWireMessage } from "@codypendent/protocol";
import { RemoteUiHost, applyBrokerMessage } from "../src/components/RemoteUiHost.js";
import type { DesktopTransport } from "../src/transport.js";
import { publishFrame, resetFrameBus } from "../src/frameBus.js";

afterEach(resetFrameBus);
const doc: UiDocument = { protocolVersion: { major: 1, minor: 0 }, documentId: "doc", revision: 1,
  metadata: { source: "untrusted-title" }, root: { kind: "element", id: "button", type: "Button", props: { label: "Run extension action", action: "go" }, children: [] } };
const snapshot = { type: "snapshot", messageId: "m1", snapshot: { document: doc } } as UiWireMessage;
const contributions: UiWireMessage = { type: "contributions", messageId: "m2", extensions: { contributionOwner: "generation-1" }, contributions: [
  { id: "registration", documentId: "doc", extensionId: "generation-1", point: "panel", slot: "tools", metadata: { hostExtensionId: "verified.extension" } },
] };
const publish = (message: UiWireMessage, session_id = "session") => publishFrame({ kind: "remote_ui", session_id, message });

describe("desktop daemon extension delivery", () => {
  it("requires broker placement, applies patches atomically and respects disposal revisions", () => {
    const store = new RemoteUiStore(createHostReactCapabilities(undefined, "desktop"));
    expect(applyBrokerMessage(store, snapshot)?.applied).toBe(false);
    applyBrokerMessage(store, contributions);
    expect(applyBrokerMessage(store, snapshot)?.applied).toBe(true);
    expect(store.getSnapshot().mounts[0].placement.extensionId).toBe("verified.extension");
    const bad = { type: "patchBatch", messageId: "patch", patchBatch: { documentId: "doc", protocolVersion: { major: 1, minor: 0 }, baseRevision: 0, revision: 2, patches: [] } } as UiWireMessage;
    expect(applyBrokerMessage(store, bad)?.resync?.knownRevision).toBe(1);
    expect(store.getSnapshot().mounts[0].document.revision).toBe(1);
    const valid = { ...bad, messageId: "valid", patchBatch: { ...bad.patchBatch!, baseRevision: 1, revision: 2, patches: [{ op: "updateProps", nodeId: "button", set: { label: "Updated action" } }] } } as UiWireMessage;
    expect(applyBrokerMessage(store, valid)?.applied).toBe(true);
    expect(store.getSnapshot().mounts[0].document.revision).toBe(2);
    expect(applyBrokerMessage(store, { type: "dispose", messageId: "d", dispose: { documentId: "doc", revision: 0 } })?.applied).toBe(false);
    applyBrokerMessage(store, { type: "dispose", messageId: "d2", dispose: { documentId: "doc", revision: 2 } });
    expect(store.getSnapshot().mounts).toHaveLength(0);
  });

  it("negotiates after subscribing, routes interaction and clears on attachment replacement", async () => {
    const sendRemoteUi = vi.fn().mockResolvedValue(undefined);
    const transport = { sendRemoteUi } as unknown as DesktopTransport;
    const { rerender } = render(<RemoteUiHost key="session" transport={transport} sessionId="session" />);
    expect(sendRemoteUi).toHaveBeenCalledWith("session", expect.objectContaining({ type: "capabilities" }));
    await act(async () => {
      const offer = createHostReactCapabilities(undefined, "desktop");
      publish({ type: "capabilitySelection", messageId: "selection", selection: { colorDepth: 24, imageProtocols: [], mouse: true, screenReader: true, unicode: true, protocolVersion: { major: 1, minor: 0 }, primitives: [...offer.primitives], capabilities: [...offer.capabilities!], contributionPoints: [...offer.contributionPoints!], limits: offer.limits! } });
      publish(contributions); publish(snapshot, "old-session"); });
    expect(screen.queryByRole("button", { name: "Run extension action" })).toBeNull();
    await act(async () => publish(snapshot));
    await act(async () => {
      const bad = { type: "patchBatch", messageId: "gap", patchBatch: { documentId: "doc", protocolVersion: { major: 1, minor: 0 }, baseRevision: 5, revision: 6, patches: [] } } as UiWireMessage;
      publish(bad); publish(bad);
    });
    expect(sendRemoteUi.mock.calls.filter(([, message]) => message.type === "resync")).toHaveLength(1);
    fireEvent.click(screen.getByRole("button", { name: "Run extension action" }));
    expect(sendRemoteUi).toHaveBeenLastCalledWith("session", expect.objectContaining({ type: "event", event: expect.objectContaining({ documentId: "doc" }) }));
    rerender(<RemoteUiHost key="next" transport={transport} sessionId="next" />);
    expect(screen.queryByRole("button", { name: "Run extension action" })).toBeNull();
    expect(sendRemoteUi).toHaveBeenLastCalledWith("next", expect.objectContaining({ type: "capabilities" }));
  });
});
