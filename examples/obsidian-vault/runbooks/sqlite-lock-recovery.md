---
title: SQLite Lock Recovery Runbook
aliases: ["Lock Troubleshooting", "锁排查手册"]
tags: ["ops/runbook", "db/sqlite"]
importance: 0.85
---
# Recovery Checklist

1. Verify no uncommitted long-running write transaction is holding an exclusive WAL lock.
2. Confirm `WAL` checkpointing is not blocked by stale read snapshots.
3. Inspect active staging entries with `mem status` and commit or discard stale workspace changes.
