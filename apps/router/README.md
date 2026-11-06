# @shogunagent/router

The ShogunAgent router — "OpenRouter for Agents" (README §7, §11): one base URL where an agent **discovers** a tool for a task, **inspects** its schema and price, and **runs** it with the user's permission tiers applied, metered usage settled and the result ready to become state. TypeScript on Hono, deployed as a Cloudflare Worker next to `apps/api` (decision: `docs/decisions/0001-router-engine.md`). E8 skeleton: the engine, the connector format, the first-party connector, the Monid passthrough and both faces are here; the wallet (E9), the conformant MCP transport (E4) and the `tool_runs` sink in the state layer are wired behind the interfaces this package exports.

## What it guarantees

- **Zero retention** (invariant 2). The only logger accepts a closed set of fields (route, status, latency, tool id, outcome, counts); the run record has no body field; `test/zero-retention.test.ts` drives sentinel strings through REST, MCP and the passthrough and asserts none reaches a log or a record.
- **No credential in the engine** (invariant 7). Provider keys are named by environment variable and injected in `src/transport.ts`, the only module that calls `fetch`. Hooks and the engine receive a request type with no credential field (`test/transport.test.ts`).
- **L1/L2/L3 on every run** (invariant 3). `src/scope.ts`: an operation not in the table is denied; sends and payments are L3 (approval URL, never executed directly); user-data writes are L2 (`input_required` once); reads are L1.
- **No self-preferencing** (invariant 13). First-party tools are scored by the same published formula, labeled `first_party`, with price inside `scoreBreakdown`. Task fit multiplies the quality terms, so a $0 first-party read cannot outrank a better-fitting paid tool on price alone.
- **Three faces, one behavior** (invariant 4). REST and MCP go through one dispatcher (`src/engine.ts`); the CLI face wraps REST (E13).
- **Every response is self-describing** (README §11). `usage`, `latency_ms`, `hints`, and on error `next_action`.

## Verbs

| Verb | REST | MCP tool | Auth | Price |
|---|---|---|---|---|
| discover | `POST /v1/router/discover` | `router.discover` | none | $0 |
| inspect | `POST /v1/router/inspect` | `router.inspect` | none | $0 |
| run | `POST /v1/router/run` | `router.run` | bearer for metered or first-party tools | the tool's quote; failures and empty results $0 |

Also: `GET /health`, `GET /v1/router/catalog` (the content-hashed manifest), `POST /mcp` (JSON-RPC: `initialize`, `ping`, `tools/list`, `tools/call`). Every response carries `Mcp-Method` and, for a tool call, `Mcp-Name`, which is what `apps/api` meters on without reading bodies (invariant 10).

### Open catalog (E29)

Public, no key, generated from the compiled catalog and the live health window (`src/open-catalog.ts`):

| Route | What |
|---|---|
| `GET /v1/catalog` | every listed endpoint: price (`quote_micro_usd`, usage model, failures and empty results $0), health (success rate, p50/p95, `observed` or `declared`), categories, `first_party` label, permission level, data-handling statement, entry hash; plus the score formula, weights and the no-self-preferencing policy. Filters: `?category=`, `?provider=`, `?first_party=true|false` |
| `GET /v1/catalog/{tool_id}` | one entry with its description, exclusions, notes, input/output JSON Schema and the formula |
| `GET /catalog` | the same list as a server-rendered HTML page: no JavaScript, every value escaped, `Content-Security-Policy: default-src 'none'` |

Legal name, contact and payout reference from `provider.json` are not in the compiled catalog, so they cannot reach these routes; `test/open-catalog.test.ts` checks every manifest's values against every public body.

### discover

```bash
curl -s -X POST http://localhost:8787/v1/router/discover \
  -H 'content-type: application/json' \
  -d '{"task":"what did the user commit to","limit":3}'
```

