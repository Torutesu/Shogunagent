/**
 * The first-party connector (E8-T8): the ShogunAgent state layer reached through its REST face.
 * Reads are $0 (invariant 10); the one write (`actions.execute`) is L3 and priced like any
 * first-party tool run (README §7: $0.002 / call). Ranked by the same score as every other
 * provider and labeled `first_party` (invariant 13).
 *
 * Auth is `caller_bearer`: the agent's own key is forwarded by the transport to the state face,
 * which scopes the read to that agent's principal. The router holds no key for its own product.
 */
import { defineProvider } from "../define.js";
import { actionsExecute } from "./endpoints/actions.execute.js";
import { memoryRecall } from "./endpoints/memory.recall.js";
import { stateCommitmentsList } from "./endpoints/state.commitments.list.js";
import { stateOpenLoopsList } from "./endpoints/state.open_loops.list.js";
import { statePeopleGet } from "./endpoints/state.people.get.js";
import { statePeopleList } from "./endpoints/state.people.list.js";
import manifest from "./provider.json" with { type: "json" };

export const shogunagentProvider = defineProvider({
  id: "shogunagent",
  name: "ShogunAgent",
  first_party: true,
  meta: {
    description: "The user's typed, provenance-backed world model: people, projects, commitments, open loops and recall over their event log. Reads are free; writes pass L2/L3.",
    docsUrl: "https://shogunai.si/docs/router/shogunagent",
    homepage: "https://shogunai.si",
  },
  request: { baseUrl: "https://api.shogunaios.com", baseUrlEnv: "SHOGUNAGENT_API_BASE_URL" },
  auth: { kind: "caller_bearer" },
  manifest,
  categories: ["state"],
  health: { p50_ms: 40, p95_ms: 120, success_rate: 0.999 },
  endpoints: [stateCommitmentsList, stateOpenLoopsList, statePeopleList, statePeopleGet, memoryRecall, actionsExecute],
});
