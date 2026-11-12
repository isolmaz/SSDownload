"use strict";

// Extension event relay → desktop session log.
//
// Contract: the background forwards events to native as
// `{type:"log",request:{events:[…]}}`; every event carries only
// `event`/`level`/`outcome`/`code`/`host`/`job`/`detail`, where the host is a host
// name only, the code must exist in `codes.js`, and the detail is one line of at
// most 200 characters with no URL, credential or file path. At most 50 events are
// queued (the oldest is dropped when full), a native request carries at most 20
// events or 8 KB of JSON, and the queue flushes when it reaches 20 events or 5 s
// after the first queued event, whichever comes first. A failed or unavailable
// transport never throws into the caller and is retried at most once before the
// batch is dropped silently: logging must never disturb a download.
//
// Load order: after `codes.js`, before `background.js` (service-worker.js) and the
// relay workers. `connect` supplies the transport; without one events are dropped,
// so every context stays silent instead of throwing.

(function installEventRelay() {
  const MAX_QUEUE = 50;
  const FLUSH_THRESHOLD = 20;
  const FLUSH_DELAY_MS = 5000;
  const MAX_BATCH_BYTES = 8 * 1024;
  const MAX_HOST = 190;
  const MAX_JOB = 64;
  const MAX_DETAIL = 200;
  const EVENT_NAME = /^[a-z][a-z0-9._-]{0,63}$/;
  const LEVELS = new Set(["debug", "info", "warn", "error"]);
  const OUTCOMES = new Set(["ok", "retry", "partial", "failed", "skipped"]);
  const CREDENTIAL = /^(?:cookie|authorization|set-cookie|bearer|token|password|secret|session|api[_-]?key|access_token)[:=]/i;

  let queue = [];
  let timer = null;
  let draining = null;
  let transport = null;

  function knownCode(code) {
    try {
      return globalThis.SSDownloadErrorCodes?.isCode?.(code) === true;
    } catch (_) {
      return false;
    }
  }

  // A full address is collapsed to its host; anything that is not a bare host name
  // is rejected so a URL, query or cookie value can never reach the log.
  function hostOnly(value) {
    if (typeof value !== "string") return null;
    let text = value.trim();
    if (/^[a-z][a-z0-9+.-]*:\/\//i.test(text)) {
      try {
        text = new URL(text).hostname;
      } catch (_) {
        return null;
      }
    }
    if (!text || text.length > MAX_HOST || /[/\\:@?#\s]/.test(text) || text.includes("..")) return null;
    return text;
  }

  function boundedToken(value, limit) {
    if (typeof value !== "string") return null;
    const text = value.trim();
    if (!text || text.length > limit || /[\r\n\0]/.test(text)) return null;
    return text;
  }

  function scrubWord(word) {
    const trimmed = word.replace(/^[("'[]+/, "").replace(/[)"'\],;.]+$/, "");
    if (/^https?:\/\//i.test(trimmed)) {
      try {
        return new URL(trimmed).hostname;
      } catch (_) {
        return "[url]";
      }
    }
    if (/^[A-Za-z]:[\\/]/.test(trimmed) || trimmed.includes("\\")) return "[path]";
    if (trimmed.length > 64 && /^[A-Za-z0-9._-]+$/.test(trimmed)) return "[redacted]";
    if (CREDENTIAL.test(trimmed)) return "[redacted]";
    return word;
  }

  // One line, length-bounded, with credential-free text only.
  function oneLine(value) {
    if (typeof value !== "string") return null;
    const text = value
      .replace(/[\u0000-\u001f\u007f]+/g, " ")
      .split(/\s+/)
      .filter(Boolean)
      .map(scrubWord)
      .join(" ")
      .slice(0, MAX_DETAIL)
      .trim();
    return text || null;
  }

  // Keeps only the contract keys; an unknown name drops the event, every other
  // invalid field is omitted instead of failing the whole batch.
  function normalize(name, fields) {
    if (typeof name !== "string" || !EVENT_NAME.test(name)) return null;
    const source = fields && typeof fields === "object" ? fields : {};
    const event = { event: name };
    const level = typeof source.level === "string" ? source.level.trim().toLowerCase() : "";
    event.level = LEVELS.has(level) ? level : "info";
    const outcome = typeof source.outcome === "string" ? source.outcome.trim().toLowerCase() : "";
    if (OUTCOMES.has(outcome)) event.outcome = outcome;
    if (knownCode(source.code)) event.code = source.code;
    const host = hostOnly(source.host);
    if (host) event.host = host;
    const job = boundedToken(source.job, MAX_JOB);
    if (job) event.job = job;
    const detail = oneLine(source.detail);
    if (detail) event.detail = detail;
    return event;
  }

  function takeBatch() {
    const batch = [];
    let bytes = 2;
    while (queue.length && batch.length < FLUSH_THRESHOLD) {
      const next = queue[0];
      const size = JSON.stringify(next).length + (batch.length ? 1 : 0);
      if (batch.length && bytes + size > MAX_BATCH_BYTES) break;
      batch.push(queue.shift());
      bytes += size;
    }
    return batch;
  }

  async function deliver(batch) {
    if (typeof transport !== "function") return false;
    for (let attempt = 0; attempt < 2; attempt += 1) {
      try {
        const response = await transport(batch.slice());
        if (!response || response.ok !== false) return true;
      } catch (_) {
        /* A dead native host is retried once, then the batch is dropped. */
      }
    }
    return false;
  }

  async function drain() {
    while (queue.length) {
      const batch = takeBatch();
      const delivered = await deliver(batch);
      // The failed batch was already taken out of the queue, so only it is dropped:
      // events queued while it was in flight stay for the next flush.
      if (!delivered) break;
    }
  }

  function scheduleTimer() {
    if (timer !== null || !queue.length) return;
    timer = setTimeout(() => {
      timer = null;
      flush();
    }, FLUSH_DELAY_MS);
  }

  function flush() {
    if (draining) return draining;
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    if (!queue.length) return Promise.resolve();
    draining = Promise.resolve()
      .then(drain)
      .catch(() => {})
      .finally(() => {
        draining = null;
        if (queue.length) scheduleTimer();
      });
    return draining;
  }

  function log(event, fields) {
    const normalized = normalize(event, fields);
    if (!normalized) return null;
    if (queue.length >= MAX_QUEUE) queue.shift();
    queue.push(normalized);
    if (queue.length === 1) scheduleTimer();
    if (queue.length >= FLUSH_THRESHOLD) flush();
    return normalized;
  }

  function connect(send) {
    transport = typeof send === "function" ? send : null;
  }

  if (typeof globalThis !== "undefined") {
    globalThis.SSDownloadEvents = { log, flush, connect };
  }
})();
