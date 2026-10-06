"use strict";

const api = globalThis.browser || chrome;
const isPromiseApi = typeof globalThis.browser !== "undefined";
const HOST = "com.ssdownload.desktop";
const MAX_MEDIA_PER_TAB = 200;
const REQUEST_CONTEXT_MAX_AGE = 120000;
const CAPABILITY_CACHE_AGE = 60000;
// The page markup a handoff may carry for the error report, bounded in bytes: a truncated
// capture says so in place, and neither side of the wire ever sees more than the bound.
const PAGE_HTML_LIMIT = 192 * 1024;
const PAGE_HTML_TRUNCATION = "<!-- ssdownload:page-html-truncated -->";
const mediaByTab = new Map();
const tabStatus = new Map();
const playTimes = new Map();
const tickets = new Map();
// launch key -> { id, tabId, url, at }; persisted so a service-worker restart
// reuses the desktop launch the user is still looking at instead of re-analysing.
const launches = new Map();
const frames = new Map();
const requestContexts = new Map();
const frameResponses = new Map();
const manifestLinks = new Map();
const manifestTasks = new Map();
const observationLimits = new Set();
let manifestActive = 0;
let detectionEnabled = true;
let capabilityState = { status:"unknown", usageModes:null, sourceRefreshJobs:[], checkedAt:0, error:"Masaüstü özellikleri henüz doğrulanmadı." };
let capabilityTask = null;
// Signature of the last capability state broadcast to the tabs; a refresh that
// computes the same signature changes nothing for them and is not re-broadcast.
let capabilitySignature = null;
// Per-worker correlation token, not page authentication: the MAIN-world bridge
// is page-visible. Privacy and submission authorization are enforced here.
const observerToken = (() => {
  try {
    const bytes = new Uint8Array(16);
    crypto.getRandomValues(bytes);
    return Array.from(bytes, byte => byte.toString(16).padStart(2, "0")).join("");
  } catch (_) { return null; }
})();
// Observation capabilities per frame key, held for a record an eviction removed:
// the page world sends them once per document, and this copy restores them when
// the frame's next report recreates its record.
const pendingCapabilities = new Map();

// The frame map is capped at 50 and the least recently reporting frame goes first;
// a still-live document that lost its record is asked for its observation
// capabilities again, because the page world only ever sends them once.
function evictOldestFrames() {
  const evicted = [];
  while (frames.size > 50) {
    let oldest = null, oldestAt = Infinity;
    for (const [key, frame] of frames) {
      const at = Number(frame.updatedAt) || 0;
      if (at < oldestAt) { oldestAt = at; oldest = key; }
    }
    frames.delete(oldest);
    evicted.push(oldest);
  }
  for (const key of evicted) {
    if (!api.tabs?.sendMessage) continue;
    const [tabId, frameId] = key.split("\n");
    invoke(api.tabs, "sendMessage", Number(tabId),
      { channel: "ssdownload-background", command: "observation-capabilities-refresh" },
      { frameId: Number(frameId) }).catch(() => {});
  }
}

function invoke(target, method, ...args) {
  if (isPromiseApi) return target[method](...args);
  return new Promise((resolve, reject) => {
    target[method](...args, (value) => {
      const error = api.runtime.lastError;
      if (error) reject(new Error(error.message));
      else resolve(value);
    });
  });
}

// The toolbar action is the whole interface: one click flips the global
// switch and nothing else. No popup, sidebar or options page replaces it, so
// the badge and tooltip are the only place the state can be read.
const TOGGLE_ON = { text: "ON", color: "#16803a", title: "SSDownload: açık — kapatmak için tıklayın" };
const TOGGLE_OFF = { text: "OFF", color: "#5f6368", title: "SSDownload: kapalı — açmak için tıklayın" };
// Completed downloads since the popup was last opened; the badge shows the count.
let pendingCompletions = 0, completedBaseline = null, lastHandoffAt = 0;
const COMPLETION_POLL_WINDOW = 3 * 60 * 60 * 1000;
function actionState() {
  if (detectionEnabled && pendingCompletions > 0) {
    return { text: String(Math.min(pendingCompletions, 99)), color: "#34c759", title: `SSDownload: ${pendingCompletions} indirme tamamlandı` };
  }
  return detectionEnabled ? TOGGLE_ON : TOGGLE_OFF;
}
function noteCompletedJobs(count) {
  if (!Number.isInteger(count) || count < 0) return;
  if (completedBaseline === null || count < completedBaseline) { completedBaseline = count; return; }
  const pending = count - completedBaseline;
  if (pending !== pendingCompletions) { pendingCompletions = pending; applyActionState(); }
}
// The whole label is global on purpose: a per-tab badge would override the
// ON/OFF switch on the tab that just acted and hide the state the user is
// about to toggle. Per-tab results go to the native message list instead.
async function applyActionState() {
  const state = actionState();
  try {
    await invoke(api.action, "setTitle", { title: state.title });
    await invoke(api.action, "setBadgeText", { text: state.text });
    await invoke(api.action, "setBadgeBackgroundColor", { color: state.color });
  } catch (_) {}
}
function clearDiscoveries() {
  mediaByTab.clear();
  frames.clear();
  pendingCapabilities.clear();
  requestContexts.clear();
  manifestLinks.clear();
  manifestTasks.clear();
  frameResponses.clear();
  tabStatus.clear();
  playTimes.clear();
}
async function setDetectionEnabled(enabled) {
  detectionEnabled = enabled === true;
  await invoke(api.storage.local, "set", { detectionEnabled });
  if (!detectionEnabled) clearDiscoveries();
  saveSession();
  await applyActionState();
  return detectionEnabled;
}
// Clicks are serialized: a burst must not interleave reads and writes, or the
// persisted value and the badge would end up disagreeing.
let toggleTask = Promise.resolve();
function toggleDetection() {
  toggleTask = toggleTask.then(async () => {
    await sessionReady;
    await setDetectionEnabled(!detectionEnabled);
  }).catch(() => {});
  return toggleTask;
}
function isExtensionContent(sender) {
  return sender?.id===api.runtime?.id && Number.isInteger(sender.tab?.id)
    && (!!safeUrl(sender.url) || sender.url==="about:blank" || sender.url==="about:srcdoc");
}
function validBrowserTabId(value) { return Number.isInteger(value) && value >= 0; }
function validTicket(key, value) {
  return /^[a-f0-9]{64}$/.test(key) && typeof value?.id==="string" && /^[0-9a-f-]{36}$/i.test(value.id)
    && typeof value.submitted==="boolean" && (value.tabId===undefined||validBrowserTabId(value.tabId));
}
function validLaunch(key, value) {
  return typeof key==="string" && key.length>0 && key.length<=16384 && typeof value?.id==="string"
    && /^[0-9a-f-]{36}$/i.test(value.id) && value.id.length<=128 && validBrowserTabId(value.tabId)
    && typeof value.at==="number";
}

// Events are relayed through SSDownloadEvents; a missing relay module (a context
// that loaded only this file) stays silent instead of throwing into discovery.
function logEvent(name, fields) {
  try { return globalThis.SSDownloadEvents?.log?.(name, fields) || null; } catch (_) { return null; }
}
function urlHost(value) { return safeUrl(value)?.hostname || null; }
function replyCode(response) {
  const codes = globalThis.SSDownloadErrorCodes;
  const explicit = response?.snapshot?.inspect_error_code;
  if (typeof explicit === "string" && codes?.isCode?.(explicit)) return explicit;
  return codes?.splitCode?.(String(response?.message || ""))?.code || null;
}
function extensionVersion() {
  try { return String(api.runtime?.getManifest?.()?.version || "unknown"); } catch (_) { return "unknown"; }
}
// Validates an inbound relay batch exactly like this context's own events:
// every field passes through SSDownloadEvents.log, invalid entries are dropped.
function enqueueRelayEvents(events) {
  if (!Array.isArray(events)) return 0;
  let accepted = 0;
  for (const raw of events.slice(0, 50)) if (raw && typeof raw === "object" && logEvent(raw.event, raw)) accepted += 1;
  return accepted;
}
function submitDetail(request) {
  const parts = [];
  const container = typeof request.container === "string" && request.container ? request.container : null;
  const kind = typeof request.kind === "string" ? request.kind : null;
  parts.push(container || kind || "media");
  const height = Number(request.exact_height || request.max_height);
  if (Number.isFinite(height) && height > 0) parts.push(`${Math.trunc(height)}p`);
  const audio = request.audio_language || request.audio_tracks?.[0]?.language;
  if (audio) parts.push(`audio=${String(audio).slice(0, 35)}`);
  const subtitle = request.subtitle_languages?.[0] || request.subtitle_tracks?.[0]?.language || request.external_subtitles?.[0]?.language;
  if (subtitle) parts.push(`subtitle=${String(subtitle).slice(0, 35)}`);
  return parts.join(" ");
}
// Bounded capability facts for a frame whose report produced no usable candidate.
function observationFacts(frame) {
  const facts = [`players=${frame.players.length}`];
  const capabilities = frame.observationCapabilities;
  if (capabilities && !frame.players.length) facts.push("closedShadow=possible");
  const blob = frame.players.filter(player => (player.urls || []).some(url => url.startsWith("blob:"))).length;
  if (blob) facts.push(`blobAmbiguous=${blob}`);
  if (!capabilities) facts.push("capabilities=none");
  else {
    if (capabilities.pageFetchConsumed !== true && capabilities.pageXhrConsumed !== true) facts.push("pageBody=none");
    if (capabilities.workerResponseBody !== true) facts.push("workerBody=none");
    if (capabilities.mseAppendBufferAncestry !== true) facts.push("mseAncestry=none");
    if (capabilities.performanceResourceUrl !== true) facts.push("performanceUrl=none");
  }
  facts.push("noCandidate");
  return facts.join(" ");
}
// A submission is recorded only after the desktop acknowledged it; the job id
// travels on when the reply carries one.
function noteNativeReply(action, response) {
  if (!action || typeof action !== "object") return;
  if (action.type === "add" && response?.ok === true) {
    const request = action.request || {};
    const job = response.result?.job_ids?.[0];
    logEvent("ext.submit", { level: "info", outcome: "ok", host: urlHost(request.page_url || request.url),
      job: typeof job === "string" ? job : null, detail: submitDetail(request) });
    return;
  }
  if ((action.type === "inspect" || action.type === "inspect_status") && response?.ok !== true) {
    logEvent("ext.inspect.fail", { level: "warn", outcome: "failed", code: replyCode(response) || "SSD-EXT-010",
      host: urlHost(action.request?.url), detail: String(response?.message || "inceleme yanıtsız kaldı") });
  }
}

async function loadPrivacySetting() {
  try {
    const saved = await invoke(api.storage.local, "get", { detectionEnabled: true });
    detectionEnabled = saved.detectionEnabled !== false;
  } catch (_) {
    detectionEnabled = false;
  }
}
let sessionError = null;
const sessionReady = (async () => {
  await loadPrivacySetting();
  const saved = await invoke(api.storage.session, "get", ["discovery", "downloadTickets", "browserLaunches"]);
  for (const [key, value] of (saved.downloadTickets || [])) if(validTicket(key,value))tickets.set(key,value);
  for (const [key, value] of (saved.browserLaunches || [])) if(validLaunch(key,value))launches.set(key,value);
  if (!saved.discovery || !detectionEnabled) return;
  for (const [tabId, entries] of saved.discovery.media) mediaByTab.set(tabId, new Map(entries.map(([,item])=>[sourceKey(item),item])));
  for (const [tabId, status] of saved.discovery.status) tabStatus.set(tabId, status);
  for (const [tabId, time] of saved.discovery.plays) playTimes.set(tabId, time);
   for (const [key, frame] of saved.discovery.frames || []) frames.set(key, frame);
  for (const [key, children] of saved.discovery.links || []) manifestLinks.set(key, normalizeRelations(children));
  for (const [key, context] of saved.discovery.contexts || []) if (context && Date.now()-Number(context.observedAt||0)<=REQUEST_CONTEXT_MAX_AGE) requestContexts.set(key,context);
})().then(
  // A later successful load clears an earlier failure: the flag reports the
  // session's current health, not the first problem it ever hit.
  () => { sessionError = null; },
  () => { sessionError = "Geçici keşif belleği yüklenemedi; sayfayı yeniden tarayın."; }
);
let sessionWrite = Promise.resolve();
let savePending = false;
function boundedEntries(entries, budget) {
  const kept=[];
  for(const entry of entries.slice().reverse()) {
    const size=JSON.stringify(entry).length*2;
    if(size>budget) continue;
    budget-=size; kept.unshift(entry);
  }
  return kept;
}
function saveSession() {
  if (savePending) return;
  savePending = true;
  queueMicrotask(() => {
    savePending = false;
    let remaining = 4 * 1024 * 1024;
    const media = [];
    for (const [tabId, values] of Array.from(mediaByTab).reverse()) {
      const entries = [];
      for (const [key, item] of Array.from(values).reverse()) {
        const size = 512 + key.length * 2 + JSON.stringify(item).length * 2;
        if (size > remaining) continue;
        remaining -= size;
        entries.unshift([key, item]);
      }
      if (entries.length) media.unshift([tabId, entries]);
    }
    const retained = new Set(media.map(([id])=>id));
    const discovery = { media, status: Array.from(tabStatus), plays: Array.from(playTimes),
      frames:boundedEntries(Array.from(frames).filter(([,f])=>retained.has(f.tabId)),1024*1024),
      links:boundedEntries(Array.from(manifestLinks),1024*1024),
      contexts:boundedEntries(Array.from(requestContexts).filter(([,context])=>retained.has(context.tabId) && Date.now()-context.observedAt<=REQUEST_CONTEXT_MAX_AGE),512*1024) };
    sessionWrite = sessionWrite
      .then(() => invoke(api.storage.session, "set", { discovery }))
      // A later successful save clears an earlier failure so a transient storage
      // error does not stick to every status the worker reports.
      .then(() => { sessionError = null; })
      .catch(() => { sessionError = "Geçici keşif belleği kaydedilemedi; arka plan işçisi kapanınca liste kaybolabilir."; });
  });
}

