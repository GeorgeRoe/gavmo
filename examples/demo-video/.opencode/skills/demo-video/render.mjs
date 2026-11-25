#!/usr/bin/env node
// Render a narrated demo video from a scene script.
//
//   node .opencode/skills/demo-video/render.mjs <scenes.mjs> [options]
//
// Options:
//   --name <name>          output base name (default: scene file name)
//   --out <dir>            output folder (default: demo-videos)
//   --voice <model.onnx>   Piper voice (default: $DEMO_VIDEO_PIPER_VOICE, set by gavmo.nix)
//   --length-scale <x>     speech speed: >1 slower, <1 faster (default: voice's own pace)
//   --gap <s>              pause between sentences (default 0.3)
//   --width/--height/--fps (default 1920 x 1080 at 25)
//   --storage-state <file> Playwright storageState JSON (signed-in session)
//   --keep-work            keep the raw frames/audio in ~/.cache/demo-video/work/<name>
//
// Output: <out>/<name>.mp4 (H.264/AAC, chapters, English subtitle track, off by default),
//         and <out>/<name>/ with contact.png, stills/ and report.json for checking.
//
// How it works: 1) narration for every scene is made first, so each scene's length is
// known; 2) the scenes are replayed while page.screencast captures frames; each scene
// runs its actions while its narration plays, then waits until the narration ends;
// 3) ffmpeg builds the video from the frames, places each narration clip at its scene's
// start, and adds subtitles and chapters. A short black flash at each end, logged and
// then found again in the video, lines audio and picture up (and is cut out).
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { parseArgs } from 'node:util';
import { pathToFileURL } from 'node:url';
import { execFileSync, spawnSync } from 'node:child_process';
import { launch } from './lib/pw.mjs';
import { synthesise } from './lib/tts.mjs';

const { values: opt, positionals } = parseArgs({
  allowPositionals: true,
  options: {
    name: { type: 'string' },
    out: { type: 'string', default: 'demo-videos' },
    voice: { type: 'string', default: process.env.DEMO_VIDEO_PIPER_VOICE },
    'length-scale': { type: 'string' },
    gap: { type: 'string', default: '0.3' },
    width: { type: 'string', default: '1920' },
    height: { type: 'string', default: '1080' },
    fps: { type: 'string', default: '25' },
    'storage-state': { type: 'string' },
    'keep-work': { type: 'boolean', default: false },
    help: { type: 'boolean', default: false },
  },
});
if (opt.help || positionals.length !== 1) {
  console.log(fs.readFileSync(new URL(import.meta.url), 'utf8').split('\n').slice(1, 20).map((l) => l.replace(/^\/\/ ?/, '')).join('\n'));
  process.exit(opt.help ? 0 : 2);
}

const sceneFile = path.resolve(positionals[0]);
const script = (await import(pathToFileURL(sceneFile).href)).default;
const SCENES = script?.scenes;
if (!Array.isArray(SCENES) || !SCENES.length) throw new Error(`${sceneFile}: default export needs a non-empty "scenes" array`);
SCENES.forEach((s, k) => {
  s.id ??= `scene${k + 1}`;
  if (!s.narration?.trim()) throw new Error(`scene "${s.id}" has no narration`);
});

const NAME = opt.name ?? path.basename(sceneFile).replace(/\.(scenes\.)?m?js$/, '');
const W = Number(opt.width), H = Number(opt.height), FPS = Number(opt.fps);
const OUT = path.resolve(opt.out);
const CHECK = path.join(OUT, NAME);
const WORK = path.join(os.homedir(), '.cache/demo-video/work', NAME);
const FRAMES = path.join(WORK, 'frames');
const LEAD_IN = 0.25;  // s from scene start to narration start
const TAIL = 0.45;     // s after the narration ends (default; per scene: `hold`)
const MARKER_MS = 200; // black sync flash at each end

