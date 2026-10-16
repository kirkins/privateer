// Real CLI/SQLite boundary with a recording native API; no provider or inference.
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { AttendedMail } from "../plugins/attended/mail.mjs";

const cli = process.argv[2];
const root = mkdtempSync(join(tmpdir(),"privateer-work-native-"));
const env = Object.fromEntries(Object.entries(process.env).filter(([k])=>!k.startsWith("PRIVATEER_")&&!k.startsWith("HERDR_")));
Object.assign(env,{PRIVATEER_RESOURCE_HOME:join(root,"ledger"),PRIVATEER_FLEET:join(root,"fleet.sqlite"),PRIVATEER_SERVICE:"none",XDG_CONFIG_HOME:join(root,"config"),XDG_STATE_HOME:join(root,"state")});
const home=join(root,"home"), repo=join(root,"repo"), holder="host:native-fixture";
const call=(command,args)=>{
  const p=spawnSync(cli,["--home",home,command,"--json",JSON.stringify(args)],{env,encoding:"utf8"});
  assert.equal(p.status,0,`${command}: ${p.stderr}`);
  return JSON.parse(p.stdout);
};
let mail;
try {
  mkdirSync(repo);
  const manifest=join(root,"manifest.json");
  writeFileSync(manifest,JSON.stringify({schema_version:1,id:"codex",executable:["/usr/bin/false"],timeout_ms:1000,max_output_bytes:4096}));
  call("init",{});
  call("charter.put",{id:"fixture",repo,objective:"Exercise durable work offers",min_free_bytes:0});
  for(const id of ["infra-duty","repair"]) call("task.create",{id,owner:"infra",charter_id:"fixture",title:id});
  call("ship.put",{name:"infra",scope:"fixture",first_mate:{manifest,duty_task:"infra-duty"}});
  call("ship.attend",{ship:"infra",holder,ttl_secs:90});
  const sent=[],timers=new Set();
  let idle=true,reject=false;
  const ctx={hasUI:true,isIdle:()=>idle,getContextUsage:()=>({tokens:1000,contextWindow:100000}),
    sessionManager:{getBranch:()=>[]},ui:{setStatus(){},getEditorText:()=>""}};
  const api={sendMessage(message,options){if(reject)throw Error("queue refused");sent.push({message,options});}};
  const dependencies={call:async(command,args)=>call(command,args),every:fn=>{timers.add(fn);return fn;},cancel:fn=>timers.delete(fn)};
  mail=new AttendedMail(api,{cli,home,ship:"infra",holder,hosted:true},dependencies);
  call("task.enqueue",{id:"repair",brief:"Complete the prepared fixture task with evidence."});
  mail.start(ctx);await mail.flight;
  assert.equal(sent.length,1,"an empty inbox must still deliver ready work");
  assert.equal(sent[0].message.customType,"privateer-work");
  assert.equal(sent[0].message.details.taskId,"repair");
  assert.deepEqual(sent[0].options,{triggerTurn:true,deliverAs:"followUp"});
  await mail.poll();await mail.poll();assert.equal(sent.length,1,"pending offers must not repeat");
  assert.equal(call("mail.pending",{address:"infra"}).count,0,"work must not manufacture mailbox traffic");
  idle=false;mail.began();await Promise.resolve();
  assert.equal(call("work.status",{ship:"infra"}).tasks[0].state,"claimed");
  await mail.poll();
  call("task.update",{id:"repair",actor:"infra",status:"done"});
  idle=true;mail.settled({});await Promise.resolve();await mail.poll();
  assert.equal(sent.length,1,"completion must leave the model idle");
  assert.equal(call("wake.list",{ship:"infra"}).wakes[0].status,"succeeded");
  // A native queue refusal releases its unaccepted offer into a durable cooldown.
  call("task.create",{id:"retry",owner:"infra",charter_id:"fixture",title:"retry"});
  call("task.enqueue",{id:"retry",brief:"A separately prepared retry task."});
  reject=true;await mail.poll();
  const retry=call("work.status",{ship:"infra"}).tasks.find(t=>t.task_id==="retry");
  assert.equal(retry.state,"ready");assert(retry.next_check_ms>Date.now());
  reject=false;await mail.poll();assert.equal(sent.length,1);
  mail.dispose();assert.equal(timers.size,0);
  console.log("ok - native work delivery crosses the actual CLI: empty inbox, one offer, progress acknowledgment, completion and bounded queue failure");
} finally {mail?.dispose();rmSync(root,{recursive:true,force:true});}
