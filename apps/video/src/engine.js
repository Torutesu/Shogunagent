// Deterministic timeline engine. Nothing here reads the clock: every visual is a
// pure function of the time passed to seek(), so a frame renders the same on
// every run and the renderer can capture frames in any order.
//
// Markup vocabulary (all times are seconds, relative to the scene start):
//   data-a="fx in [dur] [out]"   enter with an effect, optionally leave at `out`
//   data-type="in cps"           reveal the element's text character by character
//   data-v="name in [dur]; …"    drive the CSS variable --name from 0 to 1
//   data-loop="name period [phase]"  drive --name as a repeating 0→1 ramp
//   data-along="#path period [phase] [in]"  move an SVG node along a path
import { VIDEOS, FPS } from './videos.js';
import { brand, fmt } from './ui.js';

const clamp = (x) => Math.max(0, Math.min(1, x));
const easeOut = (x) => 1 - Math.pow(1 - x, 3);
const easeInOut = (x) => (x < 0.5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2);
const easeBack = (x) => 1 + 2.4 * Math.pow(x - 1, 3) + 1.4 * Math.pow(x - 1, 2);

const SCENE_IN = 0.45;
const SCENE_OUT = 0.3;

const FX = {
  fade: () => ({}),
  rise: (p, q) => ({ translate: `0 ${(1 - p) * 30 - q * 12}px` }),
  drop: (p, q) => ({ translate: `0 ${(p - 1) * 30 + q * 12}px` }),
  left: (p) => ({ translate: `${(p - 1) * 48}px 0` }),
  right: (p) => ({ translate: `${(1 - p) * 48}px 0` }),
  pop: (p, q, raw) => ({ scale: String(0.9 + 0.1 * easeBack(raw)) }),
  zoom: (p) => ({ scale: String(1.1 - 0.1 * p) }),
  grow: (p) => ({ scale: `${p} 1` }),
  wipe: (p) => ({ clipPath: `inset(0 ${(1 - p) * 100}% 0 0)` }),
  draw: (p) => ({ strokeDashoffset: String(1 - p) }),
};

function nums(text) {
  return text.trim().split(/\s+/).map(Number);
}

function prepareScene(el) {
  const anims = [...el.querySelectorAll('[data-a]')].map((node) => {
    const [fx, ...rest] = node.dataset.a.trim().split(/\s+/);
    const [start = 0, dur = 0.6, out] = rest.map(Number);
    if (!FX[fx]) throw new Error(`unknown effect "${fx}"`);
    if (fx === 'draw') {
      node.setAttribute('pathLength', '1');
      node.style.strokeDasharray = '1';
    }
    return { node, fx, start, dur, out };
  });
  const typed = [...el.querySelectorAll('[data-type]')].map((node) => {
    const [start, cps = 34] = nums(node.dataset.type);
    const chars = [...node.textContent];
    node.innerHTML = '<span class="tv"></span><span class="caret"></span><span class="th"></span>';
    const [shown, caret, hidden] = node.children;
    return { shown, caret, hidden, chars, start, cps, last: -1 };
  });
  const vars = [];
  for (const node of el.querySelectorAll('[data-v]')) {
    for (const part of node.dataset.v.split(';')) {
      const [name, ...rest] = part.trim().split(/\s+/);
      const [start = 0, dur = 0.6] = rest.map(Number);
      vars.push({ node, name, start, dur });
    }
  }
  const loops = [...el.querySelectorAll('[data-loop]')].map((node) => {
    const [name, ...rest] = node.dataset.loop.trim().split(/\s+/);
    const [period = 2, phase = 0] = rest.map(Number);
    return { node, name, period, phase };
  });
  const along = [...el.querySelectorAll('[data-along]')].map((node) => {
    const [selector, ...rest] = node.dataset.along.trim().split(/\s+/);
    const [period = 2, phase = 0, start = 0] = rest.map(Number);
    const path = el.querySelector(selector);
    return { node, path, length: path.getTotalLength(), period, phase, start };
  });
  return { anims, typed, vars, loops, along };
}

function renderScene(scene, t) {
  for (const a of scene.parts.anims) {
    const raw = clamp((t - a.start) / a.dur);
    const p = easeOut(raw);
    const q = a.out === undefined ? 0 : easeInOut(clamp((t - a.out) / 0.4));
    const style = a.node.style;
    style.opacity = String(p * (1 - q));
    Object.assign(style, FX[a.fx](p, q, raw));
  }
  for (const y of scene.parts.typed) {
    const n = Math.max(0, Math.min(y.chars.length, Math.floor((t - y.start) * y.cps)));
    if (n !== y.last) {
      y.shown.textContent = y.chars.slice(0, n).join('');
      y.hidden.textContent = y.chars.slice(n).join('');
      y.last = n;
    }
    const typing = t >= y.start && n < y.chars.length;
    y.caret.style.opacity = typing ? '1' : '0';
  }
  for (const v of scene.parts.vars) {
    v.node.style.setProperty(`--${v.name}`, String(easeInOut(clamp((t - v.start) / v.dur))));
  }
  for (const l of scene.parts.loops) {
    const x = (((t / l.period + l.phase) % 1) + 1) % 1;
    l.node.style.setProperty(`--${l.name}`, String(x));
  }
  for (const m of scene.parts.along) {
    const x = (((Math.max(0, t - m.start) / m.period + m.phase) % 1) + 1) % 1;
    const point = m.path.getPointAtLength(easeInOut(x) * m.length);
    m.node.setAttribute('cx', point.x.toFixed(2));
    m.node.setAttribute('cy', point.y.toFixed(2));
    m.node.style.opacity = String(t < m.start ? 0 : Math.sin(Math.PI * x) ** 0.5);
  }
}

