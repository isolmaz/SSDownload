"use strict";

// Firefox bundle contract. Uses only Node built-ins: the Firefox package is a
// separate MV3 tree that must stay in lockstep with the Chromium sources, so this
// test guards the manifest, the shared files and the promise-only `browser`
// namespace the background scripts load under.
const assert = require("node:assert/strict");
const { webcrypto } = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const repoRoot = path.resolve(__dirname, "..", "..");
const chromiumDir = path.resolve(__dirname, "..", "chromium");
const firefoxDir = path.resolve(__dirname, "..", "firefox");

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, "utf8"));
}

function sorted(values) {
  return [...(values ?? [])].sort();
}

// Every path-like manifest value: js/html/icon/locale payloads. Wildcards and
// absolute URLs are not local files.
function manifestFileReferences(value, found = new Set()) {
  if (typeof value === "string") {
    if (!/[\s*]/.test(value) && !/^[a-z][a-z0-9+.-]*:/i.test(value)
      && /\.(?:js|mjs|html|css|png|svg|jpe?g|json|woff2?)$/i.test(value)) {
      found.add(value);
    }
  } else if (Array.isArray(value)) {
    for (const item of value) manifestFileReferences(item, found);
  } else if (value && typeof value === "object") {
    for (const item of Object.values(value)) manifestFileReferences(item, found);
  }
  return found;
}

const firefoxManifestPath = path.join(firefoxDir, "manifest.json");
const chromiumManifestPath = path.join(chromiumDir, "manifest.json");

test("Firefox manifest declares the Firefox MV3 bundle", () => {
  const manifest = readJson(firefoxManifestPath);
  const chromium = readJson(chromiumManifestPath);

  assert.equal(manifest.manifest_version, 3);
  assert.equal(manifest.name, chromium.name);
  assert.equal(manifest.description, chromium.description);
  assert.equal(manifest.default_locale, chromium.default_locale);

  assert.deepEqual(sorted(manifest.permissions), sorted([
    "alarms", "contextMenus", "downloads", "nativeMessaging",
    "storage", "tabs", "webRequest", "webNavigation"
  ]));
  assert.deepEqual(sorted(manifest.optional_permissions), ["cookies"]);
  assert.deepEqual(sorted(manifest.host_permissions), sorted(["http://*/*", "https://*/*"]));

  const gecko = manifest.browser_specific_settings?.gecko;
  assert.ok(gecko, "browser_specific_settings.gecko is required");
  assert.equal(gecko.id, "ssdownload@ssdownload.local");
  // data_collection_permissions requires a recent Firefox; 142 also clears the
  // Android data_collection_permissions lint warning.
  assert.ok(Number.parseFloat(gecko.strict_min_version) >= 142,
    `gecko.strict_min_version must be at least 142.0, got ${gecko.strict_min_version}`);
  assert.deepEqual(gecko.data_collection_permissions?.required, ["none"]);

  assert.ok(manifest.background && typeof manifest.background === "object");
  assert.deepEqual(manifest.background.scripts, [
    "i18n.js", "codes.js", "events.js", "background.js", "relay-background.js"
  ]);
  assert.equal(Object.hasOwn(manifest.background, "service_worker"), false);
  assert.equal(Object.hasOwn(manifest, "key"), false);
  assert.equal(Object.hasOwn(manifest, "minimum_chrome_version"), false);
  assert.equal(manifest.background.scripts.includes("service-worker.js"), false);

  assert.ok(Array.isArray(manifest.content_scripts) && manifest.content_scripts.length >= 2);
  assert.deepEqual(manifest.content_scripts[0].js, [
    "i18n.js", "relay-sha256.js", "content.js", "relay-content.js"
  ]);
  assert.deepEqual(manifest.content_scripts[1].js, ["page-observer.js"]);
  assert.equal(manifest.content_scripts[1].world, "MAIN");
});

test("Firefox manifest version matches Chromium and Cargo.toml", () => {
  const firefox = readJson(firefoxManifestPath);
  const chromium = readJson(chromiumManifestPath);
  const cargo = fs.readFileSync(path.join(repoRoot, "Cargo.toml"), "utf8");
  const cargoVersion = cargo.match(/^version\s*=\s*"([^"]+)"/m)?.[1];

  assert.equal(cargoVersion, chromium.version);
  assert.equal(firefox.version, chromium.version);
  assert.equal(firefox.version, cargoVersion);
});

test("every file referenced by the Firefox manifest exists", () => {
  const manifest = readJson(firefoxManifestPath);
  const references = manifestFileReferences(manifest);
  // `default_locale` is a directory reference rather than a file path.
  const locale = `_locales/${manifest.default_locale}/messages.json`;
  references.add(locale);

  for (const reference of references) {
    const file = path.join(firefoxDir, reference);
    assert.ok(fs.existsSync(file), `manifest references missing file: ${reference}`);
    assert.ok(fs.statSync(file).isFile(), `manifest reference is not a file: ${reference}`);
  }

  assert.equal(references.has("service-worker.js"), false,
    "the Firefox manifest must not reference service-worker.js");
});

test("Firefox shared files are byte-identical to the Chromium copies", () => {
  const shared = [
    "i18n.js", "codes.js", "events.js", "background.js", "relay-background.js",
    "relay-content.js", "relay-sha256.js", "content.js", "page-observer.js",
    "popup.html", "popup.js", "welcome.html", "welcome.js"
  ];

  for (const name of shared) {
    const firefoxFile = path.join(firefoxDir, name);
    const chromiumFile = path.join(chromiumDir, name);
    assert.ok(fs.existsSync(firefoxFile), `Firefox copy is missing: ${name}`);
    assert.ok(fs.existsSync(chromiumFile), `Chromium source is missing: ${name}`);
    assert.ok(fs.readFileSync(firefoxFile).equals(fs.readFileSync(chromiumFile)),
      `Firefox copy drifted from Chromium: ${name}`);
  }
});

