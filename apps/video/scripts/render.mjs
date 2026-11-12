#!/usr/bin/env node
// Render the compositions to MP4.
//
//   node apps/video/scripts/render.mjs                       every video, both languages
//   node apps/video/scripts/render.mjs --video launch --lang ja
//   node apps/video/scripts/render.mjs --jobs 4 --no-audio --out /tmp/videos
//
// Needs Playwright with Chromium, ffmpeg (libx264, aac) and Python 3 with numpy
// for the music bed. Each frame is a pure function of its timestamp, so frames
// are captured one by one and piped straight into the encoder.
import { createRequire } from 'node:module';
import { spawn, execFileSync } from 'node:child_process';
import { mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { once } from 'node:events';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { serve } from './serve.mjs';
import { VIDEOS, LANGS, FPS, WIDTH, HEIGHT, durationOf } from '../src/videos.js';
import { subtitleFiles } from './captions.mjs';

const require = createRequire(import.meta.url);
const { chromium } = require('playwright');
const HERE = path.dirname(fileURLToPath(import.meta.url));

function parseArgs(argv) {
  const args = { video: null, lang: null, jobs: Math.max(1, Math.min(4, os.cpus().length)), audio: true, out: path.resolve(HERE, '..', 'out') };
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === '--video') args.video = argv[++i];
    else if (a === '--lang') args.lang = argv[++i];
    else if (a === '--jobs') args.jobs = Number(argv[++i]);
    else if (a === '--out') args.out = path.resolve(argv[++i]);
    else if (a === '--no-audio') args.audio = false;
    else throw new Error(`unknown argument ${a}`);
  }
  return args;
}

async function renderOne(job, server, args) {
  const { video, lang } = job;
  const dir = path.join(args.out, lang);
  mkdirSync(dir, { recursive: true });
  const target = path.join(dir, `${video.file}.mp4`);
  const silent = path.join(dir, `.${video.file}.video.mp4`);
  const wav = path.join(dir, `.${video.file}.wav`);

  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: WIDTH, height: HEIGHT }, deviceScaleFactor: 1 });
  const failures = [];
  page.on('pageerror', (e) => failures.push(e.message));
  await page.goto(`${server.url}/src/index.html?render=1`);
  const info = await page.evaluate(([v, l]) => window.__load(v, l), [video.id, lang]);
  const frames = Math.round(info.duration * FPS);

  const ffmpeg = spawn('ffmpeg', ['-loglevel', 'error', '-y', '-f', 'image2pipe', '-framerate', String(FPS), '-c:v', 'png', '-i', '-',
    '-c:v', 'libx264', '-preset', 'slow', '-crf', '15', '-tune', 'animation', '-pix_fmt', 'yuv420p',
    '-color_primaries', 'bt709', '-color_trc', 'bt709', '-colorspace', 'bt709', '-movflags', '+faststart', silent], { stdio: ['pipe', 'inherit', 'inherit'] });
  const done = once(ffmpeg, 'close');
  for (let f = 0; f < frames; f += 1) {
    await page.evaluate((t) => window.__seek(t), f / FPS);
    const png = await page.screenshot({ type: 'png' });
    if (!ffmpeg.stdin.write(png)) await once(ffmpeg.stdin, 'drain');
    if (f === Math.round(info.scenes[Math.min(1, info.scenes.length - 1)].end * FPS) - 20) {
      // Poster: late in the opening chapter card (explainers) or the title card (launch film).
      writeFileSync(path.join(dir, `${video.file}.poster.png`), png);
    }
  }
  ffmpeg.stdin.end();
  const [code] = await done;
  await browser.close();
  if (code !== 0) throw new Error(`ffmpeg exited ${code} for ${video.file} [${lang}]`);
  if (failures.length) throw new Error(`${video.file} [${lang}] page errors: ${failures.join('; ')}`);

  if (args.audio) {
    const cues = info.scenes.slice(1).map((s) => s.start.toFixed(2)).join(',');
    execFileSync('python3', [path.join(HERE, 'audio.py'), String(info.duration), wav, cues]);
    execFileSync('ffmpeg', ['-loglevel', 'error', '-y', '-i', silent, '-i', wav, '-c:v', 'copy', '-c:a', 'aac', '-b:a', '192k', '-shortest', '-movflags', '+faststart', target]);
    rmSync(wav);
    rmSync(silent);
  } else {
    execFileSync('ffmpeg', ['-loglevel', 'error', '-y', '-i', silent, '-c', 'copy', '-movflags', '+faststart', target]);
    rmSync(silent);
  }
  for (const [ext, text] of Object.entries(subtitleFiles(video, lang))) writeFileSync(path.join(dir, `${video.file}.${ext}`), text);
  return { target, frames };
}

const args = parseArgs(process.argv.slice(2));
const jobs = [];
for (const video of VIDEOS) {
  if (args.video && args.video !== video.id) continue;
  for (const lang of LANGS) {
    if (args.lang && args.lang !== lang) continue;
    jobs.push({ video, lang });
  }
}
if (!jobs.length) throw new Error(`nothing to render; videos are ${VIDEOS.map((v) => v.id).join(', ')}`);
jobs.sort((x, y) => durationOf(y.video) - durationOf(x.video));

const server = await serve();
const started = Date.now();
let next = 0;
async function worker() {
  while (next < jobs.length) {
    const job = jobs[next++];
    const t0 = Date.now();
    const { target, frames } = await renderOne(job, server, args);
    console.log(`${path.relative(process.cwd(), target)}  ${frames} frames  ${((Date.now() - t0) / 1000).toFixed(0)}s`);
  }
}
await Promise.all(Array.from({ length: Math.min(args.jobs, jobs.length) }, worker));
server.close();
console.log(`rendered ${jobs.length} file(s) in ${((Date.now() - started) / 1000).toFixed(0)}s`);
