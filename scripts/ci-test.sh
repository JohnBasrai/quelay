#!/usr/bin/env bash
set -euo pipefail

echo "==> test"
cargo test --workspace

echo "==> test OK"
