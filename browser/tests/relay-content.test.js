"use strict";

// relay-content.js runs in the page's isolated world. The double answers the worker's relay
// commands and serves the one fetch a direct run makes, so a test sees exactly which commands
// reach the desktop side: a refused page must send none that opens, writes or seals a stream.
const assert = require("node:assert/strict");
const { webcrypto } = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const { i18nApi, loadI18n } = require("./i18n-harness");

const chromium = name => fs.readFileSync(path.join(__dirname, "..", "chromium", name), "utf8");
const PAGE = "<html><body>Sign in</body></html>";

function served({ status = 200, type, disposition, redirected = false, body = PAGE, headers = {}, url = "https://files.example/final" }) {
  let cancelled = false;
  const stream = new ReadableStream({
    start(controller) { controller.enqueue(new TextEncoder().encode(body)); controller.close(); },
    cancel() { cancelled = true; }
  });
  const response = new Response(stream, { status, headers: { "content-type": type, ...(disposition ? { "content-disposition": disposition } : {}), ...headers } });
  Object.defineProperty(response, "redirected", { value: redirected });
  Object.defineProperty(response, "url", { value: url });
  return { response, wasCancelled: () => cancelled };
}

// `resume` is the stream the desktop reports as already stored: a direct run continues from it.
function relayContentHarness({ request, answer, resume = null }) {
  const commands = [], fetched = [];
  let listener = null, answered = null;
  const reply = message => {
    switch (message.command) {
      case "begin_request": return { request_granted: true };
      case "open": return { streams: [resume || { stream_id: message.streamId, offset: 0, next_sequence: 0, sealed: false }] };
      case "chunk": {
        const length = Buffer.from(message.data, "base64").length;
        return { streams: [{ stream_id: message.streamId, offset: message.offset + length, next_sequence: message.sequence + 1 }] };
      }
      default: return { ok: true };
    }
  };
  const browser = {
    i18n: i18nApi("en"),
    runtime: { onMessage: { addListener(value) { listener = value; } }, async sendMessage(message) { commands.push(message); return reply(message); } }
  };
  const context = vm.createContext({ browser, URL, Headers, AbortController, setTimeout, clearTimeout, TextEncoder, TextDecoder, atob, btoa,
    crypto: webcrypto, fetch: async (url, options) => { fetched.push({ url, range: options.headers.get("range") }); answered = answer(url); return answered.response; } });
  loadI18n(context);
  vm.runInContext(chromium("relay-sha256.js"), context, { filename: "relay-sha256.js" });
  vm.runInContext(chromium("relay-content.js"), context, { filename: "relay-content.js" });
  return {
    commands, fetched,
    names: () => commands.map(command => command.command),
    cancelled: () => answered?.wasCancelled() === true,
    async run() {
      const streams = resume ? [resume] : [];
      listener({ channel: "ssdownload-relay-internal", command: "start", jobId: "job-1", token: "t".repeat(64), runToken: "run-1",
        documentId: "doc", request: { url: "https://files.example/get", ...request }, streams }, {}, () => {});
      for (let round = 0; round < 2000 && !commands.some(command => command.command === "finish" || command.command === "failed"); round += 1) {
        await new Promise(resolve => setImmediate(resolve));
      }
    }
  };
}
// The hash state of the first 100 bytes, as the desktop stored it with the part a run continues.
const hashContext = vm.createContext({ Uint8Array, Uint32Array, atob, btoa });
vm.runInContext(chromium("relay-sha256.js"), hashContext, { filename: "relay-sha256.js" });
const hash100 = JSON.parse(vm.runInContext("JSON.stringify(new SSDownloadRelaySha256().update(new Uint8Array(100)).exportState())", hashContext));
const WRITING = ["open", "chunk", "seal"];
const wrote = h => h.names().some(name => WRITING.includes(name));

test("a fresh run refuses a page that a redirect led a file job to and writes nothing", async () => {
  const h = relayContentHarness({ request: { kind: "file", filename: null }, answer: () => served({ type: "text/html; charset=utf-8", redirected: true }) });
  await h.run();
  assert.deepEqual(h.names(), ["begin_request", "end_request", "failed"], "the permit is returned and the run is reported failed");
  assert.match(h.commands.at(-1).error, /web page instead of the file/);
  assert.ok(h.cancelled(), "the page body is not read");
});

test("the job's own file name decides before the address does", async () => {
  const h = relayContentHarness({ request: { kind: "file", filename: "data.zip", url: "https://files.example/saved.html" },
    answer: () => served({ type: "text/html", redirected: true }) });
  await h.run();
  assert.deepEqual(h.names(), ["begin_request", "end_request", "failed"]);
});

