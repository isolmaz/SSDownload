"use strict";

(function installRelayContent() {
  const relayApi = globalThis.browser || chrome;
  // The language is followed by content.js (same isolated world) from the stored app language.
  const { t } = SSDownloadI18n;
  // Stream roles are internal keys; only the messages that name one translate it.
  const ROLE_NAMES = { video: () => t("roleVideo"), audio: () => t("roleAudio"), subtitle: () => t("roleSubtitle") };
  const roleName = role => ROLE_NAMES[role]();
  const INTERNAL_CHANNEL = "ssdownload-relay-internal";
  const MAX_CHUNK = 512 * 1024;
  const MAX_MANIFEST = 2 * 1024 * 1024;
  const MAX_RESOURCES = 100000;
  const active = new Map();
  const Sha256 = globalThis.SSDownloadRelaySha256;
  if (!Sha256) throw new Error("SSDownload relay SHA-256 module did not load");

  const PERMIT_POLL_LIMIT = 120;
  const PERMIT_POLL_MAX_MS = 1000;
  const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
  const integer = value => Number.isSafeInteger(value) && value >= 0;
  const waitForAbort = (signal, ms) => new Promise((resolve, reject) => {
    if (signal.aborted) { reject(new Error(signal.reason || t("relayStopped"))); return; }
    const timer = setTimeout(done, ms);
    function done() { signal.removeEventListener("abort", aborted); resolve(); }
    function aborted() { clearTimeout(timer); signal.removeEventListener("abort", aborted); reject(new Error(signal.reason || t("relayStopped"))); }
    signal.addEventListener("abort", aborted, {once:true});
  });
  const strongEtag = value => typeof value === "string" && value.length <= 512 && /^"[^\r\n]+"$/.test(value) ? value : null;
  const absolute = (value, base) => {
    const parsed = new URL(value, base);
    if (!/^https?:$/.test(parsed.protocol) || parsed.href.length > 16384) throw new Error(t("manifestBadAddress"));
    return parsed.href;
  };
  const digestText = text => Sha256.digestHex(new TextEncoder().encode(text));
  const hashBytes = bytes => Sha256.digestHex(bytes);

  function encodeBase64(bytes) {
    let binary = "";
    for (let offset = 0; offset < bytes.length; offset += 0x8000) binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
    return btoa(binary);
  }

  async function control(run, command, fields = {}) {
    const message={ channel: INTERNAL_CHANNEL, command, jobId: run.jobId, token: run.token, runToken: run.runToken, documentId:run.documentId, ...fields };
    for(let attempt=0;;attempt++) {
      try {
        const response = await relayApi.runtime.sendMessage(message);
        if (!response || response.error) throw new Error(String(response?.error || t("relayWorkerNoReply")));
        return response;
      } catch(error) {
        const transient=/message (?:port|channel) closed|receiving end does not exist|could not establish connection|context invalidated/i.test(String(error?.message||error));
        if(!transient||attempt>=20||run.controller.signal.aborted)throw error;
        // The native receiver makes the one uncertain Chunk idempotent. Retrying
        // this exact message after MV3 worker recreation cannot advance twice.
        await sleep(250);
      }
    }
  }

  function ensureCurrent(run) {
    if (run.controller.signal.aborted || active.get(run.jobId) !== run) throw new Error(run.controller.signal.reason || t("relayStopped"));
  }

  function newRequestId() {
    if (typeof globalThis.crypto?.randomUUID !== "function") throw new Error(t("relayRequestIdFailed"));
    return `relay-${globalThis.crypto.randomUUID()}`;
  }

  async function acquireFetchPermit(run, requestId) {
    for (let attempt = 0; attempt < PERMIT_POLL_LIMIT; attempt++) {
      ensureCurrent(run);
      const response = await control(run, "begin_request", {requestId});
      if (response.request_granted === true) return;
      if (response.request_granted !== false) throw new Error(t("relayPermitReplyInvalid"));
      await waitForAbort(run.controller.signal, Math.min(PERMIT_POLL_MAX_MS, 100 + attempt * 25));
    }
    throw new Error(t("relayConcurrencyLimit"));
  }

  async function pageFetch(run, url, {offset = 0, etag = null, access = null} = {}) {
    const requestId = newRequestId();
    await acquireFetchPermit(run, requestId);
    let released = false;
    const release = async () => {
      if (released) return;
      await control(run, "end_request", {requestId});
      released = true;
    };
    let response;
    try {
      ensureCurrent(run);
      const headers = new Headers(access?.headers || {});
      if (offset) {
        headers.set("Range", `bytes=${offset}-`);
        if (etag) headers.set("If-Range", etag);
      }
      const options = {method:"GET",credentials:"include",mode:"cors",redirect:access?"error":"follow",cache:"no-store",headers,signal:run.controller.signal};
      if(access)options.referrer=access.referer||"";
      response = await fetch(url, options);
      if (!response.ok) throw new Error(t("relayFetchHttpStatus", response.status, new URL(url).origin));
      if (offset) {
        const range = /^bytes\s+(\d+)-(\d+)\/(\d+|\*)$/i.exec(response.headers.get("content-range") || "");
        if (response.status !== 206 || !range || Number(range[1]) !== offset) throw new Error(t("relayRangeUnverified"));
        if (etag && strongEtag(response.headers.get("etag")) !== etag) throw new Error(t("relayEtagChanged"));
      }
      if (!response.body) throw new Error(t("relayNoBodyStream"));
      return {response, release};
    } catch (error) {
      try { await response?.body?.cancel(); } catch (_) {}
      await release();
      throw error;
    }
  }

  async function boundedText(fetchResult) {
    let reader=null, complete=false;
    const decoder = new TextDecoder();
    let bytes = 0, text = "";
    try {
      reader=fetchResult.response.body.getReader();
      for (;;) {
        const {done, value} = await reader.read();
        if (done) { complete = true; break; }
        bytes += value.length;
        if (bytes > MAX_MANIFEST) throw new Error(t("manifestTooLarge"));
        text += decoder.decode(value, {stream:true});
      }
      text += decoder.decode();
      return text;
    } finally {
      if (!complete) try { if(reader) await reader.cancel(); else await fetchResult.response.body.cancel(); } catch (_) {}
      reader?.releaseLock();
      await fetchResult.release();
    }
  }

  function parseAttributes(value) {
    const result = {};
    let start = 0, quoted = false;
    const fields = [];
    for (let index = 0; index <= value.length; index++) {
      const char = value[index];
      if (char === '"') quoted = !quoted;
      if (index === value.length || (char === "," && !quoted)) { fields.push(value.slice(start,index)); start=index+1; }
    }
    for (const field of fields) {
      const equal = field.indexOf("=");
      if (equal < 1) continue;
      let item = field.slice(equal+1).trim();
      if (item.startsWith('"') && item.endsWith('"')) item=item.slice(1,-1);
      result[field.slice(0,equal).trim().toUpperCase()] = item;
    }
    return result;
  }

  function hlsId(candidate, index) {
    const bandwidth = Number(candidate.attributes["AVERAGE-BANDWIDTH"] || candidate.attributes.BANDWIDTH || 0);
    const names = [candidate.attributes["GROUP-ID"] && candidate.attributes.NAME ? `${candidate.attributes["GROUP-ID"]}-${candidate.attributes.NAME.replace(/ /g,"_")}` : null, candidate.attributes.NAME, candidate.attributes["VIDEO"], candidate.attributes["GROUP-ID"], candidate.uri,
      candidate.uri.split(/[/?#]/).filter(Boolean).pop(), String(index), bandwidth ? String(Math.round(bandwidth / 1000)) : null, bandwidth ? String(bandwidth) : null];
    return names.filter(Boolean);
  }

  function selectHlsVariant(candidates, request) {
    if (!candidates.length) throw new Error(t("hlsNoVideoVariant"));
    if (request.video_format_id) {
      const exact = candidates.filter((item,index)=>hlsId(item,index).some(value=>value===request.video_format_id || request.video_format_id.endsWith(`-${value}`)));
      if (exact.length !== 1) throw new Error(t("hlsVariantIdAmbiguous"));
      return exact[0];
    }
    const height = item => Number(/x(\d+)$/i.exec(item.attributes.RESOLUTION || "")?.[1] || 0);
    let allowed = candidates;
    if (request.exact_height) allowed = candidates.filter(item=>height(item)===request.exact_height);
    else if (request.max_height) allowed = candidates.filter(item=>height(item)<=request.max_height);
    if (!allowed.length) throw new Error(t("hlsResolutionMissing"));
    return allowed.slice().sort((a,b)=>height(b)-height(a) || Number(b.attributes.BANDWIDTH||0)-Number(a.attributes.BANDWIDTH||0))[0];
  }

  // CODECS naming audio codecs alone marks a variant that carries no video; without CODECS, or with a codec
  // missing here, a variant may carry video.
  const AUDIO_CODEC = /^(?:mp4a|ac-3|ec-3|ac-4|opus|flac|alac|mhm1|mha1|mp3|vorbis|dts[celh])/i;
  const hlsAudioOnly = variant => {
    const codecs = (variant.attributes.CODECS||"").split(",").map(item=>item.trim()).filter(Boolean);
    return codecs.length > 0 && codecs.every(codec=>AUDIO_CODEC.test(codec));
  };
  // yt-dlp names an audio rendition "<GROUP-ID>-<NAME>", behind the extractor's own prefix where it has one, with
  // whitespace and , / + [ ] ( ) turned into "_".
  const hlsAudioId = rendition => `${rendition.attributes["GROUP-ID"]}-${rendition.attributes.NAME}`.replace(/[\s,/+\[\]()]/g,"_");
  const namedBy = (id, own) => String(id)===own || String(id).endsWith(`-${own}`);

  // The one rendition of an audio group that a selection names. Of several ids that end the same way, the longest
  // is the more exact; a group other than the selection's own never answers for it.
  function matchHlsAudio(group, selection) {
    const matches = group.filter(item=>(!selection.id || namedBy(selection.id,hlsAudioId(item))) && (!selection.language || item.attributes.LANGUAGE===selection.language));
    const longest = selection.id ? Math.max(...matches.map(item=>hlsAudioId(item).length)) : 0;
    const exact = selection.id ? matches.filter(item=>hlsAudioId(item).length===longest) : matches;
    return exact.length===1 ? exact[0] : null;
  }

  // An audio-only request takes its audio from the first of these that fits the selection (all of them without one):
  // the renditions of an audio group, the group with the most channels first and then the one of the best variant
  // (yt-dlp sorts by channel count ahead of codec and bitrate, but lists none for the renditions of an HLS playlist, so
  // the playlist's CHANNELS decides here); a variant that carries audio only; any other variant, whose stream holds the
  // video as well. The application keeps the audio track of the stream and drops the rest, so the last still gives an
  // audio file, at the cost of the video bytes, as yt-dlp's bestaudio/best does.
  function selectHlsAudio(variants, media, selections) {
    const bandwidth = item => Number(item.attributes.BANDWIDTH || 0);
    const ranked = variants.slice().sort((a,b)=>bandwidth(b)-bandwidth(a));
    const audio = media.filter(item=>item.attributes.TYPE==="AUDIO");
    const groups = new Set(audio.map(item=>item.attributes["GROUP-ID"]));
    const options = [];
    for (const id of new Set(ranked.map(item=>item.attributes.AUDIO).filter(item=>groups.has(item)))) {
      const group = audio.filter(item=>item.attributes["GROUP-ID"]===id);
      const chosen = selections.length ? selections.map(selection=>matchHlsAudio(group,selection)) : [group.find(item=>item.attributes.DEFAULT==="YES")||group[0]];
      if (chosen.some(item=>!item)) continue;
      const users = ranked.filter(item=>item.attributes.AUDIO===id);
      const option = {tier:0,channels:Number.parseInt(chosen[0].attributes.CHANNELS,10)||0,bandwidth:bandwidth(users[0])};
      if (chosen.every(item=>item.uri)) options.push({...option,variant:users[0],renditions:chosen,own:null});
      // A rendition without URI is the audio of every variant that uses the group: the cheapest one holds it as well.
      else if (chosen.length===1) options.push({...option,variant:users.at(-1),renditions:[],own:{id:selections[0]?.id||hlsAudioId(chosen[0]),language:selections[0]?.language||chosen[0].attributes.LANGUAGE||null}});
    }
    for (const variant of ranked) {
      if (groups.has(variant.attributes.AUDIO)) continue;
      const ids = hlsId(variant,variants.indexOf(variant));
      if (selections.length && !(selections.length===1 && selections[0].id && ids.some(value=>namedBy(selections[0].id,value)))) continue;
      options.push({tier:hlsAudioOnly(variant)?1:2,channels:0,bandwidth:bandwidth(variant),variant,renditions:[],own:{id:selections[0]?.id||ids[0],language:selections[0]?.language||null}});
    }
    options.sort((a,b)=>a.tier-b.tier||b.channels-a.channels||b.bandwidth-a.bandwidth);
    if (!options.length) throw new Error(selections.length ? t("hlsRenditionAmbiguous", roleName("audio")) : t("hlsAudioGroupMissing"));
    return options[0];
  }

  function selectHlsRenditions(candidates, selections, group, role) {
    const allowed = candidates.filter(item=>!group || item.attributes["GROUP-ID"]===group);
    if (!selections.length) {
      if (!group) return [];
      const chosen = allowed.find(item=>item.attributes.DEFAULT==="YES") || allowed[0];
      return chosen ? [chosen] : [];
    }
    return selections.map(selection => {
      if (selection.automatic) throw new Error(t("hlsAutoSubtitleUnverifiable"));
      const matches = allowed.filter((item,index)=>(!selection.id || hlsId(item,index).some(value=>value===selection.id || String(selection.id).endsWith(`-${value}`))) && (!selection.language || item.attributes.LANGUAGE===selection.language));
      if (matches.length !== 1) throw new Error(t("hlsRenditionAmbiguous", roleName(role)));
      return matches[0];
    });
  }

  async function fetchManifest(run, url) {
    ensureCurrent(run);
    const fetchResult = await pageFetch(run, url);
    const response = fetchResult.response;
    const text = await boundedText(fetchResult);
    return {url:response.url || url,text,etag:strongEtag(response.headers.get("etag")),type:(response.headers.get("content-type")||"").toLowerCase()};
  }

  function parseHlsMedia(text, base) {
    const lines=text.split(/\r?\n/).map(line=>line.trim());
    if (!lines.includes("#EXT-X-ENDLIST")) throw new Error(t("hlsLiveUnsupported"));
    if (lines.some(line=>/^#EXT-X-(?:DISCONTINUITY|DISCONTINUITY-SEQUENCE|SKIP|GAP)(?::|$)/.test(line))) throw new Error(t("hlsDiscontinuity"));
    if (lines.some(line=>/^#EXT-X-(?:KEY|SESSION-KEY):/.test(line) && !/METHOD=NONE(?:,|$)/.test(line))) throw new Error(t("hlsEncrypted"));
    if (lines.some(line=>/^#EXT-X-BYTERANGE:/.test(line))) throw new Error(t("hlsByterange"));
    if (lines.some(line=>/^#EXT-X-DATERANGE:.*(?:CLASS|SCTE35)/i.test(line))) throw new Error(t("hlsAdMarkers"));
    const resources=[];
    let duration=0,durationCount=0,segmentCount=0;
    for(const line of lines) {
      if(line.startsWith("#EXTINF:")) {
        const seconds=Number(line.slice(8).split(",",1)[0]);
        if(!Number.isFinite(seconds)||seconds<=0)throw new Error(t("hlsSegmentDurationInvalid"));
        duration+=seconds;durationCount++;
      } else if(line.startsWith("#EXT-X-MAP:")) {
        const attrs=parseAttributes(line.slice(line.indexOf(":")+1));
        if(!attrs.URI || attrs.BYTERANGE) throw new Error(t("hlsInitUnsupported"));
        resources.push(absolute(attrs.URI,base));
      } else if(line && !line.startsWith("#")) {resources.push(absolute(line,base));segmentCount++;}
      if(resources.length>MAX_RESOURCES) throw new Error(t("hlsSegmentLimit"));
    }
    if(!segmentCount||segmentCount!==durationCount||!Number.isFinite(duration)) throw new Error(t("hlsVodIncomplete"));
    return {resources,duration};
  }

  async function hlsGraph(run, root) {
    const lines=root.text.split(/\r?\n/).map(line=>line.trim());
    if(!root.text.trimStart().startsWith("#EXTM3U")) throw new Error(t("hlsSignatureInvalid"));
    const variants=[], media=[];
    let pending=null;
    for(const line of lines) {
      if(line.startsWith("#EXT-X-STREAM-INF:")) pending=parseAttributes(line.slice(line.indexOf(":")+1));
      else if(pending && line && !line.startsWith("#")) { variants.push({uri:absolute(line,root.url),attributes:pending}); pending=null; }
      else if(line.startsWith("#EXT-X-MEDIA:")) {
        const attributes=parseAttributes(line.slice(line.indexOf(":")+1));
        media.push({uri:attributes.URI?absolute(attributes.URI,root.url):null,attributes});
      }
    }
    const renditions=media.filter(item=>item.uri);
    if(!variants.length) {
      if(run.request.external_subtitles?.length) throw new Error(t("hlsExternalSubtitleMuxed"));
      return {kind:"hls",manifests:[root],streams:[{streamId:1,role:"file",formatId:null,language:null,...parseHlsMedia(root.text,root.url)}]};
    }
    const audioSelections=run.request.audio_tracks?.length ? run.request.audio_tracks : run.request.audio_format_id || run.request.audio_language
      ? [{id:run.request.audio_format_id,language:run.request.audio_language}] : [];
    const chosen=run.request.kind==="audio"?selectHlsAudio(variants,media,audioSelections):null;
    const variant=chosen?chosen.variant:selectHlsVariant(variants,run.request);
    const videoManifest=!chosen||chosen.own?await fetchManifest(run,variant.uri):null;
    const audio=chosen?chosen.renditions:selectHlsRenditions(renditions.filter(item=>item.attributes.TYPE==="AUDIO"),audioSelections,variant.attributes.AUDIO,"audio");
    if (!chosen && (run.request.expected_audio===true || audioSelections.length) && !audio.length && variant.attributes.AUDIO) throw new Error(t("hlsAudioGroupMissing"));
    const subtitleSelections=run.request.subtitle_tracks?.length?run.request.subtitle_tracks:(run.request.subtitle_languages||[]).map(language=>({language,automatic:run.request.subtitle_mode==="automatic"}));
    const subtitles=run.request.external_subtitles?.length||!subtitleSelections.length?[]:selectHlsRenditions(renditions.filter(item=>item.attributes.TYPE==="SUBTITLES"),subtitleSelections,variant.attributes.SUBTITLES,"subtitle");
    const manifests=[root], streams=[];
    if(videoManifest) {
      manifests.push(videoManifest);
      streams.push({streamId:1,role:chosen?"audio":audio.length?"video":"file",formatId:chosen?chosen.own.id:run.request.video_format_id||hlsId(variant,variants.indexOf(variant))[0],language:chosen?chosen.own.language:null,...parseHlsMedia(videoManifest.text,videoManifest.url)});
    }
    if(!chosen && !audio.length && (run.request.external_subtitles?.length || subtitles.length)) throw new Error(t("hlsSeparateSubtitleMuxedAudio"));
    let streamId=streams.length+1;
    for(let index=0;index<audio.length;index++) {
      const manifest=await fetchManifest(run,audio[index].uri); manifests.push(manifest);
      const selected=audioSelections[index]||{};
      streams.push({streamId:streamId++,role:"audio",formatId:selected.id||hlsId(audio[index],renditions.indexOf(audio[index]))[0],language:selected.language||audio[index].attributes.LANGUAGE||null,...parseHlsMedia(manifest.text,manifest.url)});
    }
    for(let index=0;index<subtitles.length;index++) {
      const manifest=await fetchManifest(run,subtitles[index].uri); manifests.push(manifest);
      streams.push({streamId:streamId++,role:"subtitle",formatId:null,language:subtitleSelections[index]?.language||subtitles[index].attributes.LANGUAGE||null,...parseHlsMedia(manifest.text,manifest.url)});
    }
    for(const subtitle of run.request.external_subtitles||[]) streams.push({streamId:streamId++,role:"subtitle",formatId:null,language:subtitle.language||null,access:{headers:subtitle.headers||{},referer:subtitle.referer||null},resources:[absolute(subtitle.url,root.url)]});
    return {kind:"hls",manifests,streams,duration:streams[0].duration};
  }

  const children = (node,name) => Array.from(node?.childNodes||[]).filter(child=>child.nodeType===1 && child.localName===name);
  const first = (node,name) => children(node,name)[0] || null;
  const isoDuration = value => {
    const match=/^P(?:([0-9.]+)D)?(?:T(?:([0-9.]+)H)?(?:([0-9.]+)M)?(?:([0-9.]+)S)?)?$/.exec(value||"");
    return match ? Number(match[1]||0)*86400+Number(match[2]||0)*3600+Number(match[3]||0)*60+Number(match[4]||0) : NaN;
  };
  function dashBase(node, parent) { const value=first(node,"BaseURL")?.textContent?.trim(); return value?absolute(value,parent):parent; }
  function dashTemplate(value, representation, number, time) {
    return value.replace(/\$RepresentationID\$/g,representation.getAttribute("id")||"").replace(/\$Bandwidth\$/g,representation.getAttribute("bandwidth")||"")
      .replace(/\$Number(?:%0(\d+)d)?\$/g,(_,width)=>String(number).padStart(Number(width)||0,"0")).replace(/\$Time\$/g,String(time)).replace(/\$\$/g,"$");
  }

  function dashResources(representation, adaptation, base, duration) {
    const template=first(representation,"SegmentTemplate")||first(adaptation,"SegmentTemplate");
    const list=first(representation,"SegmentList")||first(adaptation,"SegmentList");
    if(template) {
      const media=template.getAttribute("media"), initialization=template.getAttribute("initialization");
      if(!media) throw new Error(t("dashTemplateMediaMissing"));
      const resources=[];
      if(initialization) resources.push(absolute(dashTemplate(initialization,representation,0,0),base));
      const start=Number(template.getAttribute("startNumber")||1), timescale=Number(template.getAttribute("timescale")||1);
      const timeline=first(template,"SegmentTimeline");
      if(timeline) {
        let time=0,number=start;
        for(const segment of children(timeline,"S")) {
          const d=Number(segment.getAttribute("d")), repeat=Number(segment.getAttribute("r")||0);
          if(!Number.isFinite(d)||d<=0||repeat<0) throw new Error(t("dashTimelineUnsupported"));
          if(segment.hasAttribute("t")) time=Number(segment.getAttribute("t"));
          for(let index=0;index<=repeat;index++) { resources.push(absolute(dashTemplate(media,representation,number++,time),base)); time+=d; if(resources.length>MAX_RESOURCES)throw new Error(t("dashSegmentLimit")); }
        }
      } else {
        const segmentDuration=Number(template.getAttribute("duration"));
        if(!Number.isFinite(duration)||!Number.isFinite(segmentDuration)||segmentDuration<=0||timescale<=0) throw new Error(t("dashDurationTemplateIncomplete"));
        const count=Math.ceil(duration*timescale/segmentDuration);
        if(count<=0||count>MAX_RESOURCES) throw new Error(t("dashSegmentLimit"));
        for(let index=0;index<count;index++) resources.push(absolute(dashTemplate(media,representation,start+index,index*segmentDuration),base));
      }
      return resources;
    }
    if(list) {
      const resources=[]; const init=first(list,"Initialization")?.getAttribute("sourceURL"); if(init)resources.push(absolute(init,base));
      for(const segment of children(list,"SegmentURL")) { if(segment.getAttribute("mediaRange"))throw new Error(t("dashMediaRange")); resources.push(absolute(segment.getAttribute("media"),base)); }
      if(!resources.length)throw new Error(t("dashSegmentListEmpty")); return resources;
    }
    throw new Error(t("dashSegmentBaseUnsupported"));
  }

  function selectDash(candidates,id,request,role) {
    if(id) {
      const exact=candidates.filter(item=>item.representation.getAttribute("id")===id || String(id).endsWith(`-${item.representation.getAttribute("id")}`));
      if(exact.length!==1)throw new Error(t("dashFormatIdMissing", roleName(role)));
      return exact[0];
    }
    if(role==="video") {
      let allowed=candidates;
      const height=item=>Number(item.representation.getAttribute("height")||0);
      if(request.exact_height)allowed=candidates.filter(item=>height(item)===request.exact_height);
      else if(request.max_height)allowed=candidates.filter(item=>height(item)<=request.max_height);
      if(!allowed.length)throw new Error(t("dashResolutionMissing"));
      return allowed.slice().sort((a,b)=>height(b)-height(a)||Number(b.representation.getAttribute("bandwidth")||0)-Number(a.representation.getAttribute("bandwidth")||0))[0];
    }
    return candidates[0];
  }

  async function dashGraph(run, root) {
    const documentValue=new DOMParser().parseFromString(root.text,"application/xml");
    if(documentValue.querySelector("parsererror"))throw new Error(t("dashXmlInvalid"));
    const mpd=documentValue.documentElement;
    if(mpd.localName!=="MPD")throw new Error(t("dashMpdRootMissing"));
    if((mpd.getAttribute("type")||"static").toLowerCase()!=="static")throw new Error(t("dashLiveUnsupported"));
    if(documentValue.getElementsByTagNameNS("*","ContentProtection").length)throw new Error(t("dashEncrypted"));
    const periods=children(mpd,"Period"); if(periods.length!==1)throw new Error(t("dashMultiPeriod"));
    const duration=isoDuration(periods[0].getAttribute("duration")||mpd.getAttribute("mediaPresentationDuration"));
    const mpdBase=dashBase(mpd,root.url),periodBase=dashBase(periods[0],mpdBase);
    const byRole={video:[],audio:[],subtitle:[]};
    for(const adaptation of children(periods[0],"AdaptationSet")) {
      const adaptationBase=dashBase(adaptation,periodBase);
      for(const representation of children(adaptation,"Representation")) {
        const mime=(representation.getAttribute("mimeType")||adaptation.getAttribute("mimeType")||"").toLowerCase();
        const content=(adaptation.getAttribute("contentType")||"").toLowerCase();
        const role=mime.startsWith("video/")||content==="video"?"video":mime.startsWith("audio/")||content==="audio"?"audio":mime.includes("text")||mime.includes("ttml")||content==="text"?"subtitle":null;
        if(role)byRole[role].push({representation,adaptation,base:dashBase(representation,adaptationBase),language:representation.getAttribute("lang")||adaptation.getAttribute("lang")||null});
      }
    }
    const streams=[];let streamId=1;
    if(run.request.kind!=="audio") {
      const video=selectDash(byRole.video,run.request.video_format_id,run.request,"video");
      streams.push({streamId:streamId++,role:"video",formatId:video.representation.getAttribute("id")||run.request.video_format_id||null,language:null,resources:dashResources(video.representation,video.adaptation,video.base,duration)});
    }
    const requestedAudio=run.request.audio_tracks?.length?run.request.audio_tracks:run.request.audio_format_id||run.request.audio_language?[{id:run.request.audio_format_id,language:run.request.audio_language}]:[];
    let audio=[];
    if(requestedAudio.length) audio=requestedAudio.map(selected=>{
      const candidates=byRole.audio.filter(item=>!selected.language||item.language===selected.language);
      if(!selected.id&&candidates.length!==1)throw new Error(t("dashAudioLanguageAmbiguous"));
      return selectDash(candidates,selected.id,run.request,"audio");
    });
    else if(run.request.kind==="audio"||run.request.expected_audio===true) { const selected=selectDash(byRole.audio,null,run.request,"audio"); if(selected)audio=[selected]; }
    for(let index=0;index<audio.length;index++)streams.push({streamId:streamId++,role:"audio",formatId:requestedAudio[index]?.id||audio[index].representation.getAttribute("id")||null,
      language:requestedAudio[index]?.language||audio[index].language,resources:dashResources(audio[index].representation,audio[index].adaptation,audio[index].base,duration)});
    const requestedSubtitles=run.request.external_subtitles?.length?[]:(run.request.subtitle_tracks?.length?run.request.subtitle_tracks:(run.request.subtitle_languages||[]).map(language=>({language,automatic:run.request.subtitle_mode==="automatic"})));
    for(const selected of requestedSubtitles) {
      if(selected.automatic)throw new Error(t("dashAutoSubtitleUnverifiable"));
      const candidates=byRole.subtitle.filter(item=>!selected.language||item.language===selected.language);
      if(candidates.length!==1)throw new Error(t("dashSubtitleLanguageAmbiguous"));
      streams.push({streamId:streamId++,role:"subtitle",formatId:null,language:selected.language,resources:dashResources(candidates[0].representation,candidates[0].adaptation,candidates[0].base,duration)});
    }
    for(const subtitle of run.request.external_subtitles||[]) streams.push({streamId:streamId++,role:"subtitle",formatId:null,language:subtitle.language||null,access:{headers:subtitle.headers||{},referer:subtitle.referer||null},resources:[absolute(subtitle.url,root.url)]});
    if(!streams.length)throw new Error(t("dashNoStream"));
    return {kind:"dash",manifests:[root],streams,duration:Number.isFinite(duration)?duration:null};
  }

  function checkpoint(run, graphHash, hash, resourceIndex, resourceOffset, etag) {
    const value={v:1,kind:run.kind,graph:graphHash,resource:resourceIndex,resourceOffset,resourceEtag:etag||null,sha:hash.exportState()};
    if(JSON.stringify(value).length>4096)throw new Error(t("relayCheckpointTooLarge"));
    return value;
  }

  async function* transferChunks(reader) {
    let chunk = null, used = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        if (used) yield chunk.subarray(0, used);
        return;
      }
      for (let offset = 0; offset < value.length;) {
        if (!chunk) chunk = new Uint8Array(MAX_CHUNK);
        const count = Math.min(MAX_CHUNK - used, value.length - offset);
        chunk.set(value.subarray(offset, offset + count), used);
        used += count;
        offset += count;
        if (used === MAX_CHUNK) {
          const ready = chunk;
          chunk = null;
          used = 0;
          yield ready;
        }
      }
    }
  }

  async function sendPart(run, state, bytes, cp, hash) {
    ensureCurrent(run);
    hash.update(bytes);
    cp.sha=hash.exportState();
    const response=await control(run,"chunk",{streamId:state.stream_id,sequence:state.next_sequence,offset:state.offset,
      data:encodeBase64(bytes),sha256:hashBytes(bytes),clientCheckpoint:cp});
    const acknowledged=response.streams.find(item=>item.stream_id===state.stream_id);
    if(!acknowledged||acknowledged.offset!==state.offset+bytes.length||acknowledged.next_sequence!==state.next_sequence+1)throw new Error(t("relayAckMismatch"));
    state=acknowledged;
    return state;
  }

  async function transferResources(run, stream, graphHash, existing) {
    const open=await control(run,"open",{streamId:stream.streamId,role:stream.role,formatId:stream.formatId,language:stream.language,total:null,etag:`"${graphHash}"`});
    let state=open.streams.find(item=>item.stream_id===stream.streamId);
    if(!state)throw new Error(t("relayStreamNotReported"));
    if(state.sealed)return;
    const saved=state.client_checkpoint;
    let hash,resourceIndex=0,resourceOffset=0,resourceEtag=null;
    if(state.offset) {
      if(!saved||saved.v!==1||saved.kind!==run.kind||saved.graph!==graphHash||!saved.sha)throw new Error(t("relayResumeRecordMismatch"));
      hash=new Sha256(saved.sha); resourceIndex=Number(saved.resource);resourceOffset=Number(saved.resourceOffset);resourceEtag=saved.resourceEtag;
      if(hash.bytes!==state.offset||!integer(resourceIndex)||!integer(resourceOffset)||resourceIndex>stream.resources.length)throw new Error(t("relayCursorMismatch"));
    } else hash=new Sha256();
    for(let index=resourceIndex;index<stream.resources.length;index++) {
      ensureCurrent(run);
      const start=index===resourceIndex?resourceOffset:0;
      const resumeEtag=index===resourceIndex?resourceEtag:null;
      if(start&&!resumeEtag)throw new Error(t("relayMidSegmentNoEtag"));
      const fetchResult=await pageFetch(run, stream.resources[index], {offset:start,etag:resumeEtag,access:stream.access});
      const response=fetchResult.response;
      const currentEtag=strongEtag(response.headers.get("etag"));
      let reader=null, complete=false;
      try {
        reader=response.body.getReader(); let pending=null,consumed=start;
        for await (const part of transferChunks(reader)) {
          if(pending) state=await sendPart(run,state,pending.bytes,checkpoint(run,graphHash,hash,index,pending.end,currentEtag),hash);
          consumed+=part.length; pending={bytes:part,end:consumed};
        }
        complete=true;
        if(pending) state=await sendPart(run,state,pending.bytes,checkpoint(run,graphHash,hash,index+1,0,null),hash);
      } finally {
        if(!complete) try { if(reader) await reader.cancel(); else await response.body.cancel(); } catch (_) {}
        reader?.releaseLock();
        await fetchResult.release();
      }
    }
    await control(run,"seal",{streamId:stream.streamId,sha256:hash.digestHex()});
  }

  async function directTransfer(run, fetchResult, existing) {
    const response=fetchResult.response;
    let reader=null, complete=false, alreadySealed=false, digest=null;
    try {
      const etag=strongEtag(response.headers.get("etag"));
      const range=/^bytes\s+(\d+)-(\d+)\/(\d+)$/i.exec(response.headers.get("content-range")||"");
      const length=Number(response.headers.get("content-length"));
      const total=range?Number(range[3]):Number.isSafeInteger(length)&&length>=0?length:null;
      const open=await control(run,"open",{streamId:1,role:"file",formatId:null,language:null,total,etag});
      let state=open.streams.find(item=>item.stream_id===1); if(!state)throw new Error(t("relayFileStreamNotOpened"));
      if(state.sealed) alreadySealed=true;
      else {
        let hash=state.offset?new Sha256(state.client_checkpoint?.sha):new Sha256();
        if(hash.bytes!==state.offset||(state.offset&&state.client_checkpoint?.offset!==state.offset))throw new Error(t("relayFileCursorMismatch"));
        reader=response.body.getReader();let pending=null,consumed=state.offset;
        for await (const part of transferChunks(reader)) {
          if(pending)state=await sendPart(run,state,pending.bytes,{v:1,kind:"direct",sha:hash.exportState(),offset:pending.end},hash);
          consumed+=part.length;pending={bytes:part,end:consumed};
        }
        complete=true;
        if(pending)state=await sendPart(run,state,pending.bytes,{v:1,kind:"direct",sha:hash.exportState(),offset:pending.end},hash);
        digest=hash.digestHex();
      }
    } finally {
      if(!complete) {
        try { if(reader) await reader.cancel(); else await response.body.cancel(); } catch (_) {}
      }
      reader?.releaseLock();
      await fetchResult.release();
    }
    if(!alreadySealed) await control(run,"seal",{streamId:1,sha256:digest});
    await control(run,"finish");
  }

  // A web page is never the file a relay job asked for. A video or audio job has no use for one;
  // for any other job the application's own rule applies (SSD-NET-004): the page that a redirect
  // led to is where a site sends a request without its session, so it is refused unless the
  // server sent it as an attachment or the file's name, the job's or else the one the application
  // takes from the last segment of the address, is a page's.
  function webPageInsteadOfFile(run, response, type) {
    if (!/^\s*(?:text\/html|application\/xhtml\+xml)\s*(?:;|$)/.test(type)) return false;
    if (run.request.kind === "video" || run.request.kind === "audio") return true;
    const attachment = /^\s*attachment\s*(?:;|$)/i.test(response.headers.get("content-disposition") || "");
    const name = run.request.filename || new URL(run.request.url).pathname.split("/").pop();
    return response.redirected === true && !attachment && !/\.(?:html?|xhtml)$/i.test(name);
  }

  async function runTransfer(run) {
    try {
      const existing=(run.streams||[]).find(item=>item.stream_id===1);
      const directResume=existing?.client_checkpoint?.kind==="direct"&&existing.offset>0;
      if(directResume&&existing.sealed){await control(run,"finish");return;}
      if(directResume&&!strongEtag(existing.etag))throw new Error(t("relayDirectNoEtag"));
      const rootFetch=await pageFetch(run, run.request.url, {offset:directResume?existing.offset:0,etag:directResume?existing.etag:null});
      const rootResponse=rootFetch.response;
      const type=(rootResponse.headers.get("content-type")||"").toLowerCase(), url=(rootResponse.url||run.request.url).toLowerCase();
      const manifestHint=/mpegurl/.test(type)||/\.m3u8(?:$|[?#])/.test(url)||/dash\+xml/.test(type)||/\.mpd(?:$|[?#])/.test(url);
      if(!manifestHint) {
        if(run.request.audio_tracks?.length||run.request.external_subtitles?.length||run.request.subtitle_tracks?.length) {
          try { await rootResponse.body.cancel(); } finally { await rootFetch.release(); }
          throw new Error(t("relayDirectWithTracks"));
        }
        if(webPageInsteadOfFile(run,rootResponse,type)) {
          try { await rootResponse.body.cancel(); } finally { await rootFetch.release(); }
          throw new Error(t("relayPageNotFile"));
        }
        run.kind="direct";await directTransfer(run,rootFetch,existing);return;
      }
      if(directResume) {
        try { await rootResponse.body.cancel(); } finally { await rootFetch.release(); }
        throw new Error(t("relayDirectNowManifest"));
      }
      const root={url:rootResponse.url||run.request.url,text:await boundedText(rootFetch),etag:strongEtag(rootResponse.headers.get("etag")),type};
      let graph;
      if(root.text.trimStart().startsWith("#EXTM3U"))graph=await hlsGraph(run,root);
      else graph=await dashGraph(run,root);
      run.kind=graph.kind;
      const graphHash=digestText(JSON.stringify({kind:graph.kind,manifests:graph.manifests.map(item=>[item.url,digestText(item.text)]),streams:graph.streams.map(item=>[item.role,item.formatId,item.language,item.resources])}));
      for(const stream of graph.streams)await transferResources(run,stream,graphHash,(run.streams||[]).find(item=>item.stream_id===stream.streamId));
      await control(run,"finish",{sourceDuration:graph.duration??graph.streams[0]?.duration??null});
    } catch(error) {
      if(active.get(run.jobId)===run&&!run.controller.signal.aborted)await control(run,"failed",{error:String(error?.message||error).slice(0,500)}).catch(()=>{});
      throw error;
    } finally {if(active.get(run.jobId)===run)active.delete(run.jobId);}
  }

  relayApi.runtime.onMessage.addListener((message,_sender,sendResponse)=>{
    if(!message||message.channel!==INTERNAL_CHANNEL)return false;
    if(message.command==="abort") {
      const run=active.get(message.jobId);
      if(!run||run.runToken!==message.runToken){sendResponse?.({ok:true});return false;}
      run.controller.abort(String(message.reason||t("relayStopped")));
      run.done.then(()=>sendResponse?.({ok:true}),error=>sendResponse?.({ok:false,settled:true,error:String(error?.message||error)}));
      return true;
    }
    if(message.command!=="start")return false;
    (async()=>{
      if(!message.request||typeof message.request.url!=="string"||!/^https?:/.test(message.request.url)||typeof message.token!=="string"||typeof message.runToken!=="string") {
        throw new Error(t("relayStartInvalid"));
      }
      const previous=active.get(message.jobId);
      if(previous){previous.controller.abort(t("relayNewAuthorization"));await previous.done;}
      const run={jobId:message.jobId,token:message.token,runToken:message.runToken,documentId:message.documentId,request:message.request,streams:message.streams,controller:new AbortController(),kind:"unknown",done:null};
      active.set(run.jobId,run);
      run.done=runTransfer(run);
      run.done.catch(()=>{});
      return {ok:true};
    })().then(sendResponse,error=>sendResponse({error:String(error?.message||error)}));
    return true;
  });
})();
