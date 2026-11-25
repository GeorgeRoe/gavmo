---
name: demo-video
description: Use when asked to make a demo video, screencast, walkthrough, tutorial video or narrated recording of a web app or feature, or to "show it working" on video. Produces a narrated 1080p .mp4 (animated cursor, click highlights, title cards, chapters, synthesized voice-over, built-in subtitles) from a scripted replay in headless Chromium.
---

# Narrated demo videos

Make a short "corporate tutorial" style video of a feature working: animated cursor,
highlighted clicks, title cards, a voice-over that matches what's on screen, chapters,
and subtitles. Everything is in one `.mp4`: the subtitles are an English track
inside it, off by default, which viewers can switch on in their player.

**Don't record while driving the browser live** (`browser_start_video`): every pause
between your tool calls ends up in the video as a frozen screen. Use the live browser
to *explore*, then write a scene script and let `render.mjs` replay it at a steady pace.
(`browser_start_video` is fine for a quick rough clip that nobody will watch for polish.)

## Workflow

1. **Explore** the feature with the live browser tools (`browser_navigate`,
   `browser_snapshot`, `browser_click`, ...). Note the locators that work, what each
   step looks like, how long things take to appear, and anything in the way (cookie
   banners, pop-ups, inputs that get replaced when clicked).
2. **Write a scene script** (format below), for example
   `demo-videos/<feature>.scenes.mjs`. Start from `examples/wikipedia.mjs` next to this file.
3. **Render**:
   ```sh
   node .opencode/skills/demo-video/render.mjs demo-videos/<feature>.scenes.mjs
   ```
   It takes roughly 2-3× the video length. For long videos, run it with
   `gproc start render -- node ...` and check `gproc logs render`.
4. **Check your work.** Read `demo-videos/<name>/contact.png`: two stills per scene
   (just after it starts, just before it ends). Check that each one shows what the
   narration says at that moment. The render prints PASS/WARN checks; fix every WARN:
   - *actions outlast narration*: the scene ran on after the voice stopped. Lengthen
     the narration, trim the actions, or accept it by adding `hold`.
   - *sync flashes not found / timing*: rerun; if it keeps happening, report it.
   - *a scene failed*: the error names the scene and writes `demo-videos/<name>/error.png`.
   Change the script and re-render until it's right. A render is cheap.
5. **Deliver**: link `demo-videos/<name>.mp4` from your markdown report and give the
   length. Mention that subtitles can be switched on in the player (desktop players
   such as VLC, QuickTime, Windows Media Player and mpv; browser-based previews, e.g.
   an editor's markdown preview, don't show them). Don't commit the videos unless
   asked. They're build output, so suggest adding `demo-videos/*.mp4` (or the folder)
   to `.gitignore`.

## Scene script

An ES module whose default export describes the video:

```js
export default {
  title: 'Creating an invoice',             // video title (metadata)
  url: 'http://localhost:3000/invoices',    // opened before recording starts
  hide: ['#cookie-banner', '.intercom'],    // CSS selectors hidden on every page (optional)
  storageState: 'demo-videos/auth.json',    // signed-in session (optional, see below)
  async setup({ page, context }) {},        // runs before recording (optional)
  scenes: [
    {
      id: 'intro',
      card: { kicker: 'Acme Billing', title: 'Creating an invoice', subtitle: 'From draft to sent in a minute' },
      narration: "In this video, we'll create an invoice and send it to a customer.",
    },
    {
      id: 'new',
      chapter: 'Start a draft',             // label in the corner + chapter marker
      narration: 'We start from the invoices page. The New invoice button opens a blank draft.',
      async actions({ page, pause, sentence }) {
        await sentence(1);                  // wait until "The New invoice button..." is spoken
        await page.getByRole('button', { name: 'New invoice' }).click();
        await page.getByRole('heading', { name: 'Draft invoice' }).waitFor();
      },
    },
    // ...
    { id: 'outro', card: { title: "That's it", subtitle: 'Invoices, sent in seconds' }, narration: '...' },
  ],
};
```

