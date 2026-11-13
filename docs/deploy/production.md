# Production runbook: the hosted ShogunAgent service (E41)

How the hosted service is put into production, upgraded, checked and rolled back. **Nothing in this
file has been run against production.** It is the procedure the owner follows after confirming the
first deploy; every step that spends money, binds a hostname or opens a lane is marked as the owner's
call. Values are never written here: secrets live only in each platform's secret store, and the few
public values (the licence public key, Hyperdrive ids) live in the `wrangler.jsonc` files.

Related documents, which this runbook orders and does not repeat in full:

| Document | What it owns |
|---|---|
| `apps/api/DEPLOY.md` | The relay/gateway Worker in depth: database role, Hyperdrive, provider hard limits, speech rollout, agent API, billing, enterprise |
| `apps/router/README.md`, `apps/router/src/env.ts` | The router Worker, its wallet and settlement outbox |
| `deploy/README.md` | The sidecar (`shogun-api --multi`), KMS key providers, regions, Helm, backup and restore |
| `docs/memory-processing.md` | Opt-in sidecar worker, event status, retry/restart semantics and live pilot acceptance |
| `docs/release.md` | Publishing the SDKs and the CLI (not covered here) |
| `docs/shogunai-origin/ai-gateway-launch-checklist.md` | Provider zero-retention and Azure items inherited from ShogunAI |
| `docs/directories/submission-checklist.md` | What the directory listings need from this deployment |

`scripts/check-env-docs.mjs` (CI, `invariant guards` job) fails when `apps/api/src`,
`apps/router/src` or a router connector reads an environment variable that §5 does not name. When it
fails, add the variable to the right table in §5 with its component, kind and whether it is
required.

---

## 1. Components

```
                      agents, MCP clients, the ShogunAI Mac app (HTTPS)
                                         │
                  Cloudflare (WAF rate-limit rule on /v1/, Workers Logs, no invocation logs)
                 ┌───────────────────────┴───────────────────────┐
     Worker `shogunai-relay` (apps/api)              Worker `shogunagent-router` (apps/router)
     auth, metering, 402, wallet, approvals,         discover / inspect / run, scope table,
     OAuth, Batch relay, live lanes, enterprise      catalog, settlement outbox (cron * * * * *)
     cron */15 sweep                                        │  reserve / settle (bearer
        │  HYPERDRIVE (caching off)        ◄────────────────┘  ROUTER_SETTLEMENT_TOKEN)
        │                                                   │  HYPERDRIVE (caching off)
        ▼                                                   ▼
     Postgres: schema `relay`, role `shogun_relay`   Postgres: role of the router (outbox, idempotency)
     (ledger, keys hashed, wallet, receipts, OAuth   (router_settlement_outbox, router_run_idempotency; no body)
      hashes, tenants; never a body)
        │
        │  SHOGUN_SIDECAR_URL + SHOGUN_SIDECAR_ADMIN_TOKEN (Team sync, audit export, purges,
        │  approvals, connector tokens and the connect flow, E50) and, after E34, the
        │  per-principal /mcp relay
        ▼
     Sidecar fleet: `shogun-api --multi` per region (container image from release.yml)
     per-principal SQLCipher stores on volumes; data keys wrapped by the region's KMS key
```

| Component | Code | Runs on | State | Scales by |
|---|---|---|---|---|
| Relay / gateway Worker `shogunai-relay` | `apps/api` (`src/worker.ts`) | Cloudflare Workers Paid | none in the Worker; ledger in Postgres | isolates; every cap reserves against the same Postgres rows |
| Router Worker `shogunagent-router` | `apps/router` (`src/worker.ts`) | Cloudflare Workers Paid | none in the Worker; outbox in Postgres | isolates |
| Postgres | `apps/api/sql/*.sql`, `apps/router/sql/*.sql` | Supabase project on a plan that never pauses | ledgers, hashes, counters; **no bodies** (invariant 2) | vertical; Hyperdrive pools |
| Hyperdrive ×2 | `wrangler.jsonc` `hyperdrive` binding `HYPERDRIVE` | Cloudflare | none | — |
| Sidecar fleet | `crates/` (`shogun-api --multi`) | Kubernetes (Helm chart `deploy/helm/shogunagent`) per region | principal stores (`memory.db` + wrapped key) on PVCs | shards: `fnv1a64(principal_id) mod replicaCount` (`deploy/README.md` §5.2) |
| KMS | AWS KMS, GCP Cloud KMS or Vault Transit | one key per region | wraps each principal's data key | — |
| Providers | OpenAI, Anthropic, Deepgram, TypeSafe, Groq, Stripe, Monid | their consoles | per-project budgets and hard limits (`apps/api/DEPLOY.md` §3) | — |

**Open architecture item (E34 built the seam; owner to choose the path):** the sidecar binds
`127.0.0.1` only and refuses a non-loopback `Host` (`deploy/README.md` §1), and a Worker cannot reach
a pod's loopback. The `/mcp` relay (`apps/api/src/mcp-proxy.ts`) therefore takes its transport as a
seam, and only these are supported:

| Option | Settings | Notes |
|---|---|---|
| A. Node gateway in the sidecar's pod (**recommended for launch**) | in the pod: `SHOGUN_SIDECAR_URL=http://127.0.0.1:7464` + a bearer source; route the public `/mcp` (and only it) to the regional pod's gateway, by principal (consistent hash on the key, or the 421 `wrong_shard` retry with `SHOGUN_SHARD`) | No new network path: the Helm chart and compose file already run the gateway next to the sidecar on loopback. Same code, same Postgres ledger (`RELAY_DATABASE_URL`). The Worker keeps everything else. |
| B. Worker → Workers VPC service / service binding → Cloudflare Tunnel connector in the pod | Worker: `SHOGUN_SIDECAR_BINDING=<binding name>` (+ the binding in `wrangler.jsonc`) + a bearer source; in the pod: `cloudflared` forwarding to `127.0.0.1:7464` with a loopback `Host` | Keeps `/mcp` on the Worker. Needs one tunnel (or one VPC service) per shard pod and the binding provisioned per region; no public origin. |
| C. Worker → private origin URL | `SHOGUN_SIDECAR_URL=<origin>` + a bearer source | Only for an origin reachable solely by the Worker (never a public hostname): the sidecar bearer is the only credential on the path. Not recommended. |

With none configured the hosted `/mcp` answers `503 upstream_unavailable` (`next_action:
retry_after`) before anything is reserved, and the enterprise operations that need the sidecar
answer 503. The bearer the relay presents is per principal (`SHOGUN_SIDECAR_PRINCIPAL_TOKENS`, or
the derived `sgw1` bearer from `SHOGUN_SIDECAR_BEARER_SECRET`, verified by the sidecar given the
same secret); contract in `docs/api/billing.md` "Hosted `/mcp`".

### 1.1 Hostnames (owner decision)

The code and the documents name two hostnames today:

