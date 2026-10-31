// Protocol 2 only. No v1 token import, fallback, draft or send controls.
"use strict";
const $ = id => document.getElementById(id);
const storageKey = "privateer-voice-scoped-token";
let token = "", sessionId = null, pc = null, mic = null, ready = false;

function clearAudio() {
  const old = pc;
  pc = null;
  if (old) old.close();
  if (mic) mic.stop();
  mic = null;
  $("remote").srcObject = null;
  $("start").disabled = false;
  $("stop").disabled = true;
  $("mute").disabled = true;
}
function forget() {
  token = ""; sessionId = null;
  localStorage.removeItem(storageKey);
  clearAudio();
  for (const id of ["error", "codex", "draftText", "outcome", "status", "queued"])
    $(id).textContent = "";
  $("transcript").replaceChildren();
  $("draft").style.display = "none";
  $("code").value = "";
  $("pair").hidden = false; $("talk").hidden = true;
}
async function api(path, data) {
  if (!ready) throw new Error("Scoped protocol not verified");
  const options = {headers: {"X-Privateer-Token": token}};
  if (data !== undefined) {
    options.method = "POST";
    options.headers["Content-Type"] = "application/json";
    options.body = JSON.stringify(data);
  }
  const response = await fetch(path, options);
  const body = await response.json();
  if ([401, 403, 503].includes(response.status)) forget();
  if (!response.ok) throw new Error(body.error || "Request refused");
  return body;
}
async function pair() {
  try {
    const result = await api("/api/pair", {code: $("code").value});
    clearAudio(); sessionId = null;
    token = result.token;
    localStorage.setItem(storageKey, token);
    $("code").value = "";
    $("pair").hidden = true; $("talk").hidden = false;
  } catch (e) { $("code").value = ""; alert(e.message); }
}
async function stop() {
  const old = sessionId; sessionId = null;
  clearAudio();
  if (old) await api("/api/session/stop", {session_id: old}).catch(() => {});
}
async function start() {
  $("start").disabled = true;
  try {
    if (sessionId) await stop();
    const stream = await navigator.mediaDevices.getUserMedia({audio: true});
    mic = stream.getAudioTracks()[0];
    const peer = new RTCPeerConnection(); pc = peer;
    peer.addTrack(mic, stream); peer.createDataChannel("oai-events");
    peer.ontrack = event => { if (pc === peer) $("remote").srcObject = event.streams[0]; };
    peer.onconnectionstatechange = () => {
      if (pc !== peer) return;
      if (peer.connectionState === "connected")
        api("/api/session/connected", {session_id: sessionId}).catch(() => stop());
      if (["failed", "closed"].includes(peer.connectionState)) stop();
    };
    await peer.setLocalDescription(await peer.createOffer());
    await new Promise(resolve => {
      if (peer.iceGatheringState === "complete") return resolve();
      peer.addEventListener("icegatheringstatechange", () => {
        if (peer.iceGatheringState === "complete") resolve();
      });
      setTimeout(resolve, 3000);
    });
    const result = await api("/api/session/start", {offer: peer.localDescription.sdp,
      client: {name: "Privateer page", screen: true}});
    if (pc !== peer) {
      await api("/api/session/stop", {session_id: result.session_id});
      return;
    }
    sessionId = result.session_id;
    await peer.setRemoteDescription({type: "answer", sdp: result.answer});
    $("stop").disabled = false; $("mute").disabled = false;
  } catch (e) { await stop(); $("error").textContent = e.message; }
}
async function poll() {
  if (token && ready) {
    try {
      const state = await api("/api/state");
      $("status").textContent = state.phase;
      $("queued").textContent = state.warning;
      $("codex").textContent = state.codex.message || "";
      if (sessionId && state.session_id !== sessionId) { clearAudio(); sessionId = null; }
      $("transcript").replaceChildren(...state.transcript.map(line => {
        const p = document.createElement("p"); p.textContent = `${line.role}: ${line.text}`;
        return p;
      }));
    } catch (_) { /* auth errors already clear protected display and microphone */ }
  }
  setTimeout(poll, 1000);
}
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
  clearAudio();
});
(async () => {
  try {
    const version = await (await fetch("/api/version")).json();
    if (version.protocol !== 2 || version.access_mode !== "scoped") throw new Error("Protocol mismatch");
    ready = true;
    token = localStorage.getItem(storageKey) || "";
    $("draft").style.display = "none";
    $("pair").hidden = Boolean(token); $("talk").hidden = !token;
    poll();
  } catch (_) { forget(); $("pairButton").disabled = true; }
})();
