"use strict";

const extensionApi = globalThis.browser || chrome;
// Reloading the unpacked extension invalidates the content script of every page that
// stayed open; `sendMessage` then throws synchronously ("Extension context
// invalidated") from whichever callback runs next. Failures are handled per call site,
// so an invalidated context only has to stop reporting.
function sendToExtension(message) {
  try {
    return extensionApi.runtime.sendMessage(message);
  } catch (_) {
    return undefined;
  }
}
// The in-page observer listens through `window`; the helper is defined here so the
// background message handler can reach it too, and it stays a no-op wherever the page
// exposes no `postMessage`, never an exception. The page-visible token correlates
// worker sessions; it is not an authentication boundary.
let observerToken = null;
function observerControl() {
  try {
    if (typeof window === "undefined" || typeof window.postMessage !== "function") return;
    window.postMessage({ channel: "ssdownload-observer-control", enabled: overlayEnabled, token: observerToken || "" }, "*");
  } catch (_) {}
}
function applyObserverToken(value) {
  if (typeof value !== "string" || !value || value.length > 128) return;
  observerToken = value;
  try { document.documentElement?.setAttribute?.("data-ssdownload-observer-token", value); } catch (_) {}
}
let scanTimer = 0;
let lastScan = "";
const playerIds = new WeakMap();
let playerSequence = 0;
const documentId = `${Date.now()}-${Math.random()}`;
// The in-page observer announces its capabilities once per document; the content
// script keeps that report so the background can ask for it again after a frame
// record it held was evicted.
let observationCapabilities = null;
const overlays = new Map();
let overlayEnabled = false;
let usageModes = null;
// Page-level entries are the desktop's to grant, never this document's to assume: without
// the negotiated capability the page tile is not drawn and no page handoff is submitted,
// exactly like the context menu that is not registered without it.
let siteEntries = false;
let observeLimitSent = false;
let usageModeError = "Masaüstü özellikleri henüz doğrulanmadı.";
let mediaRoots = [document];
// A player that loops, stays muted and started without the user asking is a preview or an
// ad on any site; the report carries those three facts so the background can decide. The
// own-tile shield keeps these phases from the page, so they are named once and the shield
// notes the same gesture where it stands in for this listener.
const USER_GESTURE_EVENTS = ["pointerdown", "keydown", "touchstart"];
let lastUserGestureAt = 0;
function noteUserGesture() { lastUserGestureAt = Date.now(); }
for (const event of USER_GESTURE_EVENTS) document.addEventListener(event, noteUserGesture, true);
const observedMediaRoots = new WeakSet();
function refreshMediaRoots() {
  const roots = [document];
  // querySelectorAll does not cross open shadow roots. Closed roots remain inaccessible.
  for (let index=0; index<roots.length; index++) {
    const root=roots[index];
    for (const host of root.querySelectorAll("*")) if(host.shadowRoot) roots.push(host.shadowRoot);
    if(root!==document && !observedMediaRoots.has(root)) {
      observedMediaRoots.add(root);
      root.addEventListener("play",()=>report("play"),true);
      root.addEventListener("loadstart",scheduleScan,true);
      root.addEventListener("loadedmetadata",scheduleScan,true);
      root.addEventListener("encrypted",()=>report("drm"),true);
    }
  }
  mediaRoots=roots;
}
function queryMediaRoots(selector) {
  return mediaRoots.flatMap(root=>[...root.querySelectorAll(selector)]);
}
// A player is not always an element: a JS player mounted into a container, an embed
// iframe or a player library script renders the video without ever creating a media
// element of its own. Those signals are reported in the same scan so the background can
// still offer the document (and every embed address) as a candidate; a page with none
// of them reports nothing extra.
const PLAYER_SIGNAL_PATTERN = /(?:^|[/.&?=_-])(?:player|jwplayer|hls|dplayer|plyr|videojs|preroll|embed|movie|video)(?:[/.&?=_-]|$)/i;
const PLAYER_CONTAINER_SELECTORS = ['[id*="player" i]','[class*="player" i]','[id*="video" i]','[class*="video" i]','[id*="movie" i]','[class*="movie" i]'];
function collectPlayerSignals() {
  const embeds = [], seen = new Set();
  for (const node of queryMediaRoots("iframe")) {
    if (embeds.length >= 4) break;
    const declared = node.getAttribute?.("src") || node.src || node.getAttribute?.("data-src") || node.getAttribute?.("data-lazy-src") || "";
    const url = absoluteUrl(String(declared));
    // An unloaded placeholder (about:blank, srcdoc, javascript:) and a MediaSource
    // address name no document to resolve, so neither is an embed candidate.
    if (!url || url.startsWith("blob:") || seen.has(url) || !PLAYER_SIGNAL_PATTERN.test(url)) continue;
    seen.add(url);
    embeds.push({url, title: String(node.title || node.getAttribute?.("title") || "").trim().slice(0, 120)});
  }
  let containers = false;
  for (const selector of PLAYER_CONTAINER_SELECTORS) {
    try { if (queryMediaRoots(selector).length) { containers = true; break; } } catch (_) { /* Selector form unsupported here. */ }
  }
  const playerScripts = queryMediaRoots("script[src]").some(node => PLAYER_SIGNAL_PATTERN.test(absoluteUrl(String(node.src || node.getAttribute?.("src") || ""))));
  return {embeds, containers, playerScripts};
}
function pageUrl() { return /^https?:/.test(location.href) ? location.href : /^https?:/.test(document.baseURI) ? document.baseURI : document.referrer; }
function pageTitle() {
  return document.querySelector?.('meta[property="og:title"]')?.content || document.title || "Video";
}
// The error report carries this document's own markup, serialized on demand at the moment a
// handoff asks for it. The extension's own affordances are not the page, and a script or
// style body would only pad the report, so both are dropped from the copy the report is
// taken from. The capture is bounded in bytes: an oversized page is cut between two
// characters, never inside one, and the marker states the cut.
const PAGE_HTML_LIMIT = 192 * 1024;
const PAGE_HTML_TRUNCATION = "<!-- ssdownload:page-html-truncated -->";
function truncatePageHtml(value) {
  if (!value) return "";
  const budget = PAGE_HTML_LIMIT - PAGE_HTML_TRUNCATION.length;
  let bytes = 0;
  for (let index = 0; index < value.length;) {
    const start = index;
    const code = value.charCodeAt(index);
    let width = code < 0x80 ? 1 : code < 0x800 ? 2 : 3;
    index += 1;
    if (code >= 0xd800 && code <= 0xdbff && index < value.length) {
      const low = value.charCodeAt(index);
      if (low >= 0xdc00 && low <= 0xdfff) { width = 4; index += 1; }
    }
    bytes += width;
    if (bytes > budget) return `${value.slice(0, start)}${PAGE_HTML_TRUNCATION}`;
  }
  return value;
}
function pageHtml() {
  try {
    // about:blank, a browser page and a document that answered 4xx/5xx have no page to
    // report, and neither has a frame that cannot be read: nothing is sent for them.
    if (!/^https?:/.test(location.href) || documentBlocked()) return "";
    const root = document.documentElement;
    if (!root || typeof root.cloneNode !== "function") return "";
    const clone = root.cloneNode(true);
    if (!clone) return "";
    if (typeof clone.querySelectorAll === "function") {
      for (const node of clone.querySelectorAll("script, style, noscript, template, [data-ssdownload-tile], [data-ssdownload-status]")) node.remove?.();
    }
    return truncatePageHtml(typeof clone.outerHTML === "string" ? clone.outerHTML : "");
  } catch (_) { return ""; }
}
// The content script relays its own events to the background; sendToExtension keeps an
// invalidated context silent and every field is bounded before it leaves the page.
function pageHost() { try { return new URL(pageUrl()).hostname; } catch (_) { return null; } }
function sendLogEvent(event, fields) {
  try {
    const entry = { event, level: fields.level || "info", outcome: fields.outcome || "ok" };
    if (fields.code) entry.code = fields.code;
    if (fields.host) entry.host = String(fields.host).slice(0, 190);
    if (fields.detail) entry.detail = String(fields.detail).replace(/[\u0000-\u001f\u007f]+/g, " ").slice(0, 200);
    const pending = sendToExtension({ channel: "ssdownload-content", event: "log", events: [entry] });
    if (pending && typeof pending.catch === "function") pending.catch(() => {});
  } catch (_) {}
}
function observeLimitFacts(list) {
  const facts = [`players=${list.length}`];
  const hidden = list.filter(player => player.visible === false && !player.active && !player.played).length;
  if (hidden) facts.push(`hidden=${hidden}`);
  const blob = list.filter(player => (player.urls || []).some(value => value.startsWith("blob:"))).length;
  if (blob) facts.push(`blobAmbiguous=${blob}`);
  if (list.every(player => player.visible === false)) facts.push("closedShadow=possible");
  facts.push("noCandidate");
  return facts.join(" ");
}
function playerIdentity(media) {
  const current = media.currentSrc || media.src || "";
  let identity = playerIds.get(media);
  if (!identity || (identity.current && current && identity.current !== current)) {
    identity = {id:String(++playerSequence), current, since:Date.now()}; playerIds.set(media, identity);
  }
  if (current) identity.current = current;
  return identity;
}
function playerTracks(media, playerId) {
  const tracks=[];
  for (const track of [...media.querySelectorAll("track[src]")].slice(0,32)) {
    const url=absoluteUrl(track.src || track.getAttribute?.("src"));
    if(!url || url.length>16384) continue;
    tracks.push({url, language:String(track.srclang || track.getAttribute?.("srclang") || "").slice(0,35),
      label:String(track.label || track.getAttribute?.("label") || "").slice(0,180),
      kind:String(track.kind || track.getAttribute?.("kind") || "subtitles").slice(0,32),
      isDefault:track.default === true || track.hasAttribute?.("default") === true, playerId});
  }
  return tracks;
}
function hasSkipAdControl(media) {
  let container=media.parentElement;
  for(let depth=0;container && depth<3;depth++,container=container.parentElement) {
    // Never borrow an advert control from a different player on the page.
    if(container===document.body || container===document.documentElement || container.querySelectorAll('video,audio').length>1) break;
    for(const control of container.querySelectorAll('button,[role="button"],[data-ad-skip],[class*="skip"],.videoAdUiSkipButton')) {
      const rect=control.getBoundingClientRect();
      if(rect.width<=0 || rect.height<=0 || control.checkVisibility?.()===false) continue;
      const text=`${control.textContent||""} ${control.getAttribute("aria-label")||""}`.slice(0,180);
      if(/reklam(?:ı|i|ları|lari)?\s*(?:geç|atla)|skip\s+(?:this\s+)?ads?\b/i.test(text)) return true;
    }
  }
  return false;
}

