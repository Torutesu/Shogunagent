/**
 * Body-redacting request logging (docs/batch-relay-design.md §3.2 / NFR-PRV-04).
 *
 * Written FIRST, before any route handler: the relay's core invariant is that chunk content is
 * never persisted, and logs are the easiest place to break that by accident. This middleware is
 * therefore the only sanctioned way a request leaves a trace, and it is structurally unable to
 * leak: it reads nothing but method, path, status, duration, and (on POST) the *count* of
 * accepted items a handler chose to expose via `relay-log-*` context vars. No body, no headers
 * (the Authorization header is a bearer credential), no query strings beyond the path.
 *
 * Runtime-neutral: `createHash` is the only import, and it is one of the `node:crypto` functions
 * the Workers runtime provides under `nodejs_compat` (exercised by test/workers).
 */
import { createHash } from "node:crypto";
import type { MiddlewareHandler } from "hono";

export type LogFn = (line: string) => void;

/**
 * The relay's console writers — the ONLY place `console.*` is called in the relay's own code
 * (`index.ts`, `worker.ts` and `boot.ts` all route through here). Every line is a fixed string
 * plus counters, route templates or env-variable *names*: never a body, a header, an id, a token
 * or an environment *value*. On Cloudflare Workers, Workers Logs captures all console output, so
 * this is also the complete inventory of what can reach it. Keep it that way.
 */
export const operational = {
  info: (line: string): void => console.log(line),
  warn: (line: string): void => console.warn(line),
  error: (line: string): void => console.error(line),
} as const;

/** Stable short hash of a licence id, for the log line.
 *
 * The id is not user content, but `license=<sub>` still turns the console into a per-customer
 * activity timeline, and the log sink is usually less protected than the usage ledger. A hash
 * keeps what the line is actually for — correlating requests within an incident — without the
 * identifier itself. */
export function logId(licenseId: string): string {
  return createHash("sha256").update(licenseId).digest("hex").slice(0, 12);
}

/** Context vars a handler may set for the log line. Numbers only — never content. */
export interface LogVars {
  /** Number of chunks accepted (set by POST /v1/batch). */
  relayLogChunks?: number;
  relayLogReservedChunks?: number;
  relayLogEvent?: "cap_reached" | "spend_cap_reached" | "license_spend_cap_reached" | "duplicate_prevented" | "payload_conflict" | "submission_unknown" | "provider_accepted_unbound" | "provider_rejected" | "metering_unavailable" | "running" | "batch_failed_no_results"
    // `/v1/ai/form_decide` (issue #1144).
    | "form_decide_unavailable" | "in_flight_capped" | "typesafe_key_rejected" | "upstream_rate_limited" | "upstream_overloaded"
    // The agent routes (agent-app.ts / metering.ts): fixed words, never a value.
    | "agent_key_rejected" | "agent_registered" | "pow_rejected" | "register_rate_limited" | "payment_required" | "cap_exceeded"
    | "wallet_unavailable" | "capacity_exhausted" | "dry_run" | "run_price_unavailable" | "upstream_unavailable" | "upstream_failure"
    // The hosted /mcp relay (E34, mcp-proxy.ts): a principal on another replica; a billable 2xx
    // without the core's `Mcp-Quantity` (settled at 0 — a sidecar older than the contract).
    | "wrong_shard" | "settle_quantity_missing"
    | "stripe_unavailable" | "stripe_failure" | "topup_session_created" | "topup_credited" | "topup_duplicate" | "webhook_rejected"
    // `POST /v1/handoff` (handoff.ts, plan E16 / E35): relayed to the core, which assembles it.
    | "handoff_relayed"
    // E51: a write-only ingest key issued (agent-app.ts `POST /v1/keys`), or its replay.
    | "ingest_key_issued" | "ingest_key_replayed"
    // E19 billing (billing/routes.ts, the Stripe webhook): fixed words.
    | "statement_closed" | "commit_created" | "adjustment_applied" | "refund_debited" | "dispute_resolved" | "licence_linked" | "topup_expired"
    // E3-T2 OAuth (oauth/app.ts): fixed words, never a client id, a code or a token.
    | "oauth_consent_shown" | "oauth_code_issued" | "oauth_denied" | "oauth_token_issued" | "oauth_refresh_rotated" | "oauth_refresh_reuse"
    | "oauth_code_reuse" | "oauth_invalid_request" | "oauth_client_rejected" | "oauth_revoked"

