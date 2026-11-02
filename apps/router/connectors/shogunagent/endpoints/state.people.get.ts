import { defineEndpoint, z, zodInput } from "../../define.js";

export const statePeopleGet = defineEndpoint({
  id: "state.people.get",
  meta: {
    summary: "One person's record by id: name, confidence and where the evidence about them comes from.",
    description:
      "Returns one person from the user's world model by the core's numeric id: the name, confidence and the sources of the evidence (source, app for captures, count; most-cited first). " +
      "Use for: who is this person, where does the user know them from. Find the id by name with state.people.list first. " +
      "Free read (L1); scoped to your agent key.",
    exclusions: "Not for looking someone up by name (use state.people.list), searching the public web about a person, or contact details the user never recorded.",
    categories: ["state", "memory"],
    notes: ["An unknown id returns 404."],
    examples: [
      {
        input: { id: 12 },
        output: { tool: "state.people.get", results: [{ text: "Mika Tanaka", confidence: 0.95, possibly: false, key: "person-12", table: "people", id: 12, sources: [{ source: "slack", count: 4 }] }] },
      },
    ],
  },
  request: { method: "GET", path: "/v1/state/people/:id" },
  input: {
    schema: zodInput(
      z
        .object({
          id: z.number().int().positive().describe("The person's id from state.people.list."),
        })
        .strict(),
    ),
    toRequest: ({ input }) => ({ method: "GET", path: `/v1/state/people/${(input as { id: number }).id}` }),
  },
  output: {},
  usage: { model: "per_call", cost_micro_usd: 0 },
  scope: { class: "read" },
  fixtures: [
    {
      scenario: "by-id",
      input: { id: 12 },
      response: { status: 200, body: { tool: "state.people.get", results: [{ text: "Mika Tanaka", confidence: 0.95, possibly: false, key: "person-12", table: "people", id: 12, sources: [{ source: "slack", count: 4 }] }] } },
      expect: { usage: { quantity: 1, cost_micro_usd: 0 } },
    },
    {
      scenario: "unknown-person",
      input: { id: 999999 },
      response: { status: 404, body: { error: { code: "not_found" } } },
      expect: { error: true },
    },
  ],
});