for (const kind of ["video", "audio"]) test(`a fresh run refuses any web page for a job of kind ${kind}, redirected or not`, async () => {
  for (const [type, redirected] of [["text/html", false], ["application/xhtml+xml; charset=utf-8", true]]) {
    const h = relayContentHarness({ request: { kind }, answer: () => served({ type, redirected }) });
    await h.run();
    assert.deepEqual(h.names(), ["begin_request", "end_request", "failed"], `${type} redirected=${redirected}`);
    assert.ok(h.cancelled());
  }
});

test("a resumed run cannot append a page to the stored part", async () => {
  for (const [kind, redirected] of [["file", true], ["video", false]]) {
    const h = relayContentHarness({ request: { kind },
      resume: { stream_id: 1, offset: 100, next_sequence: 3, sealed: false, etag: "\"v1\"", client_checkpoint: { v: 1, kind: "direct", offset: 100 } },
      answer: () => served({ status: 206, type: "text/html", redirected, headers: { "content-range": "bytes 100-199/200", etag: "\"v1\"" } }) });
    await h.run();
    assert.equal(h.fetched[0].range, "bytes=100-", `${kind}: the run asked to continue the part`);
    assert.equal(wrote(h), false, `${kind}: nothing may be opened, written or sealed`);
    assert.deepEqual(h.names(), ["begin_request", "end_request", "failed"]);
    assert.match(h.commands.at(-1).error, /web page instead of the file/);
  }
});

test("a resumed run still continues the stored part with the file itself", async () => {
  const h = relayContentHarness({ request: { kind: "file" },
    resume: { stream_id: 1, offset: 100, next_sequence: 3, sealed: false, etag: "\"v1\"", client_checkpoint: { v: 1, kind: "direct", sha: hash100, offset: 100 } },
    answer: () => served({ status: 206, type: "application/octet-stream", body: "tail", headers: { "content-range": "bytes 100-103/104", etag: "\"v1\"" } }) });
  await h.run();
  assert.equal(h.fetched[0].range, "bytes=100-");
  assert.deepEqual(h.names(), ["begin_request", "open", "chunk", "end_request", "seal", "finish"]);
  assert.equal(h.commands.find(command => command.command === "chunk").offset, 100);
});

test("a page is still saved when the application would save it", async () => {
  const cases = [
    ["an attachment", { kind: "file" }, { disposition: "attachment; filename=\"report.html\"", redirected: true }],
    ["a file named as a page", { kind: "file", filename: "saved.html" }, { redirected: true }],
    ["a file the address names as a page", { kind: "file", url: "https://files.example/dir/saved.html?token=1" }, { redirected: true }],
    ["the page a file job points at directly", { kind: "file" }, { redirected: false }],
    ["an unspecified job kind", { kind: "auto" }, { redirected: false }],
  ];
  for (const [label, request, serve] of cases) {
    const h = relayContentHarness({ request, answer: () => served({ type: "text/html", ...serve }) });
    await h.run();
    assert.deepEqual(h.names(), ["begin_request", "open", "chunk", "end_request", "seal", "finish"], label);
    assert.equal(Buffer.from(h.commands.find(command => command.command === "chunk").data, "base64").toString(), PAGE, label);
  }
});

test("a redirected answer that is not a page is written as before", async () => {
  const h = relayContentHarness({ request: { kind: "file" }, answer: () => served({ type: "application/zip", redirected: true, body: "PK" }) });
  await h.run();
  assert.deepEqual(h.names(), ["begin_request", "open", "chunk", "end_request", "seal", "finish"]);
});

// An HLS master playlist: what an audio-only request takes from it, and what a video request keeps.
const HOST = "https://media.example/";
const MASTER = `${HOST}master.m3u8`;
const hlsPlaylist = (...segments) => ["#EXTM3U", "#EXT-X-TARGETDURATION:4", ...segments.flatMap(segment => ["#EXTINF:4.0,", segment]), "#EXT-X-ENDLIST"].join("\n");
const hlsMaster = (...lines) => ["#EXTM3U", ...lines].join("\n");
const variantLine = (attributes, name) => [`#EXT-X-STREAM-INF:${attributes}`, `${HOST}${name}.m3u8`];
// Serves the playlists and segments of one site by address; any other address fails the run.
const site = files => url => {
  if (!(url in files)) throw new Error(`unexpected request for ${url}`);
  return served({ type: url.endsWith(".m3u8") ? "application/vnd.apple.mpegurl" : "application/octet-stream", body: files[url], url });
};
const segmentsOf = (...names) => Object.fromEntries(names.flatMap(name => [[`${HOST}${name}.m3u8`, hlsPlaylist(`${HOST}${name}-0.bin`)], [`${HOST}${name}-0.bin`, name]]));
const opened = h => h.commands.filter(command => command.command === "open").map(command => [command.role, command.formatId]);
const requested = h => h.fetched.map(item => item.url.slice(HOST.length));

