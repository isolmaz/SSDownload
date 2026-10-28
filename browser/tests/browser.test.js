"use strict";

// These tests intentionally use only Node's built-in test runner. The Chrome
// extension must remain usable unpacked without an npm dependency tree, so a
// small browser/DOM double is preferable to a test-only UI framework here.
const assert = require("node:assert/strict");
const { webcrypto } = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const { i18nApi, loadI18n } = require("./i18n-harness");

const extensionDir = path.resolve(__dirname, "..", "chromium");
const source = (name) => fs.readFileSync(path.join(extensionDir, name), "utf8");

function event() {
  const listeners = [];
  return { listeners, addListener(listener) { listeners.push(listener); } };
}

const nativeCapabilities={ok:true,result:{capabilities:{protocol:2,capability_version:3,site_entries:true,usage_modes:{video:true,file:true,audio:true},onboarding_version:1,settings_revision:1,source_refresh_jobs:[]}}};

function backgroundHarness({ native = async request => request?.action?.type==="capabilities"?nativeCapabilities:({ ok: true, message: "Tamam" }), session = {}, local = {}, navigation, pageHtml, alarmStore = new Map(), browserLanguage = "tr" } = {}) {
  const events = {
    headers: event(), beforeHeaders: event(), removed: event(), updated: event(), installed: event(), startup: event(),
    menu: event(), message: event(), activated: event(), alarm: event(), download: event(), suspend: event()
  };
  const storage = { session: { ...session }, local: { ...local } };
  const logs = [];
  const nativeRequests = [];
  const actionCalls = [];
  const alarmCreates = [];
  const downloadCalls = [];
  const menuEntries = [];
  const tabCreates = [];
  const timers = new Map();
  let nextTimer = 0;
  const browser = {
    i18n: i18nApi(browserLanguage),
    storage: {
      local: {
        async get(defaults) { return { ...defaults, ...storage.local }; },
        async set(value) { Object.assign(storage.local, value); }
      },
      session: {
        async get(keys) {
          if (Array.isArray(keys)) return Object.fromEntries(keys.map(key => [key, storage.session[key]]));
          return { ...keys, ...storage.session };
        },
        async set(value) { Object.assign(storage.session, value); }
      }
    },
    webRequest: { onHeadersReceived: events.headers, onBeforeSendHeaders: events.beforeHeaders },
    // An alarm outlives the worker: the store is shared by every worker a test restarts.
    alarms: {
      create(name, info) { alarmCreates.push({ name, info }); alarmStore.set(name, info); },
      async get(name) { return alarmStore.has(name) ? { name, ...alarmStore.get(name) } : undefined; },
      onAlarm: events.alarm
    },
    downloads: {
      onCreated: events.download,
      async pause(id) { downloadCalls.push({ method: "pause", id }); },
      async resume(id) { downloadCalls.push({ method: "resume", id }); },
      async cancel(id) { downloadCalls.push({ method: "cancel", id }); },
      async erase(query) { downloadCalls.push({ method: "erase", id: query.id }); }
    },
    webNavigation: navigation,
    permissions: {
      async contains() { return storage.granted === true; },
      async request() { storage.granted = true; return { granted: true }; }
    },
    cookies: {
      async getAllCookieStores() { return [{ id: "0", tabIds: [7] }]; },
      async getPartitionKey() { return { partitionKey: { topLevelSite: "https://site.example" } }; },
      async getAll() { return []; }
    },
    tabs: {
      onRemoved: events.removed, onUpdated: events.updated, onActivated: events.activated,
      // The worker asks the page for its own markup at the moment of a handoff; a tab that
      // cannot answer (a restarted worker, an about:blank frame) is the omitted field.
      async sendMessage(_tabId, message) {
        if (message?.channel !== "ssdownload-background" || message.command !== "page-html") return undefined;
        return typeof pageHtml === "function" ? pageHtml(message) : pageHtml === undefined ? undefined : { html: pageHtml };
      },
      async query() { return [{ id: 7, url: "https://site.example/watch", active: true }]; },
      async get(id) { return { id, url: "https://site.example/watch", cookieStoreId: "0" }; },
      async create(value) { tabCreates.push(value); return { id: 99, ...value }; }
    },
    runtime: {
      id: "test-extension", getURL(value = "") { return `chrome-extension://test-extension/${value}`; },
      getManifest() { return { version: "1.4.8" }; },
      lastError: null, onInstalled: events.installed, onStartup: events.startup, onMessage: events.message, onSuspend: events.suspend,
      async sendNativeMessage(_host, request) {
        nativeRequests.push(request);
        if (request?.action?.type === "log") logs.push(request.action.request.events);
        return native(request);
      }
    },
    action: {
      onClicked: event(),
      async setBadgeText(value) { actionCalls.push({ method: "setBadgeText", ...value }); },
      async setBadgeBackgroundColor(value) { actionCalls.push({ method: "setBadgeBackgroundColor", ...value }); },
      async setTitle(value) { actionCalls.push({ method: "setTitle", ...value }); }
    },
    contextMenus: { onClicked: events.menu, async removeAll() { menuEntries.length = 0; }, create(entry) { menuEntries.push(entry); } }
  };
  const scheduleTimer = (callback, delay) => { const id = ++nextTimer; timers.set(id, { callback, delay }); return id; };
  const cancelTimer = id => { timers.delete(id); };
  const context = vm.createContext({ browser, URL, crypto: webcrypto, TextEncoder, Uint8Array, Promise, Map, Set, Array, Object, Number, String, RegExp, JSON, Error, Date, queueMicrotask, setTimeout: scheduleTimer, clearTimeout: cancelTimer });
  loadI18n(context);
  for (const name of ["codes.js", "events.js", "background.js"]) vm.runInContext(source(name), context, { filename: name });
  // `publicState` and `hlsChildren` were removed from the worker as dead code; the
  // harness evaluates the same views against the worker's own globals, so every
  // existing assertion keeps reading exactly what it read before.
  vm.runInContext(`function publicState(tabId) {
  const values = mediaByTab.get(tabId);
  const items=values?Array.from(values.values()).reverse().filter(item=>!relatedRole(tabId,item.frameId,item.url)).map(item=>publicSource(tabId,item)):[];
  const allVideos=videoRecords(tabId).map(item=>publicSource(tabId,item));
  return {
    detectionEnabled,
    items,
    videos:allVideos.filter(item=>!item.suspectedAd && !item.ambiguous && !item.preview),
    candidates:allVideos.filter(item=>item.suspectedAd || item.ambiguous || item.preview),
    observationCapabilities:[...frames.values()].filter(frame=>frame.tabId===tabId && frame.observationCapabilities).map(frame=>({frameId:frame.frameId,documentId:frame.documentId,...frame.observationCapabilities})),
    capabilities:capabilityState,
    status: sessionError ? { ...(tabStatus.get(tabId) || {}), message: sessionError, error: true } : tabStatus.get(tabId) || null
  };
}
function hlsChildren(text, base) { return hlsRelations(text,base).filter(item=>item.role==="variant"||item.role==="audio").map(item=>item.url); }`, context, { filename: "harness.js" });
  // Discovery state is read straight from the worker global: the popup that used
  // to expose it is gone, and the candidate list is only reachable through the
  // context menu and the native picker now.
  async function state(tabId) {
    const id = Number(tabId);
    return JSON.parse(await vm.runInContext(`(async () => { await sessionReady; await pruneFrames(${id}); await Promise.all([...manifestTasks].filter(([key]) => key.startsWith("${id}\\n")).map(([, task]) => task)); return JSON.stringify(publicState(${id})); })()`, context));
  }
  async function menu(menuItemId, info = {}) {
    const tab = info.tab || { id: 7, url: "https://site.example/watch" };
    events.menu.listeners.at(-1)({ menuItemId, pageUrl: tab.url, ...info.info }, tab);
    await settle();
  }
  // Context-menu handlers are fire-and-forget in the worker; tests need to wait
  // for the observable effect instead of guessing a tick count.
  async function waitFor(predicate, timeoutMs = 500) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (predicate()) return true;
      await new Promise(resolve => setImmediate(resolve));
      await settle();
    }
    if (predicate()) return true;
    throw new Error(`waitFor timed out after ${timeoutMs}ms: ${String(predicate)}`);
  }
  async function content(message, tabId = 7, frameId = 0) {
    message={...message,sources:message.sources?.map(s=>({...s,playerId:s.playerId||message.players?.find(p=>p.urls?.includes(s.url))?.id}))};
    const listener = events.message.listeners.at(-1);
    let reply = null;
    assert.equal(listener(message, { id: "test-extension", url: message.pageUrl || "https://site.example/watch", tab: { id: tabId }, frameId }, value => { reply = value; }), true);
    for (let round = 0; round < 8; round += 1) {
      await Promise.resolve();
      await new Promise(resolve => setImmediate(resolve));
    }
    return reply;
  }
  function runTimers(delay) {
    for (const [id, entry] of [...timers]) if (delay === undefined || entry.delay === delay) { timers.delete(id); entry.callback(); }
  }
  // The worker settles asynchronously (persisted state load, menu rebuild,
  // serialized toggle chain); tests need a deterministic point past all of it.
  async function settle() {
    for (let round = 0; round < 8; round += 1) {
      // The session write trails a burst by a debounce; a settled worker has written it.
      runTimers(750);
      for (let index = 0; index < 8; index += 1) await Promise.resolve();
      await new Promise(resolve => setImmediate(resolve));
    }
  }
  return { events, storage, content, menu, state, waitFor, context, logs, timers, runTimers, settle, actionCalls, menuEntries, nativeRequests, alarmCreates, downloadCalls, tabCreates, action: browser.action };
}

// The toolbar icon opens popup.html; its switch sends this message to the worker.
async function popupToggle(h) {
  const listener = h.events.message.listeners.at(-1);
  const value = h.storage.local.detectionEnabled === false;
  listener({ channel: "ssdownload-popup", command: "set-detection", value },
    { id: "test-extension", url: "chrome-extension://test-extension/popup.html" }, () => {});
  await h.settle();
}

test("network discovery keeps final resources, filters fragments, and accepts RFC 8187 language tags", async () => {
  const h = backgroundHarness();
  const headers = h.events.headers.listeners.at(-1);
  await headers({ tabId: 7, statusCode: 200, url: "https://cdn.example/video.m3u8?token=ok", responseHeaders: [{ name: "content-type", value: "application/vnd.apple.mpegurl" }], frameId: 0 });
  await headers({ tabId: 7, statusCode: 200, url: "https://cdn.example/segment-0001.m4s", responseHeaders: [{ name: "content-type", value: "video/iso.segment" }], frameId: 0 });
  await headers({ tabId: 7, statusCode: 200, url: "https://cdn.example/video", responseHeaders: [{ name: "content-type", value: "video/mp4" }, { name: "content-disposition", value: "attachment; filename*=UTF-8'tr'video%20ad%C4%B1.mp4" }, { name: "content-length", value: "42" }], frameId: 0 });
  const state = await h.state(7);
  assert.equal(state.items.length, 2);
  assert.deepEqual(state.items.map(item => item.kind).sort(), ["hls", "video"]);
  assert.equal(state.items.find(item => item.kind === "video").filename, "video adı.mp4");
  assert.equal(state.items.find(item => item.kind === "video").size, 42);
});

test("content sources retain direct files but mark blob and WebRTC as non-downloadable", async () => {
  const h = backgroundHarness();
  await h.content({ channel: "ssdownload-content", event: "play", documentId:"doc",pageUrl:"https://site.example/watch",players:[{id:"p",urls:["https://media.example/movie.mp4"]}], sources: [
    { url: "https://media.example/movie.mp4", contentType: "video/mp4", kind: "url", pageUrl: "https://site.example/watch" },
    { url: "blob:https://site.example/abc", kind: "blob", pageUrl: "https://site.example/watch" },
    { url: "webrtc:https://site.example", kind: "webrtc", pageUrl: "https://site.example/watch" },
    { url: "https://files.example/archive.zip", kind: "file", filename: "archive.zip", pageUrl: "https://site.example/watch" },
    { url: `blob:${"x".repeat(16385)}`, kind: "blob", pageUrl: "https://site.example/watch" }
  ] });
  const state = await h.state(7);
  assert.equal(state.items.length, 4);
  assert.equal(state.items.find(item => item.kind === "blob").downloadable, false);
  assert.equal(state.items.find(item => item.kind === "webrtc").downloadable, false);
  assert.equal(state.items.find(item => item.kind === "video").afterPlay, true);
  assert.equal(state.items.find(item => item.kind === "file").filename, "archive.zip");
});

test("the toolbar switch flips discovery, persists it, and keeps the badge in step", async () => {
  const h = backgroundHarness();
  await h.settle();
  const last = (method) => h.actionCalls.filter(call => call.method === method).at(-1);
  assert.equal(last("setBadgeText").text, "AÇIK");
  assert.equal(last("setBadgeBackgroundColor").color, "#16803a");
  assert.match(last("setTitle").title, /açık — ayarlar için tıklayın/);

  await h.content({ channel: "ssdownload-content", event: "scan", sources: [{ url: "https://files.example/one.pdf", kind: "file" }] });
  assert.equal((await h.state(7)).items.length, 1);

  await popupToggle(h); await popupToggle(h); await popupToggle(h);
  assert.equal(h.storage.local.detectionEnabled, false);
  assert.equal(last("setBadgeText").text, "KAPALI");
  assert.equal(last("setBadgeBackgroundColor").color, "#5f6368");
  assert.match(last("setTitle").title, /kapalı/);

  await h.content({ channel: "ssdownload-content", event: "scan", sources: [{ url: "https://files.example/two.pdf", kind: "file" }] });
  assert.equal((await h.state(7)).items.length, 0);
});

test("the toolbar switch refuses context menu submissions while off", async () => {
  const h = backgroundHarness();
  await h.settle();
  await popupToggle(h);
  await h.settle();
  h.events.menu.listeners.at(-1)(
    { menuItemId: "ssdownload-link", linkUrl: "https://files.example/x.zip", pageUrl: "https://site.example/watch" },
    { id: 7, url: "https://site.example/watch" }
  );
  await h.settle();
  assert.equal(h.nativeRequests.filter(request => request?.action?.type === "add").length, 0);
});

test("the player affordance hands the observed candidate to the native picker", async () => {
  const h = backgroundHarness();
  const players = [{ id: "p1", title: "Film", urls: ["https://cdn.example/master.m3u8"], active: true }];
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl: "https://site.example/watch", players, sources: [{ url: "https://cdn.example/master.m3u8", contentType: "application/vnd.apple.mpegurl", playerId: "p1" }] });
  await h.content({ channel: "ssdownload-content", event: "download-video", mediaKind: "video", playerId: "p1", documentId: "d", pageUrl: "https://site.example/watch", players, sources: [] });
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.ok(handoff, "the affordance must open the picker through the native wire");
  assert.equal(handoff.version, 2);
  assert.equal(typeof handoff.action.launch_id, "string");
  assert.ok(handoff.action.launch_id.length > 0 && handoff.action.launch_id.length <= 128);
  assert.equal(handoff.action.session_consent, false);
  assert.equal(handoff.action.request.url, "https://cdn.example/master.m3u8");
  assert.equal(handoff.action.request.kind, "video");
  assert.equal(handoff.action.request.page_url, "https://site.example/watch");
  assert.equal(handoff.action.request.filename, "Film", "an explicit observed title still suggests the name");
  assert.equal(handoff.action.request.request_id, undefined, "the desktop assigns the queue key");
});

test("a captured media address never becomes the handed-over file name", async () => {
  const h = backgroundHarness();
  const pageUrl = "https://site.example/watch";
  const address = "https://cdn.example/videoplayback?itag=18";
  const players = [{ id: "p1", urls: [address], active: true }];
  await h.events.headers.listeners[0]({ tabId: 7, frameId: 0, statusCode: 200, url: address, responseHeaders: [{ name: "content-type", value: "video/mp4" }, { name: "content-length", value: "42" }] });
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl, players, sources: [{ url: address, contentType: "video/mp4", playerId: "p1" }] });
  await h.content({ channel: "ssdownload-content", event: "download-video", mediaKind: "video", playerId: "p1", documentId: "d", pageUrl, players, sources: [] });
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.ok(handoff, "the player must reach the picker");
  assert.equal(handoff.action.request.url, address);
  assert.ok(!String(handoff.action.request.filename || "").includes("videoplayback"), "the address must never become the file name; an untitled player leaves the desktop's own placeholder");
});

test("a blob player with one observed manifest hands that manifest, never its blob address", async () => {
  const h = backgroundHarness();
  const pageUrl = "https://www.youtube.com/watch?v=abc";
  const blob = "blob:https://www.youtube.com/0000";
  const players = [{ id: "movie_player", title: "Film", urls: [blob], active: true }];
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl, players, sources: [{ url: blob, kind: "blob" }] });
  await h.events.headers.listeners[0]({ tabId: 7, frameId: 0, documentId: "d", statusCode: 200, url: "https://cdn.example/master.m3u8", responseHeaders: [{ name: "content-type", value: "application/vnd.apple.mpegurl" }] });
  const reply = await h.content({ channel: "ssdownload-content", event: "download-video", mediaKind: "video", playerId: "movie_player", documentId: "d", pageUrl, players, sources: [] });
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.ok(handoff, "the blob player must reach the picker");
  assert.equal(handoff.action.request.url, "https://cdn.example/master.m3u8", "the inferred manifest is the acted-on player's own source");
  assert.ok(!String(handoff.action.request.url).startsWith("blob:"), "a blob address is never a desktop URL");
  assert.deepEqual(JSON.parse(JSON.stringify(handoff.action.request.source_identity)), { video_id: "0:d:movie_player", frame_id: 0, document_id: "d", page_url: pageUrl });
  assert.equal(reply.ok, true);
});

test("the video context entry on a blob player falls back to the page and its one observed source", async () => {
  const h = backgroundHarness();
  const pageUrl = "https://www.youtube.com/watch?v=abc";
  const blob = "blob:https://www.youtube.com/0000";
  const players = [{ id: "movie_player", title: "Film", urls: [blob], active: true }];
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl, players, sources: [{ url: blob, kind: "blob" }] });
  await h.events.headers.listeners[0]({ tabId: 7, frameId: 0, documentId: "d", statusCode: 200, url: "https://cdn.example/master.m3u8", responseHeaders: [{ name: "content-type", value: "application/vnd.apple.mpegurl" }] });
  await h.menu("ssdownload-video", { tab: { id: 7, url: pageUrl }, info: { srcUrl: blob } });
  await h.waitFor(() => h.nativeRequests.some(request => request?.action?.type === "browser_media"));
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.equal(handoff.action.request.url, "https://cdn.example/master.m3u8", "the blob address must not end the lookup before the page is tried");
});

