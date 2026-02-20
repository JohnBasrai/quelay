#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# thrift-compile.sh — compile quelay.thrift → quelay-thrift/src/gen/quelay.rs
#
# Usage:
#   ./scripts/thrift-compile.sh          # run from workspace root
#
# Requirements:
#   thrift-compiler    sudo apt install thrift-compiler
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

IDL="${WORKSPACE_ROOT}/quelay-thrift/quelay.thrift"
GENDIR="${WORKSPACE_ROOT}/quelay-thrift/src/gen"

# --- sanity checks ----------------------------------------------------------

if ! command -v thrift &>/dev/null; then
    echo "error: thrift compiler not found" >&2
    echo "       sudo apt install thrift-compiler" >&2
    exit 1
fi

if [[ ! -f "${IDL}" ]]; then
    echo "error: IDL not found: ${IDL}" >&2
    exit 1
fi

# --- compile ----------------------------------------------------------------

echo "thrift $(thrift -version)"
echo "IDL:    ${IDL}"
echo "output: ${GENDIR}"

rm -rf   "${GENDIR}"
mkdir -p "${GENDIR}"

thrift -out "${GENDIR}" --gen rs "${IDL}"

# Strip crate-level inner attributes (#![...]) from the generated file.
# They are valid at the crate root but illegal when the file is include!()-ed
# into a mod block in lib.rs.
for f in "${GENDIR}"/*.rs; do
    sed -i 's/^#!\[/\/\/ removed by thrift-compile.sh: #![/' "${f}"
done

echo "done: $(ls "${GENDIR}")"
