import { describe, expect, it } from "vitest";
import { allProviders } from "../connectors/index.js";
import { CATEGORIES } from "../connectors/categories.js";
import { CatalogCompileError, compileCatalog } from "../src/catalog.js";
import { callerPrice } from "../src/take.js";
import { run } from "../src/run.js";
import { bearer, fullCatalog, makeDeps, synthEndpoint, synthProvider } from "./helpers.js";

describe("catalog compile (E8-T2)", () => {
  it("compiles the first-party connector and at least 10 imported Monid endpoints", () => {
    const catalog = fullCatalog();
    const monid = catalog.entries().filter((e) => e.provider_id === "monid");
    const firstParty = catalog.entries().filter((e) => e.first_party);
    expect(monid.length).toBeGreaterThanOrEqual(10);
    expect(firstParty.map((e) => e.tool_id).sort()).toEqual([
      "calendar.events.create",
      "calendar.events.list",
      "github.issues.create",
      "github.issues.list",
      "gmail.drafts.create",
      "gmail.messages.get",
      "gmail.messages.search",
      "gmail.messages.send",
      "linear.issues.create",
      "notion.pages.text",
      "notion.search",
      "shogunagent.actions.execute",
      "shogunagent.memory.recall",
      "shogunagent.state.commitments.list",
      "shogunagent.state.open_loops.list",
      "shogunagent.state.people.get",
      "shogunagent.state.people.list",
      "slack.messages.post",
    ]);
    for (const e of catalog.entries()) {
      expect(e.categories.length).toBeGreaterThan(0);
      for (const c of e.categories) expect(CATEGORIES).toContain(c);
      expect(e.hash).toMatch(/^[0-9a-f]{64}$/);
      expect(e.input_schema["type"]).toBe("object");
    }
  });

  it("content hash is deterministic and changes when an entry changes", () => {
    const a = compileCatalog(allProviders());
    const b = compileCatalog(allProviders());
    expect(a.manifest.hash).toBe(b.manifest.hash);
    const changed = compileCatalog([synthProvider("p", [synthEndpoint("x", { summary: "one" })])]);
    const changed2 = compileCatalog([synthProvider("p", [synthEndpoint("x", { summary: "two" })])]);
    expect(changed.manifest.hash).not.toBe(changed2.manifest.hash);
    expect(JSON.stringify(a.manifest)).not.toContain("expose");
  });

  it("rejects an unknown category, a missing scope class and a fixture that fails its schema", () => {
    const bad = synthEndpoint("bad", { summary: "x" });
    (bad.meta as { categories: string[] }).categories = ["memes"];
    expect(() => compileCatalog([synthProvider("p", [bad])])).toThrow(CatalogCompileError);

    const noScope = synthEndpoint("noscope", { summary: "x" });
    (noScope as { scope?: unknown }).scope = undefined;
    expect(() => compileCatalog([synthProvider("p", [noScope])])).toThrow(/scope.class is required/);

    const badFixture = synthEndpoint("fx", { summary: "x" });
    badFixture.fixtures.push({ scenario: "wrong", input: { nope: 1 }, response: { status: 200, body: null } });
    expect(() => compileCatalog([synthProvider("p", [badFixture])])).toThrow(/fixture "wrong"/);

    expect(() => compileCatalog([synthProvider("p", [synthEndpoint("a", { summary: "x" }), synthEndpoint("a", { summary: "y" })])])).toThrow(/duplicate tool id/);
  });

  it("replays every recorded fixture offline through run and settles as recorded", async () => {
    const catalog = fullCatalog();
    let replayed = 0;
    for (const entry of catalog.entries()) {
      const def = catalog.lookup(entry.tool_id);
      if (!def) throw new Error(entry.tool_id);
      for (const fx of def.endpoint.fixtures) {
        const deps = makeDeps({ catalog, responder: () => fx.response });
        // L2 answers its one confirmation, as an agent would after input_required.
        const confirm = entry.scope.level === "L2" ? { inputResponses: { confirm: true } } : {};
        const out = await run(deps, { input: { tool_id: entry.tool_id, input: fx.input as Record<string, unknown>, idempotency_key: `${entry.tool_id}:${fx.scenario}`, ...confirm }, bearer: bearer() });
        replayed++;
        if (entry.scope.level === "L3") {
          expect(out.body.resultType).toBe("approval_required");
          continue;
        }
        if (fx.expect?.error) {
          // A provider 404 is the caller's unknown id (not_found); any other failure is the provider's.
          if (fx.response.status === 404) expect(out.status, `${entry.tool_id}/${fx.scenario}`).toBe(404);
          else expect(out.status, `${entry.tool_id}/${fx.scenario}`).toBeGreaterThanOrEqual(500);
          expect(out.body.usage.cost_micro_usd).toBe(0);
          continue;
        }
        expect(out.status, `${entry.tool_id}/${fx.scenario}`).toBe(200);
        // A fixture records the provider's price; the agent is shown (and charged) it plus one take.
        if (fx.expect?.usage) expect(out.body.usage, `${entry.tool_id}/${fx.scenario}`).toMatchObject({ quantity: fx.expect.usage.quantity, cost_micro_usd: callerPrice(fx.expect.usage.cost_micro_usd, entry.first_party) });
        if (fx.expect?.output !== undefined && "result" in out.body) expect(out.body.result).toEqual(fx.expect.output);
      }
    }
    expect(replayed).toBeGreaterThanOrEqual(38);
  });
});
