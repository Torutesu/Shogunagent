// Layout check: every video in every language, sampled late in each scene.
// Fails when something leaves the frame, a beat runs into the caption band,
// a caption overflows its band, or text is clipped by its own box.
import { createRequire } from 'node:module';
import { serve } from './serve.mjs';
import { VIDEOS, LANGS } from '../src/videos.js';

const require = createRequire(import.meta.url);
const { chromium } = require('playwright');
const server = await serve();
const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1920, height: 1080 } });
const errors = [];
page.on('pageerror', (e) => errors.push(`page error: ${e.message}`));
await page.goto(`${server.url}/src/index.html?render=1`);
let frames = 0;
for (const video of VIDEOS) {
  for (const lang of LANGS) {
    const info = await page.evaluate(([v, l]) => window.__load(v, l), [video.id, lang]);
    for (const scene of info.scenes) {
      const times = [scene.start + (scene.end - scene.start) * 0.5, scene.end - 0.5];
      for (const t of times) {
        frames += 1;
        const found = await page.evaluate((time) => {
          window.__seek(time);
          const out = [];
          const scene = [...document.querySelectorAll('.scene')].find((s) => s.style.visibility === 'visible');
          const label = (el) => `${el.tagName.toLowerCase()}.${String(el.className.baseVal ?? el.className).split(' ')[0]} "${(el.textContent || '').trim().slice(0, 28)}"`;
          for (const el of scene.querySelectorAll('*')) {
            if (el.closest('svg') && el.tagName !== 'svg') continue;
            const r = el.getBoundingClientRect();
            if (!r.width || !r.height || Number(getComputedStyle(el).opacity) === 0) continue;
            if (r.left < -1 || r.top < -1 || r.right > 1921 || r.bottom > 1081) out.push(`outside frame: ${label(el)}`);
            if (el.matches('.msg p,.callout p,.sc h3,.tile h3,.rung-text,.log-row span,.gate-title,.ln,.tl') && el.scrollWidth > el.clientWidth + 2 && getComputedStyle(el).overflow !== 'visible') out.push(`clipped: ${label(el)}`);
          }
          for (const el of scene.querySelectorAll('.ln,.tl,.hub-node,.step,.st,.toolgrid code')) {
            const box = el.closest('.code,.term,.hub,.pipe,.statuses,.toolgrid').getBoundingClientRect();
            const r = el.getBoundingClientRect();
            const inner = el.matches('.ln,.tl') ? el.scrollWidth * (r.width / el.clientWidth) : r.width;
            if (r.left + inner > box.right + 2) out.push(`wider than its panel: ${label(el)}`);
          }
          const fit = scene.querySelector('.beat-fit');
          const cap = document.querySelector('.cap');
          if (fit && cap) {
            if (fit.getBoundingClientRect().bottom > cap.getBoundingClientRect().top + 1) out.push('beat overlaps the caption band');
            const p = cap.querySelector('p');
            if (p.getBoundingClientRect().height > cap.getBoundingClientRect().height + 1) out.push(`caption too tall: "${p.textContent.slice(0, 30)}"`);
          }
          return out;
        }, t);
        for (const f of new Set(found)) errors.push(`${video.file} [${lang}] ${scene.id} @${t.toFixed(1)}s: ${f}`);
      }
    }
  }
}
await browser.close();
server.close();
if (errors.length) {
  console.error(errors.join('\n'));
  console.error(`\n${errors.length} layout problem(s) in ${frames} sampled frames`);
  process.exit(1);
}
console.log(`layout ok: ${frames} sampled frames, ${VIDEOS.length} videos x ${LANGS.length} languages`);
