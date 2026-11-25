// An ODP screen in a browser (the web profile). The casting computer serves this page; it connects back over a
// WebSocket at /odp. Each binary message is one byte for the channel (0: L1 control bytes, 1: one L4 media
// datagram) and the bytes. The first message from the computer is text: {"udp_port": N, "name": "..."}.
// odp_web.wasm runs the protocol (pairing, encryption, clock, reassembly); this page decodes with WebCodecs.
"use strict";

const $ = (id) => document.getElementById(id);
const store = {
  get(k) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k, v) { try { localStorage.setItem(k, v); } catch {} },
};

let wasm, mem, ws, started = false, showing = false;
let decoder = null, codec = "", screenId = "";
const canvas = $("picture"), ctx = canvas.getContext("2d");
// Without WebCodecs (a page from a computer on the network isn't a secure page; many TV browsers): MSE on a <video>.
const webcodecs = "VideoDecoder" in window;
const mse = webcodecs ? null : Mse.create($("video"), (us) => delayed(us));
// The delay from capture to picture, reported every 5 s (odp sync.latency), so a group waits for this screen.
let delays = [], reported = 0;
function delayed(us) {
  if (us <= 0 || us > 5e6) return;
  delays.push(us);
  if (delays.length > 300) delays.splice(0, 150);
  if (Date.now() - reported >= 5000 && delays.length >= 10) {
    call(wasm.odp_latency, [...delays].sort((a, b) => a - b)[delays.length >> 1]);
    reported = Date.now();
  }
}
let size = [1920, 1080];
const captured = new Map();

// USB HID usages (page 7) for KeyboardEvent.code: what ODP sends for keys.
const HID = {};
"ABCDEFGHIJKLMNOPQRSTUVWXYZ".split("").forEach((c, i) => (HID["Key" + c] = 0x04 + i));
"1234567890".split("").forEach((c, i) => (HID["Digit" + c] = 0x1e + i));
Object.assign(HID, { Enter: 0x28, Escape: 0x29, Backspace: 0x2a, Tab: 0x2b, Space: 0x2c, Minus: 0x2d, Equal: 0x2e,
  BracketLeft: 0x2f, BracketRight: 0x30, Backslash: 0x31, Semicolon: 0x33, Quote: 0x34, Backquote: 0x35, Comma: 0x36,
  Period: 0x37, Slash: 0x38, CapsLock: 0x39, Delete: 0x4c, Home: 0x4a, End: 0x4d, PageUp: 0x4b, PageDown: 0x4e,
  ArrowRight: 0x4f, ArrowLeft: 0x50, ArrowDown: 0x51, ArrowUp: 0x52, ControlLeft: 0xe0, ShiftLeft: 0xe1, AltLeft: 0xe2,
  MetaLeft: 0xe3, ControlRight: 0xe4, ShiftRight: 0xe5, AltRight: 0xe6, MetaRight: 0xe7 });
for (let i = 1; i <= 12; i++) HID["F" + i] = 0x39 + i;

function bytesIn(u8) {
  const p = wasm.odp_alloc(u8.length);
  new Uint8Array(wasm.memory.buffer, p, u8.length).set(u8);
  return [p, u8.length];
}

function call(fn, ...args) {
  fn(...args);
  const p = wasm.odp_out(), n = wasm.odp_out_len();
  const out = new Uint8Array(wasm.memory.buffer, p, n).slice();
  const dv = new DataView(out.buffer);
  for (let i = 0; i + 5 <= out.length;) {
    const kind = out[i], len = dv.getUint32(i + 1, true), body = out.subarray(i + 5, i + 5 + len);
    i += 5 + len;
    if (kind === 0 || kind === 1) send(kind, body);
    else if (kind === 2) event(JSON.parse(new TextDecoder().decode(body)));
    else if (kind === 3) {
      const dv = new DataView(body.buffer, body.byteOffset);
      frame(body.subarray(17), Number(dv.getBigInt64(0, true)), body[8] === 1, Number(dv.getBigInt64(9, true)));
    }
    else if (kind === 4) sound(body.subarray(8), Number(new DataView(body.buffer, body.byteOffset).getBigInt64(0, true)));
  }
}

function withBytes(fn, u8, ...rest) {
  const [p, n] = bytesIn(u8);
  call(fn, p, n, ...rest);
  wasm.odp_free(p, n);
}