test("an unresolved blob player hands its page address, never its blob or another player's stream", async () => {
  const h = backgroundHarness();
  const pageUrl = "https://www.youtube.com/watch?v=abc";
  const blob = "blob:https://www.youtube.com/0000";
  const players = [
    { id: "movie_player", title: "Film", urls: [blob], active: true },
    { id: "side", title: "Önerilen", urls: ["https://cdn.example/related.mp4"], active: true }
  ];
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl, players,
    sources: [{ url: blob, kind: "blob" }, { url: "https://cdn.example/related.mp4", contentType: "video/mp4", playerId: "side" }] });
  const reply = await h.content({ channel: "ssdownload-content", event: "download-video", mediaKind: "video", playerId: "movie_player", documentId: "d", pageUrl, players, sources: [] });
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.ok(handoff, "an unresolved player still reaches the picker through its own page address");
  assert.equal(handoff.action.request.url, pageUrl);
  assert.equal(handoff.action.request.page_url, pageUrl);
  assert.notEqual(handoff.action.request.url, "https://cdn.example/related.mp4", "the other player's stream is never substituted");
  assert.equal(h.nativeRequests.filter(request => request?.action?.type === "add").length, 0, "the handoff opens the picker instead of submitting");
  assert.equal(reply.ok, true);
});

test("the video entry on a blob player inside a frame hands over that frame's page, not the top page", async () => {
  const h = backgroundHarness();
  const topUrl = "https://site.example/watch", frameUrl = "https://player.example/embed/1", blob = "blob:https://player.example/inner";
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "dt", pageUrl: topUrl,
    players: [{ id: "main", title: "Ana film", urls: ["blob:https://site.example/top"], active: true }], sources: [] });
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "df", pageUrl: frameUrl,
    players: [{ id: "inner", title: "Gömülü film", urls: [blob], active: true }], sources: [] }, 7, 2);
  await h.menu("ssdownload-video", { tab: { id: 7, url: topUrl }, info: { srcUrl: blob, frameUrl, pageUrl: topUrl } });
  await h.waitFor(() => h.nativeRequests.some(request => request?.action?.type === "browser_media"));
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.equal(handoff.action.request.url, frameUrl, "the frame that holds the player is the source");
  assert.deepEqual(JSON.parse(JSON.stringify(handoff.action.request.source_identity)), { video_id: "2:df:inner", frame_id: 2, document_id: "df", page_url: frameUrl });
});

test("two unresolved blob players on one page are told apart only by an explicit submenu choice", async () => {
  const h = backgroundHarness();
  const pageUrl = "https://site.example/watch";
  const players = [
    { id: "first", title: "Birinci film", urls: ["blob:https://site.example/a"], active: true },
    { id: "second", title: "İkinci film", urls: ["blob:https://site.example/b"], active: true }
  ];
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl, players, sources: [] });
  await h.menu("ssdownload-video", { info: { srcUrl: "blob:https://site.example/b" } });
  assert.equal(h.nativeRequests.filter(request => request?.action?.type === "browser_media").length, 0, "neither player is picked silently");
  assert.match((await h.state(7)).status.message, /Kaynak seç menüsünden/);
  h.runTimers(250);
  await h.settle();
  await h.menu("ssdownload-candidate:1");
  await h.waitFor(() => h.nativeRequests.some(request => request?.action?.type === "browser_media"));
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.equal(handoff.action.request.source_identity.video_id, "0:d:second", "the explicit choice hands over that player");
});

test("a blob player with several observed roots never gets an arbitrary one", async () => {
  const h = backgroundHarness();
  const pageUrl = "https://site.example/watch";
  const players = [{ id: "p", title: "Film", urls: ["blob:https://site.example/main"], active: true }];
  await h.content({ channel: "ssdownload-content", event: "play", documentId: "d", pageUrl, players, sources: [] });
  for (const name of ["one", "two"]) await h.events.headers.listeners[0]({ tabId: 7, frameId: 0, documentId: "d", statusCode: 200, url: `https://cdn.example/${name}.m3u8`, responseHeaders: [{ name: "content-type", value: "application/vnd.apple.mpegurl" }] });
  const reply = await h.content({ channel: "ssdownload-content", event: "download-video", mediaKind: "video", playerId: "p", documentId: "d", pageUrl, players, sources: [] });
  assert.equal(h.nativeRequests.filter(request => request?.action?.type === "browser_media").length, 0, "an ambiguous player must not have one root chosen for it");
  assert.match(String(reply.error), /Kaynak seç menüsünden/);
  h.runTimers(250);
  await h.settle();
  assert.equal(h.menuEntries.filter(entry => entry.parentId === "ssdownload-candidates").length, 2, "both roots stay reachable through the submenu");
  await h.menu("ssdownload-candidate:0");
  await h.waitFor(() => h.nativeRequests.some(request => request?.action?.type === "browser_media"));
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.ok(["https://cdn.example/one.m3u8", "https://cdn.example/two.m3u8"].includes(handoff.action.request.url), "the explicit choice hands over that exact root");
});

test("a preview player is refused implicitly and reachable only by explicit choice", async () => {
  const h = backgroundHarness();
  const pageUrl = "https://site.example/";
  const players = [{ id: "preview", title: "Tanıtım", urls: ["https://cdn.example/highlight.mp4"], active: true, loop: true, muted: true, userActivated: false }];
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl, players,
    sources: [{ url: "https://cdn.example/highlight.mp4", contentType: "video/mp4", playerId: "preview" }] });
  const reply = await h.content({ channel: "ssdownload-content", event: "download-video", mediaKind: "video", playerId: "preview", documentId: "d", pageUrl, players, sources: [] });
  assert.equal(h.nativeRequests.filter(request => request?.action?.type === "browser_media").length, 0, "a preview never reaches the picker implicitly");
  assert.match(String(reply.error), /Kaynak seç menüsünden/);
  h.runTimers(250);
  await h.settle();
  await h.menu("ssdownload-candidate:0");
  await h.waitFor(() => h.nativeRequests.some(request => request?.action?.type === "browser_media"));
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.equal(handoff.action.request.url, "https://cdn.example/highlight.mp4", "the submenu is the explicit way to that preview");
});

test("a blocked player document refuses the affordance with its own reason", async () => {
  const h = backgroundHarness();
  const pageUrl = "https://frame.example/iframe";
  const players = [{ id: "p", title: "Bölüm", urls: ["blob:https://frame.example/main"], active: true }];
  await h.events.headers.listeners[0]({ tabId: 7, frameId: 2, type: "sub_frame", documentId: "d", statusCode: 403, url: pageUrl, responseHeaders: [] });
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl, players, sources: [] }, 7, 2);
  const reply = await h.content({ channel: "ssdownload-content", event: "download-video", mediaKind: "video", playerId: "p", documentId: "d", pageUrl, players, sources: [] }, 7, 2);
  assert.equal(h.nativeRequests.filter(request => request?.action?.type === "browser_media").length, 0, "a page that answered 403 is never handed over");
  assert.match(String(reply.error), /HTTP 403/);
});

test("the audio affordance keeps its requested kind on a blob player handoff", async () => {
  const h = backgroundHarness();
  const pageUrl = "https://site.example/watch";
  const players = [{ id: "p", title: "Film", urls: ["blob:https://site.example/main"], active: true }];
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl, players, sources: [] });
  await h.events.headers.listeners[0]({ tabId: 7, frameId: 0, documentId: "d", statusCode: 200, url: "https://cdn.example/master.m3u8", responseHeaders: [{ name: "content-type", value: "application/vnd.apple.mpegurl" }] });
  const reply = await h.content({ channel: "ssdownload-content", event: "download-video", mediaKind: "audio", playerId: "p", documentId: "d", pageUrl, players, sources: [] });
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.ok(handoff, "the audio affordance opens the same native picker");
  assert.equal(handoff.action.request.url, "https://cdn.example/master.m3u8");
  assert.equal(handoff.action.request.kind, "audio", "the picker must not open a video request for an audio affordance");
  assert.equal(reply.ok, true);
});

test("a renewed source address gets a fresh launch identity", async () => {
  const h = backgroundHarness();
  const open = async urls => {
    await h.content({ channel: "ssdownload-content", event: "download-video", mediaKind: "video", playerId: "p1", documentId: "d",
      pageUrl: "https://site.example/watch", players: [{ id: "p1", title: "Film", urls, active: true }], sources: [] });
    return h.nativeRequests.filter(request => request?.action?.type === "browser_media").at(-1);
  };
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl: "https://site.example/watch",
    players: [{ id: "p1", title: "Film", urls: ["https://cdn.example/a.mp4?token=1"], active: true }],
    sources: [{ url: "https://cdn.example/a.mp4?token=1", contentType: "video/mp4", playerId: "p1" }] });
  const first = await open(["https://cdn.example/a.mp4?token=1"]);
  const repeated = await open(["https://cdn.example/a.mp4?token=1"]);
  assert.equal(repeated.action.launch_id, first.action.launch_id, "the same candidate keeps one launch");
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl: "https://site.example/watch",
    players: [{ id: "p1", title: "Film", urls: ["https://cdn.example/a.mp4?token=2"], active: true }],
    sources: [{ url: "https://cdn.example/a.mp4?token=2", contentType: "video/mp4", playerId: "p1" }] });
  const renewed = await open(["https://cdn.example/a.mp4?token=2"]);
  assert.equal(renewed.action.request.url, "https://cdn.example/a.mp4?token=2");
  assert.notEqual(renewed.action.launch_id, first.action.launch_id, "a renewed address must not reuse the active launch");
});

test("cookies travel only after an explicit gesture records consent for the site", async () => {
  const h = backgroundHarness();
  const players = [{ id: "p1", title: "Film", urls: ["https://cdn.example/master.m3u8"], active: true }];
  const open = async () => {
    await h.content({ channel: "ssdownload-content", event: "download-video", mediaKind: "video", playerId: "p1", documentId: "d",
      pageUrl: "https://site.example/watch", players, sources: [] });
    return h.nativeRequests.filter(request => request?.action?.type === "browser_media").at(-1);
  };
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl: "https://site.example/watch", players,
    sources: [{ url: "https://cdn.example/master.m3u8", contentType: "application/vnd.apple.mpegurl", playerId: "p1" }] });
  const before = await open();
  assert.equal(before.action.session_consent, false);
  assert.equal(before.action.request.session_cookies, undefined);
  await h.menu("ssdownload-session-grant");
  vm.runInContext('browser.cookies.getAll = async () => [{ name: "sid", value: "v", domain: "cdn.example", path: "/", storeId: "0", secure: true }]', h.context);
  const after = await open();
  assert.equal(after.action.session_consent, true);
  assert.equal(after.action.request.session_cookies.length, 1);
  assert.equal(after.action.request.session_cookies[0].name, "sid");
  assert.equal(after.action.request.session_cookies[0].host_only, false);
  await h.menu("ssdownload-session-revoke");
  const revoked = await open();
  assert.equal(revoked.action.session_consent, false);
  assert.equal(revoked.action.request.session_cookies, undefined);
});

test("ambiguous candidates are reachable only through an explicit submenu choice", async () => {
  const h = backgroundHarness();
  const players = [
    { id: "p1", title: "Bölüm 1", urls: ["https://cdn.example/one.mp4"], active: true, adSignals: ["player-identity", "request-host"] },
    { id: "p2", title: "Bölüm 2", urls: ["https://cdn.example/two.mp4"], active: true, adSignals: ["player-identity", "request-host"] }
  ];
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl: "https://site.example/watch", players,
    sources: [{ url: "https://cdn.example/one.mp4", contentType: "video/mp4", playerId: "p1" },
      { url: "https://cdn.example/two.mp4", contentType: "video/mp4", playerId: "p2" }] });
  const state = await h.state(7);
  assert.equal(state.videos.length, 0, "flagged candidates must not become implicit videos");
  assert.ok(state.candidates.length >= 1);
  h.runTimers(250);
  await h.settle();
  const submenu = h.menuEntries.filter(entry => entry.parentId === "ssdownload-candidates");
  assert.ok(submenu.length >= 1, "ambiguous candidates stay reachable through the submenu");
  assert.equal(h.nativeRequests.filter(request => request?.action?.type === "browser_media").length, 0);
  await h.menu("ssdownload-candidate:0");
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.ok(handoff, "choosing a submenu entry opens the picker for that exact candidate");
  assert.ok(["https://cdn.example/one.mp4", "https://cdn.example/two.mp4"].includes(handoff.action.request.url));
});

test("media context entries always open the picker and never submit a download directly", async () => {
  const h = backgroundHarness();
  await h.settle();
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl: "https://site.example/watch",
    players: [{ id: "p1", title: "Film", urls: ["https://cdn.example/master.m3u8"], active: true }],
    sources: [{ url: "https://cdn.example/master.m3u8", contentType: "application/vnd.apple.mpegurl", playerId: "p1" }] });
  await h.menu("ssdownload-video", { info: { srcUrl: "https://cdn.example/master.m3u8" } });
  await h.waitFor(() => h.nativeRequests.some(request => request?.action?.type === "browser_media"));
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.equal(handoff.action.request.url, "https://cdn.example/master.m3u8", "the acted-on source is the one handed over");
  assert.equal(handoff.action.request.kind, "video");
  assert.equal(h.nativeRequests.filter(request => request?.action?.type === "add").length, 0, "media never bypasses the picker");
});

test("a page with several selectable sources requires an explicit submenu choice", async () => {
  const h = backgroundHarness();
  await h.settle();
  const players = [
    { id: "p1", title: "Bölüm 1", urls: ["https://cdn.example/one.mp4"], active: true },
    { id: "p2", title: "Bölüm 2", urls: ["https://cdn.example/two.mp4"], active: true }
  ];
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl: "https://site.example/watch", players,
    sources: [{ url: "https://cdn.example/one.mp4", contentType: "video/mp4", playerId: "p1" },
      { url: "https://cdn.example/two.mp4", contentType: "video/mp4", playerId: "p2" }] });
  h.runTimers(250);
  await h.settle();
  const entries = h.menuEntries.filter(entry => entry.parentId === "ssdownload-candidates");
  assert.equal(entries.length, 2, "every selectable source is listed, not only flagged ones");
  assert.match(h.menuEntries.find(entry => entry.id === "ssdownload-candidates").title, /\(2\)/);
  await h.menu("ssdownload-media", { info: { pageUrl: "https://site.example/watch" } });
  await h.settle();
  assert.equal(h.nativeRequests.filter(request => request?.action?.type === "browser_media").length, 0, "no source is chosen implicitly");
  const status = (await h.state(7)).status;
  assert.equal(status.error, true);
  assert.match(status.message, /Kaynak seç menüsünden/);
  await h.menu("ssdownload-candidate:1");
  await h.waitFor(() => h.nativeRequests.some(request => request?.action?.type === "browser_media"));
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.equal(handoff.action.request.url, "https://cdn.example/two.mp4", "the chosen entry is the one handed over");
  assert.deepEqual(JSON.parse(JSON.stringify(handoff.action.request.source_identity)), { video_id: "0:d:p2", frame_id: 0, document_id: "d", page_url: "https://site.example/watch" });
});

test("every selectable source stays reachable in the submenu up to the discovery cap", async () => {
  const h = backgroundHarness();
  await h.settle();
  const players = Array.from({ length: 30 }, (_, index) => ({ id: `p${index + 1}`, title: `Kaynak ${index + 1}`,
    urls: [`https://cdn.example/source-${index + 1}.mp4`], active: true, adSignals: ["player-identity", "request-host"] }));
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl: "https://site.example/watch", players,
    sources: players.map(player => ({ url: player.urls[0], contentType: "video/mp4", playerId: player.id })) });
  h.runTimers(250);
  await h.settle();
  const listed = h.menuEntries.filter(entry => String(entry.id || "").startsWith("ssdownload-candidate:"));
  const header = h.menuEntries.find(entry => entry.id === "ssdownload-candidates");
  assert.equal(listed.length, 30, "the submenu must list every selectable source instead of a truncated slice");
  assert.match(header.title, /\(30\)/, "the header must state the full count");
  assert.ok(h.menuEntries.some(entry => entry.id === "ssdownload-candidate-sep:24"), "a separator groups the list without hiding entries");
  assert.equal(h.menuEntries.filter(entry => entry.type === "separator").length, 1);
  await h.menu("ssdownload-candidate:29");
  await h.waitFor(() => h.nativeRequests.some(request => request?.action?.type === "browser_media"));
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.equal(handoff.action.request.url, "https://cdn.example/source-30.mp4", "the last listed entry must hand over its own source, not the first");
});

test("the switch and an incompatible desktop block every media entry with a visible error", async () => {
  const off = backgroundHarness();
  await off.settle();
  await off.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl: "https://site.example/watch",
    players: [{ id: "p1", title: "Film", urls: ["https://cdn.example/master.m3u8"], active: true }],
    sources: [{ url: "https://cdn.example/master.m3u8", contentType: "application/vnd.apple.mpegurl", playerId: "p1" }] });
  await popupToggle(off);
  await off.settle();
  await off.menu("ssdownload-video", { info: { srcUrl: "https://cdn.example/master.m3u8" } });
  await off.settle();
  assert.equal(off.nativeRequests.filter(request => request?.action?.type === "browser_media").length, 0);
  assert.match((await off.state(7)).status.message, /SSDownload kapalı/);

  const incompatible = backgroundHarness({ native: async request => request?.action?.type === "capabilities"
    ? { ok: true, result: { capabilities: { protocol: 2, capability_version: 1, usage_modes: { video: true, file: true, audio: true },
        onboarding_version: 1, settings_revision: 1 } } }
    : { ok: true, message: "Tamam" } });
  await incompatible.settle();
  await incompatible.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl: "https://site.example/watch",
    players: [{ id: "p1", title: "Film", urls: ["https://cdn.example/master.m3u8"], active: true }],
    sources: [{ url: "https://cdn.example/master.m3u8", contentType: "application/vnd.apple.mpegurl", playerId: "p1" }] });
  await incompatible.menu("ssdownload-video", { info: { srcUrl: "https://cdn.example/master.m3u8" } });
  await incompatible.settle();
  assert.equal(incompatible.nativeRequests.filter(request => request?.action?.type === "browser_media").length, 0,
    "an old capability desktop must never receive browser_media");
  assert.equal((await incompatible.state(7)).status.error, true, "the refusal must reach the page as a visible error");
});

