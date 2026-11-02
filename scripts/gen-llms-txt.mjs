#!/usr/bin/env node
// E10-T4: generate llms.txt and llms-full.txt from the documents agents should read first
// (README.md, SKILL.md, docs/api/*.md, apps/api/README-agent-api.md), plus the TypeScript module
// apps/api serves them from (`/llms.txt`, `/llms-full.txt`; Workers have no filesystem). The
// committed files are generated, never edited by hand (invariant 12); CI runs `--check`.
//
//   node scripts/gen-llms-txt.mjs --write
//   node scripts/gen-llms-txt.mjs --check
import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

import { parseFrontmatter } from "./lib/skill-md.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const REPO = "https://github.com/Torutesu/Shogunagent/blob/main";
const API = "https://api.shogunagent.dev";

/** The source documents, in reading order. Only these; nothing else is agent-facing yet. */
function sources() {
  const api = readdirSync(join(root, "docs/api")).filter((f) => f.endsWith(".md")).sort();
  return ["README.md", "SKILL.md", "apps/api/README-agent-api.md", ...api.map((f) => `docs/api/${f}`)];
}

/** The first paragraph after the first H1 (or the first paragraph), one line. */
function firstParagraph(md) {
  const body = md.replace(/^---\r?\n[\s\S]*?\r?\n---\r?\n/, "");
  const paras = body.split(/\r?\n\r?\n/).map((p) => p.trim()).filter((p) => p && !p.startsWith("#") && !p.startsWith("|") && !p.startsWith("```") && !p.startsWith(">") && !p.startsWith("<"));
  return (paras[0] ?? "").replace(/\s+/g, " ").replace(/\*\*/g, "");
}

function h1(md) {
  const m = /^# (.+)$/m.exec(md.replace(/^---\r?\n[\s\S]*?\r?\n---\r?\n/, ""));
  return m ? m[1].trim() : null;
}

function truncate(s, n) {
  return s.length <= n ? s : s.slice(0, n - 1).replace(/\s+\S*$/, "") + "…";
}

