// Native mod API boundary with the real durable mailbox. No model requests.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const cli = realpathSync(process.argv[2]);
const root = realpathSync(mkdtempSync(join(tmpdir(), "privateer-claude-mail-test-")));
const folder = join(root, "ship"); mkdirSync(folder);
const home = join(folder, ".privateer");
const base = { ...process.env, PRIVATEER_FLEET: join(root,"fleet.sqlite"), PRIVATEER_RESOURCE_HOME: join(root,"ledger") };
for (const k of ["PRIVATEER_ROLE","PRIVATEER_ACTOR","PRIVATEER_JOB","PRIVATEER_HOME","PRIVATEER_PRESENCE_HOLDER","PRIVATEER_CLAUDE_MAIL_UNTIL"]) delete base[k];
const run = (args, env = base, input) => execFileSync(cli,args,{cwd:folder,env,input,encoding:"utf8",timeout:10000,maxBuffer:4*1024*1024});
const call = (name,args={}) => JSON.parse(run(["--home",home,name,"--json",JSON.stringify(args)]));
let sequence=0, session="native-one", draft="", reject=false, delayed, wrapper="", attempts=0, replaceOnSubmit=false;
let storeFailure=false, later=0, usedTokens=50000, replyCompacts=false, compactDeclines=false, hostedFails=false;
const registered=new Set(), reports=[], submitted=[], compactions=[], woke=[], deliveryReports=[], bindings=[];
const records=new Map(), rows=new Map(), notices=[], queue=[], timers=new Set();
const nativeEnv={...base,PRIVATEER_ROLE:"agent",PRIVATEER_ACTOR:"ship",PRIVATEER_HOME:home,
  PRIVATEER_CONTROL_CLI:cli,PRIVATEER_PRESENCE_HOLDER:"start:test",PRIVATEER_ATTENDED_HARNESS:"claude"};