```json
{
  "result": {
    "candidates": [
      {
        "tool_id": "shogunagent.state.commitments.list",
        "provider_id": "shogunagent",
        "first_party": true,
        "summary": "Find what the user committed to: promises, deliverables and deadlines they made to others.",
        "categories": ["state", "memory"],
        "permission_level": "L1",
        "score": 0.787,
        "scoreBreakdown": { "task_fit": 0.8089, "price": 1, "health": 0.999, "latency": 0.8929, "personal": 0.9729 },
        "price": { "model": "per_call", "unit": "call", "cost_micro_usd": 0, "quote_micro_usd": 0 },
        "p50_ms": 40,
        "p95_ms": 120,
        "personal": { "value": 0.9729, "runs": 0, "successes": 0, "basis": "none", "level": null, "cluster": "commit user" }
      }
    ],
    "ranking": {
      "formula": "score = task_fit × (0.3825*price + 0.255*health + 0.2125*latency + 0.15*personal). …",
      "weights": { "price": 0.3825, "health": 0.255, "latency": 0.2125, "personal": 0.15 },
      "personal": { "formula": "personal = 0.6*success_rate + 0.25*price(cost_micro_usd / successes) + 0.15*hint_success_rate, …", "components": { "success_rate": 0.6, "cost_per_success": 0.25, "hint_success": 0.15 }, "min_runs": 5, "basis": "none", "cluster": "commit user" }
    },
    "catalog_hash": "…",
    "considered": 25
  },
  "usage": { "unit": "call", "quantity": 1, "cost_micro_usd": 0 },
  "latency_ms": 1,
  "hints": [],
  "next_action": null
}
```

Ranking (`src/discover.ts`): `task_fit` is BM25 over id, summary (×2), categories (×2), description and notes, normalised by the query's bound to [0, 1), with exclusion text left out of the index; `price = 1 / (1 + ln(1 + quote_micro_usd / 1000))` ($0 → 1, strictly decreasing); `health` = observed success rate (declared baseline until 20 runs); `latency = 1 / (1 + p95_ms / 1000)`; `personal` is described below. Ties break on lower quote, then `tool_id`. `budget_micro_usd` drops tools whose quote exceeds it; `categories` restricts; `limit` ≤ 50.

#### Personal factor (E20)

`discover` called with an agent key as the bearer is personalised from that principal's own run history (`src/personal.ts`); without a key, or with an invalid one, it is the same for everyone.

- **What is learned.** Every `ToolRunRecord` (body-less, `actor` is a hash of the key) also goes to `PersonalRanker`, which keeps per principal a row per tool × task cluster: counted runs, successes, runs that followed a hint and how many of those succeeded, and cost spent. Counted outcomes are `ok` (success), `empty`, `provider_error`, `transport_error`; permission stops, input errors, auth/payment stops, replays and anonymous runs are not evidence about the tool.
- **Task clusters** use the same BM25 tokenizer as task fit: the cluster of a task phrase is its 3 most distinctive stemmed terms (highest catalog IDF), sorted — `"what did the user commit to"` → `"commit user"`. No model call.
- **Value.** `personal = 0.6·success_rate + 0.25·price(cost_micro_usd / successes) + 0.15·hint_success_rate` over the rows of the task's cluster when it has ≥ 5 runs, otherwise over the tool's rows across clusters when those have ≥ 5 runs (`level: "task_cluster" | "tool"`). `price(·)` is the same price curve as the ranking; `hint_success_rate` falls back to `success_rate` when no hint was followed.
- **Weight.** `PERSONAL_WEIGHT = 0.15`. The E8 mix (0.45 price, 0.30 health, 0.25 latency) is scaled by 0.85, so the published weights are 0.3825 / 0.255 / 0.2125 / 0.15 and `score = task_fit × (…)` as before.
- **Neutral.** With fewer than 5 counted runs (`basis: "none"`), `personal` equals the candidate's own `0.45·price + 0.30·health + 0.25·latency`, so its score is exactly the unpersonalised score — the term has no effect until there is evidence.
- **Explained.** Every candidate carries `personal: { value, runs, successes, basis: "principal" | "none", level, cluster }`, and `ranking.personal` publishes the formula, components and minimum.
- **Isolated.** `PersonalStore` (`record(principal, …)`, `profile(principal)`) has no cross-principal read. `InMemoryPersonalStore` is the development stub; in production the store is the principal's own ShogunAgent state — `tool_runs` (V94) in that principal's file, reached through the state sidecar — never a hosted shared database. `test/personal.test.ts` shows that principal A's history never changes principal B's ranking.
- **No self-preferencing.** The factor is computed the same way for first- and third-party tools; `test/personal.test.ts` shows a first-party tool with a worse history ranked below a third-party tool with a better one at equal fit and price.