export function renderLlmsTxt(files) {
  const readme = files["README.md"];
  const skill = files["SKILL.md"];
  const fm = parseFrontmatter(skill);
  const lines = [];
  lines.push("# ShogunAgent");
  lines.push("");
  lines.push(`> ${fm.description.replace(/\s+/g, " ")}`);
  lines.push("");
  lines.push(firstParagraph(readme.split(/^## 1\. /m)[1] ? "## 1. " + readme.split(/^## 1\. /m)[1] : readme));
  lines.push("");
  lines.push(`Skill version ${fm.version}. Check the repository's release status before relying on npm packages, release downloads or a hosted service. The configured hosted MCP URL is ${API}/mcp (Streamable HTTP, 2026-07-28); use an operator-confirmed endpoint and credentials. Local setup requires binaries, an existing encrypted store and its matching key. Bare \`npx shogunagent@${fm.version.split(".").slice(0, 2).join(".")}\` only serves; installation and MCP connection are explicit steps (see SKILL.md and the CLI guide). Reads of the principal's data are free but require authorization; hosted writes and runs are metered in micro-USD.`);
  lines.push("");
  lines.push("## Start here");
  lines.push("");
  lines.push(`- [SKILL.md](${REPO}/SKILL.md): the skill agents install: when to use it and when not, setup, auth, precedence of facts, cost, what L1 / L2 / L3 mean for the caller, statuses and errors, numbered rules.`);
  lines.push(`- [CLI guide](${REPO}/packages/cli/README.md): existing-store prerequisites, source builds, explicit install/connect commands, local authentication and a synthetic first ingest.`);
  lines.push(`- [Agent API guide](${REPO}/apps/api/README-agent-api.md): ${truncate(firstParagraph(files["apps/api/README-agent-api.md"]), 200)}`);
  lines.push(`- [README](${REPO}/README.md): what is available today, the local quickstart, connecting an agent, verification and known limits.`);
  lines.push(`- [Strategy](${REPO}/docs/strategy.md): positioning, pricing model, go-to-market and roadmap (direction, not availability).`);
  lines.push("");
  lines.push("## API reference");
  lines.push("");
  for (const path of Object.keys(files).filter((p) => p.startsWith("docs/api/")).sort()) {
    lines.push(`- [${h1(files[path]) ?? path}](${REPO}/${path}): ${truncate(firstParagraph(files[path]), 200)}`);
  }
  lines.push("");
  lines.push("## Machine-readable surfaces");
  lines.push("");
  lines.push(`- [MCP server card](${API}/.well-known/mcp/server-card.json): endpoint, auth, the tool list with annotations (readOnlyHint = L1 read; destructiveHint + openWorldHint = L3).`);
  lines.push(`- [A2A agent card](${API}/.well-known/agent-card.json): signed identity, provider, skills, security schemes.`);
  lines.push(`- [Pricing](${API}/.well-known/pricing.json): the rate card, free allowance and caps, from the same table the metering enforces.`);
  lines.push(`- [Status](${API}/.well-known/status.json): live p50 / p95, error rate over five minutes, incident history. [Health](${API}/health) is the short form.`);
  lines.push(`- [OpenAPI 3.1](${API}/openapi.json): the REST face (agents, keys, wallet, quote, receipts, /mcp, router verbs).`);
  lines.push(`- [MCP Registry entry](https://registry.modelcontextprotocol.io/v0/servers/io.github.torutesu%2Fshogunagent): io.github.torutesu/shogunagent.`);
  lines.push(`- [Full documentation in one file](${API}/llms-full.txt)`);
  lines.push("");
  lines.push("## Optional");
  lines.push("");
  lines.push(`- [Implementation plan](${REPO}/docs/implementation-plan.md): epics and acceptance criteria (Japanese).`);
  lines.push(`- [Router](${REPO}/apps/router/README.md): discover / inspect / run, the ranking formula, connector format.`);
  lines.push(`- [Repository](https://github.com/Torutesu/Shogunagent): Apache-2.0.`);
  return lines.join("\n") + "\n";
}

export function renderLlmsFullTxt(files) {
  const parts = [];
  parts.push("# ShogunAgent — full documentation for agents");
  parts.push("");
  parts.push(`Generated from ${Object.keys(files).join(", ")} by scripts/gen-llms-txt.mjs. The index is llms.txt. Each section below is one file, verbatim.`);
  for (const path of Object.keys(files)) {
    parts.push("");
    parts.push("---");
    parts.push("");
    parts.push(`<!-- file: ${path} -->`);
    parts.push("");
    parts.push(files[path].replace(/\r\n/g, "\n").trimEnd());
  }
  return parts.join("\n") + "\n";
}

export function renderModule(llms, full) {
  return [
    "// GENERATED by scripts/gen-llms-txt.mjs — do not edit. `node scripts/gen-llms-txt.mjs --check` fails when stale.",
    "// The text apps/api serves at /llms.txt and /llms-full.txt (Workers have no filesystem).",
    `export const LLMS_TXT: string = ${JSON.stringify(llms)};`,
    `export const LLMS_FULL_TXT: string = ${JSON.stringify(full)};`,
    "",
  ].join("\n");
}

function main() {
  const mode = process.argv[2];
  if (mode !== "--write" && mode !== "--check") {
    console.error("usage: gen-llms-txt.mjs --write | --check");
    process.exit(2);
  }
  const files = {};
  for (const p of sources()) files[p] = readFileSync(join(root, p), "utf8");
  const llms = renderLlmsTxt(files);
  const full = renderLlmsFullTxt(files);
  const outputs = {
    "llms.txt": llms,
    "llms-full.txt": full,
    "apps/api/src/wellknown/llms-content.generated.ts": renderModule(llms, full),
  };
  let stale = [];
  for (const [path, next] of Object.entries(outputs)) {
    const abs = join(root, path);
    if (mode === "--write") {
      writeFileSync(abs, next);
      console.log(`wrote ${relative(root, abs)}`);
    } else {
      let current = "";
      try { current = readFileSync(abs, "utf8"); } catch { /* absent */ }
      if (current !== next) stale.push(path);
    }
  }
  if (mode === "--check") {
    if (stale.length > 0) {
      console.error(`stale: ${stale.join(", ")} — run \`pnpm llms:gen\` and commit the result.`);
      process.exit(1);
    }
    console.log("llms.txt, llms-full.txt and llms-content.generated.ts match their sources");
  }
}

main();
