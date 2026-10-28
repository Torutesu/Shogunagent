#!/usr/bin/env node
// shogunagent.com, generated from content/site.json.
//
// Agent-first means the machine-readable documents come first and the HTML is derived from the
// same data (CLAUDE.md invariant 12): `render()` produces index.md, llms.txt, llms-full.txt,
// setup-prompt.txt, robots.txt and sitemap.xml, and index.html is a view over the same object.
// No framework, no runtime dependency: Node 22 and this file.
//
//   node build.mjs            writes dist/
//   import { render } ...     returns { "/index.html": string, ... } for tests

import { readFileSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

/** Escape text for an HTML text node or attribute value. */
export function esc(s) {
  return String(s)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

/** `code` spans in site.json prose become <code> in HTML; everything else is escaped. */
function inline(s) {
  return esc(s).replace(/`([^`]+)`/g, "<code>$1</code>");
}

function surfaceUrl(site, s) {
  switch (s.host) {
    case "site":
      return site.url + s.path;
    case "api":
      return site.api_url + s.path;
    case "repo":
      return `${site.repo}/blob/main/${s.path}`;
    case "registry":
      return `https://registry.modelcontextprotocol.io/servers/${s.path}`;
    default:
      throw new Error(`unknown surface host: ${s.host}`);
  }
}

const STATUS_LABEL = { live: "live", phase1: "Phase 1" };

/** Every tool wire name, in page order. The HTML and the Markdown must list the same set. */
export function toolNames(site) {
  return site.tools.groups.flatMap((g) => g.items.map((t) => t.name));
}

// ───────────────────────────── Markdown ─────────────────────────────

/** The page as Markdown: `/index.md`, and what `Accept: text/markdown` returns for `/`. */
export function renderMarkdown(site) {
  const L = [];
  const p = (...xs) => L.push(...xs, "");
  p(`# ${site.name}`);
  p(`> ${site.tagline} ${site.motto}`);
  p(site.one_sentence);
  p(`**Status:** ${site.status_line}`);
  p("## Get started", "", site.setup.requirements, "", site.setup.unsupported);
  p(`[Published ${site.setup.version} release](${site.setup.release_url})`, "", "1. Download and verify ONE archive. These commands create a new private preview directory and refuse to overwrite an existing one. Review commands before running; do not enable shell tracing.");
  for (const platform of site.setup.platforms) {
    p(`### ${platform.label}`, `[Archive](${platform.archive}) · [Checksum](${platform.checksum})`, "", "```bash", platform.command, "```");
  }
  for (const step of site.setup.steps) p(`### ${step.title}`, step.description, "", "```bash", step.command, "```");
  p(`### ${site.setup.connect}`, site.setup.connect_text, `[Source CLI guide](${site.setup.docs_url})`);
  for (const error of site.setup.errors) p(`**${error.title}:** ${error.body}`);
  p(
    "Reading this as an agent? This page is also served as Markdown for `Accept: text/markdown`, " +
      `and indexed at ${site.url}/llms.txt. Everything that sends, posts or pays is L3 and waits for a human at an approval URL; do not try to route around it.`
  );

  p("## For agents");
  p("| Surface | What | Why an agent needs it | Status |", "|---|---|---|---|");
  for (const s of site.surfaces) {
    L.push(`| ${surfaceUrl(site, s)} | ${s.what} | ${s.why} | ${STATUS_LABEL[s.status]} |`);
  }
  L.push("");
  p("Setup prompt for a coding agent (also at `/setup-prompt.txt`):", "", "```", site.setup_prompt, "```");
  p(`Works with: ${site.works_with.join(", ")}.`);

  p("## What it is");
  p(site.developer_30s);
  p(site.not_a_memory_api);
  for (const pl of site.pillars) {
    p(`### ${pl.title}`, pl.claim, "", `*Proof in the code:* ${pl.proof}`);
  }

  p("## Three faces, one behavior");
  p(site.faces.intro, "", "MCP:", "", "```json", site.faces.mcp, "```", "", "REST:", "", "```sh", site.faces.rest, "```", "", "CLI:", "", "```sh", site.faces.cli, "```");
  p("Every response carries `usage`, `latency_ms`, `hints` and, on error, `next_action`:", "", "```json", site.faces.response, "```");
  p("An exhausted wallet answers 402 in the x402 `payment-required` shape:", "", "```http", site.faces.payment_required, "```");
  p("An L3 action answers with an approval URL, never with a side effect:", "", "```json", site.faces.l3, "```");

  p("## Permission tiers");
  p("| Tier | Name | What happens | Examples | MCP annotation | Cost |", "|---|---|---|---|---|---|");
  for (const t of site.tiers) {
    L.push(`| ${t.level} | ${t.name} | ${t.what} | ${t.examples} | ${t.mcp} | ${t.cost} |`);
  }
  L.push("");

  p("## Tools");
  p(site.tools.note);
  for (const g of site.tools.groups) {
    p(`### ${g.title}`);
    for (const t of g.items) L.push(`- \`${t.name}\`: ${t.task}`);
    L.push("");
  }

  p("## Pricing");
  p(site.pricing.line);
  p("| Metered item | Unit price | Notes |", "|---|---|---|");
  for (const r of site.pricing.rows) L.push(`| ${r.item} | ${r.price} | ${r.note} |`);
  L.push("");
  p(site.pricing.disclaimer);

  p("## Runs everywhere");
  for (const d of site.deploy) p(`### ${d.title}`, d.body);

  p("## Not a memory API");
  p(site.comparison.intro);
  p("| | Memory APIs | ShogunAgent |", "|---|---|---|");
  for (const r of site.comparison.rows) L.push(`| ${r.axis} | ${r.memory} | ${r.shogun} |`);
  L.push("");

  p("## Open source");
  p(`**Open (${site.license}):** ${site.open_source.open}`, "", `**Closed:** ${site.open_source.closed}`, "", site.open_source.telemetry);
  p(`Repository: ${site.repo}`);

  p("## FAQ");
  for (const f of site.faq) p(`### ${f.q}`, f.a);

  return L.join("\n").trimEnd() + "\n";
}