function players() {
  return [...queryMediaRoots("video, audio")].map(media => {
    const current = media.currentSrc || media.src || "";
    const identity=playerIdentity(media);
    const rect = media.getBoundingClientRect?.();
    const description = `${media.id || ""} ${media.className || ""} ${media.getAttribute("aria-label") || ""}`;
    const link=media.closest?.('a[href]');
    let promotionalLink=false;
    try { promotionalLink=!!link && (/sponsored/i.test(link.rel) || (/nofollow/i.test(link.rel) && link.target==="_blank" && new URL(link.href).origin!==new URL(pageUrl()).origin)); } catch (_) {}
    const adSignals=[];
    if (/(?:^|[\s_-])(ad|advert|advertisement|reklam)(?:$|[\s_-])/i.test(description)) adSignals.push("player-identity");
    if (media.closest?.('[data-ad], [data-ad-slot], [aria-label="Advertisement"], [aria-label="Reklam"]')) adSignals.push("ad-container");
    if (/(?:^|[/?&._-])(?:doubleclick|adserver|advertisement)(?:[/?&._=-]|$)/i.test(current)) adSignals.push("request-host");
    if (promotionalLink) adSignals.push("promotional-link");
    if (media.autoplay && media.loop && (media.muted || media.defaultMuted)) adSignals.push("playback-pattern");
    if (hasSkipAdControl(media)) adSignals.push("skip-control");
    const elementTitle=media.getAttribute("title") || media.getAttribute("data-title") || "";
    return {id:identity.id, urls:[current, ...[...media.querySelectorAll("source[src]")].map(s=>s.src)].map(absoluteUrl).filter(Boolean),
      title:elementTitle || pageTitle(), explicitTitle:!!elementTitle, titleSource:elementTitle?"element":"page",
      poster:media.poster || "", active:media.paused === false, played:media.currentTime>0 || media.readyState>=2,
      visible:!rect || (rect.width>=100 && rect.height>=60 && media.checkVisibility?.()!==false),
      since:identity.since, duration:Number.isFinite(media.duration)?media.duration:null,
      loop:media.loop===true, muted:media.muted===true || media.volume===0,
      userActivated:Date.now()-lastUserGestureAt<15000, adSignals,tracks:playerTracks(media,identity.id)};
  });
}

