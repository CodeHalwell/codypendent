import { expect, it, vi } from "vitest";
import { NAV_GROUPS } from "../src/components/Navigation.js";
import { featureCatalog } from "../src/agui/featureCatalog.js";
import { createHostCall, HOST_TOOLS, type HostBindings } from "../src/agui/hostTools.js";

it("discovers every real page and distinguishes tool recipes from user controls", async () => {
  const host = createHostCall(() => ({} as HostBindings));
  const catalog = await host("desktop.listFeatures", {}) as ReturnType<typeof featureCatalog>;
  expect(catalog.pages.map(({ view }) => view)).toEqual(NAV_GROUPS.flatMap(({ views }) => views.map(([view]) => view)));
  expect(catalog.pages.every((page) => page.description && page.userControls)).toBe(true);
  expect(catalog.pages.find(({ view }) => view === "keys")?.tools).toEqual([]);
  expect(catalog.pages.find(({ view }) => view === "backtrack")?.tools).toEqual([]);
  expect(catalog.pages.find(({ view }) => view === "edges")?.tools).toContain("graph.callers_of");
  expect(catalog.availability).toContain("not a permission grant");
  expect(catalog.generatedTools.recipe).toContain("Monty");
});

it("navigates every catalogued page including extensions and rejects unknown views", async () => {
  const navigate = vi.fn();
  const host = createHostCall(() => ({ navigate } as unknown as HostBindings));
  const allowed = HOST_TOOLS.find(({ name }) => name === "desktop.navigate")!.parameters.properties as Record<string, { enum: string[] }>;
  expect(allowed.view.enum).toEqual(featureCatalog().pages.map(({ view }) => view));
  for (const view of allowed.view.enum) await host("desktop.navigate", { view });
  expect(navigate).toHaveBeenCalledWith("plugins");
  await expect(host("desktop.navigate", { view: "executeAnything" })).rejects.toThrow("Invalid choice");
  expect(navigate).toHaveBeenCalledTimes(allowed.view.enum.length);
});
