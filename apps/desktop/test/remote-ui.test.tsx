import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { RemoteUiHost } from "../src/components/RemoteUiHost.js";
import type { DesktopTransport } from "../src/transport.js";
import type { UiWireMessage } from "@codypendent/protocol";
import { publishFrame, resetFrameBus } from "../src/frameBus.js";
import { createHostReactCapabilities } from "@codypendent/ui/host-react";
afterEach(resetFrameBus);
function send(message: UiWireMessage) { publishFrame({ kind: "remote_ui", session_id: "session", message }); }
function mounted(documents: Map<string, UiDocument>, onEvent?: (event: UiEvent) => void) {
  const transport = { sendRemoteUi: async (_session: string, message: import("@codypendent/ui").UiWireMessage) => {
    if (message.type === "event") onEvent?.(message.event);
  } } as unknown as DesktopTransport;
  const result = render(<RemoteUiHost transport={transport} sessionId="session" />);
  act(() => {
    const offer = createHostReactCapabilities(undefined, "desktop");
    send({ type: "capabilitySelection", messageId: "selection", selection: { colorDepth: 24, imageProtocols: [], mouse: true, screenReader: true, unicode: true, protocolVersion: UI_PROTOCOL_VERSION, primitives: [...offer.primitives], capabilities: [...offer.capabilities!], contributionPoints: [...offer.contributionPoints!], limits: offer.limits! } });
    send({ type: "contributions", messageId: "registrations", extensions: { contributionOwner: "producer" }, contributions: [...documents.keys()].map((documentId) => ({ id: documentId, documentId, extensionId: "producer", point: "panel", slot: "tools", metadata: { hostExtensionId: "verified.extension" } })) });
    for (const document of documents.values()) send({ type: "snapshot", messageId: document.documentId, snapshot: { document } } as UiWireMessage);
  });
  return result;
}
import { UI_PROTOCOL_VERSION, type UiDocument, type UiEvent } from "@codypendent/ui";

