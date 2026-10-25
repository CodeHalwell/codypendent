import { useEffect, useRef, useState } from "react";
import type { CodeGraphPage } from "@codypendent/protocol";

type Details = { scope: CodeGraphPage; id: string | null; page?: CodeGraphPage; pages: CodeGraphPage[]; loading: boolean; error?: string };
const EMPTY_PAGES: CodeGraphPage[] = [];
export function useSymbolDetails(scope: CodeGraphPage, id: string | null, read?: (id: string) => Promise<CodeGraphPage>): Omit<Details, "scope"> & { retry: () => void } {
  const cache = useRef({ scope, pages: new Map<string, CodeGraphPage>() });
  if (cache.current.scope !== scope) cache.current = { scope, pages: new Map() };
  const [attempt, setAttempt] = useState(0);
  const [details, setDetails] = useState<Details>({ scope, id: null, pages: EMPTY_PAGES, loading: false });
  useEffect(() => {
    const current = cache.current.pages;
    const pages = () => [...current.values()];
    if (!id || !read) { setDetails({ scope, id, pages: pages(), loading: false }); return; }
    const saved = current.get(id);
    if (saved) { setDetails({ scope, id, page: saved, pages: pages(), loading: false }); return; }
    let live = true;
    setDetails({ scope, id, pages: pages(), loading: true });
    void Promise.resolve().then(() => read(id)).then((page) => {
      if (!live) return;
      if (!page || !Array.isArray(page.nodes) || !Array.isArray(page.edges)) throw new Error("The symbol query did not return a graph page.");
      current.set(id, page);
      if (current.size > 12) current.delete(current.keys().next().value!);
      setDetails({ scope, id, page, pages: pages(), loading: false });
    }).catch((error) => { if (live) setDetails({ scope, id, pages: pages(), loading: false, error: error instanceof Error ? error.message : String(error) }); });
    return () => { live = false; };
  }, [scope, id, read, attempt]);
  const retry = () => { if (id) cache.current.pages.delete(id); setAttempt((value) => value + 1); };
  const pages = details.scope === scope ? details.pages : EMPTY_PAGES;
  return { ...(details.scope === scope && details.id === id ? details : { id, loading: Boolean(id && read) }), pages, retry };
}
