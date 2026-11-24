# `@engram-memory/sdk` — TypeScript SDK

Zero-dependency TypeScript SDK for **Engram (`mem`)** running natively on Node 22/24+ ESM & standard Web `fetch`.

## Remote Daemon (`mem-daemon`) Usage

```ts
import { EngramClient } from "@engram-memory/sdk";

const mem = new EngramClient({
  baseUrl: "http://127.0.0.1:7700",
  token: process.env.MEM_DAEMON_TOKEN,
});

await mem.createBranch("agent/worker-1", "main");
await mem.write({
  path: "/semantic/wal.md",
  content: "---\naliases: [\"BUSY_TIMEOUT\"]\n---\nAlways set busy_timeout=5000.",
  branch: "agent/worker-1",
  importance: 0.9,
});

const hits = await mem.search({ query: "BUSY_TIMEOUT", branch: "agent/worker-1" });
console.log(hits);
```

## Local Single-File SQLite Mode

```ts
import { LocalEngram } from "@engram-memory/sdk";

const local = new LocalEngram({ dbPath: "./memory.db" });
await local.init();
await local.write({ path: "/core/goal.md", content: "Ship reliable agents.", importance: 1.0 });
const core = await local.compileCore();
console.log(core.rendered);
```
