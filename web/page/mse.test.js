// node mse.test.js: the fragmented-MP4 boxes mse.js writes nest with the right sizes and carry the sample.
"use strict";
const assert = require("assert");
global.window = {};
const Mse = new Function(require("fs").readFileSync(__dirname + "/mse.js", "utf8") + "; return Mse;")();

function walk(b, start, end, depth, out) {
  for (let i = start; i < end;) {
    const size = (b[i] << 24 | b[i + 1] << 16 | b[i + 2] << 8 | b[i + 3]) >>> 0, type = String.fromCharCode(...b.subarray(i + 4, i + 8));
    assert(size >= 8 && i + size <= end, `${type} at ${i}: size ${size} past ${end}`);
    out.push("  ".repeat(depth) + type);
    const kids = { moov: 8, trak: 8, mdia: 8, minf: 8, dinf: 8, stbl: 8, mvex: 8, moof: 8, traf: 8, stsd: 16, avc1: 86, dref: 16 }[type];
    if (kids) walk(b, i + kids, i + size, depth + 1, out);
    i += size;
  }
  return out;
}

const sps = Uint8Array.from([0x67, 0x64, 0x00, 0x28, 0xac, 0xd9]), pps = Uint8Array.from([0x68, 0xeb, 0xe3]);
const au = Uint8Array.from([0, 0, 0, 1, ...sps, 0, 0, 0, 1, ...pps, 0, 0, 1, 0x65, 1, 2, 3]);
const units = Mse.nals(au);
assert.deepStrictEqual(units.map((n) => n[0]), [0x67, 0x68, 0x65]);
assert.deepStrictEqual([...units[2]], [0x65, 1, 2, 3]);

const init = Mse.init(sps, pps, 1280, 720);
const boxes = walk(init, 0, init.length, 0, []);
for (const t of ["ftyp", "moov", "mvhd", "tkhd", "mdhd", "hdlr", "stsd", "avc1", "avcC", "trex"]) assert(boxes.some((b) => b.trim() === t), "no " + t);

const data = Uint8Array.from([0, 0, 0, 4, 0x65, 1, 2, 3]);
const frag = Mse.fragment(7, 2 ** 33 + 5, 3000, true, data);
const fb = walk(frag, 0, frag.length, 0, []);
assert.deepStrictEqual(fb.map((b) => b.trim()), ["moof", "mfhd", "traf", "tfhd", "tfdt", "trun", "mdat"]);
// trun's data offset points at the sample inside mdat.
const trun = frag.indexOf(0x74, 0) && [...frag].findIndex((_, i) => String.fromCharCode(...frag.subarray(i, i + 4)) === "trun");
const off = (frag[trun + 12] << 24 | frag[trun + 13] << 16 | frag[trun + 14] << 8 | frag[trun + 15]) >>> 0;
assert.deepStrictEqual([...frag.subarray(off, off + data.length)], [...data]);
console.log("mse ok:", boxes.length, "init boxes,", fb.length, "fragment boxes");
