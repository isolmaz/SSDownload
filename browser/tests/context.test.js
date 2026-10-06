"use strict";

const assert = require("node:assert/strict");
const { webcrypto } = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const extensionDir = path.resolve(__dirname, "..", "chromium");
const source = name => fs.readFileSync(path.join(extensionDir, name), "utf8");

function event() {
  const listeners=[];
  return {listeners,addListener(listener){listeners.push(listener);}};
}

function capabilities(modes={video:true,file:true,audio:true}) {
  return {ok:true,result:{capabilities:{protocol:2,capability_version:3,usage_modes:modes,onboarding_version:1,settings_revision:4}}};
}

function backgroundHarness({session={},local={},native=async request=>request.action?.type==="capabilities"?capabilities():{ok:true,message:"ok"},permissions={async contains(){return true;},async request(){return {granted:true};}},cookies}={}) {
  const events={before:event(),redirect:event(),headers:event(),message:event(),removed:event(),updated:event(),installed:event(),startup:event(),menu:event(),activated:event()};
  const storage={session:{...session},local:{...local}}, menus=[];
  const nativeRequests=[];
  const browser={
    storage:{local:{async get(defaults){return {...defaults,...storage.local};},async set(value){Object.assign(storage.local,value);}},
      session:{async get(keys){return Object.fromEntries(keys.map(key=>[key,storage.session[key]]));},async set(value){Object.assign(storage.session,value);}}},
    webRequest:{onBeforeSendHeaders:events.before,onBeforeRedirect:events.redirect,onHeadersReceived:events.headers},
    permissions,
    cookies:cookies||{async getAllCookieStores(){return [{id:"chrome-main",tabIds:[7]},{id:"chrome-other",tabIds:[8]}];},async getPartitionKey(){return {partitionKey:{topLevelSite:"https://site.example"}};},async getAll(){return [];}},
    tabs:{onRemoved:events.removed,onUpdated:events.updated,onActivated:events.activated,async sendMessage(){},async query(){return [{id:7,url:"https://site.example/watch",active:true}];},async get(id){return {id,url:"https://site.example/watch",cookieStoreId:"chrome-main"};}},
    runtime:{id:"fixture",lastError:null,onInstalled:events.installed,onStartup:events.startup,onMessage:events.message,
      async sendNativeMessage(_host,request){nativeRequests.push(request);return native(request);},getURL(value){return `moz-extension://fixture/${value}`;}},
    action:{async setBadgeText(){},async setBadgeBackgroundColor(){},async setTitle(){}},
    contextMenus:{onClicked:events.menu,async removeAll(){menus.length=0;},create(entry){menus.push(entry);}},
    windows:{async create(){}},
  };
  const context=vm.createContext({browser,URL,crypto:webcrypto,TextEncoder,Uint8Array,Promise,Map,Set,Array,Object,Number,String,RegExp,JSON,Error,Date,
    queueMicrotask,setTimeout(callback){queueMicrotask(callback);return 1;},clearTimeout(){}});
  vm.runInContext(source("background.js"),context,{filename:"background.js"});
  // `publicState` was removed from the worker as dead code; the harness evaluates
  // the same discovery view against the worker's own globals, so every existing
  // assertion keeps reading exactly what it read before.
  vm.runInContext(`function publicState(tabId) {
  const values = mediaByTab.get(tabId);
  const items=values?Array.from(values.values()).reverse().filter(item=>!relatedRole(tabId,item.frameId,item.url)).map(item=>publicSource(tabId,item)):[];
  const allVideos=videoRecords(tabId).map(item=>publicSource(tabId,item));
  return {
    detectionEnabled,
    items,
    videos:allVideos.filter(item=>!item.suspectedAd && !item.ambiguous && !item.preview),
    candidates:allVideos.filter(item=>item.suspectedAd || item.ambiguous || item.preview),
    observationCapabilities:[...frames.values()].filter(frame=>frame.tabId===tabId && frame.observationCapabilities).map(frame=>({frameId:frame.frameId,documentId:frame.documentId,...frame.observationCapabilities})),
    capabilities:capabilityState,
    status: sessionError ? { ...(tabStatus.get(tabId) || {}), message: sessionError, error: true } : tabStatus.get(tabId) || null
  };
}`, context, { filename: "harness.js" });
  async function send(message,sender={id:"fixture",url:"moz-extension://fixture/popup.html"}) {
    const listener=events.message.listeners.at(-1);
    return new Promise(resolve=>listener(message,sender,resolve));
  }
  async function content(message,{tabId=7,frameId=0,documentId="doc",url="https://site.example/watch",title="Film"}={}) {
    const result=await send(message,{id:"fixture",tab:{id:tabId,title},frameId,documentId,url});
    await new Promise(resolve=>setImmediate(resolve));
    return result;
  }
  // Discovery state comes from the worker global; the popup that exposed it is gone.
  const state=async tabId=>JSON.parse(await vm.runInContext(`(async () => { await sessionReady; return JSON.stringify(publicState(${Number(tabId)})); })()`,context));
  async function menu(menuItemId,info={}){const tab=info.tab||{id:7,url:"https://site.example/watch"};events.menu.listeners[0]({menuItemId,pageUrl:tab.url,...info.info},tab);for(let round=0;round<24;round+=1){await Promise.resolve();await new Promise(resolve=>setImmediate(resolve));}}
  async function waitFor(predicate,timeoutMs=500){const deadline=Date.now()+timeoutMs;while(Date.now()<deadline){if(predicate())return true;await new Promise(resolve=>setImmediate(resolve));}return predicate();}
  return {events,storage,menus,context,content,state,menu,waitFor,nativeRequests};
}