### inspect

```bash
curl -s -X POST http://localhost:8787/v1/router/inspect \
  -H 'content-type: application/json' \
  -d '{"tool_id":"monid.strale.x402.url-to-text"}'
```

Returns the catalog entry (schemas, categories, notes, examples), `price` (`quote_micro_usd`, `failures_cost: 0`, `empty_results_cost: 0`), `permission` (class, level and what the level means for the caller), live `health` with its source, a `run_example`, and `hints` — cheaper endpoints in the same category, cheapest first, whoever provides them (here `monid.contextdev.web.scrape.markdown` at $0.000945 against $0.024948).

### run

```bash
curl -s -X POST http://localhost:8787/v1/router/run \
  -H 'content-type: application/json' -H 'authorization: Bearer sa_…' \
  -d '{"tool_id":"monid.tinyfish.web-search","input":{"query":"shogunagent"},"idempotency_key":"7b1c…","task_hint":"search the web for a company"}'
```

| Outcome | `resultType` | status | `next_action` |
|---|---|---|---|
| L1 executed | `result` (+ `run_id`, `replayed`) | 200 | `null` |
| `dryRun: true` | `quote` | 200 | per level |
| L2, not yet confirmed | `input_required` (MRTR shape: `inputRequests`, retry with `inputResponses: { confirm: true }`) | 200 | `provide_input` |
| L3 | `approval_required` (`approval.url`, 15-minute expiry) | 200 | `approve_at_url` |
| schema mismatch | `error` `invalid_input` (+ `details`) | 400 | `fix_input` |
| unknown tool / not in table | `error` `tool_not_found` / `operation_denied` | 404 / 403 | `discover` |
| no key for a metered or first-party tool | `error` `authentication_required` | 401 | `authenticate` |
| wallet short | `error` `payment_required` + x402-shaped `payment` | 402 | `top_up` |
| provider answered 404: nothing exists for this input (an unknown id) | `error` `not_found` ($0, not counted against the provider's health) | 404 | `fix_input` |
| provider failed / unreachable | `error` `upstream_error` ($0) | 502 | `try_alternative` / `retry_after` |
| provider not configured | `error` `provider_unavailable` | 503 | `try_alternative` |
| wallet unavailable / throttled (E48) | `error` `provider_unavailable` / `rate_limited`, with `retry_after` and the `Retry-After` header carrying the wallet's own value | 503 / 429 | `retry_after` |
| first-party connector, no account connected for the caller's principal (E48) | `error` `connector_not_connected` ($0) | 403 | `authenticate` |
| repeated `idempotency_key`, same input, run completed | `replay` (`run_id`, `replayed: true`, `original: { outcome, usage, completed_at }`, no `result`), usage 0 | 200 | `null` |
| repeated key while the first run is still running | `error` `idempotency_in_progress` + `retry_after` / `Retry-After` | 409 | `retry_after` |
| repeated key, different input | `error` `idempotency_conflict` | 409 | `fix_input` |

Each run emits one `ToolRunRecord { ts, actor, tool_id, task_hint, outcome, cost_micro_usd, latency_ms, hint_followed }` to the `RunLogSink` (no bodies; `actor` is a hash of the key).

Each run that reserved also settles exactly once with a `RunSettlement { format, run_id, receipt_id, ts, tool_id, provider_id, first_party, payout_account_ref, marketplace, outcome, quantity, provider_micro_usd }` (E29, E32; format `shogunagent-run-settlement/2`): `outcome` is `ok`, `empty`, `provider_error` or `transport_error`, and `provider_micro_usd` is the provider's price for an `ok` run and 0 otherwise. The settlement has no actor and no body; `run_id` is the receiver's idempotency key.

**One charge, one take (E32).** In production the router holds no balance: `ApiWalletClient` (`src/wallet.ts`) reserves each metered run on apps/api's wallet (`POST /v1/internal/router/reserve`, the caller's key forwarded in `X-Caller-Authorization`) and settles it through `POST /v1/internal/router/settlements`. Only the provider's price crosses the wire; apps/api adds the take (`RUN_MARKUP_BPS`, set once in `packages/config/pricing.json`, which `src/take.ts` reads for the prices discover / inspect / dryRun quote). Catalog `usage` figures are provider prices; `quote_micro_usd` is the caller's price (provider + one take, none for first-party), `provider_quote_micro_usd` the provider's. A $0 tool never calls the wallet.

