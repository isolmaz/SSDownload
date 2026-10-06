"use strict";
const assert=require("node:assert/strict");
const {createHash}=require("node:crypto");
const fs=require("node:fs");
const path=require("node:path");
const test=require("node:test");
const vm=require("node:vm");
const context=vm.createContext({Uint8Array,Uint32Array,atob,btoa});
vm.runInContext(fs.readFileSync(path.join(__dirname,"../chromium/relay-sha256.js"),"utf8"),context);
const Sha256=context.SSDownloadRelaySha256;
const expected=bytes=>createHash("sha256").update(bytes).digest("hex");

test("relay SHA256 survives checkpoint restoration across padding and block boundaries",()=>{
  const input=Buffer.alloc(65537);
  for(let index=0;index<input.length;index++)input[index]=(index*73+19)&255;
  for(const length of [0,55,56,63,64,65,65537]){
    const bytes=input.subarray(0,length);let hash=new Sha256(),offset=0;
    for(const size of [1,54,1,8,65000,473]){
      const end=Math.min(length,offset+size);hash.update(bytes.subarray(offset,end));offset=end;
      assert.equal(hash.digestHex(),expected(bytes.subarray(0,offset)));
      hash=new Sha256(JSON.parse(JSON.stringify(hash.exportState())));
    }
    assert.equal(hash.digestHex(),expected(bytes));
    assert.equal(Sha256.digestHex(bytes),expected(bytes));
  }
});

test("relay SHA256 refuses checkpoint tails inconsistent with acknowledged byte count",()=>{
  const state=new Sha256().update(new Uint8Array([1,2,3])).exportState();
  assert.throws(()=>new Sha256({...state,bytes:64}));
  assert.throws(()=>new Sha256({...state,bytes:-1}));
  assert.throws(()=>new Sha256({...state,bytes:Number.MAX_SAFE_INTEGER+1}));
  assert.throws(()=>new Sha256({...state,tail:"A".repeat(92)}));
});

for (const command of ["pause", "cancel"]) test(`relay ${command} remains retryable after native rejection`,async()=>{
  const jobId="12345678-1234-4234-8234-123456789abc";
  let listener,onNativeMessage,rejectNext=true,desktopStopped=false;
  const token="a".repeat(64),request={url:"https://media.example/file.bin",source_identity:{frame_id:0,document_id:"document",page_url:"https://site.example/watch"}};
  const browser={runtime:{id:"owned",getURL:value=>`chrome-extension://owned/${value}`,onMessage:{addListener:value=>{listener=value;}},connectNative(){return {onMessage:{addListener:value=>{onNativeMessage=value;}},onDisconnect:{addListener(){}},disconnect(){},postMessage(message){
    queueMicrotask(()=>{
      const action=message.action;
      if(action.type==="capabilities"){onNativeMessage({ok:true,result:{capabilities:{source_refresh_jobs:[{id:jobId}]}}});return;}
      if(action.command.operation==="begin"){onNativeMessage({ok:true,result:{browser_transfer:{job_id:jobId,max_chunk_bytes:524288,streams:[],token,request,sender_generation:1}}});return;}
      if(rejectNext){rejectNext=false;onNativeMessage({ok:false,message:"Native operation rejected"});return;}
      desktopStopped=true;
      onNativeMessage({ok:true,result:{browser_transfer:{job_id:message.action.command.id,max_chunk_bytes:524288,streams:[]}}});
    });
  }};}},storage:{session:{async get(){return {browserRelayJobs:[]};},async set(){}}},tabs:{async get(){return {id:7,url:"https://site.example/watch"};},async sendMessage(){return {ok:true};},onRemoved:{addListener(){}},onUpdated:{addListener(){}}},webNavigation:{async getAllFrames(){return [{frameId:0,documentId:"document",url:"https://site.example/watch"}];}}};
  vm.runInNewContext(fs.readFileSync(path.join(__dirname,"../chromium/relay-background.js"),"utf8"),{browser,URL,TextEncoder,crypto:require("node:crypto").webcrypto,queueMicrotask});
  const control=command=>new Promise(resolve=>listener({channel:"ssdownload-relay-control",command,jobId},{id:"owned",url:"chrome-extension://owned/popup.html"},resolve));
  const begun=await new Promise(resolve=>listener({channel:"ssdownload-relay-control",command:"start",jobId,tabId:7,frameId:0,documentId:"document",restart:false},{id:"owned",url:"chrome-extension://owned/popup.html"},resolve));
  assert.equal(begun.state,"running");
  const rejected=await control(command);
  assert.equal(typeof rejected.error,"string");
  assert.equal(rejected.state,undefined);
  assert.equal(desktopStopped,false);
  assert.equal((await control("status")).state,"running");
  assert.equal((await control(command)).state,command==="pause"?"paused":"cancelled");
  assert.equal(desktopStopped,true);
  assert.equal((await control("status")).state,command==="pause"?"paused":"inactive");
});

