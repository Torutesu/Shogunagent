/**
 * The hosted `/mcp` relay (docs/implementation-plan.md E34, gateway half): the metered proxy
 * (`metering.ts`) authenticates the agent, checks scopes and reserves; this module then hands the
 * Streamable HTTP request to the principal's Rust sidecar (`shogun-api --multi`, `POST /mcp`) and
 * hands the reply back as a stream.
 *
 * Bodies (CLAUDE.md invariants 1 and 2): the request body is passed to `fetch` as the incoming
 * stream and the reply body is returned as the sidecar's stream. Neither is read, parsed, copied,
 * buffered, logged or stored here. Only headers are looked at, from two fixed allowlists.
 *
 * Which store: the sidecar resolves a bearer to a principal (`SHOGUN_PRINCIPAL_TOKENS`, or the
 * gateway-derived bearer below). The agent's own key never reaches the sidecar; the gateway swaps
 * it for the principal's sidecar bearer:
 *
 * - `SHOGUN_SIDECAR_PRINCIPAL_TOKENS` — the same `principal:token[:agent_id],…` list the sidecar
 *   is given as `SHOGUN_PRINCIPAL_TOKENS`. Works with today's sidecar; for a fixed set of
 *   principals (BYOC, evaluation).
 * - `SHOGUN_SIDECAR_BEARER_SECRET` — a shared secret (≥ 32 characters) from which the gateway
 *   derives one bearer per principal and key: `sgw1.<principal_id>.<key_id>.<mac>`, where `mac`
 *   is base64url HMAC-SHA256(secret, "shogun/sidecar-bearer/v1\n<principal_id>\n<key_id>"). The
 *   sidecar verifies the MAC and takes the principal from the token and the actor as
 *   `agent:<key_id>`; this needs the sidecar resolver described in docs/api/billing.md §"Hosted
 *   /mcp" (not in the sidecar yet). A token from the list wins over a derived one.
 *
 * Transport (the seam): the sidecar binds 127.0.0.1 only and refuses a non-loopback `Host`
 * (deploy/README.md §1), so how the gateway reaches it depends on where the gateway runs. The relay
 * only needs a `fetch`-shaped function (`FetchFn`); boot picks one:
 *
 * - **URL** (`SHOGUN_SIDECAR_URL`, global `fetch`): the Node gateway in the sidecar's own pod
 *   (`deploy/helm`, `deploy/compose`: `http://127.0.0.1:7464`), or any private origin the runtime
 *   can reach that forwards to the sidecar's loopback with a loopback `Host`.
 * - **Workers binding** (`SHOGUN_SIDECAR_BINDING=<name>`): the Worker calls `env[<name>].fetch` — a
 *   service binding or a Workers VPC service whose private origin is a Cloudflare Tunnel connector
 *   running inside the sidecar's pod. The URL's origin is then only a label
 *   (`SHOGUN_SIDECAR_URL`, default `http://sidecar.internal`); the path is kept. A binding named in
 *   the config but absent at runtime leaves the relay off (`/mcp` answers 503).
 *
 * With neither, `/mcp` answers `503 upstream_unavailable` with `next_action: retry_after`.
 *
 * Sharding (deploy/README.md §5.2): a principal lives on exactly one replica,
 * `fnv1a64(principal_id) mod replicaCount`. Each gateway talks only to its own pod's loopback
 * sidecar, which creates a store for any principal it is asked about, so with `SHOGUN_SHARD=i/n`
 * a gateway refuses (421 `wrong_shard`) a principal homed on another replica rather than open a
 * stray store for it. Routing to the right pod stays the ingress's job.
 */
import { createHmac } from "node:crypto";

import type { AdmitRefusal, AgentPrincipal } from "./agent-keys.js";

/** The sidecar's transport path. */
export const SIDECAR_MCP_PATH = "/mcp";
/** REST reads used by the router's first-party state connector. These have the same
 * authentication and free-read classification as their MCP tools. */
export const STATE_READ_ROUTES = Object.freeze([
  { method: "get", path: "/v1/state/commitments", tool: "state.commitments.list" },
  { method: "get", path: "/v1/state/open_loops", tool: "state.open_loops.list" },
  { method: "get", path: "/v1/state/people", tool: "state.people.list" },
  // The core's person ids are integers; anything else is not a state read.
  { method: "get", path: "/v1/state/people/:person{[0-9]+}", tool: "state.people.get" },
  { method: "post", path: "/v1/memory/recall", tool: "memory.recall" },
] as const);
/** A route path without its parameter patterns (`/v1/state/people/:person`): the log and
 * OpenAPI template. */