### Idempotency (E48)

Every `run` carries an `idempotency_key`, and a key never runs or charges twice for one caller, on any isolate. In production the record lives in Postgres (`router_run_idempotency`, `sql/run-idempotency.sql`, the same `HYPERDRIVE` binding as the outbox; `src/idempotency.ts`); without the binding, a per-isolate in-memory store (development).

- The key is **claimed atomically** right before money is reserved (one `INSERT … ON CONFLICT DO UPDATE … WHERE … RETURNING`). Of two isolates racing on one key exactly one runs; the other answers `409 idempotency_in_progress` with `Retry-After`.
- A run that produced a result marks its key **done before its settlement** is written, so a crash between the two can only under-charge (the outbox draft releases the hold at $0), never run and charge again.
- A run that failed or was refused (nothing charged) **releases** its key, so the same key can be retried.
- A claim whose isolate died is taken over by a retry with the same input after 10 minutes (the outbox's abandon window, by which time the dead run's hold was released at $0).
- **No body is stored** (invariant 2). A row holds the actor (the 12-hex hash of the caller's key), a SHA-256 of the idempotency key, the tool id, a salted fingerprint of the input (conflict detection only), the run id, the state, the outcome and the usage the caller was charged. So a replay cannot repeat the result: it answers `resultType: "replay"` with the `run_id` and `original: { outcome, usage, completed_at }`. The result was returned once, to the call that ran it; to get a fresh one, run again with a new key (and pay again). Records answer for 24 hours after completion; the cron prunes them.
- If the store is unreachable, a free L1 read runs without it (running it twice is harmless and costs nothing, logged `idempotency_bypassed`); every other run is refused `503 provider_unavailable` with `retry_after`, before anything is reserved or called.

**Durable outbox (E33).** Every settlement is written to `router_settlement_outbox` (`sql/settlement-outbox.sql`, Postgres through the `HYPERDRIVE` binding; `src/outbox.ts`) before it is sent: a $0 draft before apps/api holds money, the final settlement before delivery. Delivery is idempotent by `run_id`; the cron `scheduled` handler retries due rows with exponential backoff (30 s → 1 h, never giving up on a retryable status), releases the hold of a run abandoned for 10 minutes at $0, and keeps malformed rows as `dead` for an operator. Details: docs/api/payouts.md "Delivery".

### MCP

```bash
curl -s -X POST http://localhost:8787/mcp -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"router.discover","arguments":{"task":"read a web page"}}}'
```

`tools/list` returns the three tools with input schemas generated from `src/verbs.ts` and annotations (`readOnlyHint` true for discover/inspect, `openWorldHint` true for run). `tools/call` returns `content[0].text` and `structuredContent` with the same body the REST face returns; `isError` mirrors the HTTP status.

## Connectors

Format: `connectors/schema.md`. Registry: `connectors/index.ts`. Build the content-hashed catalog with `pnpm --filter @shogunagent/router build:catalog` (writes `dist/catalog.json` and `dist/catalog.<hash12>.json`; the compiler rejects duplicate ids, unknown categories, missing scope class or health, bad usage numbers and fixtures that fail their schema).

- `shogunagent` (first-party, `caller_bearer` auth to the state REST face; base URL from `SHOGUNAGENT_API_BASE_URL`): `state.commitments.list`, `state.open_loops.list`, `state.people.get`, `memory.recall` — reads, L1, $0 — and `actions.execute` — `external_send`, L3, $0.002.
- First-party connectors to the user's services (E22; Gmail reads added in E45, Notion reads in E51 once the core gained Notion's Basic-auth token exchange and a hosted Notion sign-in; only providers the core can authenticate on the hosted service), each with `provider.json`, fixtures and replay tests, $0.002 per call (README §7). Auth is `caller_connector` (E48): the tool acts as the **calling principal's own account**. In production (`ROUTER_WALLET_URL` set) the transport fetches that principal's access token for the connector's slot from apps/api at run time (`POST /v1/internal/router/connector-token`, contract in `docs/api/connector-tokens.md`), injects it into the one vendor request and drops it; no Worker secret is shared by every caller. A principal with no account connected gets `403 connector_not_connected` (`next_action: authenticate`, $0). The `credential_env` variables are read only in development, with `ROUTER_DEV_SHARED_CREDENTIALS=1` and no `ROUTER_WALLET_URL`; with the wallet URL set the flag is ignored and logged `dev_shared_credentials_ignored`.

  | tool_id | class | level | token slot (production) | credential_env (development only) |
  |---|---|---|---|---|
  | `gmail.messages.search` (E45) | `read` | L1 | `gmail-tokenset` | `GOOGLE_OAUTH_ACCESS_TOKEN` |
  | `gmail.messages.get` (E45) | `read` | L1 (text only; attachments counted, never fetched) | `gmail-tokenset` | `GOOGLE_OAUTH_ACCESS_TOKEN` |
  | `gmail.drafts.create` | `user_data_write` | L2 (`input_required` once) | `gmail-tokenset` | `GOOGLE_OAUTH_ACCESS_TOKEN` |
  | `gmail.messages.send` | `external_send` | L3 (approval URL) | `gmail-tokenset` | `GOOGLE_OAUTH_ACCESS_TOKEN` |
  | `calendar.events.list` | `read` | L1 | `gcal-tokenset` | `GOOGLE_OAUTH_ACCESS_TOKEN` |
  | `calendar.events.create` | `external_send` | L3 | `gcal-tokenset` | `GOOGLE_OAUTH_ACCESS_TOKEN` |
  | `github.issues.list` | `read` | L1 | `github-tokenset` | `GITHUB_TOKEN` |
  | `github.issues.create` | `external_send` | L3 | `github-tokenset` | `GITHUB_TOKEN` |
  | `linear.issues.create` | `external_send` | L3 | `linear-tokenset` | `LINEAR_OAUTH_TOKEN` |
  | `slack.messages.post` | `external_send` | L3 | `slack-tokenset` | `SLACK_TOKEN` |
  | `notion.search` (E51) | `read` | L1 (pages shared with the integration at sign-in) | `notion-tokenset` | `NOTION_TOKEN` |
  | `notion.pages.text` (E51) | `read` | L1 (text blocks only; images, files and embeds counted, never fetched) | `notion-tokenset` | `NOTION_TOKEN` |

  `test/hardening.test.ts` asserts the per-caller fetch (the service token and the caller's key go to apps/api only, the principal's token to the vendor only, never into a body, a record or a log) and every refusal. `test/connectors.test.ts` asserts the levels, that no L3 run reaches the transport, that the draft asks once, header-injection safety for Gmail and credential injection at fetch time only.
