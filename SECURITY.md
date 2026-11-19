# Security Policy

## Architectural Security Properties

Engram (`mem`) enforces the following guarantees at the core engine level:

1. **VFS Path Traversal Prevention**: `VirtualPath::new` strictly rejects relative paths, `..` and `.` segments, backslashes (`\`), NUL bytes (`\0`), control characters, and unknown root namespaces outside `/core`, `/episodic`, `/semantic`, and `/procedural`.
2. **Cryptographic Integrity Verification**: Every blob retrieved from SQLite via `get_blob_conn` is decompressed, checked against its stored `raw_size`, and verified against its `BLAKE3` content hash.
3. **Strict Branch & Revision Isolation**: Hybrid search queries (`fts_chunks`, `chunk_index`, `blob_links`) filter against an explicit active `blob_id` projection for the target `Revision`, preventing cross-branch data or backlink leakage.
4. **Local Credential Protection**:
   - `~/.ai-memory/config.json` is written with `0600` (`rw-------`) file permissions on Unix when persisting API keys.
   - HTTPS API requests executed via `curl` fallback pass JSON payloads via `stdin` (`--data-binary @-`) rather than command-line body arguments.
   - Never commit `.db`, `.env`, or `config.json` files containing API keys into version control.

---

## Supported Versions

| Version | Supported |
| :--- | :--- |
| `0.1.x` (`main`) | Yes |

---

## Reporting a Vulnerability

If you discover a security vulnerability (such as path traversal, SQL injection, cross-branch leakage, or credential exposure):

1. **Do not** disclose the issue in a public GitHub issue prior to coordination.
2. Open a private GitHub Security Advisory on `https://github.com/z23ng/engram/security/advisories/new` with reproduction steps and affected versions.
3. We aim to acknowledge reports within 48 hours and provide a fix release promptly.
