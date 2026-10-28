# shogunagent.com

The landing page, built for agents first. The domain `shogunagent.com` is registered; `api.shogunagent.com` is reserved for `apps/api` (Phase 1).

## How it is built

`content/site.json` is the only thing to edit. `build.mjs` renders, with no dependencies:

| Path | What |
|---|---|
| `/index.html` | The human page. A view over the same object, with a Human / Agent toggle that shows the Markdown an agent receives |
| `/index.md` | The page as Markdown. Also what `GET /` returns for `Accept: text/markdown` (or `text/plain`, or `?agent=1`) |
| `/llms.txt` | llmstxt.org index: read-first links, tool names, pricing, and the Phase 1 surfaces marked as not live |
| `/llms-full.txt` | Same as `index.md` |
| `/setup-prompt.txt` | The one-paragraph prompt a human pastes into a coding agent |
| `/robots.txt` | `LLMs-Txt:` and `Content-Signal:` lines |
| `/sitemap.xml` | |

`src/worker.js` runs in front of the static assets on Cloudflare Workers and does the content negotiation plus `Link`, `Content-Signal` and CORS headers. It reads no body and keeps no state.

## Commands

The `Get started` section is a local Linux developer-preview flow pinned to the published
v0.1.0 archives and SHA-256 values. Copyable commands live in `content/site.json` and appear in
both HTML and Markdown. They provision a new private evaluation store, refuse an existing
directory, and demonstrate authenticated source ingestion/recall without model calls. This does
not verify extracted state or a real coding-app connection. macOS/Windows downloads, hosted
access and npm/PyPI packages are not advertised as available.

Before shipping a setup change, run its commands against a fresh synthetic directory with the
published x64 archive, check replay and key preservation, and test browser/mobile navigation and
clipboard failure. ARM64 needs its own native runtime verification; static checksum validation
alone is not a runtime test. Never test with customer stores or print their keys/tokens.

This branch's historical site differs from the publicly served site. Confirm the actual deployed
Worker version/source and preserve its rollback version before deploying; a successful local
build does not establish that the checked-out branch is the current production source.

```sh
pnpm --filter @shogun-ai/web build     # writes dist/
pnpm --filter @shogun-ai/web test      # node --test: HTML, Markdown and llms.txt agree; negotiation works
pnpm --filter @shogun-ai/web dev       # local preview with the same negotiation, http://127.0.0.1:8788/
pnpm --filter @shogun-ai/web deploy    # wrangler deploy; the runbook is DEPLOY.md
```

Look and feel: dark-first "liquid glass" (translucent blurred panels over drifting gradient light); the light scheme follows the OS and `prefers-reduced-motion` turns the motion off. Under 900px the sidebar becomes a sticky glass header with a scrollable section nav and every table stacks into labeled cards.

## Rules

- Every surface in `site.json` carries a `status`. Hosted surfaces stay `phase1` until they answer in production; a test enforces it.
- The tool list must match `docs/implementation-plan.md` §7.1; a test enforces it. The page states no tool count.
- No analytics, no third-party script. Fonts come from Google Fonts; everything else is inline.
- English only for now. Platform shelves and agent-facing text are English by decision (README §8).