test("site-level entries exist only while the desktop offers them", async () => {
  const pageUrl = "https://site.example/watch";
  const capability = (extra = {}) => async request => request?.action?.type === "capabilities"
    ? { ok: true, result: { capabilities: { protocol: 2, capability_version: 3, usage_modes: { video: true, file: true, audio: true }, ...extra } } }
    : { ok: true, message: "Tamam" };
  const pageCandidate = { channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl, pageTitle: "Film",
    players: [], sources: [], playerSignals: { embeds: [], containers: true, playerScripts: true } };
  const handoffCount = h => h.nativeRequests.filter(request => request?.action?.type === "browser_media").length;

  // A desktop that does not offer site entries registers no page item at all; every media
  // entry stays exactly as it was.
  const off = backgroundHarness({ native: capability({ site_entries: false }) });
  await off.settle();
  await off.waitFor(() => off.menuEntries.some(entry => entry.id === "ssdownload-video"));
  assert.equal(off.menuEntries.some(entry => entry.id === "ssdownload-page"), false, "no page entry without the capability");
  assert.equal(off.menuEntries.some(entry => entry.id === "ssdownload-media"), true, "the picker entry is a media entry and stays");
  await off.content(pageCandidate);
  const refused = await off.content({ ...pageCandidate, event: "download-page", mediaKind: "video" });
  assert.equal(handoffCount(off), 0, "a page handoff must not reach a desktop that does not offer it");
  assert.match(String(refused.error), /kapalı/);

  // A flag the desktop never sends is the same as false: the default is off.
  const absent = backgroundHarness({ native: capability() });
  await absent.settle();
  await absent.waitFor(() => absent.menuEntries.some(entry => entry.id === "ssdownload-video"));
  assert.equal(absent.menuEntries.some(entry => entry.id === "ssdownload-page"), false, "a missing flag means off");

  // A refresh that flips the flag rebuilds the menu in both directions.
  const on = backgroundHarness({ native: capability({ site_entries: true }), pageHtml: "<html><body>film</body></html>" });
  await on.settle();
  await on.waitFor(() => on.menuEntries.some(entry => entry.id === "ssdownload-page"));
  const activate = value => `capabilityState=validateCapabilities({ok:true,result:{capabilities:{protocol:2,capability_version:3,site_entries:${value},usage_modes:{video:true,file:true,audio:true}}}})`;
  await vm.runInContext(activate(false), on.context);
  await vm.runInContext("createMenus()", on.context);
  await on.settle();
  assert.equal(on.menuEntries.some(entry => entry.id === "ssdownload-page"), false, "the refresh must withdraw the page entry");
  assert.equal(on.menuEntries.some(entry => entry.id === "ssdownload-video"), true, "the media entries survive the refresh");
  await vm.runInContext(activate(true), on.context);
  await vm.runInContext("createMenus()", on.context);
  await on.settle();
  assert.equal(on.menuEntries.some(entry => entry.id === "ssdownload-page"), true, "and come back when the desktop offers them again");
  // The registered entry hands the page over with the page's own markup for the report.
  await on.content(pageCandidate);
  await on.menu("ssdownload-page");
  await on.waitFor(() => handoffCount(on) > 0);
  const handoff = on.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.equal(handoff.action.request.url, pageUrl, "the page entry hands over the acted-on document");
  assert.equal(handoff.action.request.page_html, "<html><body>film</body></html>");
});

test("the picker handoff carries the page's own bounded markup and omits what it cannot capture", async () => {
  const players = [{ id: "p1", title: "Film", urls: ["https://cdn.example/master.m3u8"], active: true }];
  const scan = { channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl: "https://site.example/watch", players,
    sources: [{ url: "https://cdn.example/master.m3u8", contentType: "application/vnd.apple.mpegurl", playerId: "p1" }] };
  const submit = { ...scan, event: "download-video", mediaKind: "video", playerId: "p1", sources: [] };
  const handoffOf = h => h.nativeRequests.find(request => request?.action?.type === "browser_media");

  // The page's markup is the content script's answer at the moment of the handoff.
  const captured = backgroundHarness({ pageHtml: "<html><body>film</body></html>" });
  await captured.content(scan);
  await captured.content(submit);
  assert.equal(handoffOf(captured).action.request.page_html, "<html><body>film</body></html>");

  // An oversized capture is cut at the bound, between two characters, with the marker in place.
  const oversized = backgroundHarness({ pageHtml: "ş".repeat(200000) });
  await oversized.content(scan);
  await oversized.content(submit);
  const bounded = handoffOf(oversized).action.request.page_html;
  assert.equal(typeof bounded, "string");
  assert.equal(bounded.endsWith("<!-- ssdownload:page-html-truncated -->"), true, "a cut capture states the cut");
  assert.ok(Buffer.byteLength(bounded, "utf8") <= 192 * 1024, "the report never exceeds the bound");
  assert.ok(Buffer.byteLength(bounded, "utf8") > 192 * 1024 - 64, "the capture fills the bound it is cut to");
  assert.equal(/[\uD800-\uDFFF]/.test(bounded), false, "the cut lands between two characters");

  // A page that cannot produce its markup (about:blank, a restarted content script, an empty
  // answer) is the omitted field, never an empty string.
  const silent = backgroundHarness({ pageHtml: () => ({}) });
  await silent.content(scan);
  await silent.content(submit);
  const media = handoffOf(silent);
  assert.ok(media);
  assert.equal("page_html" in media.action.request, false, "an unavailable capture stays out of the request");
  assert.equal(media.action.request.url, "https://cdn.example/master.m3u8", "the handoff itself is unaffected");

  // A plain file link is an address the user pointed at, not a page handoff.
  const files = backgroundHarness({ pageHtml: "<html><body>sayfa</body></html>" });
  await files.content({ channel: "ssdownload-content", event: "scan", sources: [{ url: "https://files.example/one.pdf", kind: "file" }] });
  await files.menu("ssdownload-link", { info: { linkUrl: "https://files.example/one.pdf" } });
  await files.waitFor(() => files.nativeRequests.some(request => request?.action?.type === "add"));
  const added = files.nativeRequests.find(request => request?.action?.type === "add");
  assert.equal("page_html" in added.action.request, false, "a file link download carries no page markup");
});

test("an unobserved Referer is never invented for the handed-over request", async () => {
  const h = backgroundHarness();
  await h.settle();
  await h.events.headers.listeners.at(-1)({ tabId: 7, statusCode: 200, url: "https://cdn.example/movie.mp4", frameId: 0,
    responseHeaders: [{ name: "content-type", value: "video/mp4" }] });
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl: "https://site.example/watch",
    players: [{ id: "p1", title: "Film", urls: ["https://cdn.example/movie.mp4"], active: true }], sources: [] });
  await h.content({ channel: "ssdownload-content", event: "download-video", mediaKind: "video", playerId: "p1", documentId: "d",
    pageUrl: "https://site.example/watch", players: [{ id: "p1", title: "Film", urls: ["https://cdn.example/movie.mp4"], active: true }], sources: [] });
  const handoff = h.nativeRequests.find(request => request?.action?.type === "browser_media");
  assert.ok(handoff);
  assert.equal(handoff.action.request.referer, null, "an unobserved Referer must not become the page address");
});

test("the relay starts only from the explicit context entry for an authorized job", async () => {
  const calls = [];
  const h = backgroundHarness({
    navigation: { async getAllFrames() { return [{ frameId: 0, documentId: "doc-top", url: "https://site.example/watch" }]; } },
    native: async request => request?.action?.type === "capabilities"
      ? { ok: true, result: { capabilities: { protocol: 2, capability_version: 3, usage_modes: { video: true, file: true, audio: true },
          onboarding_version: 1, settings_revision: 1, source_refresh_jobs: [{ id: "job-1", page_url: "https://site.example/watch", title: "Film" }] } } }
      : { ok: true, message: "Tamam" }
  });
  vm.runInContext("globalThis.SSDownloadRelayControl = { start: message => { globalThis.__relayCalls.push(message); return Promise.resolve({ ok: true, state: 'running' }); } }", h.context);
  vm.runInContext("globalThis.__relayCalls = []", h.context);
  await h.settle();
  await h.waitFor(() => h.menuEntries.some(entry => entry.id === "ssdownload-relay"));
  assert.deepEqual(vm.runInContext("JSON.stringify(globalThis.__relayCalls)", h.context), "[]", "no transfer starts without the user gesture");
  const jobEntry = h.menuEntries.find(entry => entry.id === "ssdownload-relay:0");
  assert.equal(jobEntry.title, "Film");
  assert.ok(h.menuEntries.some(entry => entry.id === "ssdownload-relay-restart:0"));
  await h.menu("ssdownload-relay:0");
  await h.waitFor(() => vm.runInContext("globalThis.__relayCalls.length", h.context) > 0);
  assert.deepEqual(JSON.parse(vm.runInContext("JSON.stringify(globalThis.__relayCalls[0])", h.context)),
    { jobId: "job-1", tabId: 7, frameId: 0, documentId: "doc-top", restart: false });
  await h.menu("ssdownload-relay-restart:0");
  await h.waitFor(() => vm.runInContext("globalThis.__relayCalls.length", h.context) > 1);
  assert.equal(JSON.parse(vm.runInContext("JSON.stringify(globalThis.__relayCalls[1])", h.context)).restart, true);
});

test("the switch clears discoveries and de-duplicates context submissions across a worker restart", async () => {
  const h = backgroundHarness();
  await h.content({ channel: "ssdownload-content", event: "scan", sources: [{ url: "https://files.example/one.pdf", kind: "file" }] });
  assert.equal((await h.state(7)).items.length, 1);
  await h.menu("ssdownload-link", { info: { linkUrl: "https://files.example/one.pdf" } });
  await h.waitFor(() => h.nativeRequests.some(request => request?.action?.type === "add"));
  assert.equal(h.nativeRequests.filter(request => request?.action?.type === "add").length, 1);
  await h.menu("ssdownload-link", { info: { linkUrl: "https://files.example/one.pdf" } });
  assert.equal(h.nativeRequests.filter(request => request?.action?.type === "add").length, 1, "a repeated click must not queue the same job twice");
  await popupToggle(h);
  await h.settle();
  assert.equal((await h.state(7)).items.length, 0);
  const restarted = backgroundHarness({ session: h.storage.session });
  assert.equal(restarted.storage.session.downloadTickets.length, 1);
  assert.equal(restarted.storage.session.downloadTickets[0][1].submitted, true);
});

test("native host failures are converted into an actionable browser message", async () => {
  const h = backgroundHarness({ native: async () => { throw new Error("Native messaging host not found"); } });
  await h.menu("ssdownload-app");
  const status = JSON.parse(vm.runInContext("JSON.stringify(tabStatus.get(7)||null)", h.context));
  assert.match(String(status?.message || ""), /uygulaması bu bilgisayarda bulunamadı/i);
  assert.equal(status?.error, true);
});

test("the popup offers the download page only when the app is missing, not when it refuses the extension", async () => {
  const popup = h => new Promise(resolve => h.events.message.listeners.at(-1)({ channel: "ssdownload-popup", command: "state" },
    { id: "test-extension", url: "chrome-extension://test-extension/popup.html" }, resolve));
  const missing = backgroundHarness({ native: async () => { throw new Error("Specified native messaging host not found."); } });
  await missing.settle();
  const state = await popup(missing);
  assert.equal(state.appMissing, true);
  assert.match(state.capabilityError, /bulunamadı/);
  const refused = backgroundHarness({ native: async () => { throw new Error("Access to the specified native messaging host is forbidden."); } });
  await refused.settle();
  assert.equal((await popup(refused)).appMissing, false);
});

test("a content script on a page named popup.html cannot speak for the popup", async () => {
  const h = backgroundHarness();
  await h.settle();
  const reply = await new Promise(resolve => h.events.message.listeners.at(-1)({ channel: "ssdownload-popup", command: "state" },
    { id: "test-extension", url: "https://site.example/popup.html", tab: { id: 7 } }, resolve));
  assert.ok(reply.error, "a page sender is refused");
  assert.equal(reply.detectionEnabled, undefined);
});

test("without the app the extension adds no context entries, but a refusing app keeps Open SSDownload", async () => {
  const missing = backgroundHarness({ native: async () => { throw new Error("Specified native messaging host not found."); } });
  await missing.settle();
  assert.deepEqual(missing.menuEntries.map(entry => entry.id), []);
  await missing.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl: "https://site.example/watch",
    players: [{ id: "p1", title: "Film", urls: ["https://cdn.example/one.mp4"], active: true }],
    sources: [{ url: "https://cdn.example/one.mp4", contentType: "video/mp4", playerId: "p1" }] });
  assert.equal((await missing.state(7)).videos.length, 1, "the page has a detected source");
  missing.runTimers(250);
  await missing.settle();
  assert.deepEqual(missing.menuEntries.map(entry => entry.id), [], "a detected source does not add a Choose source submenu");
  const refused = backgroundHarness({ native: async () => { throw new Error("Access to the specified native messaging host is forbidden."); } });
  await refused.settle();
  assert.ok(refused.menuEntries.some(entry => entry.id === "ssdownload-app"));
});

test("a refused extension identity is reported as such, never as a missing registration", async () => {
  // Chrome's wording contains "specified native messaging host", the not-found phrase.
  const h = backgroundHarness({ native: async () => { throw new Error("Access to the specified native messaging host is forbidden."); } });
  await h.menu("ssdownload-app");
  const status = JSON.parse(vm.runInContext("JSON.stringify(tabStatus.get(7)||null)", h.context));
  assert.match(String(status?.message || ""), /izin vermiyor/i);
  assert.doesNotMatch(String(status?.message || ""), /bulunamadı/i);
  assert.equal(status?.error, true);
});

function takeoverNative({ completed = () => 0, accept = () => ({ ok: true, message: "Tamam" }) } = {}) {
  return async request => request?.action?.type === "capabilities"
    ? { ok: true, result: { capabilities: { protocol: 2, capability_version: 3, site_entries: true, browser_takeover: true, completed_jobs: completed(),
      usage_modes: { video: true, file: true, audio: true }, onboarding_version: 1, settings_revision: 1, source_refresh_jobs: [] } } }
    : accept(request);
}
const takeoverItem = (extra = {}) => ({ id: 41, url: "https://files.example/get?id=7", finalUrl: "https://files.example/dl/clip.mkv", filename: "",
  state: "in_progress", incognito: false, referrer: "https://site.example/page", ...extra });

test("a link handed over from the menu leaves the file name to the server instead of the address basename", async () => {
  const h = backgroundHarness();
  await h.menu("ssdownload-link", { info: { linkUrl: "https://files.example/download.php?id=7" } });
  await h.waitFor(() => h.nativeRequests.some(request => request?.action?.type === "add"));
  const added = h.nativeRequests.find(request => request?.action?.type === "add");
  // The desktop uses a suggested name verbatim and only reads Content-Disposition when none is given.
  assert.equal(added.action.request.filename, null);
  await h.menu("ssdownload-link", { info: { linkUrl: "https://files.example/movie.mp4" } });
  await h.waitFor(() => h.nativeRequests.filter(request => request?.action?.type === "add").length === 2);
  assert.equal(h.nativeRequests.filter(request => request?.action?.type === "add")[1].action.request.filename, null, "a media suffix is never cut from a plain file");
});

test("download takeover keeps the browser's file name with its extension and erases the browser copy", async () => {
  const h = backgroundHarness({ native: takeoverNative() });
  h.events.download.listeners.at(-1)(takeoverItem({ filename: "C:\\Users\\me\\Downloads\\clip.mkv" }));
  await h.waitFor(() => h.downloadCalls.some(call => call.method === "erase"));
  const added = h.nativeRequests.find(request => request?.action?.type === "add");
  assert.equal(added.action.request.kind, "file");
  assert.equal(added.action.request.filename, "clip.mkv");
  assert.equal(added.action.request.url, "https://files.example/dl/clip.mkv");
  assert.equal(added.action.request.referer, "https://site.example/page");
  assert.deepEqual(h.downloadCalls.map(call => call.method), ["pause", "cancel", "erase"]);
});

test("download takeover without a browser file name lets the desktop read the server's name", async () => {
  const h = backgroundHarness({ native: takeoverNative() });
  h.events.download.listeners.at(-1)(takeoverItem());
  await h.waitFor(() => h.downloadCalls.some(call => call.method === "erase"));
  assert.equal(h.nativeRequests.find(request => request?.action?.type === "add").action.request.filename, null);
});

test("a refused takeover resumes the browser download", async () => {
  const h = backgroundHarness({ native: takeoverNative({ accept: () => ({ ok: false, message: "SSD-EXT-001 reddedildi" }) }) });
  h.events.download.listeners.at(-1)(takeoverItem());
  await h.waitFor(() => h.downloadCalls.some(call => call.method === "resume"));
  assert.deepEqual(h.downloadCalls.map(call => call.method), ["pause", "resume"]);
});

test("a download that wakes a cold worker honours the stored OFF switch", async () => {
  // The worker has not read its storage yet when the event arrives: the default must not decide.
  const h = backgroundHarness({ local: { detectionEnabled: false }, native: takeoverNative() });
  h.events.download.listeners.at(-1)(takeoverItem({ filename: "clip.mkv" }));
  await h.settle();
  assert.deepEqual(h.downloadCalls, [], "the browser keeps its download while the switch is off");
  assert.equal(h.nativeRequests.filter(request => request?.action?.type === "add").length, 0);
});

test("a context menu click that wakes a cold worker honours the stored OFF switch", async () => {
  const h = backgroundHarness({ local: { detectionEnabled: false } });
  h.events.menu.listeners.at(-1)(
    { menuItemId: "ssdownload-link", linkUrl: "https://files.example/x.zip", pageUrl: "https://site.example/watch" },
    { id: 7, url: "https://site.example/watch" }
  );
  await h.settle();
  assert.equal(h.nativeRequests.filter(request => request?.action?.type === "add").length, 0);
});

test("completed downloads are still counted after the worker restarted", async () => {
  let completed = 0;
  const alarmStore = new Map();
  const native = takeoverNative({ completed: () => completed });
  const first = backgroundHarness({ native, alarmStore });
  await first.menu("ssdownload-link", { info: { linkUrl: "https://files.example/x.zip" } });
  await first.waitFor(() => first.nativeRequests.some(request => request?.action?.type === "add"));
  assert.equal(first.storage.session.completion.baseline, 0, "the baseline is recorded with the session");
  assert.ok(first.storage.session.completion.handoffAt > 0);
  // Two downloads finish while the suspended worker is gone.
  completed = 2;
  const second = backgroundHarness({ native, alarmStore, session: first.storage.session, local: first.storage.local });
  await second.settle();
  const badge = second.actionCalls.filter(call => call.method === "setBadgeText").at(-1);
  assert.equal(badge.text, "2");
  assert.equal(second.storage.session.completion.pending, 2);
});

test("the completion alarm of a restarted worker still polls the desktop after a recent handoff", async () => {
  let completed = 0;
  const alarmStore = new Map();
  const native = takeoverNative({ completed: () => completed });
  const first = backgroundHarness({ native, alarmStore });
  await first.menu("ssdownload-link", { info: { linkUrl: "https://files.example/x.zip" } });
  await first.waitFor(() => first.nativeRequests.some(request => request?.action?.type === "add"));
  const second = backgroundHarness({ native, alarmStore, session: first.storage.session, local: first.storage.local });
  await second.settle();
  const probes = () => second.nativeRequests.filter(request => request?.action?.type === "capabilities").length;
  const before = probes();
  completed = 1;
  second.events.alarm.listeners.at(-1)({ name: "ssdownload-completion-poll" });
  await second.waitFor(() => probes() === before + 1);
  await second.settle();
  assert.equal(second.actionCalls.filter(call => call.method === "setBadgeText").at(-1).text, "1");
});

test("a worker start keeps the completion alarm that already exists", async () => {
  const alarmStore = new Map();
  const first = backgroundHarness({ alarmStore });
  await first.settle();
  assert.equal(first.alarmCreates.filter(call => call.name === "ssdownload-completion-poll").length, 1);
  // Creating it again would restart its period, so a worker that wakes every few
  // seconds would postpone the poll for as long as it keeps waking.
  const restarted = backgroundHarness({ alarmStore });
  await restarted.settle();
  assert.equal(restarted.alarmCreates.length, 0);
});