/** `/llms.txt` per llmstxt.org: H1, blockquote, short prose, then link lists. */
export function renderLlmsTxt(site) {
  const live = site.surfaces.filter((s) => s.status === "live");
  const planned = site.surfaces.filter((s) => s.status !== "live");
  const L = [];
  const p = (...xs) => L.push(...xs, "");
  p(`# ${site.name}`);
  p(`> ${site.tagline} ${site.one_sentence}`);
  p(site.developer_30s);
  p(`Status: ${site.status_line}`);
  p(
    "Rules an agent should know before calling anything: reads are L1 and free; writes to the user's data are L2 and return `input_required`; " +
      "sending, posting and paying are L3 and return an approval URL for a human. Every write takes an idempotency key. Every response carries `usage`, `latency_ms` and, on error, `next_action`."
  );
  p("## Read first");
  for (const s of live) L.push(`- [${s.path}](${surfaceUrl(site, s)}): ${s.what}`);
  L.push(`- [README](${site.repo}#readme): strategy, pillars, pricing and roadmap in full (English and Japanese)`);
  L.push(`- [Linux ${site.setup.version} release](${site.setup.release_url}): x64/ARM64 archives and checksums; npm/PyPI and hosted access remain unpublished`);
  L.push(`- [First local success](${site.url}/index.md#get-started): platform requirements, fresh encrypted preview store, authenticated ingest and recall; no model calls`);
  L.push(`- [Source CLI setup](${site.setup.docs_url}): existing-store prerequisites and read-only MCP; no working npx install is advertised`);
  L.push(`- [Implementation plan](${site.repo}/blob/main/docs/implementation-plan.md): epics, tasks and acceptance criteria`);
  L.push("");
  p("## Tools (core wire names, frozen)");
  for (const g of site.tools.groups) {
    L.push(`- ${g.title}: ${g.items.map((t) => `\`${t.name}\``).join(", ")}`);
  }
  L.push("");
  p("## Pricing");
  for (const r of site.pricing.rows) L.push(`- ${r.item}: ${r.price}${r.note ? ` (${r.note})` : ""}`);
  L.push("");
  p("## Phase 1 (planned, not live yet)");
  for (const s of planned) L.push(`- [${s.path}](${surfaceUrl(site, s)}): ${s.what}`);
  L.push("");
  p("## Optional");
  L.push(`- [Repository](${site.repo}): ${site.license}, Rust engine, MCP / REST / CLI`);
  L.push(`- [Setup prompt](${site.url}/setup-prompt.txt): paste into a coding agent`);
  return L.join("\n").trimEnd() + "\n";
}

// ───────────────────────────── HTML ─────────────────────────────

const CSS = String.raw`
:root{
  --bg:#070a12;--bg2:#0b1020;--fg:#eef2fa;--muted:#9aa6bf;--faint:#6b768f;
  --glass:rgba(255,255,255,.055);--glass-2:rgba(255,255,255,.09);--glass-strong:rgba(14,19,34,.72);
  --edge:rgba(255,255,255,.13);--edge-hi:rgba(255,255,255,.28);--shadow:0 18px 60px rgba(0,0,0,.45);
  --accent:#8fb0ff;--accent-2:#6ee7ff;--accent-3:#c48dff;--accent-ink:#06080f;
  --ok:#5ee6a8;--ok-soft:rgba(94,230,168,.14);--warn:#ffc57a;--warn-soft:rgba(255,197,122,.14);
  --code-bg:rgba(5,8,16,.62);--code-fg:#dfe6f5;--code-muted:#7f8ca6;--code-k:#8fb0ff;--code-s:#9ee7b5;
  --sans:"IBM Plex Sans",system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;
  --display:"Space Grotesk","IBM Plex Sans",system-ui,sans-serif;
  --mono:"IBM Plex Mono",ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;
  --maxw:1240px;--side:256px;--gap:clamp(16px,4vw,64px);--r:20px;--r-sm:12px;
  color-scheme:dark;
}
@media (prefers-color-scheme:light){:root:not([data-theme="dark"]){
  --bg:#eef2fb;--bg2:#f7f9ff;--fg:#0f1627;--muted:#4f5b75;--faint:#7c879f;
  --glass:rgba(255,255,255,.52);--glass-2:rgba(255,255,255,.72);--glass-strong:rgba(255,255,255,.78);
  --edge:rgba(255,255,255,.85);--edge-hi:rgba(255,255,255,1);--shadow:0 18px 50px rgba(44,74,138,.14);
  --accent:#2c4a8a;--accent-2:#0d8fb8;--accent-3:#7c4dff;--accent-ink:#ffffff;
  --ok:#157a4c;--ok-soft:rgba(21,122,76,.12);--warn:#8a5a1d;--warn-soft:rgba(138,90,29,.12);
  --code-bg:rgba(12,18,34,.92);--code-fg:#e3e9f6;--code-muted:#8a96ae;--code-k:#9db9ff;--code-s:#a4ecbd;
  color-scheme:light;}}
:root[data-theme="light"]{
  --bg:#eef2fb;--bg2:#f7f9ff;--fg:#0f1627;--muted:#4f5b75;--faint:#7c879f;
  --glass:rgba(255,255,255,.52);--glass-2:rgba(255,255,255,.72);--glass-strong:rgba(255,255,255,.78);
  --edge:rgba(255,255,255,.85);--edge-hi:rgba(255,255,255,1);--shadow:0 18px 50px rgba(44,74,138,.14);
  --accent:#2c4a8a;--accent-2:#0d8fb8;--accent-3:#7c4dff;--accent-ink:#ffffff;
  --ok:#157a4c;--ok-soft:rgba(21,122,76,.12);--warn:#8a5a1d;--warn-soft:rgba(138,90,29,.12);
  --code-bg:rgba(12,18,34,.92);--code-fg:#e3e9f6;--code-muted:#8a96ae;--code-k:#9db9ff;--code-s:#a4ecbd;
  color-scheme:light;}

*{box-sizing:border-box}
html{scroll-behavior:smooth;-webkit-text-size-adjust:100%}
body{margin:0;background:var(--bg);color:var(--fg);font-family:var(--sans);font-size:16px;line-height:1.65;
  -webkit-font-smoothing:antialiased;min-height:100vh;position:relative;overflow-x:hidden}
a{color:var(--accent);text-decoration:underline;text-decoration-thickness:1px;text-underline-offset:3px}
a:hover{text-decoration-thickness:2px}
:focus-visible{outline:2px solid var(--accent-2);outline-offset:3px;border-radius:4px}
::selection{background:var(--accent);color:var(--accent-ink)}
code,pre,kbd{font-family:var(--mono);font-size:.92em}
code{background:var(--glass-2);border:1px solid var(--edge);padding:.05em .4em;border-radius:6px}
pre{margin:0;background:var(--code-bg);color:var(--code-fg);padding:16px 18px;border-radius:var(--r-sm);overflow:auto;
  line-height:1.55;font-size:13.5px;tab-size:2;border:1px solid var(--edge);
  box-shadow:inset 0 1px 0 rgba(255,255,255,.08);-webkit-backdrop-filter:blur(14px);backdrop-filter:blur(14px)}
pre code{background:none;border:0;padding:0;font-size:inherit}
h1,h2,h3{font-family:var(--display);font-weight:600;letter-spacing:-.02em;line-height:1.1;margin:0;text-wrap:balance}
h1{font-size:clamp(34px,5.6vw,60px)}
h2{font-size:clamp(26px,3.4vw,38px);margin-bottom:12px}
h3{font-size:19px;letter-spacing:-.01em}
p{margin:0 0 1em}
.muted{color:var(--muted)}
.eyebrow{font-family:var(--mono);font-size:12px;letter-spacing:.14em;text-transform:uppercase;color:var(--accent-2);font-weight:500}
.skip{position:absolute;left:-999px;top:0;background:var(--glass-strong);padding:8px 12px;z-index:99}
.skip:focus{left:8px;top:8px}
.grad{background:linear-gradient(92deg,var(--fg) 0%,var(--accent) 55%,var(--accent-2) 100%);-webkit-background-clip:text;background-clip:text;color:transparent}

/* ambient background: drifting light + fine grid + grain */
.bg{position:fixed;inset:0;z-index:-1;overflow:hidden;pointer-events:none;background:
  radial-gradient(1200px 700px at 80% -10%,color-mix(in oklab,var(--accent-3) 22%,transparent),transparent 60%),
  radial-gradient(900px 600px at -10% 30%,color-mix(in oklab,var(--accent) 26%,transparent),transparent 60%),
  linear-gradient(180deg,var(--bg2),var(--bg))}
.bg i{position:absolute;border-radius:50%;filter:blur(70px);opacity:.55;mix-blend-mode:screen;will-change:transform}
.bg .b1{width:52vmax;height:52vmax;left:-14vmax;top:-18vmax;background:radial-gradient(circle,var(--accent) 0%,transparent 62%);animation:drift1 38s ease-in-out infinite alternate}
.bg .b2{width:46vmax;height:46vmax;right:-16vmax;top:12vh;background:radial-gradient(circle,var(--accent-2) 0%,transparent 62%);animation:drift2 46s ease-in-out infinite alternate;opacity:.38}
.bg .b3{width:40vmax;height:40vmax;left:28vw;bottom:-22vmax;background:radial-gradient(circle,var(--accent-3) 0%,transparent 62%);animation:drift3 54s ease-in-out infinite alternate;opacity:.42}
.bg .grid{position:absolute;inset:0;background-image:linear-gradient(var(--edge) 1px,transparent 1px),linear-gradient(90deg,var(--edge) 1px,transparent 1px);
  background-size:64px 64px;opacity:.28;-webkit-mask-image:radial-gradient(ellipse at 50% 0%,#000 20%,transparent 75%);mask-image:radial-gradient(ellipse at 50% 0%,#000 20%,transparent 75%)}
.bg .grain{position:absolute;inset:0;opacity:.07;background-image:url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='160' height='160'%3E%3Cfilter id='n'%3E%3CfeTurbulence type='fractalNoise' baseFrequency='.9' numOctaves='2' stitchTiles='stitch'/%3E%3C/filter%3E%3Crect width='100%25' height='100%25' filter='url(%23n)'/%3E%3C/svg%3E")}
@media (prefers-color-scheme:light){:root:not([data-theme="dark"]) .bg i{mix-blend-mode:multiply;opacity:.28}}
:root[data-theme="light"] .bg i{mix-blend-mode:multiply;opacity:.28}
@keyframes drift1{to{transform:translate(12vw,10vh) scale(1.12)}}
@keyframes drift2{to{transform:translate(-10vw,16vh) scale(.92)}}
@keyframes drift3{to{transform:translate(-14vw,-10vh) scale(1.08)}}

/* the glass */
.glass{position:relative;background:linear-gradient(135deg,var(--glass-2),var(--glass));
  -webkit-backdrop-filter:blur(26px) saturate(170%);backdrop-filter:blur(26px) saturate(170%);
  border:1px solid var(--edge);border-radius:var(--r);box-shadow:var(--shadow),inset 0 1px 0 var(--edge-hi),inset 0 -1px 0 rgba(255,255,255,.03)}
.glass::before{content:"";position:absolute;inset:0;border-radius:inherit;pointer-events:none;
  background:radial-gradient(120% 60% at 10% 0%,rgba(255,255,255,.18),transparent 55%);opacity:.8;mix-blend-mode:soft-light}
@supports not ((backdrop-filter:blur(1px)) or (-webkit-backdrop-filter:blur(1px))){.glass{background:var(--glass-strong)}}

.shell{max-width:var(--maxw);margin:0 auto;padding:0 clamp(14px,2.5vw,28px);display:grid;grid-template-columns:var(--side) minmax(0,1fr);gap:var(--gap)}
.side{position:sticky;top:16px;align-self:start;max-height:calc(100vh - 32px);overflow:auto;padding:22px 18px;margin:16px 0;scrollbar-width:thin}
.brand{display:flex;align-items:center;gap:11px;color:var(--fg);text-decoration:none;font-weight:600;font-size:17px;font-family:var(--display)}
.brand svg{width:30px;height:30px;flex:none;filter:drop-shadow(0 4px 14px color-mix(in oklab,var(--accent) 60%,transparent))}
.brand small{display:block;font-weight:400;color:var(--muted);font-size:12px;font-family:var(--mono);letter-spacing:.04em}
.audience{display:inline-flex;margin:18px 0 18px;border:1px solid var(--edge);border-radius:999px;padding:3px;background:var(--glass);
  box-shadow:inset 0 1px 0 var(--edge-hi)}
.audience button{appearance:none;border:0;background:transparent;color:var(--muted);font:inherit;font-size:13px;
  padding:6px 14px;border-radius:999px;cursor:pointer;transition:background .25s,color .25s}
.audience [aria-pressed="true"]{background:linear-gradient(135deg,var(--accent),var(--accent-2));color:var(--accent-ink);
  box-shadow:0 4px 18px color-mix(in oklab,var(--accent) 45%,transparent)}
.side nav a{display:block;padding:7px 10px;border-radius:9px;color:var(--fg);text-decoration:none;font-size:14.5px;transition:background .2s,color .2s}
.side nav a:hover{background:var(--glass-2)}
.side nav a.current{background:var(--glass-2);color:var(--accent-2);box-shadow:inset 2px 0 0 var(--accent-2)}
.side .ext{margin-top:16px;padding-top:14px;border-top:1px solid var(--edge);display:flex;flex-wrap:wrap;gap:6px 14px}
.side .ext a{color:var(--muted);font-size:13.5px;text-decoration:none}
.side .ext a::after{content:" ↗";font-size:.85em}
.side .ext a:hover{color:var(--accent-2)}

/* mobile bar (hidden on desktop) */
.mbar{display:none}

main{padding:16px 0 80px;min-width:0}
section{padding:clamp(44px,7vw,84px) 0 8px;position:relative}
section + section::before{content:"";position:absolute;left:0;right:0;top:0;height:1px;
  background:linear-gradient(90deg,transparent,var(--edge-hi),transparent)}
section:first-of-type{padding-top:14px}
.lede{font-size:clamp(17px,1.7vw,21px);line-height:1.55;max-width:64ch;color:var(--fg)}
.notice{display:flex;gap:12px;align-items:center;font-size:13.5px;border-radius:999px;padding:9px 16px 9px 12px;margin:0 0 26px;
  font-family:var(--mono);width:fit-content;max-width:100%}
.notice .dot{width:8px;height:8px;border-radius:50%;background:var(--warn);display:inline-block;flex:none;
  box-shadow:0 0 0 4px var(--warn-soft),0 0 18px var(--warn);animation:pulse 2.6s ease-in-out infinite}
.notice span:last-child{flex:1;min-width:0}
@keyframes pulse{50%{box-shadow:0 0 0 7px var(--warn-soft),0 0 26px var(--warn)}}
.hero{padding:clamp(26px,4vw,44px)}
.cta{display:flex;gap:10px;flex-wrap:wrap;margin:28px 0 18px}
.btn{display:inline-flex;align-items:center;gap:8px;font-family:var(--mono);font-size:13px;letter-spacing:.06em;text-transform:uppercase;
  padding:12px 18px;border-radius:999px;border:1px solid var(--edge);color:var(--fg);background:var(--glass);text-decoration:none;cursor:pointer;
  box-shadow:inset 0 1px 0 var(--edge-hi);transition:transform .2s,box-shadow .2s,background .2s;-webkit-backdrop-filter:blur(12px);backdrop-filter:blur(12px)}
.btn:hover{transform:translateY(-1px);background:var(--glass-2)}
.btn.primary{background:linear-gradient(135deg,var(--accent),var(--accent-2));color:var(--accent-ink);border-color:transparent;
  box-shadow:0 8px 28px color-mix(in oklab,var(--accent) 50%,transparent),inset 0 1px 0 rgba(255,255,255,.5)}
.btn.primary:hover{box-shadow:0 12px 36px color-mix(in oklab,var(--accent-2) 55%,transparent),inset 0 1px 0 rgba(255,255,255,.5)}
.prompt{display:grid;grid-template-columns:1fr auto;align-items:center;gap:12px;border-radius:var(--r-sm);padding:12px 12px 12px 16px;margin:0 0 22px;
  font-family:var(--mono);font-size:13px}
.prompt .text{white-space:pre-wrap;color:var(--muted);line-height:1.6}
.prompt .text b{color:var(--fg);font-weight:600}
.prompt button{appearance:none;border:1px solid var(--edge);background:var(--glass-2);color:var(--fg);font:inherit;font-size:12px;
  letter-spacing:.06em;text-transform:uppercase;padding:9px 12px;border-radius:999px;cursor:pointer;white-space:nowrap;box-shadow:inset 0 1px 0 var(--edge-hi)}
.prompt button:hover{background:var(--glass-strong)}
.works{display:flex;gap:8px;flex-wrap:wrap;align-items:center;margin:8px 0 0}
.works .eyebrow{margin-right:6px}
.chip{font-family:var(--mono);font-size:12.5px;border:1px solid var(--edge);border-radius:999px;padding:4px 11px;background:var(--glass);color:var(--fg);
  box-shadow:inset 0 1px 0 var(--edge-hi)}
.term{margin:24px 0 0;border-radius:var(--r-sm);overflow:hidden}
.term .bar{display:flex;gap:6px;align-items:center;padding:9px 14px;background:var(--code-bg);border-bottom:1px solid var(--edge);color:var(--code-muted);
  font-family:var(--mono);font-size:12px}
.term .bar i{width:10px;height:10px;border-radius:50%;background:var(--edge-hi);display:inline-block;opacity:.6}
.term pre{border-radius:0;border:0;background:var(--code-bg);box-shadow:none}
.term .c{color:var(--code-muted)}
.term .k{color:var(--code-k)}
.term .s{color:var(--code-s)}

table{width:100%;border-collapse:separate;border-spacing:0;font-size:14.5px;margin:14px 0 18px}
th,td{text-align:left;vertical-align:top;padding:11px 12px;border-bottom:1px solid var(--edge)}
th{font-weight:500;font-size:12px;letter-spacing:.1em;text-transform:uppercase;color:var(--muted);font-family:var(--mono)}
tr:last-child td{border-bottom:0}
td code{white-space:nowrap}
.tbl{overflow-x:auto;-webkit-overflow-scrolling:touch;padding:4px 6px}
.tag{display:inline-block;font-family:var(--mono);font-size:11px;letter-spacing:.06em;text-transform:uppercase;padding:3px 9px;border-radius:999px;
  border:1px solid transparent;white-space:nowrap}
.tag.live,.tag.l1{background:var(--ok-soft);color:var(--ok);border-color:color-mix(in oklab,var(--ok) 35%,transparent)}
.tag.phase1,.tag.l3{background:var(--warn-soft);color:var(--warn);border-color:color-mix(in oklab,var(--warn) 35%,transparent)}
.tag.l2{background:color-mix(in oklab,var(--accent) 16%,transparent);color:var(--accent);border-color:color-mix(in oklab,var(--accent) 35%,transparent)}

.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,250px),1fr));gap:14px;margin:20px 0}
.card{padding:22px 22px 16px;transition:transform .25s,box-shadow .25s}
.card:hover{transform:translateY(-2px)}
.card h3{margin-bottom:8px}
.card .proof{font-size:13.5px;color:var(--muted);border-top:1px solid var(--edge);margin:12px 0 0;padding-top:10px}
.card .proof b{color:var(--fg);font-weight:600}
.num{font-family:var(--mono);color:var(--accent-2);font-size:12px;letter-spacing:.14em;margin-bottom:10px;display:block}

