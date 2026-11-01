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
// Variants without an audio group, with the ids yt-dlp gives them: "63" (AVERAGE-BANDWIDTH 63600, truncated), "64" twice
// (yt-dlp numbers those "64-0" and "64-1"), "127" (BANDWIDTH 127900, truncated), "Low__360p_" (NAME), "Angle_One" (the
// NAME of the first rendition of its VIDEO group), "nogroup" (a VIDEO group that is not in the playlist) and "2400".
const withNamedVariants = {
  [MASTER]: hlsMaster(
    ...variantLine('BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS="avc1.4d401f,mp4a.40.2"', "v720"),
    ...variantLine('BANDWIDTH=64000,CODECS="mp4a.40.2"', "alc"),
    ...variantLine('BANDWIDTH=64000,CODECS="mp4a.40.5"', "ahe"),
    ...variantLine('BANDWIDTH=127900,CODECS="mp4a.40.2"', "a127"),
    ...variantLine('BANDWIDTH=999999,AVERAGE-BANDWIDTH=63600,CODECS="mp4a.40.2"', "a63"),
    ...variantLine('BANDWIDTH=500000,NAME="Low (360p)",CODECS="avc1.4d401e,mp4a.40.2"', "low"),
    `#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID="vg",NAME="Angle One",URI="${HOST}vg1.m3u8"`,
    ...variantLine('BANDWIDTH=700000,VIDEO="vg",CODECS="avc1.4d401e,mp4a.40.2"', "angle"),
    ...variantLine('BANDWIDTH=800000,VIDEO="nogroup",CODECS="avc1.4d401e,mp4a.40.2"', "nogroup")),
  ...segmentsOf("v720", "alc", "ahe", "a127", "a63", "low", "angle", "nogroup")
};
// Formats that yt-dlp gives one id and numbers in an order of its own, not the order of the playlist. Two renditions of one
// group that spell alike ("x-a_b-0" is the alternate track, "x-a_b-1" the main one); ...
const withTwinRenditions = {
  [MASTER]: hlsMaster(
    audioLine('GROUP-ID="x",NAME="a_b",LANGUAGE="en",DEFAULT=YES', "x_ab"),
    audioLine('GROUP-ID="x",NAME="a b",LANGUAGE="en",DEFAULT=NO', "x_a_sp_b"),
    ...variantLine('BANDWIDTH=4000000,RESOLUTION=1920x1080,CODECS="avc1.640028,mp4a.40.2",AUDIO="x"', "v1080")),
  ...segmentsOf("v1080", "x_ab", "x_a_sp_b")
};
// ... audio-only variants of one bandwidth ("64-0" is the E-AC-3 one, which sorts below AAC); ...
const withTwinCodecs = {
  [MASTER]: hlsMaster(
    ...variantLine('BANDWIDTH=64000,CODECS="mp4a.40.2"', "alc"),
    ...variantLine('BANDWIDTH=64000,CODECS="ec-3"', "aec3")),
  ...segmentsOf("alc", "aec3")
};
// ... and audio-only variants whose bandwidths truncate to the same kbit/s ("63-0" is the one of 63.6 kbit/s).
const withTwinBandwidths = {
  [MASTER]: hlsMaster(
    ...variantLine('BANDWIDTH=63999,CODECS="mp4a.40.5"', "a63b"),
    ...variantLine('BANDWIDTH=127900,AVERAGE-BANDWIDTH=63600,CODECS="mp4a.40.2"', "a63")),
  ...segmentsOf("a63b", "a63")
};
// Renditions whose names carry every character that yt-dlp turns into "_" in an id.
const withOddNames = {
  [MASTER]: hlsMaster(
    audioLine('GROUP-ID="aud",NAME="Audio [HQ], 2.0 (st)",LANGUAGE="en",DEFAULT=YES', "audio-hq"),
    audioLine('GROUP-ID="aud",NAME="Français/FR+",LANGUAGE="fr"', "audio-fr"),
    ...variantLine('BANDWIDTH=3000000,RESOLUTION=1280x720,CODECS="avc1.4d401f,mp4a.40.2",AUDIO="aud"', "v720")),
  ...segmentsOf("v720", "audio-hq", "audio-fr")
};
// Groups whose ids end alike: "hls-a-hi-English" ends with the id of both.
const withHyphenGroups = {
  [MASTER]: hlsMaster(
    audioLine('GROUP-ID="a-hi",NAME="English",LANGUAGE="en",CHANNELS="2",DEFAULT=YES', "audio-a-hi"),
    audioLine('GROUP-ID="hi",NAME="English",LANGUAGE="en",CHANNELS="6",DEFAULT=YES', "audio-hi"),
    ...variantLine('BANDWIDTH=3000000,RESOLUTION=1280x720,CODECS="avc1.4d401f,mp4a.40.2",AUDIO="a-hi"', "va"),
    ...variantLine('BANDWIDTH=2000000,RESOLUTION=1280x720,CODECS="avc1.4d401f,ec-3",AUDIO="hi"', "vh")),
  ...segmentsOf("va", "vh", "audio-a-hi", "audio-hi")
};
// The main audio is in the variants themselves, and a variant carries audio only.
const withMainAndAudioVariant = {
  [MASTER]: hlsMaster(
    audioLine('GROUP-ID="aud",NAME="Main",LANGUAGE="en",DEFAULT=YES'),
    ...variantLine('BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2",AUDIO="aud"', "v360"),
    ...variantLine('BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS="avc1.4d401f,mp4a.40.2",AUDIO="aud"', "v720"),
    ...variantLine('BANDWIDTH=128000,CODECS="mp4a.40.2"', "a128")),
  ...segmentsOf("v360", "v720", "a128")
};
// A variant that carries video only, on a higher tier than the combined one.
const withVideoOnlyVariant = {
  [MASTER]: hlsMaster(
    ...variantLine('BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS="avc1.4d401f"', "v720"),
    ...variantLine('BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2"', "v360")),
  ...segmentsOf("v720", "v360")
};
// A refused selection transfers and writes nothing.
async function assertRefused(request, files) {
  const h = await hlsRun({ kind: "audio", ...request }, files);
  const label = JSON.stringify(request);
  assert.deepEqual(requested(h), ["master.m3u8"], `${label}: nothing but the master is fetched`);
  assert.equal(wrote(h), false, label);
  assert.match(h.commands.at(-1).error, /audio rendition does not match exactly one/, label);
}

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
  await assertRefused({ audio_format_id: "hls-hi-English", audio_language: "de" }, withSeparateAudio);
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

