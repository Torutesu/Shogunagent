import { defineEndpoint, zodInput } from "../../define.js";
import { pageStateList, stateListInput, stateListIsEmpty, stateListRequest } from "../state-list.js";

export const stateOpenLoopsList = defineEndpoint({
  id: "state.open_loops.list",
  meta: {
    summary: "What is still hanging: unfinished threads and unanswered items the user is waiting on.",
    description:
      "Lists the user's open loops: unfinished threads, unanswered questions and follow-ups. Closed and snoozed ones are left out. Each row has its text, confidence, a `possibly` flag and its row id. " +
      "Use for: what is the user waiting on, what did the user leave open, what should the user follow up on. " +
      "Free read (L1); scoped to your agent key.",
    exclusions: "Not for the user's own promises (use state.commitments.list), calendar events, or full message history.",
    categories: ["state", "memory"],
    notes: [
      "Rows marked `possibly` are uncertain: treat them as leads, not facts. Low-confidence rows are hidden unless `include_low`.",
      "Newest first and paged: `limit` (default 50) from `offset`, with `total` and `truncated`.",
    ],
    examples: [
      {
        input: { limit: 5 },
        output: {
          tool: "state.open_loops.list",
          results: [{ text: "Mika to confirm the offsite venue", confidence: 0.88, possibly: false, table: "open_loops", id: 7 }],
          total: 1,
          truncated: false,
        },
      },
    ],
  },
  request: { method: "GET", path: "/v1/state/open_loops" },
  input: {
    schema: zodInput(stateListInput),
    toRequest: stateListRequest("/v1/state/open_loops"),
  },
  output: {
    fromResponse: pageStateList({ keepOrder: false }),
    isEmpty: stateListIsEmpty,
  },
  usage: { model: "per_call", cost_micro_usd: 0 },
  scope: { class: "read" },
  fixtures: [
    {
      scenario: "open-loops",
      input: { limit: 5 },
      response: {
        status: 200,
        body: { tool: "state.open_loops.list", results: [{ text: "Mika to confirm the offsite venue", confidence: 0.88, possibly: false, table: "open_loops", id: 7 }] },
      },
      expect: { usage: { quantity: 1, cost_micro_usd: 0 } },
    },
    {
      scenario: "synthetic-state-unavailable",
      input: {},
      response: { status: 503, body: { error: "unavailable" } },
      expect: { error: true },
    },
  ],
});
