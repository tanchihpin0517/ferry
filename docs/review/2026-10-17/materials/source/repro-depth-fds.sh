#!/bin/sh
# SRC-07: the scan keeps an fd open for every ancestor with a sibling left.
# Usage: FERRY=/path/to/ferry sh repro-depth-fds.sh [DEPTH] [LIMIT]   (default 400 256)
set -eu
FERRY=${FERRY:?set FERRY to a ferry binary}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(git -C "$HERE" rev-parse --show-toplevel)
mkdir -p "$ROOT/.scratch"
WORK=$(mktemp -d "$ROOT/.scratch/work-ferry-depth.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
python3 -I - "$WORK/deep" "${1:-400}" <<'PY'
import os, sys
p, depth = sys.argv[1], int(sys.argv[2])
os.makedirs(p)
for i in range(depth):
    os.mkdir(os.path.join(p, "z"))
    p = os.path.join(p, "a")
    os.mkdir(p)
open(os.path.join(p, "leaf"), "w").write("x")
PY
status=0
( ulimit -n "${2:-256}"; "$FERRY" copy --dry-run --no-ui "$WORK/deep" "$WORK/dst" >/dev/null 2>"$WORK/err" ) || status=$?
echo "exit status: $status"
grep -c "Too many open files" "$WORK/err" || true
grep -m1 "Too many open files" "$WORK/err" | awk -F/ '{print "fails at depth", NF}' || true
