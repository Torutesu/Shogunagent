#!/usr/bin/env node
// Subset the fonts to the glyphs the videos actually draw, so the repository
// carries tens of kilobytes instead of the full Japanese families.
//
//   node apps/video/scripts/fonts.mjs --from <dir containing @fontsource/*>   rebuild fonts/
//   node apps/video/scripts/fonts.mjs --check                                 fail if copy uses a glyph outside the subset
//
// Sources are the @fontsource packages (noto-sans-jp, noto-serif-jp, gelasio,
// jetbrains-mono), all SIL OFL 1.1. Needs `pyftsubset` (pip install fonttools brotli).
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { VIDEOS, LANGS } from '../src/videos.js';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const MANIFEST = path.join(ROOT, 'fonts', 'charset.txt');

// Everything a scene can put on screen: render each scene's markup in each
// language and keep the characters. ASCII is always included.
export function requiredCharacters() {
  const seen = new Set();
  for (let c = 0x20; c < 0x7f; c += 1) seen.add(String.fromCharCode(c));
  const add = (text) => { for (const ch of text.replace(/&nbsp;/g, ' ').replace(/&amp;/g, '&').replace(/&lt;/g, '<').replace(/&gt;/g, '>')) if (ch > ' ') seen.add(ch); };
  for (const video of VIDEOS) {
    for (const lang of LANGS) {
      const t = (pair) => (typeof pair === 'string' ? pair : pair[lang]);
      add(t(video.title));
      for (const scene of video.scenes) {
        add(scene.html(t, video));
        for (const [, text] of scene.captions || []) add(text[lang]);
      }
    }
  }
  add('説明用の例 · 架空のデータ Illustrative fictional data 解説 次の解説：✳•…—–“”‘’「」、。・：？！');
  return [...seen].sort().join('');
}

const SOURCES = [
  ['noto-sans-jp/files/noto-sans-jp-japanese-400-normal.woff2', 'noto-sans-jp-400.woff2'],
  ['noto-sans-jp/files/noto-sans-jp-japanese-500-normal.woff2', 'noto-sans-jp-500.woff2'],
  ['noto-sans-jp/files/noto-sans-jp-japanese-700-normal.woff2', 'noto-sans-jp-700.woff2'],
  ['noto-serif-jp/files/noto-serif-jp-japanese-500-normal.woff2', 'noto-serif-jp-500.woff2'],
  ['gelasio/files/gelasio-latin-400-italic.woff2', 'gelasio-400-italic.woff2'],
  ['jetbrains-mono/files/jetbrains-mono-latin-400-normal.woff2', 'jetbrains-mono-400.woff2'],
  ['jetbrains-mono/files/jetbrains-mono-latin-700-normal.woff2', 'jetbrains-mono-700.woff2'],
];

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const chars = requiredCharacters();
  if (process.argv.includes('--check')) {
    const have = new Set(readFileSync(MANIFEST, 'utf8'));
    const missing = [...chars].filter((ch) => !have.has(ch));
    if (missing.length) {
      console.error(`fonts/ lacks ${missing.length} glyph(s) the copy now uses: ${missing.join(' ')}\nRebuild with: node apps/video/scripts/fonts.mjs --from <@fontsource dir>`);
      process.exit(1);
    }
    console.log(`fonts cover all ${chars.length} characters in the copy`);
  } else {
    const from = process.argv[process.argv.indexOf('--from') + 1];
    if (!process.argv.includes('--from') || !from) throw new Error('pass --from <dir containing @fontsource packages> or --check');
    writeFileSync(MANIFEST, chars);
    for (const [source, target] of SOURCES) {
      execFileSync('pyftsubset', [path.join(from, source), `--text-file=${MANIFEST}`, '--flavor=woff2', '--layout-features=*', `--output-file=${path.join(ROOT, 'fonts', target)}`], { stdio: 'inherit' });
    }
    console.log(`subset ${SOURCES.length} fonts to ${chars.length} characters`);
  }
}