// Two audio groups, the lower variant listed first, each variant tied to its own group.
const withSeparateAudio = {
  [MASTER]: hlsMaster(
    `#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="lo",NAME="English",LANGUAGE="en",DEFAULT=YES,URI="${HOST}audio-lo.m3u8"`,
    `#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="hi",NAME="English",LANGUAGE="en",DEFAULT=YES,URI="${HOST}audio-hi.m3u8"`,
    `#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",URI="${HOST}subs-en.m3u8"`,
    ...variantLine('BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2",AUDIO="lo"', "v360"),
    ...variantLine('BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS="avc1.4d401f,mp4a.40.2",AUDIO="hi",SUBTITLES="subs"', "v720")),
  ...segmentsOf("v360", "v720", "audio-lo", "audio-hi", "subs-en")
};
// The audio of every variant is part of its video stream.
const withCombinedAudio = {
  [MASTER]: hlsMaster(
    ...variantLine('BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2"', "v360"),
    ...variantLine('BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS="avc1.4d401f,mp4a.40.2"', "v720")),
  ...segmentsOf("v360", "v720")
};
// Besides its combined variant, the playlist lists variants that carry no video.
const withAudioVariants = {
  [MASTER]: hlsMaster(
    ...variantLine('BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS="avc1.4d401f,mp4a.40.2"', "v720"),
    ...variantLine('BANDWIDTH=64000,CODECS="mp4a.40.5"', "a64"),
    ...variantLine('BANDWIDTH=128000,CODECS="mp4a.40.2"', "a128")),
  ...segmentsOf("v720", "a64", "a128")
};

test("an audio-only HLS request transfers the audio renditions of the best variant and no video", async () => {
  for (const limits of [{}, { max_height: 360 }]) {
    const h = relayContentHarness({ request: { url: MASTER, kind: "audio", ...limits }, answer: site(withSeparateAudio) });
    await h.run();
    const label = JSON.stringify(limits);
    assert.deepEqual(opened(h), [["audio", "hi-English"]], label);
    assert.deepEqual(requested(h), ["master.m3u8", "audio-hi.m3u8", "audio-hi-0.bin"], `${label}: no video playlist or segment is fetched`);
    assert.equal(h.names().at(-1), "finish", label);
  }
});

test("an audio-only HLS request keeps the subtitle rules of a video request", async () => {
  const h = relayContentHarness({ request: { url: MASTER, kind: "audio", subtitle_languages: ["en"] }, answer: site(withSeparateAudio) });
  await h.run();
  assert.deepEqual(opened(h), [["audio", "hi-English"], ["subtitle", null]]);
  assert.equal(h.names().at(-1), "finish");
});

test("an audio-only HLS request fails before any transfer when every variant combines audio with video", async () => {
  const h = relayContentHarness({ request: { url: MASTER, kind: "audio" }, answer: site(withCombinedAudio) });
  await h.run();
  assert.deepEqual(requested(h), ["master.m3u8"], "not even a video playlist is fetched");
  assert.equal(wrote(h), false);
  assert.deepEqual(h.names(), ["begin_request", "end_request", "failed"]);
  assert.match(h.commands.at(-1).error, /audio-only download needs a separate HLS audio stream/);
});

test("an audio-only HLS request takes the best variant that carries no video when no rendition holds the audio", async () => {
  const h = relayContentHarness({ request: { url: MASTER, kind: "audio" }, answer: site(withAudioVariants) });
  await h.run();
  assert.deepEqual(opened(h), [["file", `${HOST}a128.m3u8`]]);
  assert.deepEqual(requested(h), ["master.m3u8", "a128.m3u8", "a128-0.bin"]);
  assert.equal(h.names().at(-1), "finish");
});

test("a video HLS request still transfers the chosen variant with its audio rendition, or the combined variant alone", async () => {
  const separate = relayContentHarness({ request: { url: MASTER, kind: "video" }, answer: site(withSeparateAudio) });
  await separate.run();
  assert.deepEqual(opened(separate), [["video", `${HOST}v720.m3u8`], ["audio", "hi-English"]]);
  assert.deepEqual(requested(separate), ["master.m3u8", "v720.m3u8", "audio-hi.m3u8", "v720-0.bin", "audio-hi-0.bin"]);
  const combined = relayContentHarness({ request: { url: MASTER, kind: "video", max_height: 360 }, answer: site(withCombinedAudio) });
  await combined.run();
  assert.deepEqual(opened(combined), [["file", `${HOST}v360.m3u8`]]);
  assert.deepEqual(requested(combined), ["master.m3u8", "v360.m3u8", "v360-0.bin"]);
});
