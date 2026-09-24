#!/usr/bin/env bash
set -euo pipefail

echo "==> test"
cargo nextest run --workspace

echo "==> test OK"
