"use strict";

// Browser-transfer control is deliberately separate from discovery/background.js.
// Only an extension UI can authorize a run; page scripts never receive a writable
// postMessage or network endpoint.
(function installBrowserRelay() {
  const relayApi = globalThis.browser || chrome;
  const relayPromiseApi = typeof globalThis.browser !== "undefined";
  const RELAY_HOST = "com.ssdownload.desktop";
  const CONTROL_CHANNEL = "ssdownload-relay-control";
  const INTERNAL_CHANNEL = "ssdownload-relay-internal";
  const MAX_CHUNK_BYTES = 512 * 1024;
  const MAX_NATIVE_FRAME_BYTES = 1024 * 1024;
  const MAX_NATIVE_QUEUE_BYTES = 8 * 1024 * 1024;
  const nativeEncoder = new TextEncoder();
  const MAX_CHECKPOINT_BYTES = 4096;
  const MAX_SAVED_BYTES = 512 * 1024;
  const AUTO_RESUME_ALARM = "ssdownload-browser-transfer-resume";
  const AUTO_RESUME_DELAY_MINUTES = 0.5;
  const jobs = new Map();
  let nativePort = null;
  let resumeTimer = null;
  let resumeTask = null;
  let nativeCurrent = null;
  let nativeQueueBytes = 0;
  const nativeQueue = [];

  function call(target, method, ...args) {
    if (relayPromiseApi) return target[method](...args);
    return new Promise((resolve, reject) => target[method](...args, value => {
      const error = relayApi.runtime.lastError;
      if (error) reject(new Error(error.message)); else resolve(value);
    }));
  }

  function uuid(value) { return typeof value === "string" && /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value); }
  function safeDocument(value) { return typeof value === "string" && value.length > 0 && value.length <= 128; }
  function nativeError(error) {
    const message = String(error?.message || error || "Bilinmeyen yerel bağlantı hatası");
    if (/native messaging host.*not found|specified native messaging host|no such native application/i.test(message)) return "SSDownload yerel bağlantısı kayıtlı değil.";
    return `SSDownload kalıcı yerel bağlantısı kesildi: ${message}`;
  }

  // Relay progress reaches the session log through the same bounded relay module the
  // rest of the background uses; a host that did not load it stays silent.
  function relayStage(stage, state, options = {}) {
    try {
      const request = state?.request || {};
      let host = null;
      try { host = new URL(request.page_url || request.url || "").hostname; } catch (_) { host = null; }
      globalThis.SSDownloadEvents?.log?.("ext.relay.stage", {
        level: options.level || "info",
        outcome: options.outcome || (stage === "failed" ? "failed" : "ok"),
        code: options.code || null,
        host,
        job: state?.jobId,
        detail: options.reason ? `${stage} ${options.reason}` : stage
      });
    } catch (_) {}
  }

  function pauseAfterNativeDisconnect(failure) {
    const reason = `Yerel tarayıcı bağlantısı koptu; aktarım açıkça duraklatıldı: ${failure.message}`.slice(0, 500);
    for (const state of jobs.values()) {
      if (state.paused || state.stopping) continue;
      abortAndPause(state, reason).catch(error => {
        state.stopping = true;
        state.autoResume = false;
        state.reason = `Tarayıcı fetch durması doğrulanamadı; bağlantı izinleri güvenli olarak tutuluyor: ${String(error?.message || error)}`.slice(0, 500);
        saveJobs().catch(() => {});
      });
    }
  }

  function resetPort(error) {
    const failure = new Error(nativeError(error));
    const previous = nativePort;
    nativePort = null;
    if (previous) { try { previous.disconnect(); } catch (_) {} }
    if (nativeCurrent) { nativeCurrent.reject(failure); nativeCurrent = null; }
    while (nativeQueue.length) nativeQueue.shift().reject(failure);
    nativeQueueBytes = 0;
    pauseAfterNativeDisconnect(failure);
  }

  function connectPort() {
    if (nativePort) return nativePort;
    try {
      const port = relayApi.runtime.connectNative(RELAY_HOST);
      nativePort = port;
      port.onMessage.addListener(response => {
        if (nativePort !== port) return;
        const current = nativeCurrent;
        nativeCurrent = null;
        if (!current) { resetPort(new Error("Beklenmeyen Native Messaging yanıtı")); return; }
        nativeQueueBytes -= current.bytes;
        if (!response || typeof response.ok !== "boolean") current.reject(new Error("Yerel uygulama geçersiz yanıt verdi"));
        else current.resolve(response);
        drainNative();
      });
      port.onDisconnect.addListener(() => {
        if (nativePort !== port) return;
        const error = relayApi.runtime.lastError;
        resetPort(new Error(error?.message || "Native Messaging host kapandı"));
      });
      return port;
    } catch (error) { throw new Error(nativeError(error)); }
  }

  function drainNative() {
    if (nativeCurrent || !nativeQueue.length) return;
    const next = nativeQueue.shift();
    try {
      nativeCurrent = next;
      connectPort().postMessage(next.message);
    } catch (error) {
      nativeCurrent = null;
      nativeQueueBytes -= next.bytes;
      next.reject(error);
      resetPort(error);
    }
  }

  function persistentNative(action) {
    return new Promise((resolve, reject) => {
      const message = { version: 2, action };
      const bytes = nativeEncoder.encode(JSON.stringify(message)).byteLength + 4;
      if (bytes > MAX_NATIVE_FRAME_BYTES) {
        reject(new Error("Yerel aktarım iletisi 1 MiB sınırını aşıyor"));
        return;
      }
      if (nativeQueue.length + (nativeCurrent ? 1 : 0) >= 64 || nativeQueueBytes + bytes > MAX_NATIVE_QUEUE_BYTES) {
        reject(new Error("Tarayıcı aktarım kuyruğu 8 MiB sınırına ulaştı"));
        return;
      }
      nativeQueueBytes += bytes;
      nativeQueue.push({ message, bytes, resolve, reject });
      drainNative();
    });
  }

  async function transferCommand(command) {
    const response = await persistentNative({ type: "browser_transfer", command });
    if (!response.ok) throw new Error(String(response.message || "Tarayıcı aktarımı reddedildi"));
    const transfer = response.result?.browser_transfer;
    if (!transfer || transfer.job_id !== command.id || transfer.max_chunk_bytes !== MAX_CHUNK_BYTES || !Array.isArray(transfer.streams)) {
      throw new Error("Masaüstü tarayıcı aktarım yanıtı uyumsuz");
    }
    return transfer;
  }

  function savedJob(value) {
    const requestIds = value?.requestIds;
    return value && uuid(value.jobId) && safeDocument(value.documentId) && Number.isInteger(value.tabId) && value.tabId >= 0
      && Number.isInteger(value.frameId) && value.frameId >= 0 && typeof value.token === "string" && value.token.length === 64
      && Number.isSafeInteger(value.generation) && value.generation > 0
      && (requestIds === undefined || Array.isArray(requestIds) && requestIds.length <= 64
        && requestIds.every(validRequestId) && new Set(requestIds).size === requestIds.length)
      && typeof value.runToken === "string" && value.runToken.length >= 32 && value.runToken.length <= 128;
  }

  const ready = (async () => {
    try {
      const saved = await call(relayApi.storage.session, "get", { browserRelayJobs: [] });
      let interrupted = false;
      for (const value of Array.isArray(saved.browserRelayJobs) ? saved.browserRelayJobs.slice(-8) : []) if (savedJob(value)) {
        value.requestIds ??= [];
        value.requestOps = Promise.resolve();
        if (!value.paused) {
          value.stopping = true;
          value.autoResume = false;
          value.reason = "Tarayıcı bağlantısı yeniden başlatıldı; etkin fetch işlemi açıkça durduruluyor.";
          interrupted = true;
        }
        jobs.set(value.jobId, value);
      }
      if (interrupted) {
        await saveJobs();
        queueMicrotask(() => reconcileInterruptedRelays().catch(() => {}));
      }
      scheduleAutomaticResume();
    } catch (_) {}
  })();

  async function saveJobs() {
    const values = Array.from(jobs.values()).slice(-8).map(({request, requestOps, ...value}) => ({...value, request}));
    if (JSON.stringify(values).length > MAX_SAVED_BYTES) throw new Error("Tarayıcı aktarım sürdürme kaydı çok büyük");
    await call(relayApi.storage.session, "set", { browserRelayJobs: values });
  }

  function transferJobs(capabilities) {
    return Array.isArray(capabilities?.result?.capabilities?.source_refresh_jobs)
      ? capabilities.result.capabilities.source_refresh_jobs
      : Array.isArray(capabilities?.capabilities?.source_refresh_jobs) ? capabilities.capabilities.source_refresh_jobs : [];
  }

  function samePage(expected, actual) {
    try { const a = new URL(expected), b = new URL(actual); return a.origin === b.origin && a.pathname === b.pathname && a.search === b.search; }
    catch (_) { return false; }
  }

  async function sendFrame(state, message) {
    const options = { frameId: state.frameId };
    if (state.documentId) options.documentId = state.documentId;
    return call(relayApi.tabs, "sendMessage", state.tabId, message, options);
  }

  function policyPaused(reason) {
    const text = String(reason || "");
    // Current desktops tag the stop with SSD-TRF-013 (code first, localized
    // sentence after); older builds sent only the Turkish sentence. Both count.
    return globalThis.SSDownloadErrorCodes?.splitCode?.(text)?.code === "SSD-TRF-013"
      || /kuyruk kotası, zaman veya eşzamanlılık sınırında durduruldu/i.test(text);
  }

  function scheduleAutomaticResume() {
    if (![...jobs.values()].some(state => state.paused && state.autoResume)) return;
    try { relayApi.alarms?.create(AUTO_RESUME_ALARM, { delayInMinutes: AUTO_RESUME_DELAY_MINUTES }); } catch (_) {}
    if (resumeTimer === null && typeof setTimeout === "function") {
      resumeTimer = setTimeout(() => {
        resumeTimer = null;
        retryAutomaticTransfers().catch(() => {});
      }, 1000);
    }
  }

  async function resumeAutomatically(state) {
    let frame;
    try {
      const tab = await call(relayApi.tabs, "get", state.tabId);
      const frames = await call(relayApi.webNavigation, "getAllFrames", { tabId: state.tabId });
      frame = frames?.find(value => value.frameId === state.frameId);
      if (!tab || !frame || (frame.documentId && frame.documentId !== state.documentId) || !/^https?:/.test(frame.url || "")) {
        throw new Error("Kaynak frame veya belge artık aynı değil");
      }
    } catch (error) {
      state.autoResume = false;
      state.reason = String(error?.message || error).slice(0, 500);
      return;
    }

    let transfer;
    try {
      transfer = await transferCommand({ operation: "begin", id: state.jobId, total: null, etag: null, restart: false });
    } catch (error) {
      const reason = String(error?.message || error);
      if (/İş bulunamadı|ayrılamaz|yetkilendirilmeli|Deneysel tarayıcı aktarımı/i.test(reason)) {
        jobs.delete(state.jobId);
      } else {
        state.reason = reason.slice(0, 500);
      }
      return;
    }
    if (jobs.get(state.jobId) !== state) {
      await transferCommand({ operation: "pause", id: state.jobId, token: transfer.token }).catch(() => {});
      return;
    }
    const identity = transfer.request?.source_identity;
    const matches = typeof transfer.token === "string" && transfer.token.length === 64
      && Number.isSafeInteger(transfer.sender_generation) && transfer.sender_generation > 0
      && transfer.request && typeof transfer.request.url === "string"
      && (identity
        ? Number(identity.frame_id) === state.frameId && (!identity.document_id || identity.document_id === state.documentId)
          && (!identity.page_url || samePage(identity.page_url, frame.url))
        : !transfer.request.page_url || samePage(transfer.request.page_url, frame.url));
    if (!matches) {
      await transferCommand({ operation: "pause", id: state.jobId, token: transfer.token }).catch(() => {});
      state.autoResume = false;
      state.reason = "Yeniden başlayan iş kaynak frame/belge ile eşleşmiyor";
      return;
    }
    state.token = transfer.token;
    state.generation = transfer.sender_generation;
    state.requestIds = [];
    state.requestOps = Promise.resolve();
    state.runToken = crypto.randomUUID();
    state.request = transfer.request;
    state.paused = false;
    state.autoResume = false;
    state.reason = "";
    state.startedAt = Date.now();
    await saveJobs();
    try {
      const accepted = await sendFrame(state, { channel: INTERNAL_CHANNEL, command: "start", jobId: state.jobId,
        token: state.token, runToken: state.runToken, documentId: state.documentId, request: state.request, streams: transfer.streams });
      if (!accepted?.ok) throw new Error(String(accepted?.error || "Kaynak belge aktarımı kabul etmedi"));
      relayStage("resume", state);
    } catch (error) {
      const reason = `Kaynak belgeye ulaşılamadı: ${String(error?.message || error)}`;
      try {
        await pauseState(state, reason);
      } catch (stopError) {
        state.stopping = true;
        state.autoResume = false;
        state.reason = `Tarayıcı fetch durması doğrulanamadı; bağlantı izinleri güvenli olarak tutuluyor: ${String(stopError?.message || stopError)}`.slice(0, 500);
        await saveJobs().catch(() => {});
      }
    }
  }

  async function retryAutomaticTransfers() {
    if (resumeTask) return resumeTask;
    resumeTask = (async () => {
      await ready;
      for (const state of [...jobs.values()]) if (state.paused && state.autoResume) await resumeAutomatically(state);
      await saveJobs().catch(() => {});
      scheduleAutomaticResume();
    })().finally(() => { resumeTask = null; });
    return resumeTask;
  }

  async function abortContent(state, reason) {
    state.stopping = true;
    const settled = await sendFrame(state, {
      channel: INTERNAL_CHANNEL, command: "abort", jobId: state.jobId, runToken: state.runToken, reason
    });
    if (!settled?.ok && !settled?.settled) throw new Error(String(settled?.error || "Kaynak fetch işlemi durmayı doğrulamadı"));
  }

  async function endTrackedRequests(state) {
    await drainRequestOperations(state);
    for (const requestId of [...(state.requestIds || [])]) {
      await transferCommand({ operation: "end_request", id: state.jobId, token: state.token,
        generation: state.generation, request_id: requestId });
      state.requestIds = state.requestIds.filter(value => value !== requestId);
      await saveJobs();
    }
  }

  async function completePause(state, reason, automatic = false) {
    await transferCommand({ operation: "pause", id: state.jobId, token: state.token });
    state.paused = true;
    state.stopping = false;
    state.autoResume = automatic;
    state.reason = String(reason || "Tarayıcı aktarımı duraklatıldı").slice(0, 500);
    await saveJobs().catch(() => {});
    relayStage("pause", state, { level: automatic ? "warn" : "info", outcome: automatic ? "retry" : "ok", reason: state.reason });
    if (automatic) scheduleAutomaticResume();
  }

  async function abortAndPause(state, reason) {
    if (!state || state.paused) return;
    await abortContent(state, reason);
    await endTrackedRequests(state);
    await completePause(state, reason);
  }

  async function pauseState(state, reason) {
    await abortAndPause(state, reason);
  }

  async function reconcileInterruptedRelays() {
    for (const state of jobs.values()) {
      if (state.paused || !state.stopping) continue;
      try {
        await abortAndPause(state, state.reason || "Tarayıcı bağlantısı yeniden başlatıldı");
      } catch (error) {
        state.stopping = true;
        state.reason = `Tarayıcı fetch durması doğrulanamadı; bağlantı izinleri güvenli olarak tutuluyor: ${String(error?.message || error)}`.slice(0, 500);
        await saveJobs().catch(() => {});
      }
    }
  }

  async function startRelay(message) {
    await ready;
    const tabId = Number(message.tabId), frameId = Number(message.frameId);
    if (!uuid(message.jobId) || !Number.isInteger(tabId) || tabId < 0 || !Number.isInteger(frameId) || frameId < 0 || !safeDocument(message.documentId)) {
      throw new Error("Tarayıcı aktarım hedefi geçersiz");
    }
    const tab = await call(relayApi.tabs, "get", tabId);
    if (!tab || typeof tab.url !== "string" || !/^https?:/.test(tab.url)) throw new Error("Kaynak sekmesi artık açık değil");

    const frames = await call(relayApi.webNavigation, "getAllFrames", {tabId});
    const frame = frames?.find(value => value.frameId === frameId);
    if (!frame || (frame.documentId && frame.documentId !== message.documentId) || !/^https?:/.test(frame.url || "")) throw new Error("Kaynak frame veya belge artık aynı değil");

    const previous = jobs.get(message.jobId);
    if (!previous && jobs.size >= 8) throw new Error("Aynı anda en fazla sekiz tarayıcı aktarımı tutulabilir");
    const sameBinding = previous && previous.tabId === tabId && previous.frameId === frameId && previous.documentId === message.documentId;
    const capabilities = await persistentNative({ type: "capabilities" });
    if (!capabilities.ok) throw new Error(String(capabilities.message || "Masaüstü özellikleri okunamadı"));
    const authorized = transferJobs(capabilities).some(job => job?.id === message.jobId);
    if (!authorized && !sameBinding) throw new Error("Bu iş masaüstündeki Kaynak adresini yenile eylemiyle yetkilendirilmedi");

    if (previous && !previous.paused) await pauseState(previous, "Yeni açık aktarım isteği önceki tarayıcı çalışmasını durdurdu");
    const transfer = await transferCommand({ operation: "begin", id: message.jobId, total: null, etag: null, restart: message.restart === true });
    if (typeof transfer.token !== "string" || transfer.token.length !== 64
        || !Number.isSafeInteger(transfer.sender_generation) || transfer.sender_generation <= 0
        || !transfer.request || typeof transfer.request.url !== "string") {
      throw new Error("Masaüstü aktarım yetkisi veya kaynak isteği eksik");
    }
    const identity = transfer.request.source_identity;
    const matches = identity
      ? Number(identity.frame_id) === frameId && (!identity.document_id || identity.document_id === message.documentId)
        && (!identity.page_url || samePage(identity.page_url, frame.url))
      : !transfer.request.page_url || samePage(transfer.request.page_url, frame.url);
    if (!matches) {
      await transferCommand({operation:"pause",id:message.jobId,token:transfer.token}).catch(()=>{});
      throw new Error("Yetkili iş seçilen kaynak frame/belge ile eşleşmiyor");
    }
    const state = {
      jobId: message.jobId, tabId, frameId, documentId: message.documentId,
      token: transfer.token, generation: transfer.sender_generation, runToken: crypto.randomUUID(), request: transfer.request,
      requestIds: [], requestOps: Promise.resolve(), paused: false, autoResume: false, reason: "", startedAt: Date.now()
    };
    jobs.set(state.jobId, state);
    try {
      await saveJobs();
      const accepted = await sendFrame(state, { channel: INTERNAL_CHANNEL, command: "start", jobId: state.jobId,
        token: state.token, runToken: state.runToken, documentId: state.documentId, request: state.request, streams: transfer.streams });
      if (!accepted?.ok) throw new Error(String(accepted?.error || "Kaynak belge aktarımı kabul etmedi"));
      relayStage("start", state);
    } catch (error) {
      relayStage("failed", state, { level: "warn", outcome: "failed", code: "SSD-EXT-007", reason: String(error?.message || error) });
      await pauseState(state, `Kaynak belgeye ulaşılamadı: ${error.message}`);
      throw error;
    }
    return { ok: true, jobId: state.jobId, state: "running", sourceRequired: true, popupRequired: false };
  }

  function validRequestId(value) {
    return typeof value === "string" && /^relay-[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value);
  }

  function queueRequestOperation(state, operation) {
    const task = (state.requestOps || Promise.resolve()).catch(() => {}).then(operation);
    state.requestOps = task;
    return task;
  }

  async function drainRequestOperations(state) {
    await (state.requestOps || Promise.resolve()).catch(() => {});
  }

  function assertContent(message, sender, allowStopping = false) {
    const state = jobs.get(message.jobId);
    if (!state || state.paused || (!allowStopping && state.stopping) || message.runToken !== state.runToken || message.token !== state.token || message.documentId !== state.documentId
        || sender.tab?.id !== state.tabId || Number(sender.frameId || 0) !== state.frameId
        || (sender.documentId && sender.documentId !== state.documentId)) throw new Error("Tarayıcı aktarım göndereni veya yetkisi geçersiz");
    return state;
  }

  async function contentControl(message, sender) {
    await ready;
    const state = assertContent(message, sender, message.command === "end_request");
    switch (message.command) {
      case "open": {
        if (!Number.isInteger(message.streamId) || message.streamId <= 0 || message.streamId > 63) throw new Error("Akış kimliği geçersiz");
        return transferCommand({ operation: "open_stream", id: state.jobId, token: state.token, stream_id: message.streamId,
          role: message.role, format_id: message.formatId ?? null, language: message.language ?? null,
          total: Number.isSafeInteger(message.total) && message.total >= 0 ? message.total : null,
          etag: typeof message.etag === "string" ? message.etag : null });
      }
      case "chunk": {
        const checkpointText = JSON.stringify(message.clientCheckpoint ?? null);
        if (!Number.isInteger(message.streamId) || message.streamId <= 0 || !Number.isSafeInteger(message.sequence) || message.sequence < 0
            || !Number.isSafeInteger(message.offset) || message.offset < 0 || typeof message.data !== "string"
            || message.data.length > Math.ceil(MAX_CHUNK_BYTES / 3) * 4 || !/^[a-f0-9]{64}$/.test(message.sha256)
            || checkpointText.length > MAX_CHECKPOINT_BYTES) throw new Error("Tarayıcı aktarım parçası geçersiz");
        return transferCommand({ operation: "chunk", id: state.jobId, token: state.token, stream_id: message.streamId,
          sequence: message.sequence, offset: message.offset, data: message.data, sha256: message.sha256,
          client_checkpoint: message.clientCheckpoint ?? null });
      }
      case "seal":
        if (!Number.isInteger(message.streamId) || message.streamId <= 0 || !/^[a-f0-9]{64}$/.test(message.sha256)) throw new Error("Akış mühürleme bilgisi geçersiz");
        return transferCommand({ operation: "seal_stream", id: state.jobId, token: state.token, stream_id: message.streamId, sha256: message.sha256 });
      case "status": return transferCommand({ operation: "status", id: state.jobId, token: state.token });
      case "begin_request": {
        if (!validRequestId(message.requestId)) throw new Error("Tarayıcı bağlantı kimliği geçersiz");
        return queueRequestOperation(state, async () => {
          if (state.stopping || state.paused || jobs.get(state.jobId) !== state) throw new Error("Tarayıcı aktarımı durduruluyor");
          if (!(state.requestIds || []).includes(message.requestId)) {
            if ((state.requestIds || []).length >= 64) throw new Error("Tarayıcı bağlantı izni sayısı sınırı aşıldı");
            // Persist intent before native dispatch: a lost Begin response must still be
            // reconciled by this exact ID rather than risking an untracked grant.
            state.requestIds = [...(state.requestIds || []), message.requestId];
            await saveJobs();
            if (state.stopping || state.paused || jobs.get(state.jobId) !== state) throw new Error("Tarayıcı aktarımı durduruluyor");
          }
          const result = await transferCommand({ operation: "begin_request", id: state.jobId, token: state.token,
            generation: state.generation, request_id: message.requestId });
          if (result.request_granted === false) {
            state.requestIds = state.requestIds.filter(value => value !== message.requestId);
            await saveJobs();
          }
          return result;
        });
      }
      case "end_request": {
        if (!validRequestId(message.requestId)) throw new Error("Tarayıcı bağlantı kimliği geçersiz");
        return queueRequestOperation(state, async () => {
          const result = await transferCommand({ operation: "end_request", id: state.jobId, token: state.token,
            generation: state.generation, request_id: message.requestId });
          if ((state.requestIds || []).includes(message.requestId)) {
            state.requestIds = state.requestIds.filter(value => value !== message.requestId);
            await saveJobs();
          }
          return result;
        });
      }
      case "finish": {
        state.stopping = true;
        await endTrackedRequests(state);
        const result = await transferCommand({ operation: "finish", id: state.jobId, token: state.token, source_duration: message.sourceDuration ?? null });
        jobs.delete(state.jobId); await saveJobs(); relayStage("complete", state); return result;
      }
      case "failed": {
        // The content worker sends this only after its fetch/reader finally blocks have
        // cancelled or consumed every body and ended their exact permits.
        const reason = String(message.error || "Kaynak sayfa aktarımı tamamlayamadı");
        state.stopping = true;
        await endTrackedRequests(state);
        await completePause(state, reason, policyPaused(reason));
        return { job_id: state.jobId, paused: true, automatic_resume: state.autoResume === true };
      }
      default: throw new Error("Bilinmeyen tarayıcı aktarım iç komutu");
    }
  }

  async function uiControl(message, sender) {
    await ready;
    if (sender.id !== relayApi.runtime.id || typeof sender.url !== "string" || !sender.url.startsWith(relayApi.runtime.getURL(""))) {
      throw new Error("Tarayıcı aktarımı yalnız eklenti arayüzünden başlatılabilir");
    }
    if (message.command === "start") return startRelay(message);
    if (message.command === "status") {
      const state = jobs.get(message.jobId);
      return state ? {ok:true,jobId:state.jobId,state:state.paused?"paused":"running",reason:state.reason,automaticResume:state.autoResume===true,popupRequired:false,sourceRequired:true} : {ok:true,state:"inactive"};
    }
    if (message.command === "pause" || message.command === "cancel") {
      const state = jobs.get(message.jobId);
      if (!state) return {ok:true,state:"inactive"};
      if (state.paused && state.autoResume) {
        state.autoResume = false;
        state.reason = message.command === "cancel" ? "Kullanıcı iptal etti" : "Kullanıcı tarayıcı aktarımını duraklattı";
        if (message.command === "cancel") jobs.delete(state.jobId);
        await saveJobs();
        if (message.command === "cancel") relayStage("cancel", state, { reason: state.reason });
        return {ok:true,state:message.command === "cancel" ? "cancelled" : "paused"};
      }
      if (message.command === "cancel") {
        await abortContent(state, "Kullanıcı iptal etti");
        await endTrackedRequests(state);
        await transferCommand({operation:"cancel",id:state.jobId,token:state.token});
        jobs.delete(state.jobId); await saveJobs();
        relayStage("cancel", state, { reason: "Kullanıcı iptal etti" });
        return {ok:true,state:"cancelled"};
      }
      await pauseState(state,"Kullanıcı tarayıcı aktarımını duraklattı");
      return {ok:true,state:"paused"};
    }
    throw new Error("Bilinmeyen tarayıcı aktarım komutu");
  }

  relayApi.runtime.onMessage.addListener((message, sender, sendResponse) => {
    if (!message || (message.channel !== CONTROL_CHANNEL && message.channel !== INTERNAL_CHANNEL)) return false;
    const task = message.channel === CONTROL_CHANNEL ? uiControl(message, sender) : contentControl(message, sender);
    Promise.resolve(task).then(sendResponse, error => sendResponse({ error: String(error?.message || error) }));
    return true;
  });

  relayApi.alarms?.onAlarm?.addListener(alarm => {
    if (alarm?.name === AUTO_RESUME_ALARM) retryAutomaticTransfers().catch(() => {});
  });

  async function terminateClosedTab(state, reason) {
    // Chrome destroys every frame and its fetch contexts before tabs.onRemoved fires.
    // Unlike native/service-worker death, this is browser-confirmed request termination.
    state.stopping = true;
    await endTrackedRequests(state);
    await completePause(state, reason);
  }

  function invalidateTabBindings(tabId, reason, closed = false) {
    let removed = false;
    for (const state of [...jobs.values()]) {
      if (state.tabId !== tabId) continue;
      if (state.paused) {
        jobs.delete(state.jobId);
        removed = true;
      } else if (closed) {
        terminateClosedTab(state, reason).catch(error => {
          state.stopping = true;
          state.autoResume = false;
          state.reason = `Tarayıcı sekmesi kapandı ancak bağlantı izinleri sonlandırılamadı: ${String(error?.message || error)}`.slice(0, 500);
          saveJobs().catch(() => {});
        });
      } else {
        pauseState(state, reason).catch(async error => {
          // The navigation pause failed; record it and settle the record instead of
          // leaving a permanently "running" run the eight-job cap can never reclaim.
          relayStage("failed", state, { level: "warn", outcome: "failed", code: "SSD-EXT-007", reason: String(error?.message || error) });
          state.stopping = true;
          state.autoResume = false;
          state.reason = `Tarayıcı fetch durması doğrulanamadı; bağlantı izinleri güvenli olarak tutuluyor: ${String(error?.message || error)}`.slice(0, 500);
          // While the recorded document is still the one this tab shows, the record
          // stays pausable and cancelable: a retry reaches the same frame. Once the
          // document has moved on no frame round-trip can ever settle it, so the
          // desktop permit is revoked without the page and the record is released.
          let documentGone = false;
          try {
            if (relayApi.webNavigation?.getAllFrames) {
              const list = await call(relayApi.webNavigation, "getAllFrames", { tabId: state.tabId });
              documentGone = Array.isArray(list) && !list.some(frame => frame.frameId === state.frameId
                && (!frame.documentId || frame.documentId === state.documentId));
            }
          } catch (_) { /* An unavailable frame query proves nothing about live fetches. */ }
          if (!documentGone) { await saveJobs().catch(() => {}); return; }
          try {
            await endTrackedRequests(state);
            await completePause(state, reason);
            if (jobs.get(state.jobId) === state) jobs.delete(state.jobId);
            await saveJobs();
          } catch (stopError) {
            state.reason = `Belge kapandı ancak masaüstü izinleri sonlandırılamadı: ${String(stopError?.message || stopError)}`.slice(0, 500);
            await saveJobs().catch(() => {});
          }
        });
      }
    }
    if (removed) saveJobs().catch(() => {});
  }

  relayApi.tabs.onRemoved.addListener(tabId => {
    invalidateTabBindings(tabId, "Kaynak sekmesi kapandı; aktarım açıkça duraklatıldı", true);
  });
  relayApi.tabs.onUpdated.addListener((tabId, change) => {
    if (!change.url && change.status !== "loading") return;
    invalidateTabBindings(tabId, "Kaynak sekmesi başka bir belgeye geçti; aktarım açıkça duraklatıldı");
  });

  // Trusted worker-internal surface. The discovery background runs in the same
  // service-worker global scope and is the only intended caller; content scripts
  // keep using INTERNAL_CHANNEL and cannot reach an arbitrary job command here.
  // Requests still pass `uiControl`'s extension-UI guard so the existing checks
  // are not bypassed by a second entry point.
  const relaySender = () => ({ id: relayApi.runtime.id, url: relayApi.runtime.getURL("service-worker.js") });
  globalThis.SSDownloadRelayControl = {
    start: message => uiControl({ ...message, channel: CONTROL_CHANNEL, command: "start" }, relaySender()),
    status: jobId => uiControl({ channel: CONTROL_CHANNEL, command: "status", jobId }, relaySender()),
    pause: jobId => uiControl({ channel: CONTROL_CHANNEL, command: "pause", jobId }, relaySender()),
    cancel: jobId => uiControl({ channel: CONTROL_CHANNEL, command: "cancel", jobId }, relaySender())
  };
})();
