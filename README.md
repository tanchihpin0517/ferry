# Ferry

Ferry is a fast replacement for rsync. It is being built: so far it copies
and syncs trees on one machine, to and from a host over SSH, and between two
hosts. See [Usage](#usage) below, the [CLI](docs/cli.md) for what works,
the [design overview](docs/design/overview.md) for where it is going, and
[TODO.md](TODO.md) for the order it is built in.

## Install

```text
curl -fsSL https://github.com/tanchihpin0517/ferry/releases/latest/download/install.sh | sh
```

This installs the latest release as `~/.local/bin/ferry`, or in
`FERRY_INSTALL_DIR`, after checking it against the release's `SHA256SUMS`.
Releases are for Linux, static, so any distribution, and macOS, each on
x86_64 and aarch64. The binaries can also be downloaded by hand from the
[releases](https://github.com/tanchihpin0517/ferry/releases), with their
`SHA256SUMS`.

A host needs nothing installed: the first time Ferry reaches it, Ferry asks,
then downloads the same release for the host's platform and uploads it over
SSH, so the host needs no network access. Nothing else on the host is
changed. See [Installing on a host](docs/cli.md#installing-on-a-host).

### From source

With a Rust toolchain:

```text
git clone https://github.com/tanchihpin0517/ferry
cd ferry
cargo install --locked --path .
```

A build from source is a dev build: on a host, it uploads its source and
builds it there, so the host needs a Rust toolchain too.

## Usage

```text
ferry copy SOURCE DESTINATION
ferry sync SOURCE DESTINATION
```

`copy` makes DESTINATION hold everything in SOURCE, keeping anything else it
has; `sync` also deletes from DESTINATION what SOURCE does not have. Either
side may be a local path or `[user@]host:path`, over SSH, and both may be on
hosts.

```text
ferry copy ./photos ./backup/photos          # on this machine
ferry copy ./photos workbox:backup/photos    # push to a host
ferry copy workbox:results ./results         # pull from a host
ferry sync ./site web1:/srv/site             # mirror, deleting extras
ferry copy --dry-run ./photos workbox:photos # list what would be sent
ferry copy --ssh-connections 4 ./data workbox:data  # over 4 TCP connections
ferry copy hostA:data hostB:data             # host to host, through here
```

SOURCE's contents always go into DESTINATION, whether SOURCE ends with a
slash or not: `data/x` becomes `backup/x`, never `backup/data/x`.

What is kept: regular files, directories, symlinks, FIFOs, sockets, and
devices, with permissions, mtimes, and owners, as `rsync -a` keeps them.
Each file is checked against a BLAKE3 hash before it takes its name, and is
`fsync`ed, so a file at its final name is always complete. On a terminal, a
dashboard shows the transfer as it goes; `--no-ui` shows a progress line
instead. A run that fails, or is cancelled with Ctrl-C, can be run again: it
sends only what is missing.

The [CLI reference](docs/cli.md) describes every option and behaviour.

## Coming from rsync

| rsync | Ferry |
|---|---|
| `rsync -a src/ dst/` | `ferry copy src dst` |
| `rsync -a src dst/`, making `dst/src` | `ferry copy src dst/src` |
| `rsync -a --delete src/ dst/` | `ferry sync src dst` |
| `rsync -an src/ dst/`, `rsync -a --dry-run …` | `ferry copy --dry-run src dst` |
| `rsync -az src/ host:dst/` | `ferry copy src host:dst`: compression is on by default |
| `rsync -a host:src/ dst/` | `ferry copy host:src dst` |
| two hosts, which rsync cannot do in one command | `ferry copy hostA:src hostB:dst` |
| `rsync -a --progress …`, `-P` | progress is shown on a terminal anyway; `--progress` writes it to a log too |
| `rsync -a --numeric-ids …` | `ferry copy --numeric-ids …` |
| `rsync -a --modify-window=2 …` | `ferry copy --modify-window 2 …` |
| `rsync --rsync-path=PATH …` | `ferry copy --remote-ferry PATH …` |
| `rsync -e 'ssh -p 2222' …` | a `Host` entry in `~/.ssh/config`, or `FERRY_SSH` naming another program |
| `rsync --partial …` | nothing to add: a file at its final name is complete, and a rerun sends only what is missing |
| `rsync -c`, `--exclude`, `-H`, `-S`, `-X`, `-A`, `--bwlimit` | not built yet |

Ferry compares files as rsync's quick check does, by size and mtime, but
sends a changed file whole: it has no delta transfer.

## License

Licensed under the Apache License, Version 2.0. Contributors retain copyright
in their contributions while licensing them under Apache-2.0. See [LICENSE](LICENSE)
and [NOTICE](NOTICE).

Copyright 2026 Chih-Pin Tan.
