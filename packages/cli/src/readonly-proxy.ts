/**
 * `shogunagent mcp`: the stdio MCP face the agent apps spawn, made read-only here.
 *
 * The server build of `shogun-mcp` admits an environment token at its legacy (full) scope; the
 * per-app `read` connections of the Mac app live in its Keychain and do not exist on a server box.
 * So the CLI sits between the app and `shogun-mcp` on the newline-delimited JSON-RPC stream:
 * `tools/call` of a tool outside the read-only set is answered here with an MCP tool error
 * (`isError: true`, `next_action: not_allowed`) and never reaches the server; `tools/list`
 * answers are filtered to the same set; everything else is relayed byte for byte. The set is
 * the frozen core's `level: read` rows plus four governance reads. Tests pin the core fixture
 * and the governance extension's wire names / Read classification in Rust's memory_api.rs.
 */
import type { Readable, Writable } from "node:stream";

/** Supported core and governance reads; new tools remain denied until explicitly reviewed. */
export const READ_ONLY_TOOLS: ReadonlySet<string> = new Set([
  "memory.search", "memory.get_context", "memory.get_context_pack", "memory.get_events", "memory.read_session",
  "memory.get_wrap", "memory.get_periods", "memory.recall", "memory.processing_status", "calendar.upcoming", "profile.whoami",
  "state.commitments.list", "state.open_loops.list", "state.people.list", "state.projects.list",
  "state.people.get", "state.projects.get", "wiki.evidence", "actions.status",
  "approvals.list", "approvals.get", "audit.list", "acl.list",
]);

interface JsonRpc {
  jsonrpc?: unknown;
  id?: unknown;
  method?: unknown;
  params?: unknown;
  result?: unknown;
  error?: unknown;
}

function parse(line: string): JsonRpc | undefined {
  try {
    const v: unknown = JSON.parse(line);
    return typeof v === "object" && v !== null && !Array.isArray(v) ? (v as JsonRpc) : undefined;
  } catch {
    return undefined;
  }
}

/** Ids of `tools/list` requests awaiting an answer, so the response can be filtered. */
export type Pending = Set<string>;

const idKey = (id: unknown): string | undefined => (typeof id === "string" || typeof id === "number" ? `${typeof id}:${String(id)}` : undefined);

/** The refusal for any unsupported tool, including reads outside the supported set. */
export function refusal(id: unknown, name: string): string {
  return JSON.stringify({
    jsonrpc: "2.0",
    id,
    result: {
      isError: true,
      content: [{ type: "text", text: `${name} is not supported on this read-only connection. Use tools/list to see supported tools. Other operations require an appropriate authorized connection; approval decisions and access changes require the human or operator workflow. next_action: not_allowed.` }],
      structuredContent: { error: { code: "source_not_allowed", message: `${name} is not supported on this read-only connection.`, next_action: "not_allowed" } },
    },
  });
}

/** What to do with one line from the client: forward it (maybe unchanged), or answer it here. */
export function filterRequest(line: string, pending: Pending, readOnly: ReadonlySet<string> = READ_ONLY_TOOLS): { forward?: string; reply?: string } {
  const msg = parse(line);
  if (!msg || typeof msg.method !== "string") return { forward: line };
  if (msg.method === "tools/call") {
    const name = typeof msg.params === "object" && msg.params !== null ? (msg.params as { name?: unknown }).name : undefined;
    if (typeof name !== "string" || !readOnly.has(name)) {
      // A notification-style call (no id) has nothing to answer to; it is dropped.
      return msg.id === undefined ? {} : { reply: refusal(msg.id, typeof name === "string" ? name : "(unnamed tool)") };
    }
    return { forward: line };
  }
  if (msg.method === "tools/list") {
    const k = idKey(msg.id);
    if (k) pending.add(k);
  }
  return { forward: line };
}

/** What to do with one line from the server: pass it through, filtering a pending `tools/list`. */
export function filterResponse(line: string, pending: Pending, readOnly: ReadonlySet<string> = READ_ONLY_TOOLS): string {
  const msg = parse(line);
  if (!msg) return line;
  const k = idKey(msg.id);
  if (!k || !pending.has(k)) return line;
  pending.delete(k);
  if (typeof msg.result !== "object" || msg.result === null) return line;
  const result = msg.result as { tools?: unknown };
  if (!Array.isArray(result.tools)) return line;
  const tools = result.tools.filter((t) => typeof t === "object" && t !== null && readOnly.has(String((t as { name?: unknown }).name)));
  return JSON.stringify({ ...msg, result: { ...result, tools } });
}

/** Split a byte stream into complete lines, keeping the remainder. */
export function lineSplitter(onLine: (line: string) => void): (chunk: string | Uint8Array) => void {
  let rest = "";
  return (chunk) => {
    rest += typeof chunk === "string" ? chunk : Buffer.from(chunk).toString("utf8");
    let nl: number;
    while ((nl = rest.indexOf("\n")) >= 0) {
      const line = rest.slice(0, nl).replace(/\r$/, "");
      rest = rest.slice(nl + 1);
      if (line.trim().length > 0) onLine(line);
    }
  };
}

export interface ProxyEnds {
  clientIn: Readable;
  clientOut: Writable;
  serverIn: Writable;
  serverOut: Readable;
}

/** Wire the four ends. Resolves when the server's stdout ends. */
export function runReadOnlyProxy(ends: ProxyEnds, readOnly: ReadonlySet<string> = READ_ONLY_TOOLS): Promise<void> {
  const pending: Pending = new Set();
  const fromClient = lineSplitter((line) => {
    const { forward, reply } = filterRequest(line, pending, readOnly);
    if (forward !== undefined) ends.serverIn.write(forward + "\n");
    if (reply !== undefined) ends.clientOut.write(reply + "\n");
  });
  const fromServer = lineSplitter((line) => {
    ends.clientOut.write(filterResponse(line, pending, readOnly) + "\n");
  });
  ends.clientIn.on("data", fromClient);
  ends.clientIn.on("end", () => ends.serverIn.end());
  ends.serverOut.on("data", fromServer);
  return new Promise((resolve) => {
    ends.serverOut.on("end", () => resolve());
    ends.serverOut.on("close", () => resolve());
  });
}