const send=(extra={})=>call("mail.send",{id:`mail-${++sequence}`,from:"captain",to:"ship",kind:"request",body:"private body never included in notification",...extra});
const flush=async()=>{for(let n=0;n<100;n++)await new Promise(resolve=>setImmediate(resolve));};
let hooks;
const fire=async(name,e={},bottom=async x=>x)=>{
  if(name==="command.run"&&!e.origin)e={...e,origin:{kind:"composer"}};
  const entry=hooks.get(name);if(!entry)return bottom(e);
  const next=async value=>bottom(value);
  try{return await entry.fn($,e,next);}catch(error){if(entry.caught)return entry.caught($,e,next);throw error;}
};
const $={
  env:{get:async k=>nativeEnv[k],set:async(k,v)=>{if(v===undefined)delete nativeEnv[k];else nativeEnv[k]=v;}},
  session:{id:async()=>session,surfaces:async()=>["terminal"],messages:async()=>rows.get(session)??[],
    usage:async()=>({context:{tokens:usedTokens,window:200000}}),
    // The documented between-turns compaction: no model turn, and a decline is {skip}.
    compact:async()=>{
      compactions.push(session);
      if(compactDeclines)return {skip:true};
      const before=usedTokens;usedTokens=50000;
      return fire("session.compact",{},async()=>({tokensBefore:before}));
    }},
  store:{get:async k=>records.get(k),set:async(k,v)=>{if(storeFailure)throw Error('store unavailable');records.set(k,structuredClone(v));},keys:async()=>[...records.keys()],delete:async k=>records.delete(k)},
  ui:{status:async text=>notices.push(text)},
  clock:{now:async()=>1000000+later,every:(ms,fn)=>{assert.equal(ms,5000);const t={fn,cancel:()=>timers.delete(t)};timers.add(t);return t;}},
  command:{register:async spec=>{registered.add(spec.name);if(spec.name==="privateer-mail")assert.equal(spec.immediate,true);}},
  process:{run:async(argv,options)=>{
    assert.equal(argv[0],cli);
    if(options.env.PRIVATEER_ROLE==="operator"){
      // A captain command runs as the captain, in the ship folder.
      assert.equal(options.cwd,folder);
      try{return {exitCode:0,stdout:run(argv.slice(1),{...nativeEnv,...options.env}),stderr:""};}
      catch(error){return {exitCode:1,stdout:"",stderr:String(error.stderr)};}
    }
    assert.equal(options.timeoutMs,3000);
    assert.deepEqual(options.env,{PRIVATEER_ROLE:"agent",PRIVATEER_ACTOR:"ship"});
    if(argv[3]==="attention.report")reports.push(JSON.parse(argv[5]));
    if(argv[3]==="wake.hosted"){
      const args=JSON.parse(argv[5]);
      (args.trigger==="delivery"?deliveryReports:args.trigger==="delivery_bind"?bindings:woke).push(args);
      if(hostedFails)return {exitCode:1,stdout:"",stderr:"refused"};
    }
    if(delayed){await delayed;delayed=undefined;}
    let stdout=run(argv.slice(1),{...nativeEnv,...options.env});
    // The runtime's line decision is tested in Rust; here the reply stands in for a crossing.
    if(argv[3]==="attention.report"&&replyCompacts)stdout=JSON.stringify({...JSON.parse(stdout),compact:true});
    return {exitCode:0,stdout,stderr:""};
  }},
  prompt:{read:async()=>({text:draft,cursor:draft.length}),submit:async value=>{
    if(value.text==="/compact"){submitted.push(value.text);return {text:value.text};}
    attempts++;
    // Native stop latch declines before any prompt hooks get to run.
    if(reject)return {drop:typeof reject==="string"?reject:"native queue refused"};
    if(replaceOnSubmit){replaceOnSubmit=false;call("ship.attend",{ship:"ship",holder:"replacement",ttl_secs:90});}
    const text=wrapper==="unrelated"?"unrelated":wrapper==="wrapped"?`<wrapper>${value.text}</wrapper>`:value.text;
    const event={text,origin:{kind:"plugin",name:"privateer-mail"}};
    const result=await fire("prompt.submit",event);
    if(result.drop)return result;
    queue.push({session,...value});return event;
  }},
};
let activation=0;
const load=async()=>{
  hooks=new Map();
  const mod=await import(pathToFileURL(join(import.meta.dirname,"../plugins/attended-claude/hooks/register.js")).href+`?activation=${++activation}`);
  mod.register((name,matcher,fn)=>{
    if(typeof matcher==="function")fn=matcher;
    const entry={fn};hooks.set(name,entry);return {catch:fn=>{entry.caught=fn;}};
  });
  await fire("session.start",{isInteractive:true,cwd:folder});await flush();
};
const tick=async()=>{for(const timer of timers)await timer.fn();await flush();};
const begin=async text=>{
  const transcript=rows.get(session)??[];transcript.push({role:"user",text});rows.set(session,transcript);
  await fire("turn.start",{text,turnId:"turn"});
};
const end=async extra=>fire("turn.complete",{reason:"answer",isAborted:false,turnId:"turn",...extra});
// Every wake.started the runtime recorded, oldest first.
const started=()=>{
  const out=[];let after=0;
  for(;;){
    const page=call("audit.list",{after_seq:after,limit:1000}).events;
    if(!page.length)return out;
    out.push(...page.filter(e=>e.kind==="wake.started").map(e=>e.payload));after=page.at(-1).seq;
  }
};

