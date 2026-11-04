// HinterOS on-screen keyboard: report whether a text field has focus in this frame.
//
// Sends {focus: "text" | "password" | "none"} to the background script whenever that
// changes. Focus moving from one field to the next (focusout, then focusin) is settled
// before reporting, so a field change does not report "none" in between.
//
// A click into the focused field (a trusted primary-button click that lands on it, whether it
// just got focus or had it already) sends {focus, clicked: true} even when nothing changed:
// the daemon shows the keyboard for a click, not for focus that came by itself (arrow keys,
// a page focusing a field on load).
"use strict";

// <input> types that take typed text. Everything else (checkbox, range, color, date
// pickers, buttons) has its own controls.
const TEXT_TYPES = new Set([
  "text", "search", "email", "url", "tel", "number", "password",
]);

// The element that really has focus, looking inside open shadow roots.
function deepActiveElement() {
  let el = document.activeElement;
  while (el && el.shadowRoot && el.shadowRoot.activeElement) {
    el = el.shadowRoot.activeElement;
  }
  return el;
}

// "text", "password" or "none" for an element.
function kindOf(el) {
  // :disabled also covers a field in a disabled <fieldset>.
  if (!el || el.readOnly || el.matches(":disabled")) {
    return "none";
  }
  const tag = el.localName;
  if (tag === "textarea") {
    return "text";
  }
  if (tag === "input") {
    // The browser's own type: "text" for a missing, unknown or obsolete type attribute, which
    // it shows as a text field too.
    const type = String(el.type ?? "text").toLowerCase();
    if (!TEXT_TYPES.has(type)) {
      return "none";
    }
    return type === "password" ? "password" : "text";
  }
  if (el.isContentEditable) {
    return "text";
  }
  return "none";
}

let reported = "none";
let pending = false;

function report() {
  pending = false;
  // A frame without focus has nothing focused, whatever its activeElement says.
  const focus = document.hasFocus() ? kindOf(deepActiveElement()) : "none";
  if (focus === reported) {
    return;
  }
  reported = focus;
  browser.runtime.sendMessage({ focus }).catch(() => {
    // The background page is reloading; the next change reports again.
  });
}

// Look once focus has settled (after the focusout/focusin pair of a field change).
function settle() {
  if (!pending) {
    pending = true;
    setTimeout(report, 0);
  }
}

// Capture phase: pages cannot stop these from reaching us.
window.addEventListener("focusin", settle, true);
window.addEventListener("focusout", settle, true);
window.addEventListener("focus", settle, true);
window.addEventListener("blur", settle, true);
// The page goes away or into the background: nothing has focus any more.
window.addEventListener("pagehide", () => {
  if (reported !== "none") {
    reported = "none";
    browser.runtime.sendMessage({ focus: "none" }).catch(() => {});
  }
}, true);
// Back from the back/forward cache: the field may still have focus, and no focus event comes.
window.addEventListener("pageshow", settle, true);
document.addEventListener("visibilitychange", settle, true);
// A click into the focused field. "click" comes after the press moved focus, and only for
// the primary button (the right click, for the context menu, is "auxclick").
window.addEventListener("click", (event) => {
  if (!event.isTrusted || !document.hasFocus()) {
    return;
  }
  const el = deepActiveElement();
  const focus = kindOf(el);
  // On the field itself, not elsewhere (a suggestion list that keeps focus in the field).
  if (focus === "none" || !event.composedPath().includes(el)) {
    return;
  }
  reported = focus;
  browser.runtime.sendMessage({ focus, clicked: true }).catch(() => {});
}, true);
// An autofocused field may have focus before our listeners ran.
if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", settle, { once: true });
} else {
  settle();
}
// The background script dropped this tab's reports (the tab started loading): report again.
browser.runtime.onMessage.addListener((msg) => {
  if (msg && msg.report === true) {
    reported = "none";
    settle();
  }
});