export function routePathTemplate(path: string): string {
  return path.replace(/\{[^}]*\}/g, "");
}
const STATE_READ_PATTERNS = STATE_READ_ROUTES.map((route) => ({
  route,
  pattern: new RegExp(`^${route.path.replace(/:\w+(?:\{([^}]*)\})?/g, (_match, re: string | undefined) => `(?:${re ?? "[^/]+"})`)}$`),
}));
/** The state read `method` and `pathname` match, if any. HEAD is answered by the GET route. */
export function stateReadRouteOf(method: string, pathname: string): (typeof STATE_READ_ROUTES)[number] | undefined {
  const wanted = method.toUpperCase() === "HEAD" ? "GET" : method.toUpperCase();
  return STATE_READ_PATTERNS.find(({ route, pattern }) => route.method.toUpperCase() === wanted && pattern.test(pathname))?.route;
}
/** The sidecar paths the relay serves: the MCP transport, the REST handoff route whose object the
 * core assembles (E35), and the REST face of `payments.*` (E51, docs/api/payments.md). The
 * request's own path picks one; anything else goes to `/mcp`. */
export const SIDECAR_PATHS: readonly string[] = Object.freeze([
  SIDECAR_MCP_PATH, "/v1/handoff",
  "/v1/payments/mandates", "/v1/payments/mandates/list", "/v1/payments/mandates/request", "/v1/payments/mandates/revoke", "/v1/payments/spend",
]);

/** `pathname` with each segment percent-decoded, as the router matches it; undefined when a
 * segment is malformed or decodes to a `/` (it would become another path). */
function decodedPath(pathname: string): string | undefined {
  try {
    const segments = pathname.split("/").map((segment) => decodeURIComponent(segment));
    return segments.some((segment) => segment.includes("/")) ? undefined : segments.join("/");
  } catch {
    return undefined;
  }
}

/** Where and how the relay calls the sidecar for `request`. A state read goes to its own path, in
 * its decoded form (`/v1/state/people/%34%32` is person 42, as the router matched it), and a HEAD
 * on one is sent as the GET the core serves. The other relayed paths keep the request's method;
 * anything else goes to `/mcp`. */
export function sidecarTargetOf(request: Request): { path: string; method: string } {
  try {
    const url = new URL(request.url);
    const decoded = decodedPath(url.pathname);
    if (decoded !== undefined && stateReadRouteOf(request.method, decoded)) {
      return { path: decoded + url.search, method: request.method === "HEAD" ? "GET" : request.method };
    }
    return { path: SIDECAR_PATHS.includes(url.pathname) ? url.pathname : SIDECAR_MCP_PATH, method: request.method };
  } catch {
    return { path: SIDECAR_MCP_PATH, method: request.method };
  }
}
/** How long the sidecar may take to send its reply headers. The body is not under this deadline. */
export const SIDECAR_HEADERS_TIMEOUT_MS = 30_000;
/** The derived bearer's version tag and MAC context. */
export const DERIVED_BEARER_PREFIX = "sgw1";
const DERIVED_CONTEXT = "shogun/sidecar-bearer/v1";

/** Request headers passed to the sidecar. Everything else (cookies, the agent's authorization,
 * forwarding headers, payment headers) stays at the gateway. */
export const FORWARDED_REQUEST_HEADERS: readonly string[] = Object.freeze([
  "content-type", "accept", "mcp-method", "mcp-name", "mcp-protocol-version", "last-event-id", "idempotency-key",
]);
/** Reply headers passed back. `mcp-quantity` is what metering settles from; the approval token is
 * consumed (and removed) by metering. */
export const RELAYED_REPLY_HEADERS: readonly string[] = Object.freeze([
  "content-type", "cache-control", "mcp-method", "mcp-name", "mcp-quantity", "mcp-protocol-version", "allow",
  "retry-after", "x-shogun-approval-token",
  // E44: the JSON-RPC error code of an error reply, so the ingest hooks tell a store fault (retry)
  // from a refused payload (final) without reading the body.
  "mcp-error-code",
]);

const PRINCIPAL_RE = /^[a-z0-9][a-z0-9_-]{0,63}$/;

/** `SHOGUN_SIDECAR_PRINCIPAL_TOKENS` / the sidecar's `SHOGUN_PRINCIPAL_TOKENS`, parsed to
 * principal → token. The agent id is the sidecar's business and is ignored here. Throws a
 * message that names the rule, never a value. */
export function parseSidecarPrincipalTokens(spec: string): Map<string, string> {
  const out = new Map<string, string>();
  const seen = new Set<string>();
  for (const entry of spec.split(",").map((e) => e.trim()).filter((e) => e.length > 0)) {
    const [principal = "", token] = entry.split(":");
    if (token === undefined) throw new Error("each entry is principal:token[:agent_id]");
    if (!PRINCIPAL_RE.test(principal)) throw new Error("a principal id is 1-64 characters of [a-z0-9_-]");
    if (token.length < 8 || /\s/.test(token)) throw new Error("a token is at least 8 characters without whitespace");
    if (seen.has(token)) throw new Error("two entries share a token");
    if (out.has(principal)) throw new Error("a principal is listed twice");
    seen.add(token);
    out.set(principal, token);
  }
  return out;
}

/** The derived sidecar bearer of one principal and key (see the module comment). `undefined` when
 * either id cannot travel inside the token. */
export function derivedSidecarBearer(secret: string, principal: Pick<AgentPrincipal, "principalId" | "keyId">): string | undefined {
  const idOk = (s: string): boolean => /^[A-Za-z0-9_-]{1,64}$/.test(s);
  if (!PRINCIPAL_RE.test(principal.principalId) || !idOk(principal.keyId)) return undefined;
  const mac = createHmac("sha256", secret).update(`${DERIVED_CONTEXT}\n${principal.principalId}\n${principal.keyId}`).digest("base64url");
  return `${DERIVED_BEARER_PREFIX}.${principal.principalId}.${principal.keyId}.${mac}`;
}