function playerMessage(players,sources=[]) {
  return {channel:"ssdownload-content",event:"scan",documentId:"ignored-page-command",pageUrl:"https://site.example/watch",pageTitle:"Film",players,sources};
}

test("same-document navigation hands off the current video instead of the sender's initial page",async()=>{
  const h=backgroundHarness();
  const initial="https://site.example/library";
  const players=[{id:"player",urls:["blob:https://site.example/reused-player"],active:true}];
  const message={...playerMessage(players),event:"download-video",mediaKind:"video",playerId:"player"};
  const first="https://site.example/watch?v=first",second="https://site.example/watch?v=second";
  await h.content({...message,pageUrl:first},{url:initial,documentId:"same-document"});
  await h.content({...message,pageUrl:second},{url:initial,documentId:"same-document"});
  const handoffs=h.nativeRequests.filter(request=>request.action?.type==="browser_media").map(request=>request.action);
  assert.deepEqual(handoffs.map(action=>action.request.url),[first,second],"the desktop must analyze each selected video, never the library or previous video");
  assert.deepEqual(handoffs.map(action=>action.request.page_url),[first,second]);
  assert.notEqual(handoffs[0].launch_id,handoffs[1].launch_id,"changing the video must not reuse the previous native analysis");
  assert.ok(handoffs.every(action=>action.request.referer===null),"navigation must not fabricate an HTTP Referer");
});

test("a claimed page address cannot move a player handoff outside the sender origin",async()=>{
  const h=backgroundHarness();
  const trusted="https://site.example/watch";
  await h.content({...playerMessage([{id:"player",urls:["blob:https://site.example/player"],active:true}]),
    event:"download-video",mediaKind:"video",playerId:"player",pageUrl:"https://other.example/watch"},{url:trusted});
  const handoff=h.nativeRequests.find(request=>request.action?.type==="browser_media");
  assert.equal(handoff.action.request.url,trusted);
  assert.equal(handoff.action.request.page_url,trusted);
});

function before(headers,url,requestId,extra={}) {
  return {tabId:7,frameId:0,documentId:"doc",documentUrl:"https://site.example/watch",type:"xmlhttprequest",url,requestId,
    requestHeaders:headers.map(([name,value])=>({name,value})),...extra};
}

function response(url,requestId,statusCode=200,type="application/octet-stream") {
  return {tabId:7,frameId:0,documentId:"doc",documentUrl:"https://site.example/watch",type:"xmlhttprequest",url,requestId,statusCode,
    responseHeaders:[{name:"Content-Type",value:type}]};
}

