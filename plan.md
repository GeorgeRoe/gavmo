# Narrated Demo Videos from Agent VMs: Research & Plan

## Goal

Go beyond markdown reports with screenshots: have the agent produce a short, polished,
"corporate tutorial" style video showing a feature working, with:

- an animated mouse cursor moving and clicking
- the element being clicked highlighted
- title and chapter cards
- a synthesized voice-over explaining each step
- a separate subtitle file (`.srt`)

## Decisions so far

| Topic | Decision |
|---|---|
| Where it lives | A **reusable GAVMO recipe** (`gavmo.nix` snippet + skill + helper script) that any repo can add. Not tied to one app. |
| Voice engine | **Not chosen yet.** The spike tests **Piper** and **Kokoro** for quality and speed. Piper has the advantage of installing through Nix. |
| Captions | **Separate `.srt` only.** Nothing burned into the video. |
| First step | **A spike first**, using a large public site (**Wikipedia**) as the test subject. |

## What's available in the current VM

**Already there:**
- `nix` 2.34, using the nixpkgs **26.11** package set (`/nix/var/nix/profiles/per-user/root/channels/nixos`).
  - `pkgs/by-name/pi/piper-tts` exists.
  - The full `ffmpeg` exists (`pkgs/development/libraries/ffmpeg`).
- `node` v24.
- Network access to `registry.npmjs.org`, `huggingface.co` and `cache.nixos.org` all works.
- The browser server, `@playwright/mcp` **0.0.83**, installed globally at `/home/agent/.npm-global`.
  - It uses `playwright-core` **1.64.0-alpha**.
- Chromium from Nix, via `PLAYWRIGHT_MCP_EXECUTABLE_PATH`, in the bundle `/nix/store/...-playwright-browsers/`. The bundle contains:
  - `chromium-1243`
  - `chromium_headless_shell-1243`
  - `ffmpeg-1011`

**Missing:**
- Playwright's own `ffmpeg-1011` is a cut-down build. It can only produce silent `.webm` video (VP8) and has **no audio encoders and no `.mp4` output**. A full ffmpeg is needed.
- No text-to-speech tool is installed (no espeak, piper or festival).
- No `python3`, `sox` or ImageMagick.
- VM resources: **4 CPUs, 3.8 GiB of RAM.**

### Key finding: Playwright's `page.screencast` feature

Playwright 1.64-alpha includes a new `Screencast` feature (see `playwright-core/types/types.d.ts`).
It already provides most of what the video needs visually:

```ts
page.screencast.start({ path?, size?, fps?, quality?, onFrame? }): Promise<Disposable>
page.screencast.stop()
page.screencast.showActions({
  cursor?: 'pointer' | 'none',   // animated pointer travels between action points
  duration?: number,             // how long each callout stays on screen (default 500ms)
  position?: 'top-left' | 'top' | 'top-right' | 'bottom-left' | 'bottom' | 'bottom-right',
  style?: { point?: string, highlight?: string, title?: string }  // CSS
})
page.screencast.hideActions()
page.screencast.showChapter(title, { description?, duration? })  // full-screen card, blurred backdrop
page.screencast.showOverlay(html, { duration? })                 // arbitrary HTML overlay (lower thirds etc.)
page.screencast.hideOverlays() / showOverlays()
```

The browser server offers the same features as tools when launched with `--caps devtools`:
- `browser_start_video({ filename?, size?, fps?, cursor? })`. `cursor: true` animates a cursor and slows each action by 800 ms.
- `browser_stop_video`
- `browser_video_chapter({ title, description?, duration? })`
- `browser_video_show_actions({ duration?, position?, cursor?, style? })`
- `browser_video_hide_actions`

### Problem: recording while the agent drives the browser live

If the agent records while driving the browser one tool call at a time, all of its thinking time
between actions (often 5–30 seconds) ends up in the video as **frozen screen**.

**Fix:**
1. Explore with the live browser tools as usual.
2. Write a short **scene script**: a list of scenes, each with narration text and actions.
3. Replay it at a steady pace while recording.

The `--caps devtools` tools are still handy for quick rough recordings during exploration.

## Proposed design

1. **Explore** the feature using the live browser tools.
2. **Write a scene script.** Each scene has:
   - `title` (optional, shown as a chapter card)
   - `narration` (text)
   - `actions` (an async function using the Playwright `page`)
3. **Run the helper tool (Node):**
   1. Generate every scene's narration first, so each scene's length is known.
   2. Launch the Nix Chromium headless at 1920×1080 and start `page.screencast.start({ path, size, fps: 25 })`.
   3. Turn on `showActions({ cursor: 'pointer', style: { highlight: 'outline: 3px solid #4f8cff; border-radius: 6px', ... } })`.
   4. Show an intro card with `showChapter`.
   5. For each scene:
      - log the start time relative to the start of recording
      - run its actions
      - wait until the narration has finished, plus about 400 ms of padding
   6. Show an outro card, then stop.
4. **Combine with ffmpeg:**
   - Place each narration clip at its scene's start time (`adelay`) and mix them (`amix`).
   - Even out the volume (`loudnorm`).
   - Encode to H.264 + AAC `.mp4` with `-movflags +faststart`.
   - Add chapter markers (ffmetadata).
   - Write the `.srt` from the scene start times and narration lengths.
5. **Check our own work:**
   - Pull frames at each scene start and look at them.
   - Use `ffprobe` to confirm the video and audio lengths match.