// Feedback is its own surface: a refusal must be visible even while discovery has
// withdrawn the page's media entries, so a message is never carried by the affordance
// (whose visibility is the switch's own business). The surface is non-interactive and
// lives only for its bounded window.
let statusNode = null;
let statusTimer = 0;
function showPageStatus(message, error) {
  const text = String(message || "").slice(0, 240);
  if (!text) return;
  if (!statusNode) {
    statusNode = document.createElement("div");
    statusNode.dataset.ssdownloadStatus = "";
    statusNode.style.cssText = "position:fixed;left:12px;top:12px;z-index:2147483647;background:#17633d;color:white;border:1px solid white;border-radius:6px;padding:7px 10px;font:13px sans-serif;pointer-events:none";
    document.documentElement.appendChild(statusNode);
  }
  statusNode.textContent = text;
  statusNode.title = text;
  statusNode.style.background = error ? "#b42318" : "#17633d";
  clearTimeout(statusTimer);
  statusTimer = setTimeout(() => {
    statusTimer = 0;
    statusNode?.remove();
    statusNode = null;
  }, 8000);
}

// Both kinds of tile submit through one path: the gesture is the page's own report, and a
// refusal is answered on the document's status surface rather than on the tile's label. The
// request is read from the document at the moment of the gesture, inside the guard that
// answers a refusal, so nothing the tile carried can break the submission it started.
async function handOver(buildRequest) {
  report("play");
  try {
    const reply = await sendToExtension(buildRequest());
    if (reply && reply.error) showPageStatus(String(reply.error), true);
  } catch (error) { try { showPageStatus("Eklenti bağlantısı kurulamadı", true); } catch (_) {} }
}

// The affordance is not the page's surface. A page keeps its own handlers on `window` and
// `document`, and propagation reaches them before a listener on the tile itself is ever
// called: the player opens a pop-under from the user's gesture, or it swallows the event so
// the tile never sees the click at all. These listeners are added while this script runs at
// document_start — the document executes no script of its own until this whole script has
// finished — so the shield is the first listener of the capture path: an event aimed at one
// of our tiles stops there, and the tile's own submission runs from that same point, once
// per user gesture.
//
// A tile is recognised by object identity alone (`ownTiles`), so nothing the page writes —
// text, class, attribute — can make one of its own controls pass for the affordance, and
// nothing about the page is filtered: an event aimed anywhere else is dispatched exactly as
// the page wrote it, and the browser's own default actions (focus, the pointer's active
// state, the click a keyboard activation generates) are never cancelled.
const ownTiles = new WeakMap();
// The phases a pop-under is opened from, the activation itself, and the keys a tile acts on:
// a page handler that cancels those keys would suppress the click the browser generates.
const OWN_TILE_TYPES = ["pointerdown", "mousedown", "pointerup", "mouseup", "auxclick", "contextmenu", "click", "touchstart", "touchend", "keydown", "keyup"];
const OWN_TILE_KEYS = new Set(["Enter", " "]);

// The tile this event belongs to, or null for the page's own events. Identity walks the
// target's ancestry, so a click that lands on the tile's glyph or label reaches the tile.
function ownTileTarget(event) {
  for (let node = event.target; node; node = node.parentNode) if (ownTiles.has(node)) return node;
  return null;
}

// Every phase a tile is activated through is resolved here, before the page can see it. The
// phases this swallows are painted here too — a listener on the tile itself would never run
// for them — while the phases the page may keep (enter, leave, cancel) stay on the tile.
function ownTileEvent(event) {
  const tile = ownTileTarget(event);
  if (!tile) return;
  if (USER_GESTURE_EVENTS.includes(event.type)) noteUserGesture();
  if (event.type === "click") {
    // One submission per user gesture: the click the browser generated for a real press or a
    // keyboard activation, on a tile that still offers its action.
    if (event.isTrusted === true && event.button === 0 && overlayEnabled && !tile.hidden && !tile.disabled) ownTiles.get(tile).activate?.();
  } else if (event.type === "keydown" || event.type === "keyup") {
    // Only the keys a tile acts on are the tile's; every other key stays the page's event.
    if (!OWN_TILE_KEYS.has(event.key)) return;
    paintTile(tile, event.type === "keydown" ? "pressed" : "idle");
  } else if (event.type === "pointerdown") paintTile(tile, "pressed");
  else if (event.type === "pointerup") paintTile(tile, "hover");
  event.stopImmediatePropagation();
}

// A document without a window is a bare test double, never a page the extension ships to.
if (typeof window !== "undefined") {
  for (const type of OWN_TILE_TYPES) window.addEventListener(type, ownTileEvent, true);
}

// The affordance is the application icon: a blue gradient tile with the white arrow over
// the turquoise tray. A page styles `button` and `svg` however it likes, so every property
// the look depends on is an inline declaration and `:hover`, `:active` and `:focus-visible`
// come from listeners.
const TILE_FACES = {
  idle: { background: "linear-gradient(135deg,#2369E8,#123E9B)", boxShadow: "inset 0 1px 0 rgba(255,255,255,.28),0 2px 6px rgba(9,36,93,.35)", cursor: "pointer", opacity: "1" },
  hover: { background: "linear-gradient(135deg,#2E76F0,#1648B0)", boxShadow: "inset 0 1px 0 rgba(255,255,255,.32),0 3px 10px rgba(9,36,93,.45)", cursor: "pointer", opacity: "1" },
  pressed: { background: "linear-gradient(135deg,#1B57CC,#0F347F)", boxShadow: "inset 0 2px 5px rgba(4,18,48,.45)", cursor: "pointer", opacity: "1" },
  disabled: { background: "linear-gradient(135deg,#8AA6DE,#6B84BE)", boxShadow: "none", cursor: "default", opacity: ".72" }
};
const TILE_CSS = "appearance:none;-webkit-appearance:none;position:fixed;z-index:2147483647;display:inline-flex;align-items:center;gap:7px;box-sizing:border-box;margin:0;padding:7px 12px 7px 10px;border:1px solid rgba(255,255,255,.55);border-radius:10px;background:linear-gradient(135deg,#2369E8,#123E9B);color:#fff;-webkit-text-fill-color:#fff;font:600 13px/1.2 system-ui,sans-serif;text-align:left;text-decoration:none;text-transform:none;text-indent:0;text-shadow:none;letter-spacing:normal;white-space:nowrap;direction:ltr;unicode-bidi:isolate;width:auto;height:auto;min-width:0;min-height:0;max-width:none;max-height:none;overflow:visible;float:none;transform:none;visibility:visible;vertical-align:middle;user-select:none;-webkit-user-select:none;cursor:pointer;pointer-events:auto;box-shadow:inset 0 1px 0 rgba(255,255,255,.28),0 2px 6px rgba(9,36,93,.35);transition:background .12s ease,box-shadow .12s ease;outline:none;outline-offset:0";
const SVG_NAMESPACE = "http://www.w3.org/2000/svg";
const tileLabels = new WeakMap();
let tileSequence = 0;

function paintTile(button, state) {
  const face = TILE_FACES[state] || TILE_FACES.idle;
  button.style.background = face.background;
  button.style.boxShadow = face.boxShadow;
  button.style.cursor = face.cursor;
  button.style.opacity = face.opacity;
}

// The tile's own inline `display` outranks the `hidden` attribute, so withdrawal is restated inline.
function hideTile(button, hidden) {
  button.hidden = hidden;
  button.style.display = hidden ? "none" : "inline-flex";
}

// Arrow and tray paint from inline styles so a page rule such as `svg path { fill: currentColor }`
// cannot repaint them; the tray gradient id is unique per tile because it lives in the page's document.
function downloadGlyph() {
  const element = name => document.createElementNS(SVG_NAMESPACE, name);
  const gradientId = `ssdownload-tray-${++tileSequence}`;
  const glyph = element("svg");
  glyph.setAttribute("viewBox", "0 0 24 24");
  glyph.setAttribute("aria-hidden", "true");
  glyph.setAttribute("focusable", "false");
  glyph.style.cssText = "width:20px;height:20px;display:block;flex:0 0 auto;position:static;margin:0;padding:0;overflow:visible;pointer-events:none";
  const defs = element("defs");
  const gradient = element("linearGradient");
  gradient.setAttribute("id", gradientId);
  gradient.setAttribute("gradientUnits", "userSpaceOnUse");
  gradient.setAttribute("x1", "5.4");
  gradient.setAttribute("y1", "15.5");
  gradient.setAttribute("x2", "18.6");
  gradient.setAttribute("y2", "19.9");
  for (const [offset, color] of [["0", "#27D4C1"], ["1", "#16A5B9"]]) {
    const stop = element("stop");
    stop.setAttribute("offset", offset);
    stop.setAttribute("stop-color", color);
    gradient.appendChild(stop);
  }
  defs.appendChild(gradient);
  const tray = element("path");
  tray.setAttribute("d", "M4.4 16.4a1.2 1.2 0 0 1 1.2-1.2h3.4l2.5 2.2q.5.55 1 0l2.5-2.2h3.4a1.2 1.2 0 0 1 1.2 1.2v1.8a2.2 2.2 0 0 1-2.2 2.2H6.6a2.2 2.2 0 0 1-2.2-2.2z");
  tray.style.cssText = `fill:url(#${gradientId});stroke:none`;
  const arrow = element("path");
  arrow.setAttribute("d", "M12 5.2V11.4M6.4 11 12 15.8 17.4 11");
  arrow.style.cssText = "fill:none;stroke:#fff;stroke-width:3.3;stroke-linecap:round;stroke-linejoin:round";
  const accent = element("circle");
  accent.setAttribute("cx", "16.3");
  accent.setAttribute("cy", "17.5");
  accent.setAttribute("r", "0.9");
  accent.style.cssText = "fill:#E8FFFC;stroke:none";
  glyph.appendChild(defs); glyph.appendChild(tray); glyph.appendChild(arrow); glyph.appendChild(accent);
  return glyph;
}

