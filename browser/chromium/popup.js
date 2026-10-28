// Toolbar popup: detection switch, the per-site overlay switch, the current tab's
// selectable media and a way to open the desktop. Every action goes through the
// service worker ("ssdownload-popup" channel); the popup holds no state of its own.
// Its texts come from i18n.js in the desktop app's language (stored by the worker, followed live).
"use strict";
const popupApi = globalThis.browser || chrome;
const { t } = SSDownloadI18n;
const DOWNLOAD_PAGE = "https://ssdownload.isolmaz.com/download";
// Without the desktop app (not installed, or not Windows) nothing else in the popup can work, so
// it shows only the setup view, whose button opens the app's download page.
let appMissing = false;
// The last state the worker reported: the labels that depend on it are redrawn on a language change.
let lastState = null;

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

// Static texts come from the data-i18n attributes of popup.html.
function applyStaticTexts() {
  document.documentElement.lang = SSDownloadI18n.language();
  document.documentElement.dir = SSDownloadI18n.direction();
  for (const node of document.querySelectorAll("[data-i18n]")) node.textContent = t(node.getAttribute("data-i18n"));
}

// The labels whose wording depends on the last state: the switch reading (none without the app)
// and the per-site switch.
function drawLabels() {
  document.getElementById("state").textContent = !lastState || appMissing ? "" : lastState.detectionEnabled ? t("stateOn") : t("stateOff");
  document.getElementById("site-label").textContent = lastState?.host ? t("popupHideOnHost", lastState.host) : t("popupHideOnThisSite");
}

function show(state) {
  lastState = state;
  document.getElementById("detection").checked = state.detectionEnabled === true;
  // The desktop's availability problem (not installed, not running, refused) is stated where the
  // user looks; the "Open the app" button below is the explicit action that starts it.
  document.getElementById("status").textContent = state.capabilityError || "";
  appMissing = state.appMissing === true;
  document.getElementById("setup").hidden = !appMissing;
  document.getElementById("main").hidden = appMissing;
  const hide = document.getElementById("hide-site");
  hide.checked = state.siteHidden === true;
  hide.disabled = !state.host;
  drawLabels();
  const list = document.getElementById("media");
  list.textContent = "";
  for (const item of state.media || []) {
    const li = document.createElement("li");
    const button = document.createElement("button");
    const label = document.createElement("span");
    label.textContent = item.label;
    const kind = document.createElement("small");
    kind.textContent = item.kind === "audio" ? t("popupKindAudio") : t("popupKindVideo");
    button.append(label, kind);
    button.addEventListener("click", async () => {
      document.getElementById("status").textContent = t("popupOpeningPicker");
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
  const failure = reply?.error || t("popupNoReply");
  const state = await ask("state");
  if (usable(state)) show(state);
  document.getElementById("status").textContent = failure;
}

async function refresh() { await apply(await ask("state")); }

// A different language is a different page: the static texts, the state-bound labels and the
// worker's own texts (status line, source labels) are all drawn again.
function languageChanged() {
  applyStaticTexts();
  drawLabels();
  return refresh();
}

document.getElementById("detection").addEventListener("change", async (event) => {
  await apply(await ask("set-detection", { value: event.target.checked }));
});
document.getElementById("hide-site").addEventListener("change", async (event) => {
  await apply(await ask("hide-site", { value: event.target.checked }));
});
document.getElementById("setup-download").addEventListener("click", () => {
  popupApi.tabs.create({ url: DOWNLOAD_PAGE });
  window.close();
});
document.getElementById("open-app").addEventListener("click", async () => {
  const reply = await ask("open-app");
  if (reply.error) document.getElementById("status").textContent = reply.error;
  else window.close();
});
document.getElementById("close").addEventListener("click", () => window.close());
// The stored language is read first so that nothing is drawn in the wrong one.
SSDownloadI18n.load().then(() => {
  SSDownloadI18n.watch(languageChanged);
  return languageChanged();
});
