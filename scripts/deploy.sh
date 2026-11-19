#!/usr/bin/env bash
set -euo pipefail

# AI Memory (`mem`) — One-Click Deployment & Setup Wizard
# Supports:
#   - Local Ollama Mode (Dev / Offline GPU)
#   - Production OpenAI Mode (Zero local GPU footprint)
#   - Lite CPU Mode (Zero-model FTS5 BM25 + Obsidian Wiki Graph)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

MODE=""
API_KEY="${OPENAI_API_KEY:-}"
DB_PATH="${MEM_DB_PATH:-$HOME/.ai-memory/memory.db}"
INSTALL_DIR="${HOME}/.local/bin"
VAULT_DIR=""
SEED_DEMO=false
NO_INSTALL=false
RELEASE_BUILD=false
OLLAMA_EMBED_MODEL="${MEM_EMBEDDING_MODEL:-bge-m3}"
OLLAMA_LLM_MODEL="${MEM_LLM_MODEL:-qwen2.5:7b}"

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
  C_RESET='\033[0m'
  C_BOLD='\033[1m'
  C_DIM='\033[2m'
  C_GREEN='\033[32m'
  C_YELLOW='\033[33m'
  C_RED='\033[31m'
  C_CYAN='\033[36m'
else
  C_RESET='' C_BOLD='' C_DIM='' C_GREEN='' C_YELLOW='' C_RED='' C_CYAN=''
fi

ok()   { printf "${C_GREEN}[OK]${C_RESET}   %s\n" "$*"; }
warn() { printf "${C_YELLOW}[WARN]${C_RESET} %s\n" "$*"; }
err()  { printf "${C_RED}[ERR]${C_RESET}  %s\n" "$*" >&2; }
step() { printf "\n${C_CYAN}${C_BOLD}%s${C_RESET}\n" "$*"; }

usage() {
  cat <<'EOF'
AI Memory (`mem`) — One-Click Deployment Script

USAGE:
  ./scripts/deploy.sh [OPTIONS]

OPTIONS:
  --mode <ollama|openai|lite>   Deployment profile (interactive wizard if omitted on TTY)
  --api-key <KEY>               OpenAI API key (when --mode openai)
  --db <PATH>                   SQLite database path (default: ~/.ai-memory/memory.db)
  --install-dir <DIR>           Install 'mem' binary directory (default: ~/.local/bin)
  --vault <DIR>                 Import an existing local Obsidian Vault (.md directory)
  --seed-demo                   Populate demo Obsidian Wiki notes and run a live search test
  --no-install                  Keep compiled binary inside ./target/ without copying
  --release                     Build optimized release binary (cargo build --release)
  -h, --help                    Show this help message

EXAMPLES:
  # Interactive wizard
  ./scripts/deploy.sh

  # Local Ollama (Dev) + demo Obsidian Wiki seed
  ./scripts/deploy.sh --mode ollama --seed-demo

  # Production OpenAI switch
  ./scripts/deploy.sh --mode openai --api-key "sk-..." --release

  # Zero-model Lite mode + import existing Obsidian Vault
  ./scripts/deploy.sh --mode lite --vault ~/Documents/MyObsidianVault
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode)
      MODE="${2:-}"; shift 2 ;;
    --mode=*)
      MODE="${1#*=}"; shift ;;
    --api-key)
      API_KEY="${2:-}"; shift 2 ;;
    --api-key=*)
      API_KEY="${1#*=}"; shift ;;
    --db)
      DB_PATH="${2:-}"; shift 2 ;;
    --db=*)
      DB_PATH="${1#*=}"; shift ;;
    --install-dir)
      INSTALL_DIR="${2:-}"; shift 2 ;;
    --install-dir=*)
      INSTALL_DIR="${1#*=}"; shift ;;
    --vault)
      VAULT_DIR="${2:-}"; shift 2 ;;
    --vault=*)
      VAULT_DIR="${1#*=}"; shift ;;
    --seed-demo)
      SEED_DEMO=true; shift ;;
    --no-install)
      NO_INSTALL=true; shift ;;
    --release)
      RELEASE_BUILD=true; shift ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      err "Unknown option: $1"
      usage
      exit 1 ;;
  esac
done