test("an audio-only HLS request takes a rendition without URI from the best variant that holds it and a named track from its own playlist", async () => {
  for (const request of [{ kind: "audio" }, { kind: "audio", expected_audio: true }, { kind: "audio", audio_format_id: "hls-aud-Main", audio_language: "en" }]) {
    const h = await hlsRun(request, withMainInVariants);
    assert.deepEqual(opened(h), [["audio", request.audio_format_id || "aud-Main"]], JSON.stringify(request));
    assert.deepEqual(requested(h), ["master.m3u8", "v720.m3u8", "v720-0.bin"], `${JSON.stringify(request)}: the best variant that uses the group`);
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

test("an audio-only HLS request knows the audio codecs of the other families of codec strings", async () => {
  for (const codec of ["mp4a.40.34", "ac-4.02.01.01", "ec+3", "mha1.0x0D", "mha2.0x0D", "mhm1.0x0D", "mhm2.0x0D", "dtsx", "dtse", "ipcm", "fpcm", "Opus", "fLaC"]) {
    const files = { [MASTER]: hlsMaster(...variantLine('BANDWIDTH=2400000,CODECS="avc1.4d401f,mp4a.40.2"', "v720"), ...variantLine(`BANDWIDTH=64000,CODECS="${codec}"`, "alone")), ...segmentsOf("v720", "alone") };
    const h = await hlsRun({ kind: "audio", expected_audio: true }, files);
    assert.deepEqual(requested(h), ["master.m3u8", "alone.m3u8", "alone-0.bin"], codec);
  }
});

test("an audio-only HLS request takes the audio-only variant its id names", async () => {
  for (const id of ["hls-64", "64"]) {
    const h = await hlsRun({ kind: "audio", audio_format_id: id, expected_audio: true }, withAudioVariants);
    assert.deepEqual(opened(h), [["audio", id]], id);
    assert.deepEqual(requested(h), ["master.m3u8", "a64.m3u8", "a64-0.bin"], id);
  }
});

test("an audio-only HLS request takes a variant without an audio group by the id yt-dlp gives it, and by no other", async () => {
  const ids = [["hls-63", "a63"], ["127", "a127"], ["hls-Low__360p_", "low"], ["Angle_One", "angle"], ["hls-nogroup", "nogroup"], ["2400", "v720"]];
  for (const [id, name] of ids) {
    const h = await hlsRun({ kind: "audio", audio_format_id: id }, withNamedVariants);
    assert.deepEqual(opened(h), [["audio", id]], id);
    assert.deepEqual(requested(h), ["master.m3u8", `${name}.m3u8`, `${name}-0.bin`], id);
  }
  // Two variants share "64", which yt-dlp numbers "64-0" and "64-1" in an order of its own, so none of the three names a
  // track; 0 and 1 are positions; 128 is 127.9 rounded; 999 is a BANDWIDTH that an AVERAGE-BANDWIDTH overrides; a name or
  // an address is no id.
  for (const id of ["64", "hls-64", "64-0", "hls-64-1", "0", "1", "hls-128", "hls-999", "hls-64000", "hls-Low (360p)", "hls-Low", "hls-vg", "hls-vg-Angle_One", "alc.m3u8", `${HOST}alc.m3u8`]) {
    await assertRefused({ audio_format_id: id }, withNamedVariants);
  }
});

test("an audio-only HLS request refuses an id that two formats have, since yt-dlp numbers them in an order of its own", async () => {
  const cases = [
    [withTwinRenditions, "x-a_b", { audio_language: "en" }],
    [withTwinCodecs, "64", {}],
    [withTwinBandwidths, "63", {}]
  ];
  for (const [files, shared, extra] of cases) {
    const request = { kind: "audio", expected_audio: true };
    const control = await hlsRun(request, files);
    assert.equal(control.names().at(-1), "finish", `${shared}: without an id the playlist is served`);
    for (const id of [shared, `${shared}-0`, `${shared}-1`]) {
      for (const spelled of [id, `hls-${id}`]) await assertRefused({ audio_format_id: spelled, ...extra }, files);
    }
  }
});

test("an audio-only HLS request spells every character that yt-dlp turns into an underscore", async () => {
  for (const [id, name, language] of [["aud-Audio__HQ___2.0__st_", "audio-hq", "en"], ["hls-aud-Français_FR_", "audio-fr", "fr"]]) {
    const h = await hlsRun({ kind: "audio", audio_format_id: id, audio_language: language }, withOddNames);
    assert.deepEqual(opened(h), [["audio", id]], id);
    assert.deepEqual(requested(h), ["master.m3u8", `${name}.m3u8`, `${name}-0.bin`], id);
  }
});

test("an audio-only HLS request names a rendition by its whole id when the id of another group ends alike", async () => {
  for (const [id, group] of [["hls-a-hi-English", "a-hi"], ["a-hi-English", "a-hi"], ["hls-hi-English", "hi"], ["hi-English", "hi"]]) {
    const h = await hlsRun({ kind: "audio", audio_format_id: id, audio_language: "en" }, withHyphenGroups);
    assert.deepEqual(opened(h), [["audio", id]], id);
    assert.deepEqual(requested(h), ["master.m3u8", `audio-${group}.m3u8`, `audio-${group}-0.bin`], id);
  }
  // An id ends with the id of a format only at a "-": "xhi-English" is not in the group "hi".
  for (const id of ["ahi-English", "hls-xhi-English"]) await assertRefused({ audio_format_id: id, audio_language: "en" }, withHyphenGroups);
});

test("an audio-only HLS request prefers a variant that carries audio only to taking the audio out of a combined one", async () => {
  const h = await hlsRun({ kind: "audio", expected_audio: true }, withMainAndAudioVariant);
  assert.deepEqual(opened(h), [["audio", `${HOST}a128.m3u8`]]);
  assert.deepEqual(requested(h), ["master.m3u8", "a128.m3u8", "a128-0.bin"], "no video is fetched");
  const named = await hlsRun({ kind: "audio", audio_format_id: "hls-aud-Main", audio_language: "en" }, withMainAndAudioVariant);
  assert.deepEqual(requested(named), ["master.m3u8", "v720.m3u8", "v720-0.bin"], "a rendition that is named is the one taken");
});

test("an audio-only HLS request leaves out a variant that carries video only", async () => {
  const h = await hlsRun({ kind: "audio", expected_audio: true }, withVideoOnlyVariant);
  assert.deepEqual(opened(h), [["audio", `${HOST}v360.m3u8`]]);
  assert.deepEqual(requested(h), ["master.m3u8", "v360.m3u8", "v360-0.bin"]);
  await assertRefused({ audio_format_id: "2400" }, withVideoOnlyVariant);
  const only = await hlsRun({ kind: "audio", expected_audio: true }, { [MASTER]: hlsMaster(...variantLine('BANDWIDTH=2400000,CODECS="avc1.4d401f"', "v720")), ...segmentsOf("v720") });
  assert.deepEqual(requested(only), ["master.m3u8"]);
  assert.match(only.commands.at(-1).error, /audio group was not found/);
});

test("a video HLS request transfers the chosen variant with its audio rendition, or the combined variant alone", async () => {
  const separate = await hlsRun({ kind: "video" }, withSeparateAudio);
  assert.deepEqual(opened(separate), [["video", `${HOST}v720.m3u8`], ["audio", "hi-English"]]);
  assert.deepEqual(requested(separate), ["master.m3u8", "v720.m3u8", "audio-hi.m3u8", "v720-0.bin", "audio-hi-0.bin"]);
  const combined = await hlsRun({ kind: "video", max_height: 360 }, withCombinedAudio);
  assert.deepEqual(opened(combined), [["file", `${HOST}v360.m3u8`]]);
  assert.deepEqual(requested(combined), ["master.m3u8", "v360.m3u8", "v360-0.bin"]);
});
