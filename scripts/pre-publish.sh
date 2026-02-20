#!/usr/bin/env bash
set -euo pipefail

echo "==> dry-run publish quelay-domain"
cargo publish -p quelay-domain --dry-run

echo "==> dry-run publish quelay-link-sim"
cargo publish -p quelay-link-sim --dry-run

echo "==> pre-publish checks OK"