printf "${C_BOLD}${C_CYAN}"
cat <<'BANNER'
================================================================
   AI Memory (`mem`) — Versioned Structured Memory Engine
   SQLite CAS + Merkle Tree + Obsidian Wiki Graph + Ollama/OpenAI
================================================================
BANNER
printf "${C_RESET}"

# Resolve interactive mode if omitted
if [[ -z "$MODE" ]]; then
  if [[ -t 0 ]]; then
    printf "\nSelect deployment profile:\n"
    printf "  ${C_BOLD}1)${C_RESET} Local Ollama (Dev / Offline GPU)  [bge-m3 + qwen2.5:7b]\n"
    printf "  ${C_BOLD}2)${C_RESET} Production OpenAI API             [text-embedding-3-small + gpt-4o-mini]\n"
    printf "  ${C_BOLD}3)${C_RESET} Lite CPU Mode (Zero Model)        [SQLite FTS5 BM25 + Obsidian Wiki Graph]\n"
    read -r -p "Choose [1/2/3] (default 1): " choice
    case "${choice:-1}" in
      1) MODE="ollama" ;;
      2) MODE="openai" ;;
      3) MODE="lite" ;;
      *) err "Invalid selection: $choice"; exit 1 ;;
    esac
  else
    if command -v ollama >/dev/null 2>&1; then
      MODE="ollama"
    else
      MODE="lite"
    fi
  fi
fi

case "$MODE" in
  ollama|openai|lite) ;;
  *) err "Invalid --mode '$MODE'. Expected: ollama | openai | lite"; exit 1 ;;
esac

# [1/6] Environment & Toolchain Preflight
step "[1/6] Checking Rust toolchain & C compiler..."
if [[ -d "$HOME/.cargo/bin" && ":$PATH:" != *":$HOME/.cargo/bin:"* ]]; then
  export PATH="$HOME/.cargo/bin:$PATH"
fi

if ! command -v cargo >/dev/null 2>&1 || ! command -v rustc >/dev/null 2>&1; then
  err "Rust toolchain (cargo/rustc) not found. Install via: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  exit 1
fi
if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1; then
  err "C compiler (cc/gcc) not found. Install build-essential (Debian/Ubuntu) or gcc."
  exit 1
fi
ok "Toolchain ready: $(rustc --version)"

# [2/6] Model Provider Configuration
step "[2/6] Configuring model provider (${MODE})..."
MEM_PROVIDER="disabled"
EMBED_BASE_URL=""
EMBED_MODEL=""
EMBED_DIM=""
LLM_BASE_URL=""
LLM_MODEL=""

if [[ "$MODE" == "ollama" ]]; then
  MEM_PROVIDER="ollama"
  EMBED_BASE_URL="http://127.0.0.1:11434/v1"
  EMBED_MODEL="$OLLAMA_EMBED_MODEL"
  EMBED_DIM="1024"
  LLM_BASE_URL="http://127.0.0.1:11434/v1"
  LLM_MODEL="$OLLAMA_LLM_MODEL"

  if command -v ollama >/dev/null 2>&1; then
    if curl -fsS --max-time 1 "http://127.0.0.1:11434/api/tags" >/dev/null 2>&1; then
      ok "Ollama daemon detected at http://127.0.0.1:11434"
      if ! ollama list 2>/dev/null | grep -q "${EMBED_MODEL}"; then
        warn "Model '${EMBED_MODEL}' not found locally. Run: ollama pull ${EMBED_MODEL}"
      else
        ok "Embedding model '${EMBED_MODEL}' is ready"
      fi
    else
      warn "Ollama CLI is installed, but daemon is not running on 127.0.0.1:11434."
      printf "       ${C_DIM}Start later with: ollama serve && ollama pull %s && ollama pull %s${C_RESET}\n" "$EMBED_MODEL" "$LLM_MODEL"
      printf "       ${C_DIM}(Search will smoothly use Level-1 BM25 + Obsidian Wiki Graph until Ollama is online.)${C_RESET}\n"
    fi
  else
    warn "Ollama binary is not installed on this machine."
    printf "       ${C_DIM}Install with: curl -fsSL https://ollama.com/install.sh | sh${C_RESET}\n"
    printf "       ${C_DIM}Proceeding with Level-1 BM25 + Obsidian Wiki Graph fallback ready.${C_RESET}\n"
  fi
