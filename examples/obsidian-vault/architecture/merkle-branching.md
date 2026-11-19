---
title: Merkle Tree Branching & Rewind
aliases: ["Zero-Copy Branch", "Time-Travel Rewind"]
tags: ["vcs/merkle", "architecture"]
entities:
  - "Engram -> snapshots_with -> Merkle Tree"
  - "Engram -> isolates -> Subagent Branches"
importance: 0.90
---
# Versioned Memory Kernel

## Zero-Copy Branching
Creating a new branch inserts a single row in `refs` pointing to the parent `CommitId`. Even with 10,000+ active memory nodes, branching takes `< 1ms` and adds `0 bytes` of duplicated blob storage.

## Instant Anti-Poisoning Rewind
When a reasoning loop writes hallucinated facts, `mem rewind <commit_id>` atomically resets the branch ref and discards dirty staged workspace entries. Run `mem gc` afterwards to reclaim orphaned blobs and `fts_chunks`.

Related storage details: [[sqlite-cas]].
