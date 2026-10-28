"use strict";

// The extension speaks every language of LANGUAGES, English being the source. These are the
// structural checks: all languages carry exactly the same keys and placeholders, every key the
// code asks for exists,
// no key is dead, and no user-visible text hides in the sources outside the catalog. The
// behavioural checks (language follows the desktop, browser fallback) sit with the harnesses
// in browser.test.js.

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const { extensionDir, catalog } = require("./i18n-harness");

const read = name => fs.readFileSync(path.join(extensionDir, name), "utf8");
const json = name => JSON.parse(read(name));
const scripts = fs.readdirSync(extensionDir).filter(name => name.endsWith(".js"));
// The catalog itself and the generated error-code mirror hold no UI text of their own.
const codeFiles = scripts.filter(name => name !== "i18n.js");
// The extension's own pages, whose static texts come from their data-i18n attributes.
const pages = ["popup.html", "welcome.html"];

const placeholders = text => [...new Set([...text.matchAll(/\{(\d+)\}/g)].map(match => Number(match[1])))].sort((a, b) => a - b);
// What one side has that the other lacks (both directions), the check every parity test runs.
const keyDifference = (left, right) => [
  ...Object.keys(left).filter(key => !(key in right)).map(key => `+${key}`),
  ...Object.keys(right).filter(key => !(key in left)).map(key => `-${key}`)
];

test("the store locales carry the same keys, placeholders and a description for each", () => {
  const locales = fs.readdirSync(path.join(extensionDir, "_locales")).map(name => [name, json(`_locales/${name}/messages.json`)]);
  const en = json("_locales/en/messages.json");
  assert.ok(locales.length >= 2);
  for (const [language, messages] of locales) {
    assert.deepEqual(keyDifference(en, messages), [], `a key present in one store locale only (${language})`);
    assert.ok(Object.keys(messages).length > 0);
    for (const [key, entry] of Object.entries(messages)) {
      assert.equal(typeof entry.message, "string", `${language}/${key} has a message`);
      assert.notEqual(entry.message.trim(), "", `${language}/${key} is not empty`);
      assert.match(String(entry.description), /\S/, `${language}/${key} has a description`);
      assert.deepEqual(Object.keys(entry.placeholders || {}).sort(), Object.keys(en[key].placeholders || {}).sort(), `${key}: the placeholder sets differ`);
    }
  }
  // The manifest keeps the English default and every __MSG_x__ it uses exists in both.
  const manifestText = read("manifest.json");
  assert.equal(JSON.parse(manifestText).default_locale, "en");
  const used = [...manifestText.matchAll(/__MSG_([A-Za-z0-9_@]+)__/g)].map(match => match[1]);
  assert.ok(used.includes("extName") && used.includes("extDescription"));
  for (const key of used) assert.ok(key in en, `${key} is missing from the default store locale`);
  for (const [language, messages] of locales) {
    assert.ok(messages.extName.message.length <= 75, `the ${language} store name stays within 75 characters`);
    assert.ok(messages.extDescription.message.length <= 132, `the ${language} store summary stays within 132 characters`);
  }
});

test("the parity check names a key removed from one language", () => {
  const full = { a: { message: "x" }, b: { message: "y" } };
  assert.deepEqual(keyDifference(full, { a: full.a }), ["+b"]);
  assert.deepEqual(keyDifference({ a: full.a }, full), ["-b"]);
  assert.deepEqual(keyDifference(full, { ...full }), []);
});

test("every language has the same keys and placeholders as English, none empty", () => {
  const { MESSAGES, LANGUAGES } = catalog();
  assert.deepEqual(Object.keys(MESSAGES), [...LANGUAGES], "every listed language has a catalog");
  for (const language of LANGUAGES) assert.deepEqual(keyDifference(MESSAGES.en, MESSAGES[language]), [], `a key present in one language only (${language})`);
  assert.ok(Object.keys(MESSAGES.en).length > 200, "the catalog holds the extension's texts");
  for (const language of LANGUAGES) {
    for (const [key, text] of Object.entries(MESSAGES[language])) {
      assert.match(key, /^[a-z][A-Za-z0-9]*$/, `${key}: keys are descriptive camelCase`);
      assert.equal(typeof text, "string", `${language}/${key}`);
      assert.notEqual(text.trim(), "", `${language}/${key} is empty`);
      assert.equal(text, text.trim(), `${language}/${key} has stray whitespace`);
      const set = placeholders(text);
      assert.deepEqual(set, placeholders(MESSAGES.en[key]), `${language}/${key}: the placeholder set differs from English`);
      assert.deepEqual(set, set.map((_, index) => index), `${language}/${key}: placeholders must be {0}, {1}, ... without gaps`);
      assert.doesNotMatch(text.replace(/\{\d+\}/g, ""), /[{}]/, `${language}/${key}: a stray brace`);
    }
  }
  // A Turkish text that is not Turkish would be a copy-paste of the English one; only
  // words that are the same in both languages may be identical.
  const identical = Object.keys(MESSAGES.en).filter(key => MESSAGES.en[key] === MESSAGES.tr[key]);
  assert.deepEqual(identical.sort(), ["kindVideo", "playerTitleFallback", "popupKindVideo", "videoTitleFallback"]);
});