test("policy-stopped relay resumes automatically on the same source document",async()=>{
  const jobId="12345678-1234-4234-8234-123456789abc";
  const originalToken="a".repeat(64), resumedToken="c".repeat(64);
  const request={url:"https://media.example/file.bin",source_identity:{frame_id:0,document_id:"document",page_url:"https://site.example/watch"}};
  let listener,onNativeMessage,resumeTimer,started,initialStart,beginCount=0;
  const startedPromise=new Promise(resolve=>{started=resolve;});
  const browser={
    runtime:{id:"owned",getURL:value=>`chrome-extension://owned/${value}`,onMessage:{addListener:value=>{listener=value;}},connectNative(){return {onMessage:{addListener:value=>{onNativeMessage=value;}},onDisconnect:{addListener(){}},disconnect(){},postMessage(message){
      queueMicrotask(()=>{
        const action=message.action;
        if(action.type==="capabilities"){onNativeMessage({ok:true,result:{capabilities:{source_refresh_jobs:[{id:jobId}]}}});return;}
        if(action.command.operation==="begin")beginCount++;
        onNativeMessage({ok:true,result:{browser_transfer:{job_id:jobId,max_chunk_bytes:524288,streams:[],token:beginCount===1?originalToken:resumedToken,request,sender_generation:beginCount}}});
      });
    }};}},
    storage:{session:{async get(){return {browserRelayJobs:[]};},async set(){}}},
    tabs:{async get(){return {id:7,url:"https://site.example/watch"};},async sendMessage(_tabId,message){if(message.command==="start"){if(!initialStart)initialStart=message;else started(message);}return {ok:true};},onRemoved:{addListener(){}},onUpdated:{addListener(){}}},
    webNavigation:{async getAllFrames(){return [{frameId:0,documentId:"document",url:"https://site.example/watch"}];}},
    alarms:{create(){},onAlarm:{addListener(){}}}
  };
  vm.runInNewContext(fs.readFileSync(path.join(__dirname,"../chromium/relay-background.js"),"utf8"),{browser,URL,TextEncoder,crypto:require("node:crypto").webcrypto,queueMicrotask,setTimeout(callback){resumeTimer=callback;return 1;}});
  const send=(message,sender)=>new Promise(resolve=>listener(message,sender,resolve));
  const begun=await send({channel:"ssdownload-relay-control",command:"start",jobId,tabId:7,frameId:0,documentId:"document",restart:false},{id:"owned",url:"chrome-extension://owned/popup.html"});
  assert.equal(begun.state,"running");
  const stopped=await send({channel:"ssdownload-relay-internal",command:"failed",jobId,token:originalToken,runToken:initialStart.runToken,documentId:"document",error:"Tarayıcı aktarımı kuyruk kotası, zaman veya eşzamanlılık sınırında durduruldu."},{id:"owned",tab:{id:7},frameId:0,documentId:"document"});
  assert.equal(stopped.automatic_resume,true);
  const paused=await send({channel:"ssdownload-relay-control",command:"status",jobId},{id:"owned",url:"chrome-extension://owned/popup.html"});
  assert.equal(paused.state,"paused");assert.equal(paused.automaticResume,true);
  resumeTimer();
  const startMessage=await startedPromise;
  assert.equal(startMessage.token,resumedToken);
  const running=await send({channel:"ssdownload-relay-control",command:"status",jobId},{id:"owned",url:"chrome-extension://owned/popup.html"});
  assert.equal(running.state,"running");assert.equal(running.automaticResume,false);
});

