#!/usr/bin/env bash
set -euo pipefail

# Engram (`mem`) End-to-End Showcase:
#   1. Import curated Obsidian Vault
#   2. Zero-copy branch for two parallel Subagents
#   3. Verify strict branch isolation during search
#   4. Diff & 3-way merge winning branch into main
#   5. Demonstrate anti-hallucination rewind & GC

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

export PATH="${HOME}/.cargo/bin:${PATH}"

if command -v mem >/dev/null 2>&1; then
  MEM_BIN="mem"
elif [[ -x "${REPO_ROOT}/target/release/mem" ]]; then
  MEM_BIN="${REPO_ROOT}/target/release/mem"
elif [[ -x "${REPO_ROOT}/target/debug/mem" ]]; then
  MEM_BIN="${REPO_ROOT}/target/debug/mem"
else
  echo "Building 'mem' binary..."
  cargo build --offline -p mem-cli --manifest-path "${REPO_ROOT}/Cargo.toml"
  MEM_BIN="${REPO_ROOT}/target/debug/mem"
fi

DEMO_DIR="$(mktemp -d)"
trap 'rm -rf "$DEMO_DIR"' EXIT
DB_PATH="${DEMO_DIR}/engram_demo.db"

echo "==> [1/6] Initialize Engram vault at ${DB_PATH}"
"$MEM_BIN" --db "$DB_PATH" --provider disabled init

echo "==> [2/6] Import sample Obsidian Vault and commit snapshot"
"$MEM_BIN" --db "$DB_PATH" --provider disabled write /core/rules.md \
  "Never modify production schemas without branching first." --importance 1.0 --tags core,safety
"$MEM_BIN" --db "$DB_PATH" --provider disabled vault import "${SCRIPT_DIR}/obsidian-vault" --branch main --tier semantic
BASE_COMMIT="$("$MEM_BIN" --db "$DB_PATH" --provider disabled commit -m "import obsidian architecture vault" --json | sed -n 's/.*"commit_id": "\([^"]*\)".*/\1/p')"
echo "    Committed baseline: ${BASE_COMMIT:0:12}"

echo "==> [3/6] Create zero-copy parallel branches for Subagent A and Subagent B"
"$MEM_BIN" --db "$DB_PATH" --provider disabled branch subagent/sqlite-opt --from main
"$MEM_BIN" --db "$DB_PATH" --provider disabled branch subagent/postgres-eval --from main

"$MEM_BIN" --db "$DB_PATH" --provider disabled write --branch subagent/sqlite-opt /procedural/tuning.md \
  "Use mmap_size=268435456 for fast local read sweeps." --importance 0.85
"$MEM_BIN" --db "$DB_PATH" --provider disabled commit --branch subagent/sqlite-opt -m "add sqlite mmap tuning"

"$MEM_BIN" --db "$DB_PATH" --provider disabled write --branch subagent/postgres-eval /semantic/pg-notes.md \
  "PostgresSecretToken requires external server deployment." --importance 0.70
"$MEM_BIN" --db "$DB_PATH" --provider disabled commit --branch subagent/postgres-eval -m "postgres evaluation"

echo "==> [4/6] Verify strict branch isolation & 1-hop Obsidian Wiki recall"
echo "    Searching 'PostgresSecretToken' on main (expect 0 results):"
"$MEM_BIN" --db "$DB_PATH" --provider disabled search "PostgresSecretToken" --branch main

echo "    Searching 'BUSY_TIMEOUT' on main (expect sqlite-cas + 1-hop linked runbook & MOC):"
"$MEM_BIN" --db "$DB_PATH" --provider disabled search "BUSY_TIMEOUT" --branch main -k 4

echo "==> [5/6] Diff & merge winning branch (subagent/sqlite-opt) into main"
"$MEM_BIN" --db "$DB_PATH" --provider disabled diff main subagent/sqlite-opt
"$MEM_BIN" --db "$DB_PATH" --provider disabled merge subagent/sqlite-opt main

echo "==> [6/6] Simulate hallucination write -> instant rewind -> GC"
"$MEM_BIN" --db "$DB_PATH" --provider disabled write /semantic/hallucination.md \
  "BadHallucinationFact: always disable WAL mode."
"$MEM_BIN" --db "$DB_PATH" --provider disabled commit -m "hallucinated note"
"$MEM_BIN" --db "$DB_PATH" --provider disabled rewind "$BASE_COMMIT" --branch main
"$MEM_BIN" --db "$DB_PATH" --provider disabled branch -d subagent/sqlite-opt
"$MEM_BIN" --db "$DB_PATH" --provider disabled branch -d subagent/postgres-eval
"$MEM_BIN" --db "$DB_PATH" --provider disabled gc

echo "Demo completed successfully."
