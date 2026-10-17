#!/bin/sh
# SRC-01 / SRC-05: a needed file swapped for a FIFO after the scan hangs the copy.
# Usage: FERRY=/path/to/ferry sh repro-fifo.sh [WAIT_SECONDS]
# Prints the client's snapshot (sent with SIGQUIT after WAIT_SECONDS, default 75)
# and whether "nothing has passed between the servers" ever appeared on its own.
set -eu
FERRY=${FERRY:?set FERRY to a ferry binary}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(git -C "$HERE" rev-parse --show-toplevel)
mkdir -p "$ROOT/.scratch"
WAIT=${1:-75}
WORK=$(mktemp -d "$ROOT/.scratch/work-ferry-fifo.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
python3 -I - "$WORK/src" <<'PY'
import os, sys
root = sys.argv[1]
os.makedirs(os.path.join(root, "a")); os.makedirs(os.path.join(root, "b"))
for i in range(300000):
    with open(os.path.join(root, "a", "f%06d" % i), "wb") as f: f.write(b"x")
with open(os.path.join(root, "b", "target"), "wb") as f: f.write(b"hello")
PY
timeout $((WAIT + 10)) "$FERRY" copy --no-ui "$WORK/src" "$WORK/dst" 2>"$WORK/err" &
sleep 0.4
rm "$WORK/src/b/target" && mkfifo "$WORK/src/b/target"
sleep "$WAIT"
pkill -QUIT -f "copy --no-ui $WORK/src" || true
status=0; wait $! || status=$?
echo "exit status: $status (124 = killed by timeout, i.e. hung)"
grep -E "nothing has passed|as asked|reader .*read files|answers" "$WORK/err" || true
