# Example: narrated demo videos

Lets the agent produce a short, polished video of a feature working, instead of (or as
well as) a markdown report with screenshots:

- 1080p `.mp4` (H.264/AAC) with chapter markers
- animated mouse cursor that travels to each click, with the clicked element highlighted
- full-screen title cards and on-screen chapter labels
- a synthesized British voice-over ([Piper](https://github.com/OHF-Voice/piper1-gpl)),
  timed so each sentence plays while the matching thing happens on screen
- English subtitles inside the same `.mp4`, as a track viewers can switch on (off by
  default, nothing burned into the picture)

Everything runs inside the VM: no API keys, and the narration text never leaves it.

It builds on [`examples/playwright`](../playwright/) (the browser tools) and adds
ffmpeg, Piper with one pinned voice, and a `demo-video` skill.

## Setup

Copy into your repo:

| From here | To your repo |
|---|---|
| `gavmo.nix` | `gavmo.nix`, or merge it into yours (it already contains the `examples/playwright` setup) |
| `opencode.json` | `opencode.json`, or merge in the `playwright` MCP entry. Note the extra `--caps devtools` |
| `.opencode/skills/demo-video/` | `.opencode/skills/demo-video/` |

Restart the VM. The first boot builds Piper (a small local build, a few minutes; the
rest comes from the Nix cache) and downloads the voice (63 MB).

Then ask the agent for a video, e.g. *"make a demo video of the new invoice flow"*. It
explores the feature with the browser, writes a scene script, renders it, checks the
frames and links `demo-videos/<name>.mp4` from its report.

To try it without your own app:

```sh
node .opencode/skills/demo-video/render.mjs .opencode/skills/demo-video/examples/wikipedia.mjs
```

That makes `demo-videos/wikipedia.mp4`: a 36-second tour of searching Wikipedia.

## How it works

Recording while the agent drives the browser live doesn't work: every pause between
tool calls (often 5-30 s of thinking) becomes frozen screen in the video. So the agent
explores live, then writes a **scene script**: a list of scenes, each with narration
text and Playwright actions (see the skill's `SKILL.md` for the format). Then
`render.mjs`:

1. **Narrates first.** Piper speaks every sentence separately, so the length of each
   scene and the start time of each sentence are known before recording.
2. **Replays and records.** Launches the Nix Chromium headless at 1920×1080 and records
   with Playwright's `page.screencast` API (new in Playwright 1.64; `@playwright/mcp`
   0.0.83 ships it). The API draws the cursor, click highlights and overlays (title cards,
   chapter labels). Each scene runs its actions while its narration plays, then waits
   until the narration ends. Actions can wait for a particular sentence (`sentence(i)`),
   which is what keeps voice and picture together.
3. **Assembles with ffmpeg.** Builds a constant-25-fps video from the captured frames,
   places each scene's narration at its start, evens out the volume (`loudnorm`), adds
   chapters, and adds a subtitle track (`mov_text`) timed from the exact sentence
   timings. ffmpeg always marks the first subtitle track in an MP4 as "enabled", which
   players treat as "show by default", so `render.mjs` clears that flag afterwards.
4. **Checks itself.** A 200 ms black flash at a logged time at each end is found again in
   the video (`blackdetect`) and used to line audio and picture up (then cut out). The
   render reports the measured sync, checks that speech starts where planned, and writes
   a contact sheet of stills from every scene for the agent to look at.

Measured sync: the picture lands within ~50-70 ms of the logged times (a constant
offset, corrected), drift under 30 ms per minute, and speech within 30 ms of plan. That
is under two frames.

### What `gavmo.nix` adds on top of `examples/playwright`

- `pkgs.ffmpeg`: H.264 + AAC. (Playwright's own ffmpeg can only write silent `.webm`.)
- **Piper without its extras.** The default `pkgs.piper-tts` includes model training
  (PyTorch, Lightning...), Japanese support, an HTTP server and alignment tools: 16
  Python dependencies, with at least 1.7 GB from its direct dependencies alone.
  Speaking only needs `onnxruntime` and `numpy`, so the extras are turned off with
  `override`, leaving 3 dependencies (about 0.6 GB beyond node and ffmpeg). This variant isn't on
  `cache.nixos.org`, so it builds locally on first boot.
- **The voice**, fetched into the Nix store with `pkgs.fetchurl`, pinned to a commit of
  [`rhasspy/piper-voices`](https://huggingface.co/rhasspy/piper-voices), and exposed as
  `DEMO_VIDEO_PIPER_VOICE`.
- `PLAYWRIGHT_BROWSERS_PATH` pointing at the Nix browser bundle. Playwright's video
  recording needs its own ffmpeg build (`ffmpeg-1011`) and otherwise looks in
  `~/.cache/ms-playwright`. Without it, the browser server's `browser_start_video` fails.
  `render.mjs` itself doesn't need it.
- `--caps devtools` in `opencode.json` enables the browser server's `browser_start_video`
  / `browser_video_*` tools, for quick rough recordings.

## Changing the voice

Browse voices at [rhasspy/piper-voices](https://huggingface.co/rhasspy/piper-voices/tree/main)
(samples: [rhasspy.github.io/piper-samples](https://rhasspy.github.io/piper-samples/)). In
`gavmo.nix`, change `piperVoiceName`, the path in `piperVoiceUrl`, and both hashes. To
get a hash, set it to `pkgs.lib.fakeHash`, restart, and copy the correct hash from the
build error in the control panel. Or compute it from a download:
`nix hash file --sri <file>`.

**Check each voice's licence** in its `MODEL_CARD`: they vary, and several are
non-commercial only (e.g. `en_US-ryan-*`, `en_US-lessac-*`).

Speed: the voice's natural pace is the default. `--length-scale 1.1` makes it 10% slower.

## Caveats

- **Disk:** the slimmed Piper adds about 0.6 GB beyond what node and ffmpeg already pull
  in (Python, onnxruntime with OpenVINO, numpy/BLAS); the voice 63 MB;
  ffmpeg and Chromium as in `examples/playwright`. Rendering uses up to ~1 GB of
  temporary frames in `~/.cache/demo-video/work/` while it runs (deleted afterwards).
- **Speed:** a render takes roughly 2-3× the video's length (narration ~2 s, recording
  in real time, encoding). Piper speaks about 15× faster than real time on 4 CPUs.
- **Memory:** Piper peaks at ~300 MB and exits before Chromium starts.
- **Real time:** scenes are recorded in real time, so a slow app makes a slow video.
  If an action takes longer than its narration, the render warns and the scene simply
  runs longer.
- **Licences:** Piper itself is GPL-3. It runs as a separate program, so this doesn't
  affect your code. The voice has its own licence (below).
- **Subtitles need a desktop player.** VLC, QuickTime, Windows Media Player / Films &
  TV and mpv let you switch the built-in subtitles on. Browser-based players (an
  editor's markdown preview, a web page, Chrome or Firefox opened on the file) ignore
  subtitle tracks inside an `.mp4`; the video and voice-over still play.
- The default font for cards is DejaVu Sans (what the VM has).

## Troubleshooting

- **`piper: not found` or `DEMO_VIDEO_PIPER_VOICE` unset:** `gavmo.nix` isn't applied;
  restart the VM. If the Piper build failed (control panel build log), replace `piper`
  in `environment.systemPackages` with plain `pkgs.piper-tts`. It's bigger but prebuilt.
- **`playwright-core not found`:** the browser server didn't install at boot. See
  `examples/playwright`.
- **`browser_start_video` says ffmpeg is missing:** `PLAYWRIGHT_BROWSERS_PATH` isn't set,
  or a newer `@playwright/mcp` wants a newer ffmpeg revision than the Nix bundle has.
  Check `ls $PLAYWRIGHT_BROWSERS_PATH` against the path in the error.
- **"element was detached from the DOM":** the app replaced the element as you interacted
  (Wikipedia's search box does this). Click, wait for the new element, then type.
- **Banners or pop-ups in the video:** add their selectors to the scene script's `hide`.
- **Voice mispronounces something:** spell it the way it's said in the narration.

## Credits

The default voice, `en_GB-northern_english_male-medium`, is trained on the
[OpenSLR 83](https://www.openslr.org/83/) dataset (Crowdsourced high-quality UK and
Ireland English Dialect speech data set, Google), licensed
[CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/). Credit it when you
publish videos made with it, e.g. *"Narration: Piper TTS, voice
en_GB-northern_english_male-medium (OpenSLR 83, CC BY-SA 4.0)"*.
