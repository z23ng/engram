---
title: SQLite CAS & WAL Concurrency
aliases: ["BUSY_TIMEOUT", "SQLite 写锁", "database locked"]
tags: ["db/sqlite", "storage/cas"]
entities:
  - "SQLite -> uses -> WAL Mode"
  - "SQLite -> requires -> BUSY_TIMEOUT"
  - "Engram -> compresses_with -> zstd"
importance: 0.95
---
# Storage Engine

## Content-Addressed Blobs
All payloads are hashed with `BLAKE3` before `zstd` compression, guaranteeing global deduplication across all branches and commits. See [[merkle-branching]] for snapshot assembly.

## WAL Concurrency Configuration
- Enable `PRAGMA journal_mode = WAL;` for concurrent readers alongside a writer.
- Set `PRAGMA busy_timeout = 5000;` so short write bursts wait instead of failing immediately.
- If lock contention persists, follow [[sqlite-lock-recovery|SQLite Lock Recovery Runbook]].
