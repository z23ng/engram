#!/usr/bin/env bash
set -euo pipefail

# Engram (`mem`) — Modern One-Line Installer & Onboarding Launcher
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/z23ng/engram/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/z23ng/engram/main/install.sh | sh -s -- --mode ollama --seed-demo

REPO="z23ng/engram"
INSTALL_DIR="${MEM_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="latest"
NO_INSTALL=false
RELEASE_BUILD=false
SKIP_ONBOARD=false
FROM_SOURCE=false
ONBOARD_ARGS=()

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
  C_RESET='\033[0m'
  C_BOLD='\033[1m'
  C_DIM='\033[90m'
  C_GREEN='\033[32m'
  C_YELLOW='\033[33m'
  C_RED='\033[31m'
  C_CYAN='\033[36m'
else
  C_RESET='' C_BOLD='' C_DIM='' C_GREEN='' C_YELLOW='' C_RED='' C_CYAN=''
fi

# Detect whether invoked from inside a local git checkout
SCRIPT_SOURCE="${BASH_SOURCE[0]:-}"
LOCAL_REPO_ROOT=""
if [[ -n "$SCRIPT_SOURCE" && -f "$SCRIPT_SOURCE" ]]; then
  CANDIDATE_DIR="$(cd "$(dirname "$SCRIPT_SOURCE")" && pwd)"
  if [[ -f "$CANDIDATE_DIR/Cargo.toml" && -d "$CANDIDATE_DIR/crates/mem-cli" ]]; then
    LOCAL_REPO_ROOT="$CANDIDATE_DIR"
  fi
fi

usage() {
  cat <<'EOF'
Engram (`mem`) — One-Line Installer & Interactive Onboarding

USAGE:
  install.sh [INSTALLER OPTIONS] [ONBOARD OPTIONS]

INSTALLER OPTIONS:
  --install-dir <DIR>         Target bin directory (default: ~/.local/bin)
  --version <TAG>             Install a specific release tag (default: latest)
  --from-source               Build from source instead of downloading prebuilt binary
  --release                   Build release profile when compiling from local source
  --no-install                Use compiled binary in ./target without copying
  --skip-onboard              Install 'mem' binary only without launching 'mem onboard'
  -h, --help                  Show help

ONBOARD OPTIONS (forwarded to `mem onboard`):
  --mode <ollama|openai|lite> Intelligence profile (interactive prompt if omitted)
  --api-key <KEY>             OpenAI API key (when --mode openai)
  --vault <DIR>               Import an existing local Obsidian Vault (.md directory)
  --seed-demo                 Seed 4 Obsidian Wiki starter notes & run smoke search
  --no-seed                   Initialize an empty vault without starter notes
  --client <CLIENT>           MCP target: all | cursor | claude-code | claude-desktop | windsurf
  -y, --yes                   Non-interactive accept defaults
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --install-dir)
      INSTALL_DIR="${2:-}"; shift 2 ;;
    --install-dir=*)
      INSTALL_DIR="${1#*=}"; shift ;;
    --version)
      VERSION="${2:-}"; shift 2 ;;
    --version=*)
      VERSION="${1#*=}"; shift ;;
    --from-source)
      FROM_SOURCE=true; shift ;;
    --release)
      RELEASE_BUILD=true; shift ;;
    --no-install)
      NO_INSTALL=true; shift ;;
    --skip-onboard)
      SKIP_ONBOARD=true; shift ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      ONBOARD_ARGS+=("$1"); shift ;;
  esac
done

printf "\n${C_CYAN}${C_BOLD}┌${C_RESET}  ${C_BOLD}Engram Installer${C_RESET} ${C_DIM}(github.com/${REPO})${C_RESET}\n"
printf "${C_CYAN}${C_BOLD}│${C_RESET}\n"

detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "${os}:${arch}" in
    Linux:x86_64|Linux:amd64)   echo "x86_64-unknown-linux-gnu" ;;
    Linux:aarch64|Linux:arm64)  echo "aarch64-unknown-linux-gnu" ;;
    Darwin:x86_64)              echo "x86_64-apple-darwin" ;;
    Darwin:arm64|Darwin:aarch64) echo "aarch64-apple-darwin" ;;
    *)
      echo "" ;;
  esac
}

