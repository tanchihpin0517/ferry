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

- An owner root cannot set, as where root is squashed, takes setuid away,
  and a group, setgid, with a warning: the file would otherwise run as root
  on the Destination, which the Source never let it. rsync leaves the
  permissions and mtime unset then.
- An owner with no ID on the Destination, as in a user namespace, is a
  warning, and the file is kept, as rsync keeps it. Before, the file failed.
- A failure of a root that is a file names its path, not `.`.

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
