"use strict";
// Runs in the page world. Observe metadata the player has already consumed;
// never refetch, clone a response, read cookies, or retain video payloads.
(() => {
  const LIMIT = 512 * 1024;
  const capabilities={pageFetchConsumed:true,pageXhrConsumed:true,performanceResourceUrl:true,
    workerResponseBody:false,mseAppendBufferAncestry:false,responseCloning:false,drmBypass:false};
  let enabled = false;
  let installed = false;
  const originals = {};
  let wrappers = null;
  const seen = new Set();
  const send = window.postMessage.bind(window);
  // Correlates controls with the current worker session. Both the attribute and
  // messages are page-visible; the isolated script and worker enforce privacy.
  function controlToken() {
    try { return document.documentElement?.getAttribute?.("data-ssdownload-observer-token") || ""; }
    catch (_) { return ""; }
  }
  window.addEventListener("message", event => {
    if (event.source !== window || event.data?.channel !== "ssdownload-observer-control") return;
    const expected = controlToken();
    if (!expected || typeof event.data.token !== "string" || event.data.token !== expected) return;
    const next=event.data.enabled === true, changed=next!==enabled;
    enabled = next;
    if (!enabled) seen.clear();
    setObserved(enabled);
    if(changed) send({channel:"ssdownload-observer-ready",capabilities},"*");
  });
  function hlsRelations(text, base) {
    const relations=[];
    let variant=false;
    for(const raw of text.split(/\r?\n/)) {
      const line=raw.trim();
      if(line.startsWith("#EXT-X-STREAM-INF:")) { variant=true; continue; }
      let role=null, uri=null;
      if(/^#EXT-X-MEDIA:/.test(line)) { role=/\bTYPE=AUDIO\b/i.test(line)?"audio":"variant"; uri=/(?:^|,)URI="([^"]+)"/.exec(line.slice(line.indexOf(":")+1))?.[1]; }
      else if(/^#EXT-X-I-FRAME-STREAM-INF:/.test(line)) { role="variant"; uri=/(?:^|,)URI="([^"]+)"/.exec(line.slice(line.indexOf(":")+1))?.[1]; }
      else if(/^#EXT-X-MAP:/.test(line)) { role="init"; uri=/(?:^|,)URI="([^"]+)"/.exec(line.slice(line.indexOf(":")+1))?.[1]; }
      else if(variant && line && !line.startsWith("#")) { role="variant"; uri=line; variant=false; }
      else if(line && !line.startsWith("#")) { role="segment"; uri=line; }
      if(uri) { const url=new URL(uri,base).href; if(/^https?:/.test(url) && url.length<=16384) relations.push({url,role}); }
      if(relations.length>=200) break;
    }
    return relations;
  }
  function dashRelations(text, base) {
    if(typeof DOMParser!=="function") return [];
    const xml=new DOMParser().parseFromString(text,"application/xml");
    if(xml.querySelector("parsererror") || !xml.documentElement || xml.documentElement.localName!=="MPD") return [];
    const relations=[];
    const add=(value,parentBase,role)=>{
      if(!value || relations.length>=200) return;
      const url=new URL(value,parentBase).href;
      if(/^https?:/.test(url) && url.length<=16384) relations.push({url,role});
    };
    const walk=(node,parentBase,parentRole="variant")=>{
      let localBase=parentBase;
      const direct=[...node.children].find(child=>child.localName==="BaseURL");
      if(direct?.textContent?.trim()) { localBase=new URL(direct.textContent.trim(),parentBase).href; add(direct.textContent.trim(),parentBase,parentRole); }
      const content=`${node.getAttribute?.("contentType")||""} ${node.getAttribute?.("mimeType")||""}`;
      const role=/audio/i.test(content)?"audio":parentRole;
      for(const child of node.children) {
        if(child.localName==="BaseURL") continue;
        if(child.localName==="Initialization") add(child.getAttribute("sourceURL"),localBase,"init");
        else if(child.localName==="SegmentURL") add(child.getAttribute("media"),localBase,"segment");
        else {
          if(child.localName==="SegmentTemplate") { add(child.getAttribute("initialization"),localBase,"init"); add(child.getAttribute("media"),localBase,"segment"); }
          walk(child,localBase,role);
        }
        if(relations.length>=200) break;
      }
    };
    walk(xml.documentElement,base);
    return relations;
  }
  function observe(text, base, type="") {
    if (!enabled || typeof text !== "string" || text.length > LIMIT / 2) return;
    try {
      base = new URL(base, document.baseURI).href;
      if (!/^https?:/.test(base) || base.length>16384) return;
      const trimmed=text.trimStart();
      const kind=trimmed.startsWith("#EXTM3U")?"hls":(/<MPD(?:\s|>)/i.test(trimmed.slice(0,2048)) || /dash\+xml|\.mpd(?:$|[?#])/i.test(`${type} ${base}`))?"dash":null;
      if(!kind) return;
      const raw=kind==="hls"?hlsRelations(text,base):dashRelations(text,base);
      const unique=new Map();
      for(const relation of raw) if(!unique.has(`${relation.role}\n${relation.url}`)) unique.set(`${relation.role}\n${relation.url}`,relation);
      const relations=[...unique.values()].slice(0,200);
      if(JSON.stringify(relations).length>32768) return;
      const key=JSON.stringify([base,relations]);
      if (seen.has(key)) return;
      seen.add(key); while (seen.size > 64) seen.delete(seen.values().next().value);
      send({channel:"ssdownload-observed-manifest",url:base,kind,relations,children:relations.filter(r=>r.role==="variant"||r.role==="audio").map(r=>r.url)}, "*");
    } catch (_) { /* Page metadata is untrusted. */ }
  }
  // The page's prototypes are patched only while the observer is enabled and are
  // restored the moment it is turned off: with detection off the page sees its own
  // Response and XMLHttpRequest exactly as it left them. Both wrappers lean on
  // observe()'s own enabled check, so an in-flight promise from before a disable
  // still reports nothing.
  function responseWrapper(method, original) {
    return function(...args) {
      const response = this;
      return Reflect.apply(original, response, args).then(value => {
        try {
          const type=response.headers.get("content-type") || "";
          if (method === "text") observe(value, response.url, type);
          else if (value.byteLength <= LIMIT && /mpegurl|dash\+xml|\.(?:m3u8|mpd)(?:$|[?#])/i.test(`${type} ${response.url}`)) observe(new TextDecoder().decode(value), response.url, type);
        } catch (_) {}
        return value;
      });
    };
  }
  function sendWrapper(original) {
    return function(...args) {
      this.addEventListener("load", () => {
        try {
          const type=this.getResponseHeader("content-type") || "";
          if (!this.responseType || this.responseType === "text") observe(this.responseText, this.responseURL, type);
          else if (this.responseType === "arraybuffer" && this.response?.byteLength <= LIMIT && /mpegurl|dash\+xml|\.(?:m3u8|mpd)(?:$|[?#])/i.test(`${type} ${this.responseURL}`)) observe(new TextDecoder().decode(this.response), this.responseURL, type);
        } catch (_) {}
      }, {once:true});
      return Reflect.apply(original, this, args);
    };
  }
  function setObserved(next) {
    if (next === installed) return;
    if (!next && !wrappers) return;
    if (next) {
      wrappers = {};
      for (const method of ["text", "arrayBuffer"]) {
        originals[method] = Response.prototype[method];
        wrappers[method] = responseWrapper(method, originals[method]);
        try { Response.prototype[method] = wrappers[method]; } catch (_) {}
      }
      originals.send = XMLHttpRequest.prototype.send;
      wrappers.send = sendWrapper(originals.send);
      try { XMLHttpRequest.prototype.send = wrappers.send; } catch (_) {}
      capabilities.pageFetchConsumed = ["text", "arrayBuffer"].some(method => Response.prototype[method] === wrappers[method]);
      capabilities.pageXhrConsumed = XMLHttpRequest.prototype.send === wrappers.send;
      installed = true;
    } else {
      for (const method of ["text", "arrayBuffer"]) {
        try { if (Response.prototype[method] === wrappers[method]) Response.prototype[method] = originals[method]; } catch (_) {}
      }
      try { if (XMLHttpRequest.prototype.send === wrappers.send) XMLHttpRequest.prototype.send = originals.send; } catch (_) {}
      wrappers = null;
      installed = false;
    }
  }
  send({channel:"ssdownload-observer-ready",capabilities}, "*");
})();