function consentHarness({cookieList=[],partition={topLevelSite:"https://site.example"},scopes=[{site:"https://site.example",storeId:"chrome-main"}],cookieCalls=[]}={}) {
  const cookies={
    async getAllCookieStores(){return [{id:"chrome-main",tabIds:[7]},{id:"chrome-other",tabIds:[8]}];},
    async getAll(details){cookieCalls.push(details);return cookieList;},
  };
  if(partition!==false)cookies.getPartitionKey=async()=>({partitionKey:partition});
  return backgroundHarness({local:{sessionScopes:scopes},cookies,cookieCalls});
}

test("session cookies stay inside the consented store and matching targets",async()=>{
  const cookieCalls=[];
  const h=consentHarness({cookieCalls,cookieList:[
    {name:"media",value:"fixture",domain:"media.example",path:"/video",secure:true,hostOnly:true,storeId:"chrome-main",partitionKey:{topLevelSite:"https://site.example"}},
    {name:"redirect",value:"fixture",domain:"redirect.example",path:"/gate",secure:true,hostOnly:true,storeId:"chrome-main",partitionKey:{topLevelSite:"https://site.example"}},
    {name:"audio",value:"fixture",domain:"audio.example",path:"/tracks/tr",secure:true,hostOnly:true,storeId:"chrome-main",partitionKey:{topLevelSite:"https://site.example"}},
    {name:"fragment",value:"fixture",domain:"segments.example",path:"/movie/",secure:true,hostOnly:true,storeId:"chrome-main",partitionKey:{topLevelSite:"https://site.example"}},
    {name:"wrong-store",value:"fixture",domain:"media.example",path:"/video",secure:true,hostOnly:true,storeId:"chrome-other",partitionKey:{topLevelSite:"https://site.example"}},
    {name:"wrong-path",value:"fixture",domain:"media.example",path:"/private",secure:true,hostOnly:true,storeId:"chrome-main",partitionKey:{topLevelSite:"https://site.example"}},
    {name:"wrong-host",value:"fixture",domain:"other.example",path:"/",secure:true,hostOnly:true,storeId:"chrome-main",partitionKey:{topLevelSite:"https://site.example"}},
    {name:"wrong-partition",value:"fixture",domain:"media.example",path:"/video",secure:true,hostOnly:true,storeId:"chrome-main",partitionKey:{topLevelSite:"https://other.example"}}
  ]});
  const session=JSON.parse(await vm.runInContext(`(async()=>JSON.stringify(await sessionMetadata(7, {
    url:"https://media.example/video/master.m3u8", pageUrl:"https://site.example/watch", frameId:0,
    requestContext:{url:"https://media.example/video/master.m3u8",headerObservation:"observed",redirects:[{url:"https://redirect.example/gate/start"},{url:"http://insecure.example/open"}]},
    requestChain:[{url:"https://audio.example/tracks/tr/playlist.m3u8",role:"audio"},{url:"https://segments.example/movie/segment-001.m4s",role:"segment"}]
  })))()`,h.context));
  assert.deepEqual(session.session_cookies.map(cookie=>cookie.name).sort(),["audio","fragment","media","redirect"]);
  assert.ok(session.session_cookies.every(cookie=>cookie.store_id==="chrome-main"&&cookie.partition_key==="https://site.example"));
  assert.ok(cookieCalls.some(call=>call.url==="https://redirect.example/gate/start"&&call.storeId==="chrome-main"));
  assert.ok(cookieCalls.every(call=>call.partitionKey===undefined||call.partitionKey.topLevelSite==="https://site.example"));
});

test("consent recorded for another cookie store is refused explicitly",async()=>{
  const cookieCalls=[];
  const h=consentHarness({cookieCalls,scopes:[{site:"https://site.example",storeId:"chrome-other"}],cookieList:[
    {name:"media",value:"fixture",domain:"media.example",path:"/video",secure:true,hostOnly:true,storeId:"chrome-main",partitionKey:{topLevelSite:"https://site.example"}}
  ]});
  await assert.rejects(vm.runInContext("sessionMetadata(7,{url:\"https://media.example/video/master.m3u8\",pageUrl:\"https://site.example/watch\",frameId:0})",h.context),/çerez deposu için verilmiş değil/i);
  assert.deepEqual(cookieCalls,[],"a granted site must fail loudly before any cookie is read");
});

