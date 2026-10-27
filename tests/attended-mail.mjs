// Real durable mailbox and CLI; simulated native extension events. No models.
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFileSync } from "node:child_process";
import attended, { AttendedMail } from "../plugins/attended/mail.mjs";

const cli = realpathSync(process.argv[2]);
const root = realpathSync(mkdtempSync(join(tmpdir(), "privateer-attended-")));
const folder = join(root, "infra");
mkdirSync(folder);
const home = join(folder, ".privateer");
const env = { ...process.env, PRIVATEER_FLEET: join(root,"fleet.sqlite"), PRIVATEER_RESOURCE_HOME: join(root,"ledger") };
for (const key of ["PRIVATEER_ROLE","PRIVATEER_ACTOR","PRIVATEER_HOME","PRIVATEER_PRESENCE_HOLDER"]) delete env[key];
const run = (args, input) => execFileSync(cli, args, { cwd: folder, env, input, encoding:"utf8", timeout:30000, maxBuffer:4*1024*1024 });
const call = (command, args) => JSON.parse(run(["--home",home,command,"--json",JSON.stringify(args)]));
const holder = "start:fixture";
const config = { cli,home,ship:"infra",holder,harness:"pi" };
const own = () => call("ship.attend",{ship:"infra",holder,ttl_secs:90});
let sequence = 0;
const send = (overrides={}) => call("mail.send",{id:`mail-${++sequence}`,from:"captain",to:"infra",kind:"request",body:"private body",...overrides});
const receipts = [];
const statuses = [];
const sent = [];
let idle = true;
let rejectQueue = false;
const ctx = {hasUI:true,isIdle:()=>idle,sessionManager:{getBranch:()=>receipts},ui:{setStatus:(_key,text)=>statuses.push(text)}};
const api = { sendMessage(message,options) {
  if(rejectQueue) throw Error("queue failed");
  sent.push({message,options});
  receipts.push({type:"custom_message",...message});
}};
const timers = new Set();
const dependencies = { every(fn) { const timer={fn}; timers.add(timer); return timer; }, cancel(timer) {timers.delete(timer);} };
const settle = async mail => { await mail.flight; };
// Every wake.started the runtime recorded, oldest first.
const started = () => {
  const out = []; let after = 0;
  for (;;) {
    const page = call("audit.list",{after_seq:after,limit:1000}).events;
    if (!page.length) return out;
    out.push(...page.filter(e=>e.kind==="wake.started").map(e=>e.payload)); after = page.at(-1).seq;
  }
};

