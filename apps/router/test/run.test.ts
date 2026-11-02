import { describe, expect, it } from "vitest";
import { compileCatalog } from "../src/catalog.js";
import { InMemoryMeteringClient } from "../src/metering.js";
import { run } from "../src/run.js";
import { AGENT_KEY, bearer, fullCatalog, makeDeps, synthEndpoint, synthProvider } from "./helpers.js";

const SEARCH = "monid.tinyfish.web-search";
const okResponse = { status: 200, body: { status: "succeeded", output: { results: [{ url: "https://e.x/1" }] }, usage: { quantity: 1, cost_usd: 0.0013 } } };

describe("run (E8-T5)", () => {
  it("rejects input that fails the schema with fix_input and charges nothing", async () => {
    const deps = makeDeps();
    const out = await run(deps, { input: { tool_id: SEARCH, input: { nope: 1 }, idempotency_key: "k1" }, bearer: bearer() });
    expect(out.status).toBe(400);
    if (!("error" in out.body)) return;
    expect(out.body.error.code).toBe("invalid_input");
    expect(out.body.error.next_action).toBe("fix_input");
    expect(out.body.details?.length).toBeGreaterThan(0);
    expect(out.body.usage.cost_micro_usd).toBe(0);
    expect(deps.calls).toHaveLength(0);
  });

  it("executes an L1 tool through the transport and returns result, usage, latency_ms, hints, next_action", async () => {
    const deps = makeDeps({ responder: () => okResponse });
    const out = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "k2" }, bearer: bearer() });
    expect(out.status).toBe(200);
    if (out.body.resultType !== "result") throw new Error("expected result");
    expect(out.body.result).toEqual({ results: [{ url: "https://e.x/1" }] });
    expect(out.body.usage).toEqual({ unit: "call", quantity: 1, cost_micro_usd: 1365 });
    expect(typeof out.body.latency_ms).toBe("number");
    expect(Array.isArray(out.body.hints)).toBe(true);
    expect(out.body.next_action).toBeNull();
    expect(out.body.permission_level).toBe("L1");
    expect(deps.calls[0]?.request.body).toEqual({ endpoint: "tinyfish/web-search", input: { query: "x" } });
    expect(deps.metering.balanceOf(AGENT_KEY)).toBe(1_000_000 - 1365);
  });

  it("failures and empty results cost 0", async () => {
    const failing = makeDeps({ responder: () => ({ status: 200, body: { status: "failed", error: { code: "upstream" } } }) });
    const f = await run(failing, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "k3" }, bearer: bearer() });
    expect(f.status).toBe(502);
    expect(f.body.usage.cost_micro_usd).toBe(0);
    expect(failing.metering.balanceOf(AGENT_KEY)).toBe(1_000_000);
    if ("error" in f.body) expect(["retry_after", "try_alternative"]).toContain(f.body.error.next_action);

    const empty = makeDeps({ responder: () => ({ status: 200, body: { status: "succeeded", output: { results: [] } } }) });
    const e = await run(empty, { input: { tool_id: "monid.exa.search", input: { query: "x" }, idempotency_key: "k4" }, bearer: bearer() });
    expect(e.status).toBe(200);
    expect(e.body.usage.cost_micro_usd).toBe(0);
    expect(e.body.usage.quantity).toBe(0);
    expect(empty.metering.balanceOf(AGENT_KEY)).toBe(1_000_000);
    expect(empty.runLog.records[0]?.outcome).toBe("empty");
  });

  it("a repeated idempotency key answers a replay marker without re-running or re-charging; a different input conflicts", async () => {
    let n = 0;
    const deps = makeDeps({ responder: () => { n++; return okResponse; } });
    const a = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "same" }, bearer: bearer() });
    const b = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "same" }, bearer: bearer() });
    expect(n).toBe(1);
    if (a.body.resultType !== "result" || b.body.resultType !== "replay") throw new Error("expected a result, then a replay");
    // E48: the result body is never stored, so the replay names the run instead of repeating it.
    expect(b.body).not.toHaveProperty("result");
    expect(b.body.run_id).toBe(a.body.run_id);
    expect(b.body.replayed).toBe(true);
    expect(b.body.original.usage).toEqual(a.body.usage);
    expect(b.body.original.outcome).toBe("ok");
    expect(b.body.usage.cost_micro_usd).toBe(0);
    expect(deps.metering.balanceOf(AGENT_KEY)).toBe(1_000_000 - 1365);
    const c = await run(deps, { input: { tool_id: SEARCH, input: { query: "different" }, idempotency_key: "same" }, bearer: bearer() });
    expect(c.status).toBe(409);
    expect(n).toBe(1);
  });

  it("dryRun returns the quote and permission level only", async () => {
    const deps = makeDeps();
    const out = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "dry", dryRun: true } });
    expect(out.status).toBe(200);
    if (out.body.resultType !== "quote") throw new Error("expected quote");
    // Monid's price (1300) plus one take (5%, rounded up): the take is applied once, not on top of a marked-up import.
    expect(out.body.quote).toEqual({ estimate_micro_usd: 1365, provider_micro_usd: 1300, take_bps: 500, unit: "call", model: "per_call", failures_cost: 0, empty_results_cost: 0 });
    expect(out.body.permission_level).toBe("L1");
    expect(deps.calls).toHaveLength(0);
    expect(deps.runLog.records).toHaveLength(0);
  });

  it("L2 returns input_required (MRTR shape) until confirmed, then executes", async () => {
    const catalog = compileCatalog([synthProvider("notes", [synthEndpoint("append", { summary: "append a note to the user's data", cls: "user_data_write", cost: 2000 })])]);
    const deps = makeDeps({ catalog, responder: () => ({ status: 200, body: { id: "n1" } }) });
    const first = await run(deps, { input: { tool_id: "notes.append", input: { q: "hello" }, idempotency_key: "l2" }, bearer: bearer() });
    expect(first.status).toBe(200);
    if (first.body.resultType !== "input_required") throw new Error("expected input_required");
    expect(first.body.inputRequests[0]?.id).toBe("confirm");
    expect(first.body.retry).toEqual({ with: "inputResponses", example: { confirm: true } });
    expect(first.body.next_action).toBe("provide_input");
    expect(deps.calls).toHaveLength(0);
    const second = await run(deps, { input: { tool_id: "notes.append", input: { q: "hello" }, idempotency_key: "l2", inputResponses: { confirm: true } }, bearer: bearer() });
    expect(second.body.resultType).toBe("result");
    expect(deps.calls).toHaveLength(1);
  });

  it("L3 returns an approval URL and never calls the transport", async () => {
    const deps = makeDeps();
    const out = await run(deps, {
      input: { tool_id: "shogunagent.actions.execute", input: { action: { kind: "send_email", to: "a@b.c", subject: "s", body: "b" } }, idempotency_key: "l3" },
      bearer: bearer(),
    });
    expect(out.status).toBe(200);
    if (out.body.resultType !== "approval_required") throw new Error("expected approval_required");
    expect(out.body.approval.url).toMatch(/^https:\/\/approve\.test\/approve\/[0-9a-f-]{36}$/);
    expect(out.body.next_action).toBe("approve_at_url");
    expect(out.body.usage.cost_micro_usd).toBe(0);
    expect(deps.calls).toHaveLength(0);
    expect(deps.runLog.records[0]?.outcome).toBe("approval_required");
  });

  it("an unknown tool is not found; a metered tool without a key is 401; an empty wallet is 402 with the payment shape", async () => {
    const deps = makeDeps();
    const nf = await run(deps, { input: { tool_id: "ghost.tool", input: {}, idempotency_key: "x" }, bearer: bearer() });
    expect(nf.status).toBe(404);
    const anon = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "anon" } });
    expect(anon.status).toBe(401);
    if ("error" in anon.body) expect(anon.body.error.next_action).toBe("authenticate");
    const broke = makeDeps({ metering: new InMemoryMeteringClient({ balances: { [AGENT_KEY]: 10 } }) });
    const p = await run(broke, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "poor" }, bearer: bearer() });
    expect(p.status).toBe(402);
    if (!("error" in p.body)) return;
    expect(p.body.error.next_action).toBe("top_up");
    expect(p.body.payment).toMatchObject({ amount_micro_usd: 1355, unit: "call", retry_after: 0 });
    expect(p.body.payment?.accepts[0]?.rail).toBe("card");
  });

  it("a free first-party read runs without a key being charged, but needs the caller's bearer to reach the state face", async () => {
    const deps = makeDeps({ responder: () => ({ status: 200, body: { tool: "state.commitments.list", results: [{ text: "Send the deck", confidence: 0.9, possibly: false, table: "commitments", id: 1 }] } }) });
    const noKey = await run(deps, { input: { tool_id: "shogunagent.state.commitments.list", input: {}, idempotency_key: "fp0" } });
    expect(noKey.status).toBe(401);
    if ("error" in noKey.body) expect(noKey.body.error.next_action).toBe("authenticate");
    expect(deps.calls).toHaveLength(0);
    const withKey = await run(deps, { input: { tool_id: "shogunagent.state.commitments.list", input: { limit: 5 }, idempotency_key: "fp1" }, bearer: bearer() });
    expect(withKey.status).toBe(200);
    expect(withKey.body.usage.cost_micro_usd).toBe(0);
    expect(deps.metering.balanceOf(AGENT_KEY)).toBe(1_000_000);
    expect(deps.calls.at(-1)?.request).toEqual({ method: "GET", path: "/v1/state/commitments", query: {} });
  });

  it("every response carries usage and latency_ms, and errors carry next_action", async () => {
    const deps = makeDeps({ responder: () => okResponse });
    const results = await Promise.all([
      run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "a" }, bearer: bearer() }),
      run(deps, { input: { tool_id: SEARCH, input: { bad: 1 }, idempotency_key: "b" }, bearer: bearer() }),
      run(deps, { input: { tool_id: "nope", input: {}, idempotency_key: "c" }, bearer: bearer() }),
    ]);
    for (const r of results) {
      expect(r.body.usage).toBeDefined();
      expect(typeof r.body.latency_ms).toBe("number");
      if ("error" in r.body) expect(r.body.error.next_action).not.toBeNull();
    }
    expect(fullCatalog().manifest.entry_count).toBe(deps.catalog.manifest.entry_count);
  });
});