.tabs{margin:18px 0 10px;position:relative}
.tabs input{position:absolute;opacity:0;pointer-events:none}
.tabs .labels{display:inline-flex;gap:4px;padding:4px;border-radius:999px;border:1px solid var(--edge);background:var(--glass);margin-bottom:10px;
  box-shadow:inset 0 1px 0 var(--edge-hi)}
.tabs label{font-family:var(--mono);font-size:13px;letter-spacing:.06em;padding:7px 16px;cursor:pointer;color:var(--muted);border-radius:999px;transition:background .25s,color .25s}
.tabs .panel{display:none}
#f-mcp:checked~.labels label[for=f-mcp],#f-rest:checked~.labels label[for=f-rest],#f-cli:checked~.labels label[for=f-cli]{
  background:linear-gradient(135deg,var(--accent),var(--accent-2));color:var(--accent-ink)}
#f-mcp:checked~.p-mcp,#f-rest:checked~.p-rest,#f-cli:checked~.p-cli{display:block}
.twocol{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,300px),1fr));gap:14px;margin:16px 0}
.twocol .eyebrow{margin:0 0 8px}
.tools-group{margin:14px 0 6px}
.tools-group h3{font-size:12px;font-family:var(--mono);font-weight:500;letter-spacing:.12em;text-transform:uppercase;color:var(--accent-2);margin:20px 0 8px}
.tools{list-style:none;margin:0;padding:0;display:grid;grid-template-columns:repeat(auto-fill,minmax(min(100%,300px),1fr));gap:8px}
.tools li{display:grid;grid-template-columns:max-content 1fr;gap:10px;font-size:14px;padding:9px 12px;border-radius:10px;border:1px solid var(--edge);
  background:var(--glass);box-shadow:inset 0 1px 0 var(--edge-hi)}
