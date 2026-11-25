// Narration with Piper. Each sentence is synthesised separately and the clips are
// joined with a fixed gap, which gives exact per-sentence timings (for subtitles and
// for `sentence(i)` in scene actions). Runs one `piper` process for everything; the
// model's memory is freed before Chromium starts.
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { spawnSync, execFileSync } from 'node:child_process';

export const RATE = 48000; // everything is resampled to 48 kHz mono

export function sentences(text) {
  return (text.match(/[^.!?]+[.!?]+["')\]]*|[^.!?]+$/g) ?? []).map((s) => s.trim()).filter(Boolean);
}

function decode(file) {
  const buf = execFileSync('ffmpeg', ['-v', 'error', '-i', file, '-f', 'f32le', '-ac', '1', '-ar', String(RATE), '-'],
    { maxBuffer: 1 << 30 });
  return new Float32Array(buf.buffer, buf.byteOffset, buf.length / 4);
}

// Trim near-silence from both ends (Piper pads unevenly), keeping a small margin.
function trim(x, thresh = 0.01, keep = 0.03) {
  let a = 0, b = x.length - 1;
  while (a < b && Math.abs(x[a]) < thresh) a++;
  while (b > a && Math.abs(x[b]) < thresh) b--;
  const k = Math.round(keep * RATE);
  return x.subarray(Math.max(0, a - k), Math.min(x.length, b + k + 1));
}

function writeWav(file, x) {
  const data = Buffer.alloc(x.length * 2);
  for (let i = 0; i < x.length; i++) data.writeInt16LE(Math.max(-32768, Math.min(32767, Math.round(x[i] * 32767))), i * 2);
  const h = Buffer.alloc(44);
  h.write('RIFF', 0); h.writeUInt32LE(36 + data.length, 4); h.write('WAVE', 8);
  h.write('fmt ', 12); h.writeUInt32LE(16, 16); h.writeUInt16LE(1, 20); h.writeUInt16LE(1, 22);
  h.writeUInt32LE(RATE, 24); h.writeUInt32LE(RATE * 2, 28); h.writeUInt16LE(2, 32); h.writeUInt16LE(16, 34);
  h.write('data', 36); h.writeUInt32LE(data.length, 40);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, Buffer.concat([h, data]));
}

/**
 * items: [{ text, out }]  ->  [{ out, audioSec, segments: [{ text, start, end }] }]
 * options: { voice (path to .onnx), lengthScale, gap (s between sentences) }
 */
export function synthesise(items, { voice, lengthScale, gap = 0.3 } = {}) {
  if (!voice) throw new Error('No Piper voice: pass --voice <model.onnx> or set DEMO_VIDEO_PIPER_VOICE (gavmo.nix does).');
  if (!fs.existsSync(voice)) throw new Error(`Piper voice not found: ${voice}`);
  const all = items.flatMap((it, i) => sentences(it.text).map((text) => ({ i, text })));
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'piper-'));
  // One input line -> one wav file in `dir`, named by timestamp (sorting restores order).
  const args = ['-m', voice, '-d', dir];
  if (lengthScale) args.push('--length-scale', String(lengthScale));
  const t = performance.now();
  const r = spawnSync('piper', args, {
    input: all.map((s) => s.text.replace(/\s+/g, ' ')).join('\n') + '\n',
    encoding: 'utf8',
    maxBuffer: 1 << 26,
  });
  if (r.error) throw new Error(`could not run piper: ${r.error.message}`);
  if (r.status !== 0) throw new Error(`piper failed (${r.status}):\n${r.stderr.slice(-2000)}`);
  const files = fs.readdirSync(dir).filter((f) => f.endsWith('.wav')).sort();
  if (files.length !== all.length) throw new Error(`piper wrote ${files.length} files for ${all.length} sentences`);
  all.forEach((s, k) => { s.audio = trim(decode(path.join(dir, files[k]))); });
  fs.rmSync(dir, { recursive: true });

  const g = Math.round(gap * RATE);
  const results = items.map((it, i) => {
    const clips = all.filter((s) => s.i === i);
    const total = clips.reduce((n, c) => n + c.audio.length, 0) + g * Math.max(0, clips.length - 1);
    const buf = new Float32Array(total);
    const segments = [];
    let off = 0;
    clips.forEach((c, k) => {
      if (k) off += g;
      buf.set(c.audio, off);
      segments.push({ text: c.text, start: off / RATE, end: (off + c.audio.length) / RATE });
      off += c.audio.length;
    });
    writeWav(it.out, buf);
    return { out: it.out, audioSec: total / RATE, segments };
  });
  return { items: results, synthMs: Math.round(performance.now() - t) };
}
