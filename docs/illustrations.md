# Adding a missing exercise picture

The catalog (`assets/workout-guide/`) is [Workout Guide](https://github.com/bryllim/workout-guide):
302 exercises in one line-art style. When an exercise is missing, `scripts/new-illustration` draws
it in that style, so nobody can tell which pictures were added. Example: `horizontal-leg-press`.

## How the original art was made

- 76 first poses are tracings of [Everkinetic](https://github.com/everkinetic/data). The rest were
  drawn with OpenAI's image model through Codex (the author's own description), then vectorized.
- Upstream's [vectorizer](https://github.com/bryllim/workout-guide/blob/aac599224bb9780305239607ef98540b7e0ce389/scripts/lib/vectorize-png.mjs): alpha mask → potrace (`alphaMax: 1`,
  `optCurve: true`, `optTolerance: 0.2`, `threshold: 128`, `turdSize: 0`, white) → svgo. One filled
  path per picture on a 512 px canvas. `new-illustration` runs the same code.
- The style in numbers, over the 226 non-Everkinetic pictures: line width 4 px at 512 (p10–p90
  4–4.5), figure fills 0.9 of the canvas, and ink covers 0.11 of the figure's box (p90 0.15;
  machines 0.13–0.14). The script measures all three for every candidate.

## Steps

Needs `OPENROUTER_API_KEY`, `uv`, `node`, `rsvg-convert` + ImageMagick
(`brew install librsvg imagemagick`) and `ffmpeg` for animation previews.
Work files go to `target/illustrations/<slug>/`.

1. **Draw.** Give the closest catalog picture first in `--like`, then one or two others with the
   same kind of equipment. Each candidate is ~$0.20.
   ```
   scripts/new-illustration draw horizontal-leg-press "A man on a HORIZONTAL (seated) leg press \
     machine: padded seat with a reclined backrest, hands on the side handles, both feet flat on a \
     large vertical footplate in front of him, knees bent about 90 degrees (start position)." \
     --like leg-press,machine-row,seated-leg-curl
   ```
   Each candidate is cleaned right away (step 2) and shown in `sheet.png`: the `--like` pictures
   on top, the candidates below. Pick by eye.
2. **Clean** (automatic after `draw` and `animate`; run it by hand on any PNG):
   `scripts/new-illustration clean <slug> <png>… --like …`. It scales the figure to the catalog's
   size, thickens or thins its lines to 4 px, traces it, renders the Telegram PNG, and flags
   metrics outside the catalog's range.
3. **Animate** (optional): `scripts/new-illustration animate <slug> <raw draw png> "He pushes the
   footplate away until his legs are almost straight, then returns."` One call (~$0.18) returns a
   16-frame loop. The frames are cleaned together, and `anim.mp4` is a preview (Telegram plays MP4
   crisply but re-encodes GIFs). Frame 1 is often a better still than the draw itself.
4. **Install**: `scripts/new-illustration install <slug> target/illustrations/<slug>/<pick>.svg`
   copies the SVG and renders `frame-1.png`.
5. **Manifest**: add an entry to `assets/workout-guide/manifest.json` next to its closest
   exercise. Copy `horizontal-leg-press`: same fields, the attribution credits GymClaw with Workout
   Guide as the source (CC BY-SA 4.0). Add the slug under "Local additions" in
   `assets/workout-guide/NOTICE.md`.
6. **Use it**: after deploy, point the plan's exercise at it with `template_edit_exercise`
   (`changes: {"guide_id": "<slug>"}`). Keep the exercise id: history and the next weight follow it.

## What we learned

- **Too many lines look "stronger"** than the catalog even at the right line width. The prompt
  asks for sparse lines; the density check catches the rest. Keep density ≤ ~0.15 for machines.
- Editing toward the closest catalog picture holds the style better than drawing from scratch.
- Models: GPT Image 2.6 (`openai/gpt-image-2.6-sunburst`, default; `ILLUSTRATION_MODEL` to change)
  is only on OpenRouter's `/api/v1/images`, not chat completions. Gemini image models
  (`google/gemini-3.1-flash-image`, via chat completions) also hold the style, but can't animate:
  they return near-identical frames.
- Animation: the machine "boils" slightly (~1% of its lines change between frames) because each
  frame is redrawn; it doesn't drift. If that shows, keep frame 1's static parts and take only the
  moving parts from each frame.
- Upstream accepts new exercises with three frames under CC BY-SA 4.0; animation frames could
  supply frames 2 and 3.
