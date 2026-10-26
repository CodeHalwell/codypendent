import { useCallback, useEffect, useLayoutEffect, useState } from "react";

export type Appearance = "system" | "light" | "dark";
const APPEARANCE_KEY = "codypendent.appearance";
const SIDEBAR_KEY = "codypendent.sidebar";

function readPreference(key: string): string | null {
  try { return localStorage.getItem(key); } catch { return null; }
}
function writePreference(key: string, value: string) {
  try { localStorage.setItem(key, value); } catch { /* Preferences are optional in restricted webviews. */ }
}

/** Local presentation preferences never touch daemon or repository state. */
export function useWorkspacePreferences() {
  const [appearance, setAppearance] = useState<Appearance>(() => {
    const saved = readPreference(APPEARANCE_KEY);
    return saved === "light" || saved === "dark" ? saved : "system";
  });
  const [sidebarVisible, setSidebarVisible] = useState(() => readPreference(SIDEBAR_KEY) !== "hidden");
  const [compact, setCompact] = useState(() => window.matchMedia?.("(max-width: 680px)").matches ?? false);
  const toggleSidebar = useCallback(() => setSidebarVisible((visible) => !visible), []);
  const closeSidebar = useCallback(() => setSidebarVisible(false), []);

  useEffect(() => {
    const viewport = window.matchMedia?.("(max-width: 680px)");
    if (!viewport) return;
    const update = () => setCompact(viewport.matches);
    viewport.addEventListener("change", update);
    return () => viewport.removeEventListener("change", update);
  }, []);

  useLayoutEffect(() => {
    if (appearance === "system") delete document.documentElement.dataset.theme;
    else document.documentElement.dataset.theme = appearance;
    writePreference(APPEARANCE_KEY, appearance);
  }, [appearance]);
  useEffect(() => { writePreference(SIDEBAR_KEY, sidebarVisible ? "visible" : "hidden"); }, [sidebarVisible]);

  return { appearance, setAppearance, sidebarVisible, toggleSidebar, closeSidebar, compact };
}