// Both kinds of tile are the same affordance: the application icon, one label child a
// later pass rewrites without losing the glyph, the page's own hover, pressed, keyboard
// and focus states, and the submission the shield calls for the user's gesture.
function createTile(activate) {
  const button = document.createElement("button");
  button.type = "button";
  // The affordance is the extension's, not the page's: the marker lets a report of the
  // page's own markup leave our controls out of the copy it is taken from.
  button.dataset.ssdownloadTile = "";
  button.style.cssText = TILE_CSS;
  // Two children, so a later pass rewrites the label without losing the glyph.
  const labelNode = document.createElement("span");
  labelNode.style.cssText = "display:block;flex:0 0 auto;margin:0;padding:0;color:#fff;-webkit-text-fill-color:#fff;font:inherit;line-height:1.2;white-space:nowrap;text-align:left;text-transform:none;letter-spacing:normal;pointer-events:none";
  button.appendChild(downloadGlyph());
  button.appendChild(labelNode);
  tileLabels.set(button, labelNode);
  ownTiles.set(button, { activate });
  // Enter, leave and cancel are the phases the page may keep, so they stay on the tile; the
  // press, the release and the activation keys are the shield's own, and it paints them.
  button.addEventListener("pointerenter", () => paintTile(button, "hover"));
  button.addEventListener("pointerleave", () => paintTile(button, "idle"));
  button.addEventListener("pointercancel", () => paintTile(button, "idle"));
  button.addEventListener("blur", () => { paintTile(button, button.disabled ? "disabled" : "idle"); button.style.outline = ""; button.style.outlineOffset = ""; });
  // An inline style cannot express `:focus-visible`, so the keyboard ring comes from the focus event.
  button.addEventListener("focus", () => {
    if (!button.matches(":focus-visible")) return;
    button.style.outline = "3px solid #7FACFF";
    button.style.outlineOffset = "2px";
  });
  return button;
}

// A document that renders its player without a media element of its own (a JS player
// mounted into a container, a player library script, an embed iframe) keeps one
// page-level tile: the same affordance and the same click path as a media-anchored
// tile, carrying the product's own wording for a page candidate and never its address.
// It hands the document itself over, exactly like the context menu's page entry.
const PAGE_TILE_LABEL = "Sayfayı SSDownload ile indir";
const PAGE_TILE_MARGIN = 8;
let pageOverlay = null;

// The detected player container's own box, so the tile stands on the player instead of
// the viewport corner. The selector set is the one behind the `containers` signal, and
// the largest visible match is the film player rather than a sidebar list sharing its
// name. Only numbers leave this function: the scan message could not carry an element.
function playerContainerBox() {
  let best = null;
  for (const selector of PLAYER_CONTAINER_SELECTORS) {
    let nodes = [];
    try { nodes = queryMediaRoots(selector); } catch (_) { continue; }
    for (const node of nodes) {
      const rect = node.getBoundingClientRect?.();
      if (!rect || rect.width < 1 || rect.height < 1 || node.checkVisibility?.() === false) continue;
      const area = rect.width * rect.height;
      if (!best || area > best.area) best = { rect, area };
    }
  }
  return best?.rect || null;
}

// A document that answered 4xx/5xx has no content to resolve, so it is no page
// candidate at all. Chrome reports the status on the document's own navigation timing
// entry; a value this page cannot read is not a block.
function documentBlocked() {
  try {
    const status = Number(globalThis.performance?.getEntriesByType?.("navigation")?.[0]?.responseStatus);
    return Number.isFinite(status) && status >= 400;
  } catch (_) { return false; }
}

// A frame is another document's surface: that document draws the player's own controls
// there, and its own content script stands the tile of that player on the same corner
// this document would anchor to. A tile of this document is painted above the frame it
// covers, so anchoring there would take the click that belongs to the embedded document.
// Only the frame's box is read; what the embedded document draws inside it is never
// guessed, and a frame this document cannot show is no surface at all.
const EMBED_FRAME_SELECTOR = "iframe, frame, embed, object";
function frameOccupies(box) {
  for (const node of queryMediaRoots(EMBED_FRAME_SELECTOR)) {
    const rect = node.getBoundingClientRect?.();
    if (!rect || rect.width < 1 || rect.height < 1 || node.checkVisibility?.() === false) continue;
    if (rect.left < box.right && rect.right > box.left && rect.top < box.bottom && rect.bottom > box.top) return true;
  }
  return false;
}

