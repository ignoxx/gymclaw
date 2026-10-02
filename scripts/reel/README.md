# GymClaw films

HTML scenes rendered frame by frame in headless Chromium, encoded with ffmpeg. Soundtracks are synthesized by `sound.py` from cues the page exports, so audio follows the script.

| Page | Output |
| --- | --- |
| `features.html?cut=15` | 15 s motion reel |
| `features.html?cut=60` | 60 s feature tour |
| `story.html` | 60 s real-life week |

`lib.js` / `lib.css` hold the shared Telegram phone, easing helpers and chapter animation.

```bash
npm i
export CHROME_PATH=...   # any Chromium, e.g. Playwright's
node render.mjs "features.html?cut=60" --cues /tmp/cues.json
python3 sound.py /tmp/cues.json /tmp/sound.wav
node render.mjs "features.html?cut=60" ../../videos/gymclaw-tour-60s.mp4 --audio /tmp/sound.wav --workers 4
node render.mjs story.html --stills /tmp/stills 6.2 26.4   # preview frames
```

Telegram screens and training data are simulated. Exercise art: Workout Guide / Bryl Lim / Everkinetic, CC BY-SA 4.0.
