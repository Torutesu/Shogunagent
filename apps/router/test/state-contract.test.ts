import { describe, expect, it } from "vitest";

import { stateCommitmentsList } from "../connectors/shogunagent/endpoints/state.commitments.list.js";
import { statePeopleGet } from "../connectors/shogunagent/endpoints/state.people.get.js";
import { statePeopleList } from "../connectors/shogunagent/endpoints/state.people.list.js";

const request = (endpoint: typeof stateCommitmentsList, input: object) =>
  endpoint.input.toRequest!({ input: input as never, request: { method: endpoint.request.method, path: endpoint.request.path } });
const map = (endpoint: typeof stateCommitmentsList, input: object, results: object[]) =>
  endpoint.output.fromResponse!({ input: input as never, response: { status: 200, body: { tool: endpoint.id, results } as never } });

const rows = (ids: number[]) => ids.map((id) => ({ text: `row ${id}`, confidence: 0.9, possibly: false, table: "commitments", id }));

describe("the first-party state connector speaks the core's REST contract", () => {
  it("sends the core only include_low: the REST face returns every row and the router pages it", () => {
    expect(request(stateCommitmentsList, { limit: 2, offset: 4 })).toEqual({ method: "GET", path: "/v1/state/commitments", query: {} });
    expect(request(stateCommitmentsList, { include_low: true })).toEqual({ method: "GET", path: "/v1/state/commitments", query: { include_low: "true" } });
  });

  it("pages like the MCP face: newest id first, limit from offset, with total and truncated", () => {
    expect(map(stateCommitmentsList, { limit: 2 }, rows([3, 9, 5]))).toEqual({
      tool: "state.commitments.list", results: rows([9, 5]), total: 3, truncated: true,
    });
    expect(map(stateCommitmentsList, { limit: 2, offset: 2 }, rows([3, 9, 5]))).toEqual({
      tool: "state.commitments.list", results: rows([3]), total: 3, truncated: false,
    });
    expect(map(stateCommitmentsList, {}, rows([1]))).toMatchObject({ total: 1, truncated: false });
  });

  it("keeps the core's engagement order for people", () => {
    expect(map(statePeopleList, {}, rows([3, 9, 5]))).toMatchObject({ results: rows([3, 9, 5]) });
  });

  it("takes a person by the core's numeric id only", () => {
    const schema = statePeopleGet.input.schema!;
    expect(schema.validate({ id: 12 })).toEqual({ ok: true, value: { id: 12 } });
    expect(schema.validate({ person: "Mika" }).ok).toBe(false);
    expect(schema.validate({ id: "12" }).ok).toBe(false);
    expect(request(statePeopleGet, { id: 12 })).toEqual({ method: "GET", path: "/v1/state/people/12" });
  });
});
