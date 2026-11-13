/**
 * The rate table (docs/implementation-plan.md E9-T5) and the one place a call is classified
 * into a metering unit. `/.well-known/pricing.json` (E10-T7) is rendered FROM this module
 * (`pricingDocument`), so the published prices and the enforced prices are one source
 * (CLAUDE.md invariant 12).
 *
 * Units (amounts are integer micro-USD, 1 USD = 1,000,000):
 * - `ingest`   — $0.50 per 1,000 events (500 µ$ per event): `memory.ingest`, `memory.append_note`,
 *                `handoff.accept`, `memory.ingest_webhook` (E44: a provider delivery through the
 *                ingest hooks, ingest-hooks/app.ts).
 * - `model_op` — $0.10 per 1,000 model-invoking operations (100 µ$ each): the default for any
 *                `tools/call` the table does not list as free, ingest or run.
 * - `run`      — a router tool run: the provider's price plus `RUN_MARKUP_BPS` (5%), rounded up, once
 *                (first-party tools: their listed price, no take). The provider price is supplied
 *                by the router (never by the client); the router never adds a take of its own
 *                (router-wallet.ts, E32).
 * - `storage`  — $0.10 per GB-month of stored state, booked by the storage sweep (not per call).
 * - `read`     — $0 (CLAUDE.md invariant 10): `state.*`, `memory.get_context_pack`,
 *                `memory.recall`, `memory.search`, approvals and the other reads. Counted, never
 *                billed.
 * - `protocol` — $0: MCP requests that are not `tools/call` (initialize, tools/list, ping…).
 *
 * Classification reads only the `Mcp-Method` / `Mcp-Name` headers of the MCP 2026-07-28
 * Streamable HTTP transport and an optional quantity header — never the body.
 */

import sharedPricing from "../../../packages/config/pricing.json";

export type MeterUnit = "ingest" | "model_op" | "run" | "storage" | "read" | "protocol";

export const MICRO_USD_PER_USD = 1_000_000;

/** µ$ per unit for the flat units. */
export const RATE_MICROS: Readonly<Record<Exclude<MeterUnit, "run">, number>> = Object.freeze({
  ingest: 500,
  model_op: 100,
  storage: 100_000,
  read: 0,
  protocol: 0,
});

/** The take on a provider's price for a third-party `run`, in basis points (500 = 5%). Set in ONE
 * place, `packages/config/pricing.json`, which the router reads too (apps/router/src/take.ts) for
 * the prices it quotes; the wallet that charges is this module. First-party runs carry none. */
export const RUN_MARKUP_BPS: number = sharedPricing.run_markup_bps;

/** The free allowance granted to a self-registered principal (E3-T3): $5.00. */
export const FREE_ALLOWANCE_MICROS = 5 * MICRO_USD_PER_USD;

/** The MCP method a tool invocation travels on. Anything else is protocol traffic. */
export const TOOLS_CALL = "tools/call";

/** Reads: free, by name. The core 22's read set (§7.1) plus approvals and the router's
 * `discover` / `inspect`, which are free by the plan (§7.4). `state.*` matches by prefix. */
export const FREE_TOOLS: ReadonlySet<string> = new Set([
  "memory.search", "memory.get_context", "memory.get_context_pack", "memory.get_events",
  "memory.read_session", "memory.get_wrap", "memory.get_periods", "memory.recall", "memory.processing_status",
  "calendar.upcoming", "profile.whoami", "wiki.evidence", "actions.status",
  "approvals.list", "approvals.get", "approvals.decide",
  "router.discover", "router.inspect",
  // E35: the A2A handoff object is one recall, reshaped by the core.
  "handoff.create",
  // E46: the payment mandates and what was spent against them.
  "payments.mandates.list",
  // E51: the principal's connections (status only; docs/api/connector-tokens.md calls it a free
  // read on every face, and the SDKs' connections.status() reaches it over /mcp).
  "connections.list",
]);
const FREE_PREFIXES = ["state.", "approvals."] as const;

/** E35: `handoff.accept` stores the handoff in the receiver's scratch through the ingest path,
 * settled per event the core stored (its `Mcp-Quantity`). */
export const INGEST_TOOLS: ReadonlySet<string> = new Set(["memory.ingest", "memory.append_note", "handoff.accept", "memory.ingest_webhook"]);

/** E46: `payments.spend` is a run of the payment rail; asking for or revoking a mandate needs the
 * same `run` scope (an agent that may spend), free at the call. */
export const RUN_TOOLS: ReadonlySet<string> = new Set([
  "router.run", "actions.execute",
  "payments.mandates.request", "payments.mandates.revoke", "payments.spend",
]);

/** Run tools whose call is free (E34, invariant 10): `actions.execute` runs `local_search` (a
 * read), refuses the desktop-only kinds, and only QUEUES a send / post / comment for a human's
 * approval (L3). Nothing billable happens at the call; the external send, when a human approves
 * it, is carried out by the principal's core with the principal's own connector credentials, and
 * is billed where it executes (a router run, when it goes through the router, E32). Classified as a
 * `run` still, so it needs the `run` scope; priced at $0 instead of asking the router. */
