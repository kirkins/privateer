// Protocol 2 only. No v1 token import, fallback, caller-authored body/key or recovery.
"use strict";
import {createWorkspace} from "./workspace.scoped.js";
const $ = id => document.getElementById(id);
const storageKey = "privateer-voice-scoped-token";
let token = "", sessionId = null, pc = null, mic = null, ready = false, shown = null;
let authGeneration = 0, audioGeneration = 0;
const workspace = createWorkspace(api);

function clearAudio() {
  audioGeneration++;
  const old = pc;
  pc = null;
  if (old) old.close();
  if (mic) mic.stop();
  mic = null;
  $("remote").srcObject = null;
  $("start").disabled = false;
  $("stop").disabled = true;
  $("mute").disabled = true;
  $("mute").textContent = "Mute";
}
function forget() {
  authGeneration++;
  token = ""; sessionId = null; shown = null;
  workspace.reset();
  localStorage.removeItem(storageKey);
  clearAudio();
  for (const id of ["error", "codex", "draftText", "outcome", "status", "queued"])
    $(id).textContent = "";
  $("transcript").replaceChildren();
  $("draft").style.display = "none";
  $("code").value = "";
  $("pair").hidden = false; $("talk").hidden = true;
}
async function api(path, data, workspaceRequest = false, currentAudio = null) {
  if (!ready) throw new Error("Scoped protocol not verified");
  const generation = authGeneration, credential = token;
  const options = {headers: {"X-Privateer-Token": token}};
  if (data !== undefined) {
    options.method = "POST";
    options.headers["Content-Type"] = "application/json";
    options.body = JSON.stringify(data);
  }
  const response = await fetch(path, options);
  const body = await response.json();
  if (generation !== authGeneration || credential !== token)
    throw new Error("Device connection changed. Refresh before continuing.");
  if (!response.ok && currentAudio && !currentAudio())
    throw new Error("Request belonged to an earlier voice session.");
  if ([401, 403].includes(response.status) || (response.status === 503 && !workspaceRequest)) forget();
  if (!response.ok) throw new Error(body.error || "Request refused");
  return body;
}
async function pair() {
  $("pairButton").disabled = true;
  $("pairError").textContent = "";
  try {
    const result = await api("/api/pair", {code: $("code").value});
    forget();
    token = result.token;
    localStorage.setItem(storageKey, token);
    $("code").value = "";
    $("pair").hidden = true; $("talk").hidden = false;
    workspace.refresh();
  } catch (e) { $("code").value = ""; $("pairError").textContent = e.message; }
  finally { $("pairButton").disabled = !ready; }
}
async function stop() {
  const old = sessionId; sessionId = null;
  clearAudio();
  const stoppedAudio = audioGeneration;
  if (old) await api("/api/session/stop", {session_id: old}, false,
    () => stoppedAudio === audioGeneration).catch(() => {});
}
async function start() {
  $("start").disabled = true;
  let attempt;
  try {
    if (sessionId) await stop();
    $("start").disabled = true;
    attempt = ++audioGeneration;
    const authentication = authGeneration;
    const stream = await navigator.mediaDevices.getUserMedia({audio: true});
    if (attempt !== audioGeneration || authentication !== authGeneration || !token) {
      for (const track of stream.getTracks()) track.stop();
      return;
    }
    mic = stream.getAudioTracks()[0];
    const peer = new RTCPeerConnection(); pc = peer;
    const ownsAudio = () => pc === peer && attempt === audioGeneration && authentication === authGeneration;
    peer.addTrack(mic, stream); peer.createDataChannel("oai-events");
    peer.ontrack = event => { if (pc === peer) $("remote").srcObject = event.streams[0]; };
    peer.onconnectionstatechange = () => {
      if (pc !== peer) return;
      if (peer.connectionState === "connected") {
        const connectedSession = sessionId;
        api("/api/session/connected", {session_id: connectedSession}, false,
          () => ownsAudio() && sessionId === connectedSession).catch(() => {
          if (ownsAudio() && sessionId === connectedSession) stop();
        });
      }
      if (["failed", "closed"].includes(peer.connectionState)) stop();
    };
    await peer.setLocalDescription(await peer.createOffer());
    if (!ownsAudio()) return;
    await new Promise(resolve => {
      if (peer.iceGatheringState === "complete") return resolve();
      peer.addEventListener("icegatheringstatechange", () => {
        if (peer.iceGatheringState === "complete") resolve();
      });
      setTimeout(resolve, 3000);
    });
    if (!ownsAudio()) return;
    const result = await api("/api/session/start", {offer: peer.localDescription.sdp,
      client: {name: "Privateer page", screen: true}}, false, ownsAudio);
    if (!ownsAudio()) {
      await api("/api/session/stop", {session_id: result.session_id}, false, ownsAudio).catch(() => {});
      return;
    }
    sessionId = result.session_id;
    await peer.setRemoteDescription({type: "answer", sdp: result.answer});
    if (!ownsAudio()) return;
    $("stop").disabled = false; $("mute").disabled = false;
  } catch (e) {
    if (attempt === undefined || attempt === audioGeneration) {
      await stop(); $("error").textContent = e.message;
    }
  }
}
async function poll() {
  if (token && ready) {
    try {
      const observedAudio = audioGeneration;
      const state = await api("/api/state", undefined, false,
        () => observedAudio === audioGeneration);
      if (observedAudio !== audioGeneration) {
        setTimeout(poll, 1000);
        return;
      }
      $("status").textContent = state.phase;
      $("queued").textContent = state.warning;
      $("codex").textContent = state.codex.message || "";
      shown = state.draft ? {...state.draft, session_id: state.session_id} : null;
      $("draft").style.display = shown ? "block" : "none";
      $("draftText").textContent = shown ? shown.readback : "";
      $("draftSend").textContent = shown ? `${shown.state === "unknown" ? "Retry same request to" : "Send to"} ${shown.to}` : "";
      $("draftCancel").disabled = Boolean(shown && shown.state === "unknown");
      $("outcome").textContent = state.outcome || "";
      if (sessionId && state.session_id !== sessionId) { clearAudio(); sessionId = null; }
      $("transcript").replaceChildren(...state.transcript.map(line => {
        const p = document.createElement("p"); p.textContent = `${line.role}: ${line.text}`;
        return p;
      }));
    } catch (_) { /* auth errors already clear protected display and microphone */ }
  }
  setTimeout(poll, 1000);
}
async function draftAction(confirm) {
  const draft = shown;
  if (!draft || !draft.session_id) return;
  const draftAudio = audioGeneration;
  const current = () => draftAudio === audioGeneration && sessionId === draft.session_id;
  const body = {session_id: draft.session_id, id: draft.id};
  if (confirm) body.to = draft.to;
  try {
    const result = await api(confirm ? "/api/draft/confirm" : "/api/draft/cancel", body, false, current);
    if (current()) $("outcome").textContent = result.outcome;
  } catch (e) { if (current()) $("outcome").textContent = e.message; }
}
$("draftSend").addEventListener("click", () => draftAction(true));
$("draftCancel").addEventListener("click", () => draftAction(false));
$("pairButton").addEventListener("click", pair);
$("start").addEventListener("click", start);
$("stop").addEventListener("click", stop);
$("mute").addEventListener("click", () => {
  if (mic) { mic.enabled = !mic.enabled; $("mute").textContent = mic.enabled ? "Mute" : "Unmute"; }
});
window.addEventListener("pagehide", () => {
  if (token && sessionId) fetch("/api/session/stop", {method: "POST", keepalive: true,
    headers: {"X-Privateer-Token": token, "Content-Type": "application/json"},
    body: JSON.stringify({session_id: sessionId})}).catch(() => {});
  if (token && workspace.id) fetch("/api/workspace/close", {method: "POST", keepalive: true,
    headers: {"X-Privateer-Token": token, "Content-Type": "application/json"},
    body: JSON.stringify({workspace_id: workspace.id})}).catch(() => {});
  workspace.reset();
  clearAudio();
});
(async () => {
  try {
    const version = await (await fetch("/api/version")).json();
    if (version.protocol !== 2 || version.access_mode !== "scoped") throw new Error("Protocol mismatch");
    ready = true;
    workspace.configure(version.features);
    token = localStorage.getItem(storageKey) || "";
    $("draft").style.display = "none";
    $("pair").hidden = Boolean(token); $("talk").hidden = !token;
    if (token) workspace.refresh();
    poll();
  } catch (_) {
    ready = false; forget(); $("pairButton").disabled = true;
    $("pairError").textContent = "Could not verify the scoped door protocol. Reload after checking the connection.";
  }
})();