test("every key the code asks for exists and no key is dead", () => {
  const { MESSAGES } = catalog();
  const used = new Map();
  const note = (key, where) => used.set(key, [...(used.get(key) || []), where]);
  for (const name of codeFiles) {
    for (const match of read(name).matchAll(/(?<![\w$])t\(\s*(["'`])([^"'`]*)\1/g)) note(match[2], name);
    // A call whose key is not a literal could not be checked: the only one allowed is the pages'
    // data-i18n loop, whose keys are the attributes of popup.html and welcome.html, checked below.
    for (const match of read(name).matchAll(/(?<![\w$.])t\(\s*(?!["'`)])/g)) {
      if (read(name).startsWith('t(node.getAttribute("data-i18n"))', match.index)) continue;
      assert.fail(`${name}: t() with a computed key at offset ${match.index}`);
    }
  }
  for (const page of pages) for (const match of read(page).matchAll(/data-i18n="([^"]+)"/g)) note(match[1], page);
  assert.ok(used.size > 150);
  for (const [key, where] of used) assert.ok(key in MESSAGES.en, `${key} (${where.join(", ")}) is not in the catalog`);
  assert.deepEqual(Object.keys(MESSAGES.en).filter(key => !used.has(key)), [], "catalog keys no code refers to");
});

test("i18n.js comes first in every context", () => {
  assert.match(read("service-worker.js"), /importScripts\(\s*"i18n\.js",\s*"codes\.js"/);
  const manifest = json("manifest.json");
  const isolated = manifest.content_scripts.find(entry => entry.js.includes("content.js"));
  assert.deepEqual(isolated.js, ["i18n.js", "relay-sha256.js", "content.js", "relay-content.js"]);
  for (const page of pages) {
    const html = read(page), script = `src="${page.replace(".html", ".js")}"`;
    assert.ok(html.indexOf('src="i18n.js"') > 0 && html.indexOf('src="i18n.js"') < html.indexOf(script), `${page} loads i18n.js before its script`);
    assert.match(html, /<html lang="en">/);
  }
  // The main-world script has no extension API and no text of its own.
  const mainWorld = manifest.content_scripts.find(entry => entry.world === "MAIN");
  assert.deepEqual(mainWorld.js, ["page-observer.js"]);
});

test("the page-world observer carries no user-visible text", () => {
  const code = read("page-observer.js");
  assert.doesNotMatch(code, /SSDownloadI18n|chrome\.i18n/);
  assert.doesNotMatch(code, /[^\x00-\x7f]/, "no non-ASCII character, so no Turkish text");
  // Everything it posts is protocol: channel names, capability flags, relations.
  const strings = [...code.matchAll(/"([^"\\\n]*)"/g)].map(match => match[1]);
  assert.deepEqual(strings.filter(value => /\s/.test(value) && !/^[#\w:., "=\/\-]+$/.test(value)), []);
});

// Lines that are only comments, or end in one, do not ship as UI text.
function codeOnly(text) {
  let inBlock = false;
  return text.split(/\r?\n/).map((line, index) => {
    let rest = line;
    if (inBlock) {
      const end = rest.indexOf("*/");
      if (end < 0) return { index, code: "" };
      inBlock = false; rest = rest.slice(end + 2);
    }
    if (/^\s*\/\//.test(rest)) return { index, code: "" };
    const trailing = rest.indexOf(" // ");
    if (trailing >= 0) rest = rest.slice(0, trailing);
    const start = rest.indexOf("/*");
    if (start >= 0 && rest.indexOf("*/", start) < 0) { inBlock = true; rest = rest.slice(0, start); }
    return { index, code: rest };
  });
}

test("no user-visible Turkish literal is left outside the catalog", () => {
  // The only Turkish the sources may keep is matching, never display: the wording of replies a
  // desktop sends in its own language (next to their English counterparts) and the button text
  // of the page's own "skip ad" control (page markup in either language).
  const appReplyMatchers = [/reklam\(\?:ı\|i\|ları\|lari\)\?\\s\*\(\?:geç\|atla\)\|skip\\s\+/,
    /İş bulunamadı\|ayrılamaz\|yetkilendirilmeli\|Deneysel tarayıcı aktarımı\|Job not found\|cannot be set aside\|must be authorized\|Experimental browser transfer/,
    /kuyruk kotası, zaman veya eşzamanlılık sınırında durduruldu\|queue quota, time, or concurrency limit/];
  const found = [];
  for (const name of [...codeFiles, ...pages]) {
    for (const { index, code } of codeOnly(read(name))) {
      if (!/[çğıöşüÇĞİÖŞÜ]/.test(code)) continue;
      if (appReplyMatchers.some(matcher => matcher.test(code))) continue;
      found.push(`${name}:${index + 1}: ${code.trim().slice(0, 100)}`);
    }
  }
  assert.deepEqual(found, []);
  // Texts without a single Turkish letter ("Kapat", "Algılama" style): no catalog text, in
  // either language, may appear verbatim in the sources, where it would be a hard-coded copy.
  const { MESSAGES, LANGUAGES } = catalog();
  const haystack = [...codeFiles, ...pages].map(name => [name, codeOnly(read(name)).map(line => line.code).join("\n")]);
  for (const language of LANGUAGES) {
    for (const [key, text] of Object.entries(MESSAGES[language])) {
      const needle = text.replace(/\{\d+\}.*$/s, "");
      if (needle.length < 4) continue;
      // A short lowercase word is also a protocol value ("audio", "video"); a short capitalised one
      // could be part of an identifier ("Detection"): it only counts as a whole literal.
      if (needle.length < 12 && !/^[A-ZÇĞİÖŞÜ]/.test(needle)) continue;
      const whole = new RegExp(`(["'\`])${needle.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\1`);
      for (const [name, code] of haystack) assert.ok(needle.length < 12 ? !whole.test(code) : !code.includes(needle), `${name} hard-codes the ${language} text of ${key}`);
    }
  }
});

test("the lookup substitutes placeholders and follows the language it is given", () => {
  const i18n = catalog();
  // No browser in the context: English, the safe default.
  assert.equal(i18n.language(), "en");
  assert.equal(i18n.t("popupHideOnHost", "site.example"), "Hide the button on site.example");
  assert.equal(i18n.setLanguage("tr"), true);
  assert.equal(i18n.setLanguage("tr"), false, "an unchanged language reports no change");
  assert.equal(i18n.t("popupHideOnHost", "site.example"), "site.example için düğmeyi gösterme");
  assert.equal(i18n.t("menuChooseSource", 3), "Kaynak seç (3)…");
  assert.equal(i18n.t("popupHideOnHost"), " için düğmeyi gösterme", "a missing argument is empty, like chrome.i18n");
  assert.equal(i18n.t("popupHideOnHost", "$& $1 {0}"), "$& $1 {0} için düğmeyi gösterme", "an argument is inserted literally");
  assert.equal(i18n.t("noSuchKey"), "noSuchKey", "an unknown key is visible, never silent");
  assert.equal(i18n.setLanguage("ja"), true, "an unsupported language forgets the app's choice");
  assert.equal(i18n.language(), "en");
  assert.equal(i18n.setLanguage(undefined), false);
});

test("before the app reports a language the browser's own decides, by its primary language", () => {
  const languageFor = uiLanguage => {
    const context = vm.createContext({ browser: { i18n: { getUILanguage: () => uiLanguage } } });
    vm.runInContext(read("i18n.js"), context, { filename: "i18n.js" });
    return [context.SSDownloadI18n.language(), context.SSDownloadI18n.direction()];
  };
  const { LANGUAGES } = catalog();
  for (const language of LANGUAGES) assert.equal(languageFor(language)[0], language);
  assert.deepEqual(languageFor("tr-TR"), ["tr", "ltr"]);
  assert.deepEqual(languageFor("en-GB"), ["en", "ltr"]);
  assert.deepEqual(languageFor("ja"), ["en", "ltr"], "a language the extension lacks gets English");
  assert.deepEqual(languageFor(""), ["en", "ltr"]);
  assert.deepEqual(languageFor("ar-EG"), ["ar", "rtl"]);
  assert.deepEqual(languageFor("pt-BR"), ["pt", "ltr"]);
  assert.deepEqual(languageFor("de-AT"), ["de", "ltr"]);
});