test("a capability probe that finds the desktop closed says so and is retried within seconds", async () => {
  let running = false;
  const h = backgroundHarness({ native: async request => request?.action?.type === "capabilities"
    ? (running ? nativeCapabilities : { ok: false, message: "SSD-BRG-005 SSDownload uygulamasına ulaşılamadı" })
    : { ok: true, message: "Tamam" } });
  await h.settle();
  const popup = () => new Promise(resolve => h.events.message.listeners.at(-1)({ channel: "ssdownload-popup", command: "state" },
    { id: "test-extension", url: "chrome-extension://test-extension/popup.html" }, resolve));
  const closed = await popup();
  assert.match(closed.capabilityError, /çalışmıyor/);
  assert.doesNotMatch(closed.capabilityError, /protokol/, "a closed desktop is not a protocol mismatch");
  running = true;
  // Well inside the old one-minute cache, past the five-second one.
  vm.runInContext("capabilityState.checkedAt = Date.now() - 6000", h.context);
  assert.equal((await popup()).capabilityError, null);
});

test("the discovery session is written once after a burst and flushed when the worker suspends", async () => {
  const h = backgroundHarness();
  await h.settle();
  delete h.storage.session.discovery;
  const scan = url => h.content({ channel: "ssdownload-content", event: "scan", sources: [{ url, kind: "file" }] });
  const pending = () => [...h.timers.values()].filter(timer => timer.delay === 750).length;
  const drain = async () => { for (let round = 0; round < 4; round += 1) await new Promise(resolve => setImmediate(resolve)); };
  await scan("https://files.example/one.pdf");
  await scan("https://files.example/one.pdf");
  assert.equal(h.storage.session.discovery, undefined, "nothing is written while the burst runs");
  assert.equal(pending(), 1, "one trailing write covers the whole burst");
  h.runTimers(750);
  await drain();
  assert.ok(h.storage.session.discovery, "the trailing write lands");
  delete h.storage.session.discovery;
  await scan("https://files.example/two.pdf");
  assert.equal(h.storage.session.discovery, undefined);
  h.events.suspend.listeners[0]();
  await drain();
  assert.ok(h.storage.session.discovery, "suspension must not lose what the debounce still holds");
  assert.equal(pending(), 0);
});

function popupHarness(reply, { browserLanguage = "tr", local = {} } = {}) {
  const elements = {};
  // Elements keep what was appended to them, and writing textContent empties them like the DOM does.
  const node = extra => {
    let text = "";
    return { ...extra, children: [], checked: false, disabled: false, hidden: false, listeners: {}, attributes: {},
      get textContent() { return text; }, set textContent(value) { text = String(value); this.children.length = 0; },
      addEventListener(name, listener) { this.listeners[name] = listener; }, append(...nodes) { this.children.push(...nodes); },
      getAttribute(name) { return this.attributes[name] ?? null; } };
  };
  const el = id => elements[id] ||= node({ id });
  // The real popup.html says which elements carry a static text; the double finds them there.
  const staticNodes = [...source("popup.html").matchAll(/<[a-z0-9]+\b[^>]*\bdata-i18n="([^"]+)"[^>]*>/g)].map(match => {
    const node = el(/\bid="([^"]+)"/.exec(match[0])?.[1] ?? `static:${match[1]}`);
    node.attributes["data-i18n"] = match[1];
    return node;
  });
  const storage = { local: { ...local } };
  let storageListener = null;
  const created = [];
  const document = { getElementById: el, createElement: () => node(), documentElement: { lang: "" },
    querySelectorAll: selector => selector === "[data-i18n]" ? staticNodes : [] };
  const context = vm.createContext({ document,
    browser: { i18n: i18nApi(browserLanguage),
      storage: { local: { async get(defaults) { return { ...defaults, ...storage.local }; } }, onChanged: { addListener(listener) { storageListener = listener; } } },
      runtime: { lastError: null, sendMessage(message, callback) { callback(reply(message)); } },
      tabs: { create(value) { created.push(value); } } }, window: { close() {} }, Promise, Object, Array });
  loadI18n(context);
  vm.runInContext(source("popup.js"), context, { filename: "popup.js" });
  // The service worker stores the app's language; the popup follows storage.onChanged.
  const storeLanguage = value => { storage.local.uiLanguage = value; storageListener({ uiLanguage: { newValue: value } }, "local"); };
  return { el, document, staticNodes, storeLanguage, created };
}

test("the popup keeps its state and says why a switch failed instead of drawing everything off", async () => {
  const state = { detectionEnabled: true, host: "site.example", siteHidden: false, media: [], capabilityError: null };
  let failing = false;
  const h = popupHarness(message => (message.command !== "state" && failing ? { error: "Depolama hatası" } : state));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(h.el("state").textContent, "AÇIK");
  failing = true;
  await h.el("detection").listeners.change({ target: { checked: false } });
  assert.equal(h.el("detection").checked, true, "a failed switch leaves the real state on screen");
  assert.equal(h.el("state").textContent, "AÇIK");
  assert.equal(h.el("status").textContent, "Depolama hatası");
  await h.el("hide-site").listeners.change({ target: { checked: true } });
  assert.equal(h.el("status").textContent, "Depolama hatası");
});

test("the popup states why the desktop cannot be reached", async () => {
  const message = "SSDownload çalışmıyor. Araç çubuğu penceresindeki \"Uygulamayı aç\" ile başlatın.";
  const h = popupHarness(() => ({ detectionEnabled: false, host: "", siteHidden: false, media: [], capabilityError: message }));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(h.el("status").textContent, message);
  assert.equal(h.el("state").textContent, "KAPALI");
});

test("a page with detection off stops its periodic scan", async () => {
  const intervalScans = async off => {
    const h = contentHarness({ reply: { status: "ready", usageModes: { video: true, file: true, audio: true } } });
    await new Promise(resolve => setImmediate(resolve));
    if (off) h.storageListener()({ detectionEnabled: { newValue: false } }, "local");
    vm.runInContext('lastScan=""', h.context);
    const before = h.sent.length;
    h.fireTimers();
    return h.sent.length - before;
  };
  assert.ok(await intervalScans(false) > 0, "while detection is on, the interval reports a changed page");
  assert.equal(await intervalScans(true), 0, "while it is off, nothing walks the document or messages the worker");
});

test("scroll events repaint the tiles once per frame, not once per event", async () => {
  let rectCalls = 0;
  const media = {
    srcObject: null, currentSrc: "/movie.mp4", src: "/movie.mp4", tagName: "VIDEO",
    getAttribute() { return null; }, querySelectorAll() { return []; },
    getBoundingClientRect() { rectCalls += 1; return { left: 10, top: 20, right: 650, bottom: 380, width: 640, height: 360 }; },
    checkVisibility() { return true; }
  };
  const h = contentHarness({ reply: { status: "ready", usageModes: { video: true, file: true, audio: true } }, nodes: { "video, audio": [media] } });
  await vm.runInContext("refreshUsageModes()", h.context);
  const before = rectCalls;
  for (let event = 0; event < 5; event += 1) h.window.listeners.scroll({});
  assert.equal(rectCalls, before, "no layout read happens inside the scroll event itself");
  h.fireTimers();
  assert.ok(rectCalls > before, "the deferred repaint still happens");
});

test("HLS URI ancestry groups qualities and audio under one iframe player", async () => {
  const h=backgroundHarness();
  const children=vm.runInContext('hlsChildren(\'#EXTM3U\\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="a",URI="audio.m3u8"\\n#EXT-X-STREAM-INF:BANDWIDTH=500,AUDIO="a"\\nlow.m3u8\\n#EXT-X-STREAM-INF:BANDWIDTH=900,AUDIO="a"\\nhigh.m3u8\',"https://cdn.example/master.m3u8")',h.context);
  assert.equal(children.length,3);
  vm.runInContext('manifestLinks.set("7\\n2\\nhttps://cdn.example/master.m3u8",["https://cdn.example/audio.m3u8","https://cdn.example/low.m3u8","https://cdn.example/high.m3u8"])',h.context);
  for(const name of ["master","audio","low","high"]) await h.events.headers.listeners[0]({tabId:7,frameId:2,statusCode:200,url:`https://cdn.example/${name}.m3u8`,responseHeaders:[]});
  await h.content({channel:"ssdownload-content",event:"play",documentId:"doc",pageUrl:"https://frame.example/watch",players:[{id:"p1",title:"Film adı",urls:["https://cdn.example/low.m3u8"],active:true}],sources:[]},7,2);
  const state=await h.state(7);
  assert.equal(state.videos.length,1);
  assert.equal(state.videos[0].url,"https://cdn.example/master.m3u8");
  assert.equal(state.videos[0].title,"Film adı");
  assert.equal(state.videos[0].frameId,2);
});

test("player replacement, SPA navigation and tab navigation discard stale sources", async () => {
  const h=backgroundHarness();
  const send=(id,path)=>h.content({channel:"ssdownload-content",event:"scan",documentId:"doc",pageUrl:`https://site.example/${path}`,players:[{id,title:path,urls:[`https://cdn.example/${path}.mp4`]}],sources:[{url:`https://cdn.example/${path}.mp4`,contentType:"video/mp4",pageUrl:`https://site.example/${path}`}]});
  await send("1","one"); await send("2","two");
  let state=await h.state(7);
  assert.equal(state.videos.length,1); assert.equal(state.videos[0].title,"two");
  assert.ok(!state.items.some(s=>s.url.includes("one")));
  await h.events.updated.listeners[0](7,{url:"https://site.example/new"});
  state=await h.state(7);
  assert.equal(state.videos.length,0);
});

test("multiple players stay distinct and ad classification requires multiple cues", async () => {
  const h=backgroundHarness();
  await h.content({channel:"ssdownload-content",event:"play",documentId:"doc",pageUrl:"https://site.example/watch",players:[
    {id:"main",title:"Short film",urls:["https://cdn.example/tiny.mp4"],active:true,adSignals:1},
    {id:"ad",title:"Advert",urls:["https://cdn.example/ad.mp4"],active:true,adSignals:2}
  ],sources:[{url:"https://cdn.example/tiny.mp4",contentType:"video/mp4"},{url:"https://cdn.example/ad.mp4",contentType:"video/mp4"}]});
  const state=await h.state(7);
  assert.equal(state.videos.length,1);
  assert.equal(state.videos.find(v=>v.title==="Short film").downloadable,true);
  assert.equal(state.candidates.find(v=>v.title==="Advert").suspectedAd,true);
});

test("a looping muted player without a user gesture is a preview, not a page candidate", async () => {
  const h = backgroundHarness();
  await h.content({channel:"ssdownload-content",event:"play",documentId:"doc",pageUrl:"https://site.example/",players:[
    {id:"preview",title:"Kanal tanıtımı",urls:["https://cdn.example/preview.mp4"],active:true,loop:true,muted:true,userActivated:false}
  ],sources:[]});
  const state = await h.state(7);
  assert.equal(state.videos.length,0);
  assert.equal(state.candidates.length,0);
});

test("the same player stays available after a user gesture", async () => {
  const h = backgroundHarness();
  await h.content({channel:"ssdownload-content",event:"play",documentId:"doc",pageUrl:"https://site.example/watch",players:[
    {id:"reel",title:"Kısa video",urls:["https://cdn.example/reel.mp4"],active:true,loop:true,muted:true,userActivated:true}
  ],sources:[]});
  const state = await h.state(7);
  assert.equal(state.videos.length,1);
  assert.equal(state.videos[0].kind,"page");
});

test("captured media of a preview player needs deliberate confirmation", async () => {
  const h = backgroundHarness();
  await h.content({channel:"ssdownload-content",event:"play",documentId:"doc",pageUrl:"https://site.example/",players:[
    {id:"preview",title:"Öne çıkanlar",urls:["https://cdn.example/highlight.mp4"],active:true,loop:true,muted:true,userActivated:false}
  ],sources:[{url:"https://cdn.example/highlight.mp4",contentType:"video/mp4"}]});
  const state = await h.state(7);
  assert.equal(state.videos.length,0);
  const candidate = state.candidates.find(item=>item.url==="https://cdn.example/highlight.mp4");
  assert.equal(candidate.requiresConfirmation,true);
  assert.equal(candidate.preview,true);
});

test("technical resources with attachment headers never become visible files", async () => {
  const h=backgroundHarness();
  for(const path of ["poster.jpg","app.webmanifest","segment-001.mp4","init.mp4","chunk.m4s"])
    await h.events.headers.listeners[0]({tabId:7,frameId:0,statusCode:200,url:`https://cdn.example/${path}`,responseHeaders:[{name:"content-disposition",value:`attachment; filename="${path}"`},{name:"content-type",value:"application/octet-stream"}]});
  assert.equal((await h.state(7)).items.length,0);
});

test("identical URLs in two frames remain two player records", async () => {
  const h=backgroundHarness();
  for(const frame of [0,3]) await h.content({channel:"ssdownload-content",event:"scan",documentId:`doc-${frame}`,pageUrl:`https://site.example/frame-${frame}`,
    players:[{id:"p",title:`Frame ${frame}`,urls:["https://cdn.example/same.mp4"]}],sources:[{url:"https://cdn.example/same.mp4",contentType:"video/mp4"}]},7,frame);
  const state=await h.state(7);
  assert.equal(state.items.length,2);assert.equal(state.videos.length,2);
  assert.notEqual(state.videos[0].videoId,state.videos[1].videoId);
});

test("signed URL refresh keeps one player while content ID query parameters remain distinct", async () => {
  const h=backgroundHarness();
  await h.content({channel:"ssdownload-content",event:"play",documentId:"doc",pageUrl:"https://site.example/watch",
    players:[{id:"p",title:"Film",urls:["blob:https://site.example/p"],active:true}],sources:[]});
  for(const signature of ["old","new"]) await h.events.headers.listeners[0]({tabId:7,frameId:0,statusCode:200,url:`https://cdn.example/master.m3u8?id=film&signature=${signature}`,responseHeaders:[]});
  const state=await h.state(7);
  assert.equal(state.videos.length,1);assert.ok(state.videos[0].url.endsWith("signature=new"));
  assert.equal(state.videos[0].inferred,true);
  assert.notEqual(vm.runInContext('signedResourceIdentity("https://cdn.example/video?id=1&token=a")',h.context),vm.runInContext('signedResourceIdentity("https://cdn.example/video?id=2&token=a")',h.context));
});

test("empty to blob hydration preserves an already observed manifest and pause keeps it downloadable", async () => {
  const h=backgroundHarness();
  const scan=players=>h.content({channel:"ssdownload-content",event:"scan",documentId:"doc",pageUrl:"https://site.example/watch",players,sources:[]});
  await scan([{id:"p",urls:[],title:"Film"}]);
  await h.events.headers.listeners[0]({tabId:7,frameId:0,statusCode:200,url:"https://cdn.example/master.m3u8",responseHeaders:[]});
  await scan([{id:"p",urls:["blob:https://site.example/movie"],title:"Film",active:false,visible:true}]);
  const state=await h.state(7);
  assert.equal(state.videos.length,1);
  assert.equal(state.videos[0].url,"https://cdn.example/master.m3u8");
  assert.equal(state.videos[0].downloadable,true);
});

test("consumed HLS metadata groups inaccessible master and variants without choosing a different film", async () => {
  const h=backgroundHarness();
  await h.content({channel:"ssdownload-content",event:"scan",documentId:"doc",pageUrl:"https://site.example/watch",players:[{id:"p",urls:["blob:https://site.example/movie"],title:"Film",played:true}],sources:[]});
  for(const name of ["master","low","high"]) await h.events.headers.listeners[0]({tabId:7,frameId:0,statusCode:200,url:`https://cdn.example/${name}.m3u8`,responseHeaders:[]});
  await h.content({channel:"ssdownload-content",event:"manifest",manifest:{url:"https://cdn.example/master.m3u8",children:["https://cdn.example/low.m3u8","https://cdn.example/high.m3u8"]},sources:[]});
  let state=await h.state(7);
  assert.equal(state.videos[0].url,"https://cdn.example/master.m3u8");
  assert.equal(state.videos[0].downloadable,true);
  await h.events.headers.listeners[0]({tabId:7,frameId:0,statusCode:200,url:"https://cdn.example/another-film.m3u8",responseHeaders:[]});
  state=await h.state(7);
  assert.equal(state.videos.length,0);
  assert.equal(state.candidates.length,2);
  assert.ok(state.candidates.every(candidate=>candidate.requiresConfirmation));
});

test("duplicate media elements share a record but empty hidden placeholders do not become videos", async () => {
  const h=backgroundHarness();
  await h.content({channel:"ssdownload-content",event:"scan",documentId:"doc",pageUrl:"https://site.example/watch",players:[
    {id:"empty",urls:[],title:"Film",visible:false},
    {id:"a",urls:["https://cdn.example/film.mp4"],title:"Film"},
    {id:"b",urls:["https://cdn.example/film.mp4"],title:"Film"}
  ],sources:[{url:"https://cdn.example/film.mp4",contentType:"video/mp4"}]});
  const state=await h.state(7);
  assert.equal(state.videos.length,1);
  assert.deepEqual([...state.videos[0].aliases],["0:doc:b"]);
});

test("about blank sender uses inherited HTTP document context", async () => {
  const h=backgroundHarness();
  await vm.runInContext('handleContentMessage({channel:"ssdownload-content",event:"scan",documentId:"blank",pageUrl:"https://site.example/watch",players:[{id:"p",urls:["https://cdn.example/film.mp4"],title:"Frame player"}],sources:[{url:"https://cdn.example/film.mp4",contentType:"video/mp4"}]},{id:"test-extension",tab:{id:7,title:"Real film name"},frameId:2,url:"about:blank"})',h.context);
  const state=await h.state(7);
  assert.equal(state.videos.length,1);assert.equal(state.videos[0].title,"Real film name");
  assert.equal(state.videos[0].downloadable,true);
});

test("new iframe document keeps its early network request and removed frames are pruned", async () => {
  const h=backgroundHarness();
  const send=documentId=>h.content({channel:"ssdownload-content",event:"scan",documentId,pageUrl:"https://frame.example/watch",players:[{id:"p",urls:[`blob:https://frame.example/${documentId}`],title:"Film"}],sources:[]},7,2);
  await send("old");
  await h.events.headers.listeners[0]({tabId:7,frameId:2,documentId:"new",statusCode:200,url:"https://cdn.example/new.m3u8",responseHeaders:[]});
  await send("new");
  let state=await h.state(7);
  assert.equal(state.videos[0].url,"https://cdn.example/new.m3u8");
  assert.equal(state.videos[0].downloadable,true);
  vm.runInContext('browser.webNavigation={async getAllFrames(){return [{frameId:0,documentId:"top"}]}}',h.context);
  state=await h.state(7);
  assert.equal(state.videos.length,0);assert.equal(state.items.length,0);
});

