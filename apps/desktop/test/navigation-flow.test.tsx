/**
 * Navigation flow: the sidebar must not present every surface at once, and no
 * surface may be a dead end.
 *
 * The desktop had 22 destinations listed flat and Escape did nothing unless the
 * palette was open, so every secondary view had to be left by aiming at the
 * sidebar again. These tests pin both halves of the fix.
 */
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { App } from "../src/App.js";

describe("the sidebar does not shout every destination at once", () => {
  it("keeps chat navigation visible and leaves tools folded away", () => {
    render(<App />);
    expect(screen.getByLabelText("Sessions View")).toBeTruthy();
    // "Configuration" is not — its destinations are absent from the DOM, not
    // merely hidden, so they cost nothing to render either.
    expect(screen.queryByLabelText("Models View")).toBeNull();
    expect(screen.queryByLabelText("API Keys View")).toBeNull();
  });

  it("opens a folded group when its header is clicked", () => {
    render(<App />);
    expect(screen.queryByLabelText("Models View")).toBeNull();
    fireEvent.click(screen.getByLabelText("Configuration group"));
    expect(screen.getByLabelText("Models View")).toBeTruthy();
  });

  it("lets the current group collapse and reopen without changing views", () => {
    render(<App />);
    fireEvent.click(screen.getByLabelText("Configuration group"));
    fireEvent.click(screen.getByLabelText("Models View"));
    fireEvent.click(screen.getByLabelText("Configuration group"));
    expect(screen.queryByLabelText("Models View")).toBeNull();
    expect(screen.getByRole("navigation", { name: "View navigation" }).textContent).toContain("Models");
    fireEvent.click(screen.getByLabelText("Configuration group"));
    expect(screen.getByLabelText("Models View").getAttribute("aria-current")).toBe("page");
  });
});

describe("escape is always a way out", () => {
  it("walks back to the view it came from", () => {
    render(<App />);
    fireEvent.click(screen.getByLabelText("Inbox View"));
    expect(screen.getByLabelText("Inbox View").getAttribute("style")).toContain("font-weight: 600");

    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.getByLabelText("Sessions View").getAttribute("style")).toContain("font-weight: 600");
  });

  it("lands on the session rather than nowhere when there is no history", () => {
    render(<App />);
    fireEvent.click(screen.getByLabelText("Configuration group"));
    fireEvent.click(screen.getByLabelText("Models View"));
    fireEvent.keyDown(window, { key: "Escape" });
    fireEvent.keyDown(window, { key: "Escape" });
    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.getByLabelText("Sessions View").getAttribute("style")).toContain("font-weight: 600");
  });

  it("closes the palette first and leaves the view alone", () => {
    render(<App />);
    fireEvent.click(screen.getByLabelText("Inbox View"));
    fireEvent.keyDown(window, { key: "k", metaKey: true });
    fireEvent.keyDown(window, { key: "Escape" });
    // The palette absorbed that Escape; the view has not moved.
    expect(screen.getByLabelText("Inbox View").getAttribute("style")).toContain("font-weight: 600");
  });
});
