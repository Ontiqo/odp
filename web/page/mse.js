// Playing H.264 through Media Source Extensions where WebCodecs isn't there: browsers keep WebCodecs to secure pages
// (https or localhost), and a page served by a computer on the network is neither; many TV browsers lack it anyway.
// Each access unit (Annex B, SPS and PPS in key frames) becomes one fragmented-MP4 sample for a <video>, and the
// video is held to the shared clock: frame times are on this computer's clock, so media time t plays at t0 + t.
"use strict";

const Mse = (() => {
  const u32 = (n) => [(n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255];
  const u16 = (n) => [(n >>> 8) & 255, n & 255];
  const str = (s) => [...s].map((c) => c.charCodeAt(0));
  function box(type, ...parts) {
    const body = parts.flatMap((p) => (p instanceof Uint8Array ? [...p] : p));
    return [...u32(body.length + 8), ...str(type), ...body];
  }
  const full = (type, version, flags, ...parts) => box(type, [version, ...u32(flags).slice(1)], ...parts);

  /** The NAL units of an Annex B access unit. */
  function nals(au) {
    const out = [];
    let start = -1;
    for (let i = 0; i + 2 < au.length; i++) {
      if (au[i] === 0 && au[i + 1] === 0 && au[i + 2] === 1) {
        if (start >= 0) out.push(au.subarray(start, i > 0 && au[i - 1] === 0 ? i - 1 : i));
        start = i + 3;
        i += 2;
      }
    }
    if (start >= 0) out.push(au.subarray(start));
    return out;
  }

  function init(sps, pps, w, h) {
    const avcC = box("avcC", [1, sps[1], sps[2], sps[3], 0xff, 0xe1, ...u16(sps.length)], sps, [1, ...u16(pps.length)], pps);
    const avc1 = box("avc1", [0, 0, 0, 0, 0, 0, 0, 1], new Array(16).fill(0), u16(w), u16(h),
      u32(0x00480000), u32(0x00480000), u32(0), u16(1), new Array(32).fill(0), u16(0x18), u16(0xffff), avcC);
    const stbl = box("stbl", full("stsd", 0, 0, u32(1), avc1), full("stts", 0, 0, u32(0)), full("stsc", 0, 0, u32(0)),
      full("stsz", 0, 0, u32(0), u32(0)), full("stco", 0, 0, u32(0)));
    const minf = box("minf", full("vmhd", 0, 1, new Array(8).fill(0)), box("dinf", full("dref", 0, 0, u32(1), full("url ", 0, 1))), stbl);
    const mdia = box("mdia", full("mdhd", 0, 0, u32(0), u32(0), u32(90000), u32(0), u16(0x55c4), u16(0)),
      full("hdlr", 0, 0, u32(0), str("vide"), new Array(12).fill(0), str("video"), [0]), minf);
    const matrix = [...u32(0x10000), ...u32(0), ...u32(0), ...u32(0), ...u32(0x10000), ...u32(0), ...u32(0), ...u32(0), ...u32(0x40000000)];
    const tkhd = full("tkhd", 0, 3, u32(0), u32(0), u32(1), u32(0), u32(0), u32(0), u32(0), u16(0), u16(0), u16(0), u16(0), matrix, u32(w << 16), u32(h << 16));
    const mvhd = full("mvhd", 0, 0, u32(0), u32(0), u32(1000), u32(0), u32(0x10000), u16(0x100), u16(0), u32(0), u32(0), matrix, new Array(24).fill(0), u32(2));
    const mvex = box("mvex", full("trex", 0, 0, u32(1), u32(1), u32(0), u32(0), u32(0)));
    return new Uint8Array([...box("ftyp", str("isom"), u32(0x200), str("isomiso2avc1mp41")), ...box("moov", mvhd, box("trak", tkhd, mdia), mvex)]);
  }

  function fragment(seq, time90k, duration, key, data) {
    const hi = Math.floor(time90k / 2 ** 32), lo = time90k >>> 0;
    const trun = (offset) => full("trun", 0, 0x000701, u32(1), u32(offset), u32(duration), u32(data.length), u32(key ? 0x02000000 : 0x01010000));
    const traf = (offset) => box("traf", full("tfhd", 0, 0x020000, u32(1)), full("tfdt", 1, 0, u32(hi), u32(lo)), trun(offset));
    const size = box("moof", full("mfhd", 0, 0, u32(seq)), traf(0)).length;
    const moof = box("moof", full("mfhd", 0, 0, u32(seq)), traf(size + 8));
    const out = new Uint8Array(moof.length + 8 + data.length);
    out.set(moof);
    out.set([...u32(data.length + 8), ...str("mdat")], moof.length);
    out.set(data, moof.length + 8);
    return out;
  }

  /** A player on `video`; `w`/`h` are the picture's size from session.start. */
  function create(video, onDelay) {
    let shown = [];
    let source = null, buffer = null, queue = [], spsKey = "", seq = 1, t0 = 0, last = 0, started = false;
    const pump = () => {
      if (!buffer || buffer.updating || !queue.length) return;
      try { buffer.appendBuffer(queue.shift()); } catch { reset(); }
    };
    function reset() {
      queue = []; spsKey = ""; buffer = null; started = false;
      if (source) { try { URL.revokeObjectURL(video.src); } catch {} }
      source = null;
      video.removeAttribute("src");
    }
    function open(cb) {
      source = new MediaSource();
      video.src = URL.createObjectURL(source);
      source.addEventListener("sourceopen", () => cb(), { once: true });
    }
    // Keeps the video at its time on the shared clock: a little faster or slower, or a jump when far off.
    setInterval(() => {
      if (!started || !video.buffered.length) return;
      const want = (Date.now() * 1000 - t0) / 1e6, off = video.currentTime - want;
      const start = video.buffered.start(0), end = video.buffered.end(video.buffered.length - 1);
      if (Math.abs(off) > 0.5 || video.readyState < 2) video.currentTime = Math.max(start, Math.min(want, end - 0.02));
      else video.playbackRate = off > 0.02 ? 0.95 : off < -0.02 ? 1.05 : 1;
      if (video.paused) video.play().catch(() => {});
      // The frame on screen now, and how long ago it was captured.
      let i = 0;
      while (i + 1 < shown.length && shown[i + 1][0] <= video.currentTime) i++;
      if (shown.length && shown[i][0] <= video.currentTime && shown[i][1] > 0) onDelay(Date.now() * 1000 - shown[i][1]);
      shown = shown.slice(i);
      // Keep the buffer short.
      if (buffer && !buffer.updating && video.currentTime > 10) {
        try { buffer.remove(0, video.currentTime - 5); } catch {}
      }
    }, 250);
    return {
      /** One access unit, to show at `showAtUs` (this computer's clock; 0: now). False when it can't be played. */
      frame(au, showAtUs, key, w, h, capturedUs) {
        const units = nals(au);
        const sps = units.find((n) => (n[0] & 0x1f) === 7), pps = units.find((n) => (n[0] & 0x1f) === 8);
        if (key && sps && pps) {
          const k = Array.from(sps).join(",") + "/" + w + "x" + h;
          if (k !== spsKey) {
            const codec = "avc1." + [sps[1], sps[2], sps[3]].map((b) => b.toString(16).padStart(2, "0")).join("");
            if (!window.MediaSource || !MediaSource.isTypeSupported(`video/mp4; codecs="${codec}"`)) return false;
            const first = !buffer;
            spsKey = k;
            queue.push(init(sps, pps, w, h));
            if (first) open(() => { buffer = source.addSourceBuffer(`video/mp4; codecs="${codec}"`); buffer.mode = "segments"; buffer.addEventListener("updateend", pump); pump(); });
          }
        }
        if (!spsKey) return true; // waiting for a key frame
        const body = units.filter((n) => { const t = n[0] & 0x1f; return t !== 7 && t !== 8 && t !== 9; });
        const data = new Uint8Array(body.reduce((s, n) => s + 4 + n.length, 0));
        let o = 0;
        for (const n of body) { data.set(u32(n.length), o); data.set(n, o + 4); o += 4 + n.length; }
        const at = showAtUs > 0 ? showAtUs : Date.now() * 1000 + 100000;
        if (!t0) t0 = at - 1000000; // media time starts at 1 s
        const t = Math.max(Math.round(((at - t0) / 1e6) * 90000), last + 1);
        queue.push(fragment(seq++, t, 3000, key, data));
        shown.push([t / 90000, capturedUs || 0]);
        last = t;
        started = true;
        pump();
        return true;
      },
      stop() { reset(); t0 = 0; last = 0; seq = 1; shown = []; },
    };
  }
  return { create, nals, init, fragment };
})();
