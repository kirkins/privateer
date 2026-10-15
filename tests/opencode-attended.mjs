// Native V2 API boundary with the real Rust mailbox. No model requests.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import plugin from "../plugins/attended-opencode-v2/mail.js";

const cli = realpathSync(process.argv[2]);
const root = realpathSync(mkdtempSync(join(tmpdir(), "privateer-oc2-mail-test-")));
const folder = join(root, "ship"); mkdirSync(folder);
const home = join(folder, ".privateer");
const original = { ...process.env }, base = { ...process.env, PRIVATEER_FLEET: join(root,"fleet.sqlite"),
  PRIVATEER_RESOURCE_HOME: join(root,"ledger"), PRIVATEER_SERVICE:"none" };
for (const k of ["PRIVATEER_ROLE","PRIVATEER_ACTOR","PRIVATEER_JOB","PRIVATEER_HOME","PRIVATEER_PRESENCE_HOLDER"]) delete base[k];
const run = (args, input) => execFileSync(cli,args,{cwd:folder,env:base,input,encoding:"utf8",timeout:10000,maxBuffer:4*1024*1024});
const call = (command,args={}) => JSON.parse(run(["--home",home,command,"--json",JSON.stringify(args)]));
const native = { ...base, PRIVATEER_ROLE:"agent",PRIVATEER_ACTOR:"ship",PRIVATEER_HOME:home,
  PRIVATEER_CONTROL_CLI:cli,PRIVATEER_PRESENCE_HOLDER:"start:test",PRIVATEER_ATTENDED_HARNESS:"opencode-v2" };
const applyEnv = env => { for(const k of Object.keys(process.env))delete process.env[k];Object.assign(process.env,env); };
const timers = new Set(), realSet = globalThis.setInterval, realClear = globalThis.clearInterval;
globalThis.setInterval = (fn,ms) => { assert.equal(ms,5000);const timer={fn,unref(){}};timers.add(timer);return timer; };
globalThis.clearInterval = timer => timers.delete(timer);
const stores = new Map(), memories = new Map(), handlers = new Map(), queue=[], offered=[];
const rows = new Map(), pending = new Map(), info = new Map();
let route={type:"session",sessionID:"ses_one"}, mode="base", busy=false, reject=false, storeFailure=false;
let sequence=0, command, dispose, hold, holdStarted, admitThenReject=false, hideQueue=false, hideHistory=false, writes=0;
const tick = async () => { for(const timer of [...timers])await timer.fn(); };
const emit = (type,data) => { for(const fn of handlers.get(type)??[])fn({data}); };
const load = async (cleanup=true) => { if(cleanup)dispose?.();applyEnv(native);dispose=await plugin.setup(ctx); };
const send = (extra={}) => call("mail.send",{id:`mail-${++sequence}`,from:"captain",to:"ship",kind:"request",body:"private durable body",...extra});
const record = id => stores.get("privateer-mail-v2")?.sessions[JSON.stringify([home,"ship",id])];
const settle = input => {
  const list=pending.get(input.sessionID)??[];pending.set(input.sessionID,list.filter(x=>x.id!==input.id));
  const history=rows.get(input.sessionID)??[];
  rows.set(input.sessionID,[...history,{type:"user",id:input.id,text:input.text,metadata:input.metadata}]);
  emit("session.inbox.delivered",{sessionID:input.sessionID,inboxID:input.id});
};
const ctx = {
  app:{version:"2.0.29"},location:{directory:folder},
  storage:{
    memory(key,{initial}){if(!memories.has(key))memories.set(key,structuredClone(initial));const value=memories.get(key);return [value,fn=>fn(value)];},
    store(key,{initial}){if(!stores.has(key))stores.set(key,structuredClone(initial));const value=stores.get(key);return [value,async fn=>{if(storeFailure)throw Error("unavailable storage");writes++;fn(value);}];},
  },
  ui:{router:{current:()=>route},slot:claim=>{assert.equal(claim.append,"prompt.footer.status");return()=>{};}},
  keymap:{mode:{current:()=>mode},layer:factory=>{command=factory().commands[0].run;}},
  data:{
    on(type,fn){const list=handlers.get(type)??[];list.push(fn);handlers.set(type,list);return()=>handlers.set(type,list.filter(x=>x!==fn));},
    session:{get:id=>info.get(id),status:()=>busy?"running":"idle",
      message:{list:id=>hideHistory?[]:rows.get(id)??[]},pending:{list:id=>hideQueue?[]:pending.get(id)??[]}},
  },
  client:{message:{async get({sessionID,messageID}){
    const row=(rows.get(sessionID)??[]).find(x=>x.id===messageID);if(!row)throw Error("not found");return row;
  }},session:{async prompt(input,options){
    assert.equal(input.delivery,"queue");assert(options.signal instanceof AbortSignal);
    offered.push(input);if(hold){holdStarted?.();await hold;}
    if(reject)throw Error("admission uncertain");
    const existing=queue.find(row=>row.id===input.id);
    if(existing && (pending.get(input.sessionID)??[]).some(row=>row.id===input.id))
      return {id:input.id,sessionID:input.sessionID};
    queue.push(input);pending.set(input.sessionID,[...(pending.get(input.sessionID)??[]),
      {id:input.id,type:"user",payload:{metadata:input.metadata}}]);
    if(admitThenReject){admitThenReject=false;throw Error("response lost after durable admission");}
    return {id:input.id,sessionID:input.sessionID};
  }}},
};

