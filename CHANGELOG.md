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

- A host has one session, where it had two, or more: each session runs the
  login shell and its startup files, which took 1 to 2 s on a cluster's
  login node measured, and 10 to 16 s while it was loaded, where a channel
  took 40 ms. The host's first session probes it, installs Ferry there if
  need be, and becomes Ferry's launcher, which starts the servers, sudo's
  checks for `--remote-sudo`, and the fixtures of `ferry test`, each on a
  channel that runs no shell, so that nothing the shell, or what it left
  running, prints comes into what they say; a server's control connection
  stays on the channel it was started on. On that login node, a copy of 1 KB
  took 1.2 s, against 2.0 s before, as medians of the two builds run in
  turn; on a host whose sessions take 30 ms, it took 0.02 s longer. The
  launcher ends within 20 s of its SSH connection dropping without a word,
  as when the network goes away, as the servers do, rather than once `sshd`
  finds that, which with its defaults takes over two hours: the connection
  the client follows it by carries keepalives, as the stream does, and the
  client likewise says the launcher is gone.
- A release build is downloaded by the host itself, as VS Code's server
  is, with `curl` or `wget`, and checked against the SHA-256 that the
  release's `SHA256SUMS`, which the client fetches, lists for it. Only where
  the host cannot download it, or check it, does the client upload it, as
  before, in the same session. A host with no outbound network access still
  installs.
- Where a host's `sshd` forwards to no Unix socket, but to a port, as with
  `AllowStreamLocalForwarding no`, or before OpenSSH 6.7, Ferry reaches its
  launcher and its servers at a port on loopback, each connection giving a
  secret first, without `--remote-join`. That is left for a host that
  forwards to nothing, and runs each connection to the launcher, and the
  stream, through the shell, in a session of its own.
- Nothing is `fsync`ed any more, as rsync syncs nothing unless given
  `--fsync`: the system writes what Ferry wrote to storage when it will, on
  Linux by default within about half a minute. A power loss, or a crash of
  the system, soon after a transfer may lose what it wrote, and an error
  the system meets writing it back once a file is closed, as a failing
  disk, goes unseen, the file counted copied. On NFS, closing a file sends
  the server what is left and waits for it to be stored, and a file that
  fails to close, as on a full drive there, fails, as rsync checks it.
  Ferry itself stopping, however it stops, still leaves each file at its
  name whole, or not there. Before,
  each file was synced before it took its name, and each directory
  something was made in at the end; on macOS, a barrier was set on the
  drive before each batch of names, and its cache emptied at the end. A
  copy of 200,000 files of 4 KiB from memory to ext4, on three SSDs, took
  6.3 s, and takes 2.0 s now; with a `sync` after it, 3.2 s. `--no-fsync`
  is taken still, and does nothing, so that a command line that gives it
  still runs. `--fsync` is taken, but not built yet, and refused, with exit
  status 2.
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
  copy of 20,200 empty directories took 3.5 s on ext4, where each was
  synced then, and takes 1.1 s now; on NFS, 13.2 s, and 6.8 s now; on WekaFS,
  71 s, and 49 s now, where making them, one at a time, is what is left.
- Directories, symlinks and special files are made, and attributes set,
  by up to 16 threads, a directory each, while the Destination goes on
  comparing. A first copy of 20,200 empty directories took 6.5 s on NFS,
  after the change above, and takes 2.0 s now; on WekaFS, under a fifth of
  the time, measured against each other in turn.
- `sync` goes on with the other directories while it deletes what one has
  extra: the deleting runs on a lane of its own, and only that directory
  waits for it, as nothing is made in it before. A sync that deletes 20,000
  files in one directory and copies 20,000 new ones into another took
  24.6 s on WekaFS and 10.1 s on NFS, and takes 13.5 s and 7.9 s now.
- On Linux, the cores `--cpus` keeps Ferry to are all on one NUMA node,
  where one has enough, and a server on the client's machine keeps to the
  same ones as the client. Before, they were numbered in a row from where
  each process's ID said, and on the machine measured, of two nodes, about
  three runs in ten spanned both: a copy of 200,000 files of 4 KiB from
  memory to memory then took 4.3 s rather than 2.8 s.