function contentHarness(options = {}) {
  const sent = [];
  const listeners = {};
  const documentEvents = {};
  const buttons = [];
  const timers = [];
  const storageWrites = [];
  let storageListener = null;
  // A double keeps its listeners in registration order with the phase each asked for: the
  // shield's whole claim is where in the capture path a handler runs, so a dispatch that
  // ignored the phase could not show that the page's own handler was kept out.
  const listen = (store, name, listener, capture) => {
    if (!store[name]) store[name] = [];
    store[name].push({ listener, capture: capture === true || capture?.capture === true });
  };
  const sourceNode = { src: "https://cdn.example/track.m4a", type: "audio/mp4", closest(){return media;} };
  const media = {
    srcObject: null, currentSrc: "/movie.mp4", src: "/movie.mp4", tagName: "VIDEO",
    getAttribute() { return null; }, querySelectorAll(selector) { return selector === "source[src]" ? [sourceNode] : []; },
    getBoundingClientRect() { return { left: 10, top: 20, right: 650, bottom: 380, width: 640, height: 360 }; },
    checkVisibility() { return true; }
  };
  const anchor = { href: "https://files.example/report.pdf", download: "report.pdf" };
  // The double supplies the element factory Chrome gives every document, including the
  // namespaced SVG one the affordance builds its glyph from. `textContent` follows the
  // DOM: reading returns the descendants' text, writing replaces the children.
  const createNode = (tag) => {
    const children = [];
    let text = "";
    const node = {
      tagName: String(tag).toUpperCase(), style: {}, dataset: {}, hidden: false, title: "", listeners: {}, events: {}, parentNode: null, attributes: {},
      // A tile is laid out before it is placed: the double owes the button the size the
      // live overlay carries (147x36 as measured on the embedded player), so a placement
      // pass can weigh the spot its own footprint would cover.
      offsetWidth: 147, offsetHeight: 36, childNodes: children,
      getBoundingClientRect() {
        const left = parseFloat(this.style.left) || 0, top = parseFloat(this.style.top) || 0;
        return { left, top, right: left + this.offsetWidth, bottom: top + this.offsetHeight, width: this.offsetWidth, height: this.offsetHeight };
      },
      setAttribute(name, value) { this.attributes[name] = String(value); },
      getAttribute(name) { return this.attributes[name] ?? null; },
      appendChild(child) { children.push(child); child.parentNode = this; return child; },
      addEventListener(name, listener, capture) { this.listeners[name] = listener; listen(this.events, name, listener, capture); },
      remove() { const index = buttons.indexOf(this); if (index >= 0) buttons.splice(index, 1); this.parentNode = null; }
    };
    Object.defineProperty(node, "textContent", {
      get() { return children.length ? children.map(child => child.textContent).join("") : text; },
      set(value) { children.length = 0; text = String(value); }
    });
    return node;
  };
  const document = {
    baseURI: "https://site.example/watch", documentElement: { appendChild(node) { buttons.push(node); node.parentNode = this; } },
    events: documentEvents,
    createElement(tag) { return createNode(tag); },
    createElementNS(namespace, tag) { return createNode(tag); },
    querySelectorAll(selector) {
      if (options.nodes?.[selector]) return options.nodes[selector];
      if (selector === "video, audio") return [media];
      if (selector === "source[src]") return [sourceNode];
      if (selector.includes("a[download]")) return [anchor];
      return [];
    },
    addEventListener(name, listener, capture) { listeners[name] = listener; listen(documentEvents, name, listener, capture); }
  };
  // The page's own window, where the shield registers first: a script of the document that
  // registers afterwards stands exactly where a hostile capture handler stands.
  const window = {
    listeners: {}, events: {},
    addEventListener(name, listener, capture) { this.listeners[name] = listener; listen(this.events, name, listener, capture); }
  };
  // A gesture as the browser delivers it: the capture path walks from the window down to the
  // target and stops where a listener stops it. `isTrusted` marks the browser's own input, so
  // a page can dispatch neither the phases nor the trust of a real press.
  const gesture = (target, type = "click", init = {}) => {
    const path = [window, document];
    const ancestry = [];
    for (let node = target; node && node !== document; node = node.parentNode) ancestry.unshift(node);
    path.push(...ancestry);
    const event = {
      type, target, key: init.key, button: init.button ?? 0, isTrusted: init.isTrusted !== false,
      clientX: init.clientX ?? 0, clientY: init.clientY ?? 0, pointerId: init.pointerId ?? 1,
      defaultPrevented: false, propagationStopped: false, immediateStopped: false,
      preventDefault() { event.defaultPrevented = true; },
      stopPropagation() { event.propagationStopped = true; },
      stopImmediatePropagation() { event.propagationStopped = true; event.immediateStopped = true; }
    };
    const deliver = (node, capture) => {
      for (const entry of node.events?.[type] || []) {
        if (entry.capture !== capture) continue;
        entry.listener.call(node, event);
        if (event.immediateStopped) return false;
      }
      return !event.propagationStopped;
    };
    for (const node of path) if (!deliver(node, true)) return event;
    if (!deliver(target, false)) return event;
    for (let index = path.length - 2; index >= 0; index--) if (!deliver(path[index], false)) return event;
    return event;
  };
  class Observer { constructor(callback) { this.callback = callback; } observe() {} }
  const runtimeMessages = event();
  const context = vm.createContext({
    browser: {
      i18n: i18nApi(options.browserLanguage),
      runtime: { sendMessage(value) { sent.push(value); const reply = options.reply; return Promise.resolve(typeof reply === "function" ? reply(value) : reply); }, onMessage: runtimeMessages },
      // The worker stores the desktop's language next to the switch; `storedLanguage` is what it stored.
      storage: { local: { get() { return Promise.resolve({ detectionEnabled: true, ...(options.storedLanguage ? { uiLanguage: options.storedLanguage } : {}) }); }, set(value) { storageWrites.push(value); return Promise.resolve(); } }, onChanged: { addListener(listener) { storageListener = listener; } } }
    },
    URL, document, window, location: { href: "https://site.example/watch", origin: "https://site.example", hostname: "site.example" }, innerHeight: 800, innerWidth: 1280,
    MutationObserver: Observer, PerformanceObserver: Observer,
    setTimeout(callback, delay) { timers.push({ callback, delay }); return timers.length; }, clearTimeout() {},
    setInterval(callback, delay) { timers.push({ callback, delay }); return timers.length; },
    JSON, String, Map, Array, Object
  });
  loadI18n(context);
  vm.runInContext(source("content.js"), context, { filename: "content.js" });
  return { context, sent, listeners, runtimeMessages, buttons, document, window, gesture, storageWrites, storageListener: () => storageListener,
    fireTimers() { for (const timer of timers.splice(0)) timer.callback(); } };
}

test("the no-single-media command clears the in-page affordance", async () => {
  const h = contentHarness({ reply: { status: "ready", usageModes: { video: true, file: true, audio: true } } });
  await vm.runInContext("refreshUsageModes()", h.context);
  assert.equal(h.buttons.length, 1, "the ready state must publish one affordance");
  h.runtimeMessages.listeners[0]({ channel: "ssdownload-background", command: "no-single-media" });
  assert.equal(h.buttons.length, 0, "a rejected page must remove its affordance");
});

// The chevron at the tile's end is the tile's own menu: opening it hands nothing over, and its
// entries withdraw the tile for this video, this page or this site.
async function tileMenuHarness() {
  const h = contentHarness({ reply: { status: "ready", usageModes: { video: true, file: true, audio: true } } });
  await vm.runInContext("refreshUsageModes()", h.context);
  const [tile] = h.buttons;
  const trigger = tile.childNodes.find(node => node.attributes.role === "button");
  const open = () => { h.gesture(trigger); return h.buttons.find(node => node.attributes.role === "menu"); };
  const handoffs = () => h.sent.filter(message => message.event === "download-video" || message.event === "download-page");
  return { h, tile, trigger, open, handoffs };
}

test("the tile's chevron opens the hide choices without downloading", async () => {
  const { h, tile, trigger, open, handoffs } = await tileMenuHarness();
  assert.ok(trigger, "the tile carries its menu trigger");
  assert.equal(tile.textContent, "Bu videoyu indir", "the trigger adds no text to the tile's label");
  const menu = open();
  assert.ok(menu, "the trigger opens the menu");
  assert.deepEqual(menu.childNodes.map(item => item.textContent), ["Bu videoda gizle", "Bu sayfada gizle", "Bu sitede gizle"]);
  assert.equal(trigger.attributes["aria-expanded"], "true");
  // The tile stands at 28 px (player top 20 + 8) and is 36 px tall; the menu keeps a 4 px gap.
  assert.equal(menu.style.top, "68px", "the menu opens under the tile");
  assert.deepEqual(handoffs(), [], "opening the menu must not hand anything over");
  h.gesture(trigger);
  assert.equal(h.buttons.includes(menu), false, "a second press closes the menu");
  assert.equal(trigger.attributes["aria-expanded"], "false");
});

test("hiding on this video withdraws the tile and hands nothing over", async () => {
  const { h, tile, open, handoffs } = await tileMenuHarness();
  const menu = open();
  h.gesture(menu.childNodes[0]);
  assert.equal(h.buttons.includes(menu), false, "choosing an entry closes the menu");
  assert.equal(tile.hidden, true, "the tile is withdrawn on this video");
  await vm.runInContext("updateOverlays()", h.context);
  assert.equal(tile.hidden, true, "a later repaint keeps it withdrawn");
  assert.deepEqual(handoffs(), []);
  assert.deepEqual(h.storageWrites, [], "a video choice stores nothing");
});

test("hiding on this site stores the host next to the popup's list", async () => {
  const { h, tile, open } = await tileMenuHarness();
  h.gesture(open().childNodes[2]);
  await new Promise(resolve => setImmediate(resolve));
  // The write is made in the content script's realm, so it is compared as data.
  assert.deepEqual(JSON.parse(JSON.stringify(h.storageWrites)), [{ hiddenHosts: ["site.example"] }]);
  assert.equal(tile.hidden, true, "the tile goes at once, before the stored list comes back");
});

test("a press elsewhere or Escape closes the tile menu, and Enter on the trigger opens it", async () => {
  const { h, trigger, open } = await tileMenuHarness();
  const elsewhere = h.document.createElement("div");
  let menu = open();
  h.gesture(elsewhere, "pointerdown");
  assert.equal(h.buttons.includes(menu), false, "a press on the page closes the menu");
  menu = open();
  h.gesture(elsewhere, "keydown", { key: "Escape" });
  assert.equal(h.buttons.includes(menu), false, "Escape closes the menu");
  h.gesture(trigger, "keydown", { key: "Enter" });
  assert.ok(h.buttons.find(node => node.attributes.role === "menu"), "the keyboard opens the menu too");
});

test("a player scrolled out of view sideways keeps no tile at the viewport edge", async () => {
  const player = (left, right) => ({
    srcObject: null, currentSrc: "/movie.mp4", src: "/movie.mp4", tagName: "VIDEO",
    getAttribute() { return null; }, querySelectorAll() { return []; },
    getBoundingClientRect() { return { left, top: 20, right, bottom: 380, width: 640, height: 360 }; },
    checkVisibility() { return true; }
  });
  const tileOf = async media => {
    const h = contentHarness({ reply: { status: "ready", usageModes: { video: true, file: true, audio: true } }, nodes: { "video, audio": [media] } });
    await vm.runInContext("refreshUsageModes()", h.context);
    return h.buttons.find(button => button.textContent === "Bu videoyu indir");
  };
  assert.equal((await tileOf(player(10, 650))).hidden, false, "a player in view keeps its tile");
  // The tile's left edge is clamped to the viewport, so without this it would sit at x=0 for a
  // player that a carousel moved entirely off the left side.
  assert.equal((await tileOf(player(-700, -60))).hidden, true, "a player left of the viewport has no tile");
  assert.equal((await tileOf(player(1400, 2040))).hidden, true, "a player right of the viewport has no tile");
});

test("a dragged placement keeps the tile on the part of a tall player that is on screen", async () => {
  const tall = {
    srcObject: null, currentSrc: "/movie.mp4", src: "/movie.mp4", tagName: "VIDEO",
    getAttribute() { return null; }, querySelectorAll() { return []; },
    getBoundingClientRect() { return { left: 10, top: 60, right: 1110, bottom: 1260, width: 1100, height: 1200 }; },
    checkVisibility() { return true; }
  };
  const h = contentHarness({ reply: { status: "ready", usageModes: { video: true, file: true, audio: true } }, nodes: { "video, audio": [tall] } });
  await vm.runInContext("refreshUsageModes()", h.context);
  const tile = h.buttons.find(button => button.textContent === "Bu videoyu indir");
  // Dropped on a player's bottom edge in another tab; this window is 800 px high and the tile 36 px.
  h.storageListener()({ tilePlacement: { newValue: { x: 0, y: 1 } } }, "local");
  assert.deepEqual([parseFloat(tile.style.left), parseFloat(tile.style.top)], [10, 764]);
});

test("media under visibility: hidden is reported as concealed and gets no tile", async () => {
  const preview = {
    srcObject: null, currentSrc: "/movie.mp4", src: "/movie.mp4", tagName: "VIDEO",
    getAttribute() { return null; }, querySelectorAll() { return []; },
    getBoundingClientRect() { return { left: 10, top: 20, right: 650, bottom: 380, width: 640, height: 360 }; },
    checkVisibility(options) { return options?.visibilityProperty !== true; }
  };
  const h = contentHarness({ reply: { status: "ready", usageModes: { video: true, file: true, audio: true } }, nodes: { "video, audio": [preview] } });
  await vm.runInContext("refreshUsageModes()", h.context);
  assert.equal(JSON.parse(vm.runInContext("JSON.stringify(players())", h.context))[0].concealed, true);
  assert.equal(h.buttons.find(button => button.textContent === "Bu videoyu indir")?.hidden ?? true, true);
});

test("a document without a media element offers its own page and drops the offer with the page", async () => {
  // The double has no media element at all: only the embed address an iframe names.
  const h = contentHarness({ reply: { status: "ready", usageModes: { video: true, file: true, audio: true }, siteEntries: true },
    nodes: { "video, audio": [], "iframe": [{ src: "https://player.example/embed/one" }] } });
  await vm.runInContext("refreshUsageModes()", h.context);
  const [tile] = h.buttons;
  assert.equal(tile?.textContent, "Sayfayı SSDownload ile indir", "the page candidate is offered with the product's own wording");
  // The tile submits from the shield: the page's gesture is the click the browser delivers,
  // walked from the window down to the tile.
  h.gesture(tile);
  const handoff = h.sent.at(-1);
  assert.equal(handoff.event, "download-page", "the page tile must hand the document over");
  assert.equal(handoff.pageUrl, "https://site.example/watch", "the page's own address is the offered source");
  // A rejected page address keeps no offer, and the tile's own inline display goes with it.
  h.runtimeMessages.listeners[0]({ channel: "ssdownload-background", command: "no-single-media" });
  assert.equal(tile.hidden, true, "a rejected page must not keep an affordance");
  assert.equal(tile.style.display, "none", "the withdrawal must restate the tile's own display");
});

test("the page tile is drawn only while the desktop offers site entries", async () => {
  const ready = (extra = {}) => ({ status: "ready", usageModes: { video: true, file: true, audio: true }, ...extra });
  // A document with no media element of its own: the player markup is all it reports, which
  // is exactly the document the page tile exists for.
  const signals = { nodes: { "video, audio": [], "iframe": [{ src: "https://player.example/embed/one" }] } };
  const drawn = async extra => {
    const h = contentHarness({ reply: ready(extra), ...signals });
    await vm.runInContext("refreshUsageModes()", h.context);
    return h;
  };
  const labels = h => h.buttons.map(button => button.textContent);

  assert.deepEqual(labels(await drawn({ siteEntries: false })), [], "a desktop that does not offer site entries draws no page tile");
  assert.deepEqual(labels(await drawn()), [], "a missing flag means off");

  const h = await drawn({ siteEntries: true });
  assert.deepEqual(labels(h), ["Sayfayı SSDownload ile indir"], "the offered page tile keeps the product's own wording");
  // The refresh that follows a capability change reaches the open page: the tile is
  // withdrawn and restored without a reload.
  h.runtimeMessages.listeners[0]({ channel: "ssdownload-background", command: "usage-modes", ...ready({ siteEntries: false }) });
  assert.equal(h.buttons[0].hidden, true, "the tile must be withdrawn when the desktop stops offering site entries");
  h.runtimeMessages.listeners[0]({ channel: "ssdownload-background", command: "usage-modes", ...ready({ siteEntries: true }) });
  assert.equal(h.buttons[0].hidden, false, "and restored when the desktop offers them again");
});

test("one page offer per tab: a frame document keeps its media tile only", async () => {
  const ready = { status: "ready", usageModes: { video: true, file: true, audio: true }, siteEntries: true };
  const frames = [{ src: "https://player.example/embed/one" }, { src: "https://player.example/embed/two" }];
  const nested = { "video, audio": [], "iframe": frames, "iframe, frame, embed, object": frames };

  // The tab's own document: player markup, no media element, nested player frames.
  const top = contentHarness({ reply: ready, nodes: nested });
  await vm.runInContext("refreshUsageModes()", top.context);
  const topPages = top.buttons.filter(button => button.textContent === "Sayfayı SSDownload ile indir");
  assert.equal(topPages.length, 1, "a page with nested player frames keeps exactly one page offer");

  // The same document standing in a frame: it is not the tab the page offer belongs to, and
  // the manifest injects this script into every frame, so this is where the second offer came from.
  const frame = contentHarness({ reply: ready, nodes: nested });
  frame.window.self = frame.window;
  frame.window.top = { self: {} };
  await vm.runInContext("refreshUsageModes()", frame.context);
  assert.equal(frame.buttons.length, 0, "a frame document draws no page offer of its own");

  // A frame that renders a media element of its own keeps that player's affordance.
  const player = contentHarness({ reply: ready });
  player.window.self = player.window;
  player.window.top = { self: {} };
  await vm.runInContext("refreshUsageModes()", player.context);
  assert.deepEqual(player.buttons.map(button => button.textContent), ["Bu videoyu indir"], "the in-player affordance still stands in the frame");
});

test("the page markup capture is bounded, leaves our own controls out, and answers nothing it cannot read", () => {
  const h = contentHarness();
  const ask = () => {
    const answers = [];
    h.runtimeMessages.listeners[0]({ channel: "ssdownload-background", command: "page-html" }, {}, value => answers.push(value));
    return JSON.stringify(answers.at(-1));
  };
  // The double's document element cannot be copied, and a page the extension cannot read
  // reports nothing rather than an empty page.
  assert.equal(ask(), "{}", "a document that cannot serialize itself answers with nothing");
  const removed = [];
  h.document.documentElement.cloneNode = () => ({
    querySelectorAll: selector => selector.includes("[data-ssdownload-tile]") ? [{ remove() { removed.push("tile"); } }] : [],
    outerHTML: "<html><body>sayfa</body></html>"
  });
  vm.runInContext('location.href="about:blank"', h.context);
  assert.equal(ask(), "{}", "about:blank has no page markup to report");
  vm.runInContext('globalThis.performance={getEntriesByType:()=>[{responseStatus:503}]}', h.context);
  vm.runInContext('location.href="https://site.example/watch"', h.context);
  assert.equal(ask(), "{}", "a document that answered with an error has no page markup to report");
  vm.runInContext('globalThis.performance={getEntriesByType:()=>[]}', h.context);
  assert.equal(ask(), JSON.stringify({ html: "<html><body>sayfa</body></html>" }), "the document's own markup is the report");
  assert.deepEqual(removed, ["tile"], "the extension's own affordances are never part of the page's markup");
  // An oversized page is cut at the bound, between two characters, with the marker stating it.
  h.document.documentElement.cloneNode = () => ({ querySelectorAll: () => [], outerHTML: "ş".repeat(200000) });
  const truncated = JSON.parse(ask()).html;
  assert.equal(typeof truncated, "string");
  assert.equal(truncated.endsWith("<!-- ssdownload:page-html-truncated -->"), true, "a cut capture states the cut");
  assert.ok(Buffer.byteLength(truncated, "utf8") <= 192 * 1024, "the capture never exceeds the bound");
  assert.equal(/[\uD800-\uDFFF]/.test(truncated), false, "the cut lands between two characters");
});