.tools code{background:none;border:0;padding:0;color:var(--accent);white-space:nowrap}
.tools span{color:var(--muted)}
details{border-radius:var(--r-sm);padding:0 18px;margin:0 0 10px}
summary{cursor:pointer;font-weight:600;padding:14px 0;list-style:none;display:flex;justify-content:space-between;gap:10px}
summary::-webkit-details-marker{display:none}
summary::after{content:"+";font-family:var(--mono);color:var(--accent-2);font-size:18px;line-height:1}
details[open] summary::after{content:"–"}
details p{margin:0 0 16px;color:var(--muted)}
footer{margin-top:48px;padding:18px 22px;font-size:13.5px;color:var(--muted);display:flex;flex-wrap:wrap;gap:10px 22px;justify-content:space-between}
footer a{color:var(--muted)}
.agentview{display:none}
body.agent .agentview{display:block}
body.agent .humanview{display:none}
.agentview pre{font-size:13px;white-space:pre-wrap}
.agentview .eyebrow{padding:14px 4px}

@media (max-width:900px){
  .shell{display:block;padding:0 14px}
  .side{display:none}
  .mbar{display:block;position:sticky;top:0;z-index:20;margin:0;padding:10px 14px;border-radius:0;border-left:0;border-right:0;border-top:0;box-shadow:0 10px 40px rgba(0,0,0,.25)}
  .mbar .row{display:flex;align-items:center;justify-content:space-between;gap:10px}
  .mbar .audience{margin:0}
  .mbar .brand small{display:none}
  .mbar nav{display:flex;gap:6px;overflow-x:auto;scrollbar-width:none;margin:10px -14px 0;padding:0 14px 2px;-webkit-overflow-scrolling:touch}
  .mbar nav::-webkit-scrollbar{display:none}
  .mbar nav a{flex:none;font-size:12.5px;font-family:var(--mono);letter-spacing:.04em;color:var(--fg);text-decoration:none;padding:6px 11px;
    border-radius:999px;border:1px solid var(--edge);background:var(--glass)}
  .mbar nav a.current{background:linear-gradient(135deg,var(--accent),var(--accent-2));color:var(--accent-ink);border-color:transparent}
  body.agent .mbar nav{display:none}
  main{padding-top:14px}
  .hero{padding:20px 16px}
  .prompt{grid-template-columns:1fr}
  .prompt button{justify-self:stretch}
  section{scroll-margin-top:96px}
  .tbl{padding:0;overflow:visible}
  table,thead,tbody,tr,th,td{display:block}
  thead{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0)}
  tr{border:1px solid var(--edge);border-radius:var(--r-sm);padding:6px 12px;margin:0 0 10px;background:var(--glass);box-shadow:inset 0 1px 0 var(--edge-hi)}
  td{padding:8px 0;border-bottom:1px solid var(--edge)}
  tr td:last-child{border-bottom:0}
  td[data-label]::before{content:attr(data-label);display:block;font-family:var(--mono);font-size:10.5px;letter-spacing:.12em;text-transform:uppercase;color:var(--muted);margin-bottom:3px}
  td code{white-space:normal;word-break:break-word}
  .tools li{grid-template-columns:1fr}
  .tools code{white-space:normal;word-break:break-word}
  footer{flex-direction:column}
  .bg i{filter:blur(50px)}
}
@media (prefers-reduced-motion:reduce){
  html{scroll-behavior:auto}
  .bg i,.notice .dot{animation:none}
  .card,.btn{transition:none}
}
`;

const LOGO = `<svg viewBox="0 0 32 32" aria-hidden="true" focusable="false"><rect x="2" y="2" width="28" height="28" rx="7" fill="var(--accent)"/><path d="M9 11.5h14M9 16h9M9 20.5h14" stroke="var(--accent-ink)" stroke-width="2.4" stroke-linecap="round"/><circle cx="22.5" cy="16" r="1.9" fill="var(--accent-ink)"/></svg>`;

function term(lines) {
  return `<div class="term glass"><div class="bar"><i></i><i></i><i></i><span>&nbsp;sh</span></div><pre><code>${lines}</code></pre></div>`;
}

export function renderHtml(site, md) {
  const title = `${site.name}: ${site.tagline.replace(/\.$/, "")}`;
  const description = site.one_sentence;
  const live = site.surfaces.filter((s) => s.status === "live");

  const jsonld = {
    "@context": "https://schema.org",
    "@type": "SoftwareApplication",
    name: site.name,
    url: site.url,
    description,
    applicationCategory: "DeveloperApplication",
    operatingSystem: "Linux, macOS",
    license: "https://www.apache.org/licenses/LICENSE-2.0",
    codeRepository: site.repo,
    offers: { "@type": "Offer", price: "0", priceCurrency: "USD", description: "Open-source local engine; infrastructure and model costs separate. Hosted checkout is unavailable." },
    softwareHelp: { "@type": "WebPage", url: `${site.url}/llms.txt` },
  };

  const navHtml = site.nav.map((n) => `<a href="#${n.id}">${esc(n.label)}</a>`).join("");

  const surfacesRows = site.surfaces
    .map((s) => {
      const url = surfaceUrl(site, s);
      const label = s.host === "registry" ? s.path : s.host === "repo" ? s.path : url.replace(/^https:\/\//, "");
      return `<tr><td data-label="Surface"><a href="${esc(url)}"><code>${esc(label)}</code></a></td><td data-label="What">${inline(s.what)}</td><td data-label="Why an agent needs it" class="muted">${esc(s.why)}</td><td data-label="Status"><span class="tag ${s.status}">${STATUS_LABEL[s.status]}</span></td></tr>`;
    })
    .join("\n");

  const pillars = site.pillars
    .map(
      (p, i) =>
        `<div class="card glass"><span class="num">0${i + 1}</span><h3>${esc(p.title)}</h3><p>${esc(p.claim)}</p><p class="proof"><b>Proof in the code.</b> ${esc(p.proof)}</p></div>`
    )
    .join("\n");

  const tiers = site.tiers
    .map(
      (t) =>
        `<tr><td data-label="Tier"><span class="tag ${t.level.toLowerCase()}">${t.level}</span></td><td data-label="What happens"><b>${esc(t.name)}</b><br><span class="muted">${inline(t.what)}</span></td><td data-label="Examples"><code>${esc(t.examples)}</code></td><td data-label="MCP"><code>${esc(t.mcp)}</code></td><td data-label="Cost">${esc(t.cost)}</td></tr>`
    )
    .join("\n");

  const tools = site.tools.groups
    .map(
      (g) =>
        `<div class="tools-group"><h3>${esc(g.title)}</h3><ul class="tools">${g.items
          .map((t) => `<li><code>${esc(t.name)}</code><span>${esc(t.task)}</span></li>`)
          .join("")}</ul></div>`
    )
    .join("\n");

  const pricing = site.pricing.rows
    .map((r) => `<tr><td data-label="Metered item">${esc(r.item)}</td><td data-label="Unit price"><b>${esc(r.price)}</b></td><td data-label="Notes" class="muted">${esc(r.note)}</td></tr>`)
    .join("\n");

  const deploy = site.deploy
    .map((d, i) => `<div class="card glass"><span class="num">0${i + 1}</span><h3>${esc(d.title)}</h3><p>${esc(d.body)}</p></div>`)
    .join("\n");

  const compare = site.comparison.rows
    .map((r) => `<tr><td data-label="Axis" class="muted">${esc(r.axis)}</td><td data-label="Memory APIs">${esc(r.memory)}</td><td data-label="${esc(site.name)}"><b>${esc(r.shogun)}</b></td></tr>`)
    .join("\n");

  const faq = site.faq.map((f) => `<details class="glass"><summary>${esc(f.q)}</summary><p>${esc(f.a)}</p></details>`).join("\n");

  const chips = site.works_with.map((w) => `<span class="chip">${esc(w)}</span>`).join("");

  const heroTerm = term(
    `<span class="c"># Read the setup guide; no commands execute automatically</span>\n<span class="k">curl</span> --fail --silent --show-error https://${site.domain}/index.md`
  );
  const commands = (label, command) => `<pre><code>${esc(command)}</code></pre><button class="btn" type="button" data-copy="${esc(command)}">${esc(label)}</button>`;
  const downloads = site.setup.platforms.map((p) => `<details class="glass"><summary>${esc(p.label)}</summary><p><a href="${esc(p.archive)}">Download archive</a> · <a href="${esc(p.checksum)}">SHA-256 checksum</a></p>${commands(`Copy ${p.label} setup`, p.command)}</details>`).join("\n");
  const setupSteps = site.setup.steps.map((s) => `<div class="card glass"><h3>${esc(s.title)}</h3><p>${esc(s.description)}</p>${commands(`Copy ${s.title}`, s.command)}</div>`).join("\n");

  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(title)}</title>