- Where the data is compressed, as between two machines, so is the
  manifest, the list of what the Source has, which is most of what a sync
  with little to do sends: as one zstd stream, each directory as it is
  read. On `/usr`, a crate registry's sources and a build's output, it is
  2.6 to 3.4 times smaller. A sync of 220,000 entries with nothing to do,
  over a link of 100 Mbit/s with a round trip of 30 ms, took 1.08 s, and
  takes 0.85 s now, though its sizes and mtimes, as good as random,
  compressed only 1.6 times.
- Files go in blocks: packs of whole files, up to 2 MiB together, and
  segments of 2 MiB of a larger file, each compressed and checked alone,
  a segment by its BLAKE3 chaining value, merged into the file's hash. A
  large file is still read in order, but a segment at a time by whichever
  thread is free, and the files behind it are read beside it, so that they
  no longer wait for all of it: of 20,000 files of
  4 KiB behind one of 4 GiB, copied from memory to memory on one machine,
  half were written after 0.7 s, where none was before 3.1 s. It is written
  a segment at a time, in order, by whichever writer is free. A copy of the
  file of 4 GiB alone took 3.0 s and took 2.9 s then, before the change to
  the transport below.
- 16 files are read at once, whatever their size, in the order needed:
  large files are no longer read one after another. 16 files of 128 MiB
  took 3.2 s to copy from NFS to memory read one after another, and take
  2.0 s now; from a spinning disk, 16.2 s and 15.4 s. Small files are read
  as fast as before. What is needed first is read first, each file by one
  reader: 16 large files ahead of small ones take every reader, and the
  small ones wait for them. On NFS, a tree of the 16 large files and
  20,000 of 4 KiB took 3.5 s, and takes 2.4 s now, but half its small
  files are written after 2.2 s, where they were after 0.5 s. One large
  file ahead of them takes one reader, and the small ones are read beside
  it, more of them than before: from a spinning disk, a tree of a file of
  4 GiB and 20,000 of 4 KiB has half its small files written by 8 to 12 s,
  where it was 19 to 27 s, the whole up to a tenth slower.
- Files written take their names within a second, though fewer than 256
  wait for a batch: one not full goes to be renamed a second after its
  first file came. Before, the last batch of a transfer, as of small
  files written while a large file was still sent, took them only at the
  end. Of 20,000 files of 4 KiB copied beside one of 4 GiB, from memory to
  ext4, the last took its name after 1.8 to 2.0 s, where it waited for the
  end, at 3.6 s; 200,000 files of 4 KiB copy as fast as before.
- The Destination gives the Source a credit of 64 MiB of data, given back
  as blocks are written, in place of fixed queues on both sides: what both
  hold is bounded by it, so the Destination reads its connection without
  waiting for room, and the list of what the Source has, and the
  Destination's messages, never wait behind data there.
- `sync` asks for a directory's files as soon as it is compared, and writes
  them while what it has extra is deleted, where files are written without
  a name, as on Linux's own filesystems: they take their names once the
  extras are gone. A sync deleting 20,000 files in one directory and
  copying 20,000 into it took 0.62 s to ext4, and takes 0.52 s now. Where
  files are written under a temporary name, as on NFS and macOS, they are
  asked for once the extras are deleted, as before: there, writing them
  meanwhile only takes turns with the deleting at the directory's lock, and
  the same sync on NFS took 19.3 s so, against 17.0 s.
- A dry run of `sync` finds what a run would delete, or keep, and what would
  replace a directory, on the threads that compare, eight at once, rather
  than on one: listing 200,000 files to copy and as many to delete, from
  memory to memory, took 0.38 s, and takes 0.23 s now.
- A copy of many small files is a little faster, the files handed to the
  writers by the thread that decides what to do with each, which no longer
  closes them itself: 200,000 files of 4 KiB from memory to tmpfs took
  1.70 s, and take 1.56 s now.
