// Actual scoped ES modules, with deterministic browser boundaries and no network.
// These tests exercise user events and delayed responses rather than source text.
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import {fileURLToPath} from "node:url";
import {SourceTextModule, createContext} from "node:vm";

const page = new URL("../plugins/live-voice/page/", import.meta.url);
const storageKey = "privateer-voice-scoped-token";
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return {promise, resolve, reject};
};
const response = (body, status = 200) => ({ok: status >= 200 && status < 300, status, json: async () => body});

class Element {
  constructor(tagName = "div", fragment = false) {
    this.tagName = tagName;
    this.fragment = fragment;
    this.children = [];
    this.listeners = new Map();
    this.attributes = new Map();
    this.style = {};
    this.dataset = {};
    this.disabled = false;
    this.hidden = false;
    this.value = "";
    this.text = "";
  }
  set textContent(value) { this.text = String(value ?? ""); this.children = []; }
  get textContent() { return this.text + this.children.map(child => child.textContent).join(""); }
  append(...nodes) {
    for (const node of nodes) this.children.push(...(node.fragment ? node.children : [node]));
  }
  replaceChildren(...nodes) { this.text = ""; this.children = []; this.append(...nodes); }
  setAttribute(key, value) { this.attributes.set(key, String(value)); }
  addEventListener(type, callback) {
    const callbacks = this.listeners.get(type) ?? [];
    callbacks.push(callback);
    this.listeners.set(type, callbacks);
  }
  async dispatch(type) {
    if (type === "click" && this.disabled) throw new Error("Test clicked a disabled button");
    await Promise.all((this.listeners.get(type) ?? []).map(callback => callback({target: this})));
  }
  querySelectorAll(tagName) {
    return this.children.flatMap(child => [
      ...(child.tagName === tagName ? [child] : []), ...child.querySelectorAll(tagName),
    ]);
  }
}

async function flush() { for (let turn = 0; turn < 30; turn++) await Promise.resolve(); }
async function until(predicate, label) {
  for (let turn = 0; turn < 60; turn++) {
    if (predicate()) return;
    await flush();
  }
  throw new Error("Browser fixture did not settle: " + label);
}

