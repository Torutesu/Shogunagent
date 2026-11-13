/** The read-only stdio filter behind `shogunagent mcp`: writes never reach the server, the tool
 * list is filtered, everything else is relayed unchanged; metadata pins the supported read set. */
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { PassThrough } from "node:stream";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import { READ_ONLY_TOOLS, filterRequest, filterResponse, lineSplitter, runReadOnlyProxy, type Pending } from "../src/readonly-proxy.js";

const here = dirname(fileURLToPath(import.meta.url));
const governanceReads = ["approvals.list", "approvals.get", "audit.list", "acl.list"];

describe("filterRequest", () => {
  it("forwards read tools, protocol methods and notifications unchanged", () => {
    const pending: Pending = new Set();
    for (const line of [
      JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: {} }),
      JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }),
      JSON.stringify({ jsonrpc: "2.0", id: 2, method: "tools/call", params: { name: "state.commitments.list", arguments: {} } }),
      JSON.stringify({ jsonrpc: "2.0", id: 3, method: "tools/call", params: { name: "profile.whoami" } }),
      "not json at all",
    ]) {
      expect(filterRequest(line, pending)).toEqual({ forward: line });
    }
    expect(pending.size).toBe(0);
  });

  it("answers a write locally with an MCP tool error and never forwards it", () => {
    const pending: Pending = new Set();
    for (const name of ["memory.ingest", "memory.append_note", "actions.execute", "router.run", "memory.delete_everything", "state.update", "state.delete", "state.create", "approvals.decide", "approvals.approve", "approvals.reject", "acl.grant", "acl.revoke"]) {
      const out = filterRequest(JSON.stringify({ jsonrpc: "2.0", id: 7, method: "tools/call", params: { name, arguments: {} } }), pending);
      expect(out.forward).toBeUndefined();
      const reply = JSON.parse(out.reply ?? "") as { id: number; result: { isError: boolean; content: Array<{ text: string }>; structuredContent: { error: { code: string; next_action: string } } } };
      expect(reply.id).toBe(7);
      expect(reply.result.isError).toBe(true);
      expect(reply.result.content[0]?.text).toContain(name);
      expect(reply.result.structuredContent.error).toMatchObject({ code: "source_not_allowed", next_action: "not_allowed" });
    }
    // A call with no tool name, or with no id (nothing to answer to), is dropped.
    expect(filterRequest(JSON.stringify({ jsonrpc: "2.0", id: 8, method: "tools/call", params: {} }), pending).forward).toBeUndefined();
    expect(filterRequest(JSON.stringify({ jsonrpc: "2.0", method: "tools/call", params: { name: "memory.ingest" } }), pending)).toEqual({});
  });

  it("remembers tools/list ids so the answer can be filtered", () => {
    const pending: Pending = new Set();
    filterRequest(JSON.stringify({ jsonrpc: "2.0", id: "abc", method: "tools/list" }), pending);
    filterRequest(JSON.stringify({ jsonrpc: "2.0", id: 5, method: "tools/list" }), pending);
    expect(pending).toEqual(new Set(["string:abc", "number:5"]));
    const answer = JSON.stringify({ jsonrpc: "2.0", id: 5, result: { tools: [{ name: "memory.search" }, { name: "memory.ingest" }, { name: "actions.execute" }], nextCursor: null } });
    const filtered = JSON.parse(filterResponse(answer, pending)) as { result: { tools: Array<{ name: string }>; nextCursor: null } };
    expect(filtered.result.tools.map((t) => t.name)).toEqual(["memory.search"]);
    expect(filtered.result.nextCursor).toBeNull();
    expect(pending).toEqual(new Set(["string:abc"]));
    // Other responses pass through byte for byte, including ones with ids that are not pending.
    const other = JSON.stringify({ jsonrpc: "2.0", id: 9, result: { content: [] } });
    expect(filterResponse(other, pending)).toBe(other);
    expect(filterResponse("garbage", pending)).toBe("garbage");
  });

  it("supports the frozen core reads, governance reads and processing status", () => {
    const table = JSON.parse(readFileSync(join(here, "..", "..", "..", "apps", "api", "fixtures", "tool-table.json"), "utf8")) as { tools: Array<{ name: string; level: string }> };
    const reads = table.tools.filter((t) => t.level === "read").map((t) => t.name).sort();
    expect([...READ_ONLY_TOOLS].sort()).toEqual([...reads, ...governanceReads, "memory.processing_status"].sort());
    for (const t of table.tools.filter((t) => t.level !== "read")) expect(READ_ONLY_TOOLS.has(t.name), t.name).toBe(false);

    // The interim core fixture predates governance. Pin the deliberate extension to the Rust
    // metadata that generates the actual tool table, without building Rust during Node tests.
    const metadata = readFileSync(join(here, "../../../crates/shogun-mcp/src/memory_api.rs"), "utf8");
    const classification = metadata.match(/pub fn tool_level\([\s\S]*?\n\}/)?.[0] ?? "";
    const readsInRust = [...classification.matchAll(/((?:Tool::\w+\s*(?:\|\s*)?)+)=>\s*ApiLevel::Read\b/g)]
      .flatMap((match) => [...(match[1] ?? "").matchAll(/Tool::(\w+)/g)].map((variant) => variant[1]));
    expect(readsInRust).toContain("MemoryProcessingStatus");
    expect(metadata).toContain('Tool::MemoryProcessingStatus => "memory.processing_status"');
    for (const [index, variant] of ["ApprovalsList", "ApprovalsGet", "AuditList", "AclList"].entries()) {
      expect(metadata).toContain(`Tool::${variant} => "${governanceReads[index]}"`);
      expect(readsInRust).toContain(variant);
    }
  });

  it("describes unsupported reads neutrally and never trusts names or claimed annotations", () => {
    for (const name of ["state.export", "future.list", "approvals.list.extra", "ACL.list", "acl.grant"]) {
      const out = filterRequest(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name, annotations: { readOnlyHint: true } } }), new Set());
      expect(out.forward).toBeUndefined();
      const result = JSON.parse(out.reply ?? "").result;
      expect(result.structuredContent.error.message).toBe(`${name} is not supported on this read-only connection.`);
      expect(result.content[0].text).not.toContain("is a write");
      expect(result.structuredContent.error.next_action).toBe("not_allowed");
    }
  });
});

