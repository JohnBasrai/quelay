#!/usr/bin/env bash
set -euo pipefail

echo "==> fmt check"
cargo xfmt --check

echo "==> clippy"
cargo clippy --all-targets -- -D warnings

echo "==> lint OK"