// The tile anchors to the player container while that container is on screen and the spot
// is this document's own surface, and to the viewport's top-right corner otherwise, both
// with an 8 px margin. The container's corner is where the embedded document keeps its own
// affordance, so a document that hosts a frame there keeps its offer on the corner of the
// screen instead: the two tiles never fight over one click, and the page candidate itself
// is unchanged.
function placePageTile(button) {
  const width = button.offsetWidth || 0, height = button.offsetHeight || 0;
  const rect = playerContainerBox();
  if (rect && rect.bottom > PAGE_TILE_MARGIN && rect.top < innerHeight) {
    const left = Math.max(0, rect.left + PAGE_TILE_MARGIN), top = Math.max(0, rect.top + PAGE_TILE_MARGIN);
    if (!frameOccupies({ left, top, right: left + width, bottom: top + height })) {
      button.style.right = "auto";
      button.style.left = `${left}px`;
      button.style.top = `${top}px`;
      return;
    }
  }
  button.style.left = "auto";
  button.style.top = `${PAGE_TILE_MARGIN}px`;
  button.style.right = `${PAGE_TILE_MARGIN}px`;
}

// The page tile is the tab's own offer, and the tab is the top-level document: a frame is
// another document's surface, so a document that stands inside one keeps the media tiles
// of its own player and never a second offer for a page that is not its own. The manifest
// injects this script into every frame, so this is the one place the offer is decided.
function topLevelDocument() {
  if (typeof window === "undefined") return true;
  try { return window.top === window.self; } catch (_) { return true; }
}

// The page-level tile exists only while the desktop offers site entries and this document
// has no media element the background would turn into a candidate of its own: an empty
// placeholder that renders no player yet leaves the page address as the candidate, while a
// loaded, playing or preview element is a producer and keeps its own tile. A blocked
// document has no content to resolve at all, and a preview-only document is no candidate
// either; nothing about the page's candidate is decided silently here.
function updatePageOverlay(descriptions) {
  if (!topLevelDocument()) {
    if (pageOverlay) hideTile(pageOverlay, true);
    return;
  }
  const producers = descriptions.filter(player => (player.urls.length || player.active || player.played) && !(player.visible === false && !player.active && !player.played));
  const signals = producers.length ? null : collectPlayerSignals();
  const signalled = !!signals && (signals.embeds.length > 0 || signals.containers === true || signals.playerScripts === true);
  if (!signalled || siteEntries !== true || usageModes?.video !== true || documentBlocked()) {
    if (pageOverlay) hideTile(pageOverlay, true);
    return;
  }
  if (!pageOverlay) {
    // The document's own name travels with the gesture: the page candidate is named from
    // this report, so a message without it would leave the candidate to the tab name.
    pageOverlay = createTile(() => handOver(() => ({channel:"ssdownload-content",event:"download-page",mediaKind:"video",documentId,pageUrl:pageUrl(),pageTitle:pageTitle(),players:players(),sources:collectSources(),playerSignals:collectPlayerSignals()})));
    document.documentElement.appendChild(pageOverlay);
  }
  tileLabels.get(pageOverlay).textContent = PAGE_TILE_LABEL;
  pageOverlay.title = usageModeError || "";
  hideTile(pageOverlay, false);
  placePageTile(pageOverlay);
}

let overlayFailures = 0;
function updateOverlays() {
  // Every caller runs through here: a page that refuses any DOM step gets the
  // first failure stated on the page's status surface and later ones are only
  // counted, so no overlay failure can become an unhandled rejection anywhere.
  try { paintOverlays(); }
  catch (_) {
    if (++overlayFailures === 1) { try { showPageStatus("SSDownload sayfa arayüzü güncellenemedi", true); } catch (_) {} }
  }
}
function paintOverlays() {
  if (!document.createElement) return;
  if (!overlayEnabled || !usageModes || (!usageModes.video && !usageModes.audio)) {
    for (const button of overlays.values()) hideTile(button, true);
    if (pageOverlay) hideTile(pageOverlay, true);
    return;
  }
  const media = [...queryMediaRoots("video, audio")].filter(node=>node.tagName==="AUDIO"?usageModes.audio:(usageModes.video||usageModes.audio));
  const descriptions=players();
  for (const [node, button] of overlays) if (!media.includes(node)) { button.remove(); overlays.delete(node); }
  for (const node of media) {
    const mediaKind=node.tagName==="AUDIO"||!usageModes.video?"audio":"video";
    let button = overlays.get(node);
    if (!button) {
      button = createTile(() => handOver(() => ({channel:"ssdownload-content",event:"download-video",mediaKind:node.tagName==="AUDIO"||!usageModes?.video?"audio":"video",playerId:playerIds.get(node)?.id,documentId,pageUrl:pageUrl(),players:players(),sources:collectSources()})));
      document.documentElement.appendChild(button); overlays.set(node, button);
    }
    // Only the label child is rewritten: `textContent` on the tile would drop the glyph.
    tileLabels.get(button).textContent=mediaKind==="audio"?"Bu sesi indir":"Bu videoyu indir";
    button.title=usageModeError||"";
    const rect = node.getBoundingClientRect();
    const description=descriptions.find(p=>p.id===playerIds.get(node)?.id);
    const minimumHeight=mediaKind==="audio"?24:60;
    // An element with no address of its own, never started, is no source producer: its tile
    // could only fail, and a document with recognised player markup keeps the page-level tile
    // for that case instead.
    const withdrawn = (description && !description.urls.length && !description.active && !description.played) || rect.width < 100 || rect.height < minimumHeight || rect.bottom < 0 || rect.top > innerHeight
      || node.checkVisibility?.()===false || description?.adSignals.length>=2;
    hideTile(button, withdrawn);
    if (button.disabled) paintTile(button, "disabled");
    button.style.left = `${Math.max(0, rect.left + 8)}px`;
    button.style.top = `${Math.max(0, rect.top + 8)}px`;
  }
  updatePageOverlay(descriptions);
}

