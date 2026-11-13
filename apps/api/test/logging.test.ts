/** §3.2: the logger never sees a body. It logs method/path/status/duration and numeric counters
 * only — chunk content and bearer tokens are structurally out of reach. */
import { describe, expect, it } from "vitest";

import { logId } from "../src/logging.js";
import { authed, makeAgentRig, mcp, registerAgent } from "./agent-helpers.js";
import { makeRig, postBatch, sampleBody } from "./helpers.js";
import { createTenant, makeEnterpriseRig, scim } from "./enterprise-helpers.js";

describe("redacting logger", () => {
  it("logs one line per request with method, path and status — never the body", async () => {
    const rig = await makeRig();
    await postBatch(rig, sampleBody);
    expect(rig.logLines).toHaveLength(1);
    const line = rig.logLines[0] ?? "";
    expect(line).toMatch(/^POST \/v1\/batch 202 \d+ms/);
    expect(line).toContain("chunks=2");
    expect(line).toContain("reserved_chunks=2");
    expect(line).toContain("event=running");
    // The licence is correlatable but not readable: the raw id never reaches the log sink.
    expect(line).toContain(`license=${logId("lic_test_1")}`);
    expect(line).not.toContain("lic_test_1");
    expect(line).not.toContain("TOP-SECRET");
  });

  it("never logs the bearer token, even on auth failures", async () => {
    const rig = await makeRig();
    await postBatch(rig, sampleBody, "not-a-real-token-value");
    const all = rig.logLines.join("\n");
    expect(all).toContain("POST /v1/batch 401");
    expect(all).not.toContain("not-a-real-token-value");
  });

  it("logs GET result fetches without any result content", async () => {
    const rig = await makeRig();
    // The ownership check reads the store before the gateway — register rb_1 as this licence's.
    await rig.usage.attachBatch("rb_1", "lic_test_1", "2026-08-13", 1, "dev_test_device");
    rig.gateway.status = {
      id: "rb_1",
      processing_status: "ended",
      request_counts: { processing: 0, succeeded: 1, errored: 0, canceled: 0, expired: 0 },
    };
    rig.gateway.resultLines = [
      JSON.stringify({
        custom_id: "1",
        result: { type: "succeeded", message: { content: [{ type: "text", text: "SECRET-RESULT" }] } },
      }),
    ];
    const res = await rig.app.request("/v1/batch/rb_1", {
      headers: { authorization: `Bearer ${rig.token}` },
    });
    await res.text(); // drain the stream so the request fully completes
    const all = rig.logLines.join("\n");
    expect(all).toContain("GET /v1/batch/:id 200");
    expect(all).not.toContain("SECRET-RESULT");
  });

  it("hides event IDs and query strings on processing status reads", async () => {
    const rig = makeAgentRig();
    const agent = await registerAgent(rig);
    await authed(rig, agent.key, "/v1/memory/processing/982734?include_scratch=true");
    const log = rig.logLines.join("\n");
    expect(log).toContain("GET /v1/memory/processing/:id");
    expect(log).not.toContain("982734");
    expect(log).not.toContain("include_scratch");
  });

  it("never logs an agent key (E3-T1): registration, a metered call and a rejected key leave only a hashed key id", async () => {
    const rig = makeAgentRig();
    const reg = await registerAgent(rig);
    await mcp(rig, reg.key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "2" });
    await authed(rig, `sa_live_${"Q".repeat(43)}`, "/v1/receipts");
    await authed(rig, reg.key, `/v1/keys/${reg.keyId}/revoke`, { method: "POST" });
    const all = rig.logLines.join("\n");
    expect(all).toContain("POST /v1/agents 201");
    expect(all).toContain("POST /mcp 200");
    expect(all).toContain("GET /v1/receipts 401 ");
    expect(all).toContain("POST /v1/keys/:id/revoke 200");
    expect(all).toContain(`key=${logId(reg.keyId)}`);
    // Neither the secret, nor the raw key id, nor the path's id, nor the tool name.
    expect(all).not.toContain(reg.key.slice(8, 24));
    expect(all).not.toContain(reg.keyId);
    expect(all).not.toContain("Q".repeat(8));
    expect(all).not.toContain("memory.ingest");
  });

  it("never logs a SCIM token, a SCIM body, an IdP claim, an assertion or a tenant id (E26)", async () => {
    const rig = await makeEnterpriseRig();
    const { tenantId, scimToken } = await createTenant(rig, { name: "Acme Secret Corp", plan: "enterprise", sso: { protocol: "oidc", enforced: true, oidc: { issuer: "https://idp.example.com", client_id: "client-x", client_secret: "client-secret-value" } } });
    const created = await scim(rig, scimToken ?? "", "/Users", { method: "POST", body: { userName: "carol@acme.example", name: { givenName: "Carol" }, emails: [{ value: "carol@acme.example" }] } });
    const userId = ((await created.json()) as { id: string }).id;
    await scim(rig, scimToken ?? "", `/Users/${userId}`, { method: "PATCH", body: { Operations: [{ op: "replace", value: { active: false } }] } });
    await scim(rig, `sct_${"Z".repeat(43)}`, "/Users");
    await rig.app.request(`/sso/${tenantId}/saml/acs`, { method: "POST", headers: { "content-type": "application/x-www-form-urlencoded" }, body: new URLSearchParams({ SAMLResponse: Buffer.from("<x>assertion-body</x>").toString("base64"), RelayState: "relay-value" }).toString() });
    await rig.app.request(`/sso/${tenantId}/oidc/callback?code=the-code-value&state=the-state-value`);
    const all = rig.logLines.join("\n");
    expect(all).toContain("POST /v1/admin/tenants 201");
    expect(all).toContain("POST /scim/v2/Users 201");
    expect(all).toContain("event=scim_user_deactivated");
    expect(all).toContain("GET /scim/v2/Users 401");
    expect(all).toContain("POST /sso/:tenant/saml/acs");
    expect(all).toContain("GET /sso/:tenant/oidc/callback");
    for (const s of [scimToken ?? "-", "Z".repeat(8), "carol", "Carol", "acme.example", "Acme Secret", "client-secret-value", "client-x", "assertion-body", "relay-value", "the-code-value", "the-state-value", tenantId, userId]) {
      expect(all, s).not.toContain(s);
    }
  });
});