Each scene: `narration` (required), `actions` (optional), and either `card` (full-screen
title card; the page is hidden and so is the cursor) or `chapter` (a label over the page).
`hold` (ms, default 450) is how long the scene stays after its narration ends.

How timing works: a scene's narration starts as the scene begins, its `actions` run
at the same time, and the scene ends when the narration (plus `hold`) is over.

Inside `actions({ page, context, pause, sentence })`:
- `sentence(i)`: wait until sentence `i` (0-based) of this scene's narration starts.
  Use it to line an action up with the words describing it. This is what makes the video
  feel in sync.
- `pause(ms)`: a plain wait (let an animation finish, give the viewer a beat).
- Use real Playwright actions (`click`, `hover`, `fill`...) on locators: the animated
  cursor travels to each one and the target gets a highlight. `page.mouse.*` and
  `page.keyboard.*` don't move the animated cursor.
- For visible typing, click the field and then use
  `page.keyboard.type('text', { delay: 90 })` (or `locator.pressSequentially`).
  `fill()` makes the text appear all at once.
- Every action is paced (the cursor takes ~0.7 s to travel), so a scene with several
  clicks needs a longer narration.

## Writing the narration

- One idea per scene, 1-3 short sentences. Describe what the viewer **sees happening
  now**, in the present tense ("We open...", "The list updates...").
- Name the things on screen exactly as they're labelled, so the viewer can find them.
- Keep the whole video to about 30-90 seconds. Open with what the video shows, end with a recap.
- The voice reads text literally. Write numbers and abbreviations the way they should
  be said if they're unusual ("version two point one", not "v2.1"). Avoid symbols,
  URLs and code in the narration.
- Sentences end at `.`, `!` or `?`. Each one becomes a subtitle cue and a `sentence(i)`
  index.

## Signing in

Never show or say credentials in a video. Sign in **before** recording:
- In `setup({ page })`: log in with credentials from the repo's `test.env` (or
  wherever the project keeps test logins), then `page.goto` the starting page. `setup` runs
  before recording starts, so none of it is in the video.
- Or save a session once (`await context.storageState({ path: 'demo-videos/auth.json' })`)
  and pass it with `storageState` / `--storage-state`. Don't commit that file.

## Options

`render.mjs <scenes.mjs> [--name N] [--out DIR] [--voice model.onnx] [--length-scale X]
[--gap S] [--width W --height H --fps F] [--storage-state FILE] [--keep-work]`

- Output defaults to `demo-videos/` in the current directory: `<name>.mp4` (with
  chapters and subtitles inside) and `<name>/` (contact sheet, stills, `report.json`).
  To read the subtitles as text: `ffmpeg -i demo-videos/<name>.mp4 -map 0:s /tmp/<name>.srt`.
- The voice comes from `$DEMO_VIDEO_PIPER_VOICE` (set in `gavmo.nix`). Its natural pace is
  the default; `--length-scale 1.1` is 10% slower. Viewers can speed playback up themselves.
- Default 1920×1080 at 25 fps. Pages are shown at that viewport size and 100% zoom.

## Troubleshooting

- `piper: not found` / `DEMO_VIDEO_PIPER_VOICE` unset / `PLAYWRIGHT_MCP_EXECUTABLE_PATH
  is not set`: this repo's `gavmo.nix` doesn't include the demo-video recipe (or the VM
  hasn't been restarted since). Ask the user.
- `playwright-core not found`: the browser server (`@playwright/mcp`) didn't install at
  boot; check it works in opencode.
- An element is "detached from the DOM": the app replaced it (common with search boxes).
  Click, wait for the new element, then act on it again.
- Banners/pop-ups appear only sometimes (A/B tests, fundraising): add them to `hide`.
