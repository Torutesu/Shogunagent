import { describe, expect, it } from "vitest";

import { createMcpRelay } from "../src/mcp-proxy.js";
import { authed, makeAgentRig, registerAgent } from "./agent-helpers.js";

describe("the router's first-party state reads through the hosted gateway", () => {
  it("relays the REST path, query and body to the caller's sidecar as a free read", async () => {
    const seen: Array<{ url: string; method: string; body: string }> = [];
    const relay = createMcpRelay({ url: "http://127.0.0.1:7464", bearerSecret: "local-test-sidecar-secret-32-characters" }, async (url, init) => {
      seen.push({ url, method: init?.method ?? "GET", body: await new Response(init?.body).text() });
      return new Response('{"results":[]}', { headers: { "content-type": "application/json" } });
    });
    const rig = makeAgentRig({ mcpUpstream: relay.upstream, mcpAdmit: relay.admit });
    const agent = await registerAgent(rig);
    const paths = ["/v1/state/commitments?limit=2", "/v1/state/open_loops", "/v1/state/people/42", "/v1/memory/processing/42?include_scratch=true", "/v1/memory/processing/not-an-id"];
    for (const path of paths) {
      const reply = await authed(rig, agent.key, path, { headers: { "mcp-name": "memory.ingest", "mcp-quantity": "999" } });
      expect(reply.status, path).toBe(200);
      expect(reply.headers.get("x-usage-unit")).toBe("read");
      expect(reply.headers.get("x-usage-cost-micro-usd")).toBe("0");
      expect(await reply.json()).toEqual({ results: [] });
    }
    const body = { query: "the release plan", budget_tokens: 500 };
    const recall = await authed(rig, agent.key, "/v1/memory/recall", { method: "POST", body });
    expect(recall.status).toBe(200);
    expect(recall.headers.get("x-usage-unit")).toBe("read");
    expect(recall.headers.get("x-usage-cost-micro-usd")).toBe("0");
    expect(seen.map(({ url }) => url)).toEqual([...paths, "/v1/memory/recall"].map((path) => `http://127.0.0.1:7464${path}`));
    expect(JSON.parse(seen.at(-1)!.body)).toEqual(body);
  });

  it("authenticates state reads before contacting the sidecar", async () => {
    const rig = makeAgentRig();
    for (const path of ["/v1/state/commitments", "/v1/state/open_loops", "/v1/state/people/42", "/v1/memory/processing/42"]) {
      expect((await rig.app.request(path)).status, path).toBe(401);
    }
    expect((await rig.app.request("/v1/memory/recall", { method: "POST", body: "{}" })).status).toBe(401);
    expect(rig.upstreamCalls).toHaveLength(0);
  });
});
