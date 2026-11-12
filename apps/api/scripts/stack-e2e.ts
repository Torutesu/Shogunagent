/** Real local stack: SQLCipher sidecar, PostgreSQL gateway, router and SDK over HTTP.
 * Providers are never called. Only a newly created local database and temp stores are written. */
import assert from "node:assert/strict";
import { execFile, spawn, type ChildProcess } from "node:child_process";
import { randomBytes } from "node:crypto";
import { once } from "node:events";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { serve } from "@hono/node-server";
import postgres from "postgres";

import { createRelay, postgresStore, relayConfigFromEnv } from "../src/boot.js";
import { solvePow } from "../src/pow.js";
import { postgresClient } from "../src/sql.js";
import router from "../../router/src/worker.js";
import { ShogunAgent } from "../../../packages/sdk-ts/src/index.js";

const repo = resolve(fileURLToPath(new URL("../../..", import.meta.url)));
const binary = process.env.SHOGUN_API_BIN ?? join(repo, process.env.CARGO_TARGET_DIR ?? "target", "debug", "shogun-api");
assert(existsSync(binary), "Build the sidecar first: cargo build --locked -p shogun-core --features daemon-server --bin shogun-api");
const adminUrl = new URL(process.env.SHOGUN_E2E_POSTGRES_URL ?? "postgres://127.0.0.1:5432/postgres?sslmode=disable");
assert(["127.0.0.1", "localhost", "[::1]"].includes(adminUrl.hostname), "E2E PostgreSQL must be local; the runner creates and removes its own database.");
const database = `shogun_e2e_${randomBytes(8).toString("hex")}`;
const admin = postgres(adminUrl.toString(), { max: 1, connect_timeout: 5, onnotice: () => undefined });
const root = await mkdtemp(join(tmpdir(), "shogun-stack-e2e-"));
const secret = randomBytes(32).toString("hex");
const dbKey = randomBytes(32).toString("hex");
let child: ChildProcess | undefined;
let created = false;
let store: ReturnType<typeof postgresStore> | undefined;
const servers: ReturnType<typeof serve>[] = [];

function pass(message: string): void { console.log(`PASS ${message}`); }

