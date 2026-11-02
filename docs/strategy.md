# ShogunAgent strategy

**The state layer for AI agents.**

> Memory remembers. State knows.

[日本語版はこちら / Japanese version](strategy.ja.md)

> **About this document.** This is the strategy that the README carried until October 2026, moved here on 1 November 2026 when the README became the setup guide. It records direction and decisions, not availability: for what you can use today, read the [README](../README.md). Test counts and benchmark figures in §12 are as of 26 October 2026; use CI for the current revision.

This repository holds the strategy, the research and the code for ShogunAgent: the state-inference and permission engine inside [ShogunAI](https://github.com/Torutesu/ShogunAI-) offered as a service to AI agents instead of humans. The decisions below are current as of October 2026 and supersede earlier drafts in `reports/`.

**Status:** phases 1 to 5 of the roadmap are implemented in code and verified in this repository. "Implemented" means built and tested, not released: nothing has been deployed, published to npm or PyPI, or submitted to a registry or directory yet. Those wait for the owner decisions listed in §12.

---

## 1. What it is

For teams running many AI agents, ShogunAgent is the shared state layer that gives every agent one calibrated model of the user's work (people, projects, commitments, open loops) with provenance on every fact and a human gate on every consequential action.

Unlike memory APIs that store free text in a vendor cloud, it is **typed, permission-tiered, local-first, and portable**.

It is not a memory API. "Agent memory" is a saturated category (Mem0, Zep, Letta, supermemory, Cognee, Engram) and the labs now ship personalization memory for free (OpenAI Dreaming, Anthropic Managed Agents memory, Google Memory Bank, AWS AgentCore, Microsoft Copilot). ShogunAgent names a different category: not what an agent remembers, but what is true now.

## 2. Value proposition (three pillars, no more)

| Pillar | Claim | Proof in the codebase | What competitors lack |
|---|---|---|---|
| **Typed state, not text memory** | Every fact is a typed row with a verbatim quote from its source event and a confidence score. Newer facts supersede older ones, contradictions are explicit, confidence decays over time. | Writes with empty `state_provenance` are rejected at the repository layer. High / Medium / Low bands enforced in one place (`shogun-fusion/confidence.rs`). The loop consolidator stores nothing without an exact quote match and confidence ≥ 0.84. | supermemory states it is "not a system of record: no SQL, no joins, no primary keys." Mem0 and Zep are free-text fact stores. |
| **Permission tiers on every action** | Reads run immediately (L1). Writes touching user data need one-tap confirmation (L2). External sends and payments need explicit human approval (L3). Enforced by type, the same for every agent. | `SendAction` is L3 by construction in `shogun-agents`. Maps 1:1 onto MCP `readOnlyHint` / `destructiveHint` / `openWorldHint` and MRTR `input_required` / URL elicitation. | 93% of agent permission prompts are approved without reading. Competitors expose binary read/write. |
| **Local-first, open, portable** | Data stays on the device by default. The cloud path retains no bodies. The engine is open source. State exports in an open format. | Zero-retention relay (traceability log has no body column). Secret redaction: recall 1.0, 0 false positives. User-held keys. | Every funded vendor is cloud-first. supermemory's OSS is clients only; the engine is closed. 7 of 7 memory products tested lock users in. |

**Delivery line for the router:** one endpoint for your agents: discover tools, run them under permission tiers, and keep what they learned as state you own.

**Pricing line:** reads are free; you pay for what you ingest and what you run.

### Message ladder

- **Three words:** The state layer for agents.
- **One sentence:** Calibrated truth about your work (people, projects, commitments, open loops) with provenance on every fact and a human gate on every send, shared across all your agents.
- **Developer, 30s:** Your agents start every session from zero and act without asking. ShogunAgent gives them one typed, provenance-backed model of the user's work over MCP, enforces three permission tiers on every action, keeps data on the device by default, and exports everything in an open format. Reads are free; you pay for what you ingest and run.
- **CTO, 30s:** Agent failures are context failures and permission failures. ShogunAgent is a system of record for what agents believe about your work, with a quote behind every fact, a confidence score on every row, and a human approval on every external send. Nothing leaves the device unless you say so, and nothing is locked in.

### Words we do not use

"AI OS", "world model" (collides with robotics usage), "memory" as our category name, "benchmark #1" (the term has lost trust; we publish same-harness results only), region-specific compliance framing.

## 3. How state is acquired and delivered

![Figure 1. Raw data to agent context through the ShogunAgent engine](diagrams/architecture.svg)

*Figure 1. Raw data enters one encrypted store per principal; the Dream Cycle turns it into typed state with a quote and a confidence on every row; agents receive it as a context pack, as files or as tools, and every action passes the L1/L2/L3 gate. Agent writes and router runs flow back as events.*

Five acquisition paths, all landing in the same append-only `event_log` (dedup, redaction, instruction-shaped gate):

| Path | Content | Requires |
|---|---|---|
| Agent ingest (primary) | Conversation turns, tool results, task progress via `memory.ingest` and Anthropic memory-tool-compatible `/memories` writes | Nothing. Any agent. |
| Connector pull | Gmail, Calendar, Slack, GitHub, Linear, Notion. A human completes the sign-in; tokens live only in the principal's own store. The core refreshes them, and revokes them at the vendor where the vendor allows it (not Notion; GitHub only with the app secret) | `shogun-integrations`, hosted connect flow |
| Provider webhooks | GitHub, Linear and Slack deliveries become events directly. Notion, Calendar and Gmail send change notices; the core then fetches the text with the principal's own token (Notion pages only; no attachments, no images) | A registered hook per provider |
| Router run log | tool × task × outcome for every run through the router | Owning the router |
| Device capture (premium) | Accessibility-API text, Visual recall OCR, meeting transcripts | The ShogunAI Mac app |
| Import | ChatGPT / Claude / Cursor exports, export v1 and v2 packages | `shogun-memory-import/v1` |
| Browser extension | Visible page text, URL and title only; never screenshots | Specified, not built. Its write-only `ingest` key exists |

Inference is the ShogunAI pipeline unchanged: hourly Dream Cycle (sessionize, prefilter, schema-enforced extraction) → state tables with quote and confidence → consolidator → entity pages and period summaries.

Delivery takes three shapes, because that is what agents call in 2026: a budgeted context pack (`get_context_pack`: High as fact, Medium as "possibly", Low excluded), a virtual filesystem (`/memories/people/<name>.md`, `/memories/open_loops.md`, readable by Anthropic's memory tool with zero integration), and MCP tools (`state.*`, `recall`, `search`) plus an A2A Agent Card. Agent-to-agent handoff carries a typed state-of-work object (`handoff.create` / `handoff.accept`) that the core assembles; the receiver stores it in its own scratch namespace.

Every capability is the same on MCP, REST and the CLI, with the same permission level, the same result and the same response envelope (`usage`, `latency_ms`, `next_action`).

ShogunAgent is model-agnostic. Our own LLM use is extraction and consolidation only, in cheap batch lanes. Search is keyword-first (FTS5 trigram plus a two-character index for Japanese and Chinese); local embeddings are optional, with nomic-embed-text as the default and multilingual-e5-small as an opt-in model. No embedding ever goes to a cloud API.

## 4. Relationship to the model layer

**Where the labs will win and we do not compete:** personalization memory inside their own products, session state inside their own APIs, storage at the cost floor.

**Where they structurally will not go and we stand:** cross-vendor state (no lab becomes the neutral record for another lab's agents), portability (lock-in is their revenue model), human-approval governance across agents from different vendors, device-level truth.

**How we relate:** customer, supplier, never competitor. Anthropic's memory tool is storage-agnostic, so we are its backend; we list in the Claude Code plugin marketplace, the Anthropic directory, and the MCP Registry. OpenAI: Plugin directory plus an Agents SDK middleware. Google: ADK and Vertex memory interfaces. AWS: the external store behind AgentCore's self-managed memory tier. Microsoft Copilot's communication memory is the closest comparable; we offer the same to agents outside M365.

**Watch:** Anthropic Cowork / Claude Desktop reaching into device context; OpenAI Dreaming extending from personalization to work state; Microsoft opening communication memory through Foundry. Each is an extension inside one vendor; the more vendors a user mixes, the more we are worth.

## 5. Competitors by layer

| Layer | Players | Our difference | Stance |
|---|---|---|---|
| Memory APIs | Mem0, Zep, Letta, supermemory, Cognee, Engram | Typed state vs free text; three tiers vs read/write; local-first vs cloud-first; open engine vs closed | Direct competitors; we name a different category |
| Routers and registries | Monid, Composio, Arcade + Smithery, Glama, Coinbase Agentic Market | No permission tiers, no state | We build our own router as the front door and list on all of them |
| Enterprise context platforms | Microsoft Copilot, Glean, Jedify, Interloom | Suite-bound or data-platform-bound; not agent-neutral; no human gate | The real comparables; where we must win |
| Capture layer | Screenpipe, Limitless (Meta), Rewind (stopped) | Capture without state inference | Mostly exited. Lesson: nobody pays for raw recordings |

## 6. Moat (strongest first, honestly)

1. **Position in a two-sided market.** Tool suppliers connect to the router; agents connect over MCP. More connectors → more agents → more run data → better `discover` → more agents. This loop needs the state layer; a router alone does not have it.
2. **Compounding state.** The longer it runs, the more accurate the state; leaving loses calibrated confidence and learned patterns even though facts export freely. Continuity, not lock-in.
3. **Trust.** Zero retention, provenance on every row, six eval harnesses, OWASP ASI06 mitigations in the write path. Enterprise trust is earned through audits and time and cannot be bought.
4. **Device-level truth.** Accessibility and Visual recall capture that neither labs nor memory vendors have. Grows as platforms are added.
5. **Standards.** Bring the export format and the permission-tier taxonomy to the W3C memory interop CG and MCP annotations; be the reference implementation.
6. **Engineering lead.** ~370k lines of Rust, calibration rules, 105 additive migrations, six eval harnesses (one of them the open-loop benchmark), a selection eval and a multilingual retrieval suite. A lead, not a wall.

**Not a moat:** router code (MIT, 780 forks), the shape of a memory API, benchmark scores, model access.

## 7. Product decisions

### Build an "OpenRouter for Agents", as the front door

Owner decision. The research warnings (commodity connector code, $0.0015 median call price, consolidation by Arcade and Stripe, conflict of interest when ranking your own product, memory not fitting a spot market) remain true and become design constraints:

1. **Two layers.** Router (discover / inspect / run, metered, zero-retention proxy) may run at near-zero margin. Revenue is measured on ingest and state. Router KPI: share of router agents that start using the state layer.
2. **Borrow the cold start.** Monid-compatible connector definitions so existing endpoint definitions import; pass through Monid's hosted catalog. First-party connectors only where they differentiate: memory and state, and writes that need approval.
3. **L1/L2/L3 on every run.** Reads L1, user-data writes L2, external sends and payments L3. Lift `scope.rs` ("deny anything not in the table, ExternalSend is always L3") as is.
4. **The router learns in user-owned state.** tool × task × outcome recorded in ShogunAgent state; `discover` personalized per user and tenant; portable, not centralized.
5. **No self-preferencing.** Publish the score breakdown with price included, label first-party tools, keep directory submissions separate from the router's SKILL.md.

### Pricing: pure usage-based

No fixed subscription. Prepaid credits, `402` when exhausted, per-key and per-agent hard caps from day one.

These are the rates the gateway implements today (`apps/api/src/pricing.ts`, `/.well-known/pricing.json`). They are still launch estimates, to be revised from design partners' real usage.

| Metered item | Unit price | Notes |
|---|---|---|
| Tool run, third-party provider | provider price + 5% | One take, charged once by the wallet. Failures and empty results $0. Monid-listed tools carry Monid's price plus this one take, not two |
| Tool run, first-party connector | $0.002 / call | No take. Writes pass L2/L3 at the same price. The first-party state reads cost $0 |
| Ingest | $0.50 / 1K events | Includes extraction LLM cost. Webhook deliveries settle at the events actually stored |
| Model-invoking op | $0.10 / 1K | Consolidation, AI search, entity pages |
| State reads | $0 | `state.*`, `get_context_pack`, `recall`, `search`, `handoff.create`, `connections.list` |
| Remote L2/L3 approvals, queuing an action, asking for a payment mandate | $0 | |
| Payments under a mandate | the payment rail's fee | $0 today: the rail is a stub that moves no money |
| Hosted storage | $0.10 / GB-month | Delete and export always free |
| Free allowance | $5 per self-registered agent | |
| Local (OSS) | $0 | |
| Enterprise | same rates, 10–25% off with annual commit | BYOC / ZDR / SSO / audit included; no seat fees |

The gateway bills from what the core reports it actually did: a refused call, an error or an idempotent replay costs nothing.

Margin targets: ingest at 3–5× cost; runs near break-even at a 5% take. Revise from design partners' real usage.

### Do not build

A general router competing on price alone; our own payment rails (payments run through a pluggable rail, a stub today); accepting x402 in production (decided: it is implemented behind `X402_ENABLED`, off, see `docs/decisions/0002-x402.md`); a graph DB (typed relation rows in SQLite suffice); a Postgres port (reuse drops from 60–70% to 25%); in-house embedding research; a chat-personalization memory API; LoCoMo score-chasing; Windows/mobile capture before the ingest API; consumer apps; exposing the desktop-only tools on the server (a server build lists the core, files, handoff, payments, governance, import, state, skills and wiki groups; the Mac-only groups are listed only in the `desktop` build).

## 8. Go-to-market: developers and individuals first, OSS as the engine

**Who, in order:** (1) power users of coding agents (Claude Code, Codex, Cursor), (2) developers building agent products (Agents SDK, Vercel AI SDK, Mastra, LangGraph), (3) small teams running many agents, (4) enterprises promoted from 2–3. Consumer memory products all exited; we sell to people who run agents daily, not to general consumers. Existing ShogunAI Mac users are the day-one cohort: their agents already have read connections auto-installed.

**Open:** the engine (state tables, confidence, consolidator, Dream Cycle), local MCP / REST / CLI, connector definitions and the router engine, SKILL.md, the export format, all six eval harnesses. License: Apache-2.0. **Closed:** hosted sync, remote approvals, wallet and billing, the hosted router catalog and provider contracts. Funnel: same API locally and hosted, switch `baseURL`. Unlike supermemory, the engine itself is open.

**OSS launch mechanics:** a README GIF that works in ten seconds (`npx shogunagent`, and your coding agent knows your open loops); Show HN built around a reproducible claim (same-harness results, audit-style posts), not a feature list; the benchmark harness published so competitors' numbers can be reproduced; an "open-loop tracking" benchmark offered to the community; connector definitions as the low-friction contribution path.

**Agent-native distribution:** SKILL.md, `npx shogunagent help --json`, a docs MCP, `llms.txt`. Agents install it themselves and spend within their own prepaid budget.

**Platform shelves, all in English, from the start:** Claude Code plugin marketplace → Anthropic directory; official MCP Registry; Cursor / Windsurf MCP configs; OpenAI Plugin directory; Glama, Smithery, Composio toolkit, Monid once a memory category exists.

**One-line integrations:** `withShogunAgent()` for Vercel AI SDK, OpenAI Agents SDK, Claude Agent SDK, Mastra, LangGraph, CrewAI. This is what drove Mem0's 5× API growth.

**Not doing:** paid ads, SEO-first, conference booths, enterprise sales first.

**Conversion ladder:** OSS local ($0) → hosted sync (second device or agent) → wallet top-up (connector ingest, router runs) → team (shared state, remote approvals) → enterprise (BYOC, ZDR, audit). The first two rungs are free and build trust and volume; revenue starts at the third.

## 9. Roadmap (sequence only, no durations)

Development runs AI-first. Phases are defined by order and exit criteria; no day or month counts. The code for every phase below is in this repository and passes its checks. "Done" in the Code column means built and tested, not released. The exit criteria are market measurements and can only be met after launch.

| Phase | Deliverables | Code | Exit criteria (estimates) |
|---|---|---|---|
| **1. Core and router skeleton** | `shogunagent-core` standalone on Linux (one SQLCipher file per principal); Streamable HTTP MCP with the 21 frozen core tools; API-key auth; `/memories` projection; export format v1; OSS release. Router: discover / inspect / run, Monid-compatible connectors, hosted passthrough, memory/state category, per-key caps, L1/L2/L3 on every run. Prepaid credits and 402. SKILL.md, Claude Code plugin, MCP Registry entry (validated, not submitted). OSS-ready under Apache-2.0. Same-harness results in `docs/benchmarks/`. | Done | Sustained `tools/call` traffic from external agents; design partners signed; one published benchmark; router-to-state conversion measurable |
| **2. Approvals, SDKs, billing, catalog** | Remote L2/L3 approvals (web, Slack, webhooks); A2A Agent Card; TS/Python SDKs with `withShogunAgent()`; agent-scratch namespaces with ACL and audit; Stripe usage billing and wallet; OAuth 2.1 for agents; personalized `discover`; self-serve provider listing; directory submission packs; local embeddings | Done | Growing paid wallet users; ingest gross margin ≥ 3× cost; LLM cost ≤ 15% of revenue; improving router-to-state conversion |
| **3. Enterprise, standards, payments** | Approved actions execute exactly once; a persistent approval ledger; hash-chained audit export; tenants, SSO (OIDC, SAML), SCIM, regions; customer-managed keys (AWS, GCP, Vault); Helm and Compose; export format v2 with interop converters; open-loop benchmark v1; provider payouts and the open catalog; x402 decided | Done | Enterprise contracts; NRR > 110%; top-3 on same-harness multi-session and temporal categories; rising first-party connector share |
| **4. Launch readiness** | Router runs charged once through the wallet, with a durable settlement outbox; hosted `/mcp` relayed to each principal's sidecar and billed from the core's own report; handoff assembled in the core on three faces; MCP conformance suite; SDK tests inside real agent frameworks; body-free metrics and traces; load tests; threat model, fuzzing, secret scanning; manual-only publish and deploy workflows; production runbook | Done | Hosted service live; conformance green against the deployed URL; first directory listings accepted |
| **5. Expansion** | Multilingual embeddings and retrieval suite; provider webhooks and read-sync; hosted connector sign-in, refresh, status and disconnect (Gmail, Calendar, Slack, Linear, GitHub, Notion), with vendor revocation where the vendor allows it; first-party Gmail and Notion reads; agent payment mandates; export v2 community-group draft and JSON-LD context; open-loop rules for passive, Japanese and cross-thread phrasing | Done | Non-English recall on par with English; state growing from sources beyond the Mac; agent payments settled within L3 and caps |

## 10. What carries over from ShogunAI

Roughly 30–40% by lines of code and 70%+ of the hard parts.

Reuse: `shogun-memory` (state tables, `state_provenance`, loops, entity pages, federated search, sanitize, traceability, import; keep V1–V91 on SQLite, add `tenant_id` / `uid` / `actor` by additive migration), `shogun-fusion`, `shogun-agents`, `shogun-redact`, `shogun-license` (verify), `shogun-mcp` (`MemoryBackend`, `ApiLevel`, toolsets, source policy, rate limit, REST routing; replace the Keychain-bound settings and launcher check with env vars and `apps/api` tokens), platform-neutral `shogun-core` (dreamcycle, llm, ai_search, context, `shogun_api` / `shogun_mcp` binaries), `apps/api` (Ed25519 auth, reserve-then-settle metering, zero-retention logger).

Does not carry over: the macOS capture layer, Keychain-bound secrets, the Tauri desktop, the single-tenant assumption.

This is how it was built: `shogun-api --multi` serves one SQLCipher file per principal, and `apps/api` sits in front for auth, metering and the 402. The desktop-only code sits behind the `desktop` cargo feature, off by default. What moved and why is recorded in `docs/MIGRATION.md`.

## 11. Agent-native commerce: designed from day one to be chosen by agents

The buyer is increasingly an agent, not a person. An agent discovers a service, compares it on price, latency, health and trust, onboards itself, pays within a budget, and re-selects or abandons it based on how the first calls went. Every one of those steps is designed for, from the first release.

### Principles

1. **Zero-config first call.** An agent with no key can call `discover`, `inspect`, and a tightly scoped read. The first write or metered run returns `402` with everything needed to proceed. Nothing requires a human before the agent has seen value.
2. **Every response is self-describing.** Each result carries `usage` (cost, units), `latency_ms`, `next_action` on error (what to do, not just what failed), and `hints` (a cheaper or better-fitting alternative when one exists, including a competitor's connector inside the router). Agents' selection loops penalize opaque failures; honesty is the retention mechanism.
3. **Machine-readable at every well-known location.** The same identity, tool names and schemas appear everywhere an agent might look, and the human-facing docs are derived from the machine-facing ones, never the reverse.
4. **Descriptions are written for ranking, not for marketing.** Registries and routers rank on description text. Tool descriptions name the tasks an agent is trying to do ("find what the user committed to", "what is pending with this person", "run a tool and keep the result as state") and the exclusions ("not for storing chat transcripts verbatim"). Each description is tested, not guessed (see selection eval).
5. **One human approval, at most, and only for money.** A card is attached once through URL-mode elicitation; after that the agent spends within its cap. Reads never ask. Permission tiers (L1/L2/L3) govern actions on the user's world; budget caps govern spend. The two are separate and both are visible to the agent before it acts.
6. **Stability is a feature agents can measure.** Frozen wire names, a published deprecation policy, idempotency keys on every write, `ttlMs` on lists, honored `Retry-After`, and a public incident history. An agent that was burned once does not come back.

### Day-one artifacts

| Surface | What ships | Why an agent needs it |
|---|---|---|
| `/.well-known/mcp/server-card.json` | MCP Server Card: endpoint, auth, tool list with annotations | Progressive discovery without a handshake |
| `server.json` in the MCP Registry | Canonical registry entry | Where Claude Code, Cursor and gateways look first |
| `/.well-known/agent-card.json` | Signed A2A Agent Card | Cross-organization handoff and identity |
| `llms.txt` and `llms-full.txt` | Agent-readable docs index | Agents read docs before humans do |
| `SKILL.md` | Trigger conditions and non-trigger exceptions, setup, auth, precedence rules, cost warnings, status semantics, L2/L3 meaning, numbered rules | Agents install and operate it themselves |
| `openapi.json` | OpenAPI 3.1 for the REST face | Code-generating agents |
| `/.well-known/pricing.json` | Machine-readable rate card, free allowance, caps | Price comparison before the first call |
| `/health` and `/.well-known/status.json` | Live p50 / p95, error rate, incident history | Health-weighted ranking |
| `benchmarks.json` | Same-harness results with reader, judge, seeds, cost per answer | Trust signal that can be re-run |
| `quote` endpoint and `dryRun` on every metered call | Exact cost before committing | Budgeted agents refuse unknown costs |
| 402 payload | Amount, units, accepted rails, human-approval URL, `Retry-After` | Self-service top-up with no support ticket |
| `POST /v1/agents` | Agent self-registration: returns a scoped key and the free allowance, rate-limited and abuse-gated, no email | Onboarding without a form |
| Referral in the key | `ref` attribute credited to the referring agent's or developer's wallet | Agents become the channel |

All of these are generated from code (invariant 12). The well-known documents, pricing, quote and registration are served by `apps/api`; `server.json`, `SKILL.md` and `benchmarks.json` are files in this repository. Two are partial: the `ref` attribute is recorded at registration but not yet credited, and `server.json` validates against the MCP Registry schema but has not been submitted.

### Payment design

Prepaid wallet settled through Stripe. The `402` response follows the x402 payment-required shape so wallet-carrying agents already know how to read it. Accepting x402 settlement is implemented behind a flag and stays off in production (ADR 0002). Per-key and per-agent hard caps (daily and monthly) are mandatory and default to conservative values. Receipts are machine-readable per call. Failed and empty results cost nothing. Disputes are an API, not an email.

Agents can also pay on the user's behalf under a **payment mandate**: a human grants a bounded permission (payee or payee class, per-payment and total caps, currency, expiry, purpose). Granting or widening one is an L3 human approval; spending within it is pre-approved, recorded before the rail is called and never paid twice; anything outside it falls back to a one-off L3 approval. Revoking or narrowing takes effect at once from any face.

### Selection eval: the metric that matters

A harness that runs from CI: given a task description, which tool does Claude, GPT, Gemini and an open model pick when ShogunAgent sits alongside Mem0, supermemory, Zep and the router's own alternatives, across the MCP Registry listing, the Claude Code plugin description, the OpenAI Plugin listing and the router's `discover`? The result is a selection rate per surface per model. Description changes ship only when this number does not fall. This is the agent-era equivalent of conversion-rate optimization, and nobody in the category runs it.

### Being chosen inside our own router

The router ranks third-party tools and our own connectors by the same published score (task fit, price, health, latency), with first-party items labeled and a written no-self-preferencing policy. The state layer is not listed as a competing tool; it is the substrate every run writes into. Agents choose tools in the router; they choose the router because the results persist as their user's state.

## 12. Where the implementation stands

Verified on the latest commit of `main`:

| Check | Result |
|---|---|
| Rust workspace tests | 5,113 passed, 0 failed |
| Core with server features | 2,283 passed, 0 failed |
| MCP server tests (wire freeze included) | 939 passed, 0 failed |
| Feature matrix, upgrade through every migration (V1–V105), seven invariant guards | all pass |
| Gateway (`apps/api`) | 828 Node tests, 24 Workers tests |
| Router (`apps/router`) | 130 tests |
| SDKs | TypeScript 87 unit and 16 real-framework tests; Python 88 |
| MCP conformance suite against a local server | 33 pass, 0 fail, 1 skipped |
| Selection-eval gate | pass, no surface dropped |

Benchmarks, read with their caveats (`docs/benchmarks/`):

- **Golden search eval** (132 cases, mostly Japanese, keyword search): recall@5 0.985, MRR 0.948, false answers 0.000.
- **Multilingual retrieval suite** (English, Japanese, Spanish, German, French, Chinese): questions without an answer return nothing in every language. With keyword search, the default, recall@5 is 0.552: paraphrased and cross-language questions need embeddings. With the deterministic test embedder (not a real model) it is 0.966. The real multilingual model is not pinned or calibrated yet.
- **Open-loop tracking:** 1.000 on the generator's own templates, which the rules were written against, so treat that as a ceiling. On phrasings written separately, the headline score is 0.665, up from 0.489, with precision 1.000 and no loop closed by mistake.

What remains is outside code:

1. One company name, registered address, company number, public API hostname, and security, privacy and support contacts. The files currently disagree.
2. Legal review of the privacy policy, terms, data processing agreement, SLA and zero-retention terms.
3. How the gateway reaches the per-principal server (recommended: a Node gateway in the same pod).
4. Whether a send executed after approval carries its own fee.
5. The ChatGPT listing scope (recommended: free read tools only, billing linked to our site).
6. OAuth apps at Google (with Gmail's restricted-scope verification), Slack, Linear, GitHub and Notion; network egress from the server to them; one staging run per provider.
7. The GitHub `release`, `staging` and `production` environments, and npm and PyPI trusted publishing.
8. Pinning and calibrating the multilingual embedding model on a machine that has it.
9. The go-ahead for any publish, deploy or directory submission.

## Repository layout

```
README.md                  This document (English)
README.ja.md               This document (Japanese)
CLAUDE.md                  Rules for working in this repository
SKILL.md                   Agent-facing skill: when to use it, setup, cost, permission tiers
SECURITY.md                How to report a vulnerability
crates/                    Rust workspace: engine, MCP/REST/CLI, evals and benchmarks (memory, loop, pii, dedup, extract, fusion)
apps/api/                  Gateway and relay (Hono on Cloudflare Workers + Postgres): auth, metering, 402, wallet, approvals, enterprise, connectors, ingest hooks
apps/router/               Router: discover / inspect / run, first-party connectors, Monid passthrough, settlement outbox
apps/docs/                 Public Memory API docs
packages/sdk-ts, sdk-py    SDKs with withShogunAgent()
packages/cli               npx shogunagent
packages/conformance       MCP conformance suite
packages/interop           Export v2 converters with loss reports
packages/selection-eval    Selection eval harness and gate
deploy/                    Helm chart, Compose stack, BYOC runbook
bench/load/                Load-test scripts
fuzz/                      cargo-fuzz targets
integrations/monid/        Monid listing preparation
scripts/                   Invariant guards and generators
docs/implementation-plan.md  Epics and tasks per phase, with status
docs/MIGRATION.md          What was migrated from ShogunAI, what was not, and why
docs/api/                  Agent-facing API documents
docs/deploy/production.md  Production runbook
docs/security/             Threat model
docs/standards/            Export v2 community-group draft (not submitted)
docs/benchmarks/           Benchmark results and how to reproduce them
docs/migrations/           Rollback notes per schema migration
reports/                   Full strategy report (Markdown and bilingual HTML)
research_notes/            Research notes with inline sources
```

## Open questions

- Do developers without the Mac app get enough state from the ingest API, webhooks and connectors alone?
- Do router agents convert to the state layer at a rate that justifies the router?
- Which export format wins the W3C memory-interop process, and can ours be it?
- How far do open-loop rules carry on real users' phrasing before a model review is needed?

All four are answered by design partners' real usage, not by further research.
