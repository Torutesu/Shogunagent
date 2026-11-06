/**
 * The core's state lists over REST: `GET /v1/state/<noun>` returns every row (only `include_low`
 * is read from the query), and the MCP face pages that list itself (`page_list` in
 * crates/shogun-mcp/src/mcp.rs). The router pages the same way here, so an agent gets the same
 * result through the router as over MCP (invariant 4): newest first by row id (people keep the
 * core's engagement order), `limit` (default 50) from `offset`, with `total` and `truncated`.
 */
import type { Json, RunRequest } from "../types.js";
import { z } from "../define.js";

export const DEFAULT_LIMIT = 50;

export const stateListInput = z
  .object({
    include_low: z.boolean().optional().describe("Also return low-confidence rows. Default false."),
    limit: z.number().int().min(1).max(200).optional().describe("Default 50."),
    offset: z.number().int().min(0).optional().describe("Rows to skip. Default 0."),
  })
  .strict();

type ListInput = { include_low?: boolean; limit?: number; offset?: number };

/** Only `include_low` reaches the core; paging happens in `pageStateList`. */
export function stateListRequest(path: string) {
  return ({ input }: { input: Json }): RunRequest => {
    const { include_low } = (input ?? {}) as ListInput;
    return { method: "GET", path, query: include_low ? { include_low: "true" } : {} };
  };
}

export function pageStateList(opts: { keepOrder: boolean }) {
  return ({ input, response }: { input: Json; response: { body: Json } }): Json => {
    const body = response.body;
    if (!body || typeof body !== "object" || Array.isArray(body) || !Array.isArray(body["results"])) return body;
    const rows = [...(body["results"] as Json[])];
    if (!opts.keepOrder) {
      const id = (row: Json) => (row && typeof row === "object" && !Array.isArray(row) && typeof row["id"] === "number" ? row["id"] : 0);
      rows.sort((a, b) => id(b) - id(a));
    }
    const { limit = DEFAULT_LIMIT, offset = 0 } = (input ?? {}) as ListInput;
    const page = rows.slice(offset, offset + limit);
    return { ...body, results: page, total: rows.length, truncated: offset + page.length < rows.length };
  };
}

export function stateListIsEmpty(output: Json): boolean {
  if (!output || typeof output !== "object" || Array.isArray(output)) return true;
  const results = output["results"];
  return !Array.isArray(results) || results.length === 0;
}
