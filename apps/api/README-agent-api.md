# Agent API (E3 + E9)

The agent-facing half of the gateway: `sa_` keys, a prepaid micro-USD wallet, hard caps, the
`402` contract, quotes, receipts, and the metered MCP proxy. Plan: `docs/implementation-plan.md`
E3 / E9 / §7. The licence-token routes (`/v1/batch`, `/v1/review`, …) are unchanged and documented
in `DEPLOY.md`.

Everything here is served only when `AGENT_API_ENABLED=1` (see `src/boot.ts` for the variables).
The store is `sql/agent-keys.sql` (apply like `shared-store.sql`; then `agent-receipts-settled-quantity.sql` and `agent-keys-permissions.sql`).

Conventions:

- Auth: `Authorization: Bearer sa_live_…`. Scopes `read` / `write` / `run` / `admin` (`admin`
  implies all). Keys are stored as a salted PBKDF2-SHA256 hash plus a 10-character lookup
  prefix; the secret is shown once at registration (`src/agent-keys.ts` explains why PBKDF2 and
  not Argon2id on Workers).
- Amounts: integer micro-USD (`1 USD = 1_000_000`).
- Errors: `{ "error": { "code", "message", "next_action" } }` (§7.2). `message` says what to do
  next; branch on `code`.
- Nothing in a request or response body is logged or stored (`src/logging.ts` is the only console
  writer; the log line carries the route template, status, a fixed event word and a hash of the
  key *id*).

## Local end-to-end test

Build `shogun-api` with `cargo build --locked -p shogun-core --features daemon-server --bin shogun-api`,
then run `pnpm test:e2e` from the repository root. This runs the real encrypted sidecar, gateway,
PostgreSQL store, router, and TypeScript and Python SDKs over loopback. It checks tenant isolation,
ingest and router replay, billing, pending L3 approval, the local Dream Cycle and persistence after
a sidecar restart. No model, connector or payment provider is called.

Prerequisites: Node 22+, installed pnpm dependencies, Python 3.10+ with `httpx`, and a local
PostgreSQL account allowed to create databases. `SHOGUN_E2E_POSTGRES_URL` selects that local admin
connection (default `postgres://127.0.0.1:5432/postgres?sslmode=disable`). The runner creates and
removes a randomly named `shogun_e2e_*` database and temporary principal stores. `SHOGUN_API_BIN`
and `SHOGUN_E2E_PYTHON` optionally select the server binary and Python interpreter.

The optional SQL integration suites use `RELAY_TEST_DATABASE_URL`, `RELAY_LIVE_TEST_DATABASE_URL`
and `SHOGUN_ASR_TEST_DATABASE_URL`; each needs its own disposable database. The speech test accepts
only local `shogun_e2e_*` databases, or its original disposable Unix-socket configuration.

