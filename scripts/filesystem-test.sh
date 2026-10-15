#!/usr/bin/env bash
# Runs the tests, and `ferry test`, on a filesystem of another type, made on a
# loop device: `scripts/filesystem-test.sh xfs`, or `make filesystem-test
# FS=btrfs`. Linux only; needs sudo, and the filesystem's mkfs.
#
# The tests' temporary directories go there, through TMPDIR, and `ferry test`
# runs twice: with both sides there, and from the runner's own disk to there,
# so that renames, fsync, and timestamps are tried on that filesystem, and a
# copy between two filesystems too.

set -euo pipefail
cd "$(dirname "$0")/.."

fs=${1:?usage: scripts/filesystem-test.sh FSTYPE}
image=$(mktemp --tmpdir "ferry-$fs-XXXXXX.img")
mount=$(mktemp --directory --tmpdir "ferry-$fs-XXXXXX")
here=$(mktemp --directory --tmpdir "ferry-here-XXXXXX")
cleanup() {
    sudo umount "$mount" 2>/dev/null || true
    rm -rf "$image" "$here"
    rmdir "$mount" 2>/dev/null || true
}
trap cleanup EXIT

# Large enough for every filesystem's minimum, and for the tests.
truncate --size 2G "$image"
sudo "mkfs.$fs" -q "$image"
sudo mount -o loop "$image" "$mount"
sudo chown "$(id -u):$(id -g)" "$mount"
findmnt --noheadings --output TARGET,FSTYPE "$mount"

cargo build
TMPDIR=$mount cargo test
mkdir "$mount/both" "$mount/across"
./target/debug/ferry test "$mount/both" "$mount/both"
./target/debug/ferry test "$here" "$mount/across"