<meta name="description" content="${esc(description)}">
<link rel="canonical" href="${site.url}/">
<link rel="alternate" type="text/markdown" title="This page as Markdown" href="${site.url}/index.md">
<link rel="alternate" type="text/plain" title="llms.txt" href="${site.url}/llms.txt">
<link rel="alternate" type="text/plain" title="llms-full.txt" href="${site.url}/llms-full.txt">
<link rel="code-repository" href="${site.repo}">
<meta property="og:type" content="website">
<meta property="og:site_name" content="${esc(site.name)}">
<meta property="og:title" content="${esc(title)}">
<meta property="og:description" content="${esc(description)}">
<meta property="og:url" content="${site.url}/">
<meta name="twitter:card" content="summary">
<meta name="color-scheme" content="light dark">
<meta name="theme-color" content="#070a12" media="(prefers-color-scheme: dark)">
<meta name="theme-color" content="#eef2fb" media="(prefers-color-scheme: light)">
<link rel="icon" href="data:image/svg+xml,${encodeURIComponent(LOGO.replace("var(--accent)", "#2c4a8a").replaceAll("var(--accent-ink)", "#ffffff"))}">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500&family=IBM+Plex+Sans:wght@400;500;600&family=Space+Grotesk:wght@500;600&display=swap">
<script type="application/ld+json">${JSON.stringify(jsonld)}</script>
<style>${CSS}</style>
</head>
<body>
<a class="skip" href="#main">Skip to content</a>
<div class="bg" aria-hidden="true"><i class="b1"></i><i class="b2"></i><i class="b3"></i><span class="grid"></span><span class="grain"></span></div>
<header class="mbar glass">
  <div class="row">
    <a class="brand" href="/">${LOGO}<span>${esc(site.name)}<small>${esc(site.domain)}</small></span></a>
    <div class="audience" role="group" aria-label="Audience">
      <button type="button" data-aud="human" aria-pressed="true">Human</button>
      <button type="button" data-aud="agent" aria-pressed="false">Agent</button>
    </div>
  </div>
  <nav aria-label="On this page" class="humanview">${navHtml}</nav>
