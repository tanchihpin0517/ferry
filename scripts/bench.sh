#!/usr/bin/env bash
# Benchmarks Ferry against rsync on generated trees: on this machine, and,
# given a HOST, pushed to it and pulled from it. Prints a Markdown table.
#
# Usage: scripts/bench.sh [HOST [DIR]]
#
# DIR is where the host keeps its trees and its build, relative to its home,
# `ferry-bench` unless given; put it on a real disk, not a tmpfs. The host
# needs rsync and a Rust toolchain: it builds a release of this source, which
# both sides run, since a release client needs a release server.
#
# Each tree is copied fresh, to where nothing is, and some cases again,
# unchanged. Caches are warm: the trees were just made or read.

set -euo pipefail
cd "$(dirname "$0")/.."

host=${1:-}
remote_dir=${2:-ferry-bench}
work=.scratch/bench
trees="small large mixed"

cargo build --release --quiet
ferry=$PWD/target/release/ferry
mkdir -p "$work"
for tree in $trees; do
    [ -d "$work/$tree" ] || "$ferry" fixture generate --kind "$tree" "$work/$tree"
done

now() { perl -MTime::HiRes=time -e 'printf "%.2f", time'; }

results=$work/results.md
printf '| tree | case | tool | seconds |\n|---|---|---|---:|\n' >"$results"

# run TREE CASE TOOL COMMAND...: times COMMAND, and adds a row.
run() {
    local tree=$1 case=$2 tool=$3 start end
    shift 3
    start=$(now)
    if ! "$@" >"$work/run.log" 2>&1; then
        echo "failed: $*" >&2
        cat "$work/run.log" >&2
        exit 1
    fi
    end=$(now)
    local seconds
    seconds=$(perl -e "printf '%.1f', $end - $start")
    printf '| %s | %s | %s | %s |\n' "$tree" "$case" "$tool" "$seconds" | tee -a "$results"
}

for tree in $trees; do
    source=$work/$tree
    out=$work/out
    rm -rf "$out"
    run "$tree" local "rsync -a" rsync -a "$source/" "$out/"
    run "$tree" "local, unchanged" "rsync -a" rsync -a "$source/" "$out/"
    rm -rf "$out"
    run "$tree" local ferry "$ferry" copy "$source" "$out"
    run "$tree" "local, unchanged" ferry "$ferry" copy "$source" "$out"
    rm -rf "$out"
    run "$tree" local "ferry --no-fsync" "$ferry" copy --no-fsync "$source" "$out"
    rm -rf "$out"
    run "$tree" local "ferry --no-compress" "$ferry" copy --no-compress "$source" "$out"
    rm -rf "$out"
done

if [ -n "$host" ]; then
    # A release build on the host, of this source as it is.
    git ls-files -z --cached --others --exclude-standard |
        COPYFILE_DISABLE=1 tar --no-xattrs --null -T - -cf - |
        ssh "$host" "set -e; mkdir -p $remote_dir/src; cd $remote_dir/src; tar -xf -;
            PATH=\$HOME/.cargo/bin:\$PATH cargo build --release --locked --quiet"
    copy=("$ferry" copy --remote-ferry "$remote_dir/src/target/release/ferry")
    there=$host:$remote_dir

    for tree in $trees; do
        source=$work/$tree
        clear_there() { ssh "$host" "rm -rf $remote_dir/out"; }
        clear_there
        run "$tree" push "rsync -a" rsync -a "$source/" "$there/out/"
        run "$tree" "push, unchanged" "rsync -a" rsync -a "$source/" "$there/out/"
        clear_there
        run "$tree" push "rsync -az" rsync -az "$source/" "$there/out/"
        clear_there
        run "$tree" push ferry "${copy[@]}" "$source" "$there/out"
        run "$tree" "push, unchanged" ferry "${copy[@]}" "$source" "$there/out"
        clear_there
        run "$tree" push "ferry --ssh-connections 4" \
            "${copy[@]}" --ssh-connections 4 "$source" "$there/out"
        clear_there

        # The tree on the host, to pull.
        ssh "$host" "rm -rf $remote_dir/$tree"
        "${copy[@]}" "$source" "$there/$tree" >/dev/null 2>&1
        out=$work/out
        rm -rf "$out"
        run "$tree" pull "rsync -a" rsync -a "$there/$tree/" "$out/"
        rm -rf "$out"
        run "$tree" pull ferry "${copy[@]}" "$there/$tree" "$out"
        rm -rf "$out"
        run "$tree" pull "ferry --ssh-connections 4" \
            "${copy[@]}" --ssh-connections 4 "$there/$tree" "$out"
        rm -rf "$out"
        ssh "$host" "rm -rf $remote_dir/$tree"
    done
fi

echo
echo "Results in $results"