async function startSidecar(port = 0): Promise<string> {
  child = spawn(binary, ["--multi", join(root, "principals")], {
    cwd: root,
    env: { PATH: process.env.PATH ?? "", HOME: root, SHOGUN_DB_KEY: dbKey, SHOGUN_API_PORT: String(port), SHOGUN_SIDECAR_BEARER_SECRET: secret, SHOGUN_APPROVAL_BASE_URL: "https://approve.e2e.invalid", SHOGUN_APPROVAL_SIGNING_KEY: secret },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stderr = "";
  child.stderr?.on("data", (chunk) => { stderr += String(chunk); });
  const sidecarProcess = child;
  return new Promise((resolvePort, reject) => {
    const timer = setTimeout(() => reject(new Error(`Sidecar startup timed out: ${stderr}`)), 30_000);
    sidecarProcess.once("exit", (code) => { clearTimeout(timer); reject(new Error(`Sidecar exited ${code}: ${stderr}`)); });
    const lines = createInterface({ input: sidecarProcess.stdout! });
    lines.on("line", (line) => {
      const match = /listening on http:\/\/127\.0\.0\.1:(\d+)/.exec(line);
      if (match) { clearTimeout(timer); resolvePort(`http://127.0.0.1:${match[1]}`); }
    });
  });
}

async function stopSidecar(): Promise<void> {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, "exit");
  child.kill("SIGTERM");
  const timer = setTimeout(() => child?.kill("SIGKILL"), 5_000);
  try { await exited; } finally { clearTimeout(timer); }
}

async function listen(fetch: (request: Request) => Response | Promise<Response>): Promise<string> {
  const server = serve({ fetch, port: 0, hostname: "127.0.0.1" });
  servers.push(server);
  if (!server.listening) await once(server, "listening");
  const address = server.address();
  assert(address && typeof address === "object");
  return `http://127.0.0.1:${address.port}`;
}

async function json<T = Record<string, unknown>>(url: string, key?: string, body?: unknown): Promise<T> {
  const reply = await fetch(url, { method: body === undefined ? "GET" : "POST", headers: { ...(key ? { authorization: `Bearer ${key}` } : {}), "content-type": "application/json" }, ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(30_000) });
  const data: unknown = await reply.json();
  assert(reply.ok, `${new URL(url).pathname}: HTTP ${reply.status}: ${JSON.stringify(data)}`);
  return data as T;
}

type Wallet = { balance_micro_usd: number };

try {
  await admin.unsafe(`CREATE DATABASE "${database}"`);
  created = true;
  const dbUrl = new URL(adminUrl); dbUrl.pathname = `/${database}`;
  const db = postgresClient(dbUrl.toString());
  const sidecar = await startSidecar();
  const publicKey = JSON.parse(await readFile(join(repo, "packages/config/license-public-key.json"), "utf8")) as { ed25519: string };
  const config = relayConfigFromEnv({ ANTHROPIC_API_KEY: "test-no-provider-calls", OPENAI_API_KEY: "test-no-provider-calls", LICENSE_PUBKEY_B64: publicKey.ed25519, DAILY_SPEND_CAP_USD: "1", SETTLE_INTERVAL_SECONDS: "0", AGENT_API_ENABLED: "1", AGENT_POW_SECRET: secret, AGENT_POW_BITS: "8", AGENT_KEY_PBKDF2_ITERATIONS: "1000", PUBLIC_BASE_URL: "http://127.0.0.1", SHOGUN_SIDECAR_URL: sidecar, SHOGUN_SIDECAR_BEARER_SECRET: secret, ROUTER_SETTLEMENT_TOKEN: secret });
  store = postgresStore(db, config);
  for (const schema of ["apps/api/sql/shared-store.sql", "apps/api/sql/agent-keys.sql", "apps/api/sql/agent-receipts-settled-quantity.sql", "apps/router/sql/run-idempotency.sql", "apps/router/sql/settlement-outbox.sql"]) {
    await db.query(await readFile(join(repo, schema), "utf8"));
  }
  await store.usage.checkSchema(); await store.agent.checkSchema();
  const gateway = await listen(createRelay(config, store).app.fetch);
  const routerEnv = { SHOGUNAGENT_API_BASE_URL: gateway, ROUTER_WALLET_URL: gateway, ROUTER_SETTLEMENT_TOKEN: secret, HYPERDRIVE: { connectionString: dbUrl.toString() } };
  const routerUrl = await listen((request) => router.fetch(request, routerEnv));
  await json(`${gateway}/healthz`); await json(`${routerUrl}/health`);
  pass("Rust sidecar, PostgreSQL gateway and router started");

  async function register(name: string) {
    const challenge = await json<{ challenge: string }>(`${gateway}/v1/agents/challenge`);
    const nonce = solvePow(challenge.challenge, 8); assert(nonce !== undefined);
    const response = await fetch(`${gateway}/v1/agents`, { method: "POST", headers: { "content-type": "application/json", "x-agent-pow": `${challenge.challenge}:${nonce}` }, body: JSON.stringify({ name }) });
    assert.equal(response.status, 201);
    const registered = await response.json() as { key: string };
    assert.equal(typeof registered.key, "string");
    return registered;
  }
  const alice = await register("e2e-alice"); const bob = await register("e2e-bob");
  const client = new ShogunAgent({ apiKey: alice.key, baseUrl: gateway, routerBaseUrl: routerUrl });
  const other = new ShogunAgent({ apiKey: bob.key, baseUrl: gateway, routerBaseUrl: routerUrl });
  const marker = `Nebula-${randomBytes(6).toString("hex")}`;
  const ingest = { turns: [{ role: "user" as const, content: `${marker}: the launch review is on Thursday.` }], threadKey: "e2e", idempotencyKey: "e2e-ingest" };
  const wallet = () => json<Wallet>(`${gateway}/v1/wallet`, alice.key);
  const before = await wallet();
  assert.equal((await client.ingest(ingest)).inserted, 1);
  const after = await wallet();
  assert.equal(before.balance_micro_usd - after.balance_micro_usd, 500);
  assert.equal((await client.ingest(ingest)).inserted, 0);
  assert.equal((await wallet()).balance_micro_usd, after.balance_micro_usd);
  assert(JSON.stringify(await client.recall(marker)).includes(marker));
  assert(!JSON.stringify(await other.recall(marker)).includes(marker));
  pass("registration, SDK ingest, recall, tenant isolation and exactly-once billing");

  const pythonMarker = `Orion-${randomBytes(6).toString("hex")}`;
  await promisify(execFile)(process.env.SHOGUN_E2E_PYTHON ?? "python3", ["-c", `
import os, sys
from shogunagent import ShogunAgent
with ShogunAgent(base_url=sys.argv[1], api_key=os.environ['SHOGUN_E2E_KEY']) as client:
    args = dict(turns=[dict(role='user', content=sys.argv[2] + ': the design review is on Monday.')], idempotency_key='e2e-python')
    assert client.ingest(**args).inserted == 1
    assert client.ingest(**args).inserted == 0
    assert sys.argv[2] in str(client.recall(sys.argv[2]))
`, gateway, pythonMarker], { env: { PATH: process.env.PATH ?? "", HOME: root, PYTHONPATH: join(repo, "packages/sdk-py/src"), SHOGUN_E2E_KEY: bob.key }, timeout: 30_000 });
  assert(!JSON.stringify(await client.recall(pythonMarker)).includes(pythonMarker));
  pass("Python SDK ingest, replay and recall over the real gateway");

  const run = await client.router.run({ tool_id: "shogunagent.memory.recall", input: { query: marker }, idempotency_key: "e2e-router-recall" });
  assert.equal(run.resultType, "result"); assert(JSON.stringify(run.result).includes(marker));
  assert.equal(run.usage?.quantity, 1);
  const commitments = await client.router.run({ tool_id: "shogunagent.state.commitments.list", input: {}, idempotency_key: "e2e-router-state" });
  assert.equal(commitments.resultType, "result");
  const replay = await client.router.run({ tool_id: "shogunagent.memory.recall", input: { query: marker }, idempotency_key: "e2e-router-recall" });
  assert.equal(replay.replayed, true);
  assert.equal((await wallet()).balance_micro_usd, after.balance_micro_usd);
  pass("router → gateway → Rust state reads, durable run replay and free reads");

  const action = await client.actions.execute({ kind: "send_email", to: "e2e@example.invalid", subject: "E2E", body: "This must stay pending." });
  assert.equal(action.type, "approval_required");
  if (action.type === "approval_required") assert.equal((await action.status()).status, "pending");
  pass("L3 action stays pending; nothing sent");

  const tick = await promisify(execFile)(binary, ["--dream-tick", "--multi", join(root, "principals")], {
    cwd: root, env: { PATH: process.env.PATH ?? "", HOME: root, SHOGUN_DB_KEY: dbKey }, timeout: 30_000,
  });
  assert(tick.stdout.includes("dream tick:"));
  pass("Dream Cycle processes principal stores using the local inference lane");

  await stopSidecar();
  await startSidecar(Number(new URL(sidecar).port));
  assert(JSON.stringify(await client.recall(marker)).includes(marker));
  assert.equal((await client.ingest(ingest)).inserted, 0);
  assert.equal((await wallet()).balance_micro_usd, after.balance_micro_usd);
  pass("encrypted state and ingest idempotency survive a sidecar restart");
} finally {
  // Every cleanup step runs even when an earlier one fails, so one bad close never leaves the
  // sidecar, the disposable database or the temp stores behind. A cleanup failure is reported
  // and fails the run, but is not thrown: that would hide the test's own error.
  const cleanup = async (what: string, step: () => unknown) => {
    try {
      await step();
    } catch (error) {
      console.error(`cleanup failed (${what}):`, error);
      process.exitCode = 1;
    }
  };
  for (const server of servers.reverse()) {
    await cleanup("close a server", () => {
      if ("closeAllConnections" in server) server.closeAllConnections();
      return new Promise<void>((resolveClose, reject) => server.close((error) => error ? reject(error) : resolveClose()));
    });
  }
  await cleanup("close the store", () => store?.close());
  await cleanup("stop the sidecar", stopSidecar);
  if (created) {
    await cleanup("drop the database", async () => {
      await admin.unsafe("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname=$1 AND pid<>pg_backend_pid()", [database]);
      await admin.unsafe(`DROP DATABASE "${database}"`);
    });
  }
  await cleanup("close the admin connection", () => admin.end({ timeout: 2 }));
  await cleanup("remove the temp stores", () => rm(root, { recursive: true, force: true }));
}
