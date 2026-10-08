// HinterOS on-screen keyboard: combine the content scripts' reports (one per frame) and pass
// the result to the session daemon through the native-messaging host.
//
// The host ("hinteros_osk", installed with the session daemon) forwards
// {focus: "text" | "password" | "none"} to the daemon, which shows the on-screen keyboard when
// A is pressed on a focused text field or the field is clicked in the app the TV shows, and
// hides it when B is pressed or the field loses focus. A content script's {focus, clicked: true}
// (a click into its focused field) is passed on as it is, even when the focus did not change.
"use strict";

const HOST = "hinteros_osk";

// "<tabId>/<frameId>" -> "text" | "password" for frames whose field has focus.
const focused = new Map();
let sent = "none";
let port = null;
// After the host failed (not installed, daemon gone), wait before starting it again.
let retryAt = 0;

function connect() {
  if (port || Date.now() < retryAt) {
    return port;
  }
  try {
    port = browser.runtime.connectNative(HOST);
  } catch (e) {
    console.warn(`hinteros-osk: cannot start the native host: ${e}`);
    retryAt = Date.now() + 10000;
    return null;
  }
  port.onMessage.addListener((reply) => {
    if (reply && reply.ok === false) {
      console.warn(`hinteros-osk: ${reply.error}`);
    }
  });
  port.onDisconnect.addListener((p) => {
    if (p.error) {
      console.warn(`hinteros-osk: native host ended: ${p.error.message}`);
    }
    port = null;
    retryAt = Date.now() + 10000;
    // The daemon treats the closed connection as "none"; tell it again next time.
    sent = "none";
  });
  return port;
}

// What to tell the daemon: the most recent frame's field in the active tab wins.
function current(activeTab) {
  let focus = "none";
  for (const [key, kind] of focused) {
    if (key.startsWith(`${activeTab}/`)) {
      focus = kind;
    }
  }
  return focus;
}

let activeTab = null;

// `clicked`: a frame of the active tab reported a click into its field; tell the daemon even if
// the focus is unchanged.
async function update(clicked = false) {
  if (activeTab === null) {
    const [tab] = await browser.tabs.query({ active: true, currentWindow: true });
    activeTab = tab ? tab.id : -1;
  }
  const focus = current(activeTab);
  clicked = clicked && focus !== "none";
  if (focus === sent && !clicked) {
    return;
  }
  const p = connect();
  if (!p) {
    return;
  }
  try {
    p.postMessage(clicked ? { focus, clicked: true } : { focus });
    sent = focus;
  } catch (e) {
    console.warn(`hinteros-osk: ${e}`);
  }
}

browser.runtime.onMessage.addListener((msg, sender) => {
  if (!msg || !sender.tab || !["text", "password", "none"].includes(msg.focus)) {
    return;
  }
  const key = `${sender.tab.id}/${sender.frameId}`;
  // Re-insert so that the latest report is last (Map keeps insertion order).
  focused.delete(key);
  if (msg.focus !== "none") {
    focused.set(key, msg.focus);
  }
  update(msg.clicked === true && msg.focus !== "none" && sender.tab.active);
});

browser.tabs.onActivated.addListener(({ tabId }) => {
  activeTab = tabId;
  update();
});

browser.tabs.onRemoved.addListener((tabId) => {
  for (const key of [...focused.keys()]) {
    if (key.startsWith(`${tabId}/`)) {
      focused.delete(key);
    }
  }
  update();
});
