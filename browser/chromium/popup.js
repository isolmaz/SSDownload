// Toolbar popup: detection switch, the per-site overlay switch, the current tab's
// selectable media and a way to open the desktop. Every action goes through the
// service worker ("ssdownload-popup" channel); the popup holds no state of its own.
"use strict";
const popupApi = globalThis.browser || chrome;

function ask(command, extra) {
  return new Promise((resolve) => {
    try {
      popupApi.runtime.sendMessage({ channel: "ssdownload-popup", command, ...(extra || {}) }, (reply) => {
        void popupApi.runtime.lastError;
        resolve(reply || {});
      });
    } catch (_) { resolve({}); }
  });
}

function show(state) {
  document.getElementById("detection").checked = state.detectionEnabled === true;
  document.getElementById("state").textContent = state.detectionEnabled ? "ON" : "OFF";
  const hide = document.getElementById("hide-site");
  hide.checked = state.siteHidden === true;
  hide.disabled = !state.host;
  document.getElementById("site-label").textContent = state.host ? `${state.host} için düğmeyi gösterme` : "Bu sitede düğmeyi gösterme";
  const list = document.getElementById("media");
  list.textContent = "";
  for (const item of state.media || []) {
    const li = document.createElement("li");
    const button = document.createElement("button");
    const label = document.createElement("span");
    label.textContent = item.label;
    const kind = document.createElement("small");
    kind.textContent = item.kind === "audio" ? "ses" : "video";
    button.append(label, kind);
    button.addEventListener("click", async () => {
      document.getElementById("status").textContent = "Masaüstü seçicisi açılıyor…";
      const reply = await ask("open-media", { index: item.index });
      if (reply.error) document.getElementById("status").textContent = reply.error;
      else window.close();
    });
    li.append(button);
    list.append(li);
  }
  document.getElementById("empty").hidden = (state.media || []).length > 0;
}

async function refresh() { show(await ask("state")); }

document.getElementById("detection").addEventListener("change", async (event) => {
  show(await ask("set-detection", { value: event.target.checked }));
});
document.getElementById("hide-site").addEventListener("change", async (event) => {
  show(await ask("hide-site", { value: event.target.checked }));
});
document.getElementById("open-app").addEventListener("click", async () => {
  const reply = await ask("open-app");
  if (reply.error) document.getElementById("status").textContent = reply.error;
  else window.close();
});
document.getElementById("close").addEventListener("click", () => window.close());
refresh();
