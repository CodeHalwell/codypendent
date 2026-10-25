import { useState } from "react";
import type { ConnectionStatus } from "./types.js";

/** Keep recovery controls visible while an offline connection retries in the background. */
export function useConnectionPresentation(status: ConnectionStatus, starting = false): ConnectionStatus {
  const [settled, setSettled] = useState(status);
  // Adjust before children render: an effect would leave one frame of stale
  // presentation during a disconnect or a newly successful connection.
  if (status !== "connecting" && status !== settled) setSettled(status);
  if (status === "connected") return "connected";
  if (starting) return "connecting";
  return status === "connecting" && settled === "disconnected" ? "disconnected" : status;
}
