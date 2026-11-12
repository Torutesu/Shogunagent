/**
 * `run(tool_id, input, idempotency_key, dryRun?)` (E8-T5; implementation-plan §7.4). The engine:
 *
 *   lookup → scope (L1/L2/L3, deny unknown) → validate input → quote (dryRun stops here)
 *   → identify actor → idempotency replay → L2: input_required / L3: approval URL
 *   → claim the idempotency key (E48: atomic, across isolates) → reserve
 *   → execute through the transport → settle (failures and empty results cost 0)
 *   (E32: in production the reservation and the settlement are apps/api's wallet, src/wallet.ts,
 *   which charges the caller once: provider price + one take; the router adds no price of its own)
 *   → map output → mark the key done (before the settlement) → hints → run record → respond.
 *
 * A failed or refused run releases its claim (nothing was charged), so the same key can be retried.
 * The idempotency store keeps the run id and the charged usage, never the result
 * (src/idempotency.ts): a replay answers `resultType: "replay"` with the run id.
 *
 * The engine never sees a credential: the bearer is a `Secret` it forwards opaquely to metering
 * and (for `caller_bearer` providers only) to the transport; provider keys never enter this file.
 * The logger receives tool ids, outcomes and numbers; the run record has no body field.
 */
import type { EndpointDefinition, Json, RunRequest } from "../connectors/types.js";
import type { CompiledCatalog } from "./catalog.js";
import { contentHash } from "./hash.js";
import type { HealthStore } from "./health.js";
import { betterFitAlternatives, cheaperAlternatives, type Hint } from "./hints.js";
import { CLAIM_STALE_MS, idempotencyKeyHash, requestFingerprint, type IdempotencyRecord, type IdempotencyStore } from "./idempotency.js";
import type { Logger } from "./logging.js";
import type { MeteringClient } from "./metering.js";
import { ERROR_STATUS, errorBody, type ErrorBody, type NextAction } from "./responses.js";
import type { PersonalRanker } from "./personal.js";
import { clampTaskHint, type RunLogSink, type RunOutcome } from "./runlog.js";
import { authorize, type Level } from "./scope.js";
import type { Secret } from "./secret.js";
import { buildSettlement, type SettledOutcome } from "./settlement.js";
import { callerPrice, RUN_MARKUP_BPS } from "./take.js";
import { TransportError, type Transport } from "./transport.js";
import { settleUsage, type Usage, ZERO_USAGE } from "./usage.js";
import type { RunVerbInput } from "./verbs.js";

export interface RunDeps {
  catalog: CompiledCatalog;
  transport: Transport;
  metering: MeteringClient;
  runLog: RunLogSink;
  /**
   * The personal learner (E20): receives the same run records as `runLog` and keeps each
   * principal's tool × task-cluster outcomes in that principal's store. Optional.
   */
  personal?: PersonalRanker;
  idempotency: IdempotencyStore;
  health: HealthStore;
  logger: Logger;
  /** Where L3 approvals are opened. The E8 skeleton only mints the URL; the approval queue is E4/E9. */
  approvalBaseUrl: string;
  now?: () => number;
  randomId?: () => string;
}

export interface RunCall {
  input: RunVerbInput;
  bearer?: Secret;
}

interface Common {
  tool_id: string;
  permission_level: Level;
  usage: Usage;
  latency_ms: number;
  hints: Hint[];
  next_action: NextAction;
}

export interface RunOkBody extends Common {
  resultType: "result";
  result: Json;
  run_id: string;
  replayed: boolean;
}

/**
 * A repeated idempotency key whose run already completed (E48). Nothing ran and nothing was charged
 * now. The result body is not stored anywhere outside the principal's store (invariant 2), so it is
 * not repeated: `original` says how the run ended and what it cost; run again with a new key to get
 * a fresh result.
 */
export interface RunReplayBody extends Common {
  resultType: "replay";
  run_id: string;
  replayed: true;
  original: { outcome: "ok" | "empty"; usage: Usage; completed_at: number };
  note: string;
}

export interface DryRunBody extends Common {
  resultType: "quote";
  dryRun: true;
  /** `estimate_micro_usd` is what the caller pays for a typical run: `provider_micro_usd` plus one
   * take (`take_bps`, 0 for first-party tools). */
  quote: { estimate_micro_usd: number; provider_micro_usd: number; take_bps: number; unit: string; model: string; failures_cost: 0; empty_results_cost: 0 };
}

/** MCP "input required" shape (E4-T2 MRTR): the client answers with `inputResponses`. */
export interface InputRequiredBody extends Common {
  resultType: "input_required";
  inputRequests: { id: string; message: string; schema: Json }[];
  retry: { with: "inputResponses"; example: { confirm: true } };
}

