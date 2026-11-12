import { defineEndpoint, z, zodInput } from "../../define.js";

export const memoryRecall = defineEndpoint({
  id: "memory.recall",
  meta: {
    summary: "Recall what the user's memory holds on a topic: typed facts, hedged facts and cited evidence, within a token budget.",
    description:
      "Searches the user's event log and derived state for a question and returns a compact pack: typed items with confidence and quotes, facts, hedged (`possibly`) facts, and evidence excerpts with their source and time. " +
      "Use for: what do I know about X, what was decided about X, give me context before replying. " +
      "Free read (L1); scoped to your agent key.",
    exclusions: "Not for public web facts (use a search tool), storing new memory (use memory.ingest), or verbatim chat transcripts.",
    categories: ["memory", "state"],
    notes: [
      "The pack is capped by `budget_tokens` (100–32000, default 2000); when it is cut, `truncated` is true: narrow the query.",
      "Items under `possibly` are uncertain: say so, never state them as fact.",
    ],
    examples: [
      {
        input: { query: "venue for the offsite", budget_tokens: 800 },
        output: {
          tool: "memory.recall",
          result: {
            items: [{ kind: "open_loop", id: 7, text: "Mika to confirm the offsite venue", confidence: 0.88, band: "high", quote: "I'll confirm the venue tomorrow", event_id: 9003 }],
            facts: ["Mika will confirm the offsite venue"],
            possibly: [],
            evidence: [{ event_id: 9003, ts_ms: 1792833120000, source: "slack", title: "#offsite", excerpt: "Mika: I'll confirm the venue tomorrow" }],
            truncated: false,
            budget_tokens: 800,
            used_tokens: 61,
            include_scratch: false,
          },
        },
      },
    ],
  },
  request: { method: "POST", path: "/v1/memory/recall" },
  input: {
    schema: zodInput(
      z
        .object({
          query: z.string().min(1).max(4096),
          budget_tokens: z.number().int().min(100).max(32_000).optional().describe("Most tokens to return. Default 2000."),
          include_scratch: z.boolean().optional().describe("Also read your own agent_scratch namespace. Default false."),
        })
        .strict(),
    ),
  },
  output: {
    isEmpty: (output) => {
      if (!output || typeof output !== "object" || Array.isArray(output)) return true;
      const result = output["result"];
      const pack = result && typeof result === "object" && !Array.isArray(result) ? result : output;
      return !["items", "facts", "possibly", "evidence"].some((key) => {
        const rows = pack[key];
        return Array.isArray(rows) && rows.length > 0;
      });
    },
  },
  usage: { model: "per_call", cost_micro_usd: 0 },
  scope: { class: "read" },
  fixtures: [
    {
      scenario: "topic-found",
      input: { query: "venue for the offsite", budget_tokens: 800 },
      response: {
        status: 200,
        body: {
          tool: "memory.recall",
          result: {
            items: [],
            facts: [],
            possibly: [],
            evidence: [{ event_id: 9003, ts_ms: 1792833120000, source: "slack", title: "#offsite", excerpt: "Mika: I'll confirm the venue tomorrow" }],
            truncated: false,
            budget_tokens: 800,
            used_tokens: 18,
            include_scratch: false,
          },
          usage: { unit: "read", quantity: 1, cost_micro_usd: 0 },
        },
      },
      expect: { usage: { quantity: 1, cost_micro_usd: 0 } },
    },
    {
      scenario: "nothing-recalled",
      input: { query: "zebra accounting" },
      response: {
        status: 200,
        body: {
          tool: "memory.recall",
          result: { items: [], facts: [], possibly: [], evidence: [], truncated: false, budget_tokens: 2000, used_tokens: 0, include_scratch: false },
          usage: { unit: "read", quantity: 1, cost_micro_usd: 0 },
        },
      },
      expect: { usage: { quantity: 0, cost_micro_usd: 0 } },
    },
    {
      scenario: "synthetic-state-unavailable",
      input: { query: "venue for the offsite" },
      response: { status: 503, body: { error: "unavailable" } },
      expect: { error: true },
    },
  ],
});