    // Remote approvals (approvals/app.ts, plan E15): fixed words, never a token or a summary.
    | "approval_page" | "approval_decided" | "approval_refused" | "approval_notified" | "slack_rejected" | "webhook_registered"
    // E29 provider payouts (payouts-app.ts): fixed words, never an amount, a provider id or a reference.
    | "payout_store_unavailable" | "settlement_rejected" | "settlements_recorded" | "router_reserved" | "router_reserve_replayed" | "connect_link_created" | "payouts_run"
    // E48 connector tokens for the router (connector-tokens.ts): fixed words, never a token, a slot value or a principal.
    | "connector_token_served" | "connector_token_rejected" | "connector_not_connected" | "connector_token_unreadable"
    | "connector_token_expired" | "connector_token_unavailable" | "connector_vault_unconfigured"
    // E50: the sidecar vault and the hosted connect flow (connector-tokens.ts, connector-connect.ts).
    | "connector_vault_rejected_gateway" | "connector_vault_not_provisioned" | "connector_principal_refused"
    | "connector_connect_link_created" | "connector_connect_page" | "connector_connect_started" | "connector_connected" | "connector_connect_refused"
    // E50 follow-ups: GitHub's device flow, the status read and the disconnect.
    | "connector_device_pending" | "connector_status_served" | "connector_disconnected" | "connector_vault_unavailable"
    // E30 x402 (billing/x402.ts): fixed words, never a payment payload.
    | "x402_rejected" | "x402_replayed" | "x402_verified" | "x402_settled" | "x402_settle_failed"
    // E26 enterprise (enterprise/*.ts): fixed words, never a token, an assertion, a claim or a SCIM body.
    | "region_refused" | "tenant_directory_unavailable" | "tenant_created" | "tenant_updated" | "principal_attached" | "tenant_admin_added"
    | "scim_token_rotated" | "sso_login_started" | "sso_login_succeeded" | "sso_login_refused" | "saml_replay" | "admin_session_rejected"
    | "scim_rejected" | "scim_user_created" | "scim_user_updated" | "scim_user_deactivated" | "scim_user_deleted" | "scim_group_synced"
    | "sidecar_unavailable" | "audit_export_streamed" | "oauth_sso_required" | "member_key_issued" | "member_key_replayed"
    // E44 ingest hooks (ingest-hooks/app.ts): fixed words, never a hook secret, a delivery id or a payload.
    | "ingest_hook_created" | "ingest_hook_replayed" | "ingest_hook_rotated" | "ingest_hook_deleted" | "ingest_hook_rejected"
    | "ingest_hook_rate_limited" | "ingest_hook_handshake" | "ingest_hook_relayed" | "ingest_hook_refused_by_core" | "ingest_hook_store_unavailable";
  /** Server-chosen provider and model of the accepted batch (ids, not content). */
  relayLogModel?: string;
  /** License id (an opaque token id, not user content). */
  relayLogLicense?: string;
  /** Agent key id (`ak_…`, an opaque id — never the key; agent-keys.ts). Hashed in the line. */
  relayLogAgentKey?: string;
}

/** The route template for a path — never the raw path, which for `/v1/batch/{id}` carries a batch
 * id. Shared by the log line and the metrics (FR-SVC-06) so both name routes the same way. */