| Where | Hostname |
|---|---|
| `apps/api/wrangler.jsonc` `routes`, `docs/api/402.md` examples, router `vars` | `api.shogunaios.com` (the ShogunAI relay's custom domain) |
| `apps/api/src/wellknown/identity.ts` `HOSTED_MCP_URL`, `server.json`, `.claude-plugin/plugin.json`, `smithery.yaml`, `docs/legal/terms.md`, `docs/directories/*` | `api.shogunagent.dev` |

Pick one public origin for the agent API before launch. If the relay Worker serves both, add the
second custom domain to `routes`, make the WAF rule (§3, step 7) match both hostnames, and set
`PUBLIC_BASE_URL` to the one the agent documents name, so a `402` top-up link, the OAuth issuer and the
server card agree. The router needs a route too (none is bound today): a path on the API host
(`/v1/router/*`, `/v1/catalog*`, `/catalog`) or its own hostname.

---

## 2. Before the first deploy

- Cloudflare: the account that owns the zone(s) in §1.1, on Workers Paid; an API token with Workers
  Scripts edit, Workers Routes edit, Hyperdrive edit and read, Account Settings read.
- Supabase (or another managed Postgres): the project owner's connection string, on a plan that never
  pauses.
- A Kubernetes cluster per region with a storage class for the sidecar PVCs, and one KMS key per region
  with deletion protection on (`deploy/README.md` §6.1, §6.3).
- Provider accounts with **dedicated projects and hard limits set** (`apps/api/DEPLOY.md` §3,
  "Provider-side hard limits — MANDATORY before launch").
- Stripe account (live mode only after the staging run in §6 passes with test keys).
- The container image tag to deploy, built by `.github/workflows/release.yml` (`ghcr.io/<owner>/shogunagent:<tag>`).
- `pnpm install` at the repository root; commands below run from the root.

Shorthand used below:

```sh
API="pnpm --filter @shogun-ai/api exec wrangler"          # runs in apps/api
ROUTER="pnpm --filter @shogunagent/router exec wrangler"  # runs in apps/router
```

---

## 3. First deploy, in order

Each step finishes and is checked before the next starts. The order exists because every later
component refuses to serve without the earlier one: the Workers fail closed (`503
relay_misconfigured` on a missing setting, a sweep that logs the missing table), and the sidecar
refuses a store it cannot unwrap.

### Step 1 — Postgres roles and schemas

1. Relay role and schema exactly as `apps/api/DEPLOY.md` §1 (`shogun_relay`, `NOINHERIT`,
   `statement_timeout = '5s'`, schema `relay`, no grant in `public`).
2. Router role: a second login role that owns only the outbox and the run idempotency table, the same shape (its own schema, e.g.
   `router`, `search_path` set on the role, no grant in `public` or `relay`). The router and the relay
   share nothing but the settlement HTTP route (`apps/router/sql/settlement-outbox-permissions.sql`).
   They may be the same Postgres project or separate ones.

### Step 2 — SQL files, in this order

Every file is provisioning and additive only, and every one is idempotent: re-running any of them
changes nothing (`IF NOT EXISTS`, and guarded `CREATE POLICY` in `live-store-access.sql`). Apply as
the owning role with its schema on the path:

```sh
psql "<relay connection string as shogun_relay>" -v ON_ERROR_STOP=1 -c 'SET search_path = relay' -f <file>
```

| # | File | Role | Needed when | Why this position |
|---|---|---|---|---|
| 1 | `apps/api/sql/shared-store.sql` | `shogun_relay` | always | the relay ledger; `checkSchema` refuses to serve without its six tables |
| 2 | `apps/api/sql/shared-store-permissions.sql` | `shogun_relay` | always | RLS on, nothing for `PUBLIC` / `anon` / `authenticated`; after 1 |
| 3 | `apps/api/sql/asr-store.sql` | `shogun_relay` | before `ASR_ENABLED=1` | speech admission tables |
| 4 | `apps/api/sql/asr-store-permissions.sql` | `shogun_relay` | with 3 | after 3 |
| 5 | `apps/api/sql/asr-provider-usage.sql` | `shogun_relay` | with 3 | provider counter floors; after 3 |
| 6 | `apps/api/sql/agent-keys.sql` | `shogun_relay` | before `AGENT_API_ENABLED=1` | principals, keys (hashed), wallet, receipts, E19 billing tables |
| 7 | `apps/api/sql/agent-keys-permissions.sql` | `shogun_relay` | with 6 | after 6 |
| 8 | `apps/api/sql/agent-receipts-settled-quantity.sql` | `shogun_relay` | with 6, **before** deploying a gateway built with E48 (its `checkSchema` reads the column) | receipts record the settled quantity next to the reserved one; after 6 |
| 9 | `apps/api/sql/agent-oauth.sql` | `shogun_relay` | before `AGENT_OAUTH_SECRET` is set; before 17 | after 6 (references principals) |
| 10 | `apps/api/sql/agent-oauth-permissions.sql` | `shogun_relay` | with 9 | after 9 |
| 11 | `apps/api/sql/approval-webhooks.sql` | `shogun_relay` | before `SHOGUN_APPROVAL_SIGNING_KEY` is set | outbound approval webhooks |
| 12 | `apps/api/sql/approval-webhooks-permissions.sql` | `shogun_relay` | with 11 | RLS on, nothing for `PUBLIC` / `anon` / `authenticated`; after 11 (E48) |
| 13 | `apps/api/sql/provider-payouts.sql` | `shogun_relay` | before `PAYOUTS_STATE_SECRET` is set | provider ledger (E29) |
| 14 | `apps/api/sql/provider-payouts-permissions.sql` | `shogun_relay` | with 13 | after 13 (E48) |
| 15 | `apps/api/sql/x402-payments.sql` | `shogun_relay` | **only** if `X402_ENABLED=1`, which stays off (§9) | x402 replay guard |
| 16 | `apps/api/sql/x402-payments-permissions.sql` | `shogun_relay` | with 15 | after 15 (E48) |
| 17 | `apps/api/sql/enterprise.sql` | `shogun_relay` | before `ENTERPRISE_ENABLED=1` | after 6 and 9 |
| 18 | `apps/api/sql/enterprise-permissions.sql` | `shogun_relay` | with 17 | after 17 |
| 19 | `apps/api/sql/live-store.sql` | `shogun_relay` | before `LIVE_AI_ENABLED=1` | live lane admission (#1101) |
| 20 | `apps/api/sql/live-store-access.sql` | `shogun_relay` | with 19 | after 19; re-runnable since E48 |
| 21–22 | — (`connector-tokens.sql` and its permissions file were removed in E50) | — | never | connector credentials live in each principal's sidecar token store (`docs/api/connector-tokens.md`); a database that already has `relay.connector_tokens` may drop it, nothing reads or writes it |
| 23 | `apps/router/sql/settlement-outbox.sql` | router role | before the router's `ROUTER_WALLET_URL` is set | the durable outbox (E33) |
| 24 | `apps/router/sql/settlement-outbox-permissions.sql` | router role | with 23 | after 23 |
| 25 | `apps/router/sql/run-idempotency.sql` | router role | before the router's `ROUTER_WALLET_URL` is set | cross-isolate run idempotency (E48); without it every metered run is refused `503` (the store fails closed) |
| 26 | `apps/router/sql/run-idempotency-permissions.sql` | router role | with 25 | after 25 |
| 27 | `apps/api/sql/ingest-hooks.sql` | `shogun_relay` | before `INGEST_HOOKS_SECRET` is set | ingest hook registrations (E44, `docs/api/ingest-sources.md`): derived-secret generations, sealed provider secrets, Gmail route HMACs; no bodies |
| 28 | `apps/api/sql/ingest-hooks-permissions.sql` | `shogun_relay` | with 27 | RLS on, nothing for `PUBLIC` / `anon` / `authenticated`; after 27 |

Applying a file for a lane that stays off is harmless; leaving one out for a lane that is switched
on makes that lane's `checkSchema` refuse. Every file may be re-run: `live-store-access.sql` creates
each policy only when it does not exist (a `DO` block over `pg_policy`), and every table has a
permissions companion with RLS on (E48).

Check: `\dt relay.*` as `shogun_relay` lists the tables of the files applied; as `shogun_relay`,
`SELECT 1 FROM public.<any licence table>` fails with `permission denied`; `SELECT relname FROM
pg_class WHERE relkind = 'r' AND relnamespace IN ('relay'::regnamespace, '<router schema>'::regnamespace)
AND NOT relrowsecurity` returns no row.

### Step 3 — Hyperdrive, query caching disabled (both)

```sh
$API hyperdrive create shogunai-relay-store --caching-disabled \
  --connection-string "postgres://shogun_relay:<password>@<host>:5432/postgres?sslmode=require"
$ROUTER hyperdrive create shogunagent-router-outbox --caching-disabled \
  --connection-string "postgres://<router role>:<password>@<host>:5432/postgres?sslmode=require"
```

- Direct host or session pooler, never the transaction pooler (Hyperdrive pools).
- Paste the relay id into `apps/api/wrangler.jsonc` `hyperdrive[0].id`.
- **The router binding.** `apps/router/wrangler.jsonc` has no `hyperdrive` block yet; add, in the
  same PR that binds the router's route:

  ```jsonc
  "hyperdrive": [{ "binding": "HYPERDRIVE", "id": "<router hyperdrive config id>" }]
  ```

  The binding name must be `HYPERDRIVE` (`apps/router/src/env.ts`). With `ROUTER_WALLET_URL` set and
  no binding, every metered run answers `503 wallet_unavailable` (fails closed).
- Verify both: `$API hyperdrive get <id>` shows `caching: { disabled: true }`. A cached Hyperdrive
  would answer the batch-ownership check and the idempotency `SELECT … FOR UPDATE` from a stale copy.

### Step 4 — Sidecar fleet (per region)

`deploy/README.md` §6.1 is the procedure; for the hosted fleet:

1. Create the region's KMS key (deletion protection on) and the workload identity grant (§3.2 there).
2. Create the Kubernetes Secret with the sidecar's secrets (§5.4 below): principal tokens, admin token,
   approval signing key (the same value the Worker gets as `SHOGUN_APPROVAL_SIGNING_KEY`), KMS
   credentials if not IRSA / Workload Identity, and the connect flow's OAuth client secrets
   (`SHOGUN_CONNECT_*`, E50) once the vendor apps exist.
3. `helm install shogunagent deploy/helm/shogunagent -n shogunagent --create-namespace -f <region values>`
   with `region`, `keyProvider.*`, `serviceAccount.annotations`, `secrets.existingSecret`, the image
   tag, and `sharding.acknowledged=true` only if `replicaCount > 1`.
4. Check: readiness probe green (`GET /v1/status` over loopback); `kubectl logs` shows
   `shogun-api: key provider: <type>`; the first request for a test principal logs one `shogun-kms:`
   line and creates `principals/<id>/memory.db` and `<id>.key.wrapped`.
5. Take the first backup and test a restore (`deploy/README.md` §6.3) before any real principal exists.

### Step 5 — Relay Worker `shogunai-relay`, Batch lane only

1. Secrets (`$API secret put <NAME> --config wrangler.jsonc`, value on stdin, never in a shell
   history): `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` (both required), and the optional ones of §5.1 the
   owner has decided to turn on. Leave every lane switch off for the first deploy.
2. `vars` are in `apps/api/wrangler.jsonc` and ship with the deploy (a change is a PR). Check
   `LICENSE_PUBKEY_B64` equals `ed25519` in `packages/config/license-public-key.json` and
   `DAILY_SPEND_CAP_USD` is sized (`apps/api/DEPLOY.md` §3).
3. Verify, then deploy:

   ```sh
   pnpm --filter @shogun-ai/api typecheck
   pnpm --filter @shogun-ai/api test            # Node suite + Workers-runtime suite
   pnpm --filter @shogun-ai/api check:bundle    # no Node-only module in the bundle
   pnpm --filter @shogun-ai/api deploy
   ```

   Or from GitHub with the manual workflow `.github/workflows/deploy.yml` (§3.1). It has never been
   run; until the owner sets up its environments, deploy from the owner's machine as above.
4. Run the smoke checks of §7.1 (relay rows).

### 3.1 The manual deploy workflow (`.github/workflows/deploy.yml`)

The workflow also deploys the website (component `web`, apps/web, Worker `shogunagent-web`). That Worker has no route, so a deploy reaches its workers.dev address only; moving shogunagent.com to it is a separate step in `apps/web/README.md` ("Deploy").

Actions → *deploy* → Run workflow on `main`, with:

| Input | Values | Meaning |
|---|---|---|
| `component` | `relay`, `router`, `web` | `apps/api` (Worker `shogunai-relay`), `apps/router` (Worker `shogunagent-router`) or `apps/web` (Worker `shogunagent-web`, the website) |
| `environment` | `staging`, `production` | the target, and the GitHub environment the deploy job runs in |
| `confirm` | empty (default) or `deploy-<component>-<environment>` | anything but the exact phrase is a dry run: `wrangler deploy --dry-run`, nothing uploaded |

What it does: refuses any ref but `main`; runs the component's typecheck, tests and bundle check
(the router also rebuilds its catalog); then, in the chosen environment (which waits for that
environment's required reviewers), refuses a config that still holds `REPLACE_WITH`, and runs
`wrangler deploy --dry-run` or, with the confirm phrase, `wrangler deploy`, followed by a health
check when `DEPLOY_HEALTH_URL` is set. It never sets Worker secrets (`wrangler secret put` stays a
manual step, §5), never applies SQL (§3 step 2) and never touches Hyperdrive (check
`caching.disabled: true` yourself, step 3). `scripts/check-workflow-yaml.py` (CI) fails if the
workflow gains a trigger other than `workflow_dispatch`, a write permission, a confirm default, a
secret read outside the chosen environment, or an ungated `wrangler deploy`.

One-time setup (owner): create the GitHub environments `staging` and `production`, each with the
owner as a required reviewer and `main` as the only deployment branch, and in each the secrets
`CLOUDFLARE_API_TOKEN` (scoped as in §2) and `CLOUDFLARE_ACCOUNT_ID`, and optionally the variable
`DEPLOY_HEALTH_URL` (e.g. `https://<api host>/healthz`, or the router's `/v1/health`). The
`staging` environment also holds the variable `WRANGLER_STAGING_CONFIG`: the whole staging
`wrangler.jsonc` of §6 for the component being deployed (one component per run; the workflow refuses
a staging config whose `name` is the production Worker's). Production deploys the committed
`wrangler.jsonc`.

### Step 6 — Relay lanes, one at a time

Each lane: apply its SQL (step 2), set its secrets, add its `vars` in a PR, deploy, run its smoke rows
(§7), watch Workers Logs for one cron period.

1. Speech: `DEEPGRAM_API_KEY`, `ASR_ENABLED: "1"` (the shipped `wrangler.jsonc` already has it; it
   answers 503 `speech_unavailable` until the key exists). Gates: `apps/api/DEPLOY.md` "Gates before
   public activation".
2. Form decisions: `TYPESAFE_API_KEY`.
3. Agent API: `AGENT_POW_SECRET`, `AGENT_API_ENABLED: "1"`, `PUBLIC_BASE_URL`; then
   `AGENT_OAUTH_SECRET` (OAuth), `STRIPE_SECRET_KEY` + `STRIPE_WEBHOOK_SECRET` (card top-ups; webhook
   `https://<api host>/v1/stripe/webhook`, events `checkout.session.completed`,
   `checkout.session.expired`, `charge.refunded`), `AGENT_CARD_SIGNING_KEY`. Insert the operator's
   `admin`-scope key once by hand (`apps/api/DEPLOY.md` §9.1 step 4).
4. Approvals: `SHOGUN_APPROVAL_SIGNING_KEY`, `WEBHOOK_SIGNING_SECRET`, `SHOGUN_SIDECAR_URL`; Slack
   trio only if wanted. Needs the open item in §1.
5. Router wallet and payouts: `ROUTER_SETTLEMENT_TOKEN` (same value as the router's),
   `PAYOUTS_STATE_SECRET`. Connector credentials (E50, once the vendor apps of §9 exist):
   `SHOGUN_SIDECAR_ADMIN_TOKEN` with the `/mcp` relay settings, then `CONNECTOR_CONNECT_SECRET`.
6. Enterprise: `ENTERPRISE_SECRET`, `ENTERPRISE_ENABLED: "1"`, `SHOGUN_REGION`,
   `SHOGUN_REGION_ENDPOINTS`, `SHOGUN_SIDECAR_ADMIN_TOKEN`.
7. Live lanes (#1101): `OPENAI_LIVE_API_KEY`, `GROQ_LIVE_API_KEY`, `LIVE_HMAC_KEY`,
   `LIVE_LANE_CONFIG_JSON`, `LIVE_GLOBAL_DAILY_CAP_USD`, then `LIVE_AI_ENABLED: "1"`. Zero-retention
   gates of §9 first.

### Step 7 — Cloudflare settings after the first relay deploy

- WAF rate-limiting rule on every API hostname, path starts with `/v1/`, e.g. 60 requests per minute
  per IP, block 1 minute; `/healthz` outside it (`apps/api/DEPLOY.md` §4).
- Workers Logs retention at the shortest the plan allows, for both Workers; invocation logs off in the
  dashboard too.

### Step 8 — Router Worker `shogunagent-router`

1. Secrets: `ROUTER_SETTLEMENT_TOKEN` (≥ 32 characters, identical to the relay's), `MONID_API_KEY`
   (only if Monid passthrough endpoints are listed).
2. `vars` in `apps/router/wrangler.jsonc`: `SHOGUNAGENT_API_BASE_URL`, `APPROVAL_BASE_URL`,
   `ROUTER_WALLET_URL` (the relay's public origin). Leave `ROUTER_TOPUP_URL` unset in production (it
   is used by the in-memory development wallet only).
3. The `hyperdrive` block of step 3 and a `routes` entry (§1.1), in one PR.
4. **Do not set** `GOOGLE_OAUTH_ACCESS_TOKEN`, `GITHUB_TOKEN`, `LINEAR_OAUTH_TOKEN`, `SLACK_TOKEN`,
   `NOTION_TOKEN` or `ROUTER_DEV_SHARED_CREDENTIALS`. With `ROUTER_WALLET_URL` set the router never reads those
   variables: the first-party connectors (`caller_connector` auth, E48) fetch the calling
   principal's own token per run from the relay's `POST /v1/internal/router/connector-token`
   (`docs/api/connector-tokens.md`), and the development flag is ignored (logged
   `dev_shared_credentials_ignored`). The relay serves that route (E48, with
   `ROUTER_SETTLEMENT_TOKEN`) from the principal's sidecar (E50): it needs the `/mcp` relay's
   transport and bearer source and `SHOGUN_SIDECAR_ADMIN_TOKEN`; without them every caller gets
   `connector_not_connected` and the relay logs `connector_vault_unconfigured`. A human connects an
   account through the hosted connect flow (`CONNECTOR_CONNECT_SECRET` on the relay, the vendor
   clients on the sidecar). Keep those tools out of the hosted catalog until §9's item is done.
5. After the first request, `wrangler tail` shows the boot words (fixed, one per decision):
   `wallet_wired_with_outbox`, `idempotency_postgres`, `connector_credentials_per_caller`. Any of
   `wallet_outbox_missing`, `idempotency_in_memory`, `wallet_in_memory_dev` or
   `dev_shared_credentials_ignored` in production means the `hyperdrive` block or a variable is
   wrong: fix it before routing traffic.
6. Verify and deploy:

   ```sh
   pnpm --filter @shogunagent/router typecheck
   pnpm --filter @shogunagent/router test
   pnpm --filter @shogunagent/router build:catalog
   pnpm --filter @shogunagent/router check:bundle
   $ROUTER deploy --config wrangler.jsonc
   ```

### Step 9 — Smoke, then the conformance suite (§7)

---

## 4. Upgrades

Order for every release that touches more than one component:

1. **SQL** — re-apply every file the release changed, in the §3 step 2 order. Files are idempotent
   and additive, so old code keeps working against the new schema; new code's `checkSchema` refuses
   to serve against the old one. Never edit an applied file in place in a way that drops or renames
   (the same rule as invariant 8).
2. **Sidecars** — back up (`deploy/README.md` §6.3), `helm upgrade` one replica at a time, watch for
   `memory_unavailable` in gateway responses. The sidecar migrates each store on its first open and
   refuses a store newer than itself, so this step cannot be undone without the backup.
3. **Relay Worker** — `typecheck`, `test`, `check:bundle`, `deploy` (§3 step 5). It goes before any
   device build that changes a Batch contract (`apps/api/DEPLOY.md` §5) and before the router when
   the settlement format changes (the relay accepts the old and new formats; the router sends one).
4. **Router Worker** — after the relay it settles against.
5. Smoke rows of §7 for each component that changed.

Secrets and Hyperdrive configs are not part of a Worker version; change them separately and expect
the next isolate to pick them up. A `vars` change is a deploy.

---

## 5. Environment variables and secrets

Names only; never values. "Kind": **secret** = `wrangler secret put` (Workers) or a Kubernetes
Secret (sidecar); **var** = `vars` in `wrangler.jsonc` (in git) unless noted "dashboard";
**binding** = a Worker binding. "Required": **yes** = boot refuses without it; **lane** = required
once the named lane switch is on; **no** = optional, with the default in code.

### 5.1 Relay / gateway Worker (`apps/api`, documented in `src/boot.ts`)

Core (Batch relay):

| Name | Kind | Required | Notes |
|---|---|---|---|
| `ANTHROPIC_API_KEY` | secret | yes | operator's Anthropic key; `BATCH_PROVIDER=anthropic` and the brief fallback |
| `OPENAI_API_KEY` | secret | yes | operator's OpenAI key; background classes on `gpt-6-luna` and `/v1/review` |
| `LICENSE_PUBKEY_B64` | var | yes | public; must equal `ed25519` in `packages/config/license-public-key.json` |
| `LICENSE_PUBKEY_NEXT_B64` | var | during a key rotation | must match `ed25519_next`; unset otherwise |
| `DAILY_SPEND_CAP_USD` | var | yes | global daily breaker; `0` is the emergency stop |
| `RELAY_ALLOW_NO_SPEND_CAP` | env (development only) | no | `1` lets a dev relay boot without the breaker; never in production or staging |
| `BATCH_PROVIDER` | var | no | `openai` (default) or `anthropic` |
| `LICENSE_DAILY_SPEND_CAP_USD` | var | no | default 0.50 |
| `TRIAL_DAILY_SPEND_CAP_USD` | var | no | default 0.25 |
| `DAILY_CHUNK_CAP` | var | no | default 2000 |
| `TRIAL_DAILY_CHUNK_CAP` | var | no | default 500 |
| `TRIAL_SPEND_SHARE` | var | no | default 0.5 |
| `RATE_BURST` | var | no | default 12 |
| `RATE_PER_MIN` | var | no | default 12 |
| `SETTLE_INTERVAL_SECONDS` | var | no | default 900; `0` stops the sweep |
| `SETTLE_MAX_PER_TICK` | var | no | default 500 |
| `SETTLE_RECHECK_SECONDS` | var | no | default 900 |
| `SPEND_PRICE_TABLE_JSON` | var | no | price rows merged over the shipped table |
| `METRICS_TOKEN` | secret | no | bearer for `GET /metrics`; unset = 404 |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | var | no | opt-in OTLP span export (E37); https, or http to a loopback collector; unset = off |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | var | no | overrides the traces URL derived from the endpoint |
| `OTEL_EXPORTER_OTLP_HEADERS` | secret | no | `k=v,k2=v2` sent to the collector only |
| `OTEL_SERVICE_NAME` | var | no | default `shogunagent-gateway` |
| `OTEL_TRACES_SAMPLER_ARG` | var | no | span sample ratio 0–1, default 1 |
| `RELAY_DENY_SUBJECTS` | var (dashboard) | no | emergency deny list; kept across deploys by `keep_vars` |
| `HYPERDRIVE` | binding | yes | the relay's Postgres; caching disabled |

Speech and form decisions:

| Name | Kind | Required | Notes |
|---|---|---|---|
| `ASR_ENABLED` | var | no | `1` serves `/v1/asr/*` |
| `DEEPGRAM_API_KEY` | secret | lane (speech) | Member key of a dedicated, prepaid project; unset = 503 `speech_unavailable` |
| `ASR_ALLOWED_SUBJECTS` | var (dashboard) | no | narrows company speech; unset = every paid licence and trial |
| `TYPESAFE_API_KEY` | secret | no | Jev form decisions; unset = `/v1/ai/form_decide` 503 |

Agent API (`AGENT_API_ENABLED=1`):

| Name | Kind | Required | Notes |
|---|---|---|---|
| `AGENT_API_ENABLED` | var | no | `1` serves keys, wallet, 402, `/mcp`, well-known documents |
| `AGENT_POW_SECRET` | secret | lane | ≥ 16 characters |
| `PUBLIC_BASE_URL` | var | lane | the public origin (§1.1) |
| `AGENT_POW_BITS` | var | no | default 18 |
| `AGENT_REGISTER_PER_HOUR` | var | no | default 5 |
| `AGENT_KEY_PBKDF2_ITERATIONS` | var | no | default 20000 |
| `AGENT_KEY_CAP_DAY_USD` | var | no | default 1 |
| `AGENT_KEY_CAP_MONTH_USD` | var | no | default 20 |
| `AGENT_PRINCIPAL_CAP_DAY_USD` | var | no | default 5 |
| `AGENT_PRINCIPAL_CAP_MONTH_USD` | var | no | default 100 |
| `STRIPE_SECRET_KEY` | secret | no (pair) | with `STRIPE_WEBHOOK_SECRET`, both or neither; card top-ups and Connect |
| `STRIPE_WEBHOOK_SECRET` | secret | no (pair) | see above |
| `STRIPE_COMMIT_OVERAGE_PRICE_ID` | var | no | needs `STRIPE_SECRET_KEY` |
| `AGENT_CARD_SIGNING_KEY` | secret | no | Ed25519 key for the A2A card; unset = unsigned card |
| `AGENT_MCP_URL` | var | no | when the MCP endpoint is not `<PUBLIC_BASE_URL>/mcp` |
| `AGENT_OAUTH_SECRET` | secret | no | ≥ 32 characters; turns OAuth on (needs `agent-oauth.sql`) |
| `ROUTER_SETTLEMENT_TOKEN` | secret | no | ≥ 32 characters; serves the router's reserve/settle routes; same value as the router's |
| `PAYOUTS_STATE_SECRET` | secret | no | ≥ 32 characters; provider payouts (needs `provider-payouts.sql`) |
| `PAYOUT_MIN_USD` | var | no | default 10 |
| `SHOGUN_APPROVAL_SIGNING_KEY` | secret | no | ≥ 32 characters, same value as the sidecar's; turns remote approvals on |
| `SHOGUN_SIDECAR_URL` | var | with approvals, enterprise sidecar ops or the `/mcp` relay | the sidecar's base URL (open item, §1); with `SHOGUN_SIDECAR_BINDING` only a label |
| `SHOGUN_SIDECAR_BINDING` | var | no | E34: name of a Workers service / VPC binding the `/mcp` relay reaches the sidecar through (option B, §1); named but unbound = `/mcp` stays 503 |
| `SHOGUN_SIDECAR_PRINCIPAL_TOKENS` | secret | one of the two bearer sources turns the `/mcp` relay on | E34: the sidecar's `SHOGUN_PRINCIPAL_TOKENS` (`principal:token[:agent_id],…`); presented instead of the agent's key |
| `SHOGUN_SIDECAR_BEARER_SECRET` | secret | one of the two bearer sources turns the `/mcp` relay on | E34: ≥ 32 characters, same value as the sidecar's; derives `sgw1.<principal>.<key>.<mac>` |
| `SHOGUN_SHARD` | var | no | E34: `<index>/<count>` of this gateway's sidecar replica; other principals get `421 wrong_shard` (`deploy/README.md` §5.2) |
| `INGEST_HOOKS_SECRET` | secret | no | E44: ≥ 32 characters; turns the provider ingest hooks on (`/v1/ingest-hooks`, needs `ingest-hooks.sql` and the `/mcp` relay). Derives GitHub secrets and Calendar channel tokens, seals Linear / Slack / Notion secrets, keys the Gmail mailbox routes. Changing it invalidates every hook's secret: rotate each hook after |
| `INGEST_HOOKS_GMAIL_AUDIENCE` | var | no (pair) | E44: the OIDC audience set on the Gmail Pub/Sub push subscription (an https URL, e.g. `<PUBLIC_BASE_URL>/v1/ingest-hooks/gmail`); with `INGEST_HOOKS_GMAIL_SERVICE_ACCOUNT`, both or neither |
| `INGEST_HOOKS_GMAIL_SERVICE_ACCOUNT` | var | no (pair) | E44: the service account the push subscription signs as; the gateway fetches Google's OIDC certs (`www.googleapis.com/oauth2/v3/certs`) to verify it |
| `WEBHOOK_SIGNING_SECRET` | secret | with approvals | ≥ 32 characters |
| `SLACK_BOT_TOKEN` | secret | no (trio) | the three Slack settings together or none |
| `SLACK_SIGNING_SECRET` | secret | no (trio) | |
| `SLACK_APPROVAL_CHANNEL` | var | no (trio) | |
| `X402_ENABLED` | var | no | **stays unset in production** (decision 0002) |
| `X402_NETWORK` | var | with x402 | |
| `X402_PAY_TO` | var | with x402 | |
| `X402_ASSET` | var | with x402 | |
| `X402_FACILITATOR_URL` | var | with x402 | |
| `X402_MAX_TIMEOUT_SECONDS` | var | no | default 60 |

Enterprise (`ENTERPRISE_ENABLED=1`, agent API on):

| Name | Kind | Required | Notes |
|---|---|---|---|
| `ENTERPRISE_ENABLED` | var | no | |
| `ENTERPRISE_SECRET` | secret | lane | ≥ 32 characters; rotating it ends admin sessions and unseals no stored OIDC secret |
| `SHOGUN_REGION` | var | no | default `us`; one region per deployment |
| `SHOGUN_REGION_ENDPOINTS` | var | no | JSON of the other regions' origins, only named in `421` answers |
| `SCIM_DELETE_GRACE_DAYS` | var | no | default 30 |
| `SSO_CLOCK_SKEW_SECS` | var | no | default 120 |
| `SHOGUN_SIDECAR_ADMIN_TOKEN` | secret | no | the sidecar's `SHOGUN_ADMIN_TOKEN`; Team sync, audit export, purges, and (E50, with the `/mcp` relay settings) the router's connector tokens from the principal's sidecar |
| `CONNECTOR_CONNECT_SECRET` | secret | no | E50: ≥ 32 characters; seals the hosted connect flow's links and OAuth states. Set, with the sidecar vault, = `POST /v1/connectors/{provider}/connect`, the human's page and `/v1/connectors/callback` are served (the sidecar's `SHOGUN_CONNECT_REDIRECT_URI` names that callback). Rotating it voids links and flows in progress only |

Live lanes (#1101; read by `src/worker.ts` and `src/live-config.ts` only when `LIVE_AI_ENABLED=1`):

| Name | Kind | Required | Notes |
|---|---|---|---|
| `LIVE_AI_ENABLED` | var | no | `1` serves `/v1/ai/:lane` (except `form_decide`) |
| `OPENAI_LIVE_API_KEY` | secret | lane | a separate OpenAI project from the Batch key |
| `GROQ_LIVE_API_KEY` | secret | lane | dictation cleanup |
| `LIVE_HMAC_KEY` | secret | lane | ≥ 32 characters |
| `LIVE_LANE_CONFIG_JSON` | var | lane | per-lane model, prices, output caps; validated at request time |
| `LIVE_GLOBAL_DAILY_CAP_USD` | var | lane | the live lanes' own daily breaker |

Node entry only (`src/index.ts`: development, BYOC compose / Helm gateway container; never read by the Worker):

| Name | Kind | Required | Notes |
|---|---|---|---|
| `PORT` | env | no | default 8787 |
| `HOST` | env | no | bind address |
| `METRICS_LOG_INTERVAL_SEC` | env | no | default 60 |
| `RELAY_DATABASE_URL` | secret env | in BYOC | Postgres URL; unset = in-memory store (development) |

### 5.2 Router Worker (`apps/router`, documented in `src/env.ts`)

| Name | Kind | Required | Notes |
|---|---|---|---|
| `ROUTER_SETTLEMENT_TOKEN` | secret | with `ROUTER_WALLET_URL` | same value as the relay's |
| `MONID_API_KEY` | secret | no | Monid passthrough; unset = those tools answer `provider_unavailable` |
| `SHOGUNAGENT_API_BASE_URL` | var | no | default in `src/env.ts`; the first-party connector's base URL |
| `APPROVAL_BASE_URL` | var | no | where L3 approval URLs point |
| `ROUTER_WALLET_URL` | var | yes in production | the relay's origin; unset = in-memory development wallet |
| `ROUTER_TOPUP_URL` | var | no | development wallet only; leave unset in production |
| `HYPERDRIVE` | binding | with `ROUTER_WALLET_URL` | the outbox's and the run idempotency store's Postgres (E33, E48); caching disabled |
| `ROUTER_DEV_SHARED_CREDENTIALS` | var | **never in production** | development only: `1` lets the first-party connectors read one shared token from the variables below. Ignored whenever `ROUTER_WALLET_URL` is set (logged `dev_shared_credentials_ignored`) |
| `ROUTER_METRICS_TOKEN` | secret | no | bearer for `GET /metrics` (E37, Prometheus text); unset = 404 |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | var | no | opt-in OTLP span export (E37); https, or http to a loopback collector; unset = off |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | var | no | overrides the traces URL derived from the endpoint |
| `OTEL_EXPORTER_OTLP_HEADERS` | secret | no | `k=v,k2=v2` sent to the collector only |
| `OTEL_SERVICE_NAME` | var | no | default `shogunagent-router` |
| `OTEL_TRACES_SAMPLER_ARG` | var | no | span sample ratio 0–1, default 1 |

First-party connector credentials (named by `credential_env` in `apps/router/connectors/*/provider.ts`;
read **only in development** with `ROUTER_DEV_SHARED_CREDENTIALS=1` and no `ROUTER_WALLET_URL`;
**never set in production**, see §3 step 8). In production each run fetches the calling principal's
token for the slot below from the relay (`docs/api/connector-tokens.md`):

| Name (development only) | Connectors | Production source (slot in the principal's sidecar token store) |
|---|---|---|
| `GOOGLE_OAUTH_ACCESS_TOKEN` | `gmail`, `calendar` | `gmail-tokenset`, `gcal-tokenset` |
| `GITHUB_TOKEN` | `github` | `github-tokenset` |
| `LINEAR_OAUTH_TOKEN` | `linear` | `linear-tokenset` |
| `SLACK_TOKEN` | `slack` | `slack-tokenset` |
| `NOTION_TOKEN` | `notion` | `notion-tokenset` |

### 5.3 Deploy tooling and the conformance suite (not read by the Workers)

| Name | Where | Notes |
|---|---|---|
| `CLOUDFLARE_API_TOKEN` | operator shell, or a secret of the GitHub environments `staging` / `production` (§3.1) | scoped as in §2 |
| `CLOUDFLARE_ACCOUNT_ID` | same | |
| `WRANGLER_STAGING_CONFIG` | variable of the GitHub environment `staging` (§3.1) | the staging `wrangler.jsonc` of §6 for the component deployed; never the production `name` |
| `DEPLOY_HEALTH_URL` | variable of each GitHub environment (§3.1), optional | checked after a real deploy |
| `SHOGUN_CONFORMANCE_URL` | `packages/conformance` | the deployed `/mcp` |
| `SHOGUN_CONFORMANCE_TOKEN` | same | the demo principal's `sa_live_` key; never committed |
| `SHOGUN_CONFORMANCE_WRITES` | same | `1` only against a store that may hold test rows |
| `SHOGUN_CONFORMANCE_APPROVAL_URL` | same | the approval URL prefix to expect |
| `SHOGUN_CONFORMANCE_STRICT` | same | `1` to fail on warnings |

### 5.4 Sidecar (`shogun-api --multi`, details in `deploy/README.md` §3.2)

`deploy/README.md` is the source. `check-env-docs.mjs` covers the `SHOGUN_CONNECT_*` names the
sidecar's connect flow reads (the `*_VAR` constants of `crates/shogun-integrations/src/hosted_connect.rs`,
E51); the rest of the sidecar's variables are checked by hand against `deploy/README.md`.

| Name | Kind | Required | Notes |
|---|---|---|---|
| `SHOGUN_KEY_PROVIDER` | env | yes in production | `aws-kms`, `gcp-kms` or `vault`; never `env` in production |
| `SHOGUN_MULTI_ROOT` | env | yes | principals directory on the PVC |
| `SHOGUN_MULTI_MAX_OPEN` | env | no | default 64 |
| `SHOGUN_KMS_KEY_CACHE_SECS` | env | no | default 300 |
| `SHOGUN_KMS_TIMEOUT_SECS` | env | no | default 10 |
| `SHOGUN_SIDECAR_BEARER_SECRET` | secret | yes (recommended) | E34: verifies the gateway's derived `sgw1` bearers; same value as the Worker's. Replaces `SHOGUN_PRINCIPAL_TOKENS` (threat model R4) |
| `SHOGUN_PRINCIPAL_TOKENS` | secret | no with `SHOGUN_SIDECAR_BEARER_SECRET` | `principal:token[:agent_id],…`; a listed token wins |
| `SHOGUN_ADMIN_TOKEN` | secret | yes | operator bearer; the Worker's `SHOGUN_SIDECAR_ADMIN_TOKEN` |
| `SHOGUN_APPROVAL_SIGNING_KEY` | secret | with approvals | same value as the Worker's |
| `SHOGUN_APPROVAL_BASE_URL` | env | with approvals | the public approval page |
| `SHOGUN_API_PORT` | env | no | loopback port, default 7464 |
| `SHOGUN_TOKEN_STORE_DIR` | env | no | default `<root>/.tokens`; holds the connector grants (E50): keep it on the PVC |
| `SHOGUN_CONNECT_REDIRECT_URI` | env | with the connect flow | E50: `https://<api host>/v1/connectors/callback`, registered with each vendor |
| `SHOGUN_CONNECT_GOOGLE_CLIENT_ID`, `SHOGUN_CONNECT_GOOGLE_CLIENT_SECRET` | env / secret | with Gmail / Calendar | E50: a Google "Web application" client; the secret is required |
| `SHOGUN_CONNECT_SLACK_CLIENT_ID`, `SHOGUN_CONNECT_SLACK_CLIENT_SECRET` | env / secret | with Slack | E50: user scopes; the secret is optional for a PKCE-enabled app |
| `SHOGUN_CONNECT_LINEAR_CLIENT_ID`, `SHOGUN_CONNECT_LINEAR_CLIENT_SECRET` | env / secret | with Linear | E50: the secret is optional |
| `SHOGUN_CONNECT_GITHUB_CLIENT_ID`, `SHOGUN_CONNECT_GITHUB_CLIENT_SECRET` | env / secret | with GitHub | E50 follow-up: a GitHub App with the device flow enabled (no callback); the secret is optional, sent on refresh when set, and needed to revoke a grant at disconnect (E51: `DELETE /applications/{client_id}/grant` authenticates as the app; without it `vendor_revoke: "not_supported"`). Also read by `shogun-api --dream-tick` (with the other `SHOGUN_CONNECT_*`) to refresh grants for the read-sync |
| `SHOGUN_CONNECT_NOTION_CLIENT_ID`, `SHOGUN_CONNECT_NOTION_CLIENT_SECRET` | env / secret | with Notion | E51: a Notion **public** integration (OAuth) with read-content capability only and the callback above as its redirect URI; the secret is required (Notion's token endpoint takes `client_id:client_secret` as Basic auth). The grant lands in `notion-tokenset`, read by the router's `notion.*` tools and the read-sync |
| `SHOGUN_REGION` | env | yes | set by the chart from `region` |
| `SHOGUN_AWS_KMS_KEY_ID`, `SHOGUN_AWS_KMS_REGION`, `SHOGUN_AWS_KMS_ENDPOINT`, `SHOGUN_AWS_STS_ENDPOINT`, `AWS_ROLE_ARN`, `AWS_WEB_IDENTITY_TOKEN_FILE` | env | with `aws-kms` | IRSA preferred over static `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` |
| `SHOGUN_GCP_KMS_KEY`, `SHOGUN_GCP_KMS_ENDPOINT`, `SHOGUN_GCP_ACCESS_TOKEN_FILE` | env | with `gcp-kms` | Workload Identity preferred |
| `VAULT_ADDR`, `SHOGUN_VAULT_TRANSIT_KEY`, `SHOGUN_VAULT_TRANSIT_MOUNT`, `SHOGUN_VAULT_K8S_ROLE`, `VAULT_TOKEN_FILE` | env / secret | with `vault` | Kubernetes auth preferred |
| `SHOGUN_DB_KEY` | secret | **never in production** | evaluation-only key provider `env` |

---

## 6. Staging

The two `wrangler.jsonc` files have no `env` blocks, so staging is a separate Worker deployed from a
staging copy of each config rather than an `--env staging` flag. Keep the copies out of git until the
owner decides to add `env.staging` blocks (then the copies below become those blocks, and
`check-env-docs.mjs` needs no change).

What differs from production, and nothing else:

| Field | Relay (`apps/api`) | Router (`apps/router`) |
|---|---|---|
| `name` | `shogunai-relay-staging` | `shogunagent-router-staging` |
| `routes` | a staging hostname (e.g. `staging-api.<zone>`), never the production one | a staging hostname or path on it |
| `hyperdrive[0].id` | a staging Hyperdrive on a **separate** database or schema and role | a staging outbox Hyperdrive |
| `vars` | `DAILY_SPEND_CAP_USD` small (e.g. a few dollars), `PUBLIC_BASE_URL` = the staging origin, every lane switch as being tested | `ROUTER_WALLET_URL` = the staging relay origin |
| secrets | provider keys from **staging projects** with their own small hard limits; Stripe **test** keys; fresh random values for every token and HMAC secret (never production's) | its own `ROUTER_SETTLEMENT_TOKEN` matching the staging relay |

Commands (copy, edit the fields above, deploy against the copy):

```sh
cp apps/api/wrangler.jsonc apps/api/wrangler.staging.jsonc
cp apps/router/wrangler.jsonc apps/router/wrangler.staging.jsonc
# edit name / routes / hyperdrive / vars as in the table; do not commit these two files
$API hyperdrive create shogunai-relay-store-staging --caching-disabled --connection-string "<staging relay role URL>"
$ROUTER hyperdrive create shogunagent-router-outbox-staging --caching-disabled --connection-string "<staging router role URL>"
$API secret put OPENAI_API_KEY --config wrangler.staging.jsonc       # and every other secret, staging values
$API deploy --config wrangler.staging.jsonc
$ROUTER secret put ROUTER_SETTLEMENT_TOKEN --config wrangler.staging.jsonc
$ROUTER deploy --config wrangler.staging.jsonc
```

Apply the §3 step 2 SQL files to the staging database first. Staging sidecars are a separate Helm
release with their own namespace, PVCs and KMS key (`region` labelled e.g. `staging`). Run every §7
check against staging, writes included, before the same release goes to production.

`wrangler secret put` on a staging config can never touch production because the `name` differs;
re-read the `name` line before every staging command anyway.

---

## 7. Smoke checks after a deploy

### 7.1 Relay / gateway

| Check | Request | Expect |
|---|---|---|
| Boot | `GET https://<api host>/healthz` | `200 {"ok":true}`. `503 {"error":"relay_misconfigured"}` = a setting failed; Workers Logs names it |
| Store | `wrangler tail` over the next quarter hour | `settlement_sweep {"checked":…}`; no `shared store table missing` |
| Batch probe | `POST /v1/batch` with a test licence token (`apps/api/DEPLOY.md` §6 step 3) | `202` + `batch_id`; `GET /v1/batch/<id>` by another test licence = `404` |
| Rate limit | the probe past `RATE_BURST` in a minute | `429` with `Retry-After` |
| Speech (lane on) | `POST /v1/asr/grant` with a test token | `200` with a 60 s grant; `503 speech_unavailable` when the key is unset |
| Agent API (lane on) | `GET /.well-known/mcp/server-card.json`, `GET /openapi.json`, `GET /llms.txt` | `200`; tool names and annotations match `apps/api/fixtures/tool-table.json` |
| Legal pages | `GET /privacy`, `GET /terms` | `200` with the DRAFT banner until counsel clears them |
| OAuth (on) | `POST /mcp` without a bearer | `401` with `WWW-Authenticate` naming `/.well-known/oauth-protected-resource/mcp`; that URL answers `200` |
| Payment | a metered call on an empty wallet | `402 payment_required`, `next_action: "top_up"`, `accepts` holds the `card` rail only (no `x402`) |
| `/mcp` | `POST /mcp` `tools/list` with a demo key | `200` once E34 is live; `503 upstream_unavailable` before |
| Metrics (token set) | `GET /metrics` with the bearer | `200`; without it `404` |
| Logs | `wrangler tail` | only route templates, statuses, counters; a body, token or id in a line is a stop-ship regression |

### 7.2 Router

| Check | Request | Expect |
|---|---|---|
| Health | `GET https://<router host>/health` | `200 {"ok":true,"catalog_hash":…}`; the hash equals `dist/catalog.json`'s from `build:catalog` |
| Catalog | `GET /v1/catalog`, `GET /catalog` | `200`; first-party entries carry `first_party: true`; no legal name, contact or payout reference in the body |
| Discover | `POST /v1/router/discover` `{"task":"what did the user commit to"}` | `200` with ranked candidates and `scoreBreakdown` |
| Run without key | `POST /v1/router/run` on a metered tool | `401 authentication_required` |
| Run with the demo key | a $0 first-party read | `200`; then a metered tool on an empty wallet: `402 payment_required` |
| Outbox | `wrangler tail` for two minutes | the cron runs; no `dead` rows (`SELECT status, count(*) FROM router_settlement_outbox GROUP BY 1`) |
| Boot wiring | `wrangler tail` during the first request | `wallet_wired_with_outbox`, `idempotency_postgres`, `connector_credentials_per_caller`; none of the words in §3 step 8.5 |
| Idempotency | the same metered run twice with one `idempotency_key` | first `200 result`, then `200 replay` with the same `run_id` and one charge on the wallet |

### 7.3 Sidecar

`kubectl get pods` ready; `kubectl logs` has `key provider: <type>` and no `memory_unavailable`;
a test principal round-trips one ingest and one recall through the gateway once the path in §1 exists.

### 7.4 The conformance suite against the deployed URL

Against staging with writes, and against production read-only, with the demo principal's key
(`docs/directories/anthropic.md`, "Demo account procedure"):

```sh
SHOGUN_CONFORMANCE_URL=https://<staging api host>/mcp SHOGUN_CONFORMANCE_TOKEN=<demo sa_live_ key> \
SHOGUN_CONFORMANCE_WRITES=1 SHOGUN_CONFORMANCE_APPROVAL_URL=https://<staging api host>/approve/ \
  pnpm --filter @shogunagent/conformance conformance -- --json

SHOGUN_CONFORMANCE_URL=https://<api host>/mcp SHOGUN_CONFORMANCE_TOKEN=<demo sa_live_ key> \
  pnpm --filter @shogunagent/conformance conformance
```

Pass = no `fail`. Warnings are reviewed and either fixed or recorded with the release. Run it only
once `/mcp` stops answering 503 (E34).

---

## 8. Rollback

| Component | How | Notes |
|---|---|---|
| Relay Worker | `$API versions list --config wrangler.jsonc`, then `$API rollback <version-id> --config wrangler.jsonc` | instant, code and `vars` only; secrets and Hyperdrive are not versioned. If money is being spent wrongly, first set `DAILY_SPEND_CAP_USD` to `0` (commit and deploy, or dashboard + commit), then diagnose |
| Relay lane | unset its switch (`ASR_ENABLED`, `AGENT_API_ENABLED`, `ENTERPRISE_ENABLED`, `LIVE_AI_ENABLED`) or delete its secret, deploy | the lane answers 503 / 404; its tables stay |
| One subject | add it to `RELAY_DENY_SUBJECTS` in the dashboard | every route answers 401 from the next request |
| Speech, at once | `ASR_ENABLED: "0"`, or the global block row (`apps/api/DEPLOY.md` "Non-speech usage") | |
| Router Worker | `$ROUTER versions list` / `$ROUTER rollback <version-id>` | the outbox keeps undelivered settlements; the cron of the restored version delivers them |
| Router wallet | unset `ROUTER_WALLET_URL` → in-memory wallet: **do not** in production (runs would be unbilled); instead roll the version back | |
| Postgres | **never rolled back.** Files are additive; a dropped row authorises a duplicate paid submission. Undo a permissions file only with its documented `DISABLE ROW LEVEL SECURITY` | rollback notes: `docs/migrations/relay-asr-rollback.md` for speech |
| Hyperdrive | recreate with `--caching-disabled` and repoint the id; never turn caching on | |
| Sidecar | `helm rollback` only before any store was opened by the new version; after that, restore the pre-upgrade backup (`deploy/README.md` §6.5) | a newer store is refused by an older binary (`503 memory_unavailable`), never corrupted |
| KMS | never delete or disable a key that wrapped a stored file | |

---

## 9. Pre-launch checklist and open owner decisions

Nothing below is done. Each line is checked off by the owner with a date and, where it is evidence,
a link to the record.

Owner decisions still open:

- [ ] **Legal entity and contact in first-party manifests.** `apps/router/connectors/*/provider.json`
      say `legal_name: "Select, Inc."` and `contact.url: https://shogunai.si`; `.claude-plugin/*.json`,
      `docs/legal/*` and `docs/directories/*` say **Select KK** and `https://shogunagent.dev`. Pick
      the registered entity, its address and company number (`[TBD]` in `docs/legal/privacy-policy.md`
      and `dpa.md`) and one contact URL, and make all of them agree.
- [ ] **Public hostname** of the agent API and of the router (§1.1).
- [ ] **Security contact.** No `SECURITY.md` exists yet (E38); `docs/legal/privacy-policy.md` has
      `[TBD: security@ address]`. Also the privacy@ and support addresses.
- [ ] **Legal review of the drafts** in `docs/legal/` (privacy policy, terms, DPA, SLA, ZDR): every
      `[TBD]` filled, subprocessors confirmed (Cloudflare, the Postgres host, the sidecar host,
      Stripe, model providers), DRAFT banners removed only after counsel signs off.
- [ ] **x402 stays off.** `X402_ENABLED` unset in production and staging (decision
      `docs/decisions/0002-x402.md`); `x402-payments.sql` not required.
- [ ] **Worker → sidecar path** (§1) and E34 live, so `/mcp` stops answering 503.
- [ ] **Per-principal connector credentials** for the router's first-party Gmail, Calendar, GitHub,
      Linear and Slack tools (`docs/api/connector-tokens.md`). Built: the router side (E48,
      `caller_connector`); the relay's `POST /v1/internal/router/connector-token`, served from the
      principal's sidecar (E50), which opens the grant from its KMS-keyed token store, refreshes it
      through the core's egress under a per-(principal, slot) lock and writes it back; the hosted
      connect flow (`POST /v1/connectors/{provider}/connect` → the human's page → the vendor →
      `/v1/connectors/callback`, the code exchanged and stored by the sidecar) for Gmail, Calendar,
      Slack and Linear, and GitHub's device flow (the human types a code at
      `github.com/login/device` and presses Check); pending flows sealed in the principal's store
      (a restart keeps them); status and disconnect on every face (`connections.list` /
      `connections.disconnect`, `GET /v1/connectors`, `POST /v1/connectors/{provider}/disconnect`,
      revoking at Google, Slack and Linear, and at GitHub with the app's secret, E51; the answer's
      `vendor_revoke` says which); Notion's hosted sign-in and the router's `notion.search` /
      `notion.pages.text` (E51); an audit row per connect, refresh, revocation, disconnect and
      read-sync fetch in the principal's traceability log (threat model R10 closed). Still needed
      before any of them is listed in the hosted catalog: (a) the vendor OAuth apps (Google "Web
      application" client with the restricted `gmail.modify` scope verified and its security
      assessment passed, a Slack app with user scopes, a Linear OAuth app, a GitHub App with the
      device flow enabled, a Notion public integration with read-content capability), their ids and
      secrets in the sidecar's `SHOGUN_CONNECT_*` and the
      callback registered with each redirect vendor; (b) the Worker → sidecar path above, with
      `SHOGUN_SIDECAR_ADMIN_TOKEN` and `CONNECTOR_CONNECT_SECRET` on the relay; (c) the sidecar's
      NetworkPolicy egress to the vendors' token endpoints and the read-sync's read hosts
      (`deploy/README.md` §3.3); (d) one staging run per provider: connect (GitHub: the device
      code), run a tool (Gmail: `gmail.messages.search` and `gmail.messages.get` too), let the token
      expire and run again (refresh), check the audit rows on the principal's traceability log,
      disconnect from an agent (`POST /v1/connectors/{provider}/disconnect`) and see the vendor
      revocation and `connector_not_connected`.
- [ ] **Connector read-sync (E44)** for Gmail, Calendar and Notion change notices: `shogun-api
      --dream-tick` (scheduled hourly) runs with the sidecar's token store, key provider and
      `SHOGUN_CONNECT_*`, on the same volume; one staging run with a Gmail and a Calendar ingest
      hook shows the notice `queued` and, after the tick, the message or event stored once
      (`connector read-sync: … stored=…`); with the Notion integration (E51) connected, a Notion
      hook's page notice is fetched the same way (`docs/api/ingest-sources.md` §2.8).
- [ ] **The deploy workflow** (`.github/workflows/deploy.yml`, §3.1) is written and has never run:
      create the `staging` and `production` environments with their reviewers, secrets and
      variables, then do one dry run per component and environment before the first real deploy;
      or decide in writing to keep deploying from the owner's machine.
- [ ] **OpenAI directory credit rule** resolved (`docs/directories/openai-credit-rule.md`).
- [ ] **RLS for the remaining relay tables** — files written (E48: `approval-webhooks-`,
      `provider-payouts-`, `x402-payments-`; `connector-tokens-permissions.sql` was removed in E50); apply them (§3 step
      2) and run the step 2 check, which must return no row.
- [ ] **TypeSafe retention terms** for text on the company key (inherited open item).
- [ ] **Anthropic zero retention**: apply, or state the 29-day Batch result window in the privacy
      policy (inherited, `docs/shogunai-origin/ai-gateway-launch-checklist.md` §3).

Zero retention and providers (before real traffic reaches each provider):

- [ ] OpenAI: ZDR approved for the organisation; separate projects for Batch and live, each with an
      enforced monthly budget and low rate limits.
- [ ] Azure OpenAI: **no user traffic** until the modified abuse monitoring exemption is approved;
      not part of the launch path (OpenAI direct first).
- [ ] Groq: ZDR on, spend limit set (before `LIVE_AI_ENABLED`).
- [ ] Deepgram: written no-retention confirmation filed; dedicated prepaid project, auto top-up off.
- [ ] Anthropic: dedicated workspace with a monthly spend limit.
- [ ] Stripe: live keys only after the staging run; webhook events as in §3 step 6.

Operations:

- [ ] Hyperdrive caching disabled on both configs (`hyperdrive get`).
- [ ] WAF rule on `/v1/` for every API hostname; Workers Logs retention short; invocation logs off.
- [ ] `DAILY_SPEND_CAP_USD` sized; provider budgets ≤ `DAILY_SPEND_CAP_USD × 31` plus headroom.
- [ ] Sidecar backup taken and a restore tested; KMS deletion protection on.
- [ ] Staging passed every §7 check, conformance with writes included.
- [ ] Production smoke (§7) passed; conformance read-only passed.
- [ ] Alerts on `ALERT ` log lines (`typesafe_key_rejected`, `speech_`), `saml_replay`,
      `scim_rejected` bursts, `relay_misconfigured`.