describe("Desktop RemoteUiRenderer", () => {
  it("returns null when there are no active documents", () => {
    const { container } = mounted(new Map());
    expect(container.firstChild).toBeNull();
  });

  it("keeps an empty extension rail hidden after a host-level transport error", () => {
    const { container } = mounted(new Map());
    act(() => send({ type: "error", messageId: "host-error", error: { code: "ui.host.rejected", message: "capabilities-renegotiation", documentId: "host" } }));
    expect(container.firstChild).toBeNull();
  });

  it("shows an actionable failed contribution without a zero-extension heading", () => {
    mounted(new Map());
    act(() => {
      send({ type: "contributions", messageId: "declared", extensions: { contributionOwner: "verified-producer" }, contributions: [{ id: "failed", documentId: "failed-doc", extensionId: "verified-producer", point: "panel", slot: "tools" }] });
      send({ type: "error", messageId: "failed-widget", error: { code: "ui.document.invalid", message: "The extension document could not load", documentId: "failed-doc" } });
    });
    expect(screen.getByText("Extension needs attention")).toBeDefined();
    expect(screen.queryByText("Live Extensions (0)")).toBeNull();
    expect(screen.getByText(/extension document could not load/)).toBeDefined();
  });

  it("desktop_renders_semantic_remote_ui", () => {
    const events: UiEvent[] = [];
    const docId = "desktop-test-doc";

    const semanticDocument: UiDocument = {
      protocolVersion: UI_PROTOCOL_VERSION,
      documentId: docId,
      revision: 1,
      metadata: {
        title: "Test Extension Tool",
        source: "acme.tools",
      },
      root: {
        kind: "element",
        id: "root-stack",
        type: "Stack",
        props: { accessibleLabel: "Semantic Tool Container" },
        children: [
          {
            kind: "element",
            id: "status-alert",
            type: "Alert",
            props: { title: "Build Status", message: "Compilation succeeded", tone: "success" },
            children: [],
          },
          {
            kind: "element",
            id: "metric-badge",
            type: "Badge",
            props: { title: "v1.2.0", tone: "accent" },
            children: [],
          },
          {
            kind: "element",
            id: "input-field",
            type: "TextInput",
            props: { name: "searchQuery", accessibleLabel: "Search Packages", placeholder: "e.g. lodash" },
            children: [],
          },
          {
            kind: "element",
            id: "submit-button",
            type: "Button",
            props: { label: "Run Package Search", action: "search.run" },
            children: [],
          },
        ],
      },
    };

    const documents = new Map<string, UiDocument>([[docId, semanticDocument]]);

    const { container } = mounted(documents, (event) => events.push(event));

    // 1. Assert host chrome and document container render
    expect(screen.getByText(/Live Extensions \(1\)/i)).toBeDefined();
    expect(screen.getByLabelText("Extension identity")).toBeDefined();
    expect(container.querySelector('[data-ui-document-id="desktop-test-doc"]')).not.toBeNull();

    // 2. Assert semantic DOM elements are rendered instead of raw metadata strings
    // Alerts and feedback
    const alert = container.querySelector('[role="alert"]');
    expect(alert).not.toBeNull();
    expect(alert?.textContent).toContain("Build Status");
    expect(alert?.textContent).toContain("Compilation succeeded");

    // Badges
    const badge = container.querySelector(".ui-badge");
    expect(badge).not.toBeNull();
    expect(badge?.textContent).toContain("v1.2.0");

    // Text inputs
    const input = container.querySelector<HTMLInputElement>('input[placeholder="e.g. lodash"]');
    expect(input).not.toBeNull();
    expect(input?.getAttribute("aria-label")).toBe("Search Packages");

    // Buttons
    const button = container.querySelector<HTMLButtonElement>('button.ui-button');
    expect(button).not.toBeNull();
    expect(button?.textContent).toBe("Run Package Search");

    // 3. Test interactive event dispatching
    act(() => {
      button?.click();
    });

    expect(events.length).toBeGreaterThan(0);
    expect(events[0]).toMatchObject({
      documentId: docId,
      revision: 1,
      targetId: "submit-button",
      type: "action",
    });

    // Test text input change
    act(() => {
      fireEvent.change(input!, { target: { value: "vitest" } });
    });

    expect(events.length).toBeGreaterThan(1);
    expect(events.at(-1)).toMatchObject({
      documentId: docId,
      revision: 1,
      targetId: "input-field",
      type: "change",
    });
  });

  it("handles multiple documents and document disposal", () => {
    const doc1: UiDocument = {
      protocolVersion: UI_PROTOCOL_VERSION,
      documentId: "doc-1",
      revision: 1,
      metadata: { title: "Doc 1", source: "ext.one" },
      root: {
        kind: "element",
        id: "root-1",
        type: "Text",
        props: { value: "Content of Doc 1" },
        children: [],
      },
    };

    const doc2: UiDocument = {
      protocolVersion: UI_PROTOCOL_VERSION,
      documentId: "doc-2",
      revision: 1,
      metadata: { title: "Doc 2", source: "ext.two" },
      root: {
        kind: "element",
        id: "root-2",
        type: "Text",
        props: { value: "Content of Doc 2" },
        children: [],
      },
    };

    const documents = new Map<string, UiDocument>([
      ["doc-1", doc1],
      ["doc-2", doc2],
    ]);

    const { container } = mounted(documents);

    expect(container.querySelectorAll('[data-ui-document-id]').length).toBe(2);
    expect(container.textContent).toContain("Content of Doc 1");
    expect(container.textContent).toContain("Content of Doc 2");

    // Remove doc-1
    act(() => send({ type: "dispose", messageId: "dispose", dispose: { documentId: "doc-1", revision: 1 } }));

    expect(container.querySelectorAll('[data-ui-document-id]').length).toBe(1);
    expect(container.textContent).not.toContain("Content of Doc 1");
    expect(container.textContent).toContain("Content of Doc 2");
  });
});
