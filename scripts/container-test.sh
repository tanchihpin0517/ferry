#!/bin/sh
# Runs `make test` inside a Linux distribution's container image, from a clean
# install of the distribution's own packages and the pinned Rust toolchain.
# `make container-test IMAGE=<image>` runs it; the repository is mounted at
# /work, and the build goes to a directory inside the container, so it never
# mixes with the host's target/.
set -eu

if command -v apk >/dev/null 2>&1; then
    # The Makefile runs recipes with bash, which Alpine does not ship.
    apk add --no-cache bash build-base curl
elif command -v apt-get >/dev/null 2>&1; then
    apt-get update
    apt-get install --no-install-recommends --yes build-essential ca-certificates curl
elif command -v dnf >/dev/null 2>&1; then
    # Minimal images ship curl-minimal, which conflicts with the curl package,
    # so curl is installed only when neither is present.
    dnf install --assumeyes gcc make
    command -v curl >/dev/null 2>&1 || dnf install --assumeyes curl
else
    echo "container-test: no known package manager in this image" >&2
    exit 1
fi

# `--default-toolchain none` leaves the choice to rust-toolchain.toml, as on a
# developer machine.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --profile minimal --default-toolchain none
. "$HOME/.cargo/env"

cd /work
rustup show active-toolchain || rustup toolchain install
rustc --version
cargo --version
cat /etc/os-release

CARGO_TARGET_DIR=/tmp/target make test