async function browser({media = false} = {}) {
  const elements = new Map();
  const html = await readFile(new URL("index.scoped.html", page), "utf8");
  for (const match of html.matchAll(/<([a-z][\w-]*)\b([^>]*\bid="([^"]+)"[^>]*)>/gi)) {
    const element = new Element(match[1]);
    element.disabled = /\sdisabled(?:\s|>|$)/.test(match[2]);
    element.hidden = /\shidden(?:\s|>|$)/.test(match[2]);
    elements.set(match[3], element);
  }
  const el = id => {
    assert(elements.has(id), "Actual page is missing fixture element: " + id);
    return elements.get(id);
  };
  const document = {
    getElementById: el,
    createElement: tag => new Element(tag),
    createDocumentFragment: () => new Element("fragment", true),
  };
  const storage = new Map([[storageKey, "paired-old"]]);
  const window = new Element("window"), timers = [], requests = [], peers = [], tracks = [];
  const fixture = {
    el, storage, timers, requests, peers, tracks, window,
    directory: ["alpha"], session: null, serial: 0, workspace: 0,
    intercept: null, offerGate: null, holdIce: false, mediaCalls: 0,
    click: id => el(id).dispatch("click"),
    runPoll() {
      const index = timers.findIndex(timer => timer.delay === 1000);
      assert(index !== -1, "A state poll should be scheduled");
      return timers.splice(index, 1)[0].callback();
    },
  };
  fixture.state = () => ({phase: fixture.session ? "live" : "idle", session_id: fixture.session,
    codex: {}, warning: "Mail disabled; not checked.", draft: null, outcome: "", transcript: []});
  fixture.directoryReply = ident => ({workspace_id: ident, view: "directory", data: {
    names: fixture.directory, coverage: {inventory_complete: true, sources: [{
      machine: "sample", ship: null, observed_at: 1000, age_ms: 50,
      state: "covered", mode: "local_live",
    }]},
  }});
  const fetch = async (path, options = {}) => {
    const request = {path, token: options.headers?.["X-Privateer-Token"],
      body: options.body ? JSON.parse(options.body) : undefined};
    requests.push(request);
    const intercepted = fixture.intercept?.(request);
    if (intercepted !== undefined) return intercepted;
    if (path === "/api/version") return response({protocol: 2, access_mode: "scoped", features: ["workspace_reads"]});
    if (path === "/api/state") return response(fixture.state());
    if (path === "/api/pair") return response({token: "paired-new", device_id: "new-device"});
    if (path === "/api/workspace/open") return response({workspace_id: "workspace-" + ++fixture.workspace,
      device_id: "sample-device", views: ["directory", "runtime"], expires_in_secs: 900});
    if (path === "/api/workspace/read") return response(request.body.view === "directory"
      ? fixture.directoryReply(request.body.workspace_id)
      : {workspace_id: request.body.workspace_id, view: "runtime", data: {
        available: true, text: "Checked alpha at original observation time. Mail disabled; not checked.",
      }});
    if (path === "/api/session/start") {
      fixture.session = "session-" + ++fixture.serial;
      return response({session_id: fixture.session, answer: "v=0 sample answer"});
    }
    if (path === "/api/session/stop") {
      if (request.body.session_id === fixture.session) fixture.session = null;
      return response({ok: true});
    }
    if (["/api/session/connected", "/api/workspace/close"].includes(path)) return response({ok: true});
    throw new Error("Unexpected browser request: " + path);
  };
  const navigator = {};
  class Peer extends Element {
    constructor() {
      super("peer"); this.iceGatheringState = fixture.holdIce ? "gathering" : "complete";
      this.connectionState = "new"; this.closed = false; peers.push(this);
    }
    addTrack() {}
    createDataChannel() {}
    async createOffer() { return fixture.offerGate ? fixture.offerGate.promise : {sdp: "v=0 sample offer"}; }
    async setLocalDescription(offer) { this.localDescription = offer; }
    async setRemoteDescription(answer) { this.remoteDescription = answer; }
    close() { this.closed = true; this.connectionState = "closed"; }
    connect() { this.connectionState = "connected"; this.onconnectionstatechange?.(); }
    async finishIce() { this.iceGatheringState = "complete"; await this.dispatch("icegatheringstatechange"); }
  }
  if (media) navigator.mediaDevices = {async getUserMedia() {
    fixture.mediaCalls++;
    const track = {enabled: true, stopped: false, stop() { this.stopped = true; }};
    tracks.push(track);
    return {getTracks: () => [track], getAudioTracks: () => [track]};
  }};
  const context = createContext({document, window, navigator, fetch, console,
    ...(media ? {RTCPeerConnection: Peer} : {}),
    localStorage: {getItem: key => storage.get(key) ?? null,
      setItem: (key, value) => storage.set(key, value), removeItem: key => storage.delete(key)},
    setTimeout: (callback, delay) => { const timer = {callback, delay}; timers.push(timer); return timer; },
  });
  const modules = new Map();
  async function load(url) {
    if (modules.has(url.href)) return modules.get(url.href);
    assert([new URL("app.scoped.js", page).href, new URL("workspace.scoped.js", page).href].includes(url.href));
    const module = new SourceTextModule(await readFile(url, "utf8"), {context, identifier: url.href});
    modules.set(url.href, module);
    await module.link((specifier, parent) => load(new URL(specifier, parent.identifier)));
    return module;
  }
  await (await load(new URL("app.scoped.js", page))).evaluate();
  await until(() => el("fleetList").querySelectorAll("button").length === 1
    && !el("refreshWorkspace").disabled && timers.some(timer => timer.delay === 1000), "initial workspace");
  return fixture;
}

