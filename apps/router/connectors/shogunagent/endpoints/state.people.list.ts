import { defineEndpoint, zodInput } from "../../define.js";
import { pageStateList, stateListInput, stateListIsEmpty, stateListRequest } from "../state-list.js";

export const statePeopleList = defineEndpoint({
  id: "state.people.list",
  meta: {
    summary: "The people the user deals with, with their ids: find someone by name here.",
    description:
      "Lists the people in the user's world model, the ones the user deals with most first. Each row has the name, confidence, a `possibly` flag and the person's id, which state.people.get takes. " +
      "Use for: find a person's id by name, who does the user work with most. " +
      "Free read (L1); scoped to your agent key.",
    exclusions: "Not for one person's details (use state.people.get with the id), or for searching the public web about someone.",
    categories: ["state", "memory"],
    notes: [
      "Low-confidence rows are hidden unless `include_low`; rows marked `possibly` are uncertain.",
      "Paged: `limit` (default 50) from `offset`, with `total` and `truncated`.",
    ],
    examples: [
      {
        input: { limit: 2 },
        output: {
          tool: "state.people.list",
          results: [{ text: "Mika Tanaka", confidence: 0.95, possibly: false, key: "person-12", table: "people", id: 12 }],
          total: 1,
          truncated: false,
        },
      },
    ],
  },
  request: { method: "GET", path: "/v1/state/people" },
  input: {
    schema: zodInput(stateListInput),
    toRequest: stateListRequest("/v1/state/people"),
  },
  output: {
    fromResponse: pageStateList({ keepOrder: true }),
    isEmpty: stateListIsEmpty,
  },
  usage: { model: "per_call", cost_micro_usd: 0 },
  scope: { class: "read" },
  fixtures: [
    {
      scenario: "people",
      input: { limit: 2 },
      response: {
        status: 200,
        body: { tool: "state.people.list", results: [{ text: "Mika Tanaka", confidence: 0.95, possibly: false, key: "person-12", table: "people", id: 12 }] },
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
