import { act, fireEvent, render, renderHook, screen, waitFor } from "@testing-library/react";
import { StrictMode } from "react";
import { renderToString } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { useDaemon } from "../src/useDaemon.js";
import { ConnectionBanner } from "../src/components/ConnectionBanner.js";
import type { ConnectionInfo, DesktopTransport } from "../src/transport.js";
import { useConnectionPresentation } from "../src/useConnectionPresentation.js";
import type { ConnectionStatus } from "../src/types.js";
import { Transcript } from "../src/components/Transcript.js";

function ConnectionProbe({ factory }: { factory: () => DesktopTransport | null }) {
  const { state, transport, startDaemon, reconnect } = useDaemon(factory);
  return <ConnectionBanner status={state.status} detail={state.detail} hasTransport={transport !== null}
    canStart={false} onStart={startDaemon} onRetry={() => { void reconnect(); }} />;
}

function PresentationProbe({ status, starting = false }: { status: ConnectionStatus; starting?: boolean }) {
  const presentation = useConnectionPresentation(status, starting);
  return <Transcript items={[]} connectionStatus={presentation} />;
}

describe("the first connection render", () => {
  it("keeps the current connection when StrictMode retires an earlier attempt", async () => {
    const attempts: { generation?: number; resolve: (info: ConnectionInfo) => void }[] = [];
    let connectedGeneration: number | undefined;
    const transport = {
      connect: (_handler: unknown, generation?: number) => new Promise<ConnectionInfo>((resolve) => attempts.push({ generation, resolve })),
      disconnect: async (generation?: number) => { if (generation === connectedGeneration) connectedGeneration = undefined; },
      listSessions: async () => [],
      listInbox: async () => ({ items: [], next_cursor: null }),
    } as unknown as DesktopTransport;
    const daemon = renderHook(() => useDaemon(() => transport), { wrapper: StrictMode });
    expect(attempts).toHaveLength(2);
    expect(attempts[0].generation).not.toBe(attempts[1].generation);
    const info = { socket_path: "/tmp/startup.sock", protocol_version: "1.11", daemon_version: "0.14.0", daemon_instance: "startup", build_id: "startup" };
    connectedGeneration = attempts[1].generation;
    await act(async () => attempts[1].resolve(info));
    await act(async () => attempts[0].resolve(info));
    expect(connectedGeneration).toBe(attempts[1].generation);
    expect(daemon.result.current.state.status).toBe("connected");
  });
  it("retains the same focused recovery button across background retries and resets after connection", () => {
    const props = { hasTransport: true, canStart: true, onStart: vi.fn(), onRetry: vi.fn() };
    const { rerender } = render(<ConnectionBanner {...props} status="connecting" detail="Initial attempt" />);
    expect(screen.queryByTestId("start-daemon")).toBeNull();
    rerender(<ConnectionBanner {...props} status="disconnected" detail="Socket missing" />);
    const start = screen.getByTestId("start-daemon");
    start.focus();
    rerender(<ConnectionBanner {...props} status="connecting" detail="Automatic retry" />);
    expect(screen.getByTestId("start-daemon")).toBe(start);
    expect(document.activeElement).toBe(start);
    expect(screen.getByTestId("connection-banner").textContent).toContain("Workspace offline");
    rerender(<ConnectionBanner {...props} status="disconnected" detail="Still offline" />);
    expect(screen.getByTestId("start-daemon")).toBe(start);
    rerender(<ConnectionBanner {...props} status="connected" detail="Ready" />);
    expect(screen.queryByTestId("connection-banner")).toBeNull();
    rerender(<ConnectionBanner {...props} status="connecting" detail="New connection" />);
    expect(screen.getByTestId("connection-banner").textContent).toContain("Connecting your workspace");
    rerender(<ConnectionBanner {...props} status="disconnected" detail="Disconnected again" />);
    expect(screen.getByTestId("start-daemon")).toBeTruthy();
  });

  it("keeps the welcome state offline during retries but shows explicit startup progress", () => {
    const { rerender } = render(<PresentationProbe status="connecting" />);
    expect(screen.getByRole("heading", { name: "Opening your workspace" })).toBeTruthy();
    rerender(<PresentationProbe status="disconnected" />);
    const offline = screen.getByRole("heading", { level: 3 }).textContent;
    rerender(<PresentationProbe status="connecting" />);
    expect(screen.getByRole("heading", { level: 3 }).textContent).toBe(offline);
    rerender(<PresentationProbe status="connecting" starting />);
    expect(screen.getByRole("heading", { name: "Opening your workspace" })).toBeTruthy();
    rerender(<PresentationProbe status="disconnected" starting />);
    expect(screen.getByRole("heading", { name: "Opening your workspace" })).toBeTruthy();
    rerender(<PresentationProbe status="disconnected" />);
    expect(screen.getByRole("heading", { level: 3 }).textContent).toBe(offline);
    rerender(<PresentationProbe status="connected" />);
    rerender(<PresentationProbe status="connecting" />);
    expect(screen.getByRole("heading", { name: "Opening your workspace" })).toBeTruthy();
  });

  it("shows connecting before effects have checked whether the bridge exists", () => {
    const factory = vi.fn(() => null);
    const html = renderToString(<ConnectionProbe factory={factory} />);
    expect(factory).not.toHaveBeenCalled();
    expect(html).toContain("Connecting your workspace");
    expect(html).not.toContain("browser preview");
    expect(html).not.toContain("Open Codypendent to connect");
  });

  it("shows browser-preview guidance after a real absent-bridge check", async () => {
    const factory = vi.fn(() => null);
    render(<ConnectionProbe factory={factory} />);
    await waitFor(() => expect(factory).toHaveBeenCalledOnce());
    expect(screen.getByTestId("connection-banner").textContent).toContain("This browser preview");
  });

  it("stays connecting while the native bridge's connection is pending", async () => {
    const transport = {
      connect: () => new Promise(() => {}),
      disconnect: () => Promise.resolve(),
    } as unknown as DesktopTransport;
    render(<ConnectionProbe factory={() => transport} />);
    await act(async () => undefined);
    expect(screen.getByTestId("connection-banner").textContent).toContain("Connecting your workspace");
    expect(screen.getByTestId("connection-banner").textContent).not.toContain("browser preview");
  });

  it("keeps connecting visible during a slow start and reports an eventual failure", async () => {
    let finish!: (result: { started: boolean; detail: string }) => void;
    const onStart = () => new Promise<{ started: boolean; detail: string }>((resolve) => { finish = resolve; });
    const props = { hasTransport: true, canStart: true, onStart, onRetry: vi.fn() };
    const { rerender } = render(<ConnectionBanner {...props} status="disconnected" detail="Not listening yet" />);
    fireEvent.click(screen.getByTestId("start-daemon"));
    rerender(<ConnectionBanner {...props} status="connecting" detail="Retrying" />);
    rerender(<ConnectionBanner {...props} status="disconnected" detail="Connection attempt timed out" />);
    expect(screen.getByTestId("connection-banner").textContent).toContain("Connecting your workspace");
    expect(screen.getByTestId("connection-banner").textContent).not.toContain("timed out");
    await act(async () => { finish({ started: false, detail: "The local service could not start" }); });
    expect(screen.getByTestId("start-daemon-outcome").textContent).toContain("could not start");
    expect(screen.getByTestId("start-daemon")).toBeTruthy();
  });
});