6. **Deliver:** link the `.mp4` and `.srt` from the usual markdown report.

## Phase 1: the spike (Wikipedia, both voices compared)

**Goal:** a 45–60 second narrated `.mp4`, made twice (once with Piper, once with Kokoro), plus
timing numbers for choosing a voice.

### Setup (only for the spike; `gavmo.nix` stays unchanged)

- **ffmpeg and Piper**, for the agent's own user only, with no root access needed:
  `nix shell nixpkgs#ffmpeg nixpkgs#piper-tts`
- **Piper voices**, from Hugging Face `rhasspy/piper-voices`:
  - `en_GB-alan-medium`
  - `en_GB-cori-high`
  - `en_GB-northern_english_male-medium`
- **Kokoro**: `kokoro-js` from npm. Its dependencies are `@huggingface/transformers` and `phonemizer`. The model is `onnx-community/Kokoro-82M-v1.0-ONNX`, about 90 MB for the q8 version.
  - Install into `/tmp/opencode/video-spike`.
  - Voices: `bm_george`, `bf_emma`, `bm_lewis`.
- **Playwright**: the globally installed `playwright-core` 1.64-alpha, since the `screencast` feature needs it. Use the Nix Chromium via the `PLAYWRIGHT_MCP_EXECUTABLE_PATH` setting, with `headless: true` and `--no-sandbox`.

### Steps

1. **Voice test.**
   - Generate the same narration (about 6 sentences) with every voice.
   - For each one, record:
     - model and voice download size
     - first-run time (cold)
     - later-run time (warm)
     - speed relative to real time (synthesis time ÷ audio length)
     - peak memory use
   - Save the clips as `.wav` files to listen to.
2. **`director.mjs`:** a single-file recording script containing a list of scenes:
   1. Open `https://en.wikipedia.org`, then show the intro card ("Searching Wikipedia").
   2. Type "Rutherford Appleton Laboratory" into the search box and choose the suggestion.
   3. Scroll to a section and click a link in the contents.
   4. Hover over a link to show the preview pop-up.
   5. Show the outro card.
3. **Recording:**
   - Generate the narration first so each scene's length is known.
   - Record at 1920×1080 and 25 fps, with the animated cursor and highlight outline turned on.
   - Each scene runs its actions, then waits until its narration ends plus about 400 ms.
   - Log each scene's start time.
4. **Combine:**
   - Line each clip up with its scene's start time, mix, and even the volume.
   - Output an H.264/AAC `.mp4`, a `.srt` file and chapter markers.
   - One output per voice engine.
5. **Check:**
   - Pull frames at each scene start and look at them.
   - Use `ffprobe` to confirm the video and audio lengths match.
   - Note any lag between voice and picture.
6. **Deliver** to `.gavmo/video-spike/` so the files appear on the host machine:
   - `demo-piper.mp4` and `demo-piper.srt`
   - `demo-kokoro.mp4` and `demo-kokoro.srt`
   - `samples/*.wav` (each voice)
   - `NOTES.md`: timings, sizes, sync results and problems found

### Things to check during the spike

- **Kokoro speed** on 4 CPUs and 3.8 GiB of RAM, possibly with a dev server also running.
- **Sync drift** between the recording's timing and the logged scene start times. If it's poor, switch to recording frames with timestamps (`onFrame`) and assembling them with ffmpeg directly.
- **ffmpeg version:** whether the standard nixpkgs `ffmpeg` includes `libx264`, or whether `ffmpeg-full` is needed.
- **Wikipedia behaviour:** banners and pop-ups (fundraising notices) may need dismissing.

## Phase 2: the reusable GAVMO recipe (after the voice is chosen)

- **`gavmo.nix` snippet:**
  - add `pkgs.ffmpeg` (or `ffmpeg-full` if needed)
  - for Piper: `pkgs.piper-tts`, plus a voice model fetched into the Nix store (`fetchurl` from `rhasspy/piper-voices`)
  - for Kokoro: install `kokoro-js` and download the model in the same startup step that installs `@playwright/mcp` today (`systemd.services.gavmo-opencode.preStart`), with a cache under `/home/agent`
- **`opencode.json`:** add `--caps devtools` to the `playwright-mcp` command, so quick rough recordings are possible.
- **Helper tool:** takes a scene script and produces an `.mp4` + `.srt` + chapter markers. Settings include:
  - resolution and fps
  - voice
  - cursor and highlight style
  - intro and outro text
  - output folder
- **`SKILL.md`** (e.g. `.agents/skills/demo-video/`) covering the workflow:
  1. Explore with the live browser tools.
  2. Write the scene script.
  3. Render.
  4. Check frames.
  5. Link the video from the markdown report.

  It also covers:
  - signing in: Playwright saved sessions (`storageState`) or `test.env` credentials
  - keeping narration short and tied to what's on screen
  - where output goes
- **Documentation:** a section in the GAVMO README on demo videos, alongside the existing browser-server recipe.

## Options considered and rejected

- **Cloud voices (OpenAI, ElevenLabs):** the best quality, but they need an API key and changes to the gateway, the narration text leaves the VM, and the gateway may not pass audio files through cleanly.
- **espeak-ng:** too robotic for a "corporate" result.
- **Captions burned into the video:** rejected in favour of a separate `.srt`.
- **Recording live while the agent drives the browser:** the agent's thinking time between actions shows up as frozen screen. Use replayed scene scripts instead.