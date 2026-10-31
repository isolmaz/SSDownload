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
const audioLine = (attributes, name) => `#EXT-X-MEDIA:TYPE=AUDIO,${attributes}${name ? `,URI="${HOST}${name}.m3u8"` : ""}`;
// Serves the playlists and segments of one site by address; any other address fails the run.
const site = files => url => {
  if (!(url in files)) throw new Error(`unexpected request for ${url}`);
  return served({ type: url.endsWith(".m3u8") ? "application/vnd.apple.mpegurl" : "application/octet-stream", body: files[url], url });
};
const segmentsOf = (...names) => Object.fromEntries(names.flatMap(name => [[`${HOST}${name}.m3u8`, hlsPlaylist(`${HOST}${name}-0.bin`)], [`${HOST}${name}-0.bin`, name]]));
const opened = h => h.commands.filter(command => command.command === "open").map(command => [command.role, command.formatId]);
const requested = h => h.fetched.map(item => item.url.slice(HOST.length));
async function hlsRun(request, files) {
  const h = relayContentHarness({ request: { url: MASTER, ...request }, answer: site(files) });
  await h.run();
  return h;
}

// Two audio groups with the same rendition name, the lower variant listed first, each variant tied to its own group.
const withSeparateAudio = {
  [MASTER]: hlsMaster(
    audioLine('GROUP-ID="lo",NAME="English",LANGUAGE="en",DEFAULT=YES', "audio-lo"),
    audioLine('GROUP-ID="hi",NAME="English",LANGUAGE="en",DEFAULT=YES', "audio-hi"),
    `#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",URI="${HOST}subs-en.m3u8"`,
    ...variantLine('BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2",AUDIO="lo"', "v360"),
    ...variantLine('BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS="avc1.4d401f,mp4a.40.2",AUDIO="hi",SUBTITLES="subs"', "v720")),
  ...segmentsOf("v360", "v720", "audio-lo", "audio-hi", "subs-en")
};
// Stereo AAC on the best video tier, six-channel E-AC-3 on a lower one.
const withChannels = {
  [MASTER]: hlsMaster(
    audioLine('GROUP-ID="aac",NAME="English",LANGUAGE="en",CHANNELS="2",DEFAULT=YES', "audio-aac"),
    audioLine('GROUP-ID="ec3",NAME="English (surround)",LANGUAGE="en",CHANNELS="6",DEFAULT=YES', "audio-ec3"),
    ...variantLine('BANDWIDTH=8000000,RESOLUTION=1920x1080,CODECS="avc1.640028,mp4a.40.2",AUDIO="aac"', "v1080"),
    ...variantLine('BANDWIDTH=3000000,RESOLUTION=1280x720,CODECS="avc1.4d401f,ec-3",AUDIO="ec3"', "v720")),
  ...segmentsOf("v1080", "v720", "audio-aac", "audio-ec3")
};
// The main audio is in the variants themselves (a rendition without URI); the commentary is a track of its own.
const withMainInVariants = {
  [MASTER]: hlsMaster(
    audioLine('GROUP-ID="aud",NAME="Main",LANGUAGE="en",DEFAULT=YES'),
    audioLine('GROUP-ID="aud",NAME="Commentary",LANGUAGE="en"', "audio-commentary"),
    ...variantLine('BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2",AUDIO="aud"', "v360"),
    ...variantLine('BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS="avc1.4d401f,mp4a.40.2",AUDIO="aud"', "v720")),
  ...segmentsOf("v360", "v720", "audio-commentary")
};
// The audio of every variant is part of its video stream.
const withCombinedAudio = {
  [MASTER]: hlsMaster(
    ...variantLine('BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2"', "v360"),
    ...variantLine('BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS="avc1.4d401f,mp4a.40.2"', "v720")),
  ...segmentsOf("v360", "v720")
};
// Nothing says what the variants carry.
const withoutCodecs = {
  [MASTER]: hlsMaster(...variantLine("BANDWIDTH=800000", "v360"), ...variantLine("BANDWIDTH=2400000", "v720")),
  ...segmentsOf("v360", "v720")
};
// Besides its combined variant, the playlist lists two that carry audio only, one whose codec is not known here and one
// that names no codecs.
const withAudioVariants = {
  [MASTER]: hlsMaster(
    ...variantLine('BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS="avc1.4d401f,mp4a.40.2"', "v720"),
    ...variantLine('BANDWIDTH=3000000,CODECS="dav1.10.01,mp4a.40.2"', "v4k"),
    ...variantLine("BANDWIDTH=3500000", "vbare"),
    ...variantLine('BANDWIDTH=64000,CODECS="mp4a.40.5"', "a64"),
    ...variantLine('BANDWIDTH=128000,CODECS="mp4a.40.2"', "a128")),
  ...segmentsOf("v720", "v4k", "vbare", "a64", "a128")
};

test("an audio-only HLS request transfers the audio renditions of the best variant and no video", async () => {
  for (const limits of [{}, { max_height: 360 }]) {
    const h = await hlsRun({ kind: "audio", ...limits }, withSeparateAudio);
    const label = JSON.stringify(limits);
    assert.deepEqual(opened(h), [["audio", "hi-English"]], label);
    assert.deepEqual(requested(h), ["master.m3u8", "audio-hi.m3u8", "audio-hi-0.bin"], `${label}: no video playlist or segment is fetched`);
    assert.equal(h.names().at(-1), "finish", label);
  }
});

