// Markup builders shared by every scene. Pure string functions: this module is
// imported by the browser (to draw) and by Node (to extract captions and the
// glyphs the fonts need), so nothing here may touch the DOM.

export const L = (en, ja) => ({ en, ja });

const escapeHtml = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

// Copy format: *emphasis*, `code`, newline = line break.
export function fmt(text) {
  return escapeHtml(text)
    .replace(/\*([^*]+)\*/g, '<em>$1</em>')
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\n/g, '<br>');
}

export const a = (fx, start, dur, out) =>
  `data-a="${[fx, start, dur ?? 0.6, out].filter((x) => x !== undefined).join(' ')}"`;

export const LOGO_POLYGONS = [
  '759,203 585,457 759,731',
  '777,203 951,457 777,731',
  '287,303 569,466 737,730 465,594',
  '1249,303 967,466 799,730 1071,594',
  '461,617 623,697 350,819',
  '1075,617 913,697 1186,819',
];

export function logo(cls = '', stagger) {
  const polys = LOGO_POLYGONS.map((points, i) =>
    `<polygon points="${points}" ${stagger === undefined ? '' : a('pop', stagger + [0.0, 0.08, 0.2, 0.28, 0.4, 0.48][i], 0.7)}/>`).join('');
  return `<svg class="logo ${cls}" viewBox="250 165 1036 690" aria-hidden="true">${polys}</svg>`;
}

export const brand = () => `<span class="brand">${logo()}ShogunAgent</span>`;

export const icon = {
  lock: '<svg viewBox="0 0 24 24" class="ic"><rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/></svg>',
  file: '<svg viewBox="0 0 24 24" class="ic"><path d="M7 3h7l5 5v13H7z"/><path d="M14 3v5h5"/></svg>',
  check: '<svg viewBox="0 0 24 24" class="ic"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg>',
  cross: '<svg viewBox="0 0 24 24" class="ic"><path d="M6 6l12 12M18 6L6 18"/></svg>',
  arrow: '<svg viewBox="0 0 24 24" class="ic"><path d="M4 12h15M13 6l6 6-6 6"/></svg>',
  person: '<svg viewBox="0 0 24 24" class="ic"><circle cx="12" cy="8" r="3.6"/><path d="M4.5 20a7.5 7.5 0 0 1 15 0"/></svg>',
  folder: '<svg viewBox="0 0 24 24" class="ic"><path d="M3 7h7l2 2.5h9V19H3z"/></svg>',
  flag: '<svg viewBox="0 0 24 24" class="ic"><path d="M6 21V4h11l-2 4 2 4H6"/></svg>',
  loop: '<svg viewBox="0 0 24 24" class="ic"><path d="M19 12a7 7 0 1 1-2.2-5.1"/><path d="M19 4v4h-4"/></svg>',
  clock: '<svg viewBox="0 0 24 24" class="ic"><circle cx="12" cy="12" r="8"/><path d="M12 7.5V12l3 2"/></svg>',
  shield: '<svg viewBox="0 0 24 24" class="ic"><path d="M12 3l7 3v5.5c0 4.3-2.9 7.6-7 9.5-4.1-1.9-7-5.2-7-9.5V6z"/></svg>',
  code: '<svg viewBox="0 0 24 24" class="ic"><path d="M8.5 7L3.5 12l5 5M15.5 7l5 5-5 5"/></svg>',
  box: '<svg viewBox="0 0 24 24" class="ic"><path d="M12 3l8 4.5v9L12 21l-8-4.5v-9z"/><path d="M4 7.5l8 4.5 8-4.5M12 12v9"/></svg>',
  send: '<svg viewBox="0 0 24 24" class="ic"><path d="M4 12l16-8-6 16-2.5-6.5z"/></svg>',
  eye: '<svg viewBox="0 0 24 24" class="ic"><path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12z"/><circle cx="12" cy="12" r="2.8"/></svg>',
  hand: '<svg viewBox="0 0 24 24" class="ic"><path d="M5 12.5l4.5 4.5L19 7.5"/><circle cx="12" cy="12" r="9.5"/></svg>',
};

// A chat window. `rows` are already-built message strings.
export function chat({ cls = '', name, sub, rows, attrs = '' }) {
  return `<div class="chat ${cls}" ${attrs}><div class="chat-head"><span class="chat-glyph">✳</span><span class="chat-name">${name}<small>${sub}</small></span></div><div class="chat-body">${rows.join('')}</div></div>`;
}
export const userMsg = (text, attrs = '', typed) =>
  `<div class="msg user" ${attrs}><p ${typed ? `data-type="${typed}"` : ''}>${text}</p></div>`;
export const agentMsg = (inner, attrs = '') =>
  `<div class="msg agent" ${attrs}>${logo('msg-logo')}<div class="msg-text">${inner}</div></div>`;
export const toolChip = (name, tags, attrs = '') =>
  `<div class="tool" ${attrs}><i></i><code>${name}</code>${tags.map((tag) => `<span>${tag}</span>`).join('')}</div>`;

export function note({ cls = '', meta, quote, foot = '', attrs = '' }) {
  return `<div class="note ${cls}" ${attrs}><span class="eyebrow">${meta}</span><blockquote>${quote}</blockquote>${foot ? `<span class="note-foot">${foot}</span>` : ''}</div>`;
}

export function store({ cls = '', title, file, rows = [], attrs = '' }) {
  return `<div class="store ${cls}" ${attrs}><div class="store-head"><span class="store-icon">${icon.lock}</span><span class="store-title">${title}<small>${file}</small></span></div>${rows.length ? `<ul class="store-rows">${rows.join('')}</ul>` : ''}</div>`;
}

// A typed state card: person / project / commitment / open loop.
export function stateCard({ kind, glyph, label, title, quote, band, bandLabel, sources, attrs = '', cls = '' }) {
  return `<div class="sc sc-${kind} ${cls}" ${attrs}><div class="sc-top"><span class="sc-glyph">${glyph}</span><span class="eyebrow">${label}</span><span class="band band-${band}">${bandLabel}</span></div><h3>${title}</h3>${quote ? `<p class="sc-quote">${quote}</p>` : ''}<div class="sc-foot">${icon.file}<span>${sources}</span></div></div>`;
}

export function terminal({ title, lines, attrs = '', cls = '' }) {
  return `<div class="term ${cls}" ${attrs}><div class="term-bar"><i></i><i></i><i></i><span>${title}</span></div><div class="term-body">${lines.join('')}</div></div>`;
}
export const cmd = (text, start, cps = 60, attrs = '') =>
  `<div class="tl cmd" ${attrs}><b>$</b><span data-type="${start} ${cps}">${text}</span></div>`;
export const out = (html, attrs = '', cls = '') => `<div class="tl out ${cls}" ${attrs}>${html}</div>`;

export const pill = (text, cls = '', attrs = '') => `<span class="pill ${cls}" ${attrs}>${text}</span>`;

export const headline = (t, h, p, start = 0.5, extra = '') =>
  `<div class="copy"><h1 ${a('rise', start, 0.8)}>${t(h)}</h1>${p ? `<p ${a('rise', start + 0.5, 0.8)}>${t(p)}</p>` : ''}${extra}</div>`;