const STYLE = {
  highlight: 'outline: 3px solid #4f8cff; outline-offset: 2px; border-radius: 6px; background: rgba(79,140,255,.10)',
  point: 'width: 18px; height: 18px; border-radius: 50%; background: rgba(79,140,255,.45); box-shadow: 0 0 0 4px rgba(79,140,255,.25)',
  ...script.style,
  title: 'display: none', // hide Playwright's debug-style action captions
};

const log = (...a) => console.log(`[${new Date().toISOString().slice(11, 19)}]`, ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const run = (cmd, a) => execFileSync(cmd, a, { encoding: 'utf8', maxBuffer: 1 << 28, stdio: ['ignore', 'pipe', 'pipe'] });
const esc = (s) => String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

function cardHtml({ kicker, title, subtitle }, durationMs) {
  const out = Math.max(0, durationMs - 450);
  return `
<style>
  @keyframes in  { from { opacity: 0; transform: translateY(14px) } to { opacity: 1; transform: none } }
  @keyframes bgin { from { opacity: 0 } to { opacity: 1 } }
  @keyframes out { to { opacity: 0 } }
  .card { position: absolute; inset: 0; display: flex; flex-direction: column; justify-content: center;
          padding: 0 ${Math.round(W / 12)}px; color: white; font-family: 'DejaVu Sans', system-ui, sans-serif;
          background: radial-gradient(${Math.round(W * 0.6)}px ${Math.round(H * 0.65)}px at 20% 30%, #23408e 0%, #0f1d45 55%, #0a1230 100%);
          animation: bgin 450ms ease-out both, out 450ms ease-in ${out}ms forwards; }
  .k { font-size: ${Math.round(H / 36)}px; letter-spacing: .14em; text-transform: uppercase; color: #8fb3ff; animation: in 600ms 150ms ease-out both }
  .t { font-size: ${Math.round(H / 10.4)}px; font-weight: 700; line-height: 1.05; margin: ${Math.round(H / 50)}px 0 ${Math.round(H / 38)}px; animation: in 600ms 300ms ease-out both }
  .s { font-size: ${Math.round(H / 27)}px; color: rgba(255,255,255,.78); animation: in 600ms 450ms ease-out both }
  .bar { width: 140px; height: 8px; border-radius: 4px; background: #4f8cff; margin-top: ${Math.round(H / 19)}px; animation: in 600ms 600ms ease-out both }
</style>
<div class="card">${kicker ? `<div class="k">${esc(kicker)}</div>` : ''}<div class="t">${esc(title)}</div>${subtitle ? `<div class="s">${esc(subtitle)}</div>` : ''}<div class="bar"></div></div>`;
}

function chapterHtml(text) {
  return `
<style>@keyframes s{from{opacity:0;transform:translateX(-24px)}to{opacity:1;transform:none}}@keyframes o{to{opacity:0}}</style>
<div style="position:absolute;left:48px;bottom:56px;padding:18px 30px;border-radius:12px;background:rgba(10,18,48,.86);
     color:#fff;font:600 ${Math.round(H / 32)}px 'DejaVu Sans',system-ui,sans-serif;box-shadow:0 8px 30px rgba(0,0,0,.3);
     border-left:8px solid #4f8cff;animation:s 400ms ease-out both, o 400ms ease-in 2600ms forwards">${esc(text)}</div>`;
}

// ---------------------------------------------------------------------------------------
// 1) Narration
// ---------------------------------------------------------------------------------------
function narrate() {
  log(`narrating ${SCENES.length} scenes with ${path.basename(opt.voice ?? '?')}`);
  const res = synthesise(
    SCENES.map((s) => ({ text: s.narration, out: path.join(WORK, 'audio', `${s.id}.wav`) })),
    { voice: opt.voice, lengthScale: opt['length-scale'], gap: Number(opt.gap) },
  );
  const total = res.items.reduce((n, i) => n + i.audioSec, 0);
  log(`narration: ${total.toFixed(1)} s of speech, made in ${(res.synthMs / 1000).toFixed(1)} s`);
  return res.items;
}

// ---------------------------------------------------------------------------------------
// 2) Record
// ---------------------------------------------------------------------------------------
async function record(tts) {
  fs.rmSync(FRAMES, { recursive: true, force: true });
  fs.mkdirSync(FRAMES, { recursive: true });
  const storageState = opt['storage-state'] ?? script.storageState;
  const { browser, context, page } = await launch({ width: W, height: H, storageState, locale: script.locale });

  const css = [
    script.hide?.length ? `${script.hide.join(', ')} { display: none !important }` : '',
    script.smoothScroll === false ? '' : 'html { scroll-behavior: smooth }',
    script.css ?? '',
  ].join('\n');
  if (css.trim()) {
    await context.addInitScript((text) => {
      const add = () => { const s = document.createElement('style'); s.textContent = text; document.documentElement.appendChild(s); };
      if (document.documentElement) add(); else document.addEventListener('DOMContentLoaded', add);
    }, css);
  }

  if (script.url) await page.goto(script.url, { waitUntil: 'load' });
  if (script.setup) await script.setup({ page, context });
  await page.waitForTimeout(800);

  const frames = [];
  const events = [];
  const mark = (type, extra = {}) => { const e = { type, t: Date.now(), ...extra }; events.push(e); return e; };

  await page.screencast.start({
    size: { width: W, height: H },
    fps: FPS,
    quality: 90,
    onFrame: ({ data, timestamp }) => {
      const i = frames.length;
      fs.writeFileSync(path.join(FRAMES, `f${String(i).padStart(6, '0')}.jpg`), data);
      frames.push({ i, ts: timestamp });
    },
  });

  const flash = async (name) => {
    const ov = await page.screencast.showOverlay('<div style="position:absolute;inset:0;background:#000"></div>');
    mark(`${name}-on`);
    await sleep(MARKER_MS);
    await ov.dispose();
    mark(`${name}-off`);
  };

  const overruns = [];
  let failure = null;
  try {
    await sleep(300);
    await flash('marker1');
    let actionsShown = false;
    for (const [k, scene] of SCENES.entries()) {
      const item = tts[k];
      const target = (LEAD_IN + item.audioSec + (scene.hold ?? TAIL * 1000) / 1000) * 1000;
      const start = mark('scene', { id: scene.id, k });
      const voiceStart = start.t + LEAD_IN * 1000;
      log(`scene ${scene.id}: ${item.audioSec.toFixed(1)} s`);
      // Cursor and highlight only while the page is showing, not over title cards.
      if (!scene.card && !actionsShown) {
        await page.screencast.showActions({ cursor: 'pointer', duration: script.actionMs ?? 700, style: STYLE });
        actionsShown = true;
      } else if (scene.card && actionsShown) {
        await page.screencast.hideActions();
        actionsShown = false;
      }
      const card = scene.card ? await page.screencast.showOverlay(cardHtml(scene.card, target)) : null;
      if (scene.chapter) {
        const lt = await page.screencast.showOverlay(chapterHtml(scene.chapter));
        setTimeout(() => lt.dispose().catch(() => {}), 3100);
      }
      if (scene.actions) {
        const ctx = {
          page, context,
          pause: (ms) => sleep(ms),
          // Wait until sentence i (0-based) of this scene's narration starts.
          sentence: async (i) => {
            const seg = item.segments[i];
            if (!seg) throw new Error(`scene "${scene.id}" has no sentence ${i} (it has ${item.segments.length})`);
            const wait = voiceStart + seg.start * 1000 - Date.now();
            if (wait > 0) await sleep(wait);
          },
        };
        await scene.actions(ctx);
      }
      const remaining = start.t + target - Date.now();
      if (remaining > 0) await sleep(remaining);
      else {
        overruns.push({ id: scene.id, seconds: +(-remaining / 1000).toFixed(2) });
        log(`  WARNING: scene ${scene.id} actions ran ${(-remaining / 1000).toFixed(1)} s past its narration`);
      }
      if (card) await card.dispose();
    }
    if (actionsShown) await page.screencast.hideActions();
    await flash('marker2');
    await sleep(300);
  } catch (e) {
    failure = e;
    fs.mkdirSync(CHECK, { recursive: true });
    await page.screenshot({ path: path.join(CHECK, 'error.png') }).catch(() => {});
  }
  mark('screencast-stop');
  await page.screencast.stop().catch(() => {});
  await browser.close();
  if (failure) {
    const at = events.filter((e) => e.type === 'scene').at(-1)?.id;
    failure.message = `scene "${at ?? '?'}" failed: ${failure.message}\n(screenshot: ${path.join(CHECK, 'error.png')})`;
    throw failure;
  }
  log(`recorded ${frames.length} frames`);
  return { events, frames, overruns };
}

// ---------------------------------------------------------------------------------------
// 3) Assemble
// ---------------------------------------------------------------------------------------
// Resample the screencast (frames only arrive when the screen changes) to constant FPS:
// output frame k shows the latest frame captured at or before t0 + k/FPS. Every concat
// entry then lasts exactly 1/FPS; feeding raw sub-frame durations to ffmpeg drifts.
function framesToVideo({ frames, events }, file) {
  const stop = events.find((e) => e.type === 'screencast-stop').t;
  const t0 = frames[0].ts, step = 1000 / FPS;
  const lines = ['ffconcat version 1.0'];
  for (let k = 0, j = 0; k < Math.floor((stop - t0) / step); k++) {
    while (j + 1 < frames.length && frames[j + 1].ts <= t0 + k * step) j++;
    lines.push(`file 'frames/f${String(frames[j].i).padStart(6, '0')}.jpg'`, `duration ${(1 / FPS).toFixed(6)}`);
  }
  const list = path.join(WORK, 'frames.ffconcat');
  fs.writeFileSync(list, lines.join('\n') + '\n');
  run('ffmpeg', ['-v', 'error', '-y', '-f', 'concat', '-safe', '0', '-i', list, '-fps_mode', 'cfr', '-r', String(FPS),
    '-c:v', 'libx264', '-preset', 'veryfast', '-crf', '12', '-pix_fmt', 'yuv420p', file]);
}

function detectBlack(file) {
  const r = spawnSync('ffmpeg', ['-hide_banner', '-i', file, '-vf', 'blackdetect=d=0.04:pic_th=0.98:pix_th=0.06',
    '-an', '-f', 'null', '-'], { encoding: 'utf8', maxBuffer: 1 << 28 });
  return [...r.stderr.matchAll(/black_start:([\d.]+) black_end:([\d.]+)/g)].map((m) => ({ start: +m[1], end: +m[2] }));
}

const MAX_CUE = 84, MAX_LINE = 42;
function splitCue(seg) {
  const parts = [];
  const rec = (text) => {
    if (text.length <= MAX_CUE) return parts.push(text);
    const mid = text.length / 2;
    const near = (arr) => arr.sort((a, b) => Math.abs(a - mid) - Math.abs(b - mid))[0];
    let at = near([...text.matchAll(/, /g)].map((m) => m.index + 1).filter((i) => i > 15 && text.length - i > 15));
    if (at === undefined) at = near([...text.matchAll(/ /g)].map((m) => m.index));
    rec(text.slice(0, at).trim());
    rec(text.slice(at).trim());
  };
  rec(seg.text);
  const chars = parts.reduce((a, p) => a + p.length, 0);
  let t = seg.start;
  return parts.map((text) => {
    const d = (seg.end - seg.start) * (text.length / chars);
    const cue = { text, start: t, end: t + d };
    t += d;
    return cue;
  });
}
function wrap(text) {
  if (text.length <= MAX_LINE) return text;
  const mid = text.length / 2;
  const at = [...text.matchAll(/ /g)].map((m) => m.index).sort((a, b) => Math.abs(a - mid) - Math.abs(b - mid))[0];
  return at === undefined ? text : `${text.slice(0, at)}\n${text.slice(at + 1)}`;
}
function srtTime(s) {
  const ms = Math.max(0, Math.round(s * 1000));
  const p = (n, w = 2) => String(n).padStart(w, '0');
  return `${p(Math.floor(ms / 3600000))}:${p(Math.floor(ms / 60000) % 60)}:${p(Math.floor(ms / 1000) % 60)},${p(ms % 1000, 3)}`;
}

// Turn the subtitle track off by default. ffmpeg's MP4 writer always sets the "enabled"
// flag on the first subtitle track (movenc.c enable_tracks, whatever -disposition says),
// and players read that as "show it". So clear bit 0x1 of the tkhd flags of every track
// whose handler is 'sbtl' (mov_text): one byte changed in place, nothing moves.
function disableSubtitleTracks(file) {
  const buf = fs.readFileSync(file);
  const boxes = (start, end) => {
    const out = [];
    for (let p = start; p + 8 <= end;) {
      let size = buf.readUInt32BE(p), header = 8;
      if (size === 1) { size = Number(buf.readBigUInt64BE(p + 8)); header = 16; }
      else if (size === 0) size = end - p;
      if (size < header || p + size > end) break;
      out.push({ type: buf.toString('latin1', p + 4, p + 8), start: p, body: p + header, end: p + size });
      p += size;
    }
    return out;
  };
  const child = (box, type) => boxes(box.body, box.end).find((b) => b.type === type);
  const moov = boxes(0, buf.length).find((b) => b.type === 'moov');
  const fd = fs.openSync(file, 'r+');
  let patched = 0;
  try {
    for (const trak of moov ? boxes(moov.body, moov.end).filter((b) => b.type === 'trak') : []) {
      const tkhd = child(trak, 'tkhd');
      const mdia = child(trak, 'mdia');
      const hdlr = mdia && child(mdia, 'hdlr');
      // hdlr: version+flags (4), pre_defined (4), handler_type (4)
      if (!tkhd || !hdlr || buf.toString('latin1', hdlr.body + 8, hdlr.body + 12) !== 'sbtl') continue;
      const at = tkhd.body + 3; // tkhd: version (1), flags (3): lowest byte holds "enabled"
      fs.writeSync(fd, Buffer.from([buf[at] & ~0x01]), 0, 1, at);
      patched++;
    }
  } finally {
    fs.closeSync(fd);
  }
  return patched;
}

function assemble(tl, tts) {
  const { events, frames } = tl;
  const t0 = frames[0].ts;
  const vt = (t) => (t - t0) / 1000;
  const ev = (type) => events.find((e) => e.type === type);

  const raw = path.join(WORK, 'frames.mp4');
  framesToVideo(tl, raw);

  // Map logged times to video seconds through the two flashes (offset + drift).
  const blacks = detectBlack(raw);
  const predicted = [vt(ev('marker1-on').t), vt(ev('marker2-on').t)];
  let map = vt, sync = { method: 'frame-clock (flashes not found)' }, cutIn, cutOut;
  if (blacks.length >= 2) {
    const [b1, b2] = [blacks[0], blacks.at(-1)];
    const scale = (b2.start - b1.start) / (predicted[1] - predicted[0]);
    const offset = b1.start - predicted[0] * scale;
    map = (t) => offset + vt(t) * scale;
    sync = {
      method: 'flashes',
      flash1ErrMs: Math.round((b1.start - predicted[0]) * 1000),
      flash2ErrMs: Math.round((b2.start - predicted[1]) * 1000),
      driftMsPerMin: Math.round((scale - 1) * 60000),
    };
    cutIn = b1.end; cutOut = b2.start;
  } else {
    cutIn = map(ev('marker1-off').t); cutOut = map(ev('marker2-on').t);
  }
  const duration = cutOut - cutIn;
  const at = (t) => map(t) - cutIn;

  const scenes = events.filter((e) => e.type === 'scene').map((e) => ({
    id: e.id, k: e.k, start: at(e.t), voiceAt: at(e.t) + LEAD_IN, scene: SCENES[e.k], item: tts[e.k],
  }));
  scenes.forEach((s, k) => { s.end = k + 1 < scenes.length ? scenes[k + 1].start : duration; });

  fs.mkdirSync(OUT, { recursive: true });
  fs.mkdirSync(CHECK, { recursive: true });

  // Subtitles: exact sentence times from the narration; long sentences split. Only an
  // input to ffmpeg, which stores them inside the .mp4 as a subtitle track.
  let n = 0;
  const srt = scenes.flatMap((s) => s.item.segments.flatMap(splitCue).map((g) =>
    `${++n}\n${srtTime(s.voiceAt + g.start)} --> ${srtTime(s.voiceAt + g.end)}\n${wrap(g.text)}\n`)).join('\n');
  const srtFile = path.join(WORK, 'subtitles.srt');
  fs.writeFileSync(srtFile, srt);

  // Chapters.
  const metaEsc = (s) => String(s).replace(/[=;#\\\n]/g, (c) => '\\' + c);
  const meta = [';FFMETADATA1', `title=${metaEsc(script.title ?? NAME)}`, ''];
  for (const s of scenes) {
    meta.push('[CHAPTER]', 'TIMEBASE=1/1000', `START=${Math.round(s.start * 1000)}`, `END=${Math.round(s.end * 1000)}`,
      `title=${metaEsc(s.scene.chapter ?? s.scene.card?.title ?? s.id)}`, '');
  }
  const metaFile = path.join(WORK, 'chapters.ffmeta');
  fs.writeFileSync(metaFile, meta.join('\n'));

  // Audio: each narration clip delayed to its scene, mixed, loudness-normalised.
  const inputs = ['-ss', cutIn.toFixed(3), '-t', duration.toFixed(3), '-i', raw, '-i', metaFile];
  const filters = [];
  scenes.forEach((s, k) => {
    inputs.push('-i', s.item.out);
    filters.push(`[${k + 2}:a]adelay=${Math.round(s.voiceAt * 1000)}:all=1[a${k}]`);
  });
  filters.push(`${scenes.map((_, k) => `[a${k}]`).join('')}amix=inputs=${scenes.length}:normalize=0:dropout_transition=0,` +
    'loudnorm=I=-16:TP=-1.5:LRA=11,aresample=48000,apad[aout]');
  // Subtitles go in as a switchable mov_text track (not burned into the picture).
  const subIn = scenes.length + 2;
  inputs.push('-i', srtFile);
  const mp4 = path.join(OUT, `${NAME}.mp4`);
  run('ffmpeg', ['-v', 'error', '-y', ...inputs, '-filter_complex', filters.join(';'),
    '-map', '0:v', '-map', '[aout]', '-map', `${subIn}:s`, '-map_metadata', '1', '-map_chapters', '1',
    '-c:v', 'libx264', '-preset', 'medium', '-crf', '20', '-pix_fmt', 'yuv420p', '-r', String(FPS),
    '-c:a', 'aac', '-b:a', '160k', '-ar', '48000',
    '-c:s', 'mov_text', '-metadata:s:s:0', 'language=eng', '-metadata:s:s:0', 'handler_name=English',
    '-t', duration.toFixed(3), '-movflags', '+faststart', mp4]);
  disableSubtitleTracks(mp4);

  // --- Checks ---------------------------------------------------------------------
  const probe = JSON.parse(run('ffprobe', ['-v', 'error', '-show_entries',
    'stream=codec_type,codec_name,duration:stream_disposition=default:stream_tags=language:format=duration,size',
    '-of', 'json', mp4]));
  const vDur = Number(probe.streams.find((x) => x.codec_type === 'video').duration);
  const aDur = Number(probe.streams.find((x) => x.codec_type === 'audio').duration);
  const subs = probe.streams.find((x) => x.codec_type === 'subtitle');
  const sd = spawnSync('ffmpeg', ['-hide_banner', '-i', mp4, '-vn', '-af', 'silencedetect=n=-40dB:d=0.25', '-f', 'null', '-'],
    { encoding: 'utf8', maxBuffer: 1 << 26 }).stderr;
  const onsets = [...sd.matchAll(/silence_end: ([\d.]+)/g)].map((m) => +m[1]);
  const voice = scenes.map((s) => {
    const near = onsets.reduce((b, o) => (Math.abs(o - s.voiceAt) < Math.abs(b - s.voiceAt) ? o : b), Infinity);
    return { id: s.id, errMs: Math.round((near - s.voiceAt) * 1000) };
  });

  // Stills: half a second into each scene and just before it ends, plus a contact sheet.
  const stills = path.join(CHECK, 'stills');
  fs.rmSync(stills, { recursive: true, force: true });
  fs.mkdirSync(stills, { recursive: true });
  for (const s of scenes) {
    const tag = `${String(s.k + 1).padStart(2, '0')}-${s.id}`;
    for (const [suffix, t] of [['a-start', s.start + 0.5], ['b-end', Math.max(s.start, s.end - 0.6)]]) {
      run('ffmpeg', ['-v', 'error', '-y', '-ss', t.toFixed(3), '-i', mp4, '-frames:v', '1', '-vf', 'scale=960:-1',
        path.join(stills, `${tag}-${suffix}.png`)]);
    }
  }
  const cols = 4, rows = Math.ceil((scenes.length * 2) / cols);
  run('ffmpeg', ['-v', 'error', '-y', '-pattern_type', 'glob', '-i', path.join(stills, '*.png'),
    '-vf', `scale=480:-1,tile=${cols}x${rows}:padding=4:color=white`, '-frames:v', '1', path.join(CHECK, 'contact.png')]);

  const checks = [
    ['sync flashes found at both ends', sync.method === 'flashes'],
    ['flash timing within 120 ms', sync.method === 'flashes' && Math.abs(sync.flash1ErrMs) < 120 && Math.abs(sync.flash2ErrMs) < 120],
    ['every scene\'s narration starts within 100 ms of plan', voice.every((v) => Math.abs(v.errMs) < 100)],
    ['video and audio the same length (within 50 ms)', Math.abs(vDur - aDur) < 0.05],
    ['no scene\'s actions outlast its narration', tl.overruns.length === 0],
    ['subtitles built in (mov_text, English)', subs?.codec_name === 'mov_text' && subs?.tags?.language === 'eng'],
    ['subtitles off by default', subs?.disposition?.default === 0],
  ].map(([what, ok]) => ({ what, ok }));

  const report = {
    name: NAME, sceneFile, voice: opt.voice, lengthScale: opt['length-scale'] ?? null,
    mp4, duration: +duration.toFixed(3), sizeBytes: Number(probe.format.size), subtitleCues: n,
    sync, voiceOnsetErrMs: voice, overruns: tl.overruns,
    scenes: scenes.map((s) => ({ id: s.id, start: +s.start.toFixed(2), end: +s.end.toFixed(2), narrationSec: +s.item.audioSec.toFixed(2) })),
    checks,
  };
  fs.writeFileSync(path.join(CHECK, 'report.json'), JSON.stringify(report, null, 2));
  return report;
}

// ---------------------------------------------------------------------------------------
fs.mkdirSync(WORK, { recursive: true });
const tts = narrate();
const tl = await record(tts);
const report = assemble(tl, tts);
if (!opt['keep-work']) fs.rmSync(WORK, { recursive: true, force: true });

console.log(`\n${report.mp4}  (${report.duration.toFixed(1)} s, ${(report.sizeBytes / 1e6).toFixed(1)} MB)`);
console.log(`${path.join(CHECK, 'contact.png')}  <- look at this`);
for (const c of report.checks) console.log(`  ${c.ok ? 'PASS' : 'WARN'}  ${c.what}`);
if (report.overruns.length) console.log(`  overran: ${report.overruns.map((o) => `${o.id} +${o.seconds}s`).join(', ')} (lengthen its narration or trim its actions)`);