function safeUrl(value) {
  if (typeof value !== "string" || value.length > 16384) return null;
  try {
    const parsed = new URL(value);
    return parsed.protocol === "http:" || parsed.protocol === "https:" ? parsed : null;
  } catch (_) {
    return null;
  }
}

function contentType(headers) {
  if (!headers) return "";
  const header = headers.find((item) => item.name.toLowerCase() === "content-type");
  return header ? String(header.value || "").split(";", 1)[0].trim().toLowerCase() : "";
}

function dispositionFilename(headers) {
  if (!headers) return "";
  const header = headers.find((item) => item.name.toLowerCase() === "content-disposition");
  if (!header || !header.value) return "";
  // RFC 8187 allows an optional language tag between the two apostrophes, e.g.
  // filename*=UTF-8'tr'video%20ad%C4%B1.mp4.  Do not lose that safe filename
  // just because the server supplied a language.
  const utf8 = /filename\*\s*=\s*UTF-8'(?:[^']*)'([^;]+)/i.exec(header.value);
  const plain = /filename\s*=\s*(?:"([^"]+)"|([^;]+))/i.exec(header.value);
  const raw = utf8 ? utf8[1] : plain ? (plain[1] || plain[2]) : "";
  try { return decodeURIComponent(raw.trim()).replace(/[\\/:*?"<>|\u0000-\u001f]/g, "_"); }
  catch (_) { return raw.trim().replace(/[\\/:*?"<>|\u0000-\u001f]/g, "_"); }
}

function pathFilename(url) {
  try {
    const part = new URL(url).pathname.split("/").filter(Boolean).pop() || "";
    return decodeURIComponent(part).replace(/[\\/:*?"<>|\u0000-\u001f]/g, "_");
  } catch (_) { return ""; }
}

function classify(url, type) {
  const parsed = safeUrl(url);
  if (!parsed) return null;
  const path = parsed.pathname.toLowerCase();
  const mime = String(type || "").toLowerCase();
  if (technicalResource(url, mime)) return null;
  if (/\.(ts|m4s|cmfv|cmfa)$/.test(path) || /(?:^|[\/_-])(?:init|segment|frag(?:ment)?)[-_.0-9]*\.(mp4|m4v|m4a)$/.test(path) || mime === "video/mp2t") return null;
  if (mime.includes("application/vnd.apple.mpegurl") || mime.includes("application/x-mpegurl") || /\.m3u8(?:$|[?#])/.test(url.toLowerCase())) return { kind: "hls", label: "HLS yayını" };
  if (mime.includes("application/dash+xml") || /\.mpd(?:$|[?#])/.test(url.toLowerCase())) return { kind: "dash", label: "DASH yayını" };
  if (mime.startsWith("video/") || /\.(mp4|m4v|webm|mov|mkv)(?:$|[?#])/.test(url.toLowerCase())) return { kind: "video", label: mime || "Video" };
  if (mime.startsWith("audio/") || /\.(mp3|m4a|aac|ogg|opus|flac|wav)(?:$|[?#])/.test(url.toLowerCase())) return { kind: "audio", label: mime || "Ses" };
  if (/\.(zip|rar|7z|pdf|exe|msi|iso|dmg|apk|epub|docx?|xlsx?|pptx?|csv|txt)$/.test(path) || /^(application\/(pdf|zip|x-7z-compressed|x-rar-compressed|octet-stream|vnd\.openxmlformats-officedocument.*))$/.test(mime)) return {kind:"file",label:"Dosya"};
  return null;
}

function technicalResource(url, mime = "") {
  const path = safeUrl(url)?.pathname.toLowerCase() || "";
  return /\.(png|jpe?g|gif|webp|svg|ico|avif|webmanifest|ts|m4s|cmfv|cmfa)$/.test(path)
    || /(?:^|[\/_-])(?:init|segment|frag(?:ment)?)[-_.0-9]*\.(mp4|m4v|m4a)$/.test(path)
    || /^(image\/|application\/manifest\+json|video\/mp2t|video\/iso.segment)/.test(mime);
}

// Manifest ancestry is explicit URI linkage, never a filename/size heuristic.
function normalizeRelations(values) {
  const allowed=new Set(["variant","audio","init","segment"]), result=[];
  for(const value of Array.isArray(values)?values:[]) {
    const relation=typeof value==="string"?{url:value,role:"variant"}:value;
    const url=safeUrl(relation?.url)?.href;
    if(url && allowed.has(relation.role) && !result.some(item=>item.url===url && item.role===relation.role)) result.push({url,role:relation.role});
    if(result.length>=200) break;
  }
  return result;
}
function hlsRelations(text, base) {
  text=text.trimStart();
  if (!text.startsWith("#EXTM3U")) return [];
  const result = [];
  let variant = false;
  for (const line of text.split(/\r?\n/).map(s => s.trim())) {
    let role=null, uri=null;
    if (line.startsWith("#EXT-X-STREAM-INF:")) { variant = true; continue; }
    if (/^#EXT-X-MEDIA:/.test(line)) { role=/\bTYPE=AUDIO\b/i.test(line)?"audio":"variant"; uri=/(?:^|,)URI="([^"]+)"/.exec(line.slice(line.indexOf(":") + 1))?.[1]; }
    else if (/^#EXT-X-I-FRAME-STREAM-INF:/.test(line)) { role="variant"; uri=/(?:^|,)URI="([^"]+)"/.exec(line.slice(line.indexOf(":") + 1))?.[1]; }
    else if (/^#EXT-X-MAP:/.test(line)) { role="init"; uri=/(?:^|,)URI="([^"]+)"/.exec(line.slice(line.indexOf(":") + 1))?.[1]; }
    else if (variant && line && !line.startsWith("#")) { role="variant"; uri=line; variant=false; }
    else if(line && !line.startsWith("#")) { role="segment"; uri=line; }
    if(uri) { try { const value = new URL(uri, base).href; if (safeUrl(value)) result.push({url:value,role}); } catch (_) {} }
  }
  return normalizeRelations(result);
}
function sourceKey(item) { return `${item.frameId ?? -1}\n${item.kind}\n${item.url}`; }
function manifestKey(tabId, item) { return `${tabId}\n${item.frameId ?? -1}\n${item.url}`; }
async function linkManifest(tabId, item, values) {
  const key = manifestKey(tabId, item);
  const observed = values.get(sourceKey(item))?.generation;
  if (item.kind !== "hls" || manifestTasks.has(key) || manifestActive>=4 || typeof fetch !== "function") return;
  // Bounded, credential-free metadata read. Authenticated manifests are handled by the engine after site consent.
  const task = (async () => {
    ++manifestActive;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 5000);
    try {
      const response = await fetch(item.url, { credentials: "omit", signal: controller.signal, redirect: "error" });
      if (!response.ok || !response.body) return;
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let text = "", size = 0;
      try {
        for (;;) {
          const {done, value} = await reader.read(); if (done) break;
          size += value.byteLength; if (size > 512 * 1024) throw new Error("Manifest sınırı");
          text += decoder.decode(value, {stream:true});
        }
        text += decoder.decode();
      } finally { await reader.cancel().catch(() => {}); }
      if (mediaByTab.get(tabId) === values && values.get(sourceKey(item))?.generation === observed) {
        if (!manifestLinks.has(key)) manifestLinks.set(key, hlsRelations(text, item.url));
        while(manifestLinks.size>400) manifestLinks.delete(manifestLinks.keys().next().value);
        saveSession();
      }
    } catch (_) { /* Do not retry an inaccessible manifest in a loop. */ }
    finally { clearTimeout(timer); --manifestActive; }
  })();
  manifestTasks.set(key, task);
  while (manifestTasks.size > 400) manifestTasks.delete(manifestTasks.keys().next().value);
}

// A page-provided name is usable only when it names content: a player brand, an
// opaque player title, a bare technical suffix or the frame's own host name is not
// a film name and is never carried into the candidate list.
function descriptiveTitle(value, frameUrl) {
  const title = String(value || "");
  const trimmed = title.trim();
  if (!trimmed) return "";
  const host = safeUrl(frameUrl)?.hostname.split(".") || [];
  if (/^(?:[a-z0-9_-]*player|(?:html5|video|audio|media)\s+player)$/i.test(trimmed)) return "";
  if (/^["']:\d+["'](?:\s|$)/.test(trimmed)) return "";
  if (/^(?:video|player|embed|master|index|manifest|stream|hls|dash|youtube|service|forbidden|access denied|just a moment\.\.\.)$/i.test(trimmed)) return "";
  if (host.some(part => part.toLowerCase() === trimmed.toLowerCase())) return "";
  if (/\.(?:mp4|mkv|m4v|m3u8|mpd)(?:$|\s)/i.test(trimmed)) return "";
  return title;
}

function videoRecords(tabId) {
  const items = [...(mediaByTab.get(tabId)?.values() || [])].filter(s => ["hls","dash","video","audio","blob","webrtc"].includes(s.kind));
  const tabFrames = [...frames.values()].filter(f => f.tabId === tabId);
  const result = [];
  const used = new Set();
  const observedUrls = new Set([...(mediaByTab.get(tabId)?.values() || [])].map(item => item.url));
  const descendants = (item, seen = new Set()) => {
    if (seen.has(item.url)) return seen;
    seen.add(item.url);
    for (const child of manifestLinks.get(manifestKey(tabId,item)) || []) descendants({url:typeof child==="string"?child:child.url,frameId:item.frameId}, seen);
    return seen;
  };
  const sameDiscoveryDocument=(left,right)=>left.documentId||right.documentId?!!left.documentId&&left.documentId===right.documentId:!left.pageUrl||!right.pageUrl||left.pageUrl===right.pageUrl;
  const roots = items.filter(item => !items.some(parent => parent.frameId===item.frameId && sameDiscoveryDocument(parent,item) && parent.url !== item.url && descendants(parent).has(item.url)));
  for (const frame of tabFrames) {
    const produced = result.length;
    const inFrameDocument=item=>item.frameId===frame.frameId && (item.documentId?item.documentId===frame.documentId:item.pageUrl?item.pageUrl===frame.url:item.lastSeen>=frame.since);
    const directByPlayer=new Map(frame.players.map(p=>[p,roots.filter(item=>item.downloadable && inFrameDocument(item) && [...descendants(item)].some(u=>p.urls.includes(u)))]));
    const assigned=new Set([...directByPlayer.values()].flat());
    const manifests=roots.filter(item=>inFrameDocument(item) && ["hls","dash"].includes(item.kind) && item.lastSeen>=frame.since && !assigned.has(item));
    const eligible=frame.players.filter(p=>(p.active || p.played || p.visible!==false) && p.urls.some(u=>u.startsWith("blob:")) && !directByPlayer.get(p).length);
    for (const player of frame.players) {
      if (!player.urls.length && !player.active && !player.played) continue;
      if (player.visible === false && !player.active && !player.played) continue;
      const direct = directByPlayer.get(player);
      const currentMatches=direct.filter(item=>descendants(item).has(player.urls[0]));
      // Inference stays inside one document/frame and only when one blob player and one root remain.
      const inferred = !direct.length && eligible.length === 1 && eligible[0]===player && manifests.length === 1;
      let choices=inferred?manifests:currentMatches.length===1?currentMatches:direct.length===1?direct:[];
      const ambiguousChoices=choices.length?[]:((eligible.length===1 && eligible[0]===player && manifests.length>1)?manifests:direct.length>1?direct:[]);
      for (const candidate of [...choices,...ambiguousChoices]) for (const u of descendants(candidate)) used.add(`${frame.frameId}\n${u}`);
      player.urls.forEach(u => used.add(`${frame.frameId}\n${u}`));
      const ad = player.adSignals.length >= 2;
      // General preview rule: an endless, silent player that started without the user asking is a
      // preview or an ad on any site (home-page hovers, carousels, suggestion tiles). Its page
      // address is not content, so it never becomes a candidate of its own.
      const preview = player.loop && player.muted && !player.userActivated;
      const frameTitle=frame.pageTitle || "";
      const descriptiveFrameTitle=descriptiveTitle(frameTitle,frame.url);
      const onePlayer=frame.players.filter(p=>p.urls.length || p.active || p.played).length===1;
      const distinctPlayerTitle=player.title && player.title!==frameTitle && player.title!==frame.tabTitle && !/^(?:video|player|embed|master|index|manifest|stream|hls|dash)$/i.test(player.title.trim());
      const title = player.explicitTitle ? player.title : onePlayer && descriptiveFrameTitle ? frameTitle : onePlayer ? (frame.tabTitle || player.title) : distinctPlayerTitle ? player.title : `Video ${player.id}`;
      const titleSource=player.explicitTitle?"element":onePlayer&&descriptiveFrameTitle?"frame":onePlayer&&frame.tabTitle?"tab":distinctPlayerTitle?"player":"generic";
      const videoId = `${frame.frameId}:${frame.documentId}:${player.id}`;
      const base={videoId,playerId:player.id,documentId:frame.documentId,title,titleSource,filename:title,thumbnail:player.poster,
        pageUrl:frame.url,frameId:frame.frameId,tracks:player.tracks,duration:player.duration,adSignals:player.adSignals,
        suspectedAd:ad,played:player.played,active:player.active,preview};
      if(ambiguousChoices.length) {
        for(const candidate of ambiguousChoices) {
          const blocked=frame.responseStatus>=400 && !(candidate.networkStatus>=200 && candidate.networkStatus<300);
          result.push({...candidate,...base,videoId,candidateId:`${videoId}:candidate:${candidate.generation||candidate.url}`,
            downloadable:!blocked && candidate.downloadable===true,selectable:!blocked && candidate.downloadable===true,
            requiresConfirmation:true,ambiguous:true,associationUncertainty:"multiple-observed-roots",inferred:false,
            note:blocked?`Oynatıcı sayfası HTTP ${frame.responseStatus} döndürdü.`:"Bu oynatıcıyla birden fazla yayın ilişkili olabilir; yalnız açık seçimle kullanın."});
        }
        continue;
      }
      const chosen=choices[0] || null;
      const blockedFrame=frame.responseStatus>=400 && !(chosen?.networkStatus>=200 && chosen?.networkStatus<300);
      if(chosen && !blockedFrame) {
        const duplicate=!ad && result.find(v=>!v.ambiguous && !v.suspectedAd && v.frameId===frame.frameId && v.url===chosen.url
          && JSON.stringify(v.tracks||[])===JSON.stringify(player.tracks||[]) && v.downloadable);
        if(duplicate) { (duplicate.aliases ||= []).push(videoId); continue; }
        result.push({...chosen,...base,downloadable:true,selectable:true,requiresConfirmation:ad||preview,ambiguous:false,inferred,
          candidateId:ad?`${videoId}:candidate:${chosen.generation||chosen.url}`:undefined,
          associationUncertainty:ad?"suspected-ad":null,note:ad?"Bağımsız reklam belirtileri taşıyor; yalnız açık seçimle kullanın.":""});
      // No network media was captured for this player (MSE/blob players such as YouTube serve
      // fragments that never appear as a resource request). The page address itself stays
      // usable: the application resolves it with its media engine, exactly like the
      // "download this page" context menu entry does.
      } else if (!preview) result.push({...base,url:frame.url,kind:"page",downloadable:!blockedFrame,selectable:!blockedFrame,ambiguous:false,inferred:false,
        associationUncertainty:null,note:blockedFrame?`Oynatıcı sayfası HTTP ${frame.responseStatus} döndürdü; bu sayfadaki kaynak bölüm videosu olarak doğrulanamadı.`:"Oynatıcının akış isteği yakalanamadı; indirme, sayfa adresi uygulamada çözümlenerek yapılacak. Sonuç alınamazsa videoyu oynatıp Yenile'ye basın."});
    }
    // A document may render its player without ever creating a media element: a JS
    // player mounted into a container, an embed iframe, a player library script. The
    // content script reports those signals, so a frame that produced no record of its
    // own still keeps one page candidate - the address the application resolves with
    // its media engine. A frame with no signal stays silent: a plain article page must
    // not grow a tile.
    const signals = frame.playerSignals;
    const embeds = Array.isArray(signals?.embeds) ? signals.embeds : [];
    const signalled = embeds.length>0 || signals?.containers===true || signals?.playerScripts===true;
    const considered = frame.players.filter(p=>(p.urls.length || p.active || p.played) && !(p.visible === false && !p.active && !p.played));
    const previewsOnly = considered.length>0 && considered.every(p=>p.loop && p.muted && !p.userActivated);
    if (result.length === produced && signalled && !previewsOnly && !observedUrls.has(frame.url)) {
      const blocked = frame.responseStatus >= 400;
      const frameName = descriptiveTitle(frame.pageTitle, frame.url), tabName = descriptiveTitle(frame.tabTitle, frame.url);
      const title = frameName || tabName || "Video";
      result.push({videoId:`${frame.frameId}:${frame.documentId}:page`,playerId:null,documentId:frame.documentId,title,
        titleSource:frameName?"frame":tabName?"tab":"generic",filename:title,thumbnail:null,pageUrl:frame.url,frameId:frame.frameId,
        tracks:[],duration:null,adSignals:[],suspectedAd:false,played:false,active:false,preview:false,
        url:frame.url,kind:"page",downloadable:!blocked,selectable:!blocked,ambiguous:false,inferred:false,associationUncertainty:null,
        note:blocked?`Oynatıcı sayfası HTTP ${frame.responseStatus} döndürdü; bu sayfadaki kaynak bölüm videosu olarak doğrulanamadı.`:"Oynatıcının akış isteği yakalanamadı; indirme, sayfa adresi uygulamada çözümlenerek yapılacak. Sonuç alınamazsa videoyu oynatıp Yenile'ye basın."});
    }
    // An embed address the document loads is its own resolvable page candidate: the
    // application opens it exactly like a player page. A discovered item that already
    // covers the address is never duplicated.
    for (const embed of embeds) {
      if (observedUrls.has(embed.url)) continue;
      const host = safeUrl(embed.url)?.hostname || "";
      const title = (embed.title || host || "Gömülü oynatıcı").slice(0,180);
      result.push({videoId:`${frame.frameId}:${frame.documentId}:embed:${String(embed.url).slice(0,240)}`,playerId:null,documentId:frame.documentId,
        title,titleSource:embed.title?"element":"generic",filename:embed.title?title:"",thumbnail:null,pageUrl:frame.url,frameId:frame.frameId,
        tracks:[],duration:null,adSignals:[],suspectedAd:false,played:false,active:false,preview:false,
        url:embed.url,kind:"page",downloadable:true,selectable:true,ambiguous:false,inferred:false,associationUncertainty:null,
        note:"Gömülü oynatıcı adresi uygulamada çözümlenerek indirilecek."});
    }
  }
  for (const item of roots) {
    if (used.has(`${item.frameId}\n${item.url}`) || !item.downloadable) continue;
    if (tabFrames.some(frame=>frame.frameId===item.frameId && frame.players.length)) continue;
    result.push({...item, videoId:`network:${item.frameId}:${item.url}`, unassociated:true});
  }
  return result;
}

function clearTabContext(tabId, keepDocumentResponses=false) {
  for (const [key, frame] of frames) if (frame.tabId === tabId) frames.delete(key);
  if(!keepDocumentResponses) for(const key of frameResponses.keys()) if(key.startsWith(`${tabId}\n`))frameResponses.delete(key);
  for (const map of [requestContexts, manifestLinks, manifestTasks, pendingCapabilities]) for (const key of map.keys()) if (key.startsWith(`${tabId}\n`)) map.delete(key);
  for (const key of observationLimits) if (key.startsWith(`${tabId}\n`)) observationLimits.delete(key);
}

async function pruneFrames(tabId) {
  await sessionReady;
  if (!api.webNavigation?.getAllFrames) return;
  try {
    const live = await invoke(api.webNavigation,"getAllFrames",{tabId});
    if (!live) return;
    const current = new Map(live.map(f=>[f.frameId,f]));
    for (const [key,frame] of frames) if(frame.tabId===tabId && (!current.has(frame.frameId) || (current.get(frame.frameId).documentId && current.get(frame.frameId).documentId!==frame.documentId))) frames.delete(key);
    const values=mediaByTab.get(tabId);
    if(values) for(const [key,item] of values) if(!current.has(item.frameId) || (item.documentId && current.get(item.frameId).documentId && item.documentId!==current.get(item.frameId).documentId)) {
      values.delete(key);manifestLinks.delete(manifestKey(tabId,item));manifestTasks.delete(manifestKey(tabId,item));
    }
    saveSession();
  } catch (_) { /* Tab may be closing. */ }
}
if(api.webNavigation?.onCommitted) api.webNavigation.onCommitted.addListener(details=>{pruneFrames(details.tabId).catch(()=>{});});

function relatedRole(tabId,frameId,url) {
  for(const [key,relations] of manifestLinks) if(key.startsWith(`${tabId}\n${frameId}\n`)) {
    const relation=relations.find(item=>(typeof item==="string"?item:item.url)===url);
    if(relation) return typeof relation==="string"?"variant":relation.role;
  }
  return null;
}
function requestRole(details) {
  const related=relatedRole(details.tabId,details.frameId,details.url);
  if(related) return related;
  const classification=classify(details.url,"");
  if(classification?.kind==="hls" || classification?.kind==="dash") return "root";
  return technicalResource(details.url)?(/(?:^|[\/_-])init(?:[\/_-]|\.|$)/i.test(safeUrl(details.url)?.pathname||"")?"init":"segment"):"root";
}
function safeRequestHeaders(values) {
  const headers={};
  for(const header of values || []) if(header && typeof header.name==="string") {
    const name=header.name.toLowerCase();
    if (["referer","origin","accept","accept-language","user-agent"].includes(name) && typeof header.value === "string" && header.value.length <= 8192) headers[name] = header.value;
  }
  return headers;
}
function requestContextView(context) {
  return {version:1,requestId:String(context.requestId||""),url:context.url,frameId:context.frameId,
    documentId:context.documentId||null,documentUrl:context.documentUrl||null,observedAt:context.observedAt,
    role:context.role,redirects:(context.redirects||[]).slice(0,10).map(item=>({url:item.url,status:item.status})),
    headerObservation:context.headerObservation};
}
function latestRequestContext(tabId,frameId,documentId,url) {
  const frame=frames.get(`${tabId}\n${frameId}`);
  for(const context of [...requestContexts.values()].reverse()) {
    if(context.tabId!==tabId || context.frameId!==frameId || context.url!==url || Date.now()-context.observedAt>REQUEST_CONTEXT_MAX_AGE) continue;
    if(documentId ? context.documentId!==documentId : !frame?.url || context.documentUrl!==frame.url) continue;
    return context;
  }
  return null;
}
function rememberUnavailableContext(details) {
  const key=`${details.tabId}\n${details.requestId}`;
  let context=requestContexts.get(key);
  if(context && context.url===details.url) return context;
  context={requestId:String(details.requestId||""),url:details.url,frameId:details.frameId,tabId:details.tabId,
    documentId:details.documentId||null,documentUrl:safeUrl(details.documentUrl || details.initiator)?.href||null,
    observedAt:Date.now(),role:requestRole(details),redirects:context?.redirects||[],headerObservation:"unavailable",headers:{}};
  requestContexts.set(key,context);
  return context;
}

if (api.webRequest.onBeforeSendHeaders) {
  const capture = async details => {
    // The stored preference decides before anything is recorded: after a worker
    // restart nothing is captured until the session has loaded, so an OFF switch
    // never leaks a request context while its value is still unknown - exactly
    // like the response path below.
    await sessionReady;
    if (!detectionEnabled || details.tabId < 0 || !safeUrl(details.url)) return;
    const key=`${details.tabId}\n${details.requestId}`;
    const previous=requestContexts.get(key);
    const context={requestId:String(details.requestId||""),url:details.url,frameId:details.frameId,tabId:details.tabId,
      documentId:details.documentId||previous?.documentId||null,documentUrl:safeUrl(details.documentUrl || details.initiator)?.href||previous?.documentUrl||null,
      observedAt:Date.now(),role:requestRole(details),redirects:previous?.redirects||[],headerObservation:"observed",
      headers:safeRequestHeaders(details.requestHeaders)};
    requestContexts.delete(key);requestContexts.set(key,context);
    while (requestContexts.size > 1000) requestContexts.delete(requestContexts.keys().next().value);
    saveSession();
  };
  const filter = {urls:["http://*/*","https://*/*"],types:["media","xmlhttprequest","other"]};
  try { api.webRequest.onBeforeSendHeaders.addListener(capture, filter, ["requestHeaders","extraHeaders"]); }
  catch (_) { api.webRequest.onBeforeSendHeaders.addListener(capture, filter, ["requestHeaders"]); }
}
if(api.webRequest.onBeforeRedirect) api.webRequest.onBeforeRedirect.addListener(async details=>{
  // Resumes in call order behind the header capture of the same request, so the
  // redirect still finds the context that capture recorded.
  await sessionReady;
  const context=requestContexts.get(`${details.tabId}\n${details.requestId}`);
  if(!context || context.url!==details.url || !safeUrl(details.redirectUrl)) return;
  context.redirects=[...(context.redirects||[]),{url:details.url,status:details.statusCode}].slice(-10);
  context.url=details.redirectUrl;context.observedAt=Date.now();context.role=requestRole({...details,url:details.redirectUrl});
  saveSession();
},{urls:["http://*/*","https://*/*"],types:["media","xmlhttprequest","other"]});

// A text/plain (or extensionless) manifest is identified only after the player
// consumes its body. Join the earlier request by exact URL AND document context.
function observedSourceContext(tabId, sender, sourceUrl, pageUrl) {
  const frameId=sender.frameId || 0;
  const frame=frames.get(`${tabId}\n${frameId}`);
  const documentId=sender.documentId || frame?.documentId;
  for(const context of [...requestContexts.values()].reverse()) {
    if(context.tabId!==tabId || context.frameId!==frameId || context.url!==sourceUrl
      || Date.now()-context.observedAt>REQUEST_CONTEXT_MAX_AGE || context.observedAt<(frame?.since || 0)) continue;
    if(context.documentId ? context.documentId!==documentId
      : !pageUrl || context.documentUrl!==pageUrl) continue;
    return {requestHeaders:{...context.headers},requestContext:requestContextView(context),
      contextRevision:`${context.observedAt}:${context.requestId}`,contextAgeMs:Math.max(0,Date.now()-context.observedAt),networkStatus:context.status};
  }
  return {};
}

function likelyDrmUrl(url) {
  const value = url.toLowerCase();
  return /(?:^|[/?&._-])(widevine|playready|fairplay|drm|license|licence)(?:[/?&._=-]|$)/.test(value);
}

function signedResourceIdentity(value) {
  const parsed = safeUrl(value);
  if (!parsed) return value;
  // Generic token/id fields may identify different films and are deliberately preserved.
  for (const name of [...parsed.searchParams.keys()]) {
    if (/^(?:signature|expires|x-amz-(?:signature|date|expires)|x-goog-(?:signature|date|expires))$/i.test(name)) parsed.searchParams.delete(name);
  }
  return parsed.href;
}

function record(tabId, item) {
  if (!detectionEnabled || !Number.isInteger(tabId) || tabId < 0) return;
  let values = mediaByTab.get(tabId);
  if (!values) {
    values = new Map();
    mediaByTab.set(tabId, values);
  }
  const key = sourceKey(item);
  let previous = values.get(key);
  const fresh = !previous;
  if (!previous) for (const [oldKey, old] of values) {
    const frame = frames.get(`${tabId}\n${item.frameId}`);
    if (frame?.players.length===1 && old.frameId === item.frameId && old.kind === item.kind && old.url !== item.url && signedResourceIdentity(old.url) === signedResourceIdentity(item.url)) {
      previous = old; values.delete(oldKey);
      manifestLinks.delete(manifestKey(tabId,old)); manifestTasks.delete(manifestKey(tabId,old));
      break;
    }
  }
  values.delete(key);
  values.set(key, {
    ...previous,
    ...item,
    source:previous?.source==="network" ? "network" : item.source,
    generation: previous?.generation || crypto.randomUUID(),
    firstSeen: previous ? previous.firstSeen : Date.now(),
    lastSeen: Date.now(),
    afterPlay: Date.now() >= (playTimes.get(tabId) || Number.MAX_SAFE_INTEGER)
  });
  while (values.size > MAX_MEDIA_PER_TAB) values.delete(values.keys().next().value);
  mediaByTab.delete(tabId);
  mediaByTab.set(tabId, values);
  if (fresh && ["hls", "dash", "video", "audio"].includes(item.kind)) {
    logEvent("ext.capture", { level: "info", outcome: "ok", host: urlHost(item.url), detail: item.kind === "video" ? "mp4" : item.kind });
  }
  linkManifest(tabId, item, values);
  saveSession();
}

function relationRoles(tabId,item,roles=new Map(),seen=new Set()) {
  if(seen.has(item.url)) return roles;
  seen.add(item.url);
  for(const relation of manifestLinks.get(manifestKey(tabId,item)) || []) {
    const url=typeof relation==="string"?relation:relation.url, role=typeof relation==="string"?"variant":relation.role;
    if(!roles.has(url)) roles.set(url,role);
    relationRoles(tabId,{url,frameId:item.frameId},roles,seen);
  }
  return roles;
}
function sameDocumentContext(item,context) {
  if(item.documentId || context.documentId) return !!item.documentId && item.documentId===context.documentId;
  return !!item.pageUrl && item.pageUrl===context.documentUrl;
}
function publicSource(tabId,item) {
  const roles=relationRoles(tabId,item);
  const chain=[];
  const add=(context,role,headers,attached=false)=>{
    if(!context || (!attached && !sameDocumentContext(item,context)) || chain.some(value=>value.requestId===String(context.requestId||"") && value.url===context.url)) return;
    const age=Math.max(0,Date.now()-Number(context.observedAt||0));
    const fresh=age<=REQUEST_CONTEXT_MAX_AGE;
    chain.push({...requestContextView({...context,role:role||context.role,headerObservation:fresh?context.headerObservation:"unavailable"}),
      requestHeaders:fresh&&context.headerObservation==="observed"?{...(headers||context.headers||{})}:{},
      responseStatus:Number.isInteger(context.status)?context.status:null});
  };
  if(item.requestContext) add({...item.requestContext,headers:item.requestHeaders||{},status:item.networkStatus},"root",item.requestHeaders,true);
  for(const context of requestContexts.values()) {
    const role=context.url===item.url?"root":roles.get(context.url);
    if(role) add(context,role,context.headers);
  }
  chain.sort((a,b)=>Number(a.role!=="root")-Number(b.role!=="root") || a.observedAt-b.observedAt);
  while(chain.length>64 || JSON.stringify(chain).length>65536) chain.pop();
  let own=chain.find(value=>value.role==="root" && value.url===item.url);
  if(!own) {
    const observedAt=Number(item.requestContext?.observedAt||item.lastSeen||Date.now());
    own={version:1,requestId:String(item.requestContext?.requestId||""),url:item.url,frameId:item.frameId,
      documentId:item.documentId||null,documentUrl:item.pageUrl||null,observedAt,role:"root",redirects:[],headerObservation:"unavailable",requestHeaders:{}};
    chain.unshift(own);
  }
  const contextAgeMs=Math.max(0,Date.now()-own.observedAt);
  const fresh=contextAgeMs<=REQUEST_CONTEXT_MAX_AGE && own.headerObservation==="observed";
  const requestContext={...own};delete requestContext.requestHeaders;delete requestContext.responseStatus;
  return {...item,requestHeaders:fresh?{...own.requestHeaders}:{},requestContext,requestChain:chain,
    contextRevision:item.contextRevision||`${own.observedAt}:${own.requestId}`,contextAgeMs,contextFresh:fresh,contextExpired:contextAgeMs>REQUEST_CONTEXT_MAX_AGE};
}
api.webRequest.onHeadersReceived.addListener(async (details) => {
  await sessionReady;
  if (!detectionEnabled || details.tabId < 0) return;
  if(["main_frame","sub_frame"].includes(details.type)) {
    const key=`${details.tabId}\n${details.frameId}`;
    frameResponses.set(key,{status:details.statusCode,documentId:details.documentId,url:details.url});
    while(frameResponses.size>100)frameResponses.delete(frameResponses.keys().next().value);
  }
  let requestContext=requestContexts.get(`${details.tabId}\n${details.requestId}`);
  if(!requestContext && !["main_frame","sub_frame"].includes(details.type)) requestContext=rememberUnavailableContext(details);
  if(requestContext && requestContext.url===details.url) { requestContext.status=details.statusCode; requestContext.role=relatedRole(details.tabId,details.frameId,details.url)||requestContext.role; }
  const sizeHeader=(details.responseHeaders||[]).find(header=>header.name.toLowerCase()==="content-length");
  const size=Number(sizeHeader?.value);
  const mime = contentType(details.responseHeaders);
  if (technicalResource(details.url, mime) || ["init","segment"].includes(relatedRole(details.tabId,details.frameId,details.url))) return;
  if (details.statusCode < 200 || details.statusCode >= 300) return;
  const suggested = dispositionFilename(details.responseHeaders);
  let classification = classify(details.url, mime) || (suggested && safeUrl(details.url) ? {kind:"file",label:"Dosya"} : null);
  if(classification?.kind==="file" && mime==="application/octet-stream" && !suggested && !/\.(?:zip|rar|7z|exe|msi|iso|dmg|apk|pdf)(?:$|[?#])/i.test(details.url)) classification=null;
  if (!classification) {
    if (likelyDrmUrl(details.url)) {
      const current = tabStatus.get(details.tabId) || {};
      tabStatus.set(details.tabId, { ...current, drmCue: "Sayfa bir lisans/DRM uç noktasına erişti. DRM korumalı içerik desteklenmez." });
      saveSession();
    }
    return;
  }
  record(details.tabId, {
    url: details.url,
    kind: classification.kind,
    label: classification.label,
    contentType: mime,
    size:Number.isSafeInteger(size)&&size>0?size:null,
    // A technical address basename is not a name: only a page-provided name travels.
    filename: dispositionFilename(details.responseHeaders) || "",
    source: "network",
    networkStatus: details.statusCode,
    pageUrl: safeUrl(details.documentUrl || details.initiator)?.href || null,
    frameId: details.frameId,
    documentId: details.documentId,
    requestHeaders: requestContext?.headerObservation==="observed"?requestContext.headers:{},
    requestContext:requestContext?requestContextView(requestContext):null,
    contextRevision:requestContext?`${requestContext.observedAt}:${requestContext.requestId}`:null,
    downloadable: true,
    drmHint: false
  });
}, { urls: ["http://*/*", "https://*/*"], types: ["media", "xmlhttprequest", "other", "main_frame", "sub_frame"] }, ["responseHeaders"]);

api.tabs.onRemoved.addListener(async (tabId) => {
  await sessionReady;
  mediaByTab.delete(tabId);
  clearTabContext(tabId);
  tabStatus.delete(tabId);
  playTimes.delete(tabId);
  saveSession();
});
api.tabs.onUpdated.addListener(async (tabId, changeInfo) => {
  await sessionReady;
  if (changeInfo.status === "loading" || changeInfo.url) {
    mediaByTab.delete(tabId);
    // Chrome may emit this after the new document's response headers.
    // Responses are joined only by document identity and URL, never by tab alone.
    clearTabContext(tabId, true);
    tabStatus.delete(tabId);
    playTimes.delete(tabId);
  saveSession();
  }
});

function nativeErrorMessage(error) {
  const message = String(error && error.message ? error.message : error);
  if (/native messaging host.*not found|specified native messaging host|no such native application/i.test(message)) {
    return "SSDownload yerel bağlantısı kayıtlı değil. Masaüstü uygulamasında Tarayıcı Kurulumu'nu açın veya SSDownload.exe --register-host çalıştırın; ardından tarayıcıyı yeniden başlatın.";
  }
  if (/access.*denied|forbidden/i.test(message)) {
    return "SSDownload yerel bağlantısına erişim reddedildi. Uygulamayı ve tarayıcıyı aynı Windows kullanıcısıyla çalıştırın.";
  }
  return `SSDownload bağlantı hatası: ${message}`;
}

async function sendNative(action) {
  try {
    const request = action ? { version: 2, action } : { version: 2 };
    const response = await invoke(api.runtime, "sendNativeMessage", HOST, request);
    if (!response || typeof response.ok !== "boolean") throw new Error("Yerel uygulama geçersiz yanıt verdi");
    return response;
  } catch (error) {
    throw new Error(nativeErrorMessage(error));
  }
}

// The queue is forwarded through the existing native path; a failed call is
// retried once inside events.js and then dropped without touching the caller.
if (globalThis.SSDownloadEvents?.connect) {
  globalThis.SSDownloadEvents.connect(events => sendNative({ type: "log", request: { events } }));
}

// The markup the content script hands over is bounded in bytes before it leaves the worker:
// a Turkish page's two-byte letters count as what they are on the wire, and an oversized
// capture is cut between two characters, never inside one, with the marker stating the cut.
function boundedPageHtml(value) {
  if (typeof value !== "string" || !value) return null;
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
// The report's page markup is captured by the content script in the frame the handoff names,
// at the moment of the handoff. A frame with no content script left (a worker restart, an
// about:blank document, a document that answered with an error) answers with nothing and the
// request goes without the field, never with an empty page.
async function pageHtmlFor(tabId, frameId) {
  if (!api.tabs?.sendMessage || !Number.isInteger(tabId)) return null;
  try {
    const reply = await invoke(api.tabs, "sendMessage", tabId,
      { channel: "ssdownload-background", command: "page-html" }, { frameId: Number.isInteger(frameId) ? frameId : 0 });
    return boundedPageHtml(reply?.html);
  } catch (_) { return null; }
}
function validateCapabilities(response) {
  const value=response?.result?.capabilities, modes=value?.usage_modes;
  if(!response || response.ok!==true || value?.protocol!==2 || value?.capability_version!==3
    || !modes || typeof modes.video!=="boolean" || typeof modes.file!=="boolean" || typeof modes.audio!=="boolean") {
    throw new Error("Masaüstü uygulamasının özellik protokolü uyumsuz. SSDownload masaüstü uygulamasını güncelleyin.");
  }
  // Authorized source refreshes are the only jobs the browser transfer may start;
  // the desktop lists them here and re-checks the identity on every start.
  const sourceRefreshJobs = (Array.isArray(value.source_refresh_jobs)?value.source_refresh_jobs:[])
    .filter(job=>job && typeof job.id==="string" && job.id)
    .map(job=>({id:job.id.slice(0,128),title:String(job.title||"").slice(0,180),pageUrl:safeUrl(job.page_url||"")?.href||null}))
    .slice(0,32);
  return {status:"ready",protocol:2,capabilityVersion:3,siteEntries:value.site_entries===true,browserTakeover:value.browser_takeover===true,
    usageModes:{video:modes.video,file:modes.file,audio:modes.audio},
    sourceRefreshJobs,onboardingVersion:Number(value.onboarding_version)||0,settingsRevision:Number(value.settings_revision)||0,
    completedJobs:Number.isInteger(value.completed_jobs)?value.completed_jobs:null,checkedAt:Date.now(),error:null};
}
async function broadcastCapabilities() {
  if(!api.tabs?.query || !api.tabs?.sendMessage) return;
  try {
    const tabs=await invoke(api.tabs,"query",{});
    for(const tab of tabs||[]) if(Number.isInteger(tab.id)) invoke(api.tabs,"sendMessage",tab.id,{channel:"ssdownload-background",command:"usage-modes",
      status:capabilityState.status,usageModes:capabilityState.usageModes,siteEntries:capabilityState.siteEntries===true,observerToken,error:capabilityState.error}).catch(()=>{});
  } catch (_) {}
}
async function refreshCapabilities() {
  if(Date.now()-capabilityState.checkedAt<CAPABILITY_CACHE_AGE) return capabilityState;
  if(capabilityTask) return capabilityTask;
  capabilityTask=(async()=>{
    try {
      const response=await invoke(api.runtime,"sendNativeMessage",HOST,{version:2,action:{type:"capabilities"}});
      capabilityState=validateCapabilities(response);
      if (capabilityState.completedJobs !== null) noteCompletedJobs(capabilityState.completedJobs);
    } catch(error) {
      capabilityState={status:"error",usageModes:null,sourceRefreshJobs:[],checkedAt:Date.now(),error:nativeErrorMessage(error)};
    }
    // A tab switch or a debounced discovery refresh re-reads facts the tabs
    // already hold; only a signature they have never seen reaches the broadcast.
    const signature=JSON.stringify([capabilityState.status,capabilityState.protocol,capabilityState.capabilityVersion,
      capabilityState.usageModes,capabilityState.siteEntries,capabilityState.sourceRefreshJobs,
      capabilityState.onboardingVersion,capabilityState.settingsRevision,capabilityState.error]);
    if(signature!==capabilitySignature) {
      capabilitySignature=signature;
      await broadcastCapabilities();
    }
    return capabilityState;
  })().finally(()=>{capabilityTask=null;});
  return capabilityTask;
}
function modeAllows(kind) {
  const modes=capabilityState.status==="ready"?capabilityState.usageModes:null;
  if(kind==="media"||kind==="inspect"||kind==="relay") return !!modes&&(modes.video||modes.audio||modes.file);
  return !!modes?.[kind];
}

// --------------------------------------------------------------- native picker
// The desktop application owns the download UI (name, quality, FPS, duration and
// advanced tracks). The browser only hands over the observed candidate together
// with the document/frame context and the request context it was captured with.
function baseFilename(item) {
  const raw = item.filename || pathFilename(item.url) || "";
  const base = String(raw).replace(/\.(?:mp4|mkv|webm|m3u8|mpd|m4a|mp3)$/i, "")
    .replace(/[\\/:*?"<>|\u0000-\u001f]/g, "_").slice(0, 150);
  return base || null;
}
// The desktop fills the picker's name from the analysed title, and the user's edit in
// the picker always wins at confirm time. The browser therefore suggests a name only
// when the page itself named the content - a descriptive observed title or a
// page-provided file name - and never a technical address basename such as "watch"
// or "videoplayback".
function suggestedFilename(item) {
  const title = String(item.title || "").trim();
  if (title && item.titleSource && item.titleSource !== "generic") return baseFilename({ url: "", filename: title });
  return baseFilename({ url: "", filename: item.filename || "" });
}
// The exact observed URL is part of the identity: a renewed signed address must
// not silently reuse the launch the desktop still considers active.
function launchKey(tabId, item, frameId) {
  return [tabId, frameId ?? 0, item.documentId || "", item.playerId || "", item.url || ""].join("\n");
}
function launchIdFor(tabId, item, frameId) {
  const key = launchKey(tabId, item, frameId);
  let entry = launches.get(key);
  if (!entry) {
    entry = { id: crypto.randomUUID(), tabId, url: String(item.url || "").slice(0, 16384), at: Date.now() };
    launches.set(key, entry);
    while (launches.size > 200) launches.delete(launches.keys().next().value);
    invoke(api.storage.session, "set", { browserLaunches: Array.from(launches) }).catch(() => {});
  }
  return entry.id;
}

function siteOf(value) {
  const target = safeUrl(value);
  return target ? target.origin : null;
}
function cookieMatchesTarget(cookie, target) {
  const request = new URL(target), domain = String(cookie?.domain || "").replace(/^\./, "").toLowerCase();
  const host = request.hostname.toLowerCase(), path = String(cookie?.path || "/");
  if (!domain || (cookie?.hostOnly ? host !== domain : host !== domain && !host.endsWith(`.${domain}`))) return false;
  if (!path.startsWith("/") || !(request.pathname === path || request.pathname.startsWith(path) && (path.endsWith("/") || request.pathname[path.length] === "/"))) return false;
  return !cookie?.secure || request.protocol === "https:";
}
function samePartition(left, right) {
  return Boolean(left) && Boolean(right) && left.topLevelSite === right.topLevelSite && Boolean(left.hasCrossSiteAncestor) === Boolean(right.hasCrossSiteAncestor);
}
function partitionSite(partition) {
  const value = partition?.topLevelSite;
  if (typeof value !== "string" || Boolean(partition?.hasCrossSiteAncestor)) return null;
  try {
    const parsed = new URL(value);
    return /^https?:$/.test(parsed.protocol) && !parsed.username && !parsed.password && parsed.pathname === "/" && !parsed.search && !parsed.hash ? parsed.origin : null;
  } catch (_) { return null; }
}
function hasSessionConsent(scopes, site, storeId) {
  return Array.isArray(scopes) && scopes.some(scope => scope?.site === site && scope?.storeId === storeId);
}
async function currentCookieStore(tabId) {
  const tab = await invoke(api.tabs, "get", tabId);
  if (typeof tab?.cookieStoreId === "string" && tab.cookieStoreId) return tab.cookieStoreId;
  if (typeof api.cookies?.getAllCookieStores !== "function") throw new Error("Bu Chrome sürümünde geçerli sekmenin çerez deposu doğrulanamıyor.");
  const matches = (await invoke(api.cookies, "getAllCookieStores")).filter(store => typeof store?.id === "string" && store.id && Array.isArray(store.tabIds) && store.tabIds.includes(tabId));
  if (matches.length !== 1) throw new Error("Geçerli sekmenin çerez deposu tek olarak doğrulanamadı.");
  return matches[0].id;
}
function cookieTargets(item) {
  const targets = [], seen = new Set(), add = value => { const target = safeUrl(value)?.href; if (target && !seen.has(target)) { seen.add(target); targets.push(target); } };
  add(item.url);
  for (const context of [item.requestContext, ...(item.requestChain || [])]) {
    add(context?.url);
    for (const redirect of context?.redirects || []) add(redirect?.url);
  }
  if (targets.length > 64) throw new Error("Seçili kaynak zinciri için çerez hedefi sınırı aşıldı.");
  return targets;
}
// Cookie consent is granted only through the explicit context-menu gesture and is
// scoped to one site origin and one cookie store. Nothing is read by default.
async function grantSessionConsent(tabId, pageUrl) {
  const site = siteOf(pageUrl);
  if (!site) throw new Error("Oturum izni için geçerli bir sayfa adresi gerekli.");
  const granted = await invoke(api.permissions, "request", { permissions: ["cookies"] });
  if (!granted?.granted && granted !== true) throw new Error("Çerez izni verilmedi; site oturumu kullanılamaz.");
  const storeId = await currentCookieStore(tabId);
  const scopes = (await invoke(api.storage.local, "get", { sessionScopes: [] })).sessionScopes || [];
  const next = [...scopes.filter(scope => !(scope?.site === site && scope?.storeId === storeId)), { site, storeId, grantedAt: Date.now() }];
  await invoke(api.storage.local, "set", { sessionScopes: next.slice(-64) });
  logEvent("ext.session.consent", { level: "info", outcome: "ok", host: new URL(site).hostname, detail: "granted" });
  return { site, storeId };
}
async function revokeSessionConsent(tabId, pageUrl) {
  const site = siteOf(pageUrl) || siteOf((await invoke(api.tabs, "get", tabId))?.url);
  if (!site) throw new Error("Oturum izni kaldırmak için geçerli bir sayfa adresi gerekli.");
  const scopes = (await invoke(api.storage.local, "get", { sessionScopes: [] })).sessionScopes || [];
  await invoke(api.storage.local, "set", { sessionScopes: scopes.filter(scope => scope?.site !== site) });
  logEvent("ext.session.consent", { level: "info", outcome: "ok", host: new URL(site).hostname, detail: "revoked" });
  return { site };
}
// Headers and (only with recorded consent) scoped cookies for one candidate. The
// permission and the site scope are checked before any cookie store is resolved, so
// a site without consent never touches the cookie API; once a scope exists, a store
// or partition that cannot be verified is an explicit error, never a silent empty
// export for a site the user granted.
async function sessionMetadata(tabId, item) {
  const observed = item.requestHeaders || {};
  const headers = {};
  for (const name of ["origin", "accept", "accept-language", "user-agent"]) if (observed[name]) headers[name] = observed[name];
  // Only the observed Referer travels; an unobserved request is never given one.
  const referer = observed.referer || null;
  const page = safeUrl(item.pageUrl)?.href || null;
  if (item.requestContext?.observedAt && Date.now() - item.requestContext.observedAt > REQUEST_CONTEXT_MAX_AGE) {
    const error = new Error("Kaynağın istek bağlamı eskidi. Videoyu yeniden oynatıp yeniden deneyin.");
    error.code = "SSD-EXT-005";
    throw error;
  }
  const site = siteOf(page) || siteOf(item.url);
  if (!site) return { headers, referer };
  // The permission answer decides whether a recorded session may travel at all: a failing
  // query must not be read as "not granted", because that would silently drop the consent
  // the user recorded. A genuine false still means no permission and ships no cookies.
  let granted;
  try {
    granted = await invoke(api.permissions, "contains", { permissions: ["cookies"] });
  } catch (_) {
    const error = new Error("Site oturum izni sorgulanamadı. Tarayıcıyı yeniden başlatıp izni bu sekmede yeniden verin.");
    error.code = "SSD-EXT-001";
    throw error;
  }
  if (!granted) return { headers, referer };
  const scopes = (await invoke(api.storage.local, "get", { sessionScopes: [] })).sessionScopes || [];
  if (!Array.isArray(scopes) || !scopes.some(scope => scope?.site === site)) return { headers, referer };
  const storeId = await currentCookieStore(tabId);
  if (!hasSessionConsent(scopes, site, storeId)) {
    const error = new Error("Site oturum izni bu sekmenin çerez deposu için verilmiş değil. İzni bu sekmede yeniden verin.");
    error.code = "SSD-EXT-001";
    throw error;
  }
  if (typeof api.cookies?.getPartitionKey !== "function") throw new Error("Bu Chrome sürümünde bölümlenmiş oturum kapsamı doğrulanamıyor. Tarayıcıyı güncelleyin veya site oturumu iznini kaldırın.");
  const partitionResult = await invoke(api.cookies, "getPartitionKey", { tabId, frameId: Number.isInteger(item.frameId) ? item.frameId : 0 });
  const partition = partitionResult?.partitionKey || partitionResult;
  const partitionKey = partitionSite(partition);
  if (!partitionKey) throw new Error("Bu Chrome bölümünün oturum kapsamı güvenle aktarılamıyor.");
  const cookies = new Map();
  for (const target of cookieTargets(item)) {
    const ordinary = await invoke(api.cookies, "getAll", { url: target, storeId });
    const partitioned = await invoke(api.cookies, "getAll", { url: target, storeId, partitionKey: partition });
    for (const cookie of [...ordinary, ...partitioned]) {
      if (cookie?.storeId !== storeId || !cookieMatchesTarget(cookie, target)) continue;
      if (cookie.partitionKey && !samePartition(cookie.partitionKey, partition)) continue;
      const value = { name: cookie.name, value: cookie.value, domain: cookie.domain, path: cookie.path || "/", secure: Boolean(cookie.secure),
        http_only: Boolean(cookie.httpOnly), host_only: Boolean(cookie.hostOnly), expires: cookie.expirationDate || null,
        store_id: storeId, partition_key: cookie.partitionKey ? partitionKey : null };
      cookies.set(JSON.stringify([value.name, value.domain, value.path, value.partition_key]), value);
      if (cookies.size > 500) throw new Error("Seçili kaynak zincirinin çerez sınırı aşıldı.");
    }
  }
  return { headers, referer, session_cookies: [...cookies.values()] };
}
async function mediaRequest(tabId, item, session, kind, pageHtml) {
  const request = { url: item.url, kind, headers: session.headers, referer: session.referer,
    filename: suggestedFilename(item), container: null, playlist: false, page_url: item.pageUrl || null };
  // The picker's error report carries the acted-on document's own markup when the page
  // could produce it; a page that answered nothing leaves the field out entirely.
  if (pageHtml) request.page_html = pageHtml;
  if (session.session_cookies?.length) request.session_cookies = session.session_cookies;
  const identity = item.candidateId || item.videoId;
  if (identity) request.source_identity = { video_id: identity, frame_id: item.frameId || 0, document_id: item.documentId || null, page_url: item.pageUrl || null };
  const tracks = (item.tracks || []).filter(track => ["subtitles", "captions"].includes(track.kind) && safeUrl(track.url) && track.language);
  for (const track of tracks) {
    const context = item.requestChain?.find(entry => entry.url === track.url);
    const access = await sessionMetadata(tabId, { ...item, url: track.url, requestHeaders: context?.headers || {}, requestContext: context || item.requestContext, requestChain: [] });
    (request.external_subtitles ||= []).push({ url: track.url, language: track.language, label: track.label || track.language,
      kind: track.kind, is_default: Boolean(track.isDefault), headers: access.headers, referer: access.referer });
    if (access.session_cookies?.length) (request.session_cookies ||= []).push(...access.session_cookies);
  }
  return request;
}
// Opens the native picker for one explicit candidate. The candidate list is never
// auto-narrowed to an arbitrary first ambiguous source: only the observed player
// the user acted on, or an explicitly chosen candidate, reaches this function.
async function openMediaPicker(tabId, item, frameId, requestedKind) {
  lastHandoffAt = Date.now();
  if (!detectionEnabled) throw new Error("SSDownload kapalı. Açmak için araç çubuğundaki simgeye tıklayın.");
  await refreshCapabilities();
  const kind = requestedKind === "audio" || item.kind === "audio" ? "audio" : "video";
  if (!modeAllows(kind)) throw new Error(capabilityState.error || (kind === "audio" ? "Ses kullanım modu kapalı." : "Video kullanım modu kapalı."));
  const source = publicSource(tabId, item);
  const frame = Number.isInteger(frameId) ? frameId : (Number.isInteger(source.frameId) ? source.frameId : 0);
  const session = await sessionMetadata(tabId, source);
  const request = await mediaRequest(tabId, source, session, kind, await pageHtmlFor(tabId, frame));
  const launch_id = launchIdFor(tabId, source, frame);
  try {
    const response = await sendNative({ type: "browser_media", launch_id, session_consent: Boolean(session.session_cookies?.length), request });
    if (!response.ok) throw new Error(String(response.message || "") || "Masaüstü indirme penceresi açılamadı.");
    logEvent("ext.media.open", { level: "info", outcome: "ok", host: urlHost(source.url) });
    return response;
  } catch (error) {
    logEvent("ext.media.fail", { level: "warn", outcome: "failed", code: replyCode(error) || "SSD-BRG-005", host: urlHost(source.url), detail: String(error?.message || error).slice(0,300) });
    if (replyCode(error) === "SSD-MED-002") notifyNoSingleMedia(tabId);
    throw error;
  }
}
// Direct context-menu submissions keep the existing digest de-duplication so a
// repeated click cannot queue the same job twice.
async function contextAdd(tabId, target, kind, pageUrl, label, frameId) {
  lastHandoffAt = Date.now();
  const session = await sessionMetadata(tabId, { url: target, pageUrl });
  const request = { url: target, kind, headers: session.headers, referer: session.referer, page_url: pageUrl || null, playlist: false,
    container: kind === "video" ? "mp4" : null, filename: baseFilename({ url: target, filename: label }) };
  // A plain file link is an address the user pointed at, not a page handoff: the markup
  // belongs to the page and media handoffs only, so this path carries it for those kinds.
  if (kind !== "file") {
    const pageHtml = await pageHtmlFor(tabId, frameId);
    if (pageHtml) request.page_html = pageHtml;
  }
  if (session.session_cookies?.length) request.session_cookies = session.session_cookies;
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(JSON.stringify([tabId, kind, target, request.filename])));
  const key = Array.from(new Uint8Array(digest), byte => byte.toString(16).padStart(2, "0")).join("");
  let ticket = tickets.get(key);
  if (ticket && ticket.tabId === undefined) ticket.tabId = tabId;
  if (!ticket) {
    ticket = { id: crypto.randomUUID(), submitted: false, tabId };
    tickets.set(key, ticket);
    while (tickets.size > 100) tickets.delete(tickets.keys().next().value);
  }
  await invoke(api.storage.session, "set", { downloadTickets: Array.from(tickets) });
  if (ticket.submitted) return { ok: true, duplicate: true };
  const action = { type: "add", request: { ...request, request_id: ticket.id } };
  const response = await sendNative(action);
  noteNativeReply(action, response);
  if (response.ok) { ticket.submitted = true; await invoke(api.storage.session, "set", { downloadTickets: Array.from(tickets) }); }
  return response;
}
// The relay control surface stays worker-internal: relay-background.js exports it in
// the same service-worker global and the explicit context-menu job action below is
// the only entry point. The desktop never opens a port to this worker and content
// scripts cannot reach the surface, so no arbitrary job command travels through a page.
function relayFrameContext(tabId, job) {
  return Promise.resolve(api.webNavigation?.getAllFrames ? invoke(api.webNavigation, "getAllFrames", { tabId }) : []).then(list => {
    const wanted = safeUrl(job.pageUrl)?.href || null;
    const frames = Array.isArray(list) ? list : [];
    const match = (wanted ? frames.find(frame => frame.url === wanted) : null) || frames.find(frame => frame.frameId === 0);
    if (!match || typeof match.documentId !== "string" || !match.documentId) throw new Error("Aktarım için kaynak sekmesi ve belgesi bulunamadı. İndirilecek sayfayı bu sekmede açıp yeniden deneyin.");
    return { jobId: job.id, tabId, frameId: match.frameId, documentId: match.documentId, restart: false };
  });
}
async function startRelayForJob(tabId, job, restart) {
  const control = globalThis.SSDownloadRelayControl;
  if (!control?.start) throw new Error("Tarayıcı aktarımı bu eklenti sürümünde kullanılamıyor.");
  const context = await relayFrameContext(tabId, job);
  const result = await control.start({ ...context, restart: restart === true });
  logEvent("ext.relay.start", { level: "info", outcome: "ok", host: urlHost(job.pageUrl), detail: restart ? "restart" : "start" });
  return result;
}

// Per-tab explicit candidates and authorized source refreshes. Ambiguous, preview
// and ad-suspected entries never reach the picker implicitly; the submenu lists
// every selectable source the discovery cap retains (MAX_MEDIA_PER_TAB), in the
// discovered order so an entry's index stays stable, each with its ordinal and a
// separator every CANDIDATE_MENU_GROUP entries that groups the list without
// truncating it.
const candidateMenus = new Map(), relayMenus = new Map();
const CANDIDATE_MENU_GROUP = 24;
let menuTask = null, menuSignature = null;
function menuCandidateLabel(item) {
  const title = String(item.title || item.filename || urlHost(item.url) || "Kaynak").slice(0, 60);
  const flags = [item.preview ? "önizleme" : null, item.suspectedAd ? "reklam şüphesi" : null, item.ambiguous ? "belirsiz" : null].filter(Boolean).join(", ");
  return flags ? `${title} · ${flags}` : title;
}
async function activeTabId() {
  if (!api.tabs?.query) return null;
  try { const tabs = await invoke(api.tabs, "query", { active: true, currentWindow: true }); return Number.isInteger(tabs?.[0]?.id) ? tabs[0].id : null; } catch (_) { return null; }
}
async function createMenus() {
  await refreshCapabilities();
  if(menuTask){await menuTask;return createMenus();}
  menuTask=(async()=>{
  // The active tab and its candidates are read inside the serialized task so a
  // debounced rebuild cannot overwrite a newer menu with a stale signature.
  const active = await activeTabId();
  const selectable = active === null ? [] : selectableRecords(active).slice(0, MAX_MEDIA_PER_TAB);
  const candidates = selectable.map(item => publicSource(active, item));
  const relays = capabilityState.status==="ready" ? (capabilityState.sourceRefreshJobs||[]) : [];
  const signature=JSON.stringify([capabilityState.status,capabilityState.usageModes,capabilityState.siteEntries===true,detectionEnabled,active,selectable.length,
    candidates.map(item=>[item.candidateId||item.videoId||item.url,item.title||"",item.kind]),
    relays.map(job=>[job.id,job.title,job.pageUrl])]);
  if(signature===menuSignature)return;
  menuSignature=signature;
  await invoke(api.contextMenus, "removeAll");
  candidateMenus.clear();relayMenus.clear();
  const entries = [];
  if(modeAllows("file")) entries.push({ id:"ssdownload-link",title:"Bağlantıyı SSDownload ile indir",contexts:["link"] });
  if(modeAllows("video")) {
    entries.push({id:"ssdownload-video",title:"Videoyu SSDownload ile indir",contexts:["video"]});
    // The page's own entry is the desktop's privilege, never an extension default: a
    // desktop that does not offer site entries never sees the item registered at all.
    if(capabilityState.siteEntries===true) entries.push({id:"ssdownload-page",title:"Sayfayı SSDownload ile indir",contexts:["page"]});
  }
  if(modeAllows("audio")) entries.push({id:"ssdownload-audio",title:"Sesi SSDownload ile indir",contexts:["audio"]});
  if(modeAllows("media")) entries.push({id:"ssdownload-media",title:"İndirme penceresini aç…",contexts:["page","video","audio"]});
  if(candidates.length && active !== null) {
    candidateMenus.set(active, candidates);
    const contexts = ["page","video","audio"];
    entries.push({id:"ssdownload-candidates",title:`Kaynak seç (${candidates.length})…`,contexts});
    candidates.forEach((item,index)=>{
      if(index && index % CANDIDATE_MENU_GROUP === 0) entries.push({id:`ssdownload-candidate-sep:${index}`,parentId:"ssdownload-candidates",type:"separator",contexts});
      entries.push({id:`ssdownload-candidate:${index}`,parentId:"ssdownload-candidates",title:`${index+1}. ${menuCandidateLabel(item)}`,contexts});
    });
  }
  if(relays.length && active !== null) {
    relayMenus.set(active, relays);
    const contexts = ["page","video","audio","link"];
    entries.push({id:"ssdownload-relay",title:"Tarayıcı aktarımı (kaynak yenileme)…",contexts});
    relays.forEach((job,index)=>{
      entries.push({id:`ssdownload-relay:${index}`,parentId:"ssdownload-relay",title:job.title||job.id,contexts});
      entries.push({id:`ssdownload-relay-restart:${index}`,parentId:"ssdownload-relay",title:`${job.title||job.id} · yeniden başlat`,contexts});
    });
  }
  entries.push({id:"ssdownload-session-grant",title:"Bu site için oturum çerezlerine izin ver",contexts:["page","video","audio","link"]});
  entries.push({id:"ssdownload-session-revoke",title:"Site oturum iznini kaldır",contexts:["page","video","audio","link"]});
  entries.push({id:"ssdownload-app",title:"SSDownload uygulamasını aç",contexts:["page","video","audio","link"]});
  for (const entry of entries) {
    try {
      api.contextMenus.create(entry, () => { if (api.runtime.lastError) menuSignature = null; });
    } catch (_) { menuSignature = null; }
  }
  })().finally(()=>{menuTask=null;});
  return menuTask;
}
// The desktop window may be hidden behind the browser; a status line on the page
// affordance is the only feedback a context-menu action can produce.
function notifyTab(tabId, message, error = false) {
  if (!api.tabs?.sendMessage || !Number.isInteger(tabId)) return;
  invoke(api.tabs, "sendMessage", tabId, { channel: "ssdownload-background", command: "status", message, error }).catch(() => {});
}
function notifyNoSingleMedia(tabId) {
  if (!api.tabs?.sendMessage || !Number.isInteger(tabId)) return;
  invoke(api.tabs, "sendMessage", tabId, { channel: "ssdownload-background", command: "no-single-media" }).catch(() => {});
}
// Candidate flags change while the page runs, so the explicit-selection submenu is
// rebuilt on a short debounce; the menu signature keeps an unchanged list free.
let menuRefreshTimer = null;
function scheduleMenuRefresh() {
  if (menuRefreshTimer) return;
  menuRefreshTimer = setTimeout(() => { menuRefreshTimer = null; createMenus().catch(() => {}); }, 250);
}
async function runGuarded(tabId, purpose, task, host) {
  try {
    // The switch gates every submission, not just page discovery; the toolbar
    // icon is the only place to turn it back on.
    if (!detectionEnabled) throw new Error("SSDownload kapalı. Açmak için araç çubuğundaki simgeye tıklayın.");
    await refreshCapabilities();
    if(!modeAllows(purpose)) throw new Error(capabilityState.error || "Bu kullanım modu masaüstü ayarlarında kapalı.");
    const response = await task();
    if (response && response.ok === false) throw new Error(response.message || "İşlem tamamlanamadı.");
    tabStatus.set(tabId, { message: response?.message || (response?.duplicate ? "Bu indirme zaten kuyruğa eklendi." : null), error: false });
    logEvent("ext.menu.submit", { level: "info", outcome: "ok", host, detail: purpose });
    return response;
  } catch (error) {
    tabStatus.set(tabId, { message: error.message, error: true });
    logEvent("ext.menu.fail", { level: "warn", outcome: "failed", code: replyCode(error) || "SSD-EXT-010", host, detail: purpose });
    notifyTab(tabId, String(error?.message || error), true);
    throw error;
  }
}
// Every selectable source of one tab, flags included, so the submenu can list them.
function selectableRecords(tabId) {
  return videoRecords(tabId).filter(item => item.selectable === true && item.downloadable === true);
}
// One exact source for a context-menu gesture. The acted-on address wins; a page with
// exactly one selectable source is unambiguous; anything else must be chosen from the
// candidate submenu instead of silently taking the first entry.
function mediaCandidate(tabId, info, tab, kind) {
  const target = safeUrl(info.linkUrl || info.srcUrl || info.pageUrl || tab?.url)?.href || null;
  if (!target) throw new Error("Burada indirilebilir bir kaynak bulunamadı.");
  const records = selectableRecords(tabId);
  const exact = records.find(item => item.url === target);
  if (exact) return exact;
  const audioOnly = kind === "audio" ? records.filter(item => item.kind === "audio" || (item.tracks || []).some(track => track.kind === "audio")) : records;
  const usable = audioOnly.length ? audioOnly : records;
  if (usable.length === 1) return usable[0];
  if (!usable.length) throw new Error("Bu sayfada indirilebilir kaynak bulunamadı. Videoyu oynatıp yeniden deneyin.");
  throw new Error("Bu sayfada birden fazla kaynak var. Sağ tıklayıp Kaynak seç menüsünden indirilecek kaynağı seçin.");
}
// The toolbar icon opens popup.html; detection is switched there.
if (api.alarms?.create) {
  try { api.alarms.create("ssdownload-completion-poll", { periodInMinutes: 1 }); } catch (_) {}
  api.alarms.onAlarm?.addListener(alarm => {
    if (alarm?.name !== "ssdownload-completion-poll" || Date.now() - lastHandoffAt > COMPLETION_POLL_WINDOW) return;
    capabilityState.checkedAt = 0;
    refreshCapabilities().catch(() => {});
  });
}
api.runtime.onInstalled.addListener(()=>{createMenus().catch(()=>{});sessionReady.then(()=>applyActionState()).catch(()=>{});});
api.runtime.onStartup.addListener(()=>{createMenus().catch(()=>{});sessionReady.then(()=>applyActionState()).catch(()=>{});});
api.tabs?.onActivated?.addListener?.(() => { createMenus().catch(() => {}); });
api.tabs?.onUpdated?.addListener?.((_tabId, change) => { if (change?.url || change?.status === "complete") createMenus().catch(() => {}); });
createMenus().catch(()=>{});
// A restarted worker must present the persisted switch, not a default label.
sessionReady.then(()=>applyActionState()).catch(()=>{});

logEvent("ext.session", { level: "info", outcome: "ok",
  detail: `version=${extensionVersion()} protocol=${capabilityState.status === "ready" ? capabilityState.protocol : "unknown"}` });

api.contextMenus.onClicked.addListener((info, tab) => {
  const tabId = tab && tab.id;
  if (!Number.isInteger(tabId)) return;
  const menuId = String(info.menuItemId || "");
  const pageUrl = info.pageUrl || (tab && tab.url) || null;
  const target = info.linkUrl || info.srcUrl || pageUrl;
  if (menuId.startsWith("ssdownload-candidate:")) {
    const chosen = candidateMenus.get(tabId)?.[Number(menuId.slice("ssdownload-candidate:".length))];
    if (!chosen) { notifyTab(tabId, "Kaynak listesi yenilendi. Menüyü yeniden açın.", true); return; }
    const chosenKind = chosen.kind === "audio" ? "audio" : "video";
    runGuarded(tabId, chosenKind, () => openMediaPicker(tabId, { ...chosen, confirmed: true }, chosen.frameId, chosenKind), urlHost(chosen.url)).catch(() => {});
    return;
  }
  if (menuId.startsWith("ssdownload-relay-restart:") || menuId.startsWith("ssdownload-relay:")) {
    const restart = menuId.startsWith("ssdownload-relay-restart:");
    const index = Number(menuId.slice((restart ? "ssdownload-relay-restart:" : "ssdownload-relay:").length));
    const job = relayMenus.get(tabId)?.[index];
    if (!job) { notifyTab(tabId, "Kaynak yenileme listesi yenilendi. Menüyü yeniden açın.", true); return; }
    runGuarded(tabId, "relay", () => startRelayForJob(tabId, job, restart), urlHost(job.pageUrl)).catch(() => {});
    return;
  }
  if (menuId === "ssdownload-session-grant") {
    grantSessionConsent(tabId, pageUrl || target).then(result => notifyTab(tabId, `${result.site} için oturum çerezleri bu indirmede kullanılabilir.`)).catch(error => notifyTab(tabId, error.message, true));
    return;
  }
  if (menuId === "ssdownload-session-revoke") {
    revokeSessionConsent(tabId, pageUrl || target).then(result => notifyTab(tabId, `${result.site} için site oturum izni kaldırıldı.`)).catch(error => notifyTab(tabId, error.message, true));
    return;
  }
  if (menuId === "ssdownload-app") {
    sendNative({ type: "show_window" })
      .then(() => { tabStatus.set(tabId, { message: null, error: false }); })
      .catch(error => {
        tabStatus.set(tabId, { message: error.message, error: true });
        notifyTab(tabId, error.message, true);
        logEvent("ext.app.open", { level: "warn", outcome: "failed", code: replyCode(error) || "SSD-BRG-005", detail: String(error?.message || error).slice(0,300) });
      });
    return;
  }
  if (menuId === "ssdownload-link") {
    if (!safeUrl(target)) return;
    // A normal file is the only direct submission left; media always opens the picker.
    runGuarded(tabId, "file", () => contextAdd(tabId, target, "file", pageUrl, null, info.frameId), urlHost(target)).catch(() => {});
    return;
  }
  if (menuId === "ssdownload-media" || menuId === "ssdownload-video" || menuId === "ssdownload-page" || menuId === "ssdownload-audio") {
    const kind = menuId === "ssdownload-audio" ? "audio" : "video";
    runGuarded(tabId, kind, () => openMediaPicker(tabId, mediaCandidate(tabId, info, tab, kind), null, kind), urlHost(target)).catch(() => {});
    return;
  }
  if (!safeUrl(target)) return;
});

async function handleContentMessage(message, sender) {
  await sessionReady;
  if(!isExtensionContent(sender) || !message || message.channel!=="ssdownload-content")return;
  const tabId=sender.tab.id;
  if(message.event==="log") { enqueueRelayEvents(message.events); return {ok:true}; }
  if(message.event==="usage-modes") {
    await refreshCapabilities();
    return {ok:capabilityState.status==="ready",status:capabilityState.status,usageModes:capabilityState.usageModes,
      siteEntries:capabilityState.siteEntries===true,observerToken,error:capabilityState.error};
  }
  if (!detectionEnabled) {
    if (message.event === "download-video" || message.event === "download-page") throw new Error("SSDownload kapalı. Açmak için araç çubuğundaki simgeye tıklayın.");
    return;
  }
  if (Array.isArray(message.players)) {
    const frameId = sender.frameId || 0;
    const key = `${tabId}\n${frameId}`;
    const previous = frames.get(key);
    const documentId = String(sender.documentId || message.documentId || "").slice(0,100);
    // Chrome keeps MessageSender.url at the content script's initial document URL
    // across history navigation. The script reports the current location; accept
    // its same-origin changes without letting a page claim another sender origin.
    const senderUrl = safeUrl(sender.url), pageUrl = safeUrl(message.pageUrl);
    const frameUrl = (pageUrl && (!senderUrl || pageUrl.origin === senderUrl.origin) ? pageUrl : senderUrl)?.href;
    if (!frameUrl) return;
    const allowedAdSignals=new Set(["player-identity","ad-container","request-host","promotional-link","playback-pattern","skip-control"]);
    const players = message.players.slice(0,100).map(p => {
      const id=String(p.id).slice(0,100);
      const tracks=(Array.isArray(p.tracks)?p.tracks:[]).slice(0,32).flatMap(track=>{
        const url=safeUrl(track?.url)?.href;
        if(!url || String(track.playerId||id)!==id) return [];
        return [{url,language:String(track.language||"").slice(0,35),label:String(track.label||"").slice(0,180),
          kind:String(track.kind||"subtitles").slice(0,32),isDefault:track.isDefault===true,playerId:id}];
      });
      const signals=Array.isArray(p.adSignals)?[...new Set(p.adSignals.filter(value=>allowedAdSignals.has(value)))]:Array.from({length:Math.min(3,Number(p.adSignals)||0)},(_,index)=>`legacy-${index}`);
      const duration=Number(p.duration);
      return {id,title:String(p.title || sender.tab.title || "Video").slice(0,180),
        urls:(p.urls || []).filter(u => typeof u === "string" && u.length <= 16384 && (u.startsWith("blob:") || !!safeUrl(u))).slice(0,20), poster:safeUrl(p.poster)?.href || null,
        explicitTitle:p.explicitTitle === true, titleSource:p.titleSource==="element"?"element":"page", visible:p.visible!==false, played:p.played===true,
        active:p.active === true, adSignals:signals.slice(0,6),tracks,duration:Number.isFinite(duration)&&duration>=0&&duration<=604800?duration:null,
        loop:p.loop === true, muted:p.muted === true, userActivated:p.userActivated === true};
    });
    while (players.length && JSON.stringify(players).length > 32768) players.pop();
    const navigated = previous && (previous.documentId !== documentId || previous.url !== frameUrl);
    const replaced = previous && previous.players.some(old=>old.urls.length && !players.some(p=>p.id===old.id && (!p.urls.length || p.urls.some(u=>old.urls.includes(u)))));
    if (navigated || replaced) {
      const values = mediaByTab.get(tabId);
      if (values) for (const [key,item] of values) if (item.frameId === frameId && (navigated
        ? item.source!=="network" || (item.documentId ? item.documentId!==documentId : item.lastSeen<previous.updatedAt)
        : item.lastSeen<previous.updatedAt || item.source!=="network")) {
        values.delete(key); manifestLinks.delete(manifestKey(tabId,item)); manifestTasks.delete(manifestKey(tabId,item));
      }
    }
    const response=frameResponses.get(key);
    const responseStatus=response && (!response.documentId || response.documentId===documentId) && response.url===frameUrl ? response.status : (!navigated?previous?.responseStatus:undefined);
    // Player signals are page markup facts, not user data: they only say that this
    // document renders a player without a media element of its own. Every address is
    // re-validated here and the list stays bounded, exactly like the sources above.
    const declared = message.playerSignals && typeof message.playerSignals === "object" ? message.playerSignals : null;
    const embeds=[], embedSeen=new Set();
    for (const embed of (Array.isArray(declared?.embeds)?declared.embeds:[]).slice(0,4)) {
      const url=safeUrl(embed?.url)?.href;
      if(!url || embedSeen.has(url)) continue;
      embedSeen.add(url);
      embeds.push({url,title:String(embed?.title||"").slice(0,120)});
    }
    // A message without the field (the in-page gesture report) leaves the document's
    // already reported markup facts in place; a new document starts without them.
    const playerSignals = declared
      ? (embeds.length || declared.containers === true || declared.playerScripts === true
        ? {embeds,containers:declared.containers===true,playerScripts:declared.playerScripts===true} : null)
      : (navigated ? null : previous?.playerSignals ?? null);
    const pending = pendingCapabilities.get(key);
    frames.set(key, {tabId, frameId, documentId, url:frameUrl, players,responseStatus,playerSignals,
      pageTitle:String(message.pageTitle||"").slice(0,180), tabTitle:String(sender.tab.title||"").slice(0,180),
      observationCapabilities:(!navigated ? previous?.observationCapabilities : null)
        || (pending?.documentId === documentId ? pending.capabilities : null),
      since:(navigated || replaced)?previous.updatedAt:(previous?.since || 0), updatedAt:Date.now()});
    pendingCapabilities.delete(key);
    evictOldestFrames();
  }
  if(message.event==="observation-capabilities") {
    const capabilitiesKey=`${tabId}\n${sender.frameId||0}`;
    const documentId = String(sender.documentId || message.documentId || "").slice(0,100);
    const frameCapabilities={pageFetchConsumed:message.capabilities?.pageFetchConsumed===true,
      pageXhrConsumed:message.capabilities?.pageXhrConsumed===true,performanceResourceUrl:message.capabilities?.performanceResourceUrl===true,
      workerResponseBody:message.capabilities?.workerResponseBody===true,mseAppendBufferAncestry:message.capabilities?.mseAppendBufferAncestry===true,
      responseCloning:message.capabilities?.responseCloning===true,drmBypass:message.capabilities?.drmBypass===true};
    const frame=frames.get(capabilitiesKey);
    if (frame && frame.documentId === documentId) {
      frame.observationCapabilities=frameCapabilities;
    } else if (!frame) {
      // The record was evicted before this one-shot report could land; hold the
      // capabilities until the frame's next heartbeat recreates its record.
      pendingCapabilities.set(capabilitiesKey, { documentId, capabilities: frameCapabilities });
      while (pendingCapabilities.size > 200) pendingCapabilities.delete(pendingCapabilities.keys().next().value);
    }
  }
  if (message.event === "play") playTimes.set(tabId, Date.now());
  if (message.event === "drm") {
    const current = tabStatus.get(tabId) || {};
    tabStatus.set(tabId, { ...current, drmCue: "Sayfa Encrypted Media Extensions kullandı. DRM korumalı akış SSDownload tarafından desteklenmez." });
  }
  saveSession();
  if (Array.isArray(message.sources)) {
    for (const source of message.sources.slice(0, 100)) {
      const sourceUrl = String(source.url || "");
      // Content-script messages originate in a page process.  Keep their storage
      // footprint subject to the same URL bound used for network observations.
      if (!sourceUrl || sourceUrl.length > 16384) continue;
      const pageUrl = safeUrl(source.pageUrl)?.href || null;
      const frame=frames.get(`${tabId}\n${sender.frameId||0}`);
      const claimedPlayer=frame?.players.find(player=>player.id===String(source.playerId||""));
      const matchingPlayers=frame?.players.filter(player=>player.urls.includes(sourceUrl))||[];
      const sourcePlayer=claimedPlayer || (matchingPlayers.length===1?matchingPlayers[0]:null);
      const sourceIdentity={frameId:sender.frameId||0,documentId:String(sender.documentId||frame?.documentId||"").slice(0,100),
        playerId:sourcePlayer?.id||null,tracks:sourcePlayer?.tracks||[]};
      if (source.kind === "blob" || source.kind === "webrtc") {
        record(tabId, {
          url: sourceUrl,
          kind: source.kind,
          label: source.kind === "webrtc" ? "WebRTC canlı akışı" : "Tarayıcı blob/MSE kaynağı",
          contentType: String(source.contentType || ""),
          filename: "",
          source: "page",
          ...sourceIdentity,
          pageUrl,
          downloadable: false,
          note: source.kind === "webrtc" ? "WebRTC doğrudan indirilemez." : "Blob adresi doğrudan indirilemez; sayfayı analiz edin.",
          drmHint: false
        });
        continue;
      }
      const classification = classify(sourceUrl, source.contentType) || (source.kind === "file" && safeUrl(sourceUrl) ? {kind:"file",label:"Dosya"} : null);
      if (classification) {
        const observed=observedSourceContext(tabId, sender, sourceUrl, pageUrl || safeUrl(sender.url)?.href);
        const playerOwnsSource=!!sourcePlayer && sourcePlayer.urls.includes(sourceUrl);
        if(!playerOwnsSource && !observed.requestContext && source.kind!=="file") continue;
        record(tabId, {
          url: sourceUrl,
          kind: classification.kind,
          label: classification.label,
          contentType: String(source.contentType || ""),
          filename: String(source.filename || "").slice(0,180),
          source: "page",
          ...sourceIdentity,
          pageUrl,
          ...observed,
          downloadable: true,
          drmHint: false
        });
      }
    }
  }
  if (message.event === "download-video") {
    const mediaKind=message.mediaKind==="audio"?"audio":"video";
    await refreshCapabilities();
    if(!modeAllows(mediaKind)) {
      throw new Error(capabilityState.error||`${mediaKind==="audio"?"Ses":"Video"} kullanım modu kapalı.`);
    }
    const frameId = sender.frameId || 0;
    const frame = frames.get(`${tabId}\n${frameId}`);
    const player=frame?.players.find(value=>value.id===String(message.playerId||""));
    if(!player) throw new Error("Bu oynatıcı artık sayfada bulunamadı. Videoyu oynatıp yeniden deneyin.");
    // videoRecords already resolved this frame's players against their own document: it
    // keeps every record in the frame/document it was observed in, follows manifest
    // ancestry, infers the single manifest of a lone blob player and falls back to that
    // player's own page address. The gesture names one player, so only its records
    // qualify - another player's stream on the same page is never substituted - and the
    // player's blob or WebRTC address never becomes a desktop URL. Entries that may only
    // be taken by explicit choice (ambiguous roots, preview, ad suspicion, blocked frame)
    // refuse here; the "Kaynak seç" submenu stays the way to reach them.
    const videoId = `${frameId}:${frame.documentId}:${player.id}`;
    const own = videoRecords(tabId).filter(item => item.frameId === frameId && safeUrl(item.url)
      && (item.playerId === player.id || (item.aliases || []).includes(videoId)));
    const item = own.length === 1 && own[0].downloadable === true
      && !own[0].ambiguous && !own[0].suspectedAd && !own[0].preview ? own[0] : null;
    if (!item) {
      if (own.some(value => value.ambiguous)) throw new Error("Bu oynatıcıyla birden fazla yayın ilişkili. Sağ tıklayıp Kaynak seç menüsünden indirilecek kaynağı seçin.");
      if (own.some(value => value.suspectedAd || value.preview)) throw new Error("Bu oynatıcı bir reklam veya önizleme olabilir; yalnız sağ tıklayıp Kaynak seç menüsünden indirilebilir.");
      const blocked = own.find(value => value.downloadable !== true && value.note);
      if (blocked) throw new Error(blocked.note);
      throw new Error("Bu oynatıcı için indirilebilir kaynak bulunamadı. Videoyu oynatıp yeniden deneyin.");
    }
    // The requested kind travels with the handoff: without it the picker would open a
    // video request for the audio affordance of the same player.
    const response = await openMediaPicker(tabId, item, frameId, mediaKind);
    tabStatus.set(tabId, { message: null, error: false });
    return { ok: true, request_id: response?.result?.request_id || null };
  }
  if (message.event === "download-page") {
    const mediaKind="video";
    await refreshCapabilities();
    if(!modeAllows(mediaKind)) {
      throw new Error(capabilityState.error||"Video kullanım modu kapalı.");
    }
    // A content script that still holds the old markup after a refresh is not an offer the
    // desktop made: the page handoff exists only while the capability says so.
    if(capabilityState.siteEntries!==true) {
      throw new Error("Sayfa düzeyi indirme masaüstü ayarlarında kapalı.");
    }
    const frameId = sender.frameId || 0;
    const frame = frames.get(`${tabId}\n${frameId}`);
    if (!frame) throw new Error("Bu sayfa artık izlenmiyor. Sayfayı yenileyip yeniden deneyin.");
    // The gesture names the document itself, so only this frame's own page candidate
    // qualifies: another frame's address, another player's stream and an inferred blob
    // relation are never substituted for it. A document discovery never confirmed a player
    // for - no genuine player evidence, preview-only, a blocked response - has no such
    // record, and its refusal states what discovery knows instead of handing the page
    // address over as if it were the film.
    const page=videoRecords(tabId).find(item=>item.playerId===null && item.kind==="page" && item.frameId===frameId && item.url===frame.url);
    if (!page) throw new Error("Bu sayfada oynatıcıya ait bir kaynak doğrulanamadı. Videoyu oynatıp yeniden deneyin.");
    if (page.downloadable !== true) throw new Error(page.note || "Bu sayfa indirilebilir bir kaynak olarak doğrulanamadı.");
    const response = await openMediaPicker(tabId, page, frameId, mediaKind);
    tabStatus.set(tabId, { message: null, error: false });
    return { ok: true, request_id: response?.result?.request_id || null };
  }
  if(message.event==="manifest" && message.manifest && safeUrl(message.manifest.url)) {
    const frameId=sender.frameId||0, documentId=sender.documentId||frames.get(`${tabId}\n${frameId}`)?.documentId;
    const knownItem=[...(mediaByTab.get(tabId)?.values()||[])].some(item=>item.frameId===frameId && item.url===message.manifest.url && (!documentId || !item.documentId || item.documentId===documentId));
    const knownRequest=latestRequestContext(tabId,frameId,documentId,message.manifest.url);
    if(!knownItem && !knownRequest) return;
    const supplied=Array.isArray(message.manifest.relations)?message.manifest.relations:Array.isArray(message.manifest.children)?message.manifest.children:[];
    const relations=normalizeRelations(supplied);
    if(JSON.stringify(relations).length<=32768) {
      manifestLinks.set(manifestKey(tabId,{url:message.manifest.url,frameId}),relations);
      while(manifestLinks.size>400) manifestLinks.delete(manifestLinks.keys().next().value);
      saveSession();
    }
  }
  // A frame that reported players or observation capabilities but produced no
  // usable candidate is recorded once per document for the failure report.
  if (Array.isArray(message.players)) {
    const frameId = sender.frameId || 0;
    const frame = frames.get(`${tabId}\n${frameId}`);
    const key = `${tabId}\n${frameId}\n${frame?.documentId || ""}`;
    if (frame && !observationLimits.has(key) && (frame.players.length || frame.observationCapabilities)
      && !videoRecords(tabId).some(video => video.frameId === frameId && video.downloadable === true)) {
      observationLimits.add(key);
      while (observationLimits.size > 200) observationLimits.delete(observationLimits.values().next().value);
      logEvent("ext.observe.limit", { level: "warn", outcome: "failed", code: "SSD-EXT-010",
        host: urlHost(frame.url), detail: observationFacts(frame) });
    }
  }
  if (Array.isArray(message.players) || Array.isArray(message.sources)) scheduleMenuRefresh();
}

// ------------------------------------------------------------------- toolbar popup
function isPopupSender(sender) {
  return sender?.id === api.runtime.id && typeof sender?.url === "string" && sender.url.endsWith("/popup.html");
}
async function hiddenHosts() {
  try { const saved = await invoke(api.storage.local, "get", { hiddenHosts: [] }); return Array.isArray(saved.hiddenHosts) ? saved.hiddenHosts : []; } catch (_) { return []; }
}
async function popupTab() {
  const tabs = await invoke(api.tabs, "query", { active: true, currentWindow: true });
  const tab = tabs?.[0];
  return Number.isInteger(tab?.id) ? tab : null;
}
async function popupState() {
  pendingCompletions = 0;
  completedBaseline = capabilityState.completedJobs ?? completedBaseline;
  applyActionState();
  const tab = await popupTab();
  const host = safeUrl(tab?.url || "")?.hostname || "";
  const hidden = await hiddenHosts();
  const selectable = tab ? selectableRecords(tab.id).slice(0, MAX_MEDIA_PER_TAB) : [];
  const media = selectable.map((item, index) => {
    const source = publicSource(tab.id, item);
    return { index, label: menuCandidateLabel(source), kind: source.kind === "audio" ? "audio" : "video" };
  });
  return { detectionEnabled, host, siteHidden: !!host && hidden.includes(host), media };
}
async function handlePopupMessage(message) {
  switch (message.command) {
    case "state": return popupState();
    case "set-detection": await setDetectionEnabled(message.value === true); return popupState();
    case "hide-site": {
      const tab = await popupTab();
      const host = safeUrl(tab?.url || "")?.hostname || "";
      if (host) {
        const hosts = new Set(await hiddenHosts());
        if (message.value === true) hosts.add(host); else hosts.delete(host);
        await invoke(api.storage.local, "set", { hiddenHosts: [...hosts].slice(-500) });
      }
      return popupState();
    }
    case "open-media": {
      const tab = await popupTab();
      if (!tab) throw new Error("Etkin sekme bulunamadı.");
      const item = selectableRecords(tab.id).slice(0, MAX_MEDIA_PER_TAB)[Number(message.index)];
      if (!item) throw new Error("Kaynak listesi yenilendi. Pencereyi yeniden açın.");
      const chosen = publicSource(tab.id, item);
      const kind = chosen.kind === "audio" ? "audio" : "video";
      await runGuarded(tab.id, kind, () => openMediaPicker(tab.id, { ...chosen, confirmed: true }, chosen.frameId, kind), urlHost(chosen.url));
      return { ok: true };
    }
    case "open-app": {
      const response = await sendNative({ type: "show_window" });
      if (!response.ok) throw new Error(String(response.message || "SSDownload açılamadı."));
      return { ok: true };
    }
    default: throw new Error("Bilinmeyen komut");
  }
}

api.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if(message?.channel==="ssdownload-popup") {
    if(!isPopupSender(sender)){sendResponse({error:"Geçersiz gönderici"});return true;}
    handlePopupMessage(message).then(result=>sendResponse(result||{ok:true}),error=>sendResponse({error:String(error?.message||error)}));
    return true;
  }
  if(message?.channel==="ssdownload-content") {
    if(!isExtensionContent(sender)){sendResponse({error:"Geçersiz içerik göndericisi"});return true;}
    handleContentMessage(message,sender).then(result=>sendResponse(result||{ok:true}),error=>sendResponse({error:String(error?.message||error)}));
    return true;
  }
  return false;
});

// ------------------------------------------------------- browser download takeover
// Off unless the desktop enables "Tarayıcı indirmelerini devral" (capability
// browser_takeover). An ordinary http(s) file download is paused, handed to the desktop
// as a file job and erased from Chrome only after the desktop accepted it; a refusal or
// an unreachable desktop resumes it in Chrome. Incognito, blob/data and extension-made
// downloads always stay in the browser, and no cookie leaves it on this path.
const TAKEOVER_EXTENSIONS = new Set(["7z","aac","apk","avi","bin","bz2","cab","deb","dmg","doc","docx","epub","exe","flac","gz","img","iso","jar","m4a","mkv","mov","mp3","mp4","msi","msix","ogg","pdf","pkg","ppt","pptx","rar","rpm","tar","tgz","torrent","wav","webm","xls","xlsx","xz","zip","zst"]);
function takeoverExtension(item) {
  const name = String(item?.filename || "").split(/[\\/]/).pop() || pathFilename(item?.finalUrl || item?.url || "") || "";
  const match = /\.([a-z0-9]{1,8})$/i.exec(name);
  return match ? match[1].toLowerCase() : "";
}
function takeoverCandidate(item) {
  if (!item || item.incognito || item.byExtensionId || item.state !== "in_progress") return false;
  const address = safeUrl(item.finalUrl || item.url);
  if (!address || !/^https?:$/.test(address.protocol)) return false;
  return TAKEOVER_EXTENSIONS.has(takeoverExtension(item));
}
async function takeOverDownload(item) {
  if (!detectionEnabled || !takeoverCandidate(item)) return;
  const state = await refreshCapabilities();
  if (state.status !== "ready" || state.browserTakeover !== true || !modeAllows("file")) return;
  const address = safeUrl(item.finalUrl || item.url).href;
  try { await invoke(api.downloads, "pause", item.id); } catch (_) { return; }
  const referer = safeUrl(item.referrer || "")?.href || null;
  const request = { url: address, kind: "file", headers: {}, referer, page_url: referer, playlist: false,
    container: null, filename: baseFilename({ url: address, filename: String(item.filename || "").split(/[\\/]/).pop() }),
    request_id: crypto.randomUUID() };
  let accepted = false;
  try {
    const response = await sendNative({ type: "add", request });
    accepted = response?.ok === true;
    logEvent(accepted ? "ext.takeover.ok" : "ext.takeover.refused", { level: accepted ? "info" : "warn",
      outcome: accepted ? "ok" : "failed", host: urlHost(address), code: accepted ? undefined : (replyCode(response) || undefined) });
  } catch (error) {
    logEvent("ext.takeover.refused", { level: "warn", outcome: "failed", host: urlHost(address), detail: String(error?.message || error).slice(0, 300) });
  }
  if (accepted) {
    lastHandoffAt = Date.now();
    try { await invoke(api.downloads, "cancel", item.id); await invoke(api.downloads, "erase", { id: item.id }); } catch (_) {}
  } else {
    try { await invoke(api.downloads, "resume", item.id); } catch (_) {}
  }
}
if (api.downloads?.onCreated) api.downloads.onCreated.addListener(item => { takeOverDownload(item).catch(() => {}); });