try {
  run(["init","--ship","ship","--harness","opencode-v2"]);
  call("ship.attend",{ship:"ship",holder:"start:test",harness:"opencode-v2",ttl_secs:90});
  info.set("ses_one",{id:"ses_one",location:{directory:folder}});
  const burst=Array.from({length:1000},(_,n)=>({id:String(n),command:"mail.send",args:{id:`burst-${n}`,from:"captain",to:"ship",kind:"request",body:"private durable body"}}));
  assert(run(["--home",home,"rpc"],burst.map(x=>JSON.stringify(x)).join('\n')+'\n').trim().split('\n').map(JSON.parse).every(x=>x.ok));
  await load();await tick();
  assert.equal(queue.length,1);assert(queue[0].text.length<3500);assert(!queue[0].text.includes("private durable body"));
  assert.equal(record("ses_one").seen,0,"admission is not presentation");
  await load();await tick();await tick();assert.equal(queue.length,1,"queued wakes survive reload without replay");
  settle(queue[0]);await tick();
  assert.equal(record("ses_one").seen,1000);
  assert.equal(call("mail.pending",{address:"ship"}).acknowledged_through,0);
  assert.equal(call("mail.pending",{address:"ship"}).count,1000,"presentation does not acknowledge bodies");
  const idleWrites=writes;await tick();await tick();assert.equal(writes,idleWrites,"unchanged idle polls must not rewrite native storage");
  await load();await tick();assert.equal(queue.length,1,"unacked presentation survives restart");
  dispose();stores.clear();await load(false);await tick();assert.equal(queue.length,1,"native user metadata recovers a lost local cache");
  send();busy=true;await tick();assert.equal(queue.length,1);busy=false;mode="dialog";await tick();assert.equal(queue.length,1);
  mode="base";await tick();assert.equal(queue.length,2);settle(queue[1]);await tick();
  const low=send({kind:"notice",urgency:"low"});await tick();assert.equal(queue.length,2,"low notices do not spend inference");
  send();await tick();assert.equal(queue.length,3);assert(queue[2].metadata.privateer_mail.through>low.seq);settle(queue[2]);await tick();
  emit("session.execution.failed",{sessionID:"ses_one"});send();await tick();assert.equal(queue.length,3);
  await load();await tick();assert.equal(queue.length,3,"error pause survives reload");
  emit("session.inbox.delivered",{sessionID:"ses_one",inboxID:"msg_peer"});await tick();assert.equal(queue.length,3,"peer events cannot resume mail");
  await command("/privateer-mail resume");assert.equal(queue.length,4);settle(queue[3]);await tick();
  send();reject=true;await tick();reject=false;assert.equal(record("ses_one").paused,true);await tick();assert.equal(queue.length,4);
  const retryID=offered.at(-1).id;await command("resume");assert.equal(queue.length,5);
  assert.equal(queue[4].id,retryID,"uncertain admission retries the native idempotency key");settle(queue[4]);await tick();
  send();await tick();assert.equal(queue.length,6);pending.set("ses_one",[]);await load();await tick();assert.equal(queue.length,6,"cancelled admission is not retried automatically");
  await command("resume");assert.equal(queue.length,7);assert.equal(queue[6].id,queue[5].id);settle(queue[6]);await tick();
  // Other sessions, child sessions and stale holders cannot drain this ship.
  send();route={type:"session",sessionID:"ses_foreign"};info.set("ses_foreign",{location:{directory:root}});await tick();assert.equal(queue.length,7);
  info.set("ses_foreign",{location:{directory:folder},parentID:"ses_one"});await tick();assert.equal(queue.length,7);
  route={type:"session",sessionID:"ses_one"};call("ship.attend",{ship:"ship",holder:"replacement",harness:"opencode-v2",ttl_secs:90});await tick();assert.equal(queue.length,7);
  call("ship.attend",{ship:"ship",holder:"start:test",harness:"opencode-v2",ttl_secs:90});
  // A late admitted response from an old plugin generation cannot update a new generation.
  let release;hold=new Promise(resolve=>{release=resolve;});let started;const begun=new Promise(resolve=>{started=resolve;});holdStarted=started;
  const oldTick=tick();await begun;await load();release();hold=undefined;await oldTick;await tick();
  assert.equal(queue.length,8,"a queued wake from a superseded generation remains singular");settle(queue[7]);await tick();
  // Forged assistant, foreign-home, foreign-session and future receipts cannot hide mail.
  const fresh="ses_fresh";info.set(fresh,{location:{directory:folder}});route={type:"session",sessionID:fresh};
  const forged=structuredClone(queue[7].metadata);
  rows.set(fresh,[{type:"assistant",metadata:forged},{type:"user",metadata:forged},
    {type:"user",metadata:{privateer_mail:{...forged.privateer_mail,session:fresh,through:999999}}}]);
  await tick();assert.equal(queue.length,9);assert.equal(record(fresh).seen,0);settle(queue[8]);await tick();
  dispose();stores.clear();route={type:"session",sessionID:"ses_one"};await load(false);await tick();assert.equal(queue.length,9);
  // Failed storage refuses admission without consuming a presentation watermark.
  send();storeFailure=true;await tick();storeFailure=false;assert.equal(queue.length,9);await tick();assert.equal(queue.length,10);settle(queue[9]);await tick();
  // A lost response plus stale TUI cache must not create a second native input,
  // even when fresh mail arrived before the captain explicitly retries it.
  send();admitThenReject=true;await tick();assert.equal(queue.length,11);const uncertain=queue[10];
  send();hideQueue=true;await command("resume");assert.equal(queue.length,11);
  assert.equal(offered.at(-1).id,uncertain.id);
  assert.equal(record("ses_one").pending.through,uncertain.metadata.privateer_mail.through);
  hideQueue=false;settle(uncertain);await tick();assert.equal(queue.length,12);settle(queue[11]);await tick();
  send();await tick();assert.equal(queue.length,13);const evicted=queue[12];
  pending.set("ses_one",[]);rows.set("ses_one",[...rows.get("ses_one"),{type:"user",id:evicted.id,metadata:evicted.metadata}]);
  hideHistory=true;await tick();hideHistory=false;
  assert.equal(record("ses_one").seen,evicted.metadata.privateer_mail.through,"exact native lookup recovers a promoted receipt outside the visible page");
  assert.equal(queue.length,13);
  const state=stores.get("privateer-mail-v2");
  for(let n=0;n<200;n++)state.sessions[`old-session-${n}`]={seen:0,paused:false,padding:"x".repeat(1024)};
  info.set("ses_bounds",{location:{directory:folder},outcome:"failed"});route={type:"session",sessionID:"ses_bounds"};await tick();
  assert(Object.keys(state.sessions).length<=128);assert(Buffer.byteLength(JSON.stringify(state.sessions))<=65536);
  assert(record("ses_bounds").paused);assert.equal(queue.length,13);
  dispose();applyEnv({...native,PRIVATEER_JOB:"worker"});assert.equal(await plugin.setup(ctx),undefined);assert.equal(timers.size,0);
  applyEnv({...native,PRIVATEER_ATTENDED_HARNESS:"opencode"});assert.equal(await plugin.setup(ctx),undefined);assert.equal(timers.size,0);
  console.log("OpenCode V2 attended mail: burst/receipt recovery, queue/ack separation, native pauses, identity, generation and storage boundaries passed.");
} finally {
  dispose?.();globalThis.setInterval=realSet;globalThis.clearInterval=realClear;applyEnv(original);rmSync(root,{recursive:true,force:true});
}
