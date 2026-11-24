import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { EngramClient, EngramError, LocalEngram } from "../src/index.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "../../..");

function resolveBinary(envVar: string, name: string): string {
  const fromEnv = process.env[envVar];
  if (fromEnv && fs.existsSync(fromEnv)) return fromEnv;
  const rel = path.join(repoRoot, "target", "release", name);
  if (fs.existsSync(rel)) return rel;
  const dbg = path.join(repoRoot, "target", "debug", name);
  if (fs.existsSync(dbg)) return dbg;
  throw new Error(`Could not locate ${name} binary (set ${envVar} or run cargo build)`);
}

function getFreePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address();
      if (!addr || typeof addr === "string") {
        reject(new Error("unexpected address"));
        return;
      }
      const { port } = addr;
      srv.close(() => resolve(port));
    });
  });
}

test("EngramClient full HTTP REST, branch isolation, consolidation, and SSE flow", async () => {
  const daemonBin = resolveBinary("MEM_DAEMON_BIN", "mem-daemon");
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "engram-ts-daemon-"));
  const dbPath = path.join(tmpDir, "daemon.db");
  const port = await getFreePort();
  const token = "ts-sdk-secret";

  const proc = spawn(
    daemonBin,
    [
      "--host",
      "127.0.0.1",
      "--port",
      String(port),
      "--db",
      dbPath,
      "--provider",
      "disabled",
      "--token",
      token,
    ],
    { stdio: "pipe" },
  );

  try {
    const client = new EngramClient({
      baseUrl: `http://127.0.0.1:${port}`,
      token,
    });

    const deadline = Date.now() + 8000;
    while (Date.now() < deadline) {
      try {
        const h = await client.health();
        if (h.status === "ok") break;
      } catch {
        await new Promise((r) => setTimeout(r, 100));
      }
    }

    // Unauthorized request must fail with 401.
    const unauth = new EngramClient({ baseUrl: `http://127.0.0.1:${port}` });
    await assert.rejects(
      () => unauth.status(),
      (err: unknown) => err instanceof EngramError && err.statusCode === 401,
    );

    // Subscribe to live SSE events on main.
    const sseAbort = new AbortController();
    const receivedEvents: string[] = [];
    const ssePromise = (async () => {
      for await (const ev of client.streamEvents({ branch: "main", signal: sseAbort.signal })) {
        receivedEvents.push(ev.event);
        if (receivedEvents.length >= 2) {
          sseAbort.abort();
          break;
        }
      }
    })().catch(() => {
      // AbortController terminates stream cleanly.
    });

    await new Promise((r) => setTimeout(r, 200));

    await client.write({
      path: "/core/rules.md",
      content: "Never leak cross-branch data.",
      importance: 1.0,
      tags: ["core"],
    });
    const c1 = await client.commit("seed core");
    assert.ok(c1.commit_id.length === 64);

    await ssePromise;
    assert.ok(receivedEvents.includes("node_written"));
    assert.ok(receivedEvents.includes("committed"));

    // Branch isolation + 1-hop Wiki search.
    await client.createBranch("exp/ts", "main");
    await client.write({
      path: "/semantic/ts-card.md",
      content: "---\naliases: [\"TS_SECRET\"]\n---\n# TS Card\nIsolated in exp/ts.",
      branch: "exp/ts",
      importance: 0.85,
    });

    assert.equal((await client.search({ query: "TS_SECRET", branch: "main" })).length, 0);
    const branchHits = await client.search({ query: "TS_SECRET", branch: "exp/ts" });
    assert.equal(branchHits.length, 1);

    // Compile /core on main.
    const compiled = await client.compileCore("main", 500);
    assert.ok(compiled.rendered.includes("Never leak cross-branch data."));

    // Episodic log + cold-path consolidate with auto-merge.
    await client.write({
      path: "/episodic/ts-session.md",
      content: "- Runbook: Always run mem doctor after upgrade.",
      branch: "main",
      tags: ["episodic", "ops"],
    });
    await client.commit("seed episodic", "main");

    const report = await client.consolidate({
      branch: "main",
      autoMerge: true,
      archiveProcessed: true,
      deleteBranch: true,
    });
    assert.equal(report.episodes_scanned, 1);
    assert.equal(report.consolidation_branch_deleted, true);
    assert.ok(report.distilled_cards.length >= 1);

    // Rewind back to c1 and GC.
    await client.rewind(c1.commit_id, "main");
    const gc = await client.gc();
    assert.ok(gc.removed_blobs >= 1);
  } finally {
    proc.kill("SIGTERM");
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
});

test("LocalEngram embedded CLI wrapper works end-to-end", async () => {
  const memBin = resolveBinary("MEM_BIN", "mem");
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "engram-ts-local-"));
  const dbPath = path.join(tmpDir, "local.db");

  try {
    const local = new LocalEngram({ dbPath, memBin, provider: "disabled" });
    const initRes = await local.init();
    assert.equal(initRes.branch, "main");

    await local.write({
      path: "/core/identity.md",
      content: "Engram Local TS SDK",
      importance: 1.0,
    });
    await local.commit("init identity");
    const compiled = await local.compileCore("main", 500);
    assert.ok(compiled.rendered.includes("Engram Local TS SDK"));
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
});