export function routeTemplate(pathname: string): string {
  if (pathname === "/v1/batch" || pathname === "/v1/review" || pathname === "/v1/batch/recover" || pathname === "/v1/ai/form_decide" || pathname === "/healthz" || pathname === "/metrics") return pathname;
  if (["/v1/asr/grant", "/v1/asr/refresh", "/v1/asr/finish"].includes(pathname)) return pathname;
  if (pathname === "/v1/ai/usage") return pathname;
  // The agent routes (agent-app.ts). Ids in the path (`/v1/keys/{id}`, `/v1/receipts/{id}`) are
  // collapsed to their template.
  if (["/v1/agents", "/v1/agents/challenge", "/v1/agents/me", "/v1/wallet", "/v1/wallet/topup", "/v1/wallet/topup/done",
    "/v1/stripe/webhook", "/v1/quote", "/v1/receipts", "/v1/handoff", "/mcp", "/.well-known/pricing.json"].includes(pathname)) return pathname;
  // E51: the ingest-key issue route and the payments REST relay (fixed paths, no ids).
  if (["/v1/keys", "/v1/payments/mandates", "/v1/payments/mandates/list", "/v1/payments/mandates/request", "/v1/payments/mandates/revoke", "/v1/payments/spend"].includes(pathname)) return pathname;
  if (["/v1/state/commitments", "/v1/state/open_loops", "/v1/memory/recall"].includes(pathname)) return pathname;
  if (/^\/v1\/state\/people\/[^/]+$/.test(pathname)) return "/v1/state/people/:person";
  if (/^\/v1\/memory\/processing\/[^/]+$/.test(pathname)) return "/v1/memory/processing/:id";
  // The agent-readable documents (plan E10, wellknown/app.ts): fixed paths, no ids.
  if (["/.well-known/mcp/server-card.json", "/.well-known/agent-card.json", "/.well-known/status.json", "/.well-known/mcp-config",
    "/health", "/openapi.json", "/llms.txt", "/llms-full.txt", "/privacy", "/terms"].includes(pathname)) return pathname;
  // E19 billing and E3-T2 OAuth: fixed paths; ids collapsed below.
  // E32: the router's internal wallet routes (fixed paths).
  // E48: the router's connector-token route (connector-tokens.ts, a fixed path).
  if (["/v1/internal/router/reserve", "/v1/internal/router/settlements", "/v1/internal/router/connector-token"].includes(pathname)) return pathname;
  // E50: the hosted connect flow. The link is collapsed to its template, never logged; the
  // callback's query (state, code) is never part of a template.
  if (pathname === "/v1/connectors/callback") return pathname;
  if (/^\/v1\/connectors\/connect\/[^/]+$/.test(pathname)) return "/v1/connectors/connect/:link";
  if (/^\/v1\/connectors\/[^/]+\/connect$/.test(pathname)) return "/v1/connectors/:provider/connect";
  if (["/v1/statements", "/v1/commit", "/v1/agents/me/licence", "/v1/admin/commits", "/v1/admin/adjustments",
    "/.well-known/oauth-protected-resource", "/.well-known/oauth-protected-resource/mcp", "/.well-known/oauth-authorization-server",
    "/oauth/authorize", "/oauth/token", "/oauth/revoke"].includes(pathname)) return pathname;
  if (/^\/v1\/admin\/disputes\/[^/]+\/resolve$/.test(pathname)) return "/v1/admin/disputes/:id/resolve";
  // Remote approvals (plan E15). The approval page's path carries the decision token: it is
  // collapsed to its template, never logged.
  if (["/v1/slack/interactions", "/v1/approvals/notify", "/v1/webhooks"].includes(pathname)) return pathname;
  if (/^\/approve\/[^/]+$/.test(pathname)) return "/approve/:token";
  if (/^\/v1\/webhooks\/[^/]+$/.test(pathname)) return "/v1/webhooks/:id";
  // E26 enterprise. Tenant, admin, user and group ids in the path collapse to their template.
  const enterprise = enterpriseRouteTemplate(pathname);
  if (enterprise) return enterprise;
  // E44 ingest hooks: hook ids in the path collapse to their template.
  if (pathname === "/v1/ingest-hooks" || pathname === "/v1/ingest-hooks/gmail") return pathname;
  const hookDelivery = /^\/v1\/ingest-hooks\/(github|linear|slack|notion|google_calendar)\/[^/]+$/.exec(pathname);
  if (hookDelivery) return `/v1/ingest-hooks/${hookDelivery[1]}/:hook`;
  if (/^\/v1\/ingest-hooks\/[^/]+\/rotate$/.test(pathname)) return "/v1/ingest-hooks/:id/rotate";
  if (/^\/v1\/ingest-hooks\/[^/]+$/.test(pathname)) return "/v1/ingest-hooks/:id";
  if (/^\/v1\/keys\/[^/]+\/revoke$/.test(pathname)) return "/v1/keys/:id/revoke";
  if (/^\/v1\/keys\/[^/]+$/.test(pathname)) return "/v1/keys/:id";
  if (/^\/v1\/receipts\/[^/]+\/dispute$/.test(pathname)) return "/v1/receipts/:id/dispute";
  if (/^\/v1\/ai\/[^/]+$/.test(pathname)) return "/v1/ai/:lane";
  return /^\/v1\/batch\/[^/]+$/.test(pathname) ? "/v1/batch/:id" : "/unmatched";
}