verify_sha256() {
  local file="$1"
  local sha_file="$2"
  local expected actual
  expected="$(awk '{print $1}' "$sha_file")"
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$file" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$file" | awk '{print $1}')"
  else
    return 0
  fi
  [[ "$expected" == "$actual" ]]
}

ensure_cargo_in_path() {
  if [[ -d "$HOME/.cargo/bin" && ":$PATH:" != *":$HOME/.cargo/bin:"* ]]; then
    export PATH="$HOME/.cargo/bin:$PATH"
  fi
}

ensure_install_dir_in_shell() {
  local dir="$1"
  if [[ ":$PATH:" == *":${dir}:"* ]]; then
    return 0
  fi
  export PATH="${dir}:$PATH"
  local line="export PATH=\"${dir}:\$PATH\""
  local updated=""
  for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
    if [[ -f "$rc" ]]; then
      if ! grep -Fq "$dir" "$rc" 2>/dev/null; then
        printf "\n# Added by Engram installer\n%s\n" "$line" >> "$rc"
        updated="$rc"
      fi
    fi
  done
  if [[ -n "$updated" ]]; then
    printf "${C_CYAN}${C_BOLD}│${C_RESET}  ${C_DIM}└─ Added %s to PATH in %s${C_RESET}\n" "$dir" "$updated"
  fi
}

TARGET="$(detect_target)"
MEM_BIN=""

if [[ -n "$LOCAL_REPO_ROOT" && ( "$NO_INSTALL" == "true" || "$FROM_SOURCE" == "true" || "$VERSION" == "latest" ) ]]; then
  printf "${C_GREEN}${C_BOLD}◇${C_RESET}  ${C_BOLD}Building 'mem' from local workspace${C_RESET}\n"
  ensure_cargo_in_path
  if ! command -v cargo >/dev/null 2>&1; then
    printf "${C_RED}✖${C_RESET}  Rust toolchain (cargo) not found.\n" >&2
    exit 1
  fi

  BUILD_ARGS=(build -p mem-cli --manifest-path "${LOCAL_REPO_ROOT}/Cargo.toml")
  if [[ "$RELEASE_BUILD" == "true" ]]; then
    BUILD_ARGS+=(--release)
    BUILT_BIN="${LOCAL_REPO_ROOT}/target/release/mem"
  else
    BUILT_BIN="${LOCAL_REPO_ROOT}/target/debug/mem"
  fi

  if ! cargo "${BUILD_ARGS[@]}" --offline >/dev/null 2>&1; then
    cargo "${BUILD_ARGS[@]}"
  fi

  if [[ "$NO_INSTALL" == "true" ]]; then
    MEM_BIN="$BUILT_BIN"
    printf "${C_CYAN}${C_BOLD}│${C_RESET}  └─ Binary   : ${C_GREEN}%s${C_RESET}\n" "$MEM_BIN"
  else
    mkdir -p "$INSTALL_DIR"
    cp "$BUILT_BIN" "${INSTALL_DIR}/mem"
    chmod +x "${INSTALL_DIR}/mem"
    ln -sf "${INSTALL_DIR}/mem" "${INSTALL_DIR}/engram"
    MEM_BIN="${INSTALL_DIR}/mem"
    printf "${C_CYAN}${C_BOLD}│${C_RESET}  ├─ Installed: ${C_GREEN}%s${C_RESET} ${C_DIM}(alias: engram)${C_RESET}\n" "$MEM_BIN"
    ensure_install_dir_in_shell "$INSTALL_DIR"
  fi