export const FREE_RUN_TOOLS: ReadonlySet<string> = new Set([
  "actions.execute",
  // E46 (docs/api/payments.md): a mandate request queues an L3 for a human and a revoke only
  // narrows; neither runs anything billable. `payments.spend` is priced at the rail's fee, which
  // is 0 while the only rail is the stub (no money moves). A real rail removes it from this set
  // and its fee is the run's provider price. The gateway never reads or stores a payment body.
  "payments.mandates.request", "payments.mandates.revoke", "payments.spend",
]);

/** The provider price (µ$) a `run` call has without asking the router: 0 for a free run tool. */
export function fixedRunPriceMicros(name: string | undefined): number | undefined {
  return name !== undefined && FREE_RUN_TOOLS.has(name) ? 0 : undefined;
}

/** Classify one MCP request by its method and (for `tools/call`) tool name. */
export function classifyCall(method: string | undefined, name: string | undefined): MeterUnit {
  if (method !== TOOLS_CALL) return "protocol";
  if (!name) return "model_op";
  if (FREE_TOOLS.has(name) || FREE_PREFIXES.some((p) => name.startsWith(p))) return "read";
  if (INGEST_TOOLS.has(name)) return "ingest";
  if (RUN_TOOLS.has(name)) return "run";
  return "model_op";
}

/** A priced call: what the receipt records and what the 402 quotes. */
export interface Quote {
  unit: MeterUnit;
  quantity: number;
  cost_micro_usd: number;
}

/** The cost of `quantity` units. For `run` the provider price (µ$, for the whole run) is
 * required; without it the call cannot be priced and the caller must refuse it. Integer, rounded
 * up, never negative. Throws only on a programming error (a `run` without a price). */
export function quoteCost(unit: MeterUnit, quantity: number, providerPriceMicros?: number): Quote {
  const qty = Number.isSafeInteger(quantity) && quantity > 0 ? quantity : 1;
  if (unit === "run") {
    if (providerPriceMicros === undefined) throw new Error("run price missing");
    const base = Number.isSafeInteger(providerPriceMicros) && providerPriceMicros > 0 ? providerPriceMicros : 0;
    return { unit, quantity: qty, cost_micro_usd: Math.ceil((base * (10_000 + RUN_MARKUP_BPS)) / 10_000) * qty };
  }
  return { unit, quantity: qty, cost_micro_usd: RATE_MICROS[unit] * qty };
}

/** The `usage` field of the plan's common response (§7.2). */
export function usageOf(quote: Quote): { unit: MeterUnit; quantity: number; cost_micro_usd: number } {
  return { unit: quote.unit, quantity: quote.quantity, cost_micro_usd: quote.cost_micro_usd };
}

/** What `pricingDocument` needs beyond the rate table: the caps in force (`caps.ts`). */
export interface PricingContext {
  caps: { key: { day: number; month: number }; principal: { day: number; month: number } };
}

/** The body of `/.well-known/pricing.json`: the table above, rendered. Agent-readable: every
 * number is an integer µ$ and every unit says what one of it is. */
export function pricingDocument(ctx: PricingContext): Record<string, unknown> {
  return {
    schema: "shogunagent/pricing/v1",
    currency: "USD",
    amount_unit: "micro_usd",
    micro_usd_per_usd: MICRO_USD_PER_USD,
    units: {
      ingest: { per: "event", micro_usd: RATE_MICROS.ingest, usd_per_1000: RATE_MICROS.ingest * 1000 / MICRO_USD_PER_USD, tools: [...INGEST_TOOLS] },
      model_op: { per: "operation", micro_usd: RATE_MICROS.model_op, usd_per_1000: RATE_MICROS.model_op * 1000 / MICRO_USD_PER_USD, tools: "any tools/call not listed under read, ingest or run" },
      run: { per: "run", micro_usd: null, formula: `ceil(provider_price_micro_usd * (1 + ${RUN_MARKUP_BPS / 10_000}))`, markup_bps: RUN_MARKUP_BPS, first_party_markup_bps: 0, charged: "once, by the wallet, on the provider's measured price; failures and empty results 0", tools: [...RUN_TOOLS], free_at_call: { tools: [...FREE_RUN_TOOLS], why: "actions.execute and payments.mandates.request queue an L3 action for a human's approval (free, invariant 10) or run a read; payments.mandates.revoke only narrows; payments.spend is priced at the payment rail's fee, 0 while the rail is the stub" } },
      storage: { per: "gb_month", micro_usd: RATE_MICROS.storage, billed: "by the storage sweep, not per call" },
      read: { per: "call", micro_usd: 0, tools: [...FREE_TOOLS, ...FREE_PREFIXES.map((p) => `${p}*`)] },
      protocol: { per: "request", micro_usd: 0, methods: "every MCP method other than tools/call" },
    },
    free_allowance_micro_usd: FREE_ALLOWANCE_MICROS,
    caps: {
      key: { day_micro_usd: ctx.caps.key.day, month_micro_usd: ctx.caps.key.month },
      principal: { day_micro_usd: ctx.caps.principal.day, month_micro_usd: ctx.caps.principal.month },
    },
    headers: {
      classify: ["Mcp-Method", "Mcp-Name"],
      quantity: "Mcp-Quantity (events for ingest; default 1)",
      dry_run: "Mcp-Dry-Run: true (quote only, nothing reserved)",
    },
    payment_required: { status: 402, header: "PAYMENT-REQUIRED", doc: "docs/api/402.md" },
  };
}
