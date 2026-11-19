#!/usr/bin/env bash
set -euo pipefail

# Wrapper forwarding to the modern root installer + `mem onboard` wizard.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

exec "${REPO_ROOT}/install.sh" "$@"