test("a page offer keeps off the embedded player's own corner", async () => {
  const ready = { status: "ready", usageModes: { video: true, file: true, audio: true }, siteEntries: true };
  // The wrapper the container selector matches starts on the embed's own top-left corner,
  // as the live reproduction measured the pair (iframe 690x388 at 152.5,339): the anchored
  // tile would land on the corner the embedded document's own content script stands its
  // tile on, and this document's tile is painted above the frame it covers.
  const onPlayer = { left: 152.5, top: 339, right: 842.5, bottom: 727, width: 690, height: 388 };
  const embed = rect => ({ src: "https://player.example/embed/one", title: "", getBoundingClientRect: () => rect, checkVisibility: () => true });
  const wrapper = rect => ({ tagName: "DIV", getBoundingClientRect: () => rect, checkVisibility: () => true });
  const tileFor = async fixture => {
    const h = contentHarness({ reply: ready, nodes: { "video, audio": [], "iframe": [fixture.embed], "iframe, frame, embed, object": [fixture.embed], '[class*="player" i]': [fixture.wrapper] } });
    await vm.runInContext("refreshUsageModes()", h.context);
    return h.buttons[0];
  };
  const covered = await tileFor({ embed: embed(onPlayer), wrapper: wrapper(onPlayer) });
  assert.equal(covered?.textContent, "Sayfayı SSDownload ile indir", "the page candidate is still offered");
  assert.equal(covered.hidden, false, "a covered corner moves the offer, it never withdraws it");
  assert.equal(covered.style.left, "auto", "the offer must not anchor onto the embedded player");
  assert.equal(covered.style.right, "8px", "the offer stands on the viewport corner instead");
  assert.equal(covered.style.top, "8px", "the corner placement keeps its own margin");
  // A container of this document's own keeps the anchored offer, frame or not.
  const own = await tileFor({ embed: embed({ left: 900, top: 60, right: 1200, bottom: 400, width: 300, height: 340 }),
    wrapper: wrapper({ left: 40, top: 200, right: 730, bottom: 588, width: 690, height: 388 }) });
  assert.equal(own.style.right, "auto", "a container this document renders keeps the tile on itself");
  assert.equal(own.style.left, "48px", "the tile stands on the container's corner");
  assert.equal(own.style.top, "208px", "the tile stands on the container's corner");
});

test("a rejected in-page action is stated on the page's own status surface and expires", async () => {
  const message = "Bu oynatıcının medya isteği henüz yakalanamadı";
  const h = contentHarness({ reply: { status: "ready", usageModes: { video: true, file: true, audio: true }, error: message } });
  await vm.runInContext("refreshUsageModes()", h.context);
  const [button] = h.buttons;
  assert.ok(button, "the ready state must publish one affordance");
  h.gesture(button);
  // The refusal is the reply to the submission the gesture made, so it is stated once the
  // promise the tile holds has settled.
  await new Promise(resolve => setTimeout(resolve, 0));
  const status = h.buttons.find(node => node.dataset?.ssdownloadStatus !== undefined);
  assert.equal(status?.textContent, message, "the failure must be stated on the page, not on the action label");
  assert.equal(button.textContent, "Bu videoyu indir", "the affordance must keep the action label it acts on");
  h.fireTimers();
  assert.equal(h.buttons.includes(status), false, "the message must clean itself up when its window ends");
});

test("a page's own capture handler cannot take the tile's gesture", async () => {
  const h = contentHarness({ reply: { status: "ready", usageModes: { video: true, file: true, audio: true } } });
  await vm.runInContext("refreshUsageModes()", h.context);
  const [tile] = h.buttons;
  assert.ok(tile, "the ready state must publish one affordance");
  // A script of the document registers after the content script, exactly where a player's
  // own pop-under handler stands: on the window and on the document, in the capture phase,
  // and it consumes whatever it is handed.
  const pageSaw = [];
  const popunders = [];
  const consume = event => { pageSaw.push(event.type); popunders.push("pop-under"); event.stopImmediatePropagation(); };
  h.window.addEventListener("click", consume, true);
  h.window.addEventListener("pointerdown", consume, true);
  h.document.addEventListener("click", consume, true);
  h.document.addEventListener("click", consume);
  for (const type of ["pointerdown", "mousedown", "pointerup", "mouseup", "click"]) {
    const event = h.gesture(tile, type);
    assert.equal(event.defaultPrevented, false, `${type} must leave the browser's own default action alone`);
  }
  assert.deepEqual(pageSaw, [], "the page's handlers must never see the tile's gesture");
  assert.deepEqual(popunders, [], "no pop-under can be opened out of the tile's click");
  const handoff = h.sent.at(-1);
  assert.equal(handoff.event, "download-video", "the tile still hands its candidate over");
  const scan = h.sent.find(message => message.event === "scan");
  assert.equal(handoff.playerId, scan.players[0].id, "the submission is the player this tile stands on");
});

test("a tile dragged off the player's corner stays where it was dropped on every player", async () => {
  const h = contentHarness({ reply: { status: "ready", usageModes: { video: true, file: true, audio: true } } });
  await vm.runInContext("refreshUsageModes()", h.context);
  const [tile] = h.buttons;
  const at = () => [parseFloat(tile.style.left), parseFloat(tile.style.top)];
  const downloads = () => h.sent.filter(message => message.event === "download-video").length;
  // The player spans 10..650 x 20..380; the tile (147x36) starts 8 px inside its corner.
  assert.deepEqual(at(), [18, 28]);

  h.gesture(tile, "pointerdown", { clientX: 20, clientY: 30 });
  h.gesture(tile, "pointermove", { clientX: 320, clientY: 230 });
  assert.deepEqual(at(), [318, 228], "the tile follows the pointer");
  // A second finger, or input a page script dispatches, neither moves nor drops the tile.
  const before = h.storageWrites.length;
  h.gesture(tile, "pointermove", { clientX: 600, clientY: 300, pointerId: 2 });
  h.gesture(tile, "pointerup", { clientX: 600, clientY: 300, pointerId: 2 });
  h.gesture(tile, "pointermove", { clientX: 600, clientY: 300, isTrusted: false });
  h.gesture(tile, "pointerup", { clientX: 600, clientY: 300, isTrusted: false });
  assert.deepEqual(at(), [318, 228]);
  assert.equal(h.storageWrites.length, before);
  h.gesture(tile, "pointerup", { clientX: 320, clientY: 230 });
  h.gesture(tile, "click", { clientX: 320, clientY: 230 });
  assert.equal(downloads(), 0, "releasing a drag does not start a download");
  const saved = h.storageWrites.at(-1).tilePlacement;
  assert.ok(Math.abs(saved.x - 308 / 493) < 1e-9 && Math.abs(saved.y - 208 / 324) < 1e-9, JSON.stringify(saved));
  vm.runInContext("updateOverlays()", h.context);
  assert.deepEqual(at(), [318, 228], "a later layout pass keeps the dropped spot");

  // A press that moves less than 5 px is still a click.
  h.gesture(tile, "pointerdown", { clientX: 320, clientY: 230 });
  h.gesture(tile, "pointermove", { clientX: 323, clientY: 232 });
  h.gesture(tile, "pointerup", { clientX: 323, clientY: 232 });
  h.gesture(tile, "click", { clientX: 323, clientY: 232 });
  assert.equal(downloads(), 1);
  assert.deepEqual(at(), [318, 228]);

  // The placement is shared through storage: another tab's drop to the far corner moves this tile.
  h.storageListener()({ tilePlacement: { newValue: { x: 1, y: 1 } } }, "local");
  assert.deepEqual(at(), [503, 344]);

  // A drag the browser cancels (a touch taken for scrolling) saves nothing and puts the tile back.
  const writes = h.storageWrites.length;
  h.gesture(tile, "pointerdown", { clientX: 510, clientY: 350 });
  h.gesture(tile, "pointermove", { clientX: 410, clientY: 250 });
  h.gesture(tile, "pointercancel", { clientX: 410, clientY: 250 });
  assert.equal(h.storageWrites.length, writes);
  assert.deepEqual(at(), [503, 344]);
  h.gesture(tile, "pointerdown", { clientX: 510, clientY: 350 });
  h.gesture(tile, "pointerup", { clientX: 510, clientY: 350 });
  h.gesture(tile, "click", { clientX: 510, clientY: 350 });
  assert.equal(downloads(), 2, "the next press still activates the tile");
});

test("the shield leaves the page's own controls and phases alone", async () => {
  const h = contentHarness({ reply: { status: "ready", usageModes: { video: true, file: true, audio: true } } });
  await vm.runInContext("refreshUsageModes()", h.context);
  const [tile] = h.buttons;
  const control = h.document.createElement("button");
  h.document.documentElement.appendChild(control);
  const pageSaw = [];
  const controlSaw = [];
  h.window.addEventListener("click", event => pageSaw.push(`window:${event.target === control}`), true);
  h.document.addEventListener("click", () => pageSaw.push("document"), true);
  control.addEventListener("pointerdown", () => controlSaw.push("pointerdown"));
  control.addEventListener("click", () => controlSaw.push("click"));
  h.gesture(control, "pointerdown");
  h.gesture(control, "click");
  assert.deepEqual(controlSaw, ["pointerdown", "click"], "a page control keeps its own phases");
  assert.deepEqual(pageSaw, ["window:true", "document"], "the page keeps its own capture handlers");
  assert.equal(h.sent.some(message => message.event === "download-video"), false, "another node's click is not the tile's");
  // Only the keys a tile acts on are the tile's; every other key stays the page's event.
  const pageKeys = [];
  h.window.addEventListener("keydown", () => pageKeys.push("window"), true);
  h.gesture(tile, "keydown", { key: "a" });
  h.gesture(tile, "keydown", { key: "Enter" });
  assert.deepEqual(pageKeys, ["window"], "typing over the tile still reaches the page");
});

test("one tile gesture submits once and still paints the tile's own states", async () => {
  const h = contentHarness({ reply: { status: "ready", usageModes: { video: true, file: true, audio: true } } });
  await vm.runInContext("refreshUsageModes()", h.context);
  const [tile] = h.buttons;
  const submissions = () => h.sent.filter(message => message.event === "download-video").length;
  const face = () => tile.style.background;
  h.gesture(tile, "pointerenter");
  h.gesture(tile, "pointerleave");
  const idle = face();
  h.gesture(tile, "pointerenter");
  const hover = face();
  assert.notEqual(hover, idle, "the pointer must still paint the tile on entry");
  h.gesture(tile, "pointerdown");
  const pressed = face();
  assert.notEqual(pressed, hover, "the press must still paint the pressed face");
  h.gesture(tile, "pointerup");
  assert.equal(face(), hover, "the release must keep the tile in its hover face");
  h.gesture(tile, "pointerleave");
  assert.equal(face(), idle, "leaving the tile must drop it back to its idle face");
  h.gesture(tile, "keydown", { key: " " });
  assert.notEqual(face(), idle, "the keyboard must still press the tile");
  h.gesture(tile, "keyup", { key: " " });
  assert.equal(face(), idle, "releasing the key must drop the pressed face");
  // The browser turns the gesture into one click; the tile submits from it, never beside it.
  h.gesture(tile, "click");
  assert.equal(submissions(), 1, "one gesture must submit exactly once");
  h.gesture(tile, "click", { isTrusted: false });
  assert.equal(submissions(), 1, "a page-forged click is never the user's gesture");
});

test("a withdrawn or disabled tile submits nothing", async () => {
  const h = contentHarness({ reply: { status: "ready", usageModes: { video: true, file: true, audio: true } } });
  await vm.runInContext("refreshUsageModes()", h.context);
  const [tile] = h.buttons;
  const submissions = () => h.sent.filter(message => message.event === "download-video").length;
  h.gesture(tile);
  assert.equal(submissions(), 1, "a visible tile is the offer");
  // The toolbar switch withdraws the affordance without removing it from the page.
  h.storageListener()({ detectionEnabled: { newValue: false } }, "local");
  assert.equal(tile.hidden, true, "the switch must withdraw the affordance");
  h.gesture(tile);
  assert.equal(submissions(), 1, "a withdrawn tile is not an offer");
  h.storageListener()({ detectionEnabled: { newValue: true } }, "local");
  assert.equal(tile.hidden, false, "the switch must restore the affordance");
  tile.disabled = true;
  h.gesture(tile);
  assert.equal(submissions(), 1, "a disabled tile is not an offer");
});

test("switching discovery back on reports the page again", async () => {
  const h = contentHarness();
  await new Promise(resolve => setTimeout(resolve, 0));
  const before = h.sent.length;
  h.storageListener()({ detectionEnabled: { newValue: false } }, "local");
  assert.equal(h.sent.length, before, "disabling must not add a report");
  h.storageListener()({ detectionEnabled: { newValue: true } }, "local");
  assert.equal(h.sent.at(-1)?.event, "scan", "a fresh scan must make the page's sources available again");
});

test("page collector normalizes media, reports download links, and avoids duplicate scans", () => {
  const h = contentHarness();
  const collected = vm.runInContext("collectSources()", h.context);
  assert.deepEqual(collected.map(item => item.url).sort(), ["https://cdn.example/track.m4a", "https://files.example/report.pdf", "https://site.example/movie.mp4"]);
  assert.equal(collected.find(item => item.kind === "file").filename, "report.pdf");
  const afterInitial = h.sent.length;
  vm.runInContext('report("scan")', h.context);
  assert.equal(h.sent.length, afterInitial);
  h.listeners.play();
  assert.equal(h.sent.at(-1).event, "play");
});

// The page's scan carries the markup facts a player-less document still exposes; the
// double supplies exactly the nodes the collector asks for.
function scanSignals(nodes) {
  const h = contentHarness(nodes ? { nodes } : {});
  const scan = h.sent.filter(message => message.event === "scan").at(-1);
  return JSON.parse(JSON.stringify(scan.playerSignals));
}

test("a page scan reports player markup and keeps only resolvable embed addresses", () => {
  const signals = scanSignals({
    "iframe": [
      { src: "https://player.example/embed/one", title: "Bölüm 1" },
      { src: "https://player.example/embed/one", title: "yinelenen" },
      { title: "", getAttribute: name => ({ "data-lazy-src": "/embed/two" })[name] ?? null },
      { src: "about:blank", title: "boş yer tutucu" },
      { src: "blob:https://site.example/main", title: "blob" },
      { src: "https://ads.example/frame.html", title: "reklam" }
    ],
    "script[src]": [{ src: "https://cdn.example/js/hls.min.js" }],
    '[class*="video" i]': [{ tagName: "DIV" }]
  });
  assert.deepEqual(signals, {
    embeds: [
      { url: "https://player.example/embed/one", title: "Bölüm 1" },
      { url: "https://site.example/embed/two", title: "" }
    ],
    containers: true,
    playerScripts: true
  });
});

test("embed signals stay bounded to four addresses with a clipped title", () => {
  const signals = scanSignals({
    "iframe": Array.from({ length: 6 }, (_, index) => ({
      src: `https://player.example/embed/${index}`, title: "x".repeat(300)
    }))
  });
  assert.equal(signals.embeds.length, 4);
  assert.equal(signals.embeds[0].title.length, 120);
  assert.deepEqual(signals.embeds.map(embed => embed.url), [
    "https://player.example/embed/0", "https://player.example/embed/1",
    "https://player.example/embed/2", "https://player.example/embed/3"
  ]);
  // A page carrying no player markup at all reports an empty, false signal set.
  assert.deepEqual(scanSignals(), { embeds: [], containers: false, playerScripts: false });
});

test("an MSE player without captured media stays downloadable through the page address",async()=>{
  const h=backgroundHarness();
  await h.content({channel:'ssdownload-content',event:'scan',documentId:'d',pageUrl:'https://www.youtube.com/watch?v=abc',players:[
    {id:'movie_player',urls:['blob:https://www.youtube.com/0000'],visible:true,played:true}
  ],sources:[]});
  const state=await h.state(7);
  const candidate=state.videos[0];
  assert.equal(candidate.kind,'page');
  assert.equal(candidate.url,'https://www.youtube.com/watch?v=abc');
  assert.equal(candidate.downloadable,true);
  assert.equal(candidate.selectable,true);
  assert.match(candidate.note,/sayfa adresi/);
});

test("a player document without any media element keeps one selectable page candidate",async()=>{
  const h=backgroundHarness();
  const pageUrl='https://watch.example/film/sample?source=main';
  await h.content({channel:'ssdownload-content',event:'scan',documentId:'d',pageUrl,pageTitle:'Beastly (2011)',players:[],
    playerSignals:{embeds:[],containers:true,playerScripts:true},sources:[]});
  const state=await h.state(7);
  assert.equal(state.videos.length,1);
  assert.equal(state.candidates.length,0);
  const candidate=state.videos[0];
  assert.equal(candidate.kind,'page');
  assert.equal(candidate.url,pageUrl);
  assert.equal(candidate.frameId,0);
  assert.equal(candidate.title,'Beastly (2011)');
  assert.equal(candidate.downloadable,true);
  assert.equal(candidate.selectable,true);
  assert.equal(candidate.ambiguous,false);
  assert.match(candidate.note,/uygulamada çözümlenerek/);
});

test("an embed iframe address becomes its own page candidate beside the document one",async()=>{
  const h=backgroundHarness();
  const pageUrl='https://watch.example/film/sample';
  const embedUrl='https://player.example/embed/sample';
  await h.content({channel:'ssdownload-content',event:'scan',documentId:'d',pageUrl,pageTitle:'Beastly',players:[],
    playerSignals:{embeds:[{url:embedUrl,title:'Beastly 2011 Full HD'}],containers:false,playerScripts:true},sources:[]});
  let state=await h.state(7);
  assert.equal(state.videos.length,2);
  assert.ok(state.videos.some(item=>item.url===pageUrl&&item.kind==='page'));
  let embed=state.videos.find(item=>item.url===embedUrl);
  assert.equal(embed.kind,'page');
  assert.equal(embed.downloadable,true);
  assert.equal(embed.selectable,true);
  assert.equal(embed.ambiguous,false);
  assert.equal(embed.frameId,0);
  assert.equal(embed.pageUrl,pageUrl);
  assert.equal(embed.title,'Beastly 2011 Full HD');
  assert.match(embed.note,/uygulamada/);
  // A discovered item that already covers the address stays the only record for it.
  await h.events.headers.listeners[0]({tabId:7,frameId:0,statusCode:200,url:embedUrl,responseHeaders:[{name:'content-type',value:'application/vnd.apple.mpegurl'}]});
  state=await h.state(7);
  const covering=state.videos.filter(item=>item.url===embedUrl);
  assert.equal(covering.length,1);
  assert.equal(covering[0].kind,'hls');
});

