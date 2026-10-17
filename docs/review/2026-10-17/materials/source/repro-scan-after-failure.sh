#!/bin/sh
# SRC-02: the Destination fails at its root, yet the Source scans its whole tree.
# Usage: FERRY=/path/to/ferry sh repro-scan-after-failure.sh
set -eu
FERRY=${FERRY:?set FERRY to a ferry binary}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(git -C "$HERE" rev-parse --show-toplevel)
mkdir -p "$ROOT/.scratch"
WORK=$(mktemp -d "$ROOT/.scratch/work-ferry-scanfail.XXXXXX")
trap 'chmod 755 "$WORK/ro" 2>/dev/null; rm -rf "$WORK"' EXIT
python3 -I "$HERE/mk.py" "$WORK/tree" 1000 1000
mkdir "$WORK/ro" && chmod 555 "$WORK/ro"
"$FERRY" copy --no-ui --profile-file "$WORK/p.json" "$WORK/tree" "$WORK/ro/dst" || true
python3 -I - "$WORK/p.json" <<'PY'
import json, sys
p = json.load(open(sys.argv[1]))
for side in p["sides"]:
    if "Source" not in (side.get("side") or ""):
        continue
    for role in side["roles"]:
        if role["role"] in ("scan", "stat", "manifest"):
            counts = {s["state"]: s["count"] for s in role["states"]}
            print(role["role"], counts)
print("1,000 directories + root: a full scan shows 'list directories' ~2001 and 'stat entries' 1001")
PY
