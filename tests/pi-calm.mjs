// Bounded event/clock behavior tests. No snapshots, model or network.
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { assistantView, operational, readPreference, writePreference } from "../plugins/attended/calm-policy.mjs";
import { Boat, WorkingBoat } from "../plugins/attended/calm-boat.mjs";
import { registerCalm } from "../plugins/attended/calm.mjs";
import { installLayouts } from "../plugins/attended/calm-layout.mjs";
import { queueCapability } from "../plugins/attended/calm-queue.mjs";

const strip = text => text.replace(/\x1b\[[\d;]*m/g, "");
const text = text => ({type:"text",text});
const thinking = thinking => ({type:"thinking",thinking});
const tool = {type:"toolCall",id:"read-1",name:"read",arguments:{path:"fixture"}};
const assistant = (content,stopReason="toolUse") => ({role:"assistant",content,stopReason});
export const notification = (home,n) => ({role:"custom",customType:"privateer-mail",display:true,
  content:`Exact privateer notification ${n}`,details:{home,ship:"fixture",throughSeq:n,count:1}});
const flush = async () => { for(let i=0;i<8;i++) await new Promise(resolve=>setImmediate(resolve)); };

export async function policyAndClock() {
  const home=mkdtempSync(join(tmpdir(),"privateer-calm-"));
  try {
    assert.equal(readPreference(home),false);
    writePreference(home,true); assert.equal(readPreference(home),true);
    const other=join(home,"other"); assert.equal(readPreference(other),false);
    for(const raw of ["x".repeat(239)," padded ",""]) {
      const original=assistant([thinking("private"),text(raw),tool]);
      const frozen=JSON.stringify(original);
      assert.deepEqual(assistantView(original,false,true).message.content,[tool]);
      assert.equal(JSON.stringify(original),frozen);
      assert.equal(assistantView(original,true,true).message.content.some(c=>c.type==="text"),true);
      assert.equal(assistantView({...original,stopReason:"stop"},false,true).message.content.some(c=>c.type==="text"),true);
    }
    for(const raw of ["x".repeat(240),"line\n", "\nline", "line\r"]) {
      assert.equal(assistantView(assistant([text(raw),tool]),false,true).message.content[0].text,raw);
    }
    assert.equal(assistantView(assistant([text("truncated final")],"length"),false,true).message.content.length,1);
    assert.equal(assistantView(assistant([text("working"),tool],"length"),false,true).message.content.length,1);
    const mixed=assistant([thinking("one"),text("x\ny"),thinking("two"),tool]);
    const view=assistantView(mixed,false,true,new Map([[1,false]]));
    assert.deepEqual(view.thinkingRuns,[1]); assert.equal(view.message.content[1].thinking,"two");
    const op=notification(home,1);
    assert(operational(op,home));
    for(const near of [{...op,role:"user"},{...op,customType:"human-mail"},
      {...op,details:{...op.details,home:other}}, {...op,details:{...op.details,count:0}},
      {...op,content:[{type:"image",data:"fixture",mimeType:"image/png"}]},
      {role:"user",content:[text("privateer-mail: hello")] }]) assert(!operational(near,home));

    const boat=new Boat();
    for(const width of [0,1,2,3,4,5,6,12,80,160,8,100]) {
      for(let n=0;n<100;n++) {
        const frame=boat.frame(width);
        assert.equal(frame.length,width<5?1:2);
        for(const row of frame) assert.equal(Array.from(strip(row)).length,width);
        if(width>=5) {
          assert(strip(frame[0]).includes("◿│◣")); assert(strip(frame[1]).includes("╲▁▁▁╱"));
          assert(frame[1].includes("\x1b[33m╲▁▁▁╱\x1b[39m"));
        }
        const before=boat.column; boat.advance();
        if(boat.tick%4) assert.equal(boat.column,before);
        assert(boat.column>=0 && boat.column<=Math.max(0,width-5));
      }
    }
    const timers=new Set(),widgets=new Map(),visibility=[];
    const deps={every(fn,ms){assert.equal(ms,220);const timer={fn};timers.add(timer);return timer;},cancel:t=>timers.delete(t)};
    const ctx={mode:"tui",hasUI:true,isIdle:()=>true,ui:{
      setWorkingVisible:value=>visibility.push(value),
      setWidget(key,factory){widgets.get(key)?.dispose();widgets.delete(key);if(factory)widgets.set(key,factory({requestRender(){}}));},notify(){}}};
    const working=new WorkingBoat(deps);
    working.start(ctx);working.start(ctx);assert.equal(timers.size,1);
    const component=widgets.get("calm-working-ship");component.render(80);
    for(let i=0;i<12;i++) [...timers][0].fn();
    component.render(80);const frozen={column:working.boat.column,direction:working.boat.direction,tick:working.boat.tick};
    // An unpainted scheduler tick is not the last rendered boat position.
    for(let i=0;i<4;i++) [...timers][0].fn();
    working.stop();working.stop();assert.equal(timers.size,0);
    working.start(ctx);widgets.get("calm-working-ship").render(160);
    assert.deepEqual({column:working.boat.column,direction:working.boat.direction,tick:working.boat.tick},frozen);
    working.stop();
    const handlers=new Map(),commands=new Map();
    const api={on:(e,fn)=>{handlers.set(e,fn);return()=>handlers.delete(e);},registerCommand:(n,c)=>commands.set(n,c)};
    const env={PRIVATEER_ROLE:"agent",PRIVATEER_HOME:home,PRIVATEER_ATTENDED_HARNESS:"pi"};
    const controller=registerCalm(api,{},env,{...deps,warn(){}});
    handlers.get("session_start")({},ctx);
    assert(controller.state.enabled); await commands.get("calm").handler("ignored",ctx);
    assert(!controller.state.enabled);assert(!readPreference(home));
    handlers.get("agent_start")({},ctx);assert.equal(timers.size,0);
    await commands.get("calm").handler("",ctx);assert.equal(timers.size,1);
    handlers.get("agent_start")({},ctx);assert.equal(timers.size,1);
    assert(!handlers.has("agent_end"));
    for(const end of [{},{aborted:true},{error:"failed"}]) {
      handlers.get("agent_settled")(end,ctx);assert.equal(timers.size,0);
      handlers.get("agent_start")({},ctx);assert.equal(timers.size,1);
    }
    handlers.get("session_shutdown")();assert.equal(timers.size,0);
    // Session replacement may retain and rebind this extension runtime.
    assert(controller.state.disposed);
    handlers.get("session_start")({},ctx);assert(!controller.state.disposed);
    handlers.get("agent_start")({},ctx);assert.equal(timers.size,1);
    handlers.get("session_shutdown")();assert.equal(timers.size,0);
    const reloaded=registerCalm(api,{},env,{...deps,warn(){}});
    assert(reloaded.state.enabled);handlers.get("session_start")({},ctx);assert.equal(timers.size,0);
    const failure=registerCalm(api,{},env,{...deps,warn(){},write(){throw Error("read-only");}});
    await commands.get("calm").handler("",ctx);assert(failure.state.enabled);failure.dispose();
    for(const harness of ["omp","claude","codex"]) assert.equal(registerCalm(api,{}, {...env,PRIVATEER_ATTENDED_HARNESS:harness}),undefined);
    assert.equal(visibility.at(-1),true);
    // No preference rewrite or context/session entry hook on failure/off.
    assert.equal(readFileSync(join(home,"config/calm"),"utf8"),"on\n");
    console.log("ok - calm policy, home persistence, toggles, clock, width and lifecycle");
  } finally {rmSync(home,{recursive:true,force:true});}
}

export async function queueBehavior(host, session) {
  const home="/fixture/home", warnings=[];
  const state={home,enabled:true,disposed:false,epoch:0,warn:s=>warnings.push(s)};
  const layouts=installLayouts(host,state);
  const mode=Object.create(host.InteractiveMode.prototype);
  Object.defineProperty(mode,"session",{value:session,writable:true});
  mode.compactionQueuedMessages=[];
  let editor="human draft";
  mode.editor={getText:()=>editor,setText:s=>{editor=s;}};
  mode.updatePendingMessagesDisplay=()=>{};
  const notices=[];mode.showStatus=s=>notices.push(s);
  assert(queueCapability(mode),"installed native queue-retention seam changed");
  const first=notification(home,1),second=notification(home,2);
  session.agent.followUp(first);session.agent.followUp(second);
  await session._queueFollowUp("captain's queued input");
  const delivered=[];
  const originalSend=session.sendCustomMessage;
  session.sendCustomMessage=async message=>{
    delivered.push(message);
    // Simulated native turn drains the remaining queue without model calls.
    for(const q of [session.agent.steeringQueue,session.agent.followUpQueue]) {
      delivered.push(...q.messages);q.clear();
    }
  };
  try {
    assert.equal(mode.restoreQueuedMessagesToEditor({abort:true}),1);
    assert.equal(editor,"captain's queued input\n\nhuman draft");
    assert.deepEqual(session.agent.followUpQueue.messages,[first,second]);
    await flush();assert.deepEqual(delivered,[first,second]);
    assert.equal(notices.length,1);assert(!editor.includes(first.content));
    mode.restoreQueuedMessagesToEditor();await flush();assert.equal(delivered.length,2);
    // A queue_update listener can enqueue before native clear returns.
    const newer=notification(home,3),clear=session.clearQueue;
    session.agent.followUp(first);
    session.clearQueue=function(){const result=clear.call(this);this.agent.followUp(newer);return result;};
    mode.restoreQueuedMessagesToEditor();session.clearQueue=clear;
    assert.deepEqual(session.agent.followUpQueue.messages,[first,newer]);
    await flush();assert.deepEqual(delivered,[first,second,first,newer]);
    const deliveredBeforeCompaction=delivered.length;
    session.agent.followUp(first);mode.compactionQueuedMessages=[{text:"compaction human",mode:"followUp"}];
    // A live compaction (not merely its editor queue) must prevent a new turn.
    const descriptor=Object.getOwnPropertyDescriptor(session,"isCompacting");
    Object.defineProperty(session,"isCompacting",{value:true,configurable:true});
    mode.restoreQueuedMessagesToEditor();await flush();assert.equal(delivered.length,deliveredBeforeCompaction);
    assert.deepEqual(session.agent.followUpQueue.messages,[first]);
    if(descriptor) Object.defineProperty(session,"isCompacting",descriptor); else delete session.isCompacting;
    session.sendCustomMessage=async()=>{throw Error("preparation refused");};
    mode.restoreQueuedMessagesToEditor();await flush();assert.deepEqual(session.agent.followUpQueue.messages,[first]);
    assert(warnings.some(s=>s.includes("continuation failed")),JSON.stringify(warnings));
    // A failure after the durable custom receipt must never duplicate input.
    const getBranch=session.sessionManager.getBranch;
    session.sessionManager.getBranch=()=>[{type:"custom_message",customType:first.customType,details:first.details}];
    mode.restoreQueuedMessagesToEditor();await flush();assert.deepEqual(session.agent.followUpQueue.messages,[]);
    session.sessionManager.getBranch=getBranch;
    // Dequeue (without abort) retains order/priority and one in-flight owner.
    delivered.length=0;notices.length=0;
    session.sendCustomMessage=async message=>{delivered.push(message);};
    session.agent.steeringQueue.enqueue(first);session.agent.followUp(second);
    let idle;
    const wait=session.waitForIdle;
    session.waitForIdle=()=>new Promise(resolve=>{idle=resolve;});
    mode.restoreQueuedMessagesToEditor();mode.restoreQueuedMessagesToEditor();
    session.agent.followUp(newer); // Arrival while waiting must stay behind old mail.
    session.waitForIdle=wait;idle();await flush();
    assert.deepEqual(delivered,[first]);assert.deepEqual(session.agent.followUpQueue.messages,[second,newer]);
    assert.equal(notices.length,1);
    // Session shutdown or rebinding invalidates old idle waits. They cannot
    // launch on the abandoned branch, even if the old session becomes idle.
    for(const boundary of ["shutdown","rebind"]) {
      delivered.length=0;notices.length=0;
      session.waitForIdle=()=>new Promise(resolve=>{idle=resolve;});
      mode.restoreQueuedMessagesToEditor();
      state.epoch++;
      if(boundary==="shutdown") state.disposed=true;
      else mode.session={};
      session.waitForIdle=wait;idle();await flush();
      assert.deepEqual(delivered,[]);assert.equal(notices.length,0);
      assert.deepEqual(session.agent.followUpQueue.messages,[second,newer]);
      state.disposed=false;mode.session=session;
    }
    session.sendCustomMessage=originalSend;
    state.enabled=false;
    mode.restoreQueuedMessagesToEditor();await flush();assert.equal(session.agent.followUpQueue.messages.length,0,"off stays stock");
    // Missing session seam degrades before any queue is hidden/retained.
    const unsupported={...session,agent:undefined,clearQueue:()=>({steering:[],followUp:[]}),abort(){}};
    mode.session=unsupported;state.enabled=true;
    mode.restoreQueuedMessagesToEditor();assert(warnings.some(s=>s.includes("session")));
    console.log("ok - queue restore preserves custom mail, order, compaction, refusal and off behavior");
  } finally {session.sendCustomMessage=originalSend;layouts.dispose();}
}

export async function layoutBehavior(host,tui) {
  const home="/fixture/home",warnings=[];
  const state={home,enabled:true,disposed:false,warn:s=>warnings.push(s)};
  const geometry=new Boat();
  for(const width of [0,1,3,4,5,6,12,80,160,8,100]) {
    for(let tick=0;tick<40;tick++) {
      for(const row of geometry.frame(width)) assert.equal(tui.visibleWidth(row),width);
      geometry.advance();
    }
  }
  const layouts=installLayouts(host,state);
  const mode=Object.create(host.InteractiveMode.prototype);
  const ui={requestRender(){}};
  Object.defineProperty(mode,"session",{value:{extensionRunner:{getMessageRenderer:()=>undefined}}});
  mode.chatContainer=new tui.Container();mode.ui=ui;mode.outputPad=1;mode.hideThinkingBlock=true;
  mode.getMarkdownThemeWithSettings=()=>host.getMarkdownTheme();mode.getMarkdownTransformers=()=>[];
  mode.editor={};mode.updatePendingMessagesDisplay=()=>{};
  try {
    mode.addMessageToChat({role:"user",content:[text("human prompt")]});
    const baseline=mode.chatContainer.render(80).length;
    const op=notification(home,1);mode.addMessageToChat(op);
    assert.equal(mode.chatContainer.render(80).length,baseline,"complete operational row and spacer hide");
    // Real InteractiveMode streaming event path, rather than restored rows only.
    mode.isInitialized=true;mode.footer={invalidate(){}};mode.programStatus={handleEvent(){}};
    mode.pendingTools=new Map();mode.maybeShowThinkingDropNotice=()=>{};mode.maybeShowCacheMissNotice=()=>{};
    const live=assistant([text("live working note")]);
    await mode.handleEvent({type:"message_start",message:live});
    const liveComponent=mode.streamingComponent;
    assert(liveComponent.render(80).some(s=>s.includes("live working note")));
    await mode.handleEvent({type:"message_end",message:live});
    assert.equal(liveComponent.render(80).length,0);
    const message=assistant([thinking("secret one"),text("working note"),tool]);
    const bytes=JSON.stringify(message);
    mode.addMessageToChat(message);
    const component=mode.chatContainer.children.at(-1);
    assert.equal(component.render(80).length,0);
    component.updateContent(message,true);assert(component.render(80).some(s=>s.includes("working note")));
    component.updateContent(message,false);assert.equal(component.render(80).length,0);
    const mixed=assistant([thinking("first private"),text("line\nline"),thinking("second expanded"),tool]);
    component.updateContent(mixed,false);
    component.thinkingVisibilityOverrides.set(1,false);
    component.updateContent(mixed,false);
    assert(component.render(80).some(s=>s.includes("second expanded")));
    assert(!component.render(80).some(s=>s.includes("Thinking...")));
    component.thinkingVisibilityOverrides.clear();component.updateContent(message,false);
    const bash=host.createBashToolDefinition(process.cwd());
    const executed=await bash.execute("calm-execution-on",{command:"printf calm-execution"});
    assert(executed.content.some(c=>c.type==="text" && c.text.includes("calm-execution")));
    const definition=host.createReadToolDefinition(process.cwd());
    const execute=definition.execute;
    const toolComponent=new host.ToolExecutionComponent("read","fixture",{path:"fixture"},{showImages:false},definition,ui,process.cwd());
    mode.chatContainer.addChild(toolComponent);
    toolComponent.updateResult({content:[text("result intact")],details:{}});
    assert.equal(toolComponent.render(80).length,0);assert.equal(definition.execute,execute);
    for(const name of ["bash","edit","write","grep","find","ls"]) {
      const shell=new host.ToolExecutionComponent(name,`owned-${name}`,{}, {},undefined,ui,process.cwd());
      mode.chatContainer.addChild(shell);assert.equal(shell.render(80).length,0,name);
    }
    toolComponent.imageComponents=[{render:()=>["image stays visible"]}];
    assert.deepEqual(toolComponent.render(80),["image stays visible"]);
    toolComponent.imageComponents=[];
    const arbitrary=new host.ToolExecutionComponent("third_party","other",{}, {},undefined,ui,process.cwd());
    mode.chatContainer.addChild(arbitrary);assert(arbitrary.render(80).length>0);
    const exported=new host.ToolExecutionComponent("read","export",{path:"fixture"},{showImages:false},definition,ui,process.cwd());
    assert(exported.render(80).length>0,"unmounted export remains ordinary");
    state.enabled=false;layouts.redraw();
    const stockExecution=await bash.execute("calm-execution-off",{command:"printf calm-execution"});
    assert.deepEqual(stockExecution.content,executed.content,"real subprocess execution stays unchanged");
    assert(component.render(80).some(s=>s.includes("working note")));
    assert(mode.chatContainer.render(80).some(s=>s.includes(op.content)));
    assert(toolComponent.render(80).length>0);
    assert.equal(JSON.stringify(message),bytes);assert.equal(component.lastMessage,message);
    assert.equal(warnings.length,0);
    console.log("ok - installed Pi mounted layout: gapless rows, streaming, expansion, tools, off and export boundary");
  } finally {layouts.dispose();}
  // Independent degradation must not stop the other adapters or /calm.
  const missing=[];installLayouts({}, {home,enabled:true,warn:s=>missing.push(s)}).dispose();
  assert(missing.includes("assistant layout"));assert(missing.includes("queued operational rows"));
}

export function queueCapabilityBehavior() {
  // Each exact member used by retention must fail closed independently,
  // leaving stock listing/restore calls and queue objects untouched.
  const seams=[
    ["session","waitForIdle"],["session","isIdle"],["session","isCompacting"],
    ["session","sendCustomMessage"],["session","clearQueue"],["session","abort"],
    ["session","sessionManager","getBranch"],["session","agent"],
    ["session","agent","steeringQueue"],["session","agent","followUpQueue"],
    ["session","agent","steeringQueue","messages"],["session","agent","steeringQueue","enqueue"],
    ["session","agent","followUpQueue","messages"],["session","agent","followUpQueue","enqueue"],
    ["compactionQueuedMessages"],["showStatus"],["clearAllQueues"],
    ["updatePendingMessagesDisplay"],["restoreQueuedMessagesToEditor"],
  ];
  for(const seam of seams) {
    let listed=0,restored=0;
    class Stock extends SimulatedMode {
      updatePendingMessagesDisplay(){listed++;}
      restoreQueuedMessagesToEditor(){restored++;return "stock";}
    }
    const warnings=[],state={home:"/fixture/home",enabled:true,disposed:false,warn:s=>warnings.push(s)};
    const layouts=installLayouts({InteractiveMode:Stock},state);
    try {
      const mode=new Stock();mode.session=simulatedQueueSession();
      mode.compactionQueuedMessages=[];mode.showStatus=()=>{};
      const queue=mode.session.agent.followUpQueue,mail=notification(state.home,1);queue.enqueue(mail);
      assert(queueCapability(mode));
      const object=seam.slice(0,-1).reduce((o,k)=>o[k],mode),key=seam.at(-1),original=object[key];
      object[key]=undefined;
      assert.equal(queueCapability(mode),false,seam.join("."));
      // Missing native entry methods are load-time adapter degradation; all
      // other members are probed before first listing/restore for a session.
      if(key==="updatePendingMessagesDisplay" || key==="restoreQueuedMessagesToEditor") continue;
      for(let i=0;i<2;i++) {
        mode.updatePendingMessagesDisplay();
        assert.equal(mode.restoreQueuedMessagesToEditor(),"stock");
      }
      assert.equal(listed,2);assert.equal(restored,2);
      object[key]=original;
      assert.deepEqual(queue.messages,[mail]);
      assert.equal(warnings.filter(s=>s==="queued operational retention (session)").length,1,seam.join("."));
    } finally {layouts.dispose();}
  }
  for(const receipt of [()=>undefined,()=>({}),()=>{throw Error("receipt unavailable");}]) {
    let restored=0;
    class Stock extends SimulatedMode { restoreQueuedMessagesToEditor(){restored++;return "stock";} }
    const warnings=[],state={home:"/fixture/home",enabled:true,disposed:false,warn:s=>warnings.push(s)};
    const layouts=installLayouts({InteractiveMode:Stock},state);
    try {
      const mode=new Stock();mode.session=simulatedQueueSession();
      mode.compactionQueuedMessages=[];mode.showStatus=()=>{};
      mode.session.sessionManager.getBranch=receipt;
      const mail=notification(state.home,1);mode.session.agent.followUp(mail);
      assert.equal(queueCapability(mode),false);
      for(let i=0;i<2;i++) assert.equal(mode.restoreQueuedMessagesToEditor(),"stock");
      assert.equal(restored,2);assert.deepEqual(mode.session.agent.followUpQueue.messages,[mail]);
      assert.equal(warnings.filter(s=>s==="queued operational retention (session)").length,1);
    } finally {layouts.dispose();}
  }
  for(const seam of ["updatePendingMessagesDisplay","restoreQueuedMessagesToEditor","clearAllQueues"]) {
    class Missing extends SimulatedMode {}
    Missing.prototype[seam]=undefined;
    const warnings=[],original=Missing.prototype.restoreQueuedMessagesToEditor;
    const layouts=installLayouts({InteractiveMode:Missing},{enabled:true,warn:s=>warnings.push(s)});
    assert.equal(Missing.prototype.restoreQueuedMessagesToEditor,original);
    assert.equal(warnings.filter(s=>s==="queued operational rows").length,1);
    layouts.dispose();
  }
  console.log("ok - every queue retention capability degrades stock with one session diagnostic");
}

// The routine CI path simulates only events/queues. The installed-Pi fixture
// above runs the same cases over real exported classes and AgentSession queues.
function simulatedQueueSession() {
  const queue=()=>({messages:[],enqueue(m){this.messages.push(m);},clear(){this.messages=[];}});
  const agent={steeringQueue:queue(),followUpQueue:queue(),
    followUp(m){this.followUpQueue.enqueue(m);}};
  return {agent,isIdle:true,isCompacting:false,waitForIdle:async()=>{},abort(){},sessionManager:{getBranch:()=>[]},
    async _queueFollowUp(s){this.followUp??=[];this.followUp.push(s);agent.followUp({role:"user",content:[text(s)]});},
    clearQueue(){const followUp=this.followUp??[];this.followUp=[];agent.steeringQueue.clear();agent.followUpQueue.clear();return {steering:[],followUp};},
    async sendCustomMessage(){throw Error("fixture must replace continuation");}};
}
class SimulatedMode {
  updatePendingMessagesDisplay() {}
  clearAllQueues() {
    const result=this.session.clearQueue();
    result.followUp.push(...this.compactionQueuedMessages.map(m=>m.text));
    this.compactionQueuedMessages=[];return result;
  }
  restoreQueuedMessagesToEditor(options) {
    const q=this.clearAllQueues(),all=[...q.steering,...q.followUp];
    if(all.length) this.editor.setText([all.join("\n\n"),options?.currentText??this.editor.getText()].filter(s=>s.trim()).join("\n\n"));
    this.updatePendingMessagesDisplay();if(options?.abort)this.session.abort();return all.length;
  }
}
if(process.argv[1] && import.meta.url===pathToFileURL(process.argv[1]).href) {
  await policyAndClock();
  queueCapabilityBehavior();
  await queueBehavior({InteractiveMode:SimulatedMode},simulatedQueueSession());
}