## Routes

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/.well-known/pricing.json` | none | The rate table, caps and allowance, rendered from `src/pricing.ts` |
| GET | `/v1/agents/challenge` | none | A proof-of-work challenge for registration |
| POST | `/v1/agents` | PoW header, per-IP limit | Self-registration → key (`read,write,run`) + fresh principal + $5.00 wallet |
| GET | `/v1/agents/me` | `read` | The key's principal, scopes, expiry, caps |
| POST | `/v1/keys` | `write` (the key itself) | Issue a write-only `ingest` key for this principal (the browser extension's): `memory.ingest` over `/mcp` only; shown once, stored hashed ([ingest sources §3.3](../../docs/api/ingest-sources.md)) |
| POST | `/v1/keys/{id}/revoke` | any (own key; `admin` for others; an `ingest` key only itself) | Revoke a key |
| PATCH | `/v1/keys/{id}` | any (lower); `admin` (raise); an `ingest` key only itself | Set a key's hard caps |
| GET | `/v1/wallet` | `read` | Balance, reserved, spendable, caps, this day's / month's spend |
| POST | `/v1/wallet/topup` | `write` | Create a Stripe Checkout session (card rail) |
| POST | `/v1/stripe/webhook` | Stripe signature | Credit a completed Checkout (by event id), close an expired one, debit a `charge.refunded` (by charge high-water mark) |
| POST | `/v1/quote` | `read` | The exact cost of a call, nothing reserved (discounted under a commit, with the list price) |
| GET | `/v1/receipts?since=&limit=` | `read` | Per-call receipts, oldest first |
| POST | `/v1/receipts/{id}/dispute` | `write` | Open a dispute on a receipt (one per receipt) |
| POST | `/v1/handoff` | `read` | An A2A state-of-work handoff for a task, assembled by the core (`handoff.create`) and relayed untouched (free; `docs/api/handoff.md`) |
| GET | `/v1/state/commitments`, `/v1/state/open_loops`, `/v1/state/people`, `/v1/state/people/{person}` | `read` | Free state reads used by the router, relayed to the principal's core with query parameters intact; `person` is the core's numeric id (from `/v1/state/people`), and any other value is a 404 that never reaches the core |
| POST | `/v1/memory/recall` | `read` | Free recall, with the MCP arguments as the body and the core's response relayed untouched |
| GET, POST | `/v1/payments/mandates`, `/v1/payments/mandates/list` | `read` | `payments.mandates.list`, relayed to the core untouched (free; [payments](../../docs/api/payments.md)) |
| POST | `/v1/payments/mandates/request`, `/v1/payments/mandates/revoke`, `/v1/payments/spend` | `run` | `payments.*` relayed to the core untouched: a new mandate and a payment outside one answer `202` with an approval URL for a human (free at the call) |
| GET | `/v1/statements?month=&format=` | `read` | The month's statement, JSON or CSV; immutable once the month closes ([billing](../../docs/api/billing.md)) |
| GET | `/v1/commit` | `read` | The active annual commit: discount, drawn, remaining, overage |
| POST | `/v1/agents/me/licence` | `write` (key only) | Link a ShogunAI licence; Pro earns $20 a month |
| POST | `/v1/admin/commits` | `admin` key, Idempotency-Key | Create an annual commit (10–25% off) |
| POST | `/v1/admin/adjustments` | `admin` key, Idempotency-Key | Credit or refund (a ledger entry; optional Stripe refund) |
| POST | `/v1/admin/disputes/{id}/resolve` | `admin` key | Resolve a dispute: credit or reject |
| GET | `/.well-known/oauth-protected-resource[/mcp]`, `/.well-known/oauth-authorization-server` | none | OAuth metadata ([oauth](../../docs/api/oauth.md)); only with `AGENT_OAUTH_SECRET` |
| GET, POST | `/oauth/authorize` | none (consent page) | Authorization code + PKCE S256; the person pastes the principal's key once |
| POST | `/oauth/token`, `/oauth/revoke` | none (public client + PKCE) | Code exchange, rotated refresh, revocation |
| GET | `/privacy`, `/terms` | none | Privacy policy and terms (DRAFT for legal review) |
| GET | `/legal/dpa`, `/legal/zdr`, `/legal/sla` | none | Data processing addendum, zero-retention commitment, SLA (DRAFT for legal review) |
| POST | `/v1/admin/tenants` | `admin` key, Idempotency-Key | Create a tenant (E26); an enterprise tenant's SCIM token once |
| GET, PATCH | `/v1/admin/tenants/{id}` | `admin` key | Read a tenant; change name, plan, SSO (region is fixed) |
| POST | `/v1/admin/tenants/{id}/principals` | `admin` key | Attach a principal (at most one tenant per principal) |
| POST, DELETE | `/v1/admin/tenants/{id}/admins[/{admin}]` | `admin` key | Add or remove a tenant admin (by e-mail) |
| POST | `/v1/admin/tenants/{id}/scim-token` | `admin` key | Rotate the SCIM bearer |
| GET | `/sso/{tenant}/login` | none (a browser) | Tenant admin sign-in: OIDC (code + PKCE) or SAML 2.0 |
| GET | `/sso/{tenant}/oidc/callback` | the sealed flow cookie | The IdP's redirect → admin session cookie |
| POST | `/sso/{tenant}/saml/acs` | the sealed flow cookie | The IdP's POST → admin session cookie |
| GET | `/sso/{tenant}/saml/metadata` | none | SP metadata for the IdP |
| GET | `/v1/tenant/me`, `/v1/tenant/members` | admin session | The admin, the tenant, its members |
| POST | `/v1/tenant/members/{id}/keys` | admin session | Issue a member's agent key (fills the key slot SCIM opened) |
| GET | `/v1/tenant/audit/export` | admin session | Every member's audit export, concatenated (NDJSON) |
| POST | `/v1/tenant/logout` | admin session | End the session |
| * | `/scim/v2/{ServiceProviderConfig,Schemas,ResourceTypes,Users,Groups}` | tenant SCIM bearer `sct_…` | SCIM 2.0 provisioning |
| ALL | `/mcp` | by unit (see below) | The metered MCP Streamable HTTP proxy |
| POST | `/v1/webhooks` | `write` | Register an approval webhook (`approval.created`, `approval.decided`) |
| GET | `/v1/webhooks` | `read` | The principal's approval webhooks (no secrets) |
| DELETE | `/v1/webhooks/{id}` | `write` | Remove one |
| GET / POST | `/approve/{token}` | the link (+ CSRF on POST) | The human approval page (E15) |
| POST | `/v1/slack/interactions` | Slack signature | Slack button presses on approval messages (E15) |
| POST | `/v1/approvals/notify` | the signed token | An approval was queued: post to Slack, deliver webhooks |
| POST | `/v1/connectors/{provider}/connect` | `run` | A link a human opens to connect their own Gmail, Google Calendar, Slack or Linear account for the router's first-party tools (E50; [connector tokens](../../docs/api/connector-tokens.md)). The agent cannot connect it itself: hand `connect_url` to the human (`next_action: ask_human`), then retry the run |
| GET / POST | `/v1/connectors/connect/{link}` | the link (+ CSRF on POST) | The human's connect page → the provider's consent page |
| GET | `/v1/connectors/callback` | the sealed `state` + the same browser | The provider's redirect; the principal's core stores the account, nothing is kept here |
| POST | `/v1/internal/router/settlements` | `ROUTER_SETTLEMENT_TOKEN` | The router's run settlements → the provider's daily accrual (E29; [payouts](../../docs/api/payouts.md)); only with `PAYOUTS_STATE_SECRET` |
| GET | `/v1/providers/me` | `read` | The provider this principal owns: payout account status, this month's runs and earnings |
| POST | `/v1/providers/me/connect` | `write` (key only) | A Stripe Connect Express onboarding link |
| GET | `/v1/providers/connect/return`, `/v1/providers/connect/refresh` | signed `state` | Stripe's return page (binds `payout_account_ref`) and a fresh link (302) |
| GET | `/v1/providers/me/statements?month=` | `read` | The provider's monthly statement and payout decision |
| POST | `/v1/admin/providers` | `admin` key | Bind a listed provider to its owner principal |
| POST | `/v1/admin/providers/{id}/adjustments` | `admin` key, Idempotency-Key | A refund or a dispute against a provider |
| POST | `/v1/admin/providers/{id}/disputes/{entry}/resolve` | `admin` key | `provider_won` / `provider_lost` |
| POST | `/v1/admin/payouts/run` | `admin` key | Pay a closed month (idempotent per provider-month) |

The open tool catalog (`GET /v1/catalog`, `GET /v1/catalog/{tool_id}`, `GET /catalog`) is served by
the router (`apps/router`, [catalog](../../docs/api/catalog.md)). With `X402_ENABLED=1` (off by
default; [ADR 0002](../../docs/decisions/0002-x402.md)) a `/mcp` 402 also offers the x402 rail and
`X-PAYMENT` pays one call ([402](../../docs/api/402.md)).

Everywhere a key is accepted, an OAuth access token (`sat_…`) is accepted too, as the consenting
key with at most its scopes — except key management, licence linking and admin routes.

## 1. Register

```sh
BASE=https://api.shogunaios.com
CH=$(curl -s $BASE/v1/agents/challenge)            # {"challenge":"…","difficulty_bits":18,"expires_at":"…"}
CHALLENGE=$(echo "$CH" | jq -r .challenge)
BITS=$(echo "$CH" | jq -r .difficulty_bits)
# Find a nonce such that sha256(challenge + nonce) has $BITS leading zero bits (src/pow.ts `solvePow`
# does this in a loop; at 18 bits it takes well under a second).
NONCE=$(node -e '
  const {createHash}=require("node:crypto");const [c,b]=process.argv.slice(1);
  const lz=h=>{let n=0;for(const ch of h){const v=parseInt(ch,16);if(v===0){n+=4;continue;}n+=Math.clz32(v)-28;break;}return n;};
  for(let i=0;;i++){const n=i.toString(36);if(lz(createHash("sha256").update(c+n).digest("hex"))>=Number(b)){console.log(n);break;}}' "$CHALLENGE" "$BITS")

curl -s -X POST $BASE/v1/agents \
  -H "X-Agent-Pow: $CHALLENGE:$NONCE" \
  -H 'content-type: application/json' \
  -d '{"ref":"dev_alice"}'
```

```json
{
  "key": "sa_live_1ZqK…(43 chars)",
  "key_id": "ak_8c1f…", "prefix": "1ZqK7v0wQp",
  "principal_id": "prn_…", "tenant_id": "prn_…",
  "scopes": ["read", "write", "run"],
  "created_at": "2026-10-25T12:00:00.000Z", "expires_at": "2027-10-25T12:00:00.000Z", "ref": "dev_alice",
  "wallet": { "balance_micro_usd": 5000000, "reserved_micro_usd": 0 },
  "caps": { "key": { "day_micro_usd": 1000000, "month_micro_usd": 20000000 }, "principal": { "day_micro_usd": 5000000, "month_micro_usd": 100000000 } },
  "next_action": null
}
```

Body fields (all optional): `ref` (another key id or a developer id, `[A-Za-z0-9_.:-]{1,128}`),
`caps: { day_micro_usd, month_micro_usd }` (may only *lower* the key defaults).

Refusals: `401 pow_missing | pow_malformed | pow_invalid | pow_expired | pow_insufficient |
pow_reused` (`next_action: "solve_challenge"`), `429 rate_limited` (per IP per hour, with
`Retry-After`), `400 invalid_ref | invalid_caps | caps_above_default`.

## 2. Call MCP (metered)

The proxy reads three headers and never the body:

| Header | Meaning |
|---|---|
| `Mcp-Method` | the JSON-RPC method (`tools/call`, `initialize`, `tools/list`, …). Anything but `tools/call` is free `protocol` traffic |
| `Mcp-Name` | the tool name for `tools/call` |
| `Mcp-Quantity` | units in this call (events for `memory.ingest`); default `1`, max `1,000,000` |
| `Mcp-Dry-Run: true` | quote only: nothing reserved, nothing forwarded |
| `Mcp-Tool-Id` | for `router.run`: the tool whose provider price the router looks up |

Classification (`src/pricing.ts`):

| Unit | Price | Scope needed | Tools |
|---|---|---|---|
| `read` | $0 | `read` | `state.*`, `memory.search`, `memory.recall`, `memory.get_context_pack`, `memory.get_context`, `memory.get_events`, `memory.read_session`, `memory.get_wrap`, `memory.get_periods`, `calendar.upcoming`, `profile.whoami`, `wiki.evidence`, `actions.status`, `approvals.*`, `router.discover`, `router.inspect` |
| `ingest` | 500 µ$ per event ($0.50 / 1,000) | `write` | `memory.ingest`, `memory.append_note` |
| `model_op` | 100 µ$ per call ($0.10 / 1,000) | `write` | every other `tools/call` |
| `run` | provider price × 1.05, rounded up | `run` | `router.run`, `actions.execute` (price from the router; from headers alone a run is refused `400 run_price_unavailable`) |
| `storage` | 100,000 µ$ per GB-month | — | booked by the storage sweep, not per call |
| `protocol` | $0 | any valid key | every non-`tools/call` method and `GET`/`DELETE /mcp` |

```sh
curl -s -X POST $BASE/mcp \
  -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -H 'Mcp-Method: tools/call' -H 'Mcp-Name: memory.ingest' -H 'Mcp-Quantity: 12' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory.ingest","arguments":{…}}}' -i
```

```
HTTP/1.1 200 OK
X-Receipt-Id: rcpt_…
X-Usage-Unit: ingest
X-Usage-Quantity: 12
X-Usage-Cost-Micro-Usd: 6000
X-Latency-Ms: 41
```

The body is the core's, relayed untouched as a stream. Sequence inside the gateway: verify key
(`401` with `WWW-Authenticate` first, whatever else the request says) → classify → quote → **reserve** (wallet and the four cap
counters in one transaction) → relay to the principal's sidecar → **settle** once from the reply's
`Mcp-Quantity` (the units the core took, capped at the reservation; `0` for a JSON-RPC error, a
refusal or an idempotent replay, which travel as HTTP 200; a non-2xx or a reply without the header
settles at 0). `X-Usage-Quantity` is the settled figure. A free call writes a settled receipt at
cost 0. Contract and relay: [billing.md "Hosted `/mcp`"](../../docs/api/billing.md).

A dry run answers `200 { "dry_run": true, "quote": { "unit", "quantity", "cost_micro_usd" },
"sufficient": true, "balance_micro_usd": …, "usage": {…,"cost_micro_usd":0}, "latency_ms" }`.
The real call then settles at exactly `quote.cost_micro_usd` when the core takes every unit.

Refusals: `401` (key), `400 invalid_metering_header` (and the core's JSON-RPC `400 missing_header`), `403 insufficient_scope`,
`402 payment_required | cap_exceeded` (`docs/api/402.md`), `421 wrong_shard`,
`503 upstream_unavailable | principal_not_provisioned | core_rejected_gateway | wallet_unavailable | capacity_exhausted`
(the last is the company's global daily breaker; `Retry-After` to UTC midnight).

## 3. Quote

```sh
curl -s -X POST $BASE/v1/quote -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"method":"tools/call","name":"memory.ingest","quantity":250}'
```

```json
{ "quote": { "unit": "ingest", "quantity": 250, "cost_micro_usd": 125000 }, "method": "tools/call", "name": "memory.ingest",
  "balance_micro_usd": 5000000, "sufficient": true, "caps": { "…" }, "usage": { "unit": "ingest", "quantity": 0, "cost_micro_usd": 0 }, "next_action": null }