- `monid` (passthrough, `src/passthrough/monid.ts`): endpoints imported from a Monid-style catalog JSON, executed through Monid's `POST /v1/run` with the router's `MONID_API_KEY` injected at the transport, priced at Monid's own price; the caller pays it plus one take, added by the wallet (before E32 the importer baked 5% in, and the wallet would have added 5% again). The fixture catalog holds 12 endpoints from 11 upstream providers (search, scrape, enrichment, SEO, image generation, and one L3 `provision-numbers`).

## Listing pipeline (E21)

`pnpm --filter @shogunagent/router listing:check <provider-dir> | --all` validates a provider directory for listing: `provider.json` (legal name, contact, opaque payout reference, per-endpoint price declarations, data handling) and the catalog compile, offline fixture replay through the connector hooks and through `run` with its permission tier, the scope classification (sends and payments are L3; a write is never `read`), the description lint (tasks named, exclusions, cost class, ≤ 400 characters, no superlatives) and price sanity (declared = usage model = what the fixtures settle). It prints a JSON report and exits 1 on any error; `.github/workflows/router-listing.yml` runs it on PRs touching `connectors/**` and posts the markdown report as the job summary. Details: `connectors/schema.md` §9–§10.

## Pluggable seams (what apps/api and the state layer wire later)

