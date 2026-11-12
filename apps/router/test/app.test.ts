/**
 * The two faces answer identically (invariant 4) and carry the metering headers (invariant 10).
 */
import { describe, expect, it } from "vitest";
import { createApp } from "../src/app.js";
import { mcpTools } from "../src/mcp.js";
import { AGENT_KEY, makeDeps } from "./helpers.js";

const json = { "content-type": "application/json" };

describe("REST and MCP faces", () => {
  it("serves the three verbs over REST with Mcp-Method / Mcp-Name headers, free and without auth for discover and inspect", async () => {
    const app = createApp(makeDeps());
    const d = await app.request("/v1/router/discover", { method: "POST", headers: json, body: JSON.stringify({ task: "what did the user commit to", limit: 3 }) });
    expect(d.status).toBe(200);
    expect(d.headers.get("Mcp-Method")).toBe("tools/call");
    expect(d.headers.get("Mcp-Name")).toBe("router.discover");
    const db = (await d.json()) as { result: { candidates: { tool_id: string }[] }; usage: { cost_micro_usd: number } };
    expect(db.result.candidates[0]?.tool_id).toBe("shogunagent.state.commitments.list");
    expect(db.usage.cost_micro_usd).toBe(0);

    const i = await app.request("/v1/router/inspect", { method: "POST", headers: json, body: JSON.stringify({ tool_id: "shogunagent.memory.recall" }) });
    expect(i.status).toBe(200);
    expect(i.headers.get("Mcp-Name")).toBe("router.inspect");

    const bad = await app.request("/v1/router/run", { method: "POST", headers: json, body: "not json" });
    expect(bad.status).toBe(400);
    const badArgs = await app.request("/v1/router/discover", { method: "POST", headers: json, body: JSON.stringify({ nope: 1 }) });
    expect(badArgs.status).toBe(400);
    const nf = await app.request("/v1/nothing");
    expect(nf.status).toBe(404);
    const health = await app.request("/health");
    expect(health.status).toBe(200);
    const cat = await app.request("/v1/router/catalog");
    expect(((await cat.json()) as { entry_count: number }).entry_count).toBe(30);
  });

  it("MCP tools/list exposes router.discover / inspect / run with generated schemas and annotations", async () => {
    const app = createApp(makeDeps());
    const res = await app.request("/mcp", { method: "POST", headers: json, body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }) });
    expect(res.status).toBe(200);
    expect(res.headers.get("Mcp-Method")).toBe("tools/list");
    const body = (await res.json()) as { result: { tools: { name: string; inputSchema: { required?: string[] }; annotations: { readOnlyHint: boolean } }[] } };
    expect(body.result.tools.map((t) => t.name)).toEqual(["router.discover", "router.inspect", "router.run"]);
    expect(body.result.tools[2]?.inputSchema.required).toEqual(["tool_id", "input", "idempotency_key"]);
    expect(body.result.tools[0]?.annotations.readOnlyHint).toBe(true);
    expect(body.result.tools[2]?.annotations.readOnlyHint).toBe(false);
    expect(mcpTools()).toHaveLength(3);
    const init = await app.request("/mcp", { method: "POST", headers: json, body: JSON.stringify({ jsonrpc: "2.0", id: 2, method: "initialize", params: {} }) });
    expect(((await init.json()) as { result: { serverInfo: { name: string } } }).result.serverInfo.name).toBe("shogunagent-router");
    const unknown = await app.request("/mcp", { method: "POST", headers: json, body: JSON.stringify({ jsonrpc: "2.0", id: 3, method: "resources/list" }) });
    expect(((await unknown.json()) as { error: { code: number } }).error.code).toBe(-32601);
  });

  it("a tools/call to router.run returns the same body as the REST run, with Mcp-Name set", async () => {
    const responder = () => ({ status: 200, body: { status: "succeeded", output: { results: [{ url: "https://e.x" }] } } });
    const restApp = createApp(makeDeps({ responder }));
    const mcpApp = createApp(makeDeps({ responder }));
    const args = { tool_id: "monid.tinyfish.web-search", input: { query: "x" }, idempotency_key: "same" };
    const headers = { ...json, authorization: `Bearer ${AGENT_KEY}` };
    const rest = (await (await restApp.request("/v1/router/run", { method: "POST", headers, body: JSON.stringify(args) })).json()) as Record<string, unknown>;
    const mcpRes = await mcpApp.request("/mcp", { method: "POST", headers, body: JSON.stringify({ jsonrpc: "2.0", id: 9, method: "tools/call", params: { name: "router.run", arguments: args } }) });
    expect(mcpRes.headers.get("Mcp-Method")).toBe("tools/call");
    expect(mcpRes.headers.get("Mcp-Name")).toBe("router.run");
    const mcp = (await mcpRes.json()) as { result: { structuredContent: Record<string, unknown>; isError: boolean; content: { type: string; text: string }[] } };
    expect(mcp.result.isError).toBe(false);
    const strip = (b: Record<string, unknown>) => ({ ...b, latency_ms: 0, run_id: 0 });
    expect(strip(mcp.result.structuredContent)).toEqual(strip(rest));
    expect(JSON.parse(mcp.result.content[0]?.text ?? "{}")).toEqual(mcp.result.structuredContent);

    const err = await mcpApp.request("/mcp", { method: "POST", headers, body: JSON.stringify({ jsonrpc: "2.0", id: 10, method: "tools/call", params: { name: "router.run", arguments: { tool_id: "ghost", input: {}, idempotency_key: "e" } } }) });
    expect(((await err.json()) as { result: { isError: boolean } }).result.isError).toBe(true);
  });
});
