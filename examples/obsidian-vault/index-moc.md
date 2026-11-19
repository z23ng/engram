---
title: Engram Engineering Map of Content (MOC)
aliases: ["Architecture Index", "架构索引"]
tags: ["moc", "architecture"]
importance: 0.90
---
# Engram Architecture & Operations MOC

Welcome to the curated sample Obsidian Vault for **Engram (`mem`)**.

## Core Architecture
- [[sqlite-cas|SQLite CAS & WAL Storage]] — Single-file BLAKE3 content-addressed storage and lock tuning.
- [[merkle-branching|Merkle Tree Branching & Rewind]] — Zero-copy subagent branches and atomic rollback.

## Operational Runbooks
- [[sqlite-lock-recovery|SQLite Lock Recovery Runbook]] — Step-by-step recovery when concurrent writers hit lock contention.