else
  # Remote curl|sh path: prefer prebuilt release binary, fallback to cargo install
  DOWNLOADED=false
  if [[ "$FROM_SOURCE" == "false" && -n "$TARGET" ]] && command -v curl >/dev/null 2>&1; then
    if [[ "$VERSION" == "latest" ]]; then
      BASE_URL="https://github.com/${REPO}/releases/latest/download"
    else
      BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
    fi
    ARCHIVE_NAME="mem-${TARGET}.tar.gz"
    TMP_DIR="$(mktemp -d)"
    trap 'rm -rf "$TMP_DIR"' EXIT

    printf "${C_GREEN}${C_BOLD}◇${C_RESET}  ${C_BOLD}Downloading prebuilt binary (${TARGET})${C_RESET}\n"
    if curl -fsSL "${BASE_URL}/${ARCHIVE_NAME}" -o "${TMP_DIR}/${ARCHIVE_NAME}" 2>/dev/null; then
      if curl -fsSL "${BASE_URL}/${ARCHIVE_NAME}.sha256" -o "${TMP_DIR}/${ARCHIVE_NAME}.sha256" 2>/dev/null; then
        if ! verify_sha256 "${TMP_DIR}/${ARCHIVE_NAME}" "${TMP_DIR}/${ARCHIVE_NAME}.sha256"; then
          printf "${C_RED}✖${C_RESET}  SHA256 verification failed for ${ARCHIVE_NAME}\n" >&2
          exit 1
        fi
        printf "${C_CYAN}${C_BOLD}│${C_RESET}  ├─ Checksum : ${C_GREEN}verified (SHA256)${C_RESET}\n"
      fi
      tar -xzf "${TMP_DIR}/${ARCHIVE_NAME}" -C "$TMP_DIR"
      mkdir -p "$INSTALL_DIR"
      mv "${TMP_DIR}/mem" "${INSTALL_DIR}/mem"
      chmod +x "${INSTALL_DIR}/mem"
      ln -sf "${INSTALL_DIR}/mem" "${INSTALL_DIR}/engram"
      MEM_BIN="${INSTALL_DIR}/mem"
      DOWNLOADED=true
      printf "${C_CYAN}${C_BOLD}│${C_RESET}  ├─ Installed: ${C_GREEN}%s${C_RESET} ${C_DIM}(alias: engram)${C_RESET}\n" "$MEM_BIN"
      ensure_install_dir_in_shell "$INSTALL_DIR"
    fi
  fi

  if [[ "$DOWNLOADED" == "false" ]]; then
    ensure_cargo_in_path
    if ! command -v cargo >/dev/null 2>&1; then
      printf "${C_RED}✖${C_RESET}  Prebuilt binary unavailable for '${TARGET:-unknown}' and 'cargo' is not installed.\n" >&2
      printf "   Install Rust via: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh\n" >&2
      exit 1
    fi
    printf "${C_YELLOW}◇${C_RESET}  ${C_BOLD}Release binary unavailable yet — building via Cargo from GitHub${C_RESET}\n"
    mkdir -p "$INSTALL_DIR"
    CARGO_ROOT="$(mktemp -d)"
    trap 'rm -rf "$CARGO_ROOT"' EXIT
    cargo install --locked --git "https://github.com/${REPO}.git" mem-cli --root "$CARGO_ROOT"
    mv "${CARGO_ROOT}/bin/mem" "${INSTALL_DIR}/mem"
    chmod +x "${INSTALL_DIR}/mem"
    ln -sf "${INSTALL_DIR}/mem" "${INSTALL_DIR}/engram"
    MEM_BIN="${INSTALL_DIR}/mem"
    printf "${C_CYAN}${C_BOLD}│${C_RESET}  ├─ Installed: ${C_GREEN}%s${C_RESET} ${C_DIM}(alias: engram)${C_RESET}\n" "$MEM_BIN"
    ensure_install_dir_in_shell "$INSTALL_DIR"
  fi
fi

if [[ "$SKIP_ONBOARD" == "true" ]]; then
  printf "${C_GREEN}${C_BOLD}└${C_RESET}  ${C_GREEN}Installation complete. Run 'mem onboard' anytime to configure your vault.${C_RESET}\n"
  exit 0
fi

printf "${C_CYAN}${C_BOLD}│${C_RESET}\n"
printf "${C_GREEN}${C_BOLD}└${C_RESET}  ${C_DIM}Handing off to 'mem onboard'...${C_RESET}\n"

# When piped via `curl ... | sh`, reattach /dev/tty if available so interactive prompts work.
if [[ ! -t 0 && -r /dev/tty && -w /dev/tty ]]; then
  exec "$MEM_BIN" onboard "${ONBOARD_ARGS[@]}" </dev/tty
else
  exec "$MEM_BIN" onboard "${ONBOARD_ARGS[@]}"
fi