function absoluteUrl(value) {
  if (!value) return "";
  if (value.startsWith("blob:")) return value;
  try {
    const parsed = new URL(value, document.baseURI);
    return parsed.protocol === "http:" || parsed.protocol === "https:" ? parsed.href : "";
  } catch (_) { return ""; }
}

function collectSources() {
  const found = new Map();
  const add = (url, contentType, explicitKind, playerId = null, tracks = []) => {
    const absolute = explicitKind === "webrtc" ? String(url) : absoluteUrl(url);
    if (!absolute || absolute.length>16384) return;
    const kind = explicitKind || (absolute.startsWith("blob:") ? "blob" : "url");
    const key=`${kind}\n${playerId || ""}\n${absolute}`;
    found.set(key, {
      url: absolute,
      contentType: String(contentType || found.get(key)?.contentType || ""),
      kind,
      pageUrl: pageUrl(),
      playerId,
      tracks
    });
  };

  for (const media of queryMediaRoots("video, audio")) {
    const identity=playerIdentity(media);
    const tracks=playerTracks(media,identity.id);
    if (media.srcObject) add(`webrtc:${location.origin}`, "", "webrtc", identity.id, tracks);
    add(media.currentSrc || media.src, media.getAttribute("type") || (media.tagName === "AUDIO" ? "audio/unknown" : "video/unknown"), null, identity.id, tracks);
    for (const source of media.querySelectorAll("source[src]")) add(source.src, source.type || (media.tagName === "AUDIO" ? "audio/unknown" : "video/unknown"), null, identity.id, tracks);
  }
  for (const source of queryMediaRoots("source[src]")) if (!source.closest?.("video,audio")) add(source.src, source.type || "");
  for (const link of document.querySelectorAll('link[rel="preload"][href], link[rel="alternate"][href]')) {
    const type = link.getAttribute("type") || "";
    if (type.startsWith("video/") || type.startsWith("audio/") || /mpegurl|dash\+xml/i.test(type)) add(link.href, type);
  }
  for (const link of document.querySelectorAll('a[download][href]')) {
    const value=absoluteUrl(link.href);
    if(value) found.set(`file\n${value}`,{url:value,kind:"file",contentType:"",filename:link.download||"",pageUrl:pageUrl()});
    if(found.size>=100) break;
  }
  return Array.from(found.values()).slice(0, 100);
}

function report(event) {
  try {
    refreshMediaRoots();
    const sources = collectSources();
    const currentPlayers = players();
    const playerSignals = collectPlayerSignals();
    const signature = JSON.stringify([sources, currentPlayers, location.href, playerSignals]);
    if (event === "scan" && signature === lastScan) return;
    lastScan = signature;
    if (!observeLimitSent && currentPlayers.length && currentPlayers.every(player => player.visible === false && !player.active && !player.played)) {
      observeLimitSent = true;
      sendLogEvent("ext.observe.limit", { level: "warn", outcome: "failed", code: "SSD-EXT-010",
        host: pageHost(), detail: observeLimitFacts(currentPlayers) });
    }
    const result = sendToExtension({
      channel: "ssdownload-content",
      event,
      sources, players:currentPlayers, documentId, pageUrl:pageUrl(), pageTitle:pageTitle(), playerSignals
    });
    if (result && typeof result.catch === "function") result.catch(() => {});
  } catch (_) {}
  updateOverlays();
}

function scheduleScan() {
  clearTimeout(scanTimer);
  scanTimer = setTimeout(() => report("scan"), 350);
}

document.addEventListener("play", () => report("play"), true);
document.addEventListener("loadstart", scheduleScan, true);
document.addEventListener("loadedmetadata", scheduleScan, true);
document.addEventListener("encrypted", () => report("drm"), true);

new MutationObserver((mutations) => {
  if (mutations.some((mutation) => (mutation.type === "childList" && [...mutation.addedNodes, ...mutation.removedNodes].some(node => node.nodeType === 1 && (node.matches("video,audio,source,track,a[download],link[rel]") || node.querySelector("video,audio,source,track,a[download],link[rel]")))) || mutation.type === "attributes")) scheduleScan();
}).observe(document.documentElement || document, {
  childList: true,
  subtree: true,
  attributes: true,
  attributeFilter: ["src", "href", "download"]
});

try {
  const observer = new PerformanceObserver((list) => {
    const sources = [];
    for (const entry of list.getEntries()) {
      if (["video","audio","fetch","xmlhttprequest"].includes(entry.initiatorType)) {
        const url = absoluteUrl(entry.name);
        if (url) sources.push({ url, contentType: "", kind: "url", pageUrl: pageUrl() });
      }
    }
    if (sources.length) sendToExtension({ channel: "ssdownload-content", event: "resource", sources })?.catch(() => {});
  });
  observer.observe({ type: "resource", buffered: true });
} catch (_) {}