test("a frame without player signals keeps producing nothing",async()=>{
  const h=backgroundHarness();
  await h.content({channel:'ssdownload-content',event:'scan',documentId:'d',pageUrl:'https://news.example/article',pageTitle:'Haber',players:[],
    playerSignals:{embeds:[],containers:false,playerScripts:false},sources:[]});
  let state=await h.state(7);
  assert.equal(state.videos.length,0);
  assert.equal(state.candidates.length,0);
  // A document that reports no signal at all is unchanged as well.
  await h.content({channel:'ssdownload-content',event:'scan',documentId:'d2',pageUrl:'https://news.example/other',pageTitle:'Haber 2',players:[],sources:[]});
  state=await h.state(7);
  assert.equal(state.videos.length,0);
  assert.equal(state.candidates.length,0);
});

test("a blocked document with only player signals stays a non-downloadable candidate",async()=>{
  const h=backgroundHarness();
  const pageUrl='https://frame.example/iframe';
  await h.events.headers.listeners[0]({tabId:7,frameId:0,type:'main_frame',documentId:'d',statusCode:403,url:pageUrl,responseHeaders:[]});
  await h.content({channel:'ssdownload-content',event:'scan',documentId:'d',pageUrl,pageTitle:'Service',players:[],
    playerSignals:{embeds:[],containers:true,playerScripts:true},sources:[]});
  const state=await h.state(7);
  assert.equal(state.videos.length,1);
  assert.equal(state.videos[0].downloadable,false);
  assert.equal(state.videos[0].selectable,false);
  assert.match(state.videos[0].note,/HTTP 403/);
  // The page affordance cannot hand the blocked document over either.
  const refused=await h.content({channel:'ssdownload-content',event:'download-page',mediaKind:'video',documentId:'d',pageUrl,players:[],
    playerSignals:{embeds:[],containers:true,playerScripts:true},sources:[]});
  assert.equal(h.nativeRequests.filter(request=>request?.action?.type==='browser_media').length,0,'a blocked page must not reach the picker');
  assert.match(String(refused.error),/HTTP 403/);
});

test("a gesture report without the markup facts keeps the document page candidate",async()=>{
  const h=backgroundHarness();
  const pageUrl='https://site.example/watch';
  const placeholder={id:'empty',urls:[],title:'Film',visible:false};
  await h.content({channel:'ssdownload-content',event:'scan',documentId:'d',pageUrl,pageTitle:'Film',players:[placeholder],
    playerSignals:{embeds:[],containers:true,playerScripts:false},sources:[]});
  assert.equal((await h.state(7)).videos.length,1);
  await h.content({channel:'ssdownload-content',event:'download-video',mediaKind:'video',playerId:'empty',documentId:'d',pageUrl,
    players:[placeholder],sources:[]});
  const state=await h.state(7);
  assert.equal(state.videos.length,1);
  assert.equal(state.videos[0].kind,'page');
  assert.equal(state.videos[0].url,pageUrl);
});

test("a preview-only document keeps reporting nothing even with player markup",async()=>{
  const h=backgroundHarness();
  const pageUrl='https://site.example/';
  await h.content({channel:'ssdownload-content',event:'play',documentId:'d',pageUrl,pageTitle:'Öne çıkanlar',players:[
    {id:'preview',title:'Tanıtım',urls:[],active:true,loop:true,muted:true,userActivated:false}],
    playerSignals:{embeds:[],containers:true,playerScripts:false},sources:[]});
  const state=await h.state(7);
  assert.equal(state.videos.length,0);
  assert.equal(state.candidates.length,0);
});

test("a signaled player with its observed manifest keeps one record and adds no page candidate",async()=>{
  const h=backgroundHarness();
  const pageUrl='https://site.example/watch';
  const manifest='https://cdn.example/master.m3u8';
  await h.content({channel:'ssdownload-content',event:'scan',documentId:'d',pageUrl,pageTitle:'Film',players:[{id:'p',title:'Film',urls:[manifest],active:true}],
    playerSignals:{embeds:[],containers:true,playerScripts:true},sources:[{url:manifest,contentType:'application/vnd.apple.mpegurl'}]});
  const state=await h.state(7);
  assert.equal(state.videos.length,1);
  assert.equal(state.videos[0].url,manifest);
  assert.equal(state.videos[0].kind,'hls');
  assert.equal(state.videos[0].downloadable,true);
});

test("the page affordance hands over only this document's verified page candidate",async()=>{
  const h=backgroundHarness();
  const pageUrl='https://watch.example/film/sample';
  const message={channel:'ssdownload-content',documentId:'d',pageUrl,pageTitle:'Beastly (2011)',players:[],sources:[],
    playerSignals:{embeds:[],containers:true,playerScripts:true}};
  await h.content({...message,event:'scan'});
  const reply=await h.content({...message,event:'download-page',mediaKind:'video'});
  const handoff=h.nativeRequests.find(request=>request?.action?.type==='browser_media');
  assert.ok(handoff,'the page affordance must reach the native picker');
  assert.equal(handoff.action.request.url,pageUrl);
  assert.equal(handoff.action.request.page_url,pageUrl);
  assert.equal(reply?.ok,true);
  // A page discovery never confirmed a player for has no verified candidate to hand over.
  const bare=backgroundHarness();
  const bareUrl='https://news.example/article';
  await bare.content({channel:'ssdownload-content',event:'scan',documentId:'d',pageUrl:bareUrl,pageTitle:'Haber',players:[],sources:[]});
  const refused=await bare.content({channel:'ssdownload-content',event:'download-page',mediaKind:'video',documentId:'d',pageUrl:bareUrl,players:[],sources:[]});
  assert.equal(bare.nativeRequests.filter(request=>request?.action?.type==='browser_media').length,0,'a page without verified player evidence must not be handed over');
  assert.match(String(refused.error),/doğrulanamadı/);
});

test("direct players cannot block MSE inference or lend it their own manifest",async()=>{
  const h=backgroundHarness();
  await h.content({channel:'ssdownload-content',event:'scan',documentId:'d',pageUrl:'https://site.example/watch',players:[
    {id:'direct',urls:['https://cdn.example/related.mp4'],visible:true},
    {id:'blob',urls:['blob:https://site.example/main'],visible:true}
  ],sources:[{url:'https://cdn.example/related.mp4',contentType:'video/mp4'}]});
  await h.events.headers.listeners[0]({tabId:7,frameId:0,statusCode:200,url:'https://cdn.example/main.m3u8',responseHeaders:[]});
  let state=await h.state(7);
  assert.equal(state.videos.find(v=>v.videoId.endsWith(':blob')).url,'https://cdn.example/main.m3u8');
  await h.content({channel:'ssdownload-content',event:'scan',documentId:'d',pageUrl:'https://site.example/watch',players:[
    {id:'direct',urls:['https://cdn.example/main.m3u8'],visible:true},
    {id:'blob',urls:['blob:https://site.example/main'],visible:true}
  ],sources:[{url:'https://cdn.example/main.m3u8',contentType:'application/vnd.apple.mpegurl'}]});
  state=await h.state(7);
  // The blob player must not borrow the direct player's manifest. It stays usable through the
  // page address, which the application resolves with its own media engine.
  const blob=state.videos.find(v=>v.videoId.endsWith(':blob'));
  assert.equal(blob.url,'https://site.example/watch');
  assert.equal(blob.kind,'page');
  assert.equal(blob.downloadable,true);
});

test("a concealed seek preview neither offers itself nor makes the shown blob player ambiguous",async()=>{
  const h=backgroundHarness();
  await h.content({channel:'ssdownload-content',event:'scan',documentId:'d',pageUrl:'https://site.example/watch',players:[
    {id:'main',urls:['blob:https://site.example/main'],visible:true,active:true},
    {id:'scrub',urls:['blob:https://site.example/scrub'],visible:true,concealed:true,played:true,loop:true,muted:true}
  ],sources:[]});
  await h.events.headers.listeners[0]({tabId:7,frameId:0,statusCode:200,url:'https://cdn.example/main.m3u8',responseHeaders:[]});
  const state=await h.state(7);
  assert.deepEqual(state.videos.map(v=>[v.videoId.split(':').pop(),v.url]),[['main','https://cdn.example/main.m3u8']]);
});

test("a 403 player document cannot promote its Service video as a verified episode",async()=>{
  const h=backgroundHarness();
  await h.events.headers.listeners[0]({tabId:7,frameId:2,type:'sub_frame',documentId:'d',statusCode:403,url:'https://frame.example/iframe',responseHeaders:[]});
  await h.events.updated.listeners[0](7,{status:'loading'});
  await h.content({channel:'ssdownload-content',event:'scan',documentId:'d',pageUrl:'https://frame.example/iframe',pageTitle:'Service',players:[{id:'p',title:'Service',urls:['https://frame.example/master.m3u8']}],sources:[{url:'https://frame.example/master.m3u8',contentType:'application/vnd.apple.mpegurl'}]},7,2);
  const state=await h.state(7);
  assert.equal(state.videos[0].downloadable,false);assert.match(state.videos[0].note,/HTTP 403/);
  await h.events.headers.listeners[0]({tabId:7,frameId:2,type:'xmlhttprequest',documentId:'d',statusCode:200,url:'https://frame.example/master.m3u8',responseHeaders:[]});
  const verified=await h.state(7);
  assert.equal(verified.videos[0].downloadable,true);
});

test("committed navigation prunes the previous iframe without any controller page",async()=>{
  const committed=event();
  const h=backgroundHarness({navigation:{onCommitted:committed,async getAllFrames(){return [{frameId:2,documentId:'new'}];}}});
  await h.content({channel:'ssdownload-content',event:'scan',documentId:'old',pageUrl:'https://site.example/old',players:[{id:'old-player',urls:['https://cdn.example/old.mp4']}]},7,2);
  await h.events.headers.listeners[0]({tabId:7,frameId:2,documentId:'new',statusCode:200,url:'https://cdn.example/new.m3u8',responseHeaders:[]});
  committed.listeners[0]({tabId:7,frameId:2});
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(vm.runInContext('frames.size',h.context),0);
  assert.deepEqual(JSON.parse(vm.runInContext('JSON.stringify([...mediaByTab.get(7).values()].map(v=>v.url))',h.context)),['https://cdn.example/new.m3u8']);
});


test("iframe player branding yields to the film page but explicit video titles remain",async()=>{
  const h=backgroundHarness();
  const message={channel:'ssdownload-content',event:'scan',documentId:'frame',pageUrl:'https://player.example/embed',pageTitle:'MobiPlayer',players:[{id:'p',urls:['https://cdn.example/movie.m3u8'],title:'MobiPlayer'}],sources:[{url:'https://cdn.example/movie.m3u8',contentType:'application/vnd.apple.mpegurl'}]};
  const send=async()=>{
    h.events.message.listeners.at(-1)(message,{id:"test-extension",url:message.pageUrl,tab:{id:7,title:'Sevimsiz (2011)'},frameId:2},()=>{});
    await new Promise(resolve=>setImmediate(resolve));
    return h.state(7);
  };
  assert.equal((await send()).videos[0].title,'Sevimsiz (2011)');
  message.pageTitle='"'+':7275'+'" video player';
  assert.equal((await send()).videos[0].title,'Sevimsiz (2011)');
  message.pageTitle='1917';
  assert.equal((await send()).videos[0].title,'1917');
  message.players[0].explicitTitle=true;message.players[0].title='Player';
  assert.equal((await send()).videos[0].title,'Player');
});


test("late text/plain HLS discovery retains only the exact document request context",async()=>{
  const h=backgroundHarness();
  await h.state(7);
  const url='https://cdn.example/list/opaque';
  const capture=(id,frameId,documentId,accept)=>h.events.beforeHeaders.listeners[0]({tabId:7,requestId:id,frameId,documentId,url,requestHeaders:[{name:'Accept',value:accept},{name:'Referer',value:'https://player.example/watch'},{name:'Cookie',value:'private'},{name:'Authorization',value:'private'}]});
  capture('right',2,'doc','*/*');
  await h.events.headers.listeners[0]({tabId:7,requestId:'right',frameId:2,documentId:'doc',url,statusCode:200,responseHeaders:[{name:'Content-Type',value:'text/plain'}]});
  capture('wrong-frame',3,'other','wrong');capture('old-doc',2,'old','wrong');
  const message={channel:'ssdownload-content',event:'manifest',documentId:'local',pageUrl:'https://player.example/watch',sources:[{url,contentType:'application/vnd.apple.mpegurl',pageUrl:'https://player.example/watch'}],manifest:{url,children:[]}};
  h.events.message.listeners.at(-1)(message,{id:"test-extension",tab:{id:7},frameId:2,documentId:'doc',url:message.pageUrl},()=>{});
  await new Promise(resolve=>setImmediate(resolve));
  const item=(await h.state(7)).items[0];
  assert.deepEqual(JSON.parse(JSON.stringify(item.requestHeaders)),{accept:'*/*',referer:message.pageUrl});
  assert.equal(item.networkStatus,200);

  h.events.message.listeners.at(-1)({...message,sources:[{url:url+'/different',contentType:'application/vnd.apple.mpegurl'}]},{id:"test-extension",tab:{id:7},frameId:2,documentId:'doc',url:message.pageUrl},()=>{});
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal((await h.state(7)).items.some(i=>i.url.endsWith('/different')),false);
});

test("the extension event queue keeps fifty events and drops the oldest", async () => {
  const h = backgroundHarness();
  await vm.runInContext("SSDownloadEvents.flush()", h.context);
  h.logs.length = 0;
  const relay = (name, fields) => vm.runInContext(`SSDownloadEvents.log(${JSON.stringify(name)},${JSON.stringify(fields)})`, h.context);
  for (let index = 0; index < 60; index += 1) {
    relay("ext.capture", { level: "info", outcome: "ok", host: "cdn.example", code: "SSD-EXT-007", detail: `event-${index}` });
  }
  await vm.runInContext("SSDownloadEvents.flush()", h.context);
  const delivered = h.logs.flat();
  assert.equal(delivered.length, 50);
  assert.equal(delivered[0].detail, "event-10");
  assert.equal(delivered.at(-1).detail, "event-59");
  assert.equal(h.logs.every(batch => batch.length <= 20), true);
});

test("a native log request never exceeds eight kilobytes and flushes at twenty events", async () => {
  const h = backgroundHarness();
  await vm.runInContext("SSDownloadEvents.flush()", h.context);
  h.logs.length = 0;
  const relay = (name, fields) => vm.runInContext(`SSDownloadEvents.log(${JSON.stringify(name)},${JSON.stringify(fields)})`, h.context);
  for (let index = 0; index < 20; index += 1) relay("ext.popup.open", { level: "info", outcome: "ok", host: "site.example", detail: `open-${index}` });
  // The twenty-event threshold flushes on its own: the five-second timer is replaced.
  assert.equal(h.timers.size, 0);
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(h.logs.length, 1);
  assert.equal(h.logs[0].length, 20);
  h.logs.length = 0;
  for (let index = 0; index < 45; index += 1) relay("ext.capture", { level: "info", outcome: "ok", host: "media.example", job: "j".repeat(64), detail: "d".repeat(200) });
  await vm.runInContext("SSDownloadEvents.flush()", h.context);
  assert.ok(h.logs.length > 1);
  assert.equal(h.logs.every(batch => batch.length <= 20), true);
  assert.equal(h.logs.every(batch => JSON.stringify(batch).length <= 8192), true);
  assert.equal(h.logs.flat().length, 45);
});

test("relayed events keep only a bounded host, a known code and a single-line detail", async () => {
  const h = backgroundHarness();
  await vm.runInContext("SSDownloadEvents.flush()", h.context);
  h.logs.length = 0;
  const relay = (name, fields) => vm.runInContext(`SSDownloadEvents.log(${JSON.stringify(name)},${JSON.stringify(fields)})`, h.context);
  relay("ext.capture", { level: "info", outcome: "ok", host: "https://www.youtube.com/watch?v=abc&token=secret",
    code: "SSD-NOPE-999", detail: "x".repeat(500), cookie: "session=secret", job: null });
  relay("ext.capture", { level: "info", outcome: "ok", host: "session=abc123; Path=/private" });
  relay("ext.inspect.fail", { level: "warn", outcome: "failed", code: "SSD-EXT-010", host: "cdn.example",
    detail: "inspect failed https://media.example/private/movie.mp4?token=1\nsession=abc123" });
  await vm.runInContext("SSDownloadEvents.flush()", h.context);
  const delivered = h.logs.flat();
  assert.equal(delivered.length, 3);
  const [full, cookie, known] = delivered;
  assert.equal(full.host, "www.youtube.com");
  assert.equal("code" in full, false);
  assert.equal("cookie" in full, false);
  assert.equal("job" in full, false);
  assert.ok(full.detail.length <= 200);
  assert.equal(Object.keys(full).every(key => ["event", "level", "outcome", "code", "host", "job", "detail"].includes(key)), true);
  assert.equal("host" in cookie, false);
  assert.equal(known.code, "SSD-EXT-010");
  assert.equal(known.detail.includes("\n"), false);
  assert.equal(known.detail.includes("https://"), false);
  assert.equal(known.detail.includes("session="), false);
  assert.ok(known.detail.includes("media.example"));
  assert.equal(delivered.every(entry => JSON.stringify(entry).length <= 8192), true);
});

test("content log messages are accepted only from extension senders", async () => {
  const h = backgroundHarness();
  await vm.runInContext("SSDownloadEvents.flush()", h.context);
  h.logs.length = 0;
  await h.content({ channel: "ssdownload-content", event: "log", events: [{ event: "ext.content.session", level: "info", outcome: "ok", host: "player.example" }] });
  await vm.runInContext("SSDownloadEvents.flush()", h.context);
  assert.equal(h.logs.flat().length, 1);
  assert.equal(h.logs.flat()[0].event, "ext.content.session");
  h.logs.length = 0;
  const listener = h.events.message.listeners.at(-1);
  let contentReply = null;
  listener({ channel: "ssdownload-content", event: "log", events: [{ event: "ext.content.session", host: "evil.example" }] },
    { id: "other-extension", url: "https://evil.example/page", tab: { id: 9 }, frameId: 0 }, value => { contentReply = value; });
  assert.match(contentReply.error, /Geçersiz içerik göndericisi/);
  await new Promise(resolve => setImmediate(resolve));
  await vm.runInContext("SSDownloadEvents.flush()", h.context);
  assert.equal(h.logs.flat().length, 0);
});

// ----------------------------------------------------------------- interface language
// The extension speaks the desktop app's language: the capabilities reply names it, the worker
// stores it, and the popup, the page tiles, the menus and the badge follow. Until the app has
// said anything, the browser's UI language decides.
const capabilitiesIn = language => ({ ok: true, result: { capabilities: { ...nativeCapabilities.result.capabilities, ...(language === undefined ? {} : { language }) } } });
const popupState = h => new Promise(resolve => h.events.message.listeners.at(-1)({ channel: "ssdownload-popup", command: "state" },
  { id: "test-extension", url: "chrome-extension://test-extension/popup.html" }, resolve));
const lastAction = (h, method) => h.actionCalls.filter(call => call.method === method).at(-1);
const probeNow = async h => { vm.runInContext("capabilityState.checkedAt = 0", h.context); await vm.runInContext("refreshCapabilities()", h.context); await h.settle(); };