```

For a run pass `provider_price_micro_usd` (from `router.inspect`): `{"name":"router.run","provider_price_micro_usd":1999}` → `2099`.

## 4. Wallet and top-up

```sh
curl -s $BASE/v1/wallet -H "Authorization: Bearer $KEY"
```

```json
{ "principal_id": "prn_…", "balance_micro_usd": 4994000, "reserved_micro_usd": 0, "available_micro_usd": 4994000,
  "caps": { "…" }, "spend": { "key": { "day_micro_usd": 6000, "month_micro_usd": 6000 }, "principal": { "day_micro_usd": 6000, "month_micro_usd": 6000 } },
  "topup": { "rail": "card", "url": "https://api.shogunaios.com/v1/wallet/topup", "method": "POST", "min_micro_usd": 5000000, "max_micro_usd": 500000000 } }
```

```sh
curl -s -X POST $BASE/v1/wallet/topup -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -H 'Idempotency-Key: topup-2026-10-25-1' -d '{"amount_micro_usd":10000000}'
```

```json
{ "rail": "card", "url": "https://checkout.stripe.com/c/pay/cs_…", "session_id": "cs_…", "amount_micro_usd": 10000000,
  "next_action": "approve_at_url", "instructions": "Open url in a browser and pay. …" }
```

Hand `url` to a human once. Stripe calls `POST /v1/stripe/webhook` (configure the endpoint for `checkout.session.expired`, `charge.refunded` and
`checkout.session.completed`; `Stripe-Signature` is verified with `STRIPE_WEBHOOK_SECRET`); the
wallet is credited `amount_total` once per event id. Then retry the `402`'d call unchanged.
Without `STRIPE_SECRET_KEY` the route answers `503 topup_unavailable`.

## 5. Caps

Defaults (`src/caps.ts`): key $1/day, $20/month; principal $5/day, $100/month. Checked at
reservation against booked estimates (so in-flight calls count), corrected at settlement.
UTC periods.

```sh
curl -s -X PATCH $BASE/v1/keys/$KEY_ID -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"day_micro_usd":250000}'                       # lower: any key; raise above default: admin only
curl -s -X POST  $BASE/v1/keys/$KEY_ID/revoke -H "Authorization: Bearer $KEY"
```

Exceeding a cap: `402 cap_exceeded`, `payment.cap = { scope, period, limit_micro_usd,
spent_micro_usd }`, `Retry-After` until the period resets. Deployment-wide defaults:
`AGENT_KEY_CAP_DAY_USD` and friends.

## 6. Receipts and disputes

```sh
curl -s "$BASE/v1/receipts?since=2026-10-25T00:00:00Z&limit=100" -H "Authorization: Bearer $KEY"
```

```json
{ "receipts": [
    { "id": "rcpt_…", "key_id": "ak_…", "method": "tools/call", "name": "memory.ingest", "unit": "ingest", "quantity": 12,
      "reserved_quantity": 12, "settled_quantity": 10,
      "cost_micro_usd": 5000, "reserved_micro_usd": null, "settled": true, "ts": "2026-10-25T12:00:05.000Z" } ],
  "next_since": "2026-10-25T12:00:05.001Z" }