test("a failing permission query is surfaced instead of silently dropping a recorded consent",async()=>{
  const h=backgroundHarness({local:{sessionScopes:[{site:"https://site.example",storeId:"chrome-main"}]},
    permissions:{async contains(){throw new Error("permission store unavailable");}}});
  await assert.rejects(vm.runInContext("sessionMetadata(7,{url:\"https://media.example/video/master.m3u8\",pageUrl:\"https://site.example/watch\",frameId:0})",h.context),
    error=>error.code==="SSD-EXT-001");
});

test("a site without consent never reaches the cookie API and never gets an invented Referer",async()=>{
  const cookieCalls=[];
  const h=consentHarness({cookieCalls,scopes:[]});
  const session=JSON.parse(await vm.runInContext("(async()=>JSON.stringify(await sessionMetadata(7,{url:\"https://media.example/video/master.m3u8\",pageUrl:\"https://site.example/watch\",frameId:0,requestContext:{url:\"https://media.example/video/master.m3u8\",headerObservation:\"unavailable\"}})))()",h.context));
  assert.equal(session.session_cookies,undefined);
  assert.equal(session.referer,null,"an unobserved Referer is not invented from the page address");
  assert.deepEqual(cookieCalls,[],"consent is checked before any cookie store is resolved");
});

test("unsupported Chrome partition contexts never look like an empty cookie export",async()=>{
  for(const partition of [false,{topLevelSite:"https://site.example",hasCrossSiteAncestor:true},{topLevelSite:"https://site.example/path"}]) {
    const cookieCalls=[];
    const h=consentHarness({partition,cookieCalls});
    await assert.rejects(vm.runInContext("sessionMetadata(7,{url:\"https://media.example/video/master.m3u8\",pageUrl:\"https://site.example/watch\",frameId:0})",h.context),/bölümlenmiş oturum kapsamı|güvenle aktarılamıyor/i);
    assert.deepEqual(cookieCalls,[],"an unsupported partition must not look like an empty cookie export");
  }
});

test("final redirect context distinguishes observed header absence from unavailable capture",async()=>{
  const h=backgroundHarness();
  await h.content(playerMessage([{id:"p",title:"Film",urls:["https://media.example/final.mp4"],active:true}],[]));
  const capture=h.events.before.listeners[0];
  capture(before([["Accept","video/*"],["Referer","https://site.example/watch"],["Cookie","secret"],["Authorization","secret"]],"https://origin.example/start.mp4","request-1"));
  h.events.redirect.listeners[0]({...response("https://origin.example/start.mp4","request-1",302,"video/mp4"),redirectUrl:"https://media.example/final.mp4"});
  capture(before([["Accept","video/mp4"]],"https://media.example/final.mp4","request-1"));
  await h.events.headers.listeners[0](response("https://media.example/final.mp4","request-1",200,"video/mp4"));
  const item=(await h.state(7)).items.find(value=>value.url==="https://media.example/final.mp4");
  assert.equal(item.requestContext.version,1);
  assert.equal(item.requestContext.url,"https://media.example/final.mp4");
  assert.equal(item.requestContext.headerObservation,"observed");
  assert.deepEqual(JSON.parse(JSON.stringify(item.requestContext.redirects)),[{url:"https://origin.example/start.mp4",status:302}]);
  assert.deepEqual(JSON.parse(JSON.stringify(item.requestHeaders)),{accept:"video/mp4"});
  assert.equal(item.requestHeaders.referer,undefined,"an observed absent Referer stays absent");
  assert.equal(item.requestHeaders.cookie,undefined);
  assert.equal(item.requestHeaders.authorization,undefined);
  assert.equal(typeof item.contextRevision,"string");
  assert.equal(item.contextFresh,true);

  await h.events.headers.listeners[0](response("https://media.example/no-before.mp4","request-2",200,"video/mp4"));
  const unavailable=(await h.state(7)).items.find(value=>value.url.endsWith("no-before.mp4"));
  assert.equal(unavailable.requestContext.headerObservation,"unavailable");
  assert.deepEqual(JSON.parse(JSON.stringify(unavailable.requestHeaders)),{});
});

