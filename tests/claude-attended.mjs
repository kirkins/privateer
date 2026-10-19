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
let sequence=0, session="native-one", draft="", reject=false, delayed;
let storeFailure=false, later=0;
const registered=new Set(), reports=[];
const records=new Map(), rows=new Map(), notices=[], queue=[], timers=new Set();
const nativeEnv={...base,PRIVATEER_ROLE:"agent",PRIVATEER_ACTOR:"ship",PRIVATEER_HOME:home,
  PRIVATEER_CONTROL_CLI:cli,PRIVATEER_PRESENCE_HOLDER:"start:test",PRIVATEER_ATTENDED_HARNESS:"claude"};
const send=(extra={})=>call("mail.send",{id:`mail-${++sequence}`,from:"captain",to:"ship",kind:"request",body:"private body never included in notification",...extra});
const flush=async()=>{for(let n=0;n<100;n++)await new Promise(resolve=>setImmediate(resolve));};
let hooks;
const fire=async(name,e={},bottom=async x=>x)=>{
  const entry=hooks.get(name);if(!entry)return bottom(e);
  const next=async value=>bottom(value);
  try{return await entry.fn($,e,next);}catch(error){if(entry.caught)return entry.caught($,e,next);throw error;}
};
const $={
  env:{get:async k=>nativeEnv[k],set:async(k,v)=>{if(v===undefined)delete nativeEnv[k];else nativeEnv[k]=v;}},
  session:{id:async()=>session,surfaces:async()=>["terminal"],messages:async()=>rows.get(session)??[],
    usage:async()=>({context:{tokens:50000,window:200000}})},
  store:{get:async k=>records.get(k),set:async(k,v)=>{if(storeFailure)throw Error('store unavailable');records.set(k,structuredClone(v));},keys:async()=>[...records.keys()],delete:async k=>records.delete(k)},
  ui:{status:async text=>notices.push(text)},
  clock:{now:async()=>Date.now()+later,every:(ms,fn)=>{assert.equal(ms,5000);const t={fn,cancel:()=>timers.delete(t)};timers.add(t);return t;}},
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
    if(delayed){await delayed;delayed=undefined;}
    return {exitCode:0,stdout:run(argv.slice(1),{...nativeEnv,...options.env}),stderr:""};
  }},
  prompt:{read:async()=>({text:draft,cursor:draft.length}),submit:async value=>{
    if(reject)return {drop:"native queue refused"};
    const event={text:value.text,origin:{kind:"plugin",name:"privateer-mail"}};
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

try {
  run(["init","--ship","ship","--harness","claude"]);
  call("ship.attend",{ship:"ship",holder:"start:test",ttl_secs:90});
  const requests=Array.from({length:1000},(_,n)=>({id:String(n),command:"mail.send",args:{id:`burst-${n}`,from:"captain",to:"ship",kind:"request",body:"private durable body"}}));
  assert(run(["--home",home,"rpc"],base,requests.map(x=>JSON.stringify(x)).join('\n')+'\n').trim().split('\n').map(JSON.parse).every(x=>x.ok));
  await load();
  assert.equal(queue.length,1);assert(queue[0].text.length<3000);assert(!queue[0].text.includes("private durable body"));
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
  const hostedStart=queue.length;send();await tick();assert.equal(queue.length,hostedStart+1);
  await begin(queue.at(-1).text);await end({reason:"error"});send();await tick();assert.equal(queue.length,hostedStart+1);
  later=61000;await tick();assert.equal(queue.length,hostedStart+2,"hosted mail retries after a minute");
  await begin(queue.at(-1).text);await end();later=0;
  // Ready scheduled work reaches an idle hosted session as one turn, claimed at its start and settled at its end.
  call("task.create",{id:"repair",parent:"ship-duty",owner:"ship",title:"Repair"});
  call("task.enqueue",{id:"repair",brief:"Complete the prepared fixture task with evidence."});
  const beforeWork=queue.length;await tick();assert.equal(queue.length,beforeWork+1,"an empty inbox still delivers ready work");
  await tick();assert.equal(queue.length,beforeWork+1,"a pending offer does not repeat");
  await begin(queue.at(-1).text);await flush();
  assert.equal(call("work.status",{ship:"ship"}).tasks.find(t=>t.task_id==="repair").state,"claimed");
  call("task.update",{id:"repair",actor:"ship",status:"done"});await end();await flush();
  assert(call("wake.list",{ship:"ship"}).wakes.some(w=>w.status==="succeeded"),"the settled offer is recorded");
  await tick();assert.equal(queue.length,beforeWork+1,"completed work leaves the session idle");
  await fire("session.end",{});call("ship.leave",{ship:"ship",holder:"host:test"});
  call("ship.attend",{ship:"ship",holder:"start:test",ttl_secs:90});nativeEnv.PRIVATEER_PRESENCE_HOLDER="start:test";
  await fire("session.end",{});delete nativeEnv.PRIVATEER_HOSTED;await load();
  // Live hooks stay silent only while the mod heartbeat is fresh.
  send();
  assert.equal(run(["hook","claude","Stop"],nativeEnv,'{}'),"");
  assert.equal(call("mail.pending",{address:"ship"}).acknowledged_through,0);
  nativeEnv.PRIVATEER_CLAUDE_MAIL_UNTIL=String(Date.now()-1);
  assert(run(["hook","claude","UserPromptSubmit"],nativeEnv,'{}').includes("New mail for ship"));
  // A replaced presence cannot wake, including a probe completing after replace.
  await flush();let release;delayed=new Promise(resolve=>{release=resolve;});
  const inFlight=tick();await flush();await fire("session.end",{});release();await inFlight;
  assert.equal(timers.size,0);const count=queue.length;
  call("ship.attend",{ship:"ship",holder:"replacement",ttl_secs:90});
  await load();await tick();assert.equal(queue.length,count);
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