export interface ApprovalRequiredBody extends Common {
  resultType: "approval_required";
  approval: { approval_id: string; url: string; mode: "url"; expires_at: number };
}

export type RunBody = RunOkBody | RunReplayBody | DryRunBody | InputRequiredBody | ApprovalRequiredBody | ErrorBody;

export interface RunResult {
  status: number;
  body: RunBody;
}

export function defaultRequest(endpoint: EndpointDefinition, input: Json): RunRequest {
  const base: RunRequest = { method: endpoint.request.method, path: endpoint.request.path };
  if (endpoint.request.timeout_ms !== undefined) base.timeout_ms = endpoint.request.timeout_ms;
  if (endpoint.request.method === "GET") {
    const query: Record<string, string> = {};
    if (input && typeof input === "object" && !Array.isArray(input)) {
      for (const [k, v] of Object.entries(input)) if (v !== undefined && v !== null) query[k] = typeof v === "string" ? v : JSON.stringify(v);
    }
    return { ...base, query };
  }
  return { ...base, body: input };
}

function randomIdDefault(): string {
  return crypto.randomUUID();
}

export async function run(deps: RunDeps, call: RunCall): Promise<RunResult> {
  const now = deps.now ?? Date.now;
  const randomId = deps.randomId ?? randomIdDefault;
  const start = now();
  const { input: verb } = call;
  const toolId = verb.tool_id;
  const taskHint = clampTaskHint(verb.task_hint);
  const hintFollowed = verb.hint_followed === true;
  let actor = "anonymous";

  const finish = async (outcome: RunOutcome, cost: number, status: number, body: RunBody): Promise<RunResult> => {
    const latency = now() - start;
    body.latency_ms = latency;
    const record = { ts: start, actor, tool_id: toolId, task_hint: taskHint, outcome, cost_micro_usd: cost, latency_ms: latency, hint_followed: hintFollowed };
    await deps.runLog.record(record);
    if (deps.personal) await deps.personal.record(record);
    deps.logger.log({ kind: "run", tool_id: toolId, provider_id: deps.catalog.get(toolId)?.provider_id ?? "unknown", outcome, status, latency_ms: latency });
    return { status, body };
  };
  const fail = (code: ErrorBody["error"]["code"], message: string, next: NextAction, outcome: RunOutcome, extra?: Parameters<typeof errorBody>[4]) =>
    finish(outcome, 0, ERROR_STATUS[code], errorBody(code, message, next, now() - start, extra));

  // 1. lookup + scope. An operation not in the table is denied, never defaulted.
  const entry = deps.catalog.get(toolId);
  const auth = authorize(deps.catalog.scope, toolId);
  if (!entry || auth.kind !== "allowed") {
    return entry
      ? fail("operation_denied", `"${toolId}" is not in the permission table`, "discover", "denied")
      : fail("tool_not_found", `no tool "${toolId}" in the catalog; call discover with the task`, "discover", "denied");
  }
  const def = deps.catalog.lookup(toolId);
  if (!def) return fail("tool_not_found", `no definition behind "${toolId}"`, "discover", "denied");
  const level = auth.level;
  const hints = (): Hint[] => [...cheaperAlternatives(deps.catalog, entry), ...(taskHint ? betterFitAlternatives(deps.catalog, entry, taskHint) : [])];

  // 2. validate input.
  const validated = def.endpoint.input.schema.validate(verb.input);
  if (!validated.ok) {
    return fail("invalid_input", `input does not match the schema of "${toolId}"; call inspect for the schema`, "fix_input", "invalid_input", { details: validated.errors });
  }
  const input = validated.value;
  const quote = { tool_id: toolId, estimate_micro_usd: entry.quote_micro_usd, provider_micro_usd: entry.provider_quote_micro_usd, first_party: entry.first_party, unit: entry.usage.unit };

  // 3. dryRun: the quote and the permission level, nothing else.
  if (verb.dryRun) {
    const body: DryRunBody = {
      resultType: "quote",
      dryRun: true,
      tool_id: toolId,
      permission_level: level,
      quote: {
        estimate_micro_usd: quote.estimate_micro_usd, provider_micro_usd: quote.provider_micro_usd, take_bps: entry.first_party ? 0 : RUN_MARKUP_BPS,
        unit: quote.unit, model: entry.usage.model, failures_cost: 0, empty_results_cost: 0,
      },
      usage: ZERO_USAGE,
      latency_ms: 0,
      hints: hints(),
      next_action: level === "L3" ? "approve_at_url" : level === "L2" ? "provide_input" : null,
    };
    return { status: 200, body: { ...body, latency_ms: now() - start } };
  }

  // 4. identity.
  const identity = await deps.metering.identify(call.bearer);
  if ("error" in identity) return fail("authentication_required", "the bearer is not a valid agent key", "authenticate", "authentication_required");
  actor = identity.actor;

  // 5. idempotency replay. Only the key's hash and a salted fingerprint of the request are compared.
  const keyHash = idempotencyKeyHash(verb.idempotency_key);
  const fingerprint = requestFingerprint(actor, keyHash, toolId, contentHash(input));
  // A store outage may let a free L1 read through without idempotency (running it twice is harmless
  // and costs nothing); anything that charges or writes is refused instead (fails closed).
  const mayBypassStore = level === "L1" && entry.quote_micro_usd === 0;
  const storeDown = () => {
    deps.logger.log({ kind: "run", tool_id: toolId, outcome: "idempotency_unavailable" });
    return fail("provider_unavailable", "the idempotency store is unavailable; nothing ran and nothing was charged", "retry_after", "denied", { retry_after: 5 });
  };
  let bypass = false;
  let stored: IdempotencyRecord | undefined;
  try {
    stored = await deps.idempotency.get(actor, keyHash, now());
  } catch {
    if (!mayBypassStore) return storeDown();
    bypass = true;
    deps.logger.log({ kind: "run", tool_id: toolId, outcome: "idempotency_bypassed" });
  }
  const answerExisting = (rec: IdempotencyRecord): Promise<RunResult> => {
    if (rec.tool_id !== toolId || rec.fingerprint !== fingerprint) {
      return fail("idempotency_conflict", "this idempotency_key was used with a different tool or input; generate a new key", "fix_input", "denied");
    }
    if (rec.state === "running") {
      const remainingS = Math.ceil((rec.claimed_at + CLAIM_STALE_MS - now()) / 1000);
      return fail("idempotency_in_progress", "a run with this idempotency_key is still in progress; retry after the given seconds and the same key answers with its outcome", "retry_after", "denied", {
        retry_after: Math.max(1, Math.min(30, remainingS)),
      });
    }
    const body: RunReplayBody = {
      resultType: "replay",
      tool_id: toolId,
      run_id: rec.run_id,
      permission_level: level,
      replayed: true,
      original: { outcome: rec.outcome ?? "ok", usage: rec.usage ?? ZERO_USAGE, completed_at: rec.completed_at ?? rec.claimed_at },
      note: "this idempotency_key already ran; nothing ran or was charged now. The result was returned once to the original call and is not stored; run again with a new idempotency_key for a fresh result.",
      usage: ZERO_USAGE,
      latency_ms: 0,
      hints: [],
      next_action: null,
    };
    return finish("replayed", 0, 200, body);
  };
  if (stored) return answerExisting(stored);

  // 6. permission tier. L2 asks for confirmation once; L3 always goes to a human URL.
  if (level === "L2" && verb.inputResponses?.["confirm"] !== true) {
    const body: InputRequiredBody = {
      resultType: "input_required",
      tool_id: toolId,
      permission_level: level,
      inputRequests: [
        {
          id: "confirm",
          message: `"${toolId}" writes the user's data (${entry.summary}). Confirm to proceed.`,
          schema: { type: "object", properties: { confirm: { type: "boolean", const: true } }, required: ["confirm"] },
        },
      ],
      retry: { with: "inputResponses", example: { confirm: true } },
      usage: ZERO_USAGE,
      latency_ms: 0,
      hints: [],
      next_action: "provide_input",
    };
    return finish("input_required", 0, 200, body);
  }
  if (level === "L3") {
    const approvalId = randomId();
    const body: ApprovalRequiredBody = {
      resultType: "approval_required",
      tool_id: toolId,
      permission_level: level,
      approval: { approval_id: approvalId, url: `${deps.approvalBaseUrl.replace(/\/$/, "")}/${approvalId}`, mode: "url", expires_at: start + 15 * 60 * 1000 },
      usage: ZERO_USAGE,
      latency_ms: 0,
      hints: [],
      next_action: "approve_at_url",
    };
    return finish("approval_required", 0, 200, body);
  }

  // 7. a provider that reads the user's own state, or acts as the user's own account at a vendor,
  // needs the agent's own bearer; without one there is nothing to forward, so stop before reserving
  // or calling out.
  if (def.provider.auth.kind === "caller_bearer" && !call.bearer) {
    return fail("authentication_required", "this tool reads the user's own state; send your agent key as the bearer", "authenticate", "authentication_required");
  }
  if (def.provider.auth.kind === "caller_connector" && !call.bearer) {
    return fail("authentication_required", "this tool acts as the user's own account at the vendor; send your agent key as the bearer", "authenticate", "authentication_required");
  }
  // The run id names the result and is the settlement's idempotency key (E29) and, through the
  // receipt id derived from it, the wallet's (E32). The draft is what settles if the run is
  // abandoned after the hold: $0, transport_error (src/outbox.ts).
  const runId = randomId();

  // 7b. claim the key (E48). Atomic in the store, so of two isolates racing on one key exactly one
  // runs; the other answers from the record. Everything after this either completes the key or
  // releases it.
  if (!bypass) {
    let won = false;
    try {
      won = await deps.idempotency.claim(actor, keyHash, { tool_id: toolId, fingerprint, run_id: runId }, now());
    } catch {
      if (!mayBypassStore) return storeDown();
      bypass = true;
      deps.logger.log({ kind: "run", tool_id: toolId, outcome: "idempotency_bypassed" });
    }
    if (!won && !bypass) {
      let rec: IdempotencyRecord | undefined;
      try {
        rec = await deps.idempotency.get(actor, keyHash, now());
      } catch {
        return storeDown();
      }
      // Lost the race to a claim that has since been released: say so rather than run unclaimed.
      if (!rec) return fail("idempotency_in_progress", "another run with this idempotency_key just finished or failed; retry", "retry_after", "denied", { retry_after: 1 });
      return answerExisting(rec);
    }
  }
  let completed = false;
  const release = async (): Promise<void> => {
    if (bypass || completed) return;
    await deps.idempotency.release(actor, keyHash, runId).catch(() => deps.logger.log({ kind: "run", tool_id: toolId, outcome: "idempotency_release_failed" }));
  };
  const execute = async (): Promise<RunResult> => {
    const settlementOf = (receiptId: string | null, outcome: SettledOutcome, cost: number, quantity: number) =>
      buildSettlement({ runId, receiptId, ts: start, toolId, provider: def.provider, endpoint: def.endpoint, outcome, quantity, costMicroUsd: cost });
    const reservation = await deps.metering.reserve(identity, quote, { runId, ...(call.bearer ? { bearer: call.bearer } : {}), draft: settlementOf(null, "transport_error", 0, 0) });
    if (!reservation.ok) {
      if (reservation.status === 401) return fail("authentication_required", "this tool is metered; send a valid agent key with the run scope as the bearer", "authenticate", "authentication_required");
      if (reservation.status === 503) return fail("provider_unavailable", "the wallet is unavailable; nothing was charged and the provider was not called", "retry_after", "denied", { retry_after: reservation.retry_after });
      if (reservation.status === 429) return fail("rate_limited", "the wallet is rate limiting this key; nothing was charged and the provider was not called; retry after the given seconds", "retry_after", "denied", { retry_after: reservation.retry_after });
      if (reservation.error === "cap_exceeded") return fail("payment_required", "this run would cross a hard spending cap; wait for the period to reset", "retry_after", "payment_required", { payment: reservation.payment, hints: hints() });
      return fail("payment_required", "insufficient balance for this run", "top_up", "payment_required", { payment: reservation.payment, hints: hints() });
    }

    // Every run that reserved settles once, failures and empty results at $0 included. `cost` is the
    // provider's measured price; the caller is charged it plus one take (0 when not ok).
    const settle = (outcome: SettledOutcome, cost: number, quantity: number): Promise<void> =>
      deps.metering.settle(reservation.reservation_id, outcome === "ok" ? callerPrice(cost, def.provider.first_party) : 0, settlementOf(reservation.receipt_id, outcome, cost, quantity));

    // 8. execute. The transport injects the provider credential; the engine never holds it.
    const request = def.endpoint.input.toRequest ? def.endpoint.input.toRequest({ input, request: { method: def.endpoint.request.method, path: def.endpoint.request.path } }) : defaultRequest(def.endpoint, input);
    const providerRef = { id: def.provider.id, auth: def.provider.auth, baseUrl: def.provider.request.baseUrl, ...(def.provider.request.baseUrlEnv ? { baseUrlEnv: def.provider.request.baseUrlEnv } : {}) };
    const authKind = def.provider.auth.kind;
    const callerForwarding = (authKind === "caller_bearer" || authKind === "caller_connector") && call.bearer ? { caller: { bearer: call.bearer } } : {};
    let response;
    const execStart = now();
    try {
      response = await deps.transport.execute({ provider: providerRef, request, runId, ...callerForwarding });
    } catch (e) {
      await settle("transport_error", 0, 0);
      if (e instanceof TransportError) {
        // E48: a missing or refused per-caller credential says nothing about the provider's health.
        if (e.code === "connector_not_connected") {
          return fail("connector_not_connected", `this tool acts as the user's own ${def.provider.name} account and none is connected for this principal; connect it in ShogunAgent, then retry. Nothing was charged`, "authenticate", "transport_error", { hints: hints() });
        }
        if (e.code === "caller_unauthorized") return fail("authentication_required", "your agent key may not use this connector; send a key with the run scope for this principal", "authenticate", "authentication_required");
        if (e.code === "connector_unconfigured") return fail("provider_unavailable", "this router has no per-caller connector credentials configured; nothing was charged", "try_alternative", "transport_error", { hints: hints() });
        if (e.code === "connector_token_unavailable") return fail("provider_unavailable", "the connector credential service is unavailable; nothing was charged and the provider was not called", "retry_after", "transport_error", { retry_after: 5 });
      }
      deps.health.observe(toolId, { latency_ms: now() - execStart, ok: false });
      if (e instanceof TransportError) {
        if (e.code === "caller_bearer_missing") return fail("provider_unavailable", "this tool reads the user's own state; send your agent key as the bearer", "authenticate", "transport_error");
        if (e.code === "credential_missing") return fail("provider_unavailable", "the provider is not configured on this router", "try_alternative", "transport_error", { hints: hints() });
        return fail("upstream_error", e.code === "timeout" ? "the provider did not answer in time" : "the provider is unreachable", "retry_after", "transport_error", { hints: hints() });
      }
      return fail("upstream_error", "the provider call failed", "retry_after", "transport_error", { hints: hints() });
    }
    const execLatency = now() - execStart;
    const failed = def.endpoint.output.isFailure ? def.endpoint.output.isFailure(response) : response.status >= 400;
    if (failed && response.status === 404) {
      // The provider answered: what the input names does not exist (an unknown id). The caller
      // fixes the input; retrying the same call cannot help, and the provider is healthy.
      await settle("provider_error", 0, 0);
      deps.health.observe(toolId, { latency_ms: execLatency, ok: true });
      return fail("not_found", "the provider found nothing for this input; check the identifiers it names; nothing was charged", "fix_input", "provider_error");
    }
    if (failed) {
      await settle("provider_error", 0, 0);
      deps.health.observe(toolId, { latency_ms: execLatency, ok: false });
      const h = hints();
      return fail("upstream_error", `the provider answered with status ${response.status}; nothing was charged`, h.length ? "try_alternative" : "retry_after", "provider_error", { hints: h });
    }

    // 9. map, mark the key done, settle, record. The key is marked done BEFORE the settlement: a crash
    // between the two leaves the draft to release the hold at $0 (an under-charge), whereas the
    // other order could let a retry run and charge again.
    const output: Json = def.endpoint.output.fromResponse ? def.endpoint.output.fromResponse({ input, response }) : response.body;
    const measured = settleUsage(def.endpoint, { input, response, output });
    // What the agent sees is what it pays: the provider's measured price plus one take.
    const usage: Usage = { ...measured, cost_micro_usd: measured.quantity === 0 ? 0 : callerPrice(measured.cost_micro_usd, def.provider.first_party) };
    if (!bypass) {
      try {
        await deps.idempotency.complete(actor, keyHash, runId, usage.quantity === 0 ? "empty" : "ok", usage, now());
        completed = true;
      } catch {
        // The claim stays `running` and turns stale after CLAIM_STALE_MS; a retry before then answers
        // in progress. Never released here: the provider already ran.
        completed = true;
        deps.logger.log({ kind: "run", tool_id: toolId, outcome: "idempotency_complete_failed" });
      }
    }
    await settle(measured.quantity === 0 ? "empty" : "ok", measured.cost_micro_usd, measured.quantity);
    deps.health.observe(toolId, { latency_ms: execLatency, ok: true });
    const body: RunOkBody = {
      resultType: "result",
      result: output,
      tool_id: toolId,
      run_id: runId,
      permission_level: level,
      replayed: false,
      usage,
      latency_ms: 0,
      hints: hints(),
      next_action: null,
  };
  return finish(usage.quantity === 0 ? "empty" : "ok", usage.cost_micro_usd, 200, body);
  };
  try {
    const result = await execute();
    if (!completed) await release();
    return result;
  } catch (e) {
    await release();
    throw e;
  }
}