test("explicit HLS ancestry retains stage contexts while init and segments stay hidden",async()=>{
  const h=backgroundHarness();
  await h.content(playerMessage([{id:"blob",title:"Film",urls:["blob:https://site.example/player"],active:true}],[]));
  const urls={root:"https://cdn.example/master.m3u8",audio:"https://audio.example/tr/main.m3u8",init:"https://cdn.example/init.bin",segment:"https://fragments.example/0001.mp4"};
  for(const [role,url] of Object.entries(urls)) {
    h.events.before.listeners[0](before([["Accept",role==="root"?"application/vnd.apple.mpegurl":"*/*"]],url,role));
    await h.events.headers.listeners[0](response(url,role,role==="audio"?403:role==="segment"?404:200,role==="root"?"application/vnd.apple.mpegurl":"application/octet-stream"));
  }
  await h.content({channel:"ssdownload-content",event:"manifest",manifest:{url:urls.root,kind:"hls",relations:[
    {url:urls.audio,role:"audio"},{url:urls.init,role:"init"},{url:urls.segment,role:"segment"}]},sources:[]});
  const state=await h.state(7);
  assert.ok(!state.items.some(item=>item.url===urls.init || item.url===urls.segment));
  const video=state.videos.find(item=>item.url===urls.root);
  assert.ok(video);
  const stages=Object.fromEntries(video.requestChain.map(value=>[value.role,value]));
  assert.equal(stages.root.responseStatus,200);
  assert.equal(stages.audio.responseStatus,403);
  assert.equal(stages.init.responseStatus,200);
  assert.equal(stages.segment.responseStatus,404);
});

test("page-world manifest commands cannot invent an unobserved source",async()=>{
  const h=backgroundHarness();
  await h.content({channel:"ssdownload-content",event:"manifest",manifest:{url:"https://attacker.example/fake.m3u8",relations:[
    {url:"https://attacker.example/fake-segment.mp4",role:"segment"}]},sources:[{url:"https://attacker.example/fake.m3u8",contentType:"application/vnd.apple.mpegurl"}]});
  const state=await h.state(7);
  assert.equal(state.items.length,0);
  assert.equal(vm.runInContext("manifestLinks.size",h.context),0);
});

test("expired worker-session headers are restored only as stale unavailable provenance",async()=>{
  const old=Date.now()-120001;
  const item={url:"https://cdn.example/old.m3u8",kind:"hls",label:"HLS",contentType:"application/vnd.apple.mpegurl",source:"network",
    networkStatus:200,pageUrl:"https://site.example/watch",frameId:0,documentId:"doc",requestHeaders:{referer:"https://site.example/watch"},
    requestContext:{version:1,requestId:"old",url:"https://cdn.example/old.m3u8",frameId:0,documentId:"doc",documentUrl:"https://site.example/watch",
      observedAt:old,role:"root",redirects:[],headerObservation:"observed"},downloadable:true,firstSeen:old,lastSeen:old,generation:"g"};
  const discovery={media:[[7,[["legacy",item]]]],status:[],plays:[],frames:[],links:[],contexts:[]};
  const h=backgroundHarness({session:{discovery}});
  const restored=(await h.state(7)).items[0];
  assert.equal(restored.contextExpired,true);
  assert.equal(restored.contextFresh,false);
  assert.equal(restored.requestContext.headerObservation,"unavailable");
  assert.deepEqual(JSON.parse(JSON.stringify(restored.requestHeaders)),{});
  assert.ok(restored.contextAgeMs>=120001);
});

