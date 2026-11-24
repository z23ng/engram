/**
 * Engram TypeScript SDK example:
 *   Start daemon first: `mem daemon --port 7700`
 *   Run: `node examples/typescript_client.ts`
 */

import { EngramClient } from "../bindings/typescript/src/index.ts";

async function main() {
  const client = new EngramClient({
    baseUrl: process.env.MEM_DAEMON_URL ?? "http://127.0.0.1:7700",
    token: process.env.MEM_DAEMON_TOKEN,
  });

  console.log("Health:", await client.health());

  await client.write({
    path: "/core/agent-goal.md",
    content: "Keep cross-branch queries strictly isolated.",
    importance: 1.0,
    tags: ["core", "invariant"],
  });
  await client.commit("seed core goal");

  const core = await client.compileCore("main", 1000);
  console.log("Compiled /core:\n", core.rendered);

  await client.write({
    path: "/episodic/run-01.md",
    content: "- SOP: Run mem doctor before deploying updates.",
    tags: ["episodic", "ops"],
  });
  await client.commit("record ops episode");

  const consolidated = await client.consolidate({
    branch: "main",
    autoMerge: true,
    archiveProcessed: true,
    deleteBranch: true,
  });
  console.log("Consolidated cards:", consolidated.distilled_cards.map((c) => c.path));
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