/** 64-bit FNV-1a over the id's UTF-8 bytes (deploy/README.md §5.2). */
export function fnv1a64(id: string): bigint {
  let h = 0xcbf29ce484222325n;
  for (const byte of new TextEncoder().encode(id)) {
    h ^= BigInt(byte);
    h = (h * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  return h;
}

/** The replica a principal lives on. */
export function shardOf(principalId: string, count: number): number {
  return Number(fnv1a64(principalId) % BigInt(count));
}

/** `SHOGUN_SHARD` = `<index>/<count>`, e.g. `0/2`. */
export function parseShard(raw: string): { index: number; count: number } {
  const m = /^(\d{1,4})\/(\d{1,4})$/.exec(raw.trim());
  if (!m) throw new Error("SHOGUN_SHARD is <index>/<count>, e.g. 0/2");
  const index = Number(m[1]);
  const count = Number(m[2]);
  if (count < 1 || index >= count) throw new Error("SHOGUN_SHARD needs 0 <= index < count");
  return { index, count };
}

/** The URL origin used with a Workers binding when `SHOGUN_SIDECAR_URL` is unset (a label only). */
export const SIDECAR_BINDING_ORIGIN = "http://sidecar.internal";

export interface McpRelayConfig {
  /** The sidecar's base URL (`SHOGUN_SIDECAR_URL`); with `binding`, only its path matters. */
  url: string;
  /** `SHOGUN_SIDECAR_BINDING`: the name of a Workers service / VPC binding to reach the sidecar
   * through instead of the public network. */
  binding?: string;
  /** principal → sidecar bearer (`SHOGUN_SIDECAR_PRINCIPAL_TOKENS`). */
  tokens?: ReadonlyMap<string, string>;
  /** `SHOGUN_SIDECAR_BEARER_SECRET`: derive a bearer for any principal. */
  bearerSecret?: string;
  /** `SHOGUN_SHARD`: the replica this gateway's sidecar is. */
  shard?: { index: number; count: number };
}

export type FetchFn = (input: string, init?: RequestInit) => Promise<Response>;

/** The `fetch` of a Workers binding (`env[name]`), or `undefined` when the runtime has none by
 * that name. */
export function bindingFetch(env: Record<string, unknown>, name: string): FetchFn | undefined {
  const binding = env[name] as { fetch?: unknown } | undefined;
  if (!binding || typeof binding.fetch !== "function") return undefined;
  const f = binding.fetch as (input: string, init?: RequestInit) => Promise<Response>;
  return (input, init) => f.call(binding, input, init);
}

export interface McpRelay {
  /** `MeteringDeps.mcpUpstream`: relay one request. Throws only when nothing was sent or the
   * sidecar sent no reply headers (metering then settles 0 and answers 503). */
  upstream(request: Request, principal: AgentPrincipal): Promise<Response>;
  /** `MeteringDeps.mcpAdmit`: why this principal cannot be relayed from here, before anything is
   * reserved, or `undefined`. */
  admit(principal: AgentPrincipal): AdmitRefusal | undefined;
}

/** Thrown by `upstream` for a principal `admit` would refuse (a caller that skipped it). */
export class McpRelayRefused extends Error {
  constructor(readonly refusal: AdmitRefusal) {
    super(refusal.code);
    this.name = "McpRelayRefused";
  }
}

const jsonReply = (status: number, code: string, message: string, nextAction: string, extra: Record<string, string> = {}): Response =>
  new Response(JSON.stringify({ error: { code, message, next_action: nextAction } }), { status, headers: { "content-type": "application/json", "cache-control": "no-store", ...extra } });

export function createMcpRelay(config: McpRelayConfig, fetchFn: FetchFn = (input, init) => fetch(input, init), headersTimeoutMs = SIDECAR_HEADERS_TIMEOUT_MS): McpRelay {
  const base = config.url.replace(/\/+$/, "");
  const bearerFor = (p: AgentPrincipal): string | undefined =>
    config.tokens?.get(p.principalId) ?? (config.bearerSecret ? derivedSidecarBearer(config.bearerSecret, p) : undefined);

  const admit = (p: AgentPrincipal): AdmitRefusal | undefined => {
    if (config.shard && shardOf(p.principalId, config.shard.count) !== config.shard.index) {
      return {
        status: 421, code: "wrong_shard", nextAction: "retry_after",
        message: "This principal's store lives on another replica and the request reached the wrong one. Nothing was charged. Retry; the ingress routes by principal.",
      };
    }
    if (!bearerFor(p)) {
      return {
        status: 503, code: "principal_not_provisioned", nextAction: "contact_operator",
        message: "This principal has no store on this gateway's core yet. Nothing was charged. Ask the operator to provision it (SHOGUN_SIDECAR_PRINCIPAL_TOKENS or SHOGUN_SIDECAR_BEARER_SECRET).",
      };
    }
    return undefined;
  };

  return {
    admit,
    async upstream(request, principal) {
      const refused = admit(principal);
      if (refused) throw new McpRelayRefused(refused);
      const headers = new Headers();
      for (const name of FORWARDED_REQUEST_HEADERS) {
        const v = request.headers.get(name);
        if (v !== null) headers.set(name, v);
      }
      headers.set("authorization", `Bearer ${bearerFor(principal) ?? ""}`);
      const hasBody = request.method !== "GET" && request.method !== "HEAD" && request.body !== null;
      // The deadline covers the reply headers only: once they arrive the timer is cleared, so a
      // long reply stream is never cut by it.
      const abort = new AbortController();
      const timer = setTimeout(() => abort.abort(), headersTimeoutMs);
      const target = sidecarTargetOf(request);
      let reply: Response;
      try {
        const init: RequestInit & { duplex?: "half" } = {
          method: target.method,
          headers,
          signal: abort.signal,
          redirect: "manual",
          ...(hasBody ? { body: request.body, duplex: "half" } : {}),
        };
        reply = await fetchFn(`${base}${target.path}`, init);
      } finally {
        clearTimeout(timer);
      }
      // The sidecar refusing the gateway's own bearer is an operator fault, not the agent's: never
      // relay it as a 401 that would send the agent to re-authenticate.
      if (reply.status === 401 || reply.status === 403) {
        await reply.body?.cancel().catch(() => undefined);
        return jsonReply(503, "core_rejected_gateway", "The core refused this gateway's credential for the principal. This is a configuration fault on the operator's side. Nothing was charged. Retry later.", "retry_after", { "retry-after": "30" });
      }
      const out = new Headers();
      for (const name of RELAYED_REPLY_HEADERS) {
        const v = reply.headers.get(name);
        if (v !== null) out.set(name, v);
      }
      // A HEAD the core answered as a GET gets that answer's status and headers, without the body.
      if (request.method === "HEAD") {
        await reply.body?.cancel().catch(() => undefined);
        return new Response(null, { status: reply.status, headers: out });
      }
      return new Response(reply.body, { status: reply.status, headers: out });
    },
  };
}