test("same-title players with different track children are not merged",async()=>{
  const h=backgroundHarness();
  const url="https://cdn.example/shared.mp4";
  await h.content(playerMessage([
    {id:"one",title:"Film",urls:[url],active:true,tracks:[{url:"https://cdn.example/one.vtt",language:"tr",label:"One",kind:"subtitles",playerId:"one"}]},
    {id:"two",title:"Film",urls:[url],active:true,tracks:[{url:"https://cdn.example/two.vtt",language:"en",label:"Two",kind:"subtitles",playerId:"two"}]}
  ],[]));
  await h.events.headers.listeners[0](response(url,"shared",200,"video/mp4"));
  const state=await h.state(7);
  assert.equal(state.videos.length,2);
  assert.notEqual(state.videos[0].playerId,state.videos[1].playerId);
  assert.notEqual(state.videos[0].tracks[0].url,state.videos[1].tracks[0].url);
});

test("suspected ads and ambiguous roots are explicit selectable candidates, never normal videos",async()=>{
  const h=backgroundHarness();
  await h.content(playerMessage([{id:"ad",title:"Trailer",urls:["https://cdn.example/trailer.mp4"],active:true,
    adSignals:["player-identity","player-identity","skip-control"]}],[]));
  await h.events.headers.listeners[0](response("https://cdn.example/trailer.mp4","ad-request",200,"video/mp4"));
  let state=await h.state(7);
  assert.equal(state.videos.some(item=>item.playerId==="ad"),false);
  const ad=state.candidates.find(item=>item.playerId==="ad");
  assert.equal(ad.suspectedAd,true);
  assert.equal(ad.selectable,true);
  assert.deepEqual(JSON.parse(JSON.stringify(ad.adSignals)),["player-identity","skip-control"]);

  const other=backgroundHarness();
  await other.content(playerMessage([{id:"blob",title:"Film",urls:["blob:https://site.example/main"],active:true}],[]));
  for(const id of ["one","two"]) await other.events.headers.listeners[0](response(`https://cdn.example/${id}.m3u8`,id,200,"application/vnd.apple.mpegurl"));
  state=await other.state(7);
  assert.equal(state.videos.length,0);
  assert.equal(state.candidates.length,2);
  assert.ok(state.candidates.every(item=>item.ambiguous && item.requiresConfirmation && item.playerId==="blob"));
});

test("content discovery keeps only actual track children of their player",async()=>{
  const sent=[];
  const track={src:"/captions/tr.vtt",srclang:"tr",label:"Türkçe",kind:"subtitles",default:true,getAttribute(name){return this[name]||null;},hasAttribute(name){return name==="default";}};
  const unrelated={src:"/captions/wrong.vtt",srclang:"en",label:"Wrong",kind:"captions",default:false,getAttribute(name){return this[name]||null;},hasAttribute(){return false;}};
  const media={tagName:"VIDEO",currentSrc:"/movie.mp4",src:"/movie.mp4",srcObject:null,paused:false,currentTime:1,readyState:3,duration:90,
    getAttribute(name){return name==="title"?"Actual film":null;},querySelectorAll(selector){if(selector==="track[src]")return [track];if(selector==="source[src]")return [];return [];},
    getBoundingClientRect(){return {width:640,height:360,left:0,top:0,bottom:360};},checkVisibility(){return true;}};
  const document={baseURI:"https://site.example/watch",documentElement:{},title:"Page",querySelector(){return null;},
    querySelectorAll(selector){if(selector==="video, audio")return [media];if(selector==="track[src]")return [track,unrelated];return [];},addEventListener(){}};
  class Observer{constructor(callback){this.callback=callback;}observe(){}}
  const runtimeEvent=event();
  const context=vm.createContext({browser:{runtime:{sendMessage(message){sent.push(message);return Promise.resolve(message.event==="usage-modes"?{status:"ready",usageModes:{video:true,file:true,audio:true}}:{});},onMessage:runtimeEvent}},
    URL,document,location:{href:"https://site.example/watch",origin:"https://site.example"},MutationObserver:Observer,PerformanceObserver:Observer,
    setTimeout,clearTimeout,setInterval(){return 1;},JSON,String,Number,Map,WeakMap,WeakSet,Array,Object,Date,Math});
  vm.runInContext(source("content.js"),context,{filename:"content.js"});
  const players=JSON.parse(vm.runInContext("JSON.stringify(players())",context));
  const sources=JSON.parse(vm.runInContext("JSON.stringify(collectSources())",context));
  assert.deepEqual(players[0].tracks,[{url:"https://site.example/captions/tr.vtt",language:"tr",label:"Türkçe",kind:"subtitles",isDefault:true,playerId:players[0].id}]);
  assert.deepEqual(sources.find(item=>item.url.endsWith("movie.mp4")).tracks,players[0].tracks);
  assert.ok(!JSON.stringify(players).includes("wrong.vtt"));
});