</header>
<div class="shell">
<aside class="side glass">
  <a class="brand" href="/">${LOGO}<span>${esc(site.name)}<small>${esc(site.domain)}</small></span></a>
  <div class="audience" role="group" aria-label="Audience">
    <button type="button" data-aud="human" aria-pressed="true">Human</button>
    <button type="button" data-aud="agent" aria-pressed="false" title="Show this page as the Markdown an agent receives">Agent</button>
  </div>
  <nav aria-label="On this page" class="humanview">${navHtml}</nav>
  <div class="ext">
    <a href="/llms.txt">llms.txt</a>
    <a href="${esc(site.repo)}">GitHub</a>
    <a href="${esc(site.repo)}/blob/main/docs/implementation-plan.md">Plan</a>
  </div>
</aside>
<main id="main">
<div class="agentview" aria-live="polite">
  <p class="eyebrow">What an agent receives for <code>GET /</code> with <code>Accept: text/markdown</code>. Also at <a href="/index.md">/index.md</a>.</p>
  <pre><code>${esc(md)}</code></pre>
</div>
<div class="humanview">

<section id="top" class="hero glass">
  <p class="notice glass"><span class="dot"></span><span>${esc(site.status_line)}</span></p>
  <p class="eyebrow">${esc(site.motto)}</p>
  <h1><span class="grad">${esc(site.tagline)}</span></h1>
  <p class="lede" style="margin-top:14px">${esc(site.one_sentence)}</p>
  <div class="cta">
    <a class="btn primary" href="#setup">Get started locally</a>
    <a class="btn" href="${esc(site.setup.release_url)}">Linux ${esc(site.setup.version)} downloads ↗</a>
    <a class="btn ghost" href="/llms.txt">For agents: llms.txt</a>
  </div>
  <div class="prompt glass">
    <span class="text"><b>Setup prompt</b> · paste into Claude Code, Codex or Cursor\n${esc(site.setup_prompt)}</span>
    <button type="button" data-copy="${esc(site.setup_prompt)}">Copy setup prompt</button>
  </div>
  <div class="works"><span class="eyebrow">Client and framework integration paths in source</span>${chips}</div>
  ${heroTerm}
