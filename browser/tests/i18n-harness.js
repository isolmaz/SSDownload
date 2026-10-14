"use strict";

// Shared by the node:test harnesses (not a test file itself): the extension's scripts run in
// `vm` contexts with a mocked `browser`, and every one of them needs i18n.js loaded first and
// the browser's own UI language to fall back on.

const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const extensionDir = path.resolve(__dirname, "..", "chromium");
const read = name => fs.readFileSync(path.join(extensionDir, name), "utf8");

// The `browser.i18n` double. Turkish by default, so the existing assertions on Turkish wording
// keep testing the real text; a test that wants the other language says so.
function i18nApi(uiLanguage = "tr") {
  return { getUILanguage: () => uiLanguage };
}

// Runs the catalog first, exactly like the service worker's importScripts list, the content
// script list and popup.html do.
function loadI18n(context) {
  vm.runInContext(read("i18n.js"), context, { filename: "i18n.js" });
}

// Parses the real messages the extension ships, for the structural checks.
function catalog() {
  const context = vm.createContext({});
  vm.runInContext(read("i18n.js"), context, { filename: "i18n.js" });
  return context.SSDownloadI18n;
}

module.exports = { extensionDir, i18nApi, loadI18n, catalog };