try {
  run(["init","--ship","ship","--harness","claude"]);
  call("ship.attend",{ship:"ship",holder:"start:test",ttl_secs:90});
  const requests=Array.from({length:1000},(_,n)=>({id:String(n),command:"mail.send",args:{id:`burst-${n}`,from:"captain",to:"ship",kind:"request",body:"private durable body"}}));
  assert(run(["--home",home,"rpc"],base,requests.map(x=>JSON.stringify(x)).join('\n')+'\n').trim().split('\n').map(JSON.parse).every(x=>x.ok));
  await load();
  assert.equal(queue.length,1);assert(queue[0].text.length<3000);assert(!queue[0].text.includes("private durable body"));
  assert(queue[0].text.includes("not to other mail in this or later wakes"));
  assert.equal([...records.values()][0].through,0,"queue admission is not delivery evidence");
  for(const timer of timers)timer.cancel();
  await load();assert.equal(queue.length,1,"a native mod reload preserves a queued wake");
  await tick();await tick();assert.equal(queue.length,1,"one native queued notification for the burst");
  await begin(queue[0].text);await end();
  assert.equal([...records.values()][0].through,1000);
  assert.equal(call("mail.pending",{address:"ship"}).count,1000,"presentation never acknowledges mail");
  assert.equal(call("mail.pending",{address:"ship"}).acknowledged_through,0);
  await tick();assert.equal(queue.length,1,"unacknowledged mail does not prompt repeatedly");
  // Busy, subagent and typed-draft boundaries preserve the captain's lane.
  await begin("captain work");
  for(const timer of timers)timer.cancel();
  await load();send();await tick();assert.equal(queue.length,1,"a native mod reload preserves the busy boundary");
  await end({agentId:"child"});await tick();assert.equal(queue.length,1);
  await end();draft="unfinished captain input";await tick();assert.equal(queue.length,1);assert.equal(draft,"unfinished captain input");
  draft="";await tick();assert.equal(queue.length,2);await begin(queue[1].text);await end();
  // Resume/reload receipts are scoped to the exact native conversation.
  await fire("session.end",{reason:"resume"});await load();assert.equal(queue.length,2);assert.equal(timers.size,1);
  // A missing store write recovers from the native transcript on reload.
  records.clear();await fire("session.end",{});await load();assert.equal(queue.length,2);
  // Information-only notices join a real prompt without starting their own turn.
  const low=send({kind:"notice",urgency:"low"});await tick();assert.equal(queue.length,2);
  const result=await fire("prompt.submit",{text:"Captain question",origin:{kind:"composer"}},async e=>{
    assert.equal(e.text,"Captain question");assert.equal(e.context.length,1);await begin(e.text);return e;
  });
  assert.equal(result.text,"Captain question");assert.equal([...records.values()][0].through,low.seq);await end();
  // Refused native admission leaves the sequence eligible for a later attempt.
  send();reject=true;await tick();assert.equal(queue.length,2);reject=false;await tick();assert.equal(queue.length,2);
  await fire("command.run",{command:"privateer-mail",args:"resume"});await flush();assert.equal(queue.length,3);
  await begin(queue[2].text);await end({reason:"error"});send();await tick();assert.equal(queue.length,3);
  assert([...records.values()][0].paused);
  assert((await fire("prompt.submit",{text:"unowned wake",origin:{kind:"plugin",name:"privateer-mail"}})).drop);
  await fire("prompt.submit",{text:"background event",origin:{kind:"peer"}});
  await tick();assert.equal(queue.length,3,"a background peer cannot resume an interrupted captain lane");
  await fire("session.end",{});await load();await tick();assert.equal(queue.length,3,"pause survives reload");
  await fire("command.run",{command:"privateer-mail",args:"resume"});await flush();assert.equal(queue.length,4);
  await begin(queue[3].text);await end({reason:"aborted",isAborted:true});send();await tick();assert.equal(queue.length,4);
  await fire("command.run",{command:"privateer-mail",args:"pause"});
  // Human prompts recover from interruption; a refused prompt has no receipt.
  const before=[...records.values()][0].through;
  await fire("prompt.submit",{text:"blocked",origin:{kind:"composer"}},async()=>({drop:"blocked"}));
  assert.equal([...records.values()][0].through,before);
  await tick();assert.equal(queue.length,4);
  await fire("command.run",{command:"privateer-mail",args:"resume"});await flush();assert.equal(queue.length,5);await begin(queue[4].text);await end();
  // New native IDs never borrow another conversation's receipt watermark.
  await fire("session.end",{});session="native-two";await fire("classic.SessionStart",{});await tick();assert.equal(queue.length,6);
  assert.equal(queue[5].session,"native-two");await begin(queue[5].text);await end();await flush();
  // Each completed turn reports the session's context as the ship's attention.
  assert.deepEqual(reports.at(-1),{ship:"ship",kind:"turn",tokens:50000,window:200000,holder:"start:test",settled:true});
  // A completed turn that leaves running work unmoved gets one more prompt, once per set.
  call("task.create",{id:"ship-docs",parent:"ship-duty",owner:"ship",title:"Write the docs"});
  call("task.update",{id:"ship-docs",actor:"ship",status:"running"});
  await begin("captain work");await end();await flush();
  assert.equal(queue.length,7);assert(queue[6].text.startsWith("Before you stop:"));assert(queue[6].text.includes("ship-docs"));
  await begin(queue[6].text);await end();await flush();assert.equal(queue.length,7,"one nudge per set of idle tasks");
  call("task.update",{id:"ship-docs",actor:"ship",status:"done"});
  // The captain's fleet commands run from the keyboard as the captain, never for a plugin or the model.
  for(const name of ["stow","yolo","afk","back","ships"])assert(registered.has(name),name);
  const ships=await fire("command.run",{command:"ships",args:"",origin:{kind:"composer"}});
  assert(ships.text.includes("ship"),ships.text);
  const refused=await fire("command.run",{command:"yolo",args:"on",origin:{kind:"plugin",name:"other"}});
  assert(refused.text.includes("captain's keyboard"));assert.equal(call("permissions.show").mode,"standard");
  assert((await fire("command.run",{command:"back",args:"now",origin:{kind:"composer"}})).text.startsWith("Usage:"));
  // A hosted session has nobody to resume mail after a native error: it retries a minute later.
  await fire("session.end",{});call("ship.leave",{ship:"ship",holder:"start:test"});
  call("ship.attend",{ship:"ship",holder:"host:test",ttl_secs:90});
  Object.assign(nativeEnv,{PRIVATEER_HOSTED:"1",PRIVATEER_PRESENCE_HOLDER:"host:test"});await load();
  const hostedStart=queue.length;const hostedMail=send();await tick();assert.equal(queue.length,hostedStart+1);
  assert(queue.at(-1).text.includes("Attention: "),"a busy or idle Claude notice carries the attention line");
  assert.equal(woke.length,0,"queueing mail is not a wake; its turn starting is");
  await begin(queue.at(-1).text);await flush();
  // The turn's start records why the hosted first mate woke, once, with the mail it covers.
  assert.equal(woke.length,1);
  assert.equal(woke[0].trigger,"mail");assert.equal(woke[0].holder,"host:test");
  assert.equal(woke[0].through_seq,hostedMail.seq);assert(woke[0].after_seq<hostedMail.seq);
  const mailWake=started().at(-1);
  assert.equal(mailWake.mode,"hosted");assert.equal(mailWake.wake,null);
  assert.equal(mailWake.reason,"captain","the captain's mail is the cause");
  assert.equal(mailWake.causes.at(-1).mail_id,hostedMail.id);
  assert.equal(call("wake.list",{ship:"ship"}).wakes.length,0,"a hosted mail wake keeps out of the wake budget");
  await end({reason:"error"});send();await tick();assert.equal(queue.length,hostedStart+1);
  // A pane restarted while paused keeps that retry; nobody is there to type resume.
  await fire("session.end",{reason:"resume"});await load();await tick();assert.equal(queue.length,hostedStart+1);
  later=61000;await tick();assert.equal(queue.length,hostedStart+2,"hosted mail retries after a minute");
  // A failed record never holds back delivery.
  hostedFails=true;const recorded=started().length;
  await begin(queue.at(-1).text);await flush();
  assert.equal(woke.length,2);assert.equal(started().length,recorded);
  assert.equal([...records.values()].find(r=>r.holder==="host:test").through,queue.at(-1).text.match(/"through":(\d+)/)[1]*1);
  await end();later=0;hostedFails=false;
  // A pause typed in a hosted pane outlasts a restart; only an error pause retries.
  await fire("command.run",{command:"privateer-mail",args:"pause"});const typedPause=queue.length;send();
  await fire("session.end",{});await load();later=61000;await tick();assert.equal(queue.length,typedPause,"a typed pause survives a restart");
  await fire("command.run",{command:"privateer-mail",args:"resume"});await flush();assert.equal(queue.length,typedPause+1);
  await begin(queue.at(-1).text);await end();await flush();later=0;
  // Ready scheduled work reaches an idle hosted session as one turn, claimed at its start and settled at its end.
  call("task.create",{id:"repair",parent:"ship-duty",owner:"ship",title:"Repair"});
  call("task.enqueue",{id:"repair",brief:"Complete the prepared fixture task with evidence."});
  const beforeWork=queue.length;await tick();assert.equal(queue.length,beforeWork+1,"an empty inbox still delivers ready work");
  await tick();assert.equal(queue.length,beforeWork+1,"a pending offer does not repeat");
  const beforeOffer=woke.length;
  await begin(queue.at(-1).text);await flush();
  assert.equal(woke.length,beforeOffer,"a work offer's wake is recorded by its claim, not the pane");
  const workWake=started().find(w=>w.mode==="work");
  assert.equal(workWake.reason,"enqueued");assert.equal(workWake.task_id,"repair");
  assert.equal(call("work.status",{ship:"ship"}).tasks.find(t=>t.task_id==="repair").state,"claimed");
  call("task.update",{id:"repair",actor:"ship",status:"done"});await end();await flush();
  assert(call("wake.list",{ship:"ship"}).wakes.some(w=>w.status==="succeeded"),"the settled offer is recorded");
  await tick();assert.equal(queue.length,beforeWork+1,"completed work leaves the session idle");
  // A hosted resumable first mate compacts through the session API when a settled turn crosses the line.
  // The compaction is not a prompt, so the prompt hook never sees it and no /compact is submitted.
  const reportsBefore=reports.length;
  usedTokens=150000;replyCompacts=true;
  await begin("captain question across the line");await end();await flush();
  replyCompacts=false;
  assert.deepEqual(compactions,[session],"a crossing compacts once through the session API");
  assert.deepEqual(submitted,[],"a crossing never submits /compact as a prompt");
  assert.equal(usedTokens,50000,"the fake compacted");
  assert(reports.slice(reportsBefore).some(r=>r.kind==="compaction"&&r.tokens===150000),"the compaction is reported with the tokens before it");
  await begin("follow-up below the line");await end();await flush();
  assert.equal(compactions.length,1,"a reply without a crossing compacts nothing");
  // A declined compaction ({skip}) is not reported, and the next settled turn past the line asks again.
  usedTokens=150000;replyCompacts=true;compactDeclines=true;
  const declinedReports=reports.filter(r=>r.kind==="compaction").length;
  await begin("declined across the line");await end();await flush();
  assert.equal(compactions.length,2,"the declined compaction was asked");
  assert.equal(reports.filter(r=>r.kind==="compaction").length,declinedReports,"a declined compaction is not reported");
  compactDeclines=false;
  await begin("asked again at the next settled turn");await end();await flush();
  replyCompacts=false;
  assert.equal(compactions.length,3,"a declined crossing asks again at the next settled turn");
  // A stale draft in the pane's input box does not block the compaction, and the draft is left as typed.
  usedTokens=150000;replyCompacts=true;
  await begin("across the line with a stale draft");draft="stale unsent captain input";
  await end();await flush();replyCompacts=false;
  assert.equal(compactions.length,4,"a pane with a draft still compacts at the line");
  assert.equal(draft,"stale unsent captain input","the compaction leaves the draft untouched");
  assert.equal(usedTokens,50000,"the fake compacted with a draft present");
  draft="";
  assert.deepEqual(submitted,[],"no /compact prompt on any path");
  usedTokens=50000;await tick();assert.equal(queue.length,beforeWork+1,"the compaction adds no mail prompt");
  assert.equal(woke.length,beforeOffer,"turns nothing started for the pane record nothing");
  // The captain's own prompt is a wake of its own, with no mail behind it.
  await fire("prompt.submit",{text:"Captain asks directly",origin:{kind:"composer"}},async e=>{await begin(e.text);return e;});
  await flush();await end();await flush();
  assert.deepEqual(woke.at(-1),{ship:"ship",holder:"host:test",session,trigger:"captain"});
  assert.equal(started().at(-1).reason,"captain_pane");
  // A follow-through nudge is the runtime's own follow-up.
  call("task.create",{id:"ship-tests",parent:"ship-duty",owner:"ship",title:"Run the tests"});
  call("task.update",{id:"ship-tests",actor:"ship",status:"running"});
  await begin("captain work in the hosted pane");await end();await flush();
  assert(queue.at(-1).text.startsWith("Before you stop:"));
  await begin(queue.at(-1).text);await flush();
  assert.equal(woke.at(-1).trigger,"follow");assert.equal(started().at(-1).reason,"follow_up");
  call("task.update",{id:"ship-tests",actor:"ship",status:"done"});await end();await flush();
  // Native stop-latch drops before prompt hooks; only three automatic attempts,
  // across reloads, and no submission advances the accepted watermark.
  const accepted=[...records.values()].find(r=>r.holder==="host:test").through;
  const initialAttempts=attempts;
  reject="the session was stopped and nobody has sent input since "+"x".repeat(1000);send();await tick();
  assert.equal(deliveryReports.at(-1).decline_reason,"native_stopped");
  assert.equal(deliveryReports.at(-1).decline_count,1);
  await fire("session.end",{});await load();later+=60001;await tick();
  assert.equal(deliveryReports.at(-1).decline_count,2);
  await fire("session.end",{});await load();later+=60001;await tick();
  assert.equal(deliveryReports.at(-1).decline_count,3);
  const bounded=[...records.values()].find(r=>r.holder==="host:test");
  assert.equal(bounded.through,accepted);assert.equal(bounded.retry,false);
  assert.equal(bounded.lastDrop,reject.slice(0,512));
  assert.equal(attempts,initialAttempts+3);
  await fire("session.end",{});await load();later+=600001;await tick();await tick();
  assert.equal(attempts,initialAttempts+3,"bounded retry survives reload");
  assert(deliveryReports.at(-1).paused);
  // Real accepted input, not submission, resets the count. Wrappers retain receipts.
  reject=false;wrapper="wrapped";
  await fire("command.run",{command:"privateer-mail",args:"resume"});await flush();
  assert.equal(deliveryReports.at(-1).decline_count,3);
  await begin(`<wrapper>${queue.at(-1).text}</wrapper>`);await end();await flush();
  assert.equal(deliveryReports.at(-1).decline_count,0);wrapper="";
  // The guard's own rejection is distinguished from a native stop.
  wrapper="unrelated";send();await tick();wrapper="";
  assert.equal(deliveryReports.at(-1).decline_reason,"guard_not_ours");
  assert.equal(deliveryReports.at(-1).decline_count,1);
  const guarded=await fire("prompt.submit",{text:"stale",origin:{kind:"plugin",name:"privateer-mail"}});
  assert(guarded.drop.startsWith("guard_paused:"));
  // Explicit Stop is never auto-resumed, even by a reload or a future clock.
  await fire("command.run",{command:"privateer-mail",args:"resume"});await flush();
  await begin(queue.at(-1).text);await end({isAborted:true,reason:"aborted"});send();
  const stoppedAttempts=attempts;
  await fire("session.end",{});await load();later+=600001;await tick();
  assert.equal(attempts,stoppedAttempts,"a human Stop stays paused");
  assert([...records.values()].find(r=>r.holder==="host:test").humanPaused);
  // A new native conversation can retain the hosted holder. Its zero receipt
  // does not erase custody's accepted mail, nor reject its new decline reports.
  await fire("session.end",{});session="native-three";
  reject="the session was stopped and nobody has sent input since";await load();
  assert.equal(bindings.at(-1).session,session);
  assert.equal(deliveryReports.at(-1).accepted_through,0);
  assert.equal(deliveryReports.at(-1).decline_count,1);
  later+=60001;await tick();later+=60001;await tick();
  assert.equal(deliveryReports.at(-1).decline_count,3);
  const rolloverReport=deliveryReports.at(-1);
  assert(JSON.parse(run(["--home",home,"wake.hosted","--json",JSON.stringify(rolloverReport)],nativeEnv)).recorded);
  reject=false;
  await fire("session.end",{});call("ship.leave",{ship:"ship",holder:"host:test"});
  call("ship.attend",{ship:"ship",holder:"start:test",ttl_secs:90});nativeEnv.PRIVATEER_PRESENCE_HOLDER="start:test";
  await fire("session.end",{});delete nativeEnv.PRIVATEER_HOSTED;await load();
  // Live hooks use the runtime's clock; model retry tests above use only the
  // deterministic mod clock. Install a fresh marker for this separate boundary.
  send();
  nativeEnv.PRIVATEER_CLAUDE_MAIL_UNTIL=String(Date.now()+12000);
  assert.equal(run(["hook","claude","Stop"],nativeEnv,'{}'),"");
  assert.equal(call("mail.pending",{address:"ship"}).acknowledged_through,0);
  nativeEnv.PRIVATEER_CLAUDE_MAIL_UNTIL=String(Date.now()-1);
  assert(run(["hook","claude","UserPromptSubmit"],nativeEnv,'{}').includes("New mail for ship"));
  // A replaced presence cannot wake, including a probe completing after replace.
  await flush();let release;delayed=new Promise(resolve=>{release=resolve;});
  const inFlight=tick();await flush();await fire("session.end",{});release();await inFlight;
  assert.equal(timers.size,0);const count=queue.length;
  // The lease can move between the delivery probe and the prompt hook.
  await load();send();replaceOnSubmit=true;
  await fire("command.run",{command:"privateer-mail",args:"resume"});await flush();await tick();
  assert.equal(records.get("privateer-mail:"+JSON.stringify([home,"ship",session])).declineReason,"guard_not_owned");
  assert.equal(queue.length,count);
  await fire("session.end",{});await load();await tick();assert.equal(queue.length,count);
  assert.equal(nativeEnv.PRIVATEER_CLAUDE_MAIL_UNTIL,undefined);
  // Storage/probe errors cannot block a captain prompt or leave an active timer.
  call("ship.attend",{ship:"ship",holder:"start:test",ttl_secs:90});await load();
  storeFailure=true;
  const allowed=await fire("prompt.submit",{text:"still works",origin:{kind:"composer"}});
  assert.equal(allowed.text,"still works");assert.equal(timers.size,0);assert.equal(nativeEnv.PRIVATEER_CLAUDE_MAIL_UNTIL,undefined);
  storeFailure=false;nativeEnv.PRIVATEER_JOB="crew-job";
  const beforeCrew=queue.length;await load();await tick();
  assert.equal(timers.size,0);assert.equal(queue.length,beforeCrew,"a crew process never attaches to the captain mailbox");
  console.log("Claude attended mail: burst, native receipts, draft/busy gates, reload, error/abort, lease replacement and hook fallback passed");
} finally { rmSync(root,{recursive:true,force:true}); }
