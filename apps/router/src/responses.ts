/**
 * Response shapes shared by the three verbs (implementation-plan §7.2 / §7.3). Every response is
 * self-describing: `usage`, `latency_ms`, `hints`, and on error `next_action` saying what to do.
 */
import type { Hint } from "./hints.js";
import type { PaymentRequired } from "./metering.js";
import type { Usage } from "./usage.js";

export type NextAction =
  | "top_up"
  | "approve_at_url"
  | "provide_input"
  | "retry_after"
  | "fix_input"
  | "broaden_query"
  | "discover"
  | "authenticate"
  | "try_alternative"
  | null;

export type ErrorCode =
  | "invalid_request"
  | "tool_not_found"
  | "operation_denied"
  | "invalid_input"
  | "idempotency_conflict"
  | "idempotency_in_progress"
  | "connector_not_connected"
  | "rate_limited"
  | "authentication_required"
  | "payment_required"
  | "provider_unavailable"
  | "not_found"
  | "upstream_error";

export interface ErrorBody {
  resultType: "error";
  error: { code: ErrorCode; message: string; next_action: NextAction };
  usage: Usage;
  latency_ms: number;
  hints: Hint[];
  payment?: PaymentRequired;
  details?: string[];
  /** Seconds to wait before retrying, when the router knows (a wallet 503/429, a run in progress).
   * The REST and MCP faces also send it as the `Retry-After` header. */
  retry_after?: number;
}

export interface Envelope<T> {
  result: T;
  usage: Usage;
  latency_ms: number;
  hints: Hint[];
  next_action: NextAction;
}

export const FREE_USAGE: Usage = { unit: "call", quantity: 1, cost_micro_usd: 0 };

export function errorBody(code: ErrorCode, message: string, next_action: NextAction, latency_ms: number, extra: Partial<Pick<ErrorBody, "hints" | "payment" | "details" | "retry_after">> = {}): ErrorBody {
  const body: ErrorBody = {
    resultType: "error",
    error: { code, message, next_action },
    usage: { unit: "call", quantity: 0, cost_micro_usd: 0 },
    latency_ms,
    hints: extra.hints ?? [],
  };
  if (extra.payment) body.payment = extra.payment;
  if (extra.details) body.details = extra.details;
  if (extra.retry_after !== undefined && Number.isFinite(extra.retry_after) && extra.retry_after > 0) body.retry_after = Math.ceil(extra.retry_after);
  return body;
}

export const ERROR_STATUS: Record<ErrorCode, number> = {
  invalid_request: 400,
  tool_not_found: 404,
  operation_denied: 403,
  invalid_input: 400,
  idempotency_conflict: 409,
  idempotency_in_progress: 409,
  connector_not_connected: 403,
  rate_limited: 429,
  authentication_required: 401,
  payment_required: 402,
  provider_unavailable: 503,
  not_found: 404,
  upstream_error: 502,
};

/** The `Retry-After` header value for a verb's answer, when its body carries one. */
export function retryAfterOf(body: unknown): string | undefined {
  if (!body || typeof body !== "object") return undefined;
  const r = body as { resultType?: unknown; retry_after?: unknown };
  return r.resultType === "error" && typeof r.retry_after === "number" && r.retry_after > 0 ? String(Math.ceil(r.retry_after)) : undefined;
}
