#!/usr/bin/env bash
# Runs the tests, and `ferry test`, on a filesystem of another type, made on a
# loop device: `scripts/filesystem-test.sh xfs`, or `make filesystem-test
# FS=btrfs`. Linux only; needs sudo, and the filesystem's mkfs.
#
# `ext4-casefold` is ext4 with a directory that ignores case, as a Mac's or
# a USB stick's filesystem does: the tests run in it, where two names can be
# one.
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

# Large enough for every filesystem's minimum, and for the tests, which run
# at once: at 2G, Btrfs ran out of room for its metadata, and went read-only.
# Sparse, it takes only what is written.
truncate --size 4G "$image"
case $fs in
    ext4-casefold) sudo mkfs.ext4 -q -O casefold "$image" ;;
    *) sudo "mkfs.$fs" -q "$image" ;;
esac
sudo mount -o loop "$image" "$mount"
sudo chown "$(id -u):$(id -g)" "$mount"
findmnt --noheadings --output TARGET,FSTYPE "$mount"

# Where the tests run: on ext4-casefold, a directory that ignores case, which
# what is made in it inherits.
work=$mount
if [ "$fs" = ext4-casefold ]; then
    work=$mount/casefold
    mkdir "$work"
    chattr +F "$work"
    lsattr -d "$work"
fi

cargo build
TMPDIR=$work make test
mkdir "$work/both" "$work/across"
./target/debug/ferry test "$work/both" "$work/both"
./target/debug/ferry test "$here" "$work/across"
