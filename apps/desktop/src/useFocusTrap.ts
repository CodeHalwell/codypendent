/**
 * Keyboard focus for a dialog: move it in, keep it in, give it back.
 *
 * Every overlay in the app declared `aria-modal="true"`, which promises a
 * screen reader that focus is confined to the dialog — and none of them
 * confined it. Tab walked straight out of the command palette into the
 * sidebar behind it, and closing any dialog dropped focus on `<body>`, so a
 * keyboard user lost their place every time. The inline confirmations
 * (remove a model, delete a key, consent to a community bridge) never moved
 * focus at all: after clicking Remove the focus stayed on the Remove button,
 * and Enter fired it again.
 *
 * One hook, three duties:
 *
 * 1. On activation, remember what had focus and move focus into the
 *    container — to `initialFocus` when given, else the first focusable
 *    element, else the container itself (which gets `tabIndex=-1` so it can
 *    take focus).
 * 2. While active, wrap Tab and Shift-Tab at the container's edges.
 * 3. On deactivation, restore focus to the remembered element if it is still
 *    in the document.
 *
 * Escape is the caller's business: `onEscape` is invoked with the event and
 * stops propagation, so a dialog's Escape never also reaches the app's
 * "go back" handler — which is how Escape used to dismiss a confirmation AND
 * navigate the view away underneath it.
 */
import { useEffect, useRef, type RefObject } from "react";

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]):not([type="hidden"]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function focusables(container: HTMLElement): HTMLElement[] {
  // `hidden` and `aria-hidden` subtrees are skipped; layout-based checks
  // (`offsetParent`) are not used because a test DOM has no layout.
  return Array.from(container.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (element) => !element.hidden && element.closest('[hidden], [aria-hidden="true"]') === null,
  );
}

export interface FocusTrapOptions {
  /** Whether the trap is engaged. A closed dialog engages nothing. */
  active: boolean;
  /** The element to focus first; defaults to the first focusable, else the container. */
  initialFocus?: RefObject<HTMLElement | null>;
  /** Where to return if the dialog's action removed its original trigger. */
  restoreFocusFallback?: RefObject<HTMLElement | null>;
  /** Called on Escape, after propagation is stopped. */
  onEscape?: (event: KeyboardEvent) => void;
}

export function useFocusTrap(
  container: RefObject<HTMLElement | null>,
  { active, initialFocus, restoreFocusFallback, onEscape }: FocusTrapOptions,
): void {
  // The latest callback, read at key time: callers pass inline arrows, and
  // an effect keyed on them would re-arm — and re-focus — on every render.
  const escape = useRef(onEscape);
  escape.current = onEscape;
  useEffect(() => {
    if (!active) {
      return;
    }
    const node = container.current;
    if (!node) {
      return;
    }
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;

    if (!node.hasAttribute("tabindex")) {
      node.setAttribute("tabindex", "-1");
    }
    const first = initialFocus?.current ?? focusables(node)[0] ?? node;
    // Now, and again next frame: a dialog that mounts in the same tick as the
    // click that opened it can otherwise lose the focus call to the click's
    // own focus handling.
    first.focus();
    const frame = requestAnimationFrame(() => {
      if (node.isConnected && !node.contains(document.activeElement)) {
        first.focus();
      }
    });

    const onKeyDown = (event: KeyboardEvent) => {
      // Escape/Enter can belong to the IME candidate window. Let composition
      // finish without dismissing the dialog or reaching an app shortcut.
      if (event.isComposing || event.keyCode === 229) {
        event.stopPropagation();
        return;
      }
      if (event.key === "Escape" && escape.current) {
        event.stopPropagation();
        escape.current(event);
        return;
      }
      if (event.key !== "Tab") {
        return;
      }
      const items = focusables(node);
      if (items.length === 0) {
        event.preventDefault();
        node.focus();
        return;
      }
      const firstItem = items[0];
      const lastItem = items[items.length - 1];
      const current = document.activeElement;
      if (event.shiftKey) {
        if (current === firstItem || current === node || !node.contains(current)) {
          event.preventDefault();
          lastItem.focus();
        }
      } else if (current === lastItem || !node.contains(current)) {
        event.preventDefault();
        firstItem.focus();
      }
    };
    node.addEventListener("keydown", onKeyDown);

    return () => {
      cancelAnimationFrame(frame);
      node.removeEventListener("keydown", onKeyDown);
      if (previous && previous.isConnected && previous.closest('[hidden], [aria-hidden="true"]') === null) {
        previous.focus();
      } else {
        restoreFocusFallback?.current?.focus();
      }
    };
  }, [active, container, initialFocus, restoreFocusFallback]);
}