</section>

<section id="setup">
  <p class="eyebrow">Get started · ${esc(site.setup.version)}</p>
  <h2>Save one fact. Find it again.</h2>
  <p class="lede">A local developer preview using synthetic text. No signup, hosted service or model API key required.</p>
  <p>${esc(site.setup.requirements)}</p>
  <h3>1. Download and verify your Linux build</h3>
  <p>Choose one architecture. Review the commands before running them. They create a private preview directory and refuse to overwrite existing data. Do not enable shell tracing (<code>set -x</code>) or share key/token files.</p>
  ${downloads}
  <details class="glass"><summary>macOS or Windows</summary><p>${esc(site.setup.unsupported)}</p><p><a href="${esc(site.setup.docs_url)}">Read platform and source-build limitations</a></p></details>
  <p id="copy-status" role="status" aria-live="polite" class="muted">Copying commands does not run them.</p>
  ${setupSteps}
  <div class="card glass"><h3>${esc(site.setup.connect)}</h3><p>${esc(site.setup.connect_text)}</p><a class="btn" href="${esc(site.setup.docs_url)}">Read the source CLI connection guide ↗</a></div>
  <h3>If something fails</h3>
  ${site.setup.errors.map((e) => `<details class="glass"><summary>${esc(e.title)}</summary><p>${esc(e.body)}</p></details>`).join("\n")}
</section>

<section id="agents">
  <p class="eyebrow">For agents</p>
  <h2>Machine-readable at every well-known location.</h2>
  <p class="lede">The same identity, tool names and schemas appear everywhere an agent might look. The human page you are reading is generated from the same data. ${live.length} of ${site.surfaces.length} surfaces are live today; the rest are Phase 1 deliverables and are labeled as such.</p>
  <div class="tbl glass"><table>
    <thead><tr><th>Surface</th><th>What</th><th>Why an agent needs it</th><th>Status</th></tr></thead>
    <tbody>${surfacesRows}</tbody>
  </table></div>
  <p class="muted" style="font-size:14px">Hosted surfaces are not publicly available. Local reads require authentication; free does not mean anonymous. The preview above tests source retrieval. Model-backed state extraction and actual coding-client connections require separate setup and validation.</p>
</section>

<section id="what">
  <p class="eyebrow">What it is</p>
  <h2>${esc(site.motto)}</h2>
  <p class="lede">${esc(site.developer_30s)}</p>
  <p>${esc(site.not_a_memory_api)}</p>
  <div class="grid">${pillars}</div>
</section>

<section id="faces">
  <p class="eyebrow">Three faces</p>
  <h2>One behavior over MCP, REST and CLI.</h2>
  <p class="lede">${esc(site.faces.intro)} A tool that exists on one face exists on all three.</p>
  <div class="tabs">
    <input type="radio" name="face" id="f-mcp" checked>
    <input type="radio" name="face" id="f-rest">
    <input type="radio" name="face" id="f-cli">
    <div class="labels"><label for="f-mcp">MCP</label><label for="f-rest">REST</label><label for="f-cli">CLI</label></div>
    <div class="panel p-mcp"><pre><code>${esc(site.faces.mcp)}</code></pre></div>
    <div class="panel p-rest"><pre><code>${esc(site.faces.rest)}</code></pre></div>
    <div class="panel p-cli"><pre><code>${esc(site.faces.cli)}</code></pre></div>
  </div>
  <p class="eyebrow" style="margin-top:18px">Same answer on every face</p>
  <pre><code>${esc(site.faces.response)}</code></pre>
  <div class="twocol">
    <div><p class="eyebrow">Out of credit: 402 in the x402 shape</p><pre><code>${esc(site.faces.payment_required)}</code></pre></div>
    <div><p class="eyebrow">An L3 action: approval URL, no side effect</p><pre><code>${esc(site.faces.l3)}</code></pre></div>
  </div>
</section>

<section id="tiers">
  <p class="eyebrow">Permission tiers</p>
  <h2>Reads run. Writes confirm. Sends wait for a human.</h2>
  <p class="lede">Three tiers, decided by the type of the action, identical for every agent and every human. An operation not in the scope table is denied; <code>ExternalSend</code> is always L3.</p>
  <div class="tbl glass"><table>
    <thead><tr><th>Tier</th><th>What happens</th><th>Examples</th><th>MCP</th><th>Cost</th></tr></thead>
    <tbody>${tiers}</tbody>
  </table></div>
</section>

<section id="tools">
  <p class="eyebrow">Tools</p>
  <h2>The core wire names, frozen.</h2>
  <p class="lede">${esc(site.tools.note)}</p>
  ${tools}
</section>

