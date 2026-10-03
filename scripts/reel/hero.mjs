// Screenshots hero.html to a PNG: node hero.mjs ../../assets/hero.png  (CHROME_PATH = any Chromium)
import { chromium } from 'playwright-core';
import { pathToFileURL } from 'node:url';
const out = process.argv[2] ?? 'hero.png';
const b = await chromium.launch({ executablePath: process.env.CHROME_PATH });
const p = await b.newPage({ viewport: { width: 1280, height: 640 }, deviceScaleFactor: 2 });
await p.goto(pathToFileURL(new URL('hero.html', import.meta.url).pathname).href);
await p.evaluate(() => document.fonts.ready);
await p.screenshot({ path: out });
await b.close();
