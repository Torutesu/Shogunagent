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

The live shogunagent.com currently serves an older homepage, so links in this build such as `/docs/quickstart/` answer 404 there. Publishing this build fixes that. The repository now carries everything needed to publish it; the publish itself is an owner step.

- `worker.mjs` is a Cloudflare Worker in front of `dist/` (Workers Static Assets). It answers the way `scripts/serve.mjs` does: GET and HEAD only (405 otherwise), `/` returns `index.md` for `Accept: text/markdown` or `?agent=1`, directory paths serve their `index.html`, unknown paths get `404.html` with status 404, and every answer carries `nosniff` and the referrer policy. `test/worker.test.mjs` checks this against the built site.
- `wrangler.jsonc` names the Worker `shogunagent-web` and has **no route**: a deploy publishes to its workers.dev address only and does not touch shogunagent.com.
- Deploy with the manual workflow (`.github/workflows/deploy.yml`, component `web`): it builds the site, runs the tests and makes a dry run unless `confirm` is `deploy-web-production` (or `deploy-web-staging`). `pnpm --filter @shogun-ai/web deploy:dry-run` does the same dry run locally.

Steps to replace the live site:

1. Find the Cloudflare project that serves shogunagent.com today and keep it for rollback.
2. Review the pages first: hosted prices are estimates, and the website privacy page and availability notes must match what is live.
3. Run the workflow with component `web` and confirm the deploy. Check the workers.dev address: `/`, `/docs/quickstart/`, `/llms.txt`, `/index.md`, a Markdown request to `/`, and a 404.
4. Attach shogunagent.com to the `shogunagent-web` Worker as a custom domain in the Cloudflare dashboard, and remove it from the old project. Roll back by moving the domain back.
5. Re-run the checks in step 3 on shogunagent.com.

### MacBook asset provenance

`assets/macbook-real.webp` is the unmodified 1040px MacBook frame from `https://shogunai.si/reference-macbook/assets/img/hero-macbook-1040.webp`, reused from the reference explicitly supplied for this project. Screen content is original HTML/CSS. Playback uses a visibility-aware animation clock, typed requests, staged results, a moving context packet, pause/replay and reduced-motion support.
