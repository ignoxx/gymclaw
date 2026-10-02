// Renders a film page frame by frame in headless Chromium and encodes with ffmpeg.
// Usage:
//   node render.mjs PAGE OUT.mp4 [--audio sound.wav] [--fps 60] [--workers 4]
//   node render.mjs PAGE --cues cues.json            (sound cues for sound.py)
//   node render.mjs PAGE --stills DIR 0.5 3.2 ...    (preview frames)
// PAGE may carry a query, e.g. "features.html?cut=60". Duration comes from the page.
import { chromium } from 'playwright-core';
import { spawn } from 'node:child_process';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const flag = (name, fallback) => { const i = args.indexOf(name); return i < 0 ? fallback : args.splice(i, 2)[1]; };
const fps = Number(flag('--fps', 60));
const workers = Number(flag('--workers', 4));
const audio = flag('--audio', null);
const stills = flag('--stills', null);
const cuesOut = flag('--cues', null);
const [pageArg, ...rest] = args;
const [file, query = ''] = pageArg.split('?');
const url = pathToFileURL(resolve(here, file)).href + (query ? '?' + query : '');

const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH || undefined });

async function openPage() {
  const page = await browser.newPage({ viewport: { width: 1920, height: 1080 }, deviceScaleFactor: 1 });
  page.on('pageerror', e => { console.error('page error:', e.message); process.exit(1); });
  await page.goto(url);
  await page.evaluate(() => window.ready);
  return page;
}
const shot = async (page, t) => { await page.evaluate(t => window.render(t), t); return page.screenshot({ type: 'png' }); };

function encoder(out) {
  const ff = spawn('ffmpeg', ['-y', '-loglevel', 'error', '-f', 'image2pipe', '-framerate', String(fps), '-i', '-',
    '-c:v', 'libx264', '-preset', 'slow', '-crf', '16', '-pix_fmt', 'yuv420p', out], { stdio: ['pipe', 'inherit', 'inherit'] });
  return { ff, done: new Promise(r => ff.on('close', r)) };
}

const page = await openPage();
const duration = await page.evaluate(() => window.DURATION);

if (cuesOut) {
  writeFileSync(cuesOut, JSON.stringify({ duration, cues: await page.evaluate(() => window.CUES) }));
} else if (stills) {
  mkdirSync(stills, { recursive: true });
  for (const t of rest.map(Number)) writeFileSync(`${stills}/t${t.toFixed(2)}.png`, await shot(page, t));
} else {
  // Frames are independent, so split the timeline into chunks, encode each, then concat.
  const out = resolve(rest[0]), total = Math.round(duration * fps), tmp = mkdtempSync(join(tmpdir(), 'gymclaw-film-'));
  const size = Math.ceil(total / workers);
  let doneFrames = 0;
  const parts = await Promise.all(Array.from({ length: workers }, async (_, w) => {
    const p = w === 0 ? page : await openPage(), part = join(tmp, `part${w}.mp4`), { ff, done } = encoder(part);
    for (let f = w * size; f < Math.min(total, (w + 1) * size); f++) {
      if (!ff.stdin.write(await shot(p, f / fps))) await new Promise(r => ff.stdin.once('drain', r));
      if (++doneFrames % fps === 0) process.stdout.write(`\r${doneFrames}/${total}`);
    }
    ff.stdin.end(); await done; return part;
  }));
  writeFileSync(join(tmp, 'list.txt'), parts.map(p => `file '${p}'`).join('\n'));
  await new Promise(r => spawn('ffmpeg', ['-y', '-loglevel', 'error', '-f', 'concat', '-safe', '0', '-i', join(tmp, 'list.txt'),
    ...(audio ? ['-i', audio, '-c:a', 'aac', '-b:a', '192k', '-shortest'] : []), '-c:v', 'copy', '-movflags', '+faststart', out],
    { stdio: 'inherit' }).on('close', r));
  rmSync(tmp, { recursive: true });
  console.log(`\nwrote ${out}`);
}
await browser.close();
