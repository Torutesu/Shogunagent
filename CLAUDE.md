# CLAUDE.md — ShogunAgent

Rules for working in this repository. Strategy and product decisions live in `README.md` (English) and `README.ja.md` (Japanese); the implementation plan is `docs/implementation-plan.md`; what was migrated from ShogunAI and why is `docs/MIGRATION.md`. The ShogunAI `CLAUDE.md` is kept at `docs/shogunai-origin/CLAUDE.shogunai.md` as reference only. Where the two disagree, this file wins.

## One-line definition

The state layer for AI agents: a typed, provenance-backed model of a person's work (people, projects, commitments, open loops) that many agents read and write over MCP / REST / CLI, with L1/L2/L3 permission tiers on every action. Local-first, open engine, portable. Not a memory API.

## Invariants (never write code that violates these)

1. **The Rust core owns the data.** DB, inference, permission decisions, approval queue and the per-principal store live in `crates/`. `apps/api` (TypeScript) does auth, metering, 402, router control plane and webhooks, and never reads or stores request or response bodies.
2. **Bodies never persist outside the principal's store.** Hosted paths (gateway, router, relay, logs, metrics, error reports) carry no bodies. `traceability_log` has no body column and stays that way. Streams are relayed, not buffered.
3. **L1 never sends externally.** Sending, posting, paying, calendar creation are L3 by type (`SendAction` in `shogun-agents`). The router's `run` follows `shogun-integrations/src/scope.rs`: an operation not in the table is denied; `ExternalSend` is always L3. This holds for every caller, agent or human.
4. **Three faces, one behavior.** Every capability is exposed over MCP, REST and CLI with the same permission level and the same result. A tool that exists on one face exists on all three.
5. **Provenance on every fact.** A state row without `state_provenance` is rejected at the repository layer. A write without `actor` is rejected (from V92). Confidence bands are enforced in one place (`shogun-fusion/src/confidence.rs`); low-confidence state is never rendered as fact.
6. **Tenancy is a file boundary.** One principal = one SQLCipher file. Cross-principal reads happen only through explicit ACLs (Team), never by query. `tests/invariant_tenant.rs` must show zero rows leak between principals.
7. **Secrets are never written in clear.** No Keychain here; API keys are stored hashed, provider keys are injected at the transport layer and never reach the router engine process, tenant keys come from KMS or environment. Hold secrets in the `Secret` type; `.expose(` only in the allowlisted files (`scripts/check-secret-exposure.py`).
8. **Migrations are additive and frozen.** V1–V91 are never edited, renamed or renumbered. New schema goes in `V<max+1>__*.sql` with a rollback note in `docs/migrations/`. `scripts/check-migrations.py` and the upgrade-lineage test must pass.
9. **One HTTP egress.** Only `shogun-core` depends on an HTTP or WebSocket client (`scripts/check-http-egress.py`).
10. **Reads are free.** `state.*`, `memory.get_context_pack`, `memory.recall`, `memory.search` and remote approvals are metered for observability (`Mcp-Method` / `Mcp-Name`) and never billed. Billing attaches to ingest, model-invoking operations, tool runs and storage only.
11. **Wire names are frozen.** The 21 core tool names, their input schemas and their MCP annotations are snapshot-tested (`wire-freeze`). Changing one requires a new name and a deprecation entry, never an in-place change.
12. **Agent-readable surfaces are generated, not hand-written.** `server-card.json`, `openapi.json`, `pricing.json`, tool descriptions' annotations and `llms.txt` are derived from code and config. Human docs derive from them, never the reverse.
13. **No self-preferencing in the router.** First-party connectors are ranked by the same published score as third-party ones, labeled `first_party`, with price inside `scoreBreakdown`.

## Code rules

- Rust: `clippy::all` deny, `unwrap_used` deny, rustc `unused` deny (workspace lints). `expect` is allowed in tests. Every crate builds and tests on Linux; there is no macOS-only member.
- macOS-only code inherited from ShogunAI sits behind the cargo feature `desktop` of `shogun-core` and `shogun-mcp` (E1-T7, default off; `audio` implies it). The default build is the server and carries no module that exists only for the Mac app and no `cfg(target_os = "macos")` site: `scripts/check-desktop-gate.py` (run by `check-feature-matrix.sh` and the CI guards) fails on one outside its justified allowlist. New Mac-only code goes behind `desktop` (in `shogun_core::desktop` when it reaches the Keychain or the host shell), with a `not(feature = "desktop")` arm that answers what the server answers; `--features desktop` must keep compiling on Linux. Do not add new macOS dependencies, and do not gate a module the data layer or a Memory API tool depends on — split the shared type out instead. Which modules are behind the feature and which stay is recorded in `docs/MIGRATION.md`.
- Every write path takes an idempotency key. Every metered response returns `usage`, `latency_ms`, and on error `next_action`.
- Tool descriptions are written for agents: name the task, name the exclusions, state the cost class. Changes to descriptions, `SKILL.md` or directory listings must not lower the selection-eval rate (E11).
- No telemetry by default. Opt-in only, counts only, never content.
- UI copy does not exist here; API error messages are plain English and say what to do next.
- Conventional Commits. Schema changes ship with the migration and its rollback note in the same commit.

## Local verification (run before pushing)

```bash
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
cargo clippy --workspace --all-targets --locked
cargo test --workspace --locked
scripts/check-feature-matrix.sh
for g in http-egress secret-exposure media-writes log-hygiene migrations batch-source-filter desktop-gate; do
  python3 scripts/check-$g.py --self-test && python3 scripts/check-$g.py
done
pnpm install && pnpm --filter @shogun-ai/api typecheck && pnpm --filter @shogun-ai/api test:node
```

## Repository layout

```
crates/            Rust workspace (engine, MCP/REST/CLI, evals). See docs/MIGRATION.md.
apps/api/          Gateway and relay (Hono on Cloudflare Workers + Postgres).
apps/video/        Launch film and explainer videos as HTML compositions (render with scripts/render.mjs).
packages/config/   Shared config (license public key).
scripts/           Invariant guards and the feature-matrix runner.
docs/              implementation-plan.md, MIGRATION.md, migrations/ (rollbacks), mcp/, shogunai-origin/ (reference).
reports/           Strategy report and bilingual HTML.
research_notes/    Research behind the strategy, with sources.
```

## Branching

Work happens on feature branches; `main` receives the merged result. Do not push to `main` directly except to fast-forward a reviewed branch.