extensionApi.runtime.onMessage.addListener((message, sender, sendResponse) => {
  // The worker asks for this document's markup at the moment of a handoff. A document
  // with nothing to report (about:blank, a blocked frame, a page without a document
  // element) answers with nothing, so the request goes without the field at all.
  if(message && message.channel==="ssdownload-background" && message.command==="page-html") {
    const html=pageHtml();
    if(sendResponse) sendResponse(html?{html}:{});
    return;
  }
  // The desktop rejected this document as a whole page (a feed, a search page, an
  // empty playlist). Stop offering the in-page button here so the same address is
  // not retried by every click; a navigation resets this because the script runs
  // per document.
  if(message && message.channel==="ssdownload-background" && message.command==="no-single-media") {
    overlayEnabled=false;
    for (const button of overlays.values()) button.remove();
    overlays.clear();
    // The document-level tile is the same offer for the rejected page address.
    if (pageOverlay) hideTile(pageOverlay, true);
    observerControl();
  }
  if(message && message.channel==="ssdownload-background" && message.command==="observation-capabilities-refresh") {
    // The worker lost the frame record that held them; the page world announces its
    // capabilities once per document, so this re-send is the only recovery path.
    if (observationCapabilities) sendToExtension({channel:"ssdownload-content",event:"observation-capabilities",documentId,pageUrl:pageUrl(),capabilities:observationCapabilities})?.catch(()=>{});
  }
  if(message && message.channel==="ssdownload-background" && message.command==="status") {
    showPageStatus(message.message, message.error===true);
  }
  if(message && message.channel==="ssdownload-background" && message.command==="usage-modes") {
    usageModes=message.status==="ready"&&message.usageModes?{video:message.usageModes.video===true,file:message.usageModes.file===true,audio:message.usageModes.audio===true}:null;
    siteEntries=message.status==="ready"&&message.siteEntries===true;
    usageModeError=String(message.error||"").slice(0,240);
    // A restarted worker broadcasts a fresh token; re-delivering it here keeps the
    // legitimate enable/disable flow working across worker sessions.
    applyObserverToken(message.observerToken);
    observerControl();
    updateOverlays();
  }
});
async function refreshUsageModes() {
  try {
    const result=await sendToExtension({channel:"ssdownload-content",event:"usage-modes"});
    usageModes=result?.status==="ready"&&result.usageModes?{video:result.usageModes.video===true,file:result.usageModes.file===true,audio:result.usageModes.audio===true}:null;
    siteEntries=result?.status==="ready"&&result.siteEntries===true;
    usageModeError=String(result?.error||"").slice(0,240);
    applyObserverToken(result?.observerToken);
    observerControl();
  } catch(error) { usageModes=null;siteEntries=false;usageModeError=String(error?.message||error).slice(0,240); }
  updateOverlays();
}

sendLogEvent("ext.content.session", { level: "info", outcome: "ok", host: pageHost() });
report("scan");
refreshUsageModes();
if (extensionApi.storage?.local) {
  // A site the user hid in the popup keeps discovery but shows no overlay button.
  const siteHidden=hosts=>Array.isArray(hosts)&&hosts.includes(location.hostname);
  let detectionOn=true, hostHidden=false;
  Promise.resolve(extensionApi.storage.local.get({detectionEnabled:true,hiddenHosts:[]})).then(saved=>{detectionOn=saved.detectionEnabled!==false;hostHidden=siteHidden(saved.hiddenHosts);overlayEnabled=detectionOn&&!hostHidden;if(overlayEnabled){lastScan="";report("scan");}observerControl();updateOverlays();}).catch(()=>{});
  extensionApi.storage.onChanged?.addListener((changes,area)=>{
    if(area!=="local" || (!changes.detectionEnabled && !changes.hiddenHosts)) return;
    if(changes.detectionEnabled) detectionOn=changes.detectionEnabled.newValue!==false;
    if(changes.hiddenHosts) hostHidden=siteHidden(changes.hiddenHosts.newValue);
    overlayEnabled=detectionOn&&!hostHidden;
    // Switching discovery back on must bring the page's sources back: the worker has
    // cleared its discoveries, and a paused player would otherwise never be reported
    // again until the page changed. The captured context rule (only observed request
    // context travels) is unchanged.
    if(overlayEnabled) { lastScan=""; report("scan"); }
    observerControl();
    updateOverlays();
  });
}
if (typeof window !== "undefined") {
  window.addEventListener("message",event=>{
    if(event.source!==window || !event.data || !overlayEnabled) return;
    if(event.data.channel==="ssdownload-observer-ready") {
      observerControl();
      const capabilities=event.data.capabilities;
      if(capabilities && typeof capabilities==="object") {
        // Kept so the background can ask for them again when an eviction removed
        // the frame record that held them: the page world sends these once.
        observationCapabilities=capabilities;
        sendToExtension({channel:"ssdownload-content",event:"observation-capabilities",documentId,pageUrl:pageUrl(),capabilities})?.catch(()=>{});
      }
      return;
    }
    const data=event.data;
    if(data.channel!=="ssdownload-observed-manifest" || typeof data.url!=="string" || data.url.length>16384 || !/^https?:/.test(data.url)) return;
    const roles=new Set(["variant","audio","init","segment"]);
    const relations=Array.isArray(data.relations)?data.relations.slice(0,200).flatMap(relation=>{
      if(!relation || !roles.has(relation.role) || typeof relation.url!=="string" || relation.url.length>16384 || !/^https?:/.test(relation.url)) return [];
      return [{url:relation.url,role:relation.role}];
    }):[];
    const children=Array.isArray(data.children)?data.children.filter(u=>typeof u==="string" && u.length<=16384 && /^https?:/.test(u)).slice(0,200):[];
    if(JSON.stringify(relations).length>32768 || JSON.stringify(children).length>32768) return;
    const kind=data.kind==="dash"?"dash":"hls";
    sendToExtension({channel:"ssdownload-content",event:"manifest",documentId,pageUrl:pageUrl(),
      manifest:{url:data.url,kind,relations,children},sources:[{url:data.url,contentType:kind==="dash"?"application/dash+xml":"application/vnd.apple.mpegurl",pageUrl:pageUrl()}]})?.catch(()=>{});
  });
  observerControl();
  window.addEventListener("scroll", updateOverlays, true);
  window.addEventListener("resize", updateOverlays);
  window.addEventListener("popstate", scheduleScan);
  // Covers history.pushState and MSE source changes without patching page APIs.
  setInterval(() => report("scan"), 1500);
}
