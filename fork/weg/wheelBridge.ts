/**
 * [fork] Replays the mouse wheel forwarded by the backend (`fork/background/wheel_bridge.rs`).
 *
 * Windows doesn't deliver the wheel to the dock unless "Scroll inactive windows when hovering over them" is
 * enabled, so the backend captures it and here it's dispatched as a regular DOM `wheel` event on the hovered
 * element. Dock code can then just listen to `wheel` as usual.
 */
import { Widget } from "@seelen-ui/lib";

// one wheel notch is 120 on Win32 and 100px on the DOM, positive Win32 deltas mean scrolling up
const WIN32_WHEEL_DELTA = 120;
const DOM_WHEEL_DELTA = 100;

// tracked from pointer events so a wheel doesn't have to resolve `:hover` (a style recalculation)
let hovered: Element | null = null;

document.addEventListener(
  "pointerover",
  (e) => {
    hovered = e.target instanceof Element ? e.target : null;
  },
  { capture: true, passive: true },
);

document.addEventListener(
  "pointerout",
  (e) => {
    // no related target means the pointer left the webview
    if (!e.relatedTarget) hovered = null;
  },
  { capture: true, passive: true },
);

/** Deepest hovered element, falling back to `:hover` if the tracked one was removed from the DOM */
function getHovered(): Element | undefined {
  if (hovered?.isConnected) return hovered;
  const all = document.querySelectorAll(":hover");
  return all[all.length - 1];
}

Widget.getCurrent()
  .window.listen<number>("fork::wheel", ({ payload: delta }) => {
    getHovered()?.dispatchEvent(
      new WheelEvent("wheel", {
        deltaY: (-delta / WIN32_WHEEL_DELTA) * DOM_WHEEL_DELTA,
        deltaMode: WheelEvent.DOM_DELTA_PIXEL,
        bubbles: true,
        cancelable: true,
      }),
    );
  })
  .catch(console.error);
