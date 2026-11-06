# ShogunAgent website

A dependency-free static launch website. Source in this directory, generated pages in `dist/`. Uses the existing ShogunAgent mark and self-hosted Manrope font.

## Local preview

Requires Node.js 22 or later. From the repository root:

```sh
node apps/web/build.mjs
PORT=3000 node apps/web/scripts/serve.mjs
```

Or `pnpm --filter @shogun-ai/web dev` (default port 3001).

## Edit and verify

- `home.html`: preserved original shogunagent.com HTML, CSS and interactions, captured from the public site on 2026-10-28.
- `build.mjs`: shared layout, supporting pages, and homepage additions.
- `laptop.html`, `laptop.css`, `laptop.js`: responsive local-workflow animation with manual chapters, pause/replay, visibility-aware playback, and reduced-motion support.
- `content.mjs`: documentation, canonical local setup snippets.
- `app.js`: synthetic handoff demos, navigation, docs filtering, clipboard.
- `onboarding.mjs`: source-only agent connection and state walkthrough pages.
- `setup-prompt.mjs`: agent-neutral setup prompt and reusable project memory
  instructions. The homepage copy button uses the current browser origin. The
  preview server serves matching local links in `setup-prompt.txt` and the agent
  references; the static production build retains the public domain.
- `styles.css`: responsive design and reduced-motion support.
- `laptop.html`, `laptop.css`, `laptop.js`: photographic MacBook with a four-part, plain-language launch-review story.
- `visuals.html`, `visuals.css`, `visuals.js`: interactive relationship graph and task timeline, with sample evidence, pause controls and reduced-motion support.
- `assets/`: locally served brand and font assets.

```sh
node apps/web/build.mjs
node --check apps/web/app.js
node --check apps/web/content.mjs
node --test apps/web/test/*.test.mjs
```

Tests cover broken internal destinations, setup safety invariants, HTTP routes, content negotiation and 404 handling. Browser QA must cover the demo stages, scenario changes, empty source selection, JSON views, clipboard, responsive navigation and docs filtering.

## Local onboarding beyond the website

- [Agent setup](../../docs/local-agent-setup.md): a dependency-free Python stdio
  bridge supplies the HTTP endpoint's dynamic MCP headers and generates configs
  without embedding tokens. The running server still owns authentication and permissions.
- [State walkthrough](../../docs/local-state-walkthrough.md): run the Maya engine
  replay, then follow the separately labeled credentialed processing path. The
  replay's model answers are fixtures; the website animation remains illustrative.

From the repository root, test the bridge with
`python3 -m unittest discover -s scripts -p test_mcp_local_bridge.py`.
Client connection and live-model limitations are recorded in [VERIFICATION.md](VERIFICATION.md).

See [VERIFICATION.md](VERIFICATION.md) for the completed browser/fresh-user audit, screenshots, and known launch gaps. The CI website job builds and checks this app independently of the Rust engine.

## Product truth and launch boundaries

The original homepage identity is retained: white background, blue accents, Manrope, and the original black folded mark. Supporting pages use the same identity. The laptop uses the photographic MacBook frame from the user-provided ShogunAI reference. Its animated HTML screen illustrates remembering a launch note, answering a question, continuing with another agent and requesting approval. It is a conceptual interface, not a shipped desktop application.

The browser workspace is an illustrative product view, not a shipped dashboard or a connection to the engine. All demos are synthetic browser-only simulations; approvals never send externally. Sample downloads explicitly declare themselves illustrative subsets, not exact schema responses.

Quickstart commands are based on the current source contracts. Building this website does not execute the engine, verify release archives, or validate OAuth integrations. Source-based local evaluation uses multi-principal mode; default single-store CLI needs an existing store and its original key. Hosted availability and npm availability must be verified separately.

The supporting pages cover product, demos, integrations, documentation, pricing, security, comparison, research, changelog, and website privacy. Supermemory was researched as a website/content benchmark on 2026-10-28. This does not imply engine feature parity. No fabricated customers, certifications, benchmark wins, or signup flow.

## Publication handoff

## Deploy

Production is the `shogunagent-web` Worker, serving `shogunagent.com` and `www.shogunagent.com`. Its domain mappings are declared in `wrangler.jsonc`; no API Worker or domain is part of this deployment.

Cloudflare Workers Builds is connected to `Torutesu/Shogunagent`, production branch **main**, repository root `/`. Pushes to main build and deploy the website automatically. Preview-branch builds are disabled.

- Build command: `pnpm --filter @shogun-ai/web typecheck && pnpm --filter @shogun-ai/web test` (the test script builds all pages first).
- Deploy command: `pnpm --filter @shogun-ai/web exec wrangler deploy --config wrangler.jsonc`.
- Authentication stays in Cloudflare's build settings, never in the repository.
- Failed tests stop publication and leave the previous deployment active.
- The existing GitHub Actions manual deploy workflow remains available as a fallback; it does not run on pushes. Relay and router deployment policies are unchanged.

Verify `/`, `/docs/quickstart/`, `/docs/agent-setup/`, `/llms.txt`, `/index.md`, a Markdown request to `/`, and a real 404 after publication. Compare the deployed homepage with `dist/index.html` from the deployed commit. Check the build's source SHA in Cloudflare.

Rollback: select the previous version in Cloudflare Deployments, then revert the faulty commit on main so the next automatic build keeps the correction. The pre-migration version is `c4cbff3b-444a-4337-9693-942359513bec` (2026-10-27).

### MacBook asset provenance

`assets/macbook-real.webp` is the unmodified 1040px MacBook frame from `https://shogunai.si/reference-macbook/assets/img/hero-macbook-1040.webp`, reused from the reference explicitly supplied for this project. Screen content is original HTML/CSS. Playback uses a visibility-aware animation clock, typed requests, staged results, a moving context packet, pause/replay and reduced-motion support.
