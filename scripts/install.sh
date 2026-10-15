#!/bin/sh
# Installs Ferry: downloads this release's binary for this machine, checks it
# against the release's SHA256SUMS, and puts it in ~/.local/bin, or in
# FERRY_INSTALL_DIR. Each release publishes this script with its version
# written in, so that
#
#   curl -fsSL https://github.com/tanchihpin0517/ferry/releases/latest/download/install.sh | sh
#
# installs the latest release. FERRY_DOWNLOAD_URL replaces where the release
# is downloaded from, for testing.
#
# Everything is in a function called on the last line, so that a download of
# this script cut short runs nothing.
set -eu

version="@VERSION@"
repository=https://github.com/tanchihpin0517/ferry

fail() {
    echo "ferry install: $*" >&2
    exit 1
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1"
    else
        shasum -a 256 "$1"
    fi | awk '{ print $1 }'
}

main() {
    case "$version" in
        *@*) fail "this is the template; use the install.sh a release publishes" ;;
    esac

    os=$(uname -s)
    arch=$(uname -m)
    case "$os/$arch" in
        Linux/x86_64) target=x86_64-unknown-linux-musl ;;
        Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-musl ;;
        Darwin/x86_64) target=x86_64-apple-darwin ;;
        Darwin/arm64 | Darwin/aarch64) target=aarch64-apple-darwin ;;
        *) fail "Ferry has no release for $os on $arch" ;;
    esac

    command -v curl >/dev/null 2>&1 || fail "curl is needed"
    if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
        fail "sha256sum or shasum is needed, to check the download"
    fi

    base=${FERRY_DOWNLOAD_URL:-$repository/releases/download/v$version}
    asset=ferry-$version-$target
    dir=${FERRY_INSTALL_DIR:-$HOME/.local/bin}

    temp=$(mktemp -d)
    trap 'rm -rf "$temp"' EXIT

    echo "Downloading Ferry $version for $target"
    # GitHub redirects a release download to its CDN; never to plain HTTP.
    curl -fsSL --proto-redir =https -o "$temp/$asset" "$base/$asset"
    curl -fsSL --proto-redir =https -o "$temp/SHA256SUMS" "$base/SHA256SUMS"

    # `sha256sum --binary` marks each file with `*`.
    expected=$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1 }' "$temp/SHA256SUMS")
    [ -n "$expected" ] || fail "SHA256SUMS lists no $asset"
    actual=$(sha256 "$temp/$asset")
    [ "$actual" = "$expected" ] || fail "$asset does not match its checksum: $actual, not $expected"

    # Put in place by a rename, so that a ferry already there is replaced whole.
    mkdir -p "$dir"
    cp "$temp/$asset" "$dir/.ferry.new"
    chmod 755 "$dir/.ferry.new"
    mv -f "$dir/.ferry.new" "$dir/ferry"
    echo "Installed Ferry $version as $dir/ferry"

    case ":$PATH:" in
        *":$dir:"*) ;;
        *)
            # shellcheck disable=SC2016 # $PATH is for the profile, unexpanded.
            printf '\n%s is not on your PATH. Add it, such as in your shell profile:\n\n    export PATH="%s:$PATH"\n' "$dir" "$dir"
            ;;
    esac
}

main
