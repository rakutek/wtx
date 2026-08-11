#!/usr/bin/env bash
# Verify the P0 orchestrator contract with a real VM.
# Create a temporary repository and dedicated VM, then delete only resources created by this
# script when it exits.
set -euo pipefail

WTX=${WTX:-$(cd "$(dirname "$0")/.." && pwd)/target/release/wtx}
CHECK_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/wtx-orchestration.XXXXXX")
CHECK_VM="wtx-orchestration-$$"

cleanup() {
  "$WTX" rm "$CHECK_VM" >/dev/null 2>&1 || true
  case "$CHECK_ROOT" in
    "${TMPDIR:-/tmp}"/wtx-orchestration.*) rm -rf "$CHECK_ROOT" ;;
    *) echo "refusing to remove unexpected path: $CHECK_ROOT" >&2 ;;
  esac
}
trap cleanup EXIT

git -C "$CHECK_ROOT" init -q -b main
git -C "$CHECK_ROOT" config user.name wtx-check
git -C "$CHECK_ROOT" config user.email wtx-check@localhost
echo base > "$CHECK_ROOT/base.txt"
git -C "$CHECK_ROOT" add base.txt
git -C "$CHECK_ROOT" commit -qm init

FIRST=$(
  "$WTX" ensure "$CHECK_VM" "$CHECK_ROOT" --json
)
printf '%s' "$FIRST" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["schema_version"] == 2
assert d["action"] == "created"
assert d["instance"]["ready"] is True
assert d["instance"]["runtime"]["docker"] == "ready"
'

SECOND=$("$WTX" ensure "$CHECK_VM" "$CHECK_ROOT" --json)
printf '%s' "$SECOND" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["action"] == "reused"
'

"$WTX" stop "$CHECK_VM" >/dev/null
THIRD=$("$WTX" ensure "$CHECK_VM" "$CHECK_ROOT" --json)
printf '%s' "$THIRD" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["action"] == "started"
assert d["instance"]["ready"] is True
'

if "$WTX" ensure "$CHECK_VM" "$CHECK_ROOT" --from another-seed --json >/dev/null 2>&1; then
  echo "ensure unexpectedly accepted a different seed for an existing VM" >&2
  exit 1
fi

INSPECT=$("$WTX" inspect "$CHECK_VM" --json)
printf '%s' "$INSPECT" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["schema_version"] == 2
assert d["instance"]["worktree"]["orphaned"] is False
assert len(d["instance"]["worktree"]["head"]) == 40
'

"$WTX" exec "$CHECK_VM" --tty bash -c 'test -t 0 && test -t 1'

REMOVED=$("$WTX" rm "$CHECK_VM" --if-exists --json)
printf '%s' "$REMOVED" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["schema_version"] == 2
assert d["action"] == "deleted"
assert d["name"] == sys.argv[1]
' "$CHECK_VM"

MISSING=$("$WTX" rm "$CHECK_VM" --if-exists --json)
printf '%s' "$MISSING" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["schema_version"] == 2
assert d["action"] == "not_found"
assert d["name"] == sys.argv[1]
' "$CHECK_VM"

echo "P0 orchestration contract: PASS"