try {
  run(["--home",home,"init","--ship","infra","--harness","pi"]);
  own();
  call("attention.report",{ship:"infra",holder,kind:"sample",tokens:1000,window:100000});
  // One thousand requests produce one bounded native message; bodies stay in SQLite.
  const requests = Array.from({length:1000},(_,n)=>({id:String(n),command:"mail.send",args:{
    id:`burst-${n}`,from:"captain",to:"infra",kind:"request",body:"private durable body, not a notification payload"}}));
  const results = run(["--home",home,"rpc"],requests.map(r=>JSON.stringify(r)).join("\n")+"\n").trim().split("\n").map(JSON.parse);
  assert.equal(results.length,1000);
  assert(results.every(r=>r.ok));
  const mail = new AttendedMail(api,config,dependencies);
  mail.start(ctx); await settle(mail);
  assert.equal(sent.length,1);
  assert.equal(sent[0].message.details.count,1000);
  assert(sent[0].message.content.length<3000);
  assert(!sent[0].message.content.includes("private durable body"));
  assert(sent[0].message.content.includes("not to other mail in this or later wakes"));
  assert(sent[0].message.content.includes("Attention: normal"), sent[0].message.content);
  assert.deepEqual(sent[0].options,{triggerTurn:true,deliverAs:"followUp"});
  const through = mail.seen;
  assert.equal(call("mail.pending",{address:"infra"}).count,1000);
  assert.equal(call("mail.pending",{address:"infra"}).acknowledged_through,0);
  await mail.poll(); await mail.poll();
  assert.equal(sent.length,1,"unacknowledged mail must not wake repeatedly");
  // Busy native turns show status only. A fresh unassigned arrival stays independent.
  mail.began(); idle=false;
  const fresh = send();
  await mail.poll();
  assert.equal(sent.length,1);
  assert(statuses.at(-1).includes("1 new mail"));
  assert.equal(mail.seen,through);
  mail.settled({aborted:false}); idle=true;
  await mail.poll();
  assert.equal(sent.length,2);
  assert.equal(mail.seen,fresh.seq);
  mail.settled({});
  // Resume consumes actual native custom-message receipts, never a separate watermark.
  receipts.push({type:"custom_message",customType:"privateer-mail",details:{home:"/another/home",ship:"infra",throughSeq:1_000_000}});
  receipts.push({type:"custom_message",customType:"privateer-mail",details:{home,ship:"another-ship",throughSeq:1_000_000}});
  mail.stop(); mail.start(ctx); await settle(mail);
  assert.equal(sent.length,2);
  assert.equal(mail.seen,fresh.seq);
  assert.equal(timers.size,1);
  // Low notices wait for a natural prompt, without an autonomous model turn.
  const low = send({kind:"notice",urgency:"low"});
  await mail.poll();
  assert.equal(sent.length,2);
  const context = await mail.poll("context");
  assert.equal(context.message.details.throughSeq,low.seq);
  assert.equal(sent.length,2);
  receipts.push({type:"custom_message",...context.message});
  // A failed native queue does not mark mail as delivered, and can recover.
  const retry = send(); rejectQueue=true;
  await mail.poll();
  assert.equal(mail.seen,low.seq);
  assert.equal(mail.pending,false);
  assert(statuses.at(-1).includes("unavailable"));
  rejectQueue=false; await mail.poll();
  assert.equal(mail.seen,retry.seq);
  assert.equal(sent.length,3);
  // Native errors/aborts pause unattended wakes; human context can recover.
  mail.began(); mail.messageEnded({message:{role:"assistant",stopReason:"error"}}); mail.settled({});
  assert.equal(mail.paused,true);
  const afterError=send(); await mail.poll();
  assert.equal(sent.length,3);
  mail.paused=false;
  const recovery=await mail.poll("context");
  assert.equal(recovery.message.details.throughSeq,afterError.seq);
  receipts.push({type:"custom_message",...recovery.message});
  mail.began(); mail.settled({aborted:true});
  assert.equal(mail.paused,true);
  // OMP advisory/continuation events are not a settled boundary.
  mail.paused=false; mail.began();
  mail.ompEnded({isTerminal:false}); assert.equal(mail.busy,true);
  mail.ompEnded({willContinue:true}); assert.equal(mail.busy,true);
  mail.ompEnded({yielded:false}); assert.equal(mail.busy,true);
  mail.ompEnded({isTerminal:true,messages:[{role:"assistant",stopReason:"error"}]});
  assert.equal(mail.busy,false); assert.equal(mail.paused,true);
  // Replaced ownership refuses even new mail. No acknowledgement is fabricated.
  send(); mail.paused=false;
  call("ship.attend",{ship:"infra",holder:"replacement",ttl_secs:90});
  await mail.poll();
  assert.equal(sent.length,3);
  assert(statuses.at(-1).includes("another session"));
  mail.dispose();
  assert.equal(timers.size,0);
  await mail.poll(); assert.equal(sent.length,3);
  // An in-flight probe completed after shutdown cannot queue a turn.
  let release;
  const delayed = new AttendedMail(api,config,{...dependencies,call:()=>new Promise(resolve=>{release=resolve;})});
  delayed.start(ctx); const flight=delayed.flight; delayed.stop();
  release({attended:true,lease:{holder}}); await flight;
  assert.equal(sent.length,3); assert.equal(timers.size,0);
  delayed.dispose();
  const headless = new AttendedMail(api,config,dependencies);
  headless.start({...ctx,hasUI:false});
  assert.equal(timers.size,0); assert.equal(headless.flight,undefined); headless.dispose();
  // A hosted first mate has no captain to resume it, so it retries after a minute.
  let clock = 0;
  const hosted = new AttendedMail(api,{...config,holder:"replacement",hosted:true},{...dependencies,now:()=>clock});
  hosted.start(ctx); await settle(hosted);
  hosted.began(); hosted.messageEnded({message:{role:"assistant",stopReason:"error"}}); hosted.settled({});
  assert.equal(hosted.paused,true);
  const queued = sent.length; send(); await hosted.poll();
  assert.equal(sent.length,queued);
  clock += 60_000; await hosted.poll();
  assert.equal(sent.length,queued+1);
  // Its holder is not a hosted lease, so its wake records are refused, and delivery goes on.
  assert.equal(started().length,0);
  hosted.dispose();
  // A hosted pane records why it woke: queued mail, then the captain's prompt with mail beside it.
  call("ship.attend",{ship:"infra",holder:"host:pi",ttl_secs:90});
  const pane = new AttendedMail(api,{...config,holder:"host:pi",hosted:true},dependencies);
  const woke = [], recording = [], real = pane.call;
  pane.call = (command,args) => {
    const result = real(command,args);
    if (command === "wake.hosted") { woke.push(args); recording.push(result.catch(()=>{})); }
    return result;
  };
  const mailed = send();
  pane.start(ctx); await settle(pane); await Promise.all(recording);
  assert.equal(sent.at(-1).message.details.throughSeq,mailed.seq);
  assert.equal(woke.length,1);
  assert.equal(woke[0].trigger,"mail"); assert.equal(woke[0].through_seq,mailed.seq);
  assert(woke[0].after_seq<mailed.seq);
  const mailWake = started().at(-1);
  assert.equal(mailWake.mode,"hosted"); assert.equal(mailWake.reason,"captain");
  assert.equal(mailWake.causes.at(-1).mail_id,mailed.id);
  pane.began(); pane.settled({});
  const typedBeside = send({kind:"notice",urgency:"low"});
  const beside = await pane.poll("context"); await Promise.all(recording);
  assert.equal(beside.message.details.throughSeq,typedBeside.seq);
  assert.equal(woke.length,2); assert.equal(woke[1].trigger,"captain");
  assert.equal(started().at(-1).reason,"captain_pane");
  assert.deepEqual(started().at(-1).causes,[],"a routine notice is no cause");
  pane.dispose();
  // Native factory registers the correct terminal event and compact renderer.
  const saved = {...process.env};
  try {
    Object.assign(process.env,{PRIVATEER_ROLE:"agent",PRIVATEER_ACTOR:"infra",PRIVATEER_HOME:home,
      PRIVATEER_CONTROL_CLI:cli,PRIVATEER_PRESENCE_HOLDER:holder});
    for(const harness of ["pi","pi-signed","omp"]) {
      process.env.PRIVATEER_ATTENDED_HARNESS=harness;
      const handlers=new Map(),commands=new Map(),renderers=new Map();
      attended({on:(event,fn)=>handlers.set(event,fn),registerCommand:(name,fn)=>commands.set(name,fn),
        registerMessageRenderer:(name,fn)=>renderers.set(name,fn)});
      assert(handlers.has(harness==="omp"?"agent_end":"agent_settled"));
      assert(!handlers.has(harness==="omp"?"agent_settled":"agent_end"));
      assert(handlers.has("session_tree"));
      assert.equal(handlers.has("session_switch"),harness==="omp");
      assert.equal(handlers.has("session_branch"),harness==="omp");
      handlers.get("session_start")({}, {...ctx,hasUI:false});
      assert(commands.has("privateer-mail"));
      const component=renderers.get("privateer-mail")(sent[0].message);
      assert.equal(component.render(80).length,1);
      assert(component.render(80)[0].includes("1000 new mail"));
      assert(component.render(10)[0].length<=10);
      handlers.get("session_shutdown")();
    }
  } finally {
    for(const key of Object.keys(process.env)) if(!(key in saved)) delete process.env[key];
    Object.assign(process.env,saved);
  }
  console.log("Attended mail: burst coalescing, busy/idle, receipts, errors, native events, ownership and shutdown passed");
} finally { rmSync(root,{recursive:true,force:true}); }