function send(channel, body) {
  if (!ws || ws.readyState !== 1) return;
  const m = new Uint8Array(body.length + 1);
  m[0] = channel;
  m.set(body, 1);
  ws.send(m);
}

function showWaiting(pin) {
  $("waiting").style.display = "flex";
  $("how").textContent = pin ? "On the computer: Omninput, Screens, Add a screen, this screen. Type this PIN there:"
                             : "Press Enter (OK on the remote) to show a PIN to pair with a computer.";
  $("pin").textContent = pin ? pin.slice(0, 3) + " " + pin.slice(3) : "";
}

function event(e) {
  switch (e.t) {
    case "started_receiver":
      store.set("odp.secret", e.secret);
      screenId = e.id;
      break;
    case "pin":
      if (!showing) showWaiting(e.pin);
      break;
    case "paired": {
      const known = JSON.parse(store.get("odp.known") || "[]").filter((k) => k[0] !== e.id);
      known.push([e.id, e.key]);
      store.set("odp.known", JSON.stringify(known));
      break;
    }
    case "started":
      showing = true;
      if (e.w && e.h) size = [e.w, e.h];
      $("waiting").style.display = "none";
      call(wasm.odp_decoder_failed);
      break;
    case "stopped":
      showing = false;
      if (decoder) { try { decoder.close(); } catch {} decoder = null; codec = ""; }
      if (mse) mse.stop();
      showWaiting(null);
      break;
    case "problem":
      console.warn("odp:", e.text);
      break;
  }
}

// avc1.PPCCLL from the SPS in a key frame (profile, constraints, level).
function codecOf(au) {
  for (let i = 0; i + 7 < au.length; i++) {
    if (au[i] === 0 && au[i + 1] === 0 && au[i + 2] === 1 && (au[i + 3] & 0x1f) === 7) {
      const h = (b) => b.toString(16).padStart(2, "0");
      return "avc1." + h(au[i + 4]) + h(au[i + 5]) + h(au[i + 6]);
    }
  }
  return null;
}

function frame(au, showAtUs, key, capturedUs) {
  if (mse) {
    if (!mse.frame(au, showAtUs, key, size[0], size[1], capturedUs)) $("how").textContent = "This browser can't play the cast's video.";
    return;
  }
  if (key) {
    const c = codecOf(au);
    if (c && c !== codec) {
      if (decoder) { try { decoder.close(); } catch {} }
      codec = c;
      decoder = new VideoDecoder({
        output: (vf) => {
          // Each frame's own times: its timestamp is when to show it, and `captured` keeps when it was captured.
          const showAt = vf.timestamp, cap = captured.get(showAt) || 0;
          captured.delete(showAt);
          const draw = () => {
            if (canvas.width !== vf.displayWidth) { canvas.width = vf.displayWidth; canvas.height = vf.displayHeight; }
            ctx.drawImage(vf, 0, 0);
            vf.close();
            if (cap > 0) delayed(Date.now() * 1000 - cap);
          };
          // At its time on the shared clock (late frames at once).
          const wait = showAt / 1000 - Date.now();
          if (wait > 0 && wait < 1000) setTimeout(draw, wait); else draw();
        },
        error: () => { decoder = null; codec = ""; call(wasm.odp_decoder_failed); },
      });
      // No description: the stream is Annex B, with SPS and PPS in each key frame.
      decoder.configure({ codec, optimizeForLatency: true });
    }
  }
  if (!decoder) return;
  const at = showAtUs || Date.now() * 1000;
  if (capturedUs > 0) { captured.set(at, capturedUs); if (captured.size > 120) captured.delete(captured.keys().next().value); }
  try {
    decoder.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: at, data: au }));
  } catch {
    call(wasm.odp_decoder_failed);
  }
}

