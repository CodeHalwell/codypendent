import { useEffect, useRef } from "react";
import { shellAvailable } from "./components/localConfig.js";

/** One native menu listener, with the current render's handlers and safe late cleanup. */
export function useNativeDesktopActions(onAction: (action: string) => void) {
  const latest = useRef(onAction);
  latest.current = onAction;
  useEffect(() => {
    if (!shellAvailable()) return;
    let closed = false;
    let unlisten: (() => void) | undefined;
    void import("@tauri-apps/api/event").then(({ listen }) => {
      if (closed) return undefined;
      return listen<string>("desktop-action", (event) => {
        if (!closed) latest.current(event.payload);
      });
    }).then((stop) => {
      if (closed) stop?.();
      else unlisten = stop;
    }).catch((error: unknown) => console.error("Could not connect native menu actions", error));
    return () => { closed = true; unlisten?.(); };
  }, []);
}