/** E26: the enterprise routes as templates (`enterprise/app.ts`, `enterprise/scim.ts`), or
 * `undefined` when the path is not one of them. */
export function enterpriseRouteTemplate(pathname: string): string | undefined {
  if (["/v1/admin/tenants", "/v1/tenant/me", "/v1/tenant/audit/export", "/v1/tenant/members", "/v1/tenant/logout", "/legal/dpa", "/legal/zdr", "/legal/sla"].includes(pathname)) return pathname;
  if (["/scim/v2/ServiceProviderConfig", "/scim/v2/Schemas", "/scim/v2/ResourceTypes", "/scim/v2/Users", "/scim/v2/Groups"].includes(pathname)) return pathname;
  if (/^\/scim\/v2\/Users\/[^/]+$/.test(pathname)) return "/scim/v2/Users/:id";
  if (/^\/scim\/v2\/Groups\/[^/]+$/.test(pathname)) return "/scim/v2/Groups/:id";
  if (/^\/scim\/v2\/(Schemas|ResourceTypes)\/[^/]+$/.test(pathname)) return pathname.replace(/\/[^/]+$/, "/:id");
  if (/^\/v1\/tenant\/members\/[^/]+\/keys$/.test(pathname)) return "/v1/tenant/members/:id/keys";
  if (/^\/v1\/admin\/tenants\/[^/]+$/.test(pathname)) return "/v1/admin/tenants/:id";
  const admin = /^\/v1\/admin\/tenants\/[^/]+\/(principals|admins|scim-token)$/.exec(pathname);
  if (admin) return `/v1/admin/tenants/:id/${admin[1]}`;
  if (/^\/v1\/admin\/tenants\/[^/]+\/admins\/[^/]+$/.test(pathname)) return "/v1/admin/tenants/:id/admins/:admin";
  const sso = /^\/sso\/[^/]+\/(login|oidc\/callback|saml\/acs|saml\/metadata)$/.exec(pathname);
  if (sso) return `/sso/:tenant/${sso[1]}`;
  return undefined;
}

export function redactingLogger(log: LogFn): MiddlewareHandler<{ Variables: LogVars }> {
  return async (c, next) => {
    const start = Date.now();
    try {
      await next();
    } finally {
      const ms = Date.now() - start;
      const chunks = c.get("relayLogChunks");
      const reserved = c.get("relayLogReservedChunks");
      const license = c.get("relayLogLicense");
      const agentKey = c.get("relayLogAgentKey");
      const model = c.get("relayLogModel");
      const event = c.get("relayLogEvent");
      const extras = [
        event ? `event=${event}` : null,
        typeof reserved === "number" ? `reserved_chunks=${reserved}` : null,
        typeof chunks === "number" ? `chunks=${chunks}` : null,
        typeof model === "string" ? `provider=${model.startsWith("gpt-") ? "openai" : model.startsWith("jev-") ? "typesafe" : "anthropic"} model=${model}` : null,
        typeof license === "string" ? `license=${logId(license)}` : null,
        typeof agentKey === "string" ? `key=${logId(agentKey)}` : null,
      ]
        .filter((v): v is string => v !== null)
        .join(" ");
      // Path only — never the body, never a header value.
      const route = routeTemplate(new URL(c.req.url).pathname);
      const method = ["GET", "POST", "HEAD", "OPTIONS", "PUT", "PATCH", "DELETE"].includes(c.req.method) ? c.req.method : "OTHER";
      log(`${method} ${route} ${c.res.status} ${ms}ms${extras ? " " + extras : ""}`);
    }
  };
}