<section id="pricing">
  <p class="eyebrow">Pricing</p>
  <h2>Reads are free. You pay for what you ingest and run.</h2>
  <p class="lede">${esc(site.pricing.line)}</p>
  <div class="tbl glass"><table>
    <thead><tr><th>Metered item</th><th>Unit price</th><th>Notes</th></tr></thead>
    <tbody>${pricing}</tbody>
  </table></div>
  <p class="muted" style="font-size:14px">${esc(site.pricing.disclaimer)} A machine-readable rate card ships at <code>/.well-known/pricing.json</code> in Phase 1.</p>
</section>

<section id="deploy">
  <p class="eyebrow">Runs everywhere</p>
  <h2>Same API on your laptop, hosted, or in your cloud.</h2>
  <div class="grid">${deploy}</div>
</section>

<section id="compare">
  <p class="eyebrow">Not a memory API</p>
  <h2>Not what an agent remembers. What is true now.</h2>
  <p class="lede">${esc(site.comparison.intro)}</p>
  <div class="tbl glass"><table>
    <thead><tr><th></th><th>Memory APIs</th><th>${esc(site.name)}</th></tr></thead>
    <tbody>${compare}</tbody>
  </table></div>
</section>

<section id="open">
  <p class="eyebrow">Open source</p>
  <h2>The engine is open. The hosting is the business.</h2>
  <div class="twocol">
    <div class="card glass"><h3>Open · ${esc(site.license)}</h3><p>${esc(site.open_source.open)}</p></div>
    <div class="card glass"><h3>Closed</h3><p>${esc(site.open_source.closed)}</p><p class="muted" style="margin-bottom:0;font-size:14px">${esc(site.open_source.telemetry)}</p></div>
  </div>
  <div class="cta"><a class="btn primary" href="#setup">Start the local preview</a><a class="btn" href="${esc(site.repo)}">Inspect the source ↗</a></div>
</section>

<section id="faq">
  <p class="eyebrow">FAQ</p>
  <h2>Questions agents and their operators ask.</h2>
  ${faq}
</section>

<footer class="glass">
  <span>© ${new Date().getUTCFullYear()} ${esc(site.name)} · ${esc(site.license)}</span>
  <span><a href="/llms.txt">llms.txt</a> · <a href="/llms-full.txt">llms-full.txt</a> · <a href="/index.md">index.md</a> · <a href="/setup-prompt.txt">setup prompt</a> · <a href="${esc(site.repo)}">GitHub</a></span>
</footer>
</div>
</main>
</div>
<script>
(function(){
  var body=document.body, btns=document.querySelectorAll('[data-aud]');
  function setAud(a){ body.classList.toggle('agent',a==='agent'); btns.forEach(function(b){b.setAttribute('aria-pressed',String(b.dataset.aud===a));});
    try{ var u=new URL(location.href); if(a==='agent'){u.searchParams.set('agent','1');}else{u.searchParams.delete('agent');} history.replaceState(null,'',u);}catch(e){} }
  btns.forEach(function(b){ b.addEventListener('click',function(){ setAud(b.dataset.aud); }); });
  try{ if(new URL(location.href).searchParams.get('agent')==='1') setAud('agent'); }catch(e){}
  document.querySelectorAll('[data-copy]').forEach(function(b){ b.addEventListener('click',function(){
    var t=b.dataset.copyLabel||b.textContent, status=document.getElementById('copy-status'); b.dataset.copyLabel=t; (navigator.clipboard?navigator.clipboard.writeText(b.dataset.copy):Promise.reject()).then(function(){b.textContent='Copied';if(status)status.textContent='Copied. Review before pasting into a terminal.';},function(){b.textContent='Select and copy above';if(status)status.textContent='Clipboard unavailable. Select and copy the displayed text.';});
    clearTimeout(b.copyTimer); b.copyTimer=setTimeout(function(){b.textContent=t;},1800); }); });
  var links=document.querySelectorAll('.side nav a, .mbar nav a'), secs=[].map.call(links,function(a){return document.querySelector(a.getAttribute('href'));});
  if('IntersectionObserver' in window){ var io=new IntersectionObserver(function(es){ es.forEach(function(e){ if(e.isIntersecting){ links.forEach(function(a){var on=a.getAttribute('href')==='#'+e.target.id;a.classList.toggle('current',on);var n=a.parentNode;if(on&&n.classList.contains('humanview')&&n.closest('.mbar')){n.scrollTo({left:Math.max(0,a.offsetLeft-14),behavior:'smooth'});}}); } }); },{rootMargin:'-20% 0px -70% 0px'});
    secs.forEach(function(s){ if(s) io.observe(s); }); }
})();
</script>
</body>
</html>
`;
}

// ───────────────────────────── Other files ─────────────────────────────

export function renderRobots(site) {
  return [
    "User-agent: *",
    "Allow: /",
    "",
    `Sitemap: ${site.url}/sitemap.xml`,
    `LLMs-Txt: ${site.url}/llms.txt`,
    "",
    "# Agents are the audience. Training, search and inference use are all welcome.",
    "Content-Signal: ai-train=yes, search=yes, ai-input=yes",
    "",
  ].join("\n");
}

export function renderSitemap(site, paths) {
  const urls = paths.map((p) => `  <url><loc>${site.url}${p === "/index.html" ? "/" : p}</loc></url>`).join("\n");
  return `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${urls}\n</urlset>\n`;
}

/** Everything the site publishes, keyed by URL path. */
export function render(site) {
  const md = renderMarkdown(site);
  const out = {
    "/index.md": md,
    "/llms.txt": renderLlmsTxt(site),
    "/llms-full.txt": md,
    "/setup-prompt.txt": site.setup_prompt + "\n",
    "/robots.txt": renderRobots(site),
  };
  out["/index.html"] = renderHtml(site, md);
  out["/sitemap.xml"] = renderSitemap(site, ["/index.html", "/index.md", "/llms.txt", "/llms-full.txt", "/setup-prompt.txt"]);
  return out;
}

export function loadSite() {
  return JSON.parse(readFileSync(join(here, "content", "site.json"), "utf8"));
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const site = loadSite();
  const dist = join(here, "dist");
  rmSync(dist, { recursive: true, force: true });
  mkdirSync(dist, { recursive: true });
  const files = render(site);
  for (const [p, body] of Object.entries(files)) {
    const target = join(dist, p);
    mkdirSync(dirname(target), { recursive: true });
    writeFileSync(target, body);
  }
  console.log(`wrote ${Object.keys(files).length} files to ${dist}`);
}
