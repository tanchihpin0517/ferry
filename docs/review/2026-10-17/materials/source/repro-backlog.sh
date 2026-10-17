#!/bin/sh
# SRC-04: needed files waiting to be sent overshoot the 100,000 bound.
# Usage: FERRY=/path/to/ferry sh repro-backlog.sh [DIRS] [FILES_PER_DIR]   (default 100 10000)
set -eu
FERRY=${FERRY:?set FERRY to a ferry binary}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(git -C "$HERE" rev-parse --show-toplevel)
mkdir -p "$ROOT/.scratch"
WORK=$(mktemp -d "$ROOT/.scratch/work-ferry-backlog.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
python3 -I "$HERE/mk.py" "$WORK/src" "${1:-100}" "${2:-10000}"
"$FERRY" copy --no-ui --no-fsync --profile-file "$WORK/p.json" "$WORK/src" "$WORK/dst"
python3 -I - "$WORK/p.json" <<'PY'
import json, sys
p = json.load(open(sys.argv[1]))
peak = 0
for sample in p["queues"]:
    for q in sample["queues"]:
        if q["name"] == "needed files":
            peak = max(peak, q["peak"]); room = q["capacity"]
print("needed files: peak", peak, "bound", room)
PY