test("an audio-only HLS request keeps the subtitle rules of a video request", async () => {
  const h = await hlsRun({ kind: "audio", subtitle_languages: ["en"] }, withSeparateAudio);
  assert.deepEqual(opened(h), [["audio", "hi-English"], ["subtitle", null]]);
  assert.equal(h.names().at(-1), "finish");
});

test("an audio-only HLS request takes the rendition its id names, never one of the same name in another group", async () => {
  for (const [id, group] of [["hls-lo-English", "lo"], ["hls-hi-English", "hi"], ["lo-English", "lo"]]) {
    const h = await hlsRun({ kind: "audio", audio_format_id: id, audio_language: "en" }, withSeparateAudio);
    assert.deepEqual(opened(h), [["audio", id]], id);
    assert.deepEqual(requested(h), ["master.m3u8", `audio-${group}.m3u8`, `audio-${group}-0.bin`], id);
  }
  const h = await hlsRun({ kind: "audio", audio_format_id: "hls-mid-English", audio_language: "en" }, withSeparateAudio);
  assert.deepEqual(requested(h), ["master.m3u8"], "an id that names no rendition transfers nothing");
  assert.equal(wrote(h), false);
  assert.match(h.commands.at(-1).error, /audio rendition does not match exactly one/);
});

test("an audio-only HLS request ranks the audio groups by channels before the video tier of their variants", async () => {
  const best = await hlsRun({ kind: "audio" }, withChannels);
  assert.equal(opened(best).length, 1);
  assert.deepEqual(requested(best), ["master.m3u8", "audio-ec3.m3u8", "audio-ec3-0.bin"]);
  // The ids are the ones yt-dlp spells (the extractor's prefix is optional), whitespace and brackets turned into "_".
  for (const [id, group] of [["aac-English", "aac"], ["hls-aac-English", "aac"], ["ec3-English__surround_", "ec3"], ["hls-ec3-English__surround_", "ec3"]]) {
    const named = await hlsRun({ kind: "audio", audio_format_id: id }, withChannels);
    assert.deepEqual(opened(named), [["audio", id]], id);
    assert.deepEqual(requested(named), ["master.m3u8", `audio-${group}.m3u8`, `audio-${group}-0.bin`], id);
  }
});

test("an audio-only HLS request takes a rendition without URI from the variant that holds it and a named track from its own playlist", async () => {
  for (const request of [{ kind: "audio" }, { kind: "audio", expected_audio: true }, { kind: "audio", audio_format_id: "hls-aud-Main", audio_language: "en" }]) {
    const h = await hlsRun(request, withMainInVariants);
    assert.deepEqual(opened(h), [["audio", request.audio_format_id || "aud-Main"]], JSON.stringify(request));
    assert.deepEqual(requested(h), ["master.m3u8", "v360.m3u8", "v360-0.bin"], `${JSON.stringify(request)}: the cheapest variant that uses the group`);
    assert.equal(h.names().at(-1), "finish");
  }
  const commentary = await hlsRun({ kind: "audio", expected_audio: true, audio_format_id: "hls-aud-Commentary", audio_language: "en" }, withMainInVariants);
  assert.deepEqual(opened(commentary), [["audio", "hls-aud-Commentary"]]);
  assert.deepEqual(requested(commentary), ["master.m3u8", "audio-commentary.m3u8", "audio-commentary-0.bin"]);
});

test("an audio-only HLS request takes the audio out of the best combined variant, as an audio stream", async () => {
  for (const [label, files, best] of [["declared codecs", withCombinedAudio, "v720"], ["no codecs at all", withoutCodecs, "v720"]]) {
    const h = await hlsRun({ kind: "audio", expected_audio: true }, files);
    assert.deepEqual(opened(h), [["audio", `${HOST}${best}.m3u8`]], label);
    assert.deepEqual(requested(h), ["master.m3u8", `${best}.m3u8`, `${best}-0.bin`], label);
    assert.equal(h.names().at(-1), "finish", label);
  }
});

test("an audio-only HLS request prefers a variant that declares audio codecs only to any other, however high its bandwidth", async () => {
  const h = await hlsRun({ kind: "audio", expected_audio: true }, withAudioVariants);
  assert.deepEqual(opened(h), [["audio", `${HOST}a128.m3u8`]]);
  assert.deepEqual(requested(h), ["master.m3u8", "a128.m3u8", "a128-0.bin"], "no variant that may carry video is fetched");
});

test("an audio-only HLS request takes the audio-only variant its id names", async () => {
  for (const id of ["hls-64", "64"]) {
    const h = await hlsRun({ kind: "audio", audio_format_id: id, expected_audio: true }, withAudioVariants);
    assert.deepEqual(opened(h), [["audio", id]], id);
    assert.deepEqual(requested(h), ["master.m3u8", "a64.m3u8", "a64-0.bin"], id);
  }
});

test("a video HLS request still transfers the chosen variant with its audio rendition, or the combined variant alone", async () => {
  const separate = await hlsRun({ kind: "video" }, withSeparateAudio);
  assert.deepEqual(opened(separate), [["video", `${HOST}v720.m3u8`], ["audio", "hi-English"]]);
  assert.deepEqual(requested(separate), ["master.m3u8", "v720.m3u8", "audio-hi.m3u8", "v720-0.bin", "audio-hi-0.bin"]);
  const combined = await hlsRun({ kind: "video", max_height: 360 }, withCombinedAudio);
  assert.deepEqual(opened(combined), [["file", `${HOST}v360.m3u8`]]);
  assert.deepEqual(requested(combined), ["master.m3u8", "v360.m3u8", "v360-0.bin"]);
});
