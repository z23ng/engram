## Summary
Describe the changes and motivation clearly.

## Invariants & Verification Checklist
- [ ] Preserves CAS immutability (`blobs`, `trees`, `commits` are never mutated in place).
- [ ] Enforces strict branch/revision isolation across reads, search, graph hops, and `/core` compilation.
- [ ] Indices (`chunk_index`, `fts_chunks`, `blob_links`) bind strictly to `blob_id`.
- [ ] `cargo test --offline --all-targets` passes with zero warnings.
- [ ] Relevant documentation (`README.md`, `docs/`) updated if behavior or CLI changed.