elif [[ "$MODE" == "openai" ]]; then
  MEM_PROVIDER="openai"
  EMBED_BASE_URL="https://api.openai.com/v1"
  EMBED_MODEL="text-embedding-3-small"
  EMBED_DIM="1536"
  LLM_BASE_URL="https://api.openai.com/v1"
  LLM_MODEL="gpt-4o-mini"

  if [[ -z "$API_KEY" && -t 0 ]]; then
    read -r -s -p "Enter OPENAI_API_KEY (sk-...): " API_KEY
    printf "\n"
  fi
  if [[ -z "$API_KEY" ]]; then
    warn "OPENAI_API_KEY was not provided; write it into mem.env before running production embeddings."
  else
    ok "OpenAI API key configured (embed=${EMBED_MODEL}, llm=${LLM_MODEL})"
  fi
  if ! command -v curl >/dev/null 2>&1; then
    warn "System 'curl' not found; HTTPS requests to api.openai.com require 'curl'."
  fi
else
  MEM_PROVIDER="disabled"
  ok "Lite CPU mode selected: zero external models, pure FTS5 BM25 + Obsidian Wiki Graph."
fi

# [3/6] Build 'mem' Binary
step "[3/6] Building 'mem' binary..."
BUILD_ARGS=(build -p mem-cli)
if [[ "$RELEASE_BUILD" == "true" ]]; then
  BUILD_ARGS+=(--release)
  TARGET_BIN="${REPO_ROOT}/target/release/mem"
else
  TARGET_BIN="${REPO_ROOT}/target/debug/mem"
fi

if ! cargo "${BUILD_ARGS[@]}" --offline --manifest-path "${REPO_ROOT}/Cargo.toml"; then
  warn "Offline cargo build needed index/cache refresh; retrying without --offline..."
  cargo "${BUILD_ARGS[@]}" --manifest-path "${REPO_ROOT}/Cargo.toml"
fi

if [[ "$NO_INSTALL" == "true" ]]; then
  MEM_BIN="$TARGET_BIN"
  ok "Built binary in-place: ${MEM_BIN}"
else
  mkdir -p "$INSTALL_DIR"
  cp "$TARGET_BIN" "${INSTALL_DIR}/mem"
  chmod +x "${INSTALL_DIR}/mem"
  MEM_BIN="${INSTALL_DIR}/mem"
  ok "Installed 'mem' binary to ${MEM_BIN}"
fi

# [4/6] Initialize Repository & Config Files
step "[4/6] Initializing memory database and generating configs..."
DB_DIR="$(dirname "$DB_PATH")"
mkdir -p "$DB_DIR"

ENV_FILE="${DB_DIR}/mem.env"
MCP_CONFIG_FILE="${DB_DIR}/mcp_config.json"

{
  printf "# AI Memory (mem) runtime environment (%s mode)\n" "$MODE"
  printf "export MEM_DB_PATH=%q\n" "$DB_PATH"
  printf "export MEM_PROVIDER=%q\n" "$MEM_PROVIDER"
  if [[ -n "$EMBED_BASE_URL" ]]; then
    printf "export MEM_EMBEDDING_BASE_URL=%q\n" "$EMBED_BASE_URL"
    printf "export MEM_EMBEDDING_MODEL=%q\n" "$EMBED_MODEL"
    printf "export MEM_EMBEDDING_DIMENSIONS=%q\n" "$EMBED_DIM"
    printf "export MEM_LLM_BASE_URL=%q\n" "$LLM_BASE_URL"
    printf "export MEM_LLM_MODEL=%q\n" "$LLM_MODEL"
  fi
  if [[ -n "$API_KEY" ]]; then
    printf "export OPENAI_API_KEY=%q\n" "$API_KEY"
  else
    printf "# export OPENAI_API_KEY=\"sk-...\"\n"
  fi
} > "$ENV_FILE"
chmod 600 "$ENV_FILE"

cat > "$MCP_CONFIG_FILE" <<EOF
{
  "mcpServers": {
    "ai-memory": {
      "command": "${MEM_BIN}",
      "args": ["--db", "${DB_PATH}", "mcp"],
      "env": {
        "MEM_PROVIDER": "${MEM_PROVIDER}",
        "MEM_EMBEDDING_MODEL": "${EMBED_MODEL}",
        "MEM_LLM_MODEL": "${LLM_MODEL}"
      }
    }
  }
}
EOF

