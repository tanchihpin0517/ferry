#!/bin/sh
# TRN-02: the script `release_script` in src/remote/install.rs generated at
# 3c17a2f, as it ran on a host, with the host's directory as an argument
# rather than written in. It installs whatever arrives on its input, however
# short: `cat` ends with 0 at the end of its input, so `set -e` never stops it.
# Usage: head -c 1000 /bin/ls | sh release_script.sh HOST_DIR
set -e
dir=${1:?usage: sh release_script.sh HOST_DIR < BINARY}
build='0.14.4'
mkdir -p "$dir/bin/$build"
cat > "$dir/bin/$build/ferry.new"
chmod 755 "$dir/bin/$build/ferry.new"
mv "$dir/bin/$build/ferry.new" "$dir/bin/$build/ferry"
