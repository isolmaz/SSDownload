// Toolbar popup: detection switch, the per-site overlay switch, the current tab's
// selectable media and a way to open the desktop. Every action goes through the
// service worker ("ssdownload-popup" channel); the popup holds no state of its own.
"use strict";
const popupApi = globalThis.browser || chrome;
const DOWNLOAD_PAGE = "https://ssdownload.isolmaz.com/download";
// Without the desktop app (not installed, or not Windows) the main button leads to its download page.
let appMissing = false;

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

// A reply that failed or never came carries no state: it must not be drawn as "everything off".
function usable(reply) {
  return !!reply && !reply.error && typeof reply.detectionEnabled === "boolean";
}

function show(state) {
  document.getElementById("detection").checked = state.detectionEnabled === true;
  document.getElementById("state").textContent = state.detectionEnabled ? "AÇIK" : "KAPALI";
  // The desktop's availability problem (not installed, not running, refused) is stated where the
  // user looks; "Uygulamayı aç" below is the explicit action that starts it.
  document.getElementById("status").textContent = state.capabilityError || "";
  appMissing = state.appMissing === true;
  document.getElementById("open-app").textContent = appMissing ? "Uygulamayı indir" : "Uygulamayı aç";
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

// Draws a state reply; a failed one keeps what is on screen, says why, and re-reads the
// worker's real state so the switches cannot drift from it.
async function apply(reply) {
  if (usable(reply)) { show(reply); return; }
  const failure = reply?.error || "Eklenti arka planı yanıt vermedi.";
  const state = await ask("state");
  if (usable(state)) show(state);
  document.getElementById("status").textContent = failure;
}

async function refresh() { await apply(await ask("state")); }

document.getElementById("detection").addEventListener("change", async (event) => {
  await apply(await ask("set-detection", { value: event.target.checked }));
});
document.getElementById("hide-site").addEventListener("change", async (event) => {
  await apply(await ask("hide-site", { value: event.target.checked }));
});
document.getElementById("open-app").addEventListener("click", async () => {
  if (appMissing) {
    popupApi.tabs.create({ url: DOWNLOAD_PAGE });
    window.close();
    return;
  }
  const reply = await ask("open-app");
  if (reply.error) document.getElementById("status").textContent = reply.error;
  else window.close();
});
document.getElementById("close").addEventListener("click", () => window.close());
refresh();