# Load newly created environment for subsequent CLI steps
set -a
# shellcheck source=/dev/null
source "$ENV_FILE"
set +a

"$MEM_BIN" --db "$DB_PATH" init
ok "Config generated: ${ENV_FILE}"
ok "MCP client snippet: ${MCP_CONFIG_FILE}"

# [5/6] Seed Demo / Import Obsidian Vault
step "[5/6] Loading knowledge base..."
if [[ -n "$VAULT_DIR" ]]; then
  "$MEM_BIN" --db "$DB_PATH" vault import "$VAULT_DIR" --branch main --tier semantic
  "$MEM_BIN" --db "$DB_PATH" commit -m "import local obsidian vault"
fi

if [[ "$SEED_DEMO" == "true" ]]; then
  "$MEM_BIN" --db "$DB_PATH" write /core/rules.md \
    "Never mutate production database tables without creating a branch snapshot first." \
    --importance 1.0 --tags "core,safety"

  "$MEM_BIN" --db "$DB_PATH" write /semantic/sqlite-wal.md \
    $'---\ntitle: SQLite 并发与锁配置\naliases: ["BUSY_TIMEOUT", "数据库写锁"]\ntags: ["db/sqlite", "storage"]\nentities:\n  - "SQLite -> uses -> WAL Mode"\nimportance: 0.92\n---\n# 存储引擎\n\n## WAL 并发配置\n开启 WAL 模式并设置 busy_timeout=5000，排查 SOP 见 [[/procedural/sqlite-lock-sop.md|锁排查指南]]。'

  "$MEM_BIN" --db "$DB_PATH" write /procedural/sqlite-lock-sop.md \
    $'---\ntitle: 数据库连接锁定排查指南\ntags: ["ops/runbook"]\nimportance: 0.85\n---\n# 排查步骤\n1. 检查长事务是否未提交。\n2. 检查 WAL checkpoint 是否被只读连接阻塞。'

  "$MEM_BIN" --db "$DB_PATH" write /semantic/architecture-moc.md \
    $'---\ntitle: 存储架构导航 (MOC)\ntags: ["moc"]\nimportance: 0.80\n---\n# 存储与检索 MOC\n- 并发参数与锁配置：[[SQLite 并发与锁配置]]'

  "$MEM_BIN" --db "$DB_PATH" commit -m "seed obsidian wiki demo knowledge base"
  ok "Seeded demo Obsidian Wiki notes and committed snapshot."

  printf "\n${C_DIM}--- Live Demo: mem search \"BUSY_TIMEOUT\" ---${C_RESET}\n"
  "$MEM_BIN" --db "$DB_PATH" search "BUSY_TIMEOUT" -k 5

  printf "${C_DIM}--- Live Demo: mem compile ---${C_RESET}\n"
  "$MEM_BIN" --db "$DB_PATH" compile --max-tokens 500
else
  if [[ -z "$VAULT_DIR" ]]; then
    ok "Repository initialized clean (pass --seed-demo or --vault <dir> to pre-populate)."
  fi
fi

# [6/6] Health Check & Quick Start Summary
step "[6/6] Running diagnostics (mem doctor)..."
"$MEM_BIN" --db "$DB_PATH" doctor

printf "\n${C_GREEN}${C_BOLD}Deployment complete!${C_RESET}\n"
printf "Next steps:\n"
printf "  1. Load environment : ${C_BOLD}source %s${C_RESET}\n" "$ENV_FILE"
if [[ "$NO_INSTALL" == "false" && ":$PATH:" != *":${INSTALL_DIR}:"* ]]; then
  printf "  2. Add to PATH      : ${C_BOLD}export PATH=\"%s:\$PATH\"${C_RESET}\n" "$INSTALL_DIR"
fi
printf "  3. Search memory    : ${C_BOLD}%s --db %s search \"your query\"${C_RESET}\n" "$MEM_BIN" "$DB_PATH"
printf "  4. Attach to Agent  : Copy MCP config from ${C_BOLD}%s${C_RESET}\n" "$MCP_CONFIG_FILE"
printf "  5. Prod Switch      : Update MEM_PROVIDER=openai & OPENAI_API_KEY in ${C_BOLD}%s${C_RESET}\n" "$ENV_FILE"