// The cast's sound (16-bit PCM, 48 kHz stereo, 10 ms a frame), each frame at its time. Browsers allow sound only
// after a key press or a click, which starts it.
let audio = null, audioNext = 0;
function startSound() {
  if (!audio) audio = new AudioContext({ sampleRate: 48000 });
  if (audio.state !== "running") audio.resume();
}
addEventListener("click", startSound);
function sound(pcm, playAtUs) {
  if (!audio || audio.state !== "running") return;
  const n = pcm.length / 4;
  const buf = audio.createBuffer(2, n, 48000);
  const l = buf.getChannelData(0), r = buf.getChannelData(1);
  const dv = new DataView(pcm.buffer, pcm.byteOffset, pcm.length);
  for (let i = 0; i < n; i++) { l[i] = dv.getInt16(i * 4, true) / 32768; r[i] = dv.getInt16(i * 4 + 2, true) / 32768; }
  const ahead = playAtUs > 0 ? (playAtUs / 1000 - Date.now()) / 1000 : 0.04;
  if (ahead < -0.1) return; // too late to be in step
  let when = audio.currentTime + Math.max(ahead, 0);
  if (when < audioNext && audioNext - when < 0.05) when = audioNext; // back to back, no clicks
  const src = audio.createBufferSource();
  src.buffer = buf;
  src.connect(audio.destination);
  src.start(when);
  audioNext = when + n / 48000;
}

function connect(first) {
  ws = new WebSocket((location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/odp");
  ws.binaryType = "arraybuffer";
  let hello = false;
  ws.onmessage = (m) => {
    if (typeof m.data === "string" && hello) {
      // A new control connection from the casting computer: the handshake starts again.
      if (JSON.parse(m.data).t === "connected") call(wasm.odp_connected);
      return;
    }
    if (!hello) {
      hello = true;
      const cfg = JSON.parse(m.data);
      if (!started) {
        started = true;
        const name = store.get("odp.name") || cfg.name || "Browser";
        $("name").textContent = name;
        const caps = { decode: [{ codec: "h264", max_w: 1920, max_h: 1080, max_fps: 60, hw: true }], keyboard: true, audio_channels: 2 };
        const secret = store.get("odp.secret");
        const s = secret ? Uint8Array.from(atob(secret), (c) => c.charCodeAt(0)) : new Uint8Array(0);
        const enc = new TextEncoder();
        const parts = [enc.encode(name), enc.encode(JSON.stringify(caps)), s, enc.encode(store.get("odp.known") || "[]")].map(bytesIn);
        call(wasm.odp_start, parts[0][0], parts[0][1], parts[1][0], parts[1][1], parts[2][0], parts[2][1], parts[3][0], parts[3][1], cfg.udp_port);
        parts.forEach(([p, n]) => wasm.odp_free(p, n));
        if (!store.get("odp.known")) call(wasm.odp_pairing, 1);
        else showWaiting(null);
      }
      // This connection's media port (a new one each time the page connects).
      call(wasm.odp_udp_port, cfg.udp_port);
      // Which screen this is, so a computer it's paired with finds it at this connection.
      if (screenId) ws.send(JSON.stringify({ id: screenId }));
      return;
    }
    const d = new Uint8Array(m.data);
    if (d[0] === 0) withBytes(wasm.odp_control, d.subarray(1), Date.now());
    else if (d[0] === 1) withBytes(wasm.odp_media, d.subarray(1), Date.now());
  };
  ws.onclose = () => setTimeout(() => connect(false), 2000);
}

addEventListener("keydown", (e) => {
  startSound();
  if (!showing) {
    if (e.code === "Enter" || e.keyCode === 13) call(wasm.odp_pairing, 1);
    return;
  }
  const u = HID[e.code];
  if (u) { call(wasm.odp_key, u, 1, (e.ctrlKey ? 1 : 0) | (e.shiftKey ? 2 : 0) | (e.altKey ? 4 : 0) | (e.metaKey ? 8 : 0)); e.preventDefault(); }
});
addEventListener("keyup", (e) => {
  const u = HID[e.code];
  if (showing && u) { call(wasm.odp_key, u, 0, 0); e.preventDefault(); }
});

(async () => {
  if (!webcodecs && !window.MediaSource) { $("how").textContent = "This browser can't play the cast's video (no WebCodecs or Media Source)."; return; }
  $(webcodecs ? "video" : "picture").style.display = "none";
  const env = { odp_random: (p, n) => crypto.getRandomValues(new Uint8Array(wasm.memory.buffer, p, n)) };
  const { instance } = await WebAssembly.instantiateStreaming(fetch("odp_web.wasm"), { env });
  wasm = instance.exports;
  setInterval(() => { if (showing) call(wasm.odp_ping, Date.now()); }, 1000);
  connect(true);
})();
