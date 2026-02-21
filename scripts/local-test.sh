#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

"$SCRIPT_DIR/ci-lint.sh"
"$SCRIPT_DIR/ci-test.sh"
"$SCRIPT_DIR/ci-e2e-test.sh"

echo "==> all checks passed"
