# Changelog

What changed in each release, as a user of Ferry sees it. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Ferry is at 0.x:
a release may change how a command behaves, and each such change is listed
under **Changed**. Releases up to 0.14.4 are on the
[releases page](https://github.com/tanchihpin0517/ferry/releases), as their
commits.

A release's notes are its section here: the release workflow publishes
the section under the tag's version, `scripts/release-notes.sh VERSION`,
and fails if there is none. So, to release, rename **Unreleased** to the
version, with its date, as `## [0.15.0] - 2026-11-01`, and start a new
**Unreleased** above it.

## [Unreleased]

### Changed

- On Linux, a directory is listed by reading the descriptor it was opened
  with, 256 KiB of names at a time. Before, it was opened again to be read,
  one more round trip to the server on NFS, and read a few names at a time,
  each read a round trip on WekaFS: a directory of 5,000 entries there took
  99 ms to list, and takes 22 ms now.
- `sync` deletes what the Source does not have faster on a filesystem over
  the network: up to 16 threads delete at once, each in a directory of its
  own, and a file is unlinked in one round trip, not two, as only a
  directory is opened. Deleting 50,000 files in 500 directories took 44 s
  on WekaFS, and takes 2.3 s now; on NFS, 8.5 s, and 1.7 s now.
- Directories are finished, their owner, permissions and mtime set, by up
  to 16 threads at once, each only after every directory under it. A first
  copy of 20,200 empty directories took 3.5 s on ext4, where each is
  synced, and takes 1.1 s now; on NFS, 13.2 s, and 6.8 s now; on WekaFS,
  71 s, and 49 s now, where making them, one at a time, is what is left.
- Directories, symlinks and special files are made, and attributes set,
  by up to 16 threads, a directory each, while the Destination goes on
  comparing. A first copy of 20,200 empty directories took 6.5 s on NFS,
  after the change above, and takes 2.0 s now; on WekaFS, under a fifth of
  the time, measured against each other in turn.

## [0.16.0] - 2026-10-22

### Changed

- An owner root cannot set, as where root is squashed, takes setuid away,
  and a group, setgid, with a warning: the file would otherwise run as root
  on the Destination, which the Source never let it. rsync leaves the
  permissions and mtime unset then.
- An owner with no ID on the Destination, as in a user namespace, is a
  warning, and the file is kept, as rsync keeps it. Before, the file failed.
- A failure of a root that is a file names its path, not `.`.
- A path in SOURCE that cannot be read exits with 23, as rsync does for a
  file it cannot open; 24 is for paths that vanished as they were read,
  alone. 0.15.0 exited with 24 for both, which misread rsync.
- A SOURCE that is a symlink to a file, or to nothing, is copied as a
  symlink, as rsync copies one. Before, one to a file was followed, and one
  to nothing was refused as missing. A symlink to a directory is still gone
  into, as rsync goes into `SOURCE/`.

### Fixed

- A Ctrl-C exits with 130 every time. It reaches what the transfer
  started too, a server on this machine or `ssh`, which then ends; the
  transfer at times saw it gone, and exited with 1, before it took the
  Ctrl-C itself.

## [0.15.0] - 2026-10-22

### Added

- `-x`, `--one-file-system`, as rsync's `-x`: the scan sends a mount point,
  as a directory, but not what it holds, and `sync` deletes nothing on
  another filesystem than the Destination's root.
- `--cpus N|all`: on Linux, each machine runs Ferry on at most N of its
  cores. The default is 8.
- `--profile`: after the summary, say where each side's threads spent their
  time.
- `-0`, `--null`: with `--dry-run`, list each entry as `+ PATH` or
  `- PATH`, ending in a NUL, for a script.
- `--allow-dangerous-destination`: sync into `/`, a home directory, or a
  directory that holds one, without being asked.

### Changed

- Exit statuses are as rsync's:
  - 23 when some entries could not be copied or deleted;
  - 24 when some paths in SOURCE could not be read, or vanished as they
    were read;
  - 2 for a wrong command line;
  - 3 for `ferry update --check` when a newer release is out;
  - 101 for an internal error.

  Before, 3 was 2, and 23 and 24 were 1.
- A DESTINATION that is SOURCE, is inside it, or holds it is refused, for
  `copy` and `sync`, in a dry run too.
- `sync` asks before it deletes in `/`, in a home directory, or in a
  directory that holds one, such as `host:` alone. Without a terminal, it
  is refused unless `--allow-dangerous-destination` is given; `--yes` does
  not agree to it.
- Where the time went is no longer shown after every summary, only with
  `--profile`.
- A copy between two sides on this machine is not compressed, unless
  `--compress-level` is given.
- A path is printed safe: control characters and bytes that are not UTF-8
  are escaped.
- An owner, permissions, or an mtime that cannot be set is a warning, and
  the file is kept. Before, the file failed.
- A full or read-only drive stops the transfer, rather than failing every
  file after.
- On Linux, a file is written without a name and linked into place, so no
  `.NAME.ferry-XXXX` file is seen on the way, and writing many small files
  no longer contends for each directory's lock. 64 threads `fsync`, so that
  writers go on writing.
- A root that is a symlink to a file is replaced, as rsync does, rather
  than compared through.
- `copy` puts a file copied onto a directory into it, as rsync does,
  without asking. `--yes` now agrees only to installing Ferry on a host:
  before, it agreed to this too, and without a terminal, nothing was copied
  without it.
- `--modify-window` takes -1, or 0 and more, and no other negative number.
- A remote-only option, such as `--remote-sudo`, given with both sides on
  this machine, is warned of.
- A file changed in place while it is read, within the same second, keeping
  its size, is not found changed by the next run, as with rsync. The docs
  no longer promise otherwise.

### Fixed

- A panic no longer hangs a transfer: it is an internal error, said by the
  part of Ferry it was in. Temporary files are removed, and Ferry exits
  with 101.
- A transfer that fails leaves no temporary file behind, and gives back the
  permissions of any directory it let itself into.
- `sync` no longer deletes a tree that another of the Source's names stands
  for, on a filesystem that ignores case.
- A FIFO put where a file was is reported as unreadable, rather than waited
  on for ever.
- A mode is never set through a symlink put where a file was compared.
- Many small files, and deep trees, copy where few files may be open at
  once, as on macOS by default. A tree however deep is deleted without
  running out of stack.
- A slow link no longer fails a connection: the heartbeats waiting behind
  data count as heard.
- A host slow to install or join no longer fails the other side's
  connection.
- An upload cut short is not installed on a host. Preparing a transfer no
  longer waits for hours on an SSH connection gone silent.
- SSH works with any login shell, says why a login failed, and `FERRY_SSH`
  may carry options.
- On a signal, Ferry ends the `ssh` it holds.
- A server of another build is refused, saying both builds.
- `ferry update` syncs the new binary to disk before it takes the name.
- No message grows with a directory, so a directory of millions of entries
  syncs.
- Memory: the Source's scan waits while too many entries are not compared
  yet, and keeps less for each directory still to list.

### Security

- `SECURITY.md` says how to report a vulnerability, and what Ferry trusts.
- The README says releases are not signed, and how to run a macOS binary
  downloaded in a browser, which Gatekeeper refuses.
- Every action the workflows use is pinned by its commit. A release
  publishes `THIRD-PARTY-LICENSES.txt`, and every push checks the
  dependencies for advisories.