describe("runReadOnlyProxy over streams", () => {
  it("discovers and relays governance reads without changing backend scope or crossing connections", async () => {
    // Two synthetic backend connections; the Rust backend remains responsible for tenancy and
    // redaction. This checks transport separation and exact response/denial preservation only.
    const makeConnection = () => {
      const ends = { clientIn: new PassThrough(), clientOut: new PassThrough(), serverIn: new PassThrough(), serverOut: new PassThrough() };
      const requests: string[] = [];
      const replies: string[] = [];
      ends.serverIn.on("data", lineSplitter((line) => requests.push(line)));
      ends.clientOut.on("data", lineSplitter((line) => replies.push(line)));
      return { ...ends, requests, replies, done: runReadOnlyProxy(ends) };
    };
    const alice = makeConnection();
    const bob = makeConnection();
    const mutations = ["approvals.decide", "acl.grant", "acl.revoke", "state.update", "state.delete", "memory.ingest"];
    const tools = [...governanceReads, ...mutations, "future.list"].map((name) => ({ name, inputSchema: { type: "object" }, annotations: { readOnlyHint: true } }));
    for (const connection of [alice, bob]) {
      connection.clientIn.write(JSON.stringify({ jsonrpc: "2.0", id: "list", method: "tools/list" }) + "\n");
    }
    // Even the same JSON-RPC id on separate connections cannot share pending discovery state.
    alice.serverOut.write(JSON.stringify({ jsonrpc: "2.0", id: "list", result: { tools, nextCursor: "next-page" } }) + "\n");
    expect(bob.replies).toEqual([]);
    bob.serverOut.write(JSON.stringify({ jsonrpc: "2.0", id: "list", result: { tools } }) + "\n");
    expect(JSON.parse(alice.replies[0] ?? "").result).toEqual({ tools: tools.slice(0, 4), nextCursor: "next-page" });
    expect(JSON.parse(bob.replies[0] ?? "").result.tools).toEqual(tools.slice(0, 4));
    for (const [index, name] of governanceReads.entries()) {
      const args = name === "approvals.get" ? { approval_id: 17 } : name === "audit.list" ? { since_ms: 100, limit: 5 } : {};
      const request = JSON.stringify({ jsonrpc: "2.0", id: index, method: "tools/call", params: { name, arguments: args } });
      alice.clientIn.write(request + "\n");
      bob.clientIn.write(request + "\n");
      expect(alice.requests.at(-1)).toBe(request);
      expect(bob.requests.at(-1)).toBe(request);
      const own = JSON.stringify({ jsonrpc: "2.0", id: index, result: { isError: false, content: [{ type: "text", text: JSON.stringify({ tool: name, principal: "alice", records: [] }) }] } });
      const denied = JSON.stringify({ jsonrpc: "2.0", id: index, result: { isError: true, content: [{ type: "text", text: JSON.stringify({ tool: name, error: "not_found" }) }] } });
      alice.serverOut.write(own + "\n");
      bob.serverOut.write(denied + "\n");
      expect(alice.replies.at(-1)).toBe(own);
      expect(bob.replies.at(-1)).toBe(denied);
      expect(bob.replies.join("\n")).not.toContain("alice");
    }
    for (const connection of [alice, bob]) {
      const before = [...connection.requests];
      for (const name of mutations) {
        for (const id of [99, undefined]) {
          connection.clientIn.write(JSON.stringify({ jsonrpc: "2.0", id, method: "tools/call", params: { name, arguments: { approval_id: 17, access: "write" } } }) + "\n");
        }
      }
      expect(connection.requests).toEqual(before);
      connection.serverOut.end();
    }
    await Promise.all([alice.done, bob.done]);
  });

  it("relays, refuses and filters on a live stream, and ends when the server does", async () => {
    const clientIn = new PassThrough();
    const clientOut = new PassThrough();
    const serverIn = new PassThrough();
    const serverOut = new PassThrough();
    const seenByServer: string[] = [];
    serverIn.on("data", lineSplitter((l) => seenByServer.push(l)));
    const seenByClient: string[] = [];
    clientOut.on("data", lineSplitter((l) => seenByClient.push(l)));
    const done = runReadOnlyProxy({ clientIn, clientOut, serverIn, serverOut });

    // Chunk boundaries do not matter.
    const list = JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" });
    const write = JSON.stringify({ jsonrpc: "2.0", id: 2, method: "tools/call", params: { name: "memory.ingest" } });
    clientIn.write(list.slice(0, 10));
    clientIn.write(list.slice(10) + "\n" + write + "\n");
    await new Promise((r) => setImmediate(r));
    expect(seenByServer).toEqual([list]);
    expect(seenByClient).toHaveLength(1);
    expect((JSON.parse(seenByClient[0] ?? "") as { id: number }).id).toBe(2);

    serverOut.write(JSON.stringify({ jsonrpc: "2.0", id: 1, result: { tools: [{ name: "memory.recall" }, { name: "memory.ingest" }] } }) + "\n");
    serverOut.end();
    await done;
    const listed = JSON.parse(seenByClient[1] ?? "") as { result: { tools: Array<{ name: string }> } };
    expect(listed.result.tools.map((t) => t.name)).toEqual(["memory.recall"]);
  });
});