test("page observer reports bounded HLS relations and its worker/MSE observation boundary",async()=>{
  const posted=[];
  let control;
  const window={postMessage(value){posted.push(value);},addEventListener(name,listener){if(name==="message")control=listener;}};
  class FixtureResponse {
    constructor(text,url,type){this.value=text;this.url=url;this.headers={get(){return type;}};}
    text(){return Promise.resolve(this.value);}
    arrayBuffer(){return Promise.resolve(new TextEncoder().encode(this.value).buffer);}
  }
  class FixtureXhr {addEventListener(){}send(){}getResponseHeader(){return "";}}
  const node=(localName,attributes={},children=[],textContent="")=>({localName,children,textContent,getAttribute(name){return attributes[name]||null;}});
  class FixtureDomParser {parseFromString(){
    const representation=node("Representation",{},[
      node("BaseURL",{},[],"audio/main.m4a"),
      node("SegmentList",{},[node("Initialization",{sourceURL:"../init.mp4"}),node("SegmentURL",{media:"segment-1.m4s"})])
    ]);
    const root=node("MPD",{},[node("BaseURL",{},[],"media/"),node("Period",{},[node("AdaptationSet",{contentType:"audio"},[representation])])]);
    return {documentElement:root,querySelector(){return null;}};
  }}
  const context=vm.createContext({window,document:{baseURI:"https://site.example/watch",documentElement:{getAttribute(name){return name==="data-ssdownload-observer-token"?"fixture-token":null;}}},URL,Response:FixtureResponse,XMLHttpRequest:FixtureXhr,DOMParser:FixtureDomParser,
    TextDecoder,TextEncoder,Reflect,Set,Map,JSON,String,Array,Object});
  vm.runInContext(source("page-observer.js"),context,{filename:"page-observer.js"});
  const ready=posted.find(message=>message.channel==="ssdownload-observer-ready");
  assert.deepEqual(JSON.parse(JSON.stringify(ready.capabilities)),{pageFetchConsumed:true,pageXhrConsumed:true,performanceResourceUrl:true,
    workerResponseBody:false,mseAppendBufferAncestry:false,responseCloning:false,drmBypass:false});
  control({source:window,data:{channel:"ssdownload-observer-control",enabled:true,token:"fixture-token"}});
  const body='#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,URI="audio/main.m3u8"\n#EXT-X-MAP:URI="init.mp4"\nsegment-1.m4s\n';
  await vm.runInContext(`new Response(${JSON.stringify(body)},"https://cdn.example/path/media.m3u8","application/vnd.apple.mpegurl").text()`,context);
  const manifest=posted.find(message=>message.channel==="ssdownload-observed-manifest");
  assert.deepEqual(JSON.parse(JSON.stringify(manifest.relations)),[
    {url:"https://cdn.example/path/audio/main.m3u8",role:"audio"},
    {url:"https://cdn.example/path/init.mp4",role:"init"},
    {url:"https://cdn.example/path/segment-1.m4s",role:"segment"}
  ]);
  await vm.runInContext('new Response("<MPD></MPD>","https://cdn.example/path/master.mpd","application/dash+xml").text()',context);
  const dash=posted.find(message=>message.channel==="ssdownload-observed-manifest" && message.kind==="dash");
  assert.deepEqual(JSON.parse(JSON.stringify(dash.relations)),[
    {url:"https://cdn.example/path/media/",role:"variant"},
    {url:"https://cdn.example/path/media/audio/main.m4a",role:"audio"},
    {url:"https://cdn.example/path/media/init.mp4",role:"init"},
    {url:"https://cdn.example/path/media/audio/segment-1.m4s",role:"segment"}
  ]);
  assert.equal(typeof FixtureResponse.prototype.clone,"undefined","observer does not add response cloning");
});
