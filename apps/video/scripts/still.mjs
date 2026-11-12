// Capture still frames for review: node still.mjs <video> <lang> <t,t,t> <outDir>
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { serve } from './serve.mjs';
const require = createRequire(import.meta.url);
const { chromium } = require('playwright');
const [video, lang, times, outDir, scale = '1'] = process.argv.slice(2);
const server = await serve();
const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1920, height: 1080 }, deviceScaleFactor: Number(scale) });
page.on('console', (m) => console.log('[page]', m.text()));
page.on('pageerror', (e) => console.log('[error]', e.message));
await page.goto(`${server.url}/src/index.html?render=1`);
const info = await page.evaluate(([v, l]) => window.__load(v, l), [video, lang]);
const list = times === 'scenes' ? info.scenes.map((s) => Math.min(s.end - 0.5, s.start + (s.end - s.start) * 0.86)) : times.split(',').map(Number);
for (const t of list) {
  await page.evaluate((x) => window.__seek(x), t);
  await page.screenshot({ path: path.join(outDir, `${video}-${lang}-${t.toFixed(1).padStart(5, '0')}.png`) });
}
console.log(JSON.stringify(info.scenes.map((s) => [s.id, s.start, s.end])), info.duration);
await browser.close();
server.close();