function browserEvent() {
  const listeners = [];
  return {
    listeners,
    addListener(listener) {
      if (typeof listener === "function") listeners.push(listener);
    },
    removeListener(listener) {
      const index = listeners.indexOf(listener);
      if (index !== -1) listeners.splice(index, 1);
    },
    hasListener(listener) {
      return listeners.includes(listener);
    }
  };
}

function storageArea() {
  return {
    get: async () => ({}),
    set: async () => undefined,
    remove: async () => undefined,
    clear: async () => undefined
  };
}

// A promise-only `browser` double: no `chrome` fallback and no callback API.
// Every method resolves a sensible empty value, and every event exposes the
// three listener methods Firefox does.
function browserDouble() {
  const runtime = {
    id: "ssdownload@ssdownload.local",
    lastError: undefined,
    getURL: (resource) => String(resource ?? "")
      ? `moz-extension://test/${String(resource).replace(/^\/+/, "")}`
      : "moz-extension://test/",
    getManifest: () => ({ version: "1.11.0" }),
    getPlatformInfo: async () => ({ os: "win", arch: "x86-64" }),
    sendNativeMessage: async () => ({ ok: false, message: "native host unavailable" }),
    connectNative: () => ({
      onMessage: browserEvent(),
      onDisconnect: browserEvent(),
      postMessage: () => undefined,
      disconnect: () => undefined
    }),
    onInstalled: browserEvent(),
    onStartup: browserEvent(),
    onMessage: browserEvent(),
    onSuspend: browserEvent(),
    onConnect: browserEvent()
  };

  const tabs = {
    query: async () => [],
    get: async () => null,
    create: async () => ({ id: 1 }),
    sendMessage: async () => undefined,
    update: async () => null,
    onUpdated: browserEvent(),
    onRemoved: browserEvent(),
    onActivated: browserEvent()
  };

  const cookies = {
    getAll: async () => [],
    getAllCookieStores: async () => [],
    onChanged: browserEvent()
  };

  const downloads = {
    search: async () => [],
    pause: async () => undefined,
    resume: async () => undefined,
    cancel: async () => undefined,
    erase: async () => [],
    onCreated: browserEvent(),
    onChanged: browserEvent()
  };

  const alarms = {
    create: async () => undefined,
    clear: async () => false,
    get: async () => undefined,
    onAlarm: browserEvent()
  };

  const action = {
    setBadgeText: async () => undefined,
    setBadgeBackgroundColor: async () => undefined,
    setIcon: async () => undefined,
    setTitle: async () => undefined
  };

  const contextMenus = {
    create: async () => undefined,
    removeAll: async () => undefined,
    update: async () => undefined,
    onClicked: browserEvent()
  };

  const permissions = {
    contains: async () => false,
    request: async () => false,
    getAll: async () => ({}),
    remove: async () => false
  };

  const i18n = {
    getMessage: (key) => String(key ?? ""),
    getUILanguage: () => "en",
    getAcceptLanguages: async () => ["en"]
  };

  return {
    runtime,
    storage: { local: storageArea(), session: storageArea(), sync: storageArea(), onChanged: browserEvent() },
    tabs,
    cookies,
    webRequest: {
      onBeforeSendHeaders: browserEvent(),
      onHeadersReceived: browserEvent(),
      onBeforeRedirect: browserEvent()
    },
    webNavigation: {
      getAllFrames: async () => [],
      getFrame: async () => null,
      onCommitted: browserEvent()
    },
    downloads,
    alarms,
    action,
    contextMenus,
    permissions,
    i18n
  };
}

test("Firefox background scripts load under the promise-only browser namespace", async () => {
  const browser = browserDouble();
  const context = vm.createContext({
    browser,
    URL,
    TextEncoder,
    TextDecoder,
    crypto: webcrypto,
    console,
    fetch: async () => ({
      ok: false,
      status: 0,
      text: async () => "",
      json: async () => ({})
    }),
    setTimeout: () => 0,
    clearTimeout: () => undefined,
    queueMicrotask: (task) => Promise.resolve().then(task)
  });

  const scripts = ["i18n.js", "codes.js", "events.js", "background.js", "relay-background.js"];
  for (const name of scripts) {
    const source = fs.readFileSync(path.join(firefoxDir, name), "utf8");
    assert.doesNotThrow(() => vm.runInContext(source, context, { filename: name }),
      `${name} must load against the promise-only browser API`);
  }

  assert.ok(browser.runtime.onMessage.listeners.length >= 1, "runtime.onMessage listener not registered");
  assert.ok(browser.runtime.onInstalled.listeners.length >= 1, "runtime.onInstalled listener not registered");
  assert.ok(browser.webRequest.onBeforeSendHeaders.listeners.length >= 1,
    "webRequest.onBeforeSendHeaders listener not registered");
  assert.ok(browser.downloads.onCreated.listeners.length >= 1, "downloads.onCreated listener not registered");

  const installed = browser.runtime.onInstalled.listeners[0];
  assert.equal(typeof installed, "function");
  assert.doesNotThrow(() => installed({ reason: "install" }));

  // The listener starts promise work and returns; let its microtasks settle and
  // confirm nothing rejects unhandled.
  await new Promise((resolve) => setTimeout(resolve, 30));
});