```

`since` is ISO-8601 or epoch milliseconds; page with `next_since`. `reserved_micro_usd` is
non-null only while a call is in flight. `reserved_quantity` (also `quantity`) is what was quoted
and held; `settled_quantity` is what the call settled at: the quantity the core reported (at most
the reservation), 1 for a router run that succeeded, 0 for a failed, refused or replayed call. It is
`null` while the call is in flight, and on receipts settled before the gateway recorded it.

```sh
curl -s -X POST $BASE/v1/receipts/rcpt_…/dispute -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"reason":"ingest counted 12 events, I sent 10"}'
```

`201 { "dispute_id": "dsp_…", "receipt_id": "rcpt_…", "status": "open", "ts": "…" }`; a second
dispute on the same receipt answers `200` with the existing one. Someone else's receipt is `404`.

## 7. Remote approvals (E15)

An L3 call (and an L2 one the core queues) answers with an approval URL on this gateway:
`https://<gateway>/approve/<token>`. The token is minted by the sidecar, signed with
`SHOGUN_APPROVAL_SIGNING_KEY` (the sidecar and the gateway hold the same value; nothing else
does), expires with the approval (10 minutes) and decides once. Hand the URL to a human; poll
`actions.status` until it reads `approved` or `rejected`. An agent never decides: there is no
tool for it, and the sidecar refuses an agent's bearer on the decide route (`403 human_only`).