test("the desktop's language switches the context menus and the badge title, and back", async () => {
  let language = "en";
  const h = backgroundHarness({ native: async request => request?.action?.type === "capabilities" ? capabilitiesIn(language) : { ok: true, message: "ok" } });
  await h.settle();
  const titles = () => h.menuEntries.map(entry => entry.title);
  assert.ok(titles().includes("Download link with SSDownload"), "the link entry is English once the desktop says so");
  assert.ok(titles().includes("Download video with SSDownload"));
  assert.ok(titles().includes("Download page with SSDownload"));
  assert.ok(titles().includes("Download audio with SSDownload"));
  assert.ok(titles().includes("Open the download picker…"));
  assert.ok(titles().includes("Allow session cookies for this site"));
  assert.ok(titles().includes("Remove site session permission"));
  assert.ok(titles().includes("Open SSDownload"));
  assert.ok(!titles().some(title => /SSDownload ile|indir/.test(title)), "no Turkish title is left in an English menu");
  assert.equal(h.storage.local.uiLanguage, "en", "the worker stores the language for the popup and the pages");
  assert.equal(lastAction(h, "setBadgeText").text, "ON");
  assert.equal(lastAction(h, "setTitle").title, "SSDownload: on — click for settings");

  language = "tr";
  await probeNow(h);
  assert.ok(titles().includes("Bağlantıyı SSDownload ile indir"), "the menus are rebuilt in Turkish");
  assert.ok(titles().includes("Videoyu SSDownload ile indir"));
  assert.ok(titles().includes("SSDownload uygulamasını aç"));
  assert.ok(!titles().some(title => /\b(?:Download|Open|Allow|Remove|Choose)\b/.test(title)), "no English title is left in a Turkish menu");
  assert.equal(h.storage.local.uiLanguage, "tr");
  assert.equal(lastAction(h, "setBadgeText").text, "AÇIK");
  assert.match(lastAction(h, "setTitle").title, /açık — ayarlar için tıklayın/);

  language = "en";
  await probeNow(h);
  assert.ok(titles().includes("Download link with SSDownload"));
  assert.equal(lastAction(h, "setBadgeText").text, "ON");
});

test("the candidate submenu, completed-download title and refusals follow the desktop's language", async () => {
  const h = backgroundHarness({ local: { uiLanguage: "en" }, native: async request => request?.action?.type === "capabilities" ? capabilitiesIn("en") : { ok: true, message: "ok" } });
  const players = [{ id: "a", title: "Film", urls: ["https://cdn.example/a.mp4"], active: true }, { id: "b", title: "Clip", urls: ["https://cdn.example/b.mp4"], active: true, adSignals: ["player-identity", "request-host"] }];
  await h.content({ channel: "ssdownload-content", event: "scan", documentId: "d", pageUrl: "https://site.example/watch", players,
    sources: players.map(player => ({ url: player.urls[0], contentType: "video/mp4", playerId: player.id })) });
  h.runTimers(250);
  await h.settle();
  assert.ok(h.menuEntries.some(entry => entry.id === "ssdownload-candidates" && entry.title === "Choose source (2)…"));
  assert.ok(h.menuEntries.some(entry => entry.id === "ssdownload-candidate:1" && /· suspected ad$/.test(entry.title)), "the candidate flags are English too");
  vm.runInContext("noteCompletedJobs(0); noteCompletedJobs(1)", h.context);
  await h.settle();
  assert.equal(lastAction(h, "setTitle").title, "SSDownload: 1 download completed");
  vm.runInContext("noteCompletedJobs(3)", h.context);
  await h.settle();
  assert.equal(lastAction(h, "setTitle").title, "SSDownload: 3 downloads completed");
  await popupToggle(h);
  const refused = await new Promise(resolve => h.events.message.listeners.at(-1)({ channel: "ssdownload-content", event: "download-video", mediaKind: "video", playerId: "a", documentId: "d", pageUrl: "https://site.example/watch" },
    { id: "test-extension", url: "https://site.example/watch", tab: { id: 7 }, frameId: 0 }, resolve));
  assert.equal(refused.error, "SSDownload is off. Click the toolbar icon to turn it on.");
});

test("a missing app is described in the language the app last reported, else in the browser's", async () => {
  const missing = async () => { throw new Error("Native messaging host not found"); };
  const cases = [
    { stored: "en", browser: "tr", expected: /app was not found on this computer/, appMissing: true },
    { stored: "tr", browser: "en", expected: /uygulaması bu bilgisayarda bulunamadı/, appMissing: true },
    { stored: undefined, browser: "en", expected: /app was not found on this computer/, appMissing: true },
    { stored: undefined, browser: "tr", expected: /uygulaması bu bilgisayarda bulunamadı/, appMissing: true },
    { stored: undefined, browser: "en-GB", expected: /app was not found on this computer/, appMissing: true },
    { stored: undefined, browser: "tr-TR", expected: /uygulaması bu bilgisayarda bulunamadı/, appMissing: true },
    { stored: undefined, browser: "de", expected: /SSDownload-App wurde auf diesem Computer nicht gefunden/, appMissing: true },
    { stored: "de", browser: "tr", expected: /SSDownload-App wurde auf diesem Computer nicht gefunden/, appMissing: true },
    { stored: undefined, browser: "ja", expected: /app was not found on this computer/, appMissing: true }
  ];
  for (const { stored, browser, expected, appMissing } of cases) {
    const h = backgroundHarness({ native: missing, browserLanguage: browser, local: stored ? { uiLanguage: stored } : {} });
    await h.settle();
    const state = await popupState(h);
    assert.match(state.capabilityError, expected, JSON.stringify({ stored, browser }));
    assert.equal(state.appMissing, appMissing);
    await h.menu("ssdownload-app");
    const status = JSON.parse(vm.runInContext("JSON.stringify(tabStatus.get(7)||null)", h.context));
    assert.match(String(status?.message), expected);
  }
  const english = backgroundHarness({ native: missing, local: { uiLanguage: "en" } });
  await english.settle();
  const refused = backgroundHarness({ native: async () => { throw new Error("Access to the specified native messaging host is forbidden."); }, local: { uiLanguage: "en" } });
  await refused.settle();
  assert.match((await popupState(refused)).capabilityError, /does not allow this extension/);
  assert.doesNotMatch((await popupState(refused)).capabilityError, /was not found/);
  const closed = backgroundHarness({ native: async request => request?.action?.type === "capabilities" ? { ok: false, message: "SSD-BRG-005 SSDownload uygulamasına ulaşılamadı" } : { ok: true }, local: { uiLanguage: "en" } });
  await closed.settle();
  assert.match((await popupState(closed)).capabilityError, /SSDownload is not running\. Start it with "Open the app" in the toolbar popup\./);
});

test("an app that reports no valid language leaves the stored one alone, also across a worker restart", async () => {
  let reply = capabilitiesIn("en");
  const native = async request => request?.action?.type === "capabilities" ? reply : { ok: true, message: "ok" };
  const first = backgroundHarness({ native });
  await first.settle();
  assert.equal(first.storage.local.uiLanguage, "en");
  for (const unknown of [undefined, "xx", null, 5]) {
    reply = capabilitiesIn(unknown);
    await probeNow(first);
    assert.equal(first.storage.local.uiLanguage, "en", "an old app, or garbage, never resets the language: " + String(unknown));
    assert.ok(first.menuEntries.some(entry => entry.title === "Download link with SSDownload"));
  }
  // A restarted worker reads the stored language before the desktop has answered.
  const gone = async () => { throw new Error("Native messaging host not found"); };
  const restarted = backgroundHarness({ native: gone, local: first.storage.local, session: first.storage.session });
  await restarted.settle();
  assert.match((await popupState(restarted)).capabilityError, /was not found on this computer/);
});

test("the language reaches the popup: texts, state reading, buttons and the source kinds", async () => {
  const media = [{ index: 0, label: "Film", kind: "video" }, { index: 1, label: "Song", kind: "audio" }];
  let state = { detectionEnabled: true, host: "site.example", siteHidden: false, media, capabilityError: null, appMissing: false };
  const h = popupHarness(() => state, { local: { uiLanguage: "en" } });
  await new Promise(resolve => setImmediate(resolve));
  const texts = () => Object.fromEntries(h.staticNodes.map(node => [node.id, node.textContent]));
  assert.equal(h.document.documentElement.lang, "en");
  assert.equal(h.el("state").textContent, "ON");
  assert.equal(h.el("site-label").textContent, "Hide the button on site.example");
  assert.equal(h.el("open-app").textContent, "Open the app");
  assert.equal(h.el("close").textContent, "Close");
  assert.equal(h.el("empty").textContent, "No selectable media found on this page. Play the video and open this again.");
  assert.ok(Object.values(texts()).includes("Detection") && Object.values(texts()).includes("Media on this page"));
  assert.deepEqual(h.el("media").children.map(item => item.children[0].children[1].textContent), ["video", "audio"]);
  assert.ok(h.staticNodes.every(node => node.textContent !== ""), "every data-i18n element received a text");

  // The worker stores another language: the popup redraws everything, including what the worker words.
  state = { ...state, detectionEnabled: false, appMissing: true, capabilityError: "Uygulama bulunamadı" };
  h.storeLanguage("tr");
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(h.document.documentElement.lang, "tr");
  assert.equal(h.el("state").textContent, "", "no on/off reading without the app");
  assert.equal(h.el("site-label").textContent, "site.example için düğmeyi gösterme");
  assert.equal(h.el("open-app").textContent, "Uygulamayı aç");
  assert.equal(h.el("setup-download").textContent, "Uygulamayı indir");
  assert.equal(h.el("close").textContent, "Kapat");
  assert.equal(h.el("empty").textContent, "Bu sayfada seçilebilir medya bulunamadı. Videoyu oynatıp tekrar açın.");
  assert.deepEqual(h.el("media").children.map(item => item.children[0].children[1].textContent), ["video", "ses"]);
  assert.equal(h.el("status").textContent, "Uygulama bulunamadı");
  state = { ...state, host: "", appMissing: false };
  h.storeLanguage("en");
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(h.el("site-label").textContent, "Hide the button on this site");
  assert.equal(h.el("open-app").textContent, "Open the app");
  assert.equal(h.document.documentElement.dir, "ltr");

  // Arabic is written right to left: the page turns its direction with its texts.
  h.storeLanguage("ar");
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(h.document.documentElement.lang, "ar");
  assert.equal(h.document.documentElement.dir, "rtl");
  assert.equal(h.el("close").textContent, "إغلاق");
  h.storeLanguage("en");
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(h.document.documentElement.dir, "ltr");
});

test("without the desktop app the popup shows only the setup view, whose button opens the download page", async () => {
  const state = { detectionEnabled: true, host: "site.example", siteHidden: false, media: [], capabilityError: "The SSDownload app was not found on this computer.", appMissing: true };
  const h = popupHarness(() => state, { local: { uiLanguage: "en" } });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(h.el("setup").hidden, false);
  assert.equal(h.el("main").hidden, true, "the switches, the media list and the status line are out of the way");
  assert.equal(h.el("state").textContent, "");
  const texts = Object.fromEntries(h.staticNodes.map(node => [node.attributes["data-i18n"], node.textContent]));
  assert.equal(texts.setupRequiredTitle, "The SSDownload app is required");
  assert.match(texts.setupText, /free SSDownload app for Windows 10 and 11/);
  assert.equal(h.el("setup-download").textContent, "Download the app");
  h.el("setup-download").listeners.click();
  assert.deepEqual(h.created.map(value => value.url), ["https://ssdownload.isolmaz.com/download"]);

  // Once the app answers, the normal popup is back.
  Object.assign(state, { appMissing: false, capabilityError: null });
  h.storeLanguage("tr");
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(h.el("setup").hidden, true);
  assert.equal(h.el("main").hidden, false);
  assert.equal(h.el("state").textContent, "AÇIK");
});

test("a fresh install without the desktop app opens the welcome page once", async () => {
  const install = async (native, reason) => {
    const h = backgroundHarness(native ? { native } : {});
    await h.settle();
    for (const listener of h.events.installed.listeners) listener({ reason });
    await h.settle();
    return h.tabCreates.map(value => value.url);
  };
  const missing = async () => { throw new Error("Specified native messaging host not found."); };
  assert.deepEqual(await install(missing, "install"), ["chrome-extension://test-extension/welcome.html"]);
  assert.deepEqual(await install(missing, "update"), [], "an update never opens it");
  assert.deepEqual(await install(null, "install"), [], "an install next to the app skips it");
  const refused = async () => { throw new Error("Access to the specified native messaging host is forbidden."); };
  assert.deepEqual(await install(refused, "install"), [], "an app that refuses this copy is installed: no welcome");
});

test("the welcome page fills every text from the catalog in the stored language", async () => {
  const nodes = [...source("welcome.html").matchAll(/data-i18n="([^"]+)"/g)].map(match => ({ attributes: { "data-i18n": match[1] }, textContent: "",
    getAttribute(name) { return this.attributes[name] ?? null; } }));
  const document = { documentElement: { lang: "" }, querySelectorAll: selector => selector === "[data-i18n]" ? nodes : [] };
  const context = vm.createContext({ document, browser: { i18n: i18nApi("en"),
    storage: { local: { async get(defaults) { return { ...defaults, uiLanguage: "tr" }; } }, onChanged: { addListener() {} } } }, Promise, Object, Array });
  loadI18n(context);
  vm.runInContext(source("welcome.js"), context, { filename: "welcome.js" });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(document.documentElement.lang, "tr");
  const texts = Object.fromEntries(nodes.map(node => [node.attributes["data-i18n"], node.textContent]));
  assert.equal(texts.welcomeTitle, "SSDownload'u kurduğunuz için teşekkürler");
  assert.equal(texts.welcomeDownload, "Windows uygulamasını indir — ücretsiz");
  assert.ok(nodes.every(node => node.textContent !== ""), "every data-i18n element received a text");
  assert.match(source("welcome.html"), /href="https:\/\/ssdownload\.isolmaz\.com\/download"/);
});

test("the popup's missing-app status, picker progress and failure texts are English for an English app", async () => {
  const worker = backgroundHarness({ native: async () => { throw new Error("Specified native messaging host not found."); }, local: { uiLanguage: "en" } });
  await worker.settle();
  const ask = message => new Promise(resolve => worker.events.message.listeners.at(-1)({ channel: "ssdownload-popup", ...message },
    { id: "test-extension", url: "chrome-extension://test-extension/popup.html" }, resolve));
  const reply = await ask({ command: "state" });
  assert.equal(reply.appMissing, true);
  assert.match(reply.capabilityError, /free SSDownload app for Windows: install it with "Download the app"/);
  assert.match(reply.capabilityError, /Tools → Browser setup/);
  const unknown = await ask({ command: "no-such-command" });
  assert.equal(unknown.error, "Unknown command");
  const stranger = await new Promise(resolve => worker.events.message.listeners.at(-1)({ channel: "ssdownload-popup", command: "state" }, { id: "other" }, resolve));
  assert.equal(stranger.error, "Invalid sender");
  const failing = popupHarness(message => message.command === "state" ? { detectionEnabled: true, host: "", siteHidden: false, media: [{ index: 0, label: "Film", kind: "video" }] } : { error: "No active tab was found." }, { local: { uiLanguage: "en" } });
  await new Promise(resolve => setImmediate(resolve));
  const click = failing.el("media").children[0].children[0].listeners.click;
  const pending = click();
  assert.equal(failing.el("status").textContent, "Opening the desktop picker…");
  await pending;
  assert.equal(failing.el("status").textContent, "No active tab was found.");
  const silent = popupHarness(() => undefined, { local: { uiLanguage: "en" } });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(silent.el("status").textContent, "The extension background did not respond.");
});

test("the in-page tiles follow the stored language live and fall back to the browser's", async () => {
  const ready = { status: "ready", usageModes: { video: true, file: true, audio: true }, siteEntries: true };
  const media = tag => ({ srcObject: null, currentSrc: "/movie.mp4", src: "/movie.mp4", tagName: tag, getAttribute() { return null; }, querySelectorAll() { return []; },
    getBoundingClientRect() { return { left: 10, top: 20, right: 650, bottom: 380, width: 640, height: 360 }; }, checkVisibility() { return true; } });
  const labels = h => h.buttons.map(button => button.textContent);
  const draw = async options => { const h = contentHarness({ reply: ready, ...options }); await vm.runInContext("refreshUsageModes()", h.context); await new Promise(resolve => setImmediate(resolve)); return h; };

  const video = await draw({ storedLanguage: "en", nodes: { "video, audio": [media("VIDEO")] } });
  assert.deepEqual(labels(video), ["Download this video"]);
  video.storageListener()({ uiLanguage: { newValue: "tr" } }, "local");
  assert.deepEqual(labels(video), ["Bu videoyu indir"], "a language written to storage repaints the tile at once");
  video.storageListener()({ uiLanguage: { newValue: "en" } }, "local");
  assert.deepEqual(labels(video), ["Download this video"]);
  assert.equal(video.buttons[0].style.direction, "ltr");
  video.storageListener()({ uiLanguage: { newValue: "ar" } }, "local");
  assert.deepEqual(labels(video), ["تنزيل هذا الفيديو"]);
  assert.equal(video.buttons[0].style.direction, "rtl", "an Arabic tile is laid out right to left");
  video.storageListener()({ uiLanguage: { newValue: "en" } }, "local");
  assert.equal(video.buttons[0].style.direction, "ltr", "and turns back with the language");

  const audio = await draw({ storedLanguage: "en", nodes: { "video, audio": [media("AUDIO")] } });
  assert.deepEqual(labels(audio), ["Download this audio"]);
  audio.storageListener()({ uiLanguage: { newValue: "tr" } }, "local");
  assert.deepEqual(labels(audio), ["Bu sesi indir"]);

  const page = await draw({ storedLanguage: "en", nodes: { "video, audio": [], iframe: [{ src: "https://player.example/embed/one" }] } });
  assert.deepEqual(labels(page), ["Download page with SSDownload"]);
  page.storageListener()({ uiLanguage: { newValue: "tr" } }, "local");
  assert.deepEqual(labels(page), ["Sayfayı SSDownload ile indir"]);

  // No stored language: the browser's decides.
  assert.deepEqual(labels(await draw({ browserLanguage: "en-US", nodes: { "video, audio": [media("VIDEO")] } })), ["Download this video"]);
  assert.deepEqual(labels(await draw({ browserLanguage: "tr", nodes: { "video, audio": [media("VIDEO")] } })), ["Bu videoyu indir"]);
  assert.deepEqual(labels(await draw({ browserLanguage: "tr", storedLanguage: "en", nodes: { "video, audio": [media("VIDEO")] } })), ["Download this video"], "the app's language beats the browser's");

  // The page's own status line and the tile title are what the worker words; the tile only
  // adds the text of the one case the worker has not answered yet.
  const unverified = contentHarness({ reply: new Promise(() => {}), storedLanguage: "en" });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(vm.runInContext("usageModeTitle()", unverified.context), "Desktop capabilities have not been verified yet.");
  unverified.storageListener()({ uiLanguage: { newValue: "tr" } }, "local");
  assert.equal(vm.runInContext("usageModeTitle()", unverified.context), "Masaüstü özellikleri henüz doğrulanmadı.");
});
