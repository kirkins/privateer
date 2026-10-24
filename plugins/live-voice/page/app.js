// The live voice page: pair once, then a WebRTC call whose offer the door
// hands to Codex. Audio flows between this page and OpenAI directly; the door
// only ever returns the SDP answer. Everything else is the door's.
"use strict";

const $ = (id) => document.getElementById(id);
let token = localStorage.getItem("privateer-voice-token") || "";
let pc = null;
let mic = null;
let speaking = false;
// The draft card's draft, so a click names exactly the one the captain saw.
let shown = null;

async function api(path, body) {
  const options = { headers: { "X-Privateer-Token": token } };
  if (body !== undefined) {
    options.method = "POST";
    options.headers["Content-Type"] = "application/json";
    options.body = JSON.stringify(body);
  }
  const response = await fetch(path, options);
  const data = await response.json().catch(() => ({}));
  if (response.status === 401) showPairing();
  if (!response.ok) throw new Error(data.error || response.statusText);
  return data;
}

function showPairing() {
  token = "";
  localStorage.removeItem("privateer-voice-token");
  $("pair").hidden = false;
  $("talk").hidden = true;
}

async function pair() {
  try {
    const data = await api("/api/pair", { code: $("code").value });
    token = data.token;
    localStorage.setItem("privateer-voice-token", token);
    $("pair").hidden = true;
    $("talk").hidden = false;
  } catch (e) {
    alert("Not paired: " + e.message);
  }
}

// Barge-in is the server's voice activity detection; there is no client
// interrupt, so the page stops its own playback while the captain speaks.
function watchMic(stream) {
  const context = new AudioContext();
  const analyser = context.createAnalyser();
  context.createMediaStreamSource(stream).connect(analyser);
  const samples = new Uint8Array(analyser.fftSize);
  const tick = () => {
    if (!pc) { context.close(); return; }
    analyser.getByteTimeDomainData(samples);
    let peak = 0;
    for (const s of samples) peak = Math.max(peak, Math.abs(s - 128));
    const loud = peak > 24 && mic && mic.enabled;
    if (loud !== speaking) {
      speaking = loud;
      $("remote").muted = loud;
    }
    requestAnimationFrame(tick);
  };
  tick();
}

async function start() {
  $("start").disabled = true;
  $("error").textContent = "";
  try {
    const state = await api("/api/state");
    if (state.phase !== "idle") await api("/api/session/stop", {});
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    mic = stream.getAudioTracks()[0];
    pc = new RTCPeerConnection();
    pc.addTrack(mic, stream);
    pc.createDataChannel("oai-events");
    pc.ontrack = (event) => { $("remote").srcObject = event.streams[0]; };
    pc.onconnectionstatechange = () => {
      if (pc && pc.connectionState === "connected") api("/api/session/connected", {}).catch(() => {});
      if (pc && ["failed", "closed"].includes(pc.connectionState)) stop();
    };
    await pc.setLocalDescription(await pc.createOffer());
    // Send the offer once ICE gathering is done, as the Codex voice host does.
    await new Promise((resolve) => {
      if (pc.iceGatheringState === "complete") return resolve();
      const done = () => { if (pc.iceGatheringState === "complete") resolve(); };
      pc.addEventListener("icegatheringstatechange", done);
      setTimeout(resolve, 3000);
    });
    const { answer } = await api("/api/session/start", {
      offer: pc.localDescription.sdp,
      client: { name: "Privateer page", screen: true },
    });
    await pc.setRemoteDescription({ type: "answer", sdp: answer });
    $("stop").disabled = false;
    $("mute").disabled = false;
  } catch (e) {
    $("error").textContent = e.message;
    await stop();
  }
}

async function stop() {
  const had = pc;
  pc = null;
  if (had) had.close();
  if (mic) mic.stop();
  mic = null;
  $("remote").srcObject = null;
  $("start").disabled = false;
  $("stop").disabled = true;
  $("mute").disabled = true;
  $("mute").textContent = "Mute";
  await api("/api/session/stop", {}).catch(() => {});
}

function mute() {
  if (!mic) return;
  mic.enabled = !mic.enabled;
  $("mute").textContent = mic.enabled ? "Mute" : "Unmute";
}

// The door stops a session whose page stops polling (a closed tab, a
// crash); closing the tab also asks it to stop at once, best effort.
window.addEventListener("pagehide", () => {
  if (!pc || !token) return;
  fetch("/api/session/stop", {
    method: "POST", keepalive: true, body: "{}",
    headers: { "X-Privateer-Token": token, "Content-Type": "application/json" },
  }).catch(() => {});
});

// Every second: the page's view, and the heartbeat that keeps a session up.
async function poll() {
  if (token) {
    try {
      const state = await api("/api/state");
      $("status").textContent = state.phase;
      $("queued").textContent = state.queued;
      if (state.error) $("error").textContent = state.error;
      // The codex version check (feature codex_version): what was found and
      // what to do, when the door could not read it or it is out of range.
      // A refused start already shows the same sentence in the error box.
      const codexMessage = (state.codex && state.codex.message) || "";
      $("codex").textContent = codexMessage === $("error").textContent ? "" : codexMessage;
      shown = state.draft;
      $("draft").style.display = shown ? "block" : "none";
      // The door's own read-back: the recipient is what the door will use,
      // whatever the voice said aloud.
      $("draftText").textContent = shown ? `To ${shown.to}. ${shown.readback}` : "";
      $("draftSend").textContent = shown ? `Send to ${shown.to}` : "";
      const box = $("transcript");
      box.replaceChildren(...state.transcript.map((line) => {
        const p = document.createElement("p");
        p.className = line.role;
        p.textContent = `${line.role}: ${line.text}`;
        return p;
      }));
    } catch (e) { /* shown on the next action */ }
  }
  setTimeout(poll, 1000);
}

// The click names the draft it saw; the door sends only if that is still
// the one waiting, and answers with the true outcome.
async function draftAction(path) {
  if (!shown) return;
  const body = path === "/api/draft/confirm" ? { id: shown.id, to: shown.to } : { id: shown.id };
  try {
    const result = await api(path, body);
    $("outcome").textContent = result.outcome;
  } catch (e) {
    $("outcome").textContent = e.message;
  }
}

$("pairButton").addEventListener("click", pair);
$("draftSend").addEventListener("click", () => draftAction("/api/draft/confirm"));
$("draftCancel").addEventListener("click", () => draftAction("/api/draft/cancel"));
$("start").addEventListener("click", start);
$("stop").addEventListener("click", stop);
$("mute").addEventListener("click", mute);
if (token) { $("talk").hidden = false; } else { showPairing(); }
poll();