- Once a transfer fails, the Destination says, and does, nothing more: an
  entry it fails to make after the cause is not reported, and no directory
  more is finished, its owner, permissions and mtime set. Those it let
  itself into get their permissions back, as before.
- The client no longer passes on what one server sends the other: it
  makes the stream between them, a socket pair whose two ends it hands to
  the servers, and touches none of what goes on it. On a host, the
  stream's end is a channel SSH forwards to the server's socket, as
  before; between two hosts, the client joins their two channels back to
  back. The peers' packets go on the stream as they are, and what the
  servers report comes to the client on their control connections. A copy
  of 1 GiB on one machine took 0.52 to 0.57 s of the client's CPU, and
  takes under 10 ms now; from memory to memory, a file of 4 GiB took
  3.0 s, and takes 1.6 to 2.0 s now; 200,000 files of 4 KiB, 1.69 s and
  1.49 s.
- A dropped SSH connection fails the transfer within 20 s, where it took
  30 s, saying why: the stream between the servers carries a keepalive
  once nothing went on it for 5 s, and is taken for broken once nothing
  came on it for 20 s, or as it ends without its end, where each SSH
  connection had a heartbeat of its own; and the client takes a server
  that says nothing on its control connection for 20 s, as when its `ssh`
  is stopped, for gone. Nothing is logged in again, as with rsync: a rerun
  skips the files already complete.
- The dashboard shows each host's SSH connection on one row: its rate, and
  how long since anything came on the stream over it, as the server at the
  far end says, under "heard", where it showed the last heartbeat: yellow
  after 10 s, and red after 13 s, nearing the 20 s at which the stream is
  taken for broken. The client's own queues, which it no longer has, are
  gone, and so are the connection's, as the stream has none of its own:
  what waits for it is the data's. The table of queues is 5 rows shorter.
- Standard error says where every thread is once nothing has moved for
  60 s, no count either server reports changed, where it waited for
  nothing to pass between the servers, which the client no longer sees.

### Removed

- `--ssh-connections`, which opened several SSH connections to a host: a
  host has one, which everything Ferry runs there goes over, as a host
  name may lead to several machines, as a cluster's login nodes behind one
  name do, and a second connection could reach another one than the
  server's. A command line that still gives it is refused, with exit
  status 2.

### Fixed

- A dry run whose listing came faster than it was written out stood still
  10 ms at a time, at times for a second or more in all: the client, finding
  the display's queue full, waited that long before it tried again. It now
  hands each report on as soon as there is room. A dry run of `sync` that
  lists 200,000 files to copy and as many to delete, from memory to memory,
  took 0.35 to 1.8 s, and takes 0.35 to 0.48 s now.
- A Ctrl-C exits with 130 every time, still: once the transfer had failed,
  as its server, which the Ctrl-C also reached, ended, it exited with 1 if
  it looked just as the Ctrl-C was taken, before it was done with. Seen
  once on CI, under load; made certain in a test, by holding the taking up.
- `ferry test` at scale failed with a host whose shell prints as it starts,
  as a tool that warns from a startup file does, saying the trees differ
  though they did not: it took what the shell printed for part of the
  digest `ferry fixture` gave there. `ferry fixture` now runs on a host
  through Ferry's launcher there, on a channel no shell writes to, and what
  the shell printed as the host's session started is skipped, and said, as
  for a transfer. Seen on a cluster's login node.
- With an SSH master already up, your own or one Ferry left running, each
  run started a login shell on the host for nothing, which ran the shell's
  startup files: the `ssh -N` that Ferry runs to log in asks a master it
  finds for a session. Ferry now asks first whether the master is up, and
  runs no `ssh -N` if it is. An `ssh -N` that ended, as one does that
  leaves the master in the background, was also not waited for until the
  run ended; and, where it had ended before the master answered, its
  process ID stayed among those Ferry ends with `SIGTERM` should it exit on
  a signal, though that ID could be another process's by then.

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
