import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadSite, render } from "../build.mjs";

const site = loadSite();
const output = render(site);

test("release setup links both published architectures and checksums on every reading surface", () => {
  assert.deepEqual(site.setup.platforms.map((p) => p.id), ["x64", "arm64"]);
  for (const platform of site.setup.platforms) {
    assert.match(platform.archive, /^https:\/\/github\.com\/Torutesu\/Shogunagent\/releases\/download\/v0\.1\.0\/shogunagent-v0\.1\.0-(x86_64|aarch64)-unknown-linux-gnu\.tar\.gz$/);
    assert.equal(platform.checksum, `${platform.archive}.sha256`);
    assert.match(platform.sha256, /^[a-f0-9]{64}$/);
    for (const path of ["/index.html", "/index.md"]) {
      assert.ok(output[path].includes(platform.archive));
      assert.ok(output[path].includes(platform.checksum));
    }
  }
  assert.ok(output["/llms.txt"].includes(site.setup.release_url));
  assert.ok(output["/index.html"].includes('href="#setup">Get started locally'));
  assert.ok(output["/index.html"].includes('href="#setup">Start the local preview'));
  assert.ok(site.setup.requirements.includes("glibc 2.39"));
  assert.ok(site.setup.unsupported.includes("No macOS or Windows binaries"));
  assert.ok(site.setup.connect_text.includes("read-only"));
  assert.ok(site.setup.steps[1].description.includes("not model extraction"));
  for (const body of Object.values(output)) assert.doesNotMatch(body, /npx shogunagent|one command, your coding agent knows/);
});

test("all copyable shell examples parse as Bash", () => {
  for (const command of [...site.setup.platforms, ...site.setup.steps].map((x) => x.command)) {
    const result = spawnSync("bash", ["-n"], { input: command, encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
  }
});

test("repeating setup refuses an existing directory before downloading or replacing keys", () => {
  const root = mkdtempSync(join(tmpdir(), "shogun-web-setup-"));
  try {
    const preview = join(root, "existing");
    mkdirSync(preview);
    const key = join(preview, "db.key");
    writeFileSync(key, "synthetic sentinel; do not replace\n");
    const marker = join(root, "download-attempted");
    const bin = join(root, "bin");
    mkdirSync(bin);
    writeFileSync(join(bin, "curl"), `#!/bin/sh\ntouch '${marker}'\nexit 99\n`, { mode: 0o700 });
    for (const platform of site.setup.platforms) {
      const result = spawnSync("bash", ["-c", platform.command], {
        env: { PATH: `${bin}:${process.env.PATH}`, SHOGUN_PREVIEW_DIR: preview }, encoding: "utf8",
      });
      assert.notEqual(result.status, 0);
      assert.equal(readFileSync(key, "utf8"), "synthetic sentinel; do not replace\n");
      assert.throws(() => readFileSync(marker), { code: "ENOENT" });
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