async function repair(fixture) {
  fixture.el("code").value = "SAMPLECODE";
  await fixture.click("pairButton");
  await until(() => !fixture.el("refreshWorkspace").disabled
    && fixture.el("fleetList").querySelectorAll("button").length === 1, "re-paired workspace");
}

const tests = [
  ["plain clients read the workspace without browser or audio APIs", async () => {
    const f = await browser();
    await f.el("fleetList").querySelectorAll("button")[0].dispatch("click");
    assert.match(f.el("runtimeReport").textContent, /Checked alpha/);
    assert.equal(f.mediaCalls, 0);
    assert(!f.requests.some(request => request.path.startsWith("/api/session/")));
    assert(!f.requests.some(request => request.body?.tools));
  }],
  ["stopping voice retains workspace observations and pairing", async () => {
    const f = await browser({media: true});
    await f.el("fleetList").querySelectorAll("button")[0].dispatch("click");
    const observations = f.el("runtimeReport").textContent;
    await f.click("start");
    await f.click("stop");
    assert.equal(f.el("runtimeReport").textContent, observations);
    assert.equal(f.el("fleetList").querySelectorAll("button").length, 1);
    assert.equal(f.storage.get(storageKey), "paired-old");
    assert(f.peers[0].closed && f.tracks[0].stopped);
    assert(!f.requests.some(request => request.path === "/api/workspace/close"));
  }],
  ["an old state response cannot clear a replacement voice session", async () => {
    const f = await browser({media: true});
    await f.click("start");
    const gate = deferred(), oldState = {...f.state(), transcript: [{role: "captain", text: "old-session-text"}]};
    f.intercept = request => request.path === "/api/state" ? gate.promise : undefined;
    const pending = f.runPoll();
    await flush();
    await f.click("stop");
    await f.click("start");
    const replacement = f.peers.at(-1);
    gate.resolve(response(oldState));
    await pending;
    assert.equal(replacement.closed, false, "old poll closed the new peer");
    assert.equal(f.el("stop").disabled, false);
    assert(!f.el("transcript").textContent.includes("old-session-text"));
  }],
  ...["offer", "ice"].map(boundary => ["abandoned " + boundary + " cannot start voice after re-pair", async () => {
    const f = await browser({media: true});
    if (boundary === "offer") f.offerGate = deferred();
    else f.holdIce = true;
    const abandoned = f.click("start");
    await until(() => f.peers.length === 1 && (boundary === "offer"
      || f.peers[0].listeners.has("icegatheringstatechange")), "pending " + boundary);
    f.intercept = request => request.path === "/api/workspace/open"
      ? response({error: "Not paired", code: "not_paired"}, 401) : undefined;
    await f.click("refreshWorkspace");
    assert.equal(f.storage.has(storageKey), false);
    f.intercept = null;
    await repair(f);
    if (boundary === "offer") f.offerGate.resolve({sdp: "v=0 abandoned offer"});
    else await f.peers[0].finishIce();
    await abandoned;
    assert.equal(f.storage.get(storageKey), "paired-new");
    assert(!f.requests.some(request => request.path === "/api/session/start"),
      "an abandoned peer started a session with the replacement credential");
    assert(f.peers[0].closed && f.tracks[0].stopped);
  }]),
  ...[409, 503].map(status => ["a delayed connected " + status + " cannot stop a replacement session", async () => {
    const f = await browser({media: true});
    await f.click("start");
    const gate = deferred();
    f.intercept = request => request.path === "/api/session/connected" ? gate.promise : undefined;
    f.peers[0].connect();
    await flush();
    await f.click("stop");
    await f.click("start");
    const replacement = f.peers.at(-1), stopCount = f.requests.filter(r => r.path === "/api/session/stop").length;
    gate.resolve(response({error: "Old connected request failed", code: "unavailable"}, status));
    await flush();
    assert.equal(replacement.closed, false);
    assert.equal(f.requests.filter(r => r.path === "/api/session/stop").length, stopCount);
  }]),
  ["workspace reset rejects delayed observations under the same credential", async () => {
    const f = await browser(), gate = deferred();
    let held;
    f.intercept = request => {
      if (!held && request.path === "/api/workspace/read") { held = request; return gate.promise; }
    };
    const pending = f.click("refreshWorkspace");
    await until(() => held, "held directory read");
    await f.window.dispatch("pagehide");
    f.directory = ["beta"];
    f.intercept = null;
    await f.click("refreshWorkspace");
    gate.resolve(response({workspace_id: held.body.workspace_id, view: "directory",
      data: {names: ["old-protected-ship"], coverage: {sources: [], inventory_complete: true}}}));
    await pending;
    assert.equal(f.el("fleetList").textContent, "beta");
    assert.equal(f.storage.get(storageKey), "paired-old");
  }],
  ...[200, 401].map(status => ["old credential " + status + " cannot affect a re-paired workspace", async () => {
    const f = await browser(), gate = deferred();
    let held;
    f.intercept = request => {
      if (request.path === "/api/workspace/read" && request.body.view === "runtime") {
        held = request; return gate.promise;
      }
      if (request.path === "/api/state") return response({error: "Not paired", code: "not_paired"}, 401);
    };
    const pending = f.el("fleetList").querySelectorAll("button")[0].dispatch("click");
    await until(() => held, "held runtime read");
    await f.runPoll();
    f.intercept = null;
    f.directory = ["beta"];
    await repair(f);
    gate.resolve(status === 200 ? response({workspace_id: held.body.workspace_id, view: "runtime",
      data: {available: true, text: "old-protected-runtime"}})
      : response({error: "Old token no longer paired", code: "not_paired"}, status));
    await pending;
    assert.equal(f.el("fleetList").textContent, "beta");
    assert(!f.el("runtimeReport").textContent.includes("old-protected-runtime"));
    assert.equal(f.storage.get(storageKey), "paired-new");
  }]),
  ["workspace core unavailability preserves valid pairing and live audio", async () => {
    const f = await browser({media: true});
    await f.click("start");
    f.intercept = request => request.path === "/api/workspace/open"
      ? response({error: "Core unavailable", code: "unavailable"}, 503) : undefined;
    await f.click("refreshWorkspace");
    assert.equal(f.storage.get(storageKey), "paired-old");
    assert.equal(f.el("talk").hidden, false);
    assert.equal(f.peers[0].closed, false);
    assert.equal(f.el("fleetList").children.length, 0);
    assert.match(f.el("workspaceError").textContent, /Core unavailable/);
    f.intercept = null;
    await f.click("refreshWorkspace");
    assert.equal(f.el("fleetList").textContent, "alpha");
  }],
  ...[401, 403].map(status => ["workspace " + status + " clears pairing, observations and microphone", async () => {
    const f = await browser({media: true});
    await f.el("fleetList").querySelectorAll("button")[0].dispatch("click");
    await f.click("start");
    f.intercept = request => request.path === "/api/workspace/open"
      ? response({error: "Access unavailable", code: "access_denied"}, status) : undefined;
    await f.click("refreshWorkspace");
    assert.equal(f.storage.has(storageKey), false);
    assert.equal(f.el("talk").hidden, true);
    assert.equal(f.el("pair").hidden, false);
    assert.equal(f.el("fleetList").children.length, 0);
    assert(!f.el("runtimeReport").textContent.includes("Checked alpha"));
    assert(f.peers[0].closed && f.tracks[0].stopped);
    assert.equal(f.el("remote").srcObject, null);
  }]),
];

let failed = 0;
for (const [name, run] of tests) {
  try { await run(); console.log("PASS " + name); }
  catch (error) { failed++; console.error("FAIL " + name); console.error(error); }
}
assert.equal(failed, 0, `${failed} scoped page behavior checks failed`);
console.log(`${tests.length} scoped page behavior checks passed (${fileURLToPath(page)}).`);