- **Web page** (`GET /approve/{token}`): server-rendered, no JavaScript, the redacted summary
  (kind, tier, recipient class — never the recipient — a masked one-line preview, times) and
  Approve / Reject. The form carries a CSRF value bound to a `SameSite=Strict` cookie and to the
  link; a POST from another origin, without the cookie or with a wrong value is refused. The page
  sends `Referrer-Policy: no-referrer` and a `default-src 'none'` CSP. The decision is recorded as
  `web:approval-page`.
- **Slack** (`SLACK_BOT_TOKEN`, `SLACK_SIGNING_SECRET`, `SLACK_APPROVAL_CHANNEL`): when an
  approval is queued the gateway posts the summary with Approve / Reject buttons to the channel
  (or a user's DM) with `chat.postMessage`. A press reaches `POST /v1/slack/interactions`, which
  verifies Slack's signature (`v0`, five-minute window) before parsing, decides as
  `slack:<user id>`, and replaces the message with the outcome.
- **Webhooks**: `POST /v1/webhooks {"url": "https://…", "events": ["approval.created",
  "approval.decided"]}` → `201 {id, url, events, secret}` (the secret is shown once; it is derived
  from `WEBHOOK_SIGNING_SECRET` and never stored). Each delivery is signed
  `X-Shogun-Signature: t=<unix>,v1=hex(HMAC-SHA256(secret, "<t>.<raw body>"))` with
  `X-Shogun-Event` and `X-Shogun-Delivery`; a network error, 429 or 5xx is retried after 1 s, 5 s
  and 30 s. The body is the redacted summary only:

  ```json
  { "id": "whd_…", "event": "approval.created", "created_at": "…", "principal_id": "prn_…",
    "approval": { "approval_id": 9, "status": "pending", "level": "L3", "kind": "Send email",
                  "recipient_class": "email_recipient", "preview": "…", "created_at": "…", "expires_at": "…" } }
  ```

How the gateway learns of a new approval without reading a body: the core's answer to the call
that queued it carries `X-Shogun-Approval-Token`; the `/mcp` proxy reads that header (and drops
it from the relayed answer) and fans out. A core that is not behind `/mcp` posts the token to
`POST /v1/approvals/notify`. All three surfaces reach the sidecar through one client
(`src/approvals/sidecar-client.ts`): `GET /v1/approvals/{id}` and
`POST /v1/approvals/{id}/decide` with the token in `X-Shogun-Approval-Token`. The sidecar verifies
the token again, spends its nonce (`409 token_replayed` on a second use, `410 token_expired`
after expiry), records who decided and when, and writes the decision to the principal's audit
log.

## Licence tokens and agent keys together (E3-T4)

The Ed25519 licence token stays the credential of the Mac app's routes. Both credentials meter
into the same string space (`auth.ts` `meterSubjectOf`: a licence id or `account:<acc>` for a
licence, `principal:<id>` for an agent key), and cost-bearing agent calls book their quote
against the same global daily breaker row the Batch lane uses (`relay_spend_days`), so the
company's spend ceiling covers both. The wallet, caps and receipts are the agent side's own
tables.

## Operations

- Provision: `psql … -f apps/api/sql/agent-keys.sql`, `-f apps/api/sql/agent-receipts-settled-quantity.sql`, then `-f apps/api/sql/agent-keys-permissions.sql`.
- Secrets (`wrangler secret put`): `AGENT_POW_SECRET`, `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET`;
  for remote approvals `SHOGUN_APPROVAL_SIGNING_KEY` (≥ 32 characters, the sidecar's value),
  `WEBHOOK_SIGNING_SECRET` (≥ 32 characters) and optionally `SLACK_BOT_TOKEN` +
  `SLACK_SIGNING_SECRET`; vars `SHOGUN_SIDECAR_URL` and `SLACK_APPROVAL_CHANNEL`. Provision
  `apps/api/sql/approval-webhooks.sql`. The sidecar needs `SHOGUN_APPROVAL_SIGNING_KEY`,
  `SHOGUN_APPROVAL_BASE_URL=https://<gateway>` and, for the operator's CLI, `SHOGUN_ADMIN_TOKEN`.
- Vars: `AGENT_API_ENABLED=1`, `PUBLIC_BASE_URL`, optionally `AGENT_POW_BITS`,
  `AGENT_REGISTER_PER_HOUR`, `AGENT_KEY_PBKDF2_ITERATIONS`, the four `AGENT_*_CAP_*_USD`.
- Boot log: `agent_api open (card top-ups on)` / `agent_api closed (AGENT_API_ENABLED unset)`.
- Log events to alert on: `event=payment_required` runs (a client looping on 402),
  `event=capacity_exhausted` (the breaker closed the day), `event=webhook_rejected` (a bad
  Stripe signature), `stripe_failure status=…`.
- The periodic sweep deletes expired PoW rows and settled receipts older than 400 days;
  unsettled receipts are never deleted (they hold money).

The core behind `/mcp` is the sidecar relay (E34, `src/mcp-proxy.ts`): on with
`SHOGUN_SIDECAR_URL` (or, on the Worker, `SHOGUN_SIDECAR_BINDING`) and
`SHOGUN_SIDECAR_PRINCIPAL_TOKENS` or `SHOGUN_SIDECAR_BEARER_SECRET` (optionally `SHOGUN_SHARD`;
transport options in `docs/deploy/production.md` §1); without them `/mcp` answers `503 upstream_unavailable` before
reserving. Not in this slice: the router's `runPricer`, storage billing (the
rate is published; the sweep that books it comes with the core's usage report), and webhook
delivery of the billing events (`wallet.low_balance`, `cap.reached` go to the in-memory notifier
until E15 adds per-principal webhooks; shapes in `docs/api/billing.md`).

## 7. Statements, commits and OAuth

- Monthly statements, annual commits, refunds, credits, dispute resolution, the ShogunAI Pro
  bundle and the billing events: [`docs/api/billing.md`](../../docs/api/billing.md).
- OAuth 2.1 with Client ID Metadata Documents for MCP clients (no dynamic registration):
  [`docs/api/oauth.md`](../../docs/api/oauth.md).
- Operations (schema, Stripe events, the overage price, the admin key, `AGENT_OAUTH_SECRET`):
  `DEPLOY.md` §9.1.

## 8. Enterprise: tenants, SSO, SCIM, regions, audit export (E26)

Served when `ENTERPRISE_ENABLED=1` (with the agent API on). Code: `src/enterprise/`
(`app.ts` routes, `oidc.ts`, `saml.ts`, `scim.ts`, `region.ts`, `store.ts`, `crypto.ts`,
`fetch-guard.ts`, `sidecar-ops.ts`); schema `sql/enterprise.sql`; operations in `DEPLOY.md` §9.2.
Contract drafts: `/legal/dpa`, `/legal/zdr`, `/legal/sla`.

**Tenants and admins.** A tenant is an organization: `name`, `region` (us | eu | jp | …, fixed at
creation), `plan` (`self-serve` | `enterprise`) and, for enterprise, an SSO configuration. The
operator creates it with an `admin` key:

```bash
curl -X POST "$BASE/v1/admin/tenants" -H "authorization: Bearer $ADMIN_KEY" -H "idempotency-key: acme-1" \
  -H 'content-type: application/json' \
  -d '{"name":"Acme","region":"eu","plan":"enterprise",
       "sso":{"protocol":"oidc","enforced":true,
              "oidc":{"issuer":"https://acme.okta.com","client_id":"0oa…","client_secret":"…"}}}'
# 201 {"tenant":{…,"sso":{"login_url":".../sso/tnt_…/login","oidc":{"redirect_uri":".../sso/tnt_…/oidc/callback",…}}},
#      "scim_token":"sct_…"}          ← shown once; stored as a SHA-256 hash
```

Then `POST /v1/admin/tenants/{id}/admins {"email": "it@acme.example"}` names the people who
administer it. Tenant admins are not agent keys: they hold no key, they sign in at
`/sso/{tenant}/login` with the customer's IdP, and the first sign-in binds their IdP subject (by a
verified e-mail for OIDC; by the NameID or an e-mail attribute for SAML). A principal belongs to at
most one tenant (`POST /v1/admin/tenants/{id}/principals` attaches an existing one; `409
principal_in_other_tenant` otherwise).

**OIDC.** Authorization code with PKCE S256. The issuer's discovery document must name the same
`issuer`; the ID token is verified against its JWKS (by `kid`, one refetch for rotation), with
`alg` limited to RS/PS/ES (never `none` or HMAC), `iss` exact, `aud` containing the client id (and
`azp` when there are several), `exp` / `iat` / `nbf` within `SSO_CLOCK_SKEW_SECS` (default 120),
and the `nonce` sealed in the flow cookie. The client secret is sealed at rest (AES-256-GCM under
a key derived from `ENTERPRISE_SECRET`) and never returned. Every fetch to the IdP (discovery,
JWKS, token) goes through the same SSRF guard as CIMD fetches.

**SAML 2.0.** SP-initiated: `/sso/{tenant}/login` sends an AuthnRequest (HTTP-Redirect) and the IdP
posts to `/sso/{tenant}/saml/acs`. Give the IdP the metadata at `/sso/{tenant}/saml/metadata`
(entity id `<base>/sso/{tenant}/saml/metadata`, ACS `<base>/sso/{tenant}/saml/acs`) and give the
operator the IdP's entity id, SSO URL and signing certificate(s). Accepted: exactly one assertion,
itself signed (enveloped, one reference to its own ID; a signed Response around an unsigned
assertion is refused), RSA-SHA256/512 and SHA-256/512 only, verified against the configured
certificates (never `KeyInfo`), every value read from the signed bytes, `Issuer`, audience,
recipient, `InResponseTo`, validity window with skew, and the assertion id spent in a replay cache
until it expires. Also (E39): SAML `Version="2.0"` only; the Response's own `Issuer`, when present,
must be the IdP; the Response and assertion `IssueInstant` may be neither ahead of now nor older
than the ten-minute sign-in flow (each with the skew); one `Issuer`, `Subject` and `Conditions` per
assertion; a bearer confirmation whose `NotBefore` is still ahead does not confirm. Not supported,
and refused with a fixed code rather than half-handled: XML Encryption (an `EncryptedAssertion`,
`EncryptedID` or `EncryptedAttribute` is `saml_encrypted_assertion_unsupported`; no XML-Enc library
has been vetted to run on Workers, so configure the IdP to sign the assertion and not encrypt it —
the POST travels over TLS) and IdP-initiated sign-in (no `InResponseTo` is
`saml_in_response_to_mismatch`). Library:
`xml-crypto` 6.4.0 over `@xmldom/xmldom` 0.8.15 — both run on Workers (`nodejs_compat`); the
workerd test suite signs and verifies an assertion, so SAML is served on Workers, not behind a
Node-only flag.

**Admin session.** A completed sign-in sets `__Host-shogun_tenant_admin`: a signed cookie
(`HttpOnly; Secure; SameSite=Strict; Path=/`), 30 minutes, re-checked on each request against the
admin row, so removing an admin ends their session at once. The sign-in itself rides a sealed,
ten-minute `__Host-shogun_sso_flow` cookie (`SameSite=None`, because the SAML IdP returns with a
cross-site POST) that holds the state, nonce and PKCE verifier or the request id.

**SSO enforcement.** With `"enforced": true`, the tenant's principals cannot connect OAuth clients
by pasting an agent key on `/oauth/authorize` (403 on the consent page). Their keys are issued by a
tenant admin instead (`POST /v1/tenant/members/{id}/keys`, scopes read / write / run, never
admin, shown once). Send an `Idempotency-Key` to make issuance safe to retry: the key id derives
from (tenant, member, Idempotency-Key), so the same request answers `200` with the same `key_id`
and `key: null` instead of minting a second key (the secret is shown only by the call that created
it; a lost secret means revoke and issue again with a new Idempotency-Key). The same
Idempotency-Key with other scopes is `409 idempotency_key_reused`.

**SCIM 2.0.** Base `<base>/scim/v2`, bearer `sct_…` from tenant creation or
`POST /v1/admin/tenants/{id}/scim-token`. Tested with request shapes from Okta and Entra ID.

| SCIM | ShogunAgent |
|---|---|
| `POST /Users` | A new principal under the tenant (`id` = principal id), no key yet: the key slot is open until a tenant admin issues one |
| `GET /Users?filter=userName eq "…"` | Exact, case-insensitive match; `externalId eq` too; `startIndex` (1-based) and `count` (≤ 200) per RFC 7644 |
| `active: false` by `PATCH` or `PUT` (Okta `{"op":"replace","value":{"active":false}}`, Entra `{"op":"Replace","path":"active","value":"False"}`) | Every key of the principal and every OAuth grant minted from them is revoked immediately; reactivation does not bring them back |
| `DELETE /Users/{id}` | Access revoked now; the user leaves its groups; a tombstone (ids and times) stays; the principal store is purged by the sweep after `SCIM_DELETE_GRACE_DAYS` (default 30): the sweep calls the sidecar's `DELETE /v1/principals/{id}` (operator bearer), which closes the store, removes the KMS-wrapped key first, then the store files and the principal's directory, drops it from every team, and appends a content-free line to `principal-purges.jsonl`. Idempotent; a store still in use answers `409 principal_busy` and the purge stays due for the next sweep |
| `POST /Groups`, `PATCH` members | A Team (E18): every pair of members gets a world-read grant both ways, pushed to the sidecar (`POST /v1/teams`, `/v1/acl/grant`, `/v1/acl/revoke`); a grant is revoked only when two people share no other group. At most 30 members per group |

Unknown attributes (for example Entra's enterprise extension) are accepted and ignored; nothing
beyond userName, name, e-mails, external id and active is stored, and no SCIM body is logged.

**Regions.** A deployment serves one region (`SHOGUN_REGION`). A request whose principal belongs to
a tenant in another region is refused before anything is read or booked:

```json
421 {"error":{"code":"wrong_region","message":"This principal is served in region eu. Send the request to https://eu.api.example instead; …",
              "next_action":"use_region_endpoint","region":"eu","region_endpoint":"https://eu.api.example"}}
```

The endpoint comes from `SHOGUN_REGION_ENDPOINTS`. Regional deployments are separated completely:
each has its own Worker, Postgres, sidecar fleet and KMS keys; they share no database and never
call each other, so there is no cross-region data path. A tenant's region cannot be changed in
place (`400 region_immutable`): moving means creating the tenant in the new region and moving each
principal with export and import; the old deployment keeps a routing stub (the tenant row and its
memberships, ids only) so old keys get the 421 instead of a bare 401. SCIM, SSO and the admin
routes of a tenant homed elsewhere answer the same 421.

**Audit export.** `GET /v1/tenant/audit/export[?since_ms=&until_ms=]` (admin session) streams
NDJSON: a header line, then for each member principal
`{"type":"principal","principal_id":"prn_…","status":200}` followed by that principal's sidecar
export (`GET {sidecar}/v1/audit/export?principal=…`) byte for byte, and a final
`{"type":"end","principals":N,"failed":M}`. A member whose export failed gets its header with the
status and error and no body. The gateway never parses the records.