export function build(stage, videoId, lang) {
  const video = VIDEOS.find((v) => v.id === videoId);
  if (!video) throw new Error(`unknown video "${videoId}"`);
  const t = (pair) => fmt(typeof pair === 'string' ? pair : pair[lang]);
  document.documentElement.lang = lang;
  stage.className = `stage ${video.kind}`;

  let cursor = 0;
  const scenes = video.scenes.map((def) => {
    const scene = { def, start: cursor, end: cursor + def.dur };
    cursor += def.dur;
    return scene;
  });
  const duration = cursor;

  const chrome = video.kind === 'explainer'
    ? `<header class="chrome">${brand()}<span class="chrome-rule"></span><span class="chrome-chapter"><b>${String(video.n).padStart(2, '0')}</b>${t(video.title)}</span><span class="chrome-note">${t({ en: 'Illustrative · fictional data', ja: '説明用の例 · 架空のデータ' })}</span></header><div class="cap"><p></p></div>`
    : `<header class="chrome">${brand()}<span class="chrome-note">${t({ en: 'Illustrative · fictional data', ja: '説明用の例 · 架空のデータ' })}</span></header>`;
  stage.innerHTML = `${scenes.map((s, i) => `<section class="scene scene-${s.def.id}" data-i="${i}">${s.def.html(t, video)}</section>`).join('')}${chrome}<div class="progress"><i></i></div>`;

  for (const beat of stage.querySelectorAll('.beat')) {
    const wrap = document.createElement('div');
    wrap.className = 'beat-fit';
    for (const child of [...beat.children]) if (!child.classList.contains('beat-title')) wrap.append(child);
    beat.append(wrap);
  }

  const sections = [...stage.querySelectorAll('.scene')];
  scenes.forEach((scene, i) => {
    scene.el = sections[i];
    scene.parts = prepareScene(scene.el);
  });

  const captions = [];
  for (const scene of scenes) {
    const list = scene.def.captions || [];
    list.forEach(([at, text], i) => {
      const next = list[i + 1];
      captions.push({ start: scene.start + at, end: next ? scene.start + next[0] : scene.end - 0.1, html: t(text) });
    });
  }

  const header = stage.querySelector('.chrome');
  const capBox = stage.querySelector('.cap');
  const capText = capBox && capBox.querySelector('p');
  const bar = stage.querySelector('.progress i');
  let shownCaption = null;

  function seek(T) {
    let chromeAlpha = 0;
    scenes.forEach((scene, i) => {
      const local = T - scene.start;
      const active = T >= scene.start && T < scene.end;
      if (!active) {
        scene.el.style.visibility = 'hidden';
        return;
      }
      const first = i === 0;
      const fadeIn = first ? clamp(local / 0.3) : clamp(local / SCENE_IN);
      const fadeOut = clamp((scene.end - T) / SCENE_OUT);
      const alpha = easeInOut(fadeIn) * easeInOut(fadeOut);
      scene.el.style.visibility = 'visible';
      scene.el.style.opacity = String(alpha);
      scene.el.style.translate = `0 ${(1 - easeOut(fadeIn)) * 14}px`;
      renderScene(scene, local);
      // Chrome stays lit across neighbouring scenes that both show it.
      const prev = scenes[i - 1];
      const next = scenes[i + 1];
      const wantsIn = !scene.def.bare;
      const holdIn = wantsIn && prev && !prev.def.bare ? 1 : easeInOut(fadeIn);
      const holdOut = wantsIn && next && !next.def.bare ? 1 : easeInOut(fadeOut);
      chromeAlpha = wantsIn ? holdIn * holdOut : 0;
    });
    header.style.opacity = String(chromeAlpha);
    if (capBox) {
      const current = captions.find((c) => T >= c.start && T < c.end) || null;
      if (current !== shownCaption) {
        capText.innerHTML = current ? current.html : '';
        shownCaption = current;
      }
      const alpha = current ? clamp((T - current.start) / 0.25) * clamp((current.end - T) / 0.2) : 0;
      capBox.style.opacity = String(easeInOut(alpha));
      capBox.style.translate = `0 ${current ? (1 - easeOut(clamp((T - current.start) / 0.35))) * 10 : 0}px`;
    }
    bar.style.scale = `${clamp(T / duration)} 1`;
  }

  // Scale each explainer beat to fill its area, whatever the language's line
  // lengths are. Runs once the fonts are in, before the first frame.
  function fit() {
    for (const beat of stage.querySelectorAll('.beat-fit')) {
      beat.style.zoom = '1';
      const area = beat.parentElement.getBoundingClientRect();
      const box = beat.getBoundingClientRect();
      const room = area.height - 64;
      beat.style.zoom = String(Math.min(area.width / box.width, room / box.height, 1.5).toFixed(3));
    }
  }

  return { video, duration, fps: FPS, scenes, captions, seek, fit };
}