| Interface | Default | Production home |
|---|---|---|
| `MeteringClient` (`identify`, `reserve(identity, quote, { runId, bearer, draft })`, `settle(reservation, charged, settlement)`) | `InMemoryMeteringClient` — `sa_` keys, $1 default balance, 401 for anonymous metered runs, x402-shaped 402; forwards each run's settlement to its `settlements` sink | `ApiWalletClient` (E32, `src/wallet.ts`): apps/api's wallet (`ROUTER_WALLET_URL` + secret `ROUTER_SETTLEMENT_TOKEN`) |
| `SettlementOutbox` (E33, `src/outbox.ts`) | `InMemorySettlementOutbox` (tests) | `SqlSettlementOutbox` on `router_settlement_outbox` (`HYPERDRIVE`), delivered by `SettlementDelivery` inline and from the cron trigger |
| `RunLogSink` | `InMemoryRunLogSink` | `tool_runs` (V94) in the principal's store |
| `PersonalStore` (behind `PersonalRanker`, E20) | `InMemoryPersonalStore` | the principal's `tool_runs` (V94) via the sidecar — never a hosted shared DB |
| `IdempotencyStore` (E48) | `InMemoryIdempotencyStore` (24 h TTL, bounded) | `SqlIdempotencyStore` on `router_run_idempotency` (`HYPERDRIVE`): outcome references only (run id, state, charged usage), never a body (invariant 2) |
| `HealthStore` | `InMemoryHealthStore` (observed after 20 runs) | shared status store (`/.well-known/status.json`) |
| `Transport` | `createFetchTransport({ env, connectors })` (`connectors` from `connectorSourceFor(env)`: `per_caller` / `dev_shared` / `none`) | same; `createReplayTransport` for tests |
| `Logger` | `consoleLogger` (closed field set) | Workers Logs, invocation logs off |

## Develop

```bash
pnpm install
pnpm --filter @shogunagent/router typecheck
pnpm --filter @shogunagent/router test
pnpm --filter @shogunagent/router build:catalog
pnpm --filter @shogunagent/router dev          # wrangler dev on :8787 (no route bound in wrangler.jsonc; workers_dev: false)
```

Secrets are set with `wrangler secret put` and documented in `src/env.ts`; non-secrets are `vars` in `wrangler.jsonc`. Nothing in this package is live until E9 wires the wallet and a route is bound.