test("native host disconnect pauses each relay through a replacement host",async()=>{
  const jobId="12345678-1234-4234-8234-123456789abc";
  const token="a".repeat(64), request={url:"https://media.example/file.bin",source_identity:{frame_id:0,document_id:"document",page_url:"https://site.example/watch"}};
  let listener,firstPort,pauseCount=0,aborted=false,saved=[];
  const ports=[];
  function port(){
    const messages=[],disconnects=[];
    return {onMessage:{addListener(value){messages.push(value);}},onDisconnect:{addListener(value){disconnects.push(value);}},disconnect(){},close(){for(const value of disconnects)value();},postMessage(message){queueMicrotask(()=>{
      const action=message.action;
      const response=action.type==="capabilities"?{ok:true,result:{capabilities:{source_refresh_jobs:[{id:jobId}]}}}:action.command.operation==="begin"?{ok:true,result:{browser_transfer:{job_id:jobId,max_chunk_bytes:524288,streams:[],token,request,sender_generation:1}}}:{ok:true,result:{browser_transfer:{job_id:jobId,max_chunk_bytes:524288,streams:[]}}};
      if(action.command?.operation==="pause")pauseCount++;
      for(const value of messages)value(response);
    });}};
  }
  const browser={
    runtime:{id:"owned",getURL:value=>`chrome-extension://owned/${value}`,onMessage:{addListener:value=>{listener=value;}},connectNative(){const value=port();ports.push(value);return value;}},
    storage:{session:{async get(){return {browserRelayJobs:[]};},async set(value){saved=value.browserRelayJobs;}}},
    tabs:{async get(){return {id:7,url:"https://site.example/watch"};},async sendMessage(_tabId,message){if(message.command==="abort")aborted=true;return {ok:true};},onRemoved:{addListener(){}},onUpdated:{addListener(){}}},
    webNavigation:{async getAllFrames(){return [{frameId:0,documentId:"document",url:"https://site.example/watch"}];}}
  };
  vm.runInNewContext(fs.readFileSync(path.join(__dirname,"../chromium/relay-background.js"),"utf8"),{browser,URL,TextEncoder,crypto:require("node:crypto").webcrypto,queueMicrotask});
  const send=message=>new Promise(resolve=>listener(message,{id:"owned",url:"chrome-extension://owned/popup.html"},resolve));
  assert.equal((await send({channel:"ssdownload-relay-control",command:"start",tabId:7,frameId:0,documentId:"document",jobId,restart:false})).state,"running");
  firstPort=ports[0];firstPort.close();
  await new Promise(resolve=>setImmediate(resolve));
  const status=await send({channel:"ssdownload-relay-control",command:"status",jobId});
  assert.equal(status.state,"paused");assert.equal(status.automaticResume,false);
  assert.equal(pauseCount,1);assert.equal(aborted,true);assert.equal(saved[0].paused,true);
});

test("relay controls accept own extension tabs but reject web content and other extensions",async()=>{
  let listener;
  const browser={runtime:{id:"owned",getURL:value=>`chrome-extension://owned/${value}`,onMessage:{addListener:value=>{listener=value;}}},storage:{session:{async get(){return {browserRelayJobs:[]};}}},tabs:{onRemoved:{addListener(){}},onUpdated:{addListener(){}}}};
  vm.runInNewContext(fs.readFileSync(path.join(__dirname,"../chromium/relay-background.js"),"utf8"),{browser,URL,TextEncoder});
  const status=sender=>new Promise(resolve=>listener({channel:"ssdownload-relay-control",command:"status",jobId:"none"},sender,resolve));
  const permitted=await status({id:"owned",url:"chrome-extension://owned/popup.html",tab:{id:7}});
  assert.equal(permitted.state,"inactive");
  for(const sender of [{id:"owned",url:"https://site.example/",tab:{id:7}},{id:"other",url:"chrome-extension://other/popup.html"},{id:"owned"}]){
    const denied=await status(sender);assert.equal(denied.state,undefined);assert.equal(typeof denied.error,"string");
  }
});
