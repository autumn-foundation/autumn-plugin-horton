#!/usr/bin/env bash
# Run Verus on every spec in verification/. Exit 1 if a spec fails.
set -euo pipefail
shopt -s nullglob
cd "$(dirname "$0")/.."
verus="${VERUS_BIN:-verus}"
specs=(verification/*.rs)
if (( ${#specs[@]} == 0 )); then
  echo "error: no Verus specs in verification/" >&2
  exit 1
fi
failed=0
for spec in "${specs[@]}"; do
  echo "==> verus ${spec}"
  if ! "$verus" "$spec"; then
    failed=1
  fi
done
exit "$failed"
