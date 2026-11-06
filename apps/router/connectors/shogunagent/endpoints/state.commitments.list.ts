import { defineEndpoint, zodInput } from "../../define.js";
import { pageStateList, stateListInput, stateListIsEmpty, stateListRequest } from "../state-list.js";

export const stateCommitmentsList = defineEndpoint({
  id: "state.commitments.list",
  meta: {
    summary: "Find what the user committed to: promises and deliverables still open.",
    description:
      "Lists the user's open commitments: things the user said they would do. Done, cancelled and snoozed ones are left out. Each row has its text, confidence, a `possibly` flag and its row id. " +
      "Use for: what did the user commit to, what does the user owe, what is still open from their promises. " +
      "Free read (L1); scoped to your agent key.",
    exclusions: "Not for tasks other people owe the user (use state.open_loops.list), calendar events, raw notes, or finished commitments.",
    categories: ["state", "memory"],
    notes: [
      "Rows marked `possibly` are uncertain: treat them as leads, not facts. Low-confidence rows are hidden unless `include_low`.",
      "Newest first and paged: `limit` (default 50) from `offset`, with `total` and `truncated`.",
    ],
    examples: [
      {
        input: { limit: 5 },
        output: {
          tool: "state.commitments.list",
          results: [{ text: "Send the revised deck to Mika by Monday", confidence: 0.91, possibly: false, table: "commitments", id: 31 }],
          total: 1,
          truncated: false,
        },
      },
    ],
  },
  request: { method: "GET", path: "/v1/state/commitments" },
  input: {
    schema: zodInput(stateListInput),
    toRequest: stateListRequest("/v1/state/commitments"),
  },
  output: {
    fromResponse: pageStateList({ keepOrder: false }),
    isEmpty: stateListIsEmpty,
  },
  usage: { model: "per_call", cost_micro_usd: 0 },
  scope: { class: "read" },
  fixtures: [
    {
      scenario: "open-commitments",
      input: { limit: 2 },
      response: {
        status: 200,
        body: {
          tool: "state.commitments.list",
          results: [
            { text: "Review the contract draft", confidence: 0.86, possibly: false, table: "commitments", id: 30 },
            { text: "Send the revised deck to Mika by Monday", confidence: 0.91, possibly: false, table: "commitments", id: 31 },
          ],
        },
      },
      expect: { usage: { quantity: 1, cost_micro_usd: 0 } },
    },
    {
      scenario: "nothing-open",
      input: {},
      response: { status: 200, body: { tool: "state.commitments.list", results: [] } },
      expect: { usage: { quantity: 0, cost_micro_usd: 0 } },
    },
    {
      scenario: "synthetic-state-unavailable",
      input: {},
      response: { status: 503, body: { error: "unavailable" } },
      expect: { error: true },
    },
  ],
});
