# Ferry design overview

[中文](overview.zh-TW.md)

Status: **draft, under discussion.** Each section says whether it is decided,
proposed, or still open. A proposal is a starting point, not a decision.

## Goal

Ferry replaces rsync. The first thing it has to be better at is speed.

## Design documents

- [Architecture](architecture.md): the client, the servers, the peers, and how
  they connect.
- [Pipeline](pipeline.md): how data moves through a transfer.
- [Dashboard](dashboard.md): what the client shows while a transfer runs.
- [Profiling](profiling.md): where the time of a transfer's threads goes, and
  where each is when it hangs.

Changes to the design still being worked out are in
[docs/drafts/](../drafts/README.md).

## Terms

| Term | Meaning |
|---|---|
| client | The `ferry` command you run. It prepares, starts the two servers of a transfer, relays between them, and shows what they report. It lives as long as the command does |
| server | A `ferry server` process that the client starts through a transport, on a host or on this machine. It runs one peer, and exits when its control connection closes |
| peer | One side of a transfer, always run in a server |
| Source, Destination | The peer that reads the files, and the peer that writes them |
| record | One file in the data stream: header, content, and BLAKE3 hash |
| frame | A 2 MiB piece of the record stream, compressed on its own with zstd. Frame boundaries ignore file boundaries |
| packet | The unit sent over the connection: `[type][length][payload]`. Its type is control, manifest, needs, data, or results. A data packet carries at most 64 KiB of a compressed frame |

## Decided

### Reliability comes first, then speed

Ferry is reliable before it is fast. When the two conflict, reliability wins.

**Everything a transfer needs is ready before it starts.** Every connection is
open and logged in, and every host has its server running, before Ferry scans,
compares, or touches a file. Anything that needs you, such as a password, an
OTP, or the question before a first install, is asked at the start, so a
transfer that has started never stops to wait for you, and never fails halfway
for want of a connection it could have opened first. Preparing takes the time
it needs: none of it is overlapped with the transfer to save time.

**Speed is the speed of the transfer once it starts.** From then on, when
speed conflicts with other goals, speed wins unless a section below says
otherwise. rsync is slow for two structural reasons, and Ferry addresses both:

1. **It runs on one thread.** Ferry parallelises every stage of a transfer.
2. **It handles one file at a time.** Ferry sends many small files together
   and unpacks them on the receiving side.

### Every file is complete, correct, and durable

A file at its final path on the receiver is either the old version or the
complete, verified new version, even after a power loss. Where this conflicts
with speed, integrity wins.

1. **Atomic per file.** Ferry writes each file where nothing sees it, and
   gives it its final name only when the file is complete. On Linux, where
   the filesystem can, a file has no name while it is written (`O_TMPFILE`),
   and takes its own with `linkat`; where something has that name, the file
   takes a temporary one first, and is renamed over it. Elsewhere, as on
   macOS or NFS, each file is written to a temporary name in the same
   directory, such as `.foo.ferry-XXXX`, and renamed. There is no in-place
   mode.
   When a run is interrupted, or fails, as when the Source fails or a
   connection drops, the Destination deletes the temporary files of the
   files it did not finish. A file without a name goes with the process,
   however it ends; only the Destination itself crashing, or being killed,
   can leave a temporary name behind, and only where files are written
   under one.
2. **Verified end to end.** The sender hashes each file with BLAKE3 as it reads
   it, and the receiver hashes it as it writes. The file takes its name only if
   the two hashes match. A file is read up to the size it had when it was opened,
   as rsync does; one that shrinks while it is read, or whose reading fails,
   is reported as an error, and nothing is sent again. One that changes while
   it is read keeps the mtime it had when it was opened, so the next run
   finds it changed.
3. **Durable by default.** Each file is `fsync`ed before it takes its name.
   Without this, a power loss can leave the name on disk but not the data.
   To keep this affordable with many small files, Ferry works in batches: it
   `fsync`s a batch of files in parallel, names the whole batch, then
   `fsync`s each directory once. A root the run made is a new name in the
   directory it is in, which is `fsync`ed at the end too. This is on by
   default, and `--no-fsync` turns it off.

   On macOS, `fsync` only hands data to the drive, which may still reorder it
   in its own cache, so between a batch's `fsync`s and its renames Ferry sets
   one barrier per drive (`F_BARRIERFSYNC`): every file of the batch reaches
   storage before any of their names. When the transfer is done, one
   `F_FULLFSYNC` empties the drive's cache, so what Ferry reports as copied is
   on storage. On Linux, `fsync` already waits for storage, so neither is
   needed.

Ferry does not make the destination tree as a whole atomic. An interrupted run
can leave a mix of old and new files, each one complete. Running Ferry again
brings the destination to the correct state.

### The first version covers at least `rsync -a`

`rsync -a` means `-rlptgoD`, and Ferry's first version preserves all of it:

| rsync | What is preserved |
|---|---|
| `-r` | directories, recursively, including empty ones |
| `-l` | symlinks, copied as links rather than followed |
| `-p` | permission bits, including setuid, setgid, and sticky |
| `-t` | mtime, to the nanosecond where both filesystems allow it |
| `-g` | group |
| `-o` | owner |
| `-D` | device files, FIFOs, and sockets |

Metadata is applied to the file before it takes its name, so the new
content and its metadata appear together. Owner and group are set before the
mode, because `chown` clears setuid and setgid. A directory's mtime is set
after everything inside it is written, deepest directories first, because
writing into a directory changes its mtime.

The receiver never follows a symlink that already exists in the destination
while it writes. Each path is resolved one component at a time with `openat`
and `O_NOFOLLOW`, so a link in the destination cannot redirect a write outside
it. Owners, permissions, and mtimes are set the same way, never through a
link: an entry swapped for a symlink after it was compared fails, rather than
have a file outside changed. A hard link is another matter: an entry swapped
for a hard link to a file outside has that file's attributes set, as a hard
link is that file. Linux refuses to make a hard link to a file one does not
own, where `fs.protected_hardlinks` is on, as most distributions have it;
macOS has no such guard.

If Ferry lacks the privilege to set something, it skips that item and warns,
where rsync stays silent. Examples: a user who is not root cannot set another
user as owner, cannot set a group they do not belong to, cannot set the
group, permissions, or mtime of an entry another user owns, and cannot
create device files; root cannot set an owner where it is squashed, as on
NFS; and FAT keeps no permissions. The file itself is still transferred.

### Two commands, `copy` and `sync`, that copy contents

Ferry has its own CLI, not an rsync-compatible one, in the way ripgrep relates
to grep. Instead of rsync's `--delete`, the choice is the command:

| Command | Makes DESTINATION | As rsync |
|---|---|---|
| `ferry copy SOURCE DESTINATION` | hold everything in SOURCE, keeping anything else it has | `rsync -a SOURCE/ DESTINATION/` |
| `ferry sync SOURCE DESTINATION` | the same as SOURCE, deleting anything else it has | `rsync -a --delete SOURCE/ DESTINATION/` |

Both copy SOURCE's contents into DESTINATION, as if SOURCE ended with a slash,
whether it does or not: from `data` to `backup`, `data/x` becomes `backup/x`,
never `backup/data/x`. rsync's distinction between `data` and `data/` is a
common mistake, and Ferry does not have it. Each command takes one SOURCE,
because several sources' contents would collide in one DESTINATION.

### Names are looked up with `getent` on Linux

Linux releases are static musl builds, which cannot see LDAP or SSSD accounts,
so owners and groups are mapped by name through the system's `getent`; see
[Static musl builds on Linux](architecture.md#names-are-looked-up-with-getent).

## Proposed

### Equivalents for common rsync invocations

Common rsync invocations need a documented equivalent. If drop-in
compatibility turns out to matter, a compatibility shim can be added later.

### Behaviour of `-a` metadata

- **`-a` is the default.** Ferry preserves everything above without a flag.
- **Owner and group are mapped by name, as in rsync.** The manifest carries
  numeric IDs; before the first entry that uses an ID, the Source sends its
  name, once. The Destination maps each name to its own ID; an unknown name
  keeps its number. `--numeric-ids` sends no names. IDs 0 are never mapped.
- **What cannot be set is not a difference.** An owner this process cannot
  set, or the permissions or mtime of an entry another user owns. Otherwise
  every such entry would look changed on every run.
- **Privilege warnings are summarised.** Without root, every file may fail to
  keep its owner, so one warning per file would bury everything else. Ferry
  reports one warning per kind, with a count and a few example paths: an
  owner, permissions, or an mtime that could not be set, and sockets and
  devices that could not be created.
  Listing every path with `--verbose` is still to come.
- **Device numbers are copied as they are.** Major and minor numbers are
  encoded differently on Linux and macOS, so a device file copied between them
  is unlikely to be meaningful.

### Behaviour of `ferry sync`

**Built as described.**

- **Deletion runs during the transfer, a directory at a time,** as rsync's
  does by default. A name the Source lists but cannot read is still in the
  manifest, as unreadable, and the Destination leaves what it has of that
  name as it is: a read error on the source never makes a file look extra.
  rsync instead stops deleting altogether after an I/O error, since it does
  not know which name it lost. A transfer that stops partway may leave the Destination
  with less than it had; running it again completes it.
- **Extra files are found during the transfer, one directory at a time.** When
  the Destination has a directory's complete listing from the manifest, it
  reads its own copy of that directory and records the entries the Source
  does not have, and deletes them then, before anything of that directory
  is made, keeping nothing of it.
- **`ferry sync --dry-run` lists what would be deleted**, as well as what
  would be transferred, in manifest order.
- **SOURCE and DESTINATION may not overlap.** Each server tells the client
  what its root is, as its kernel knows it: its device and inode, and those
  of each directory above it. A DESTINATION that is SOURCE, is inside it, or
  holds it is refused before anything changes, for `copy` and `sync` alike,
  and in a dry run too: syncing a directory into one it holds would delete
  it. The same is checked as the transfer goes, so that a mount that makes
  one root appear inside the other stops the transfer: the Source's scan
  never goes into the Destination's root, and the Destination never goes
  into the Source's root, nor deletes it or a directory above it. Roots are
  compared only when both servers run on the same kernel; two hosts that
  share a network filesystem, such as NFS, are not compared, yet.
- **A DESTINATION that sync would empty by mistake is asked about.** `sync`
  into `/`, into a home directory, or into a directory that holds one, such
  as `host:` alone, asks first; without a terminal it is refused unless
  given `--allow-dangerous-destination`. `--yes` does not agree to it. A dry
  run only warns. `copy` deletes nothing, and is not asked.
- Deletion follows the same symlink rule as writing: it never follows a link
  out of the destination.

### Later, with room left for them in the design

- hardlinks (rsync `-H`)
- sparse files (rsync `-S`)
- xattrs (rsync `-X`)
- `--verify`, re-reading each written file from disk, for errors in the disk
  or filesystem, at the cost of reading everything twice

### Not planned

ACLs (rsync `-A`), atime (`-U`), creation time (`-N`), and file flags. POSIX
ACLs and macOS or NFSv4 ACLs do not map onto each other.

### Scope: one-way only

Ferry makes the destination match the source. These are non-goals:

- two-way sync (the domain of unison and Syncthing)
- continuous watching of files for changes
- an atomic switch of the whole destination tree
- resuming a partly transferred file: an interrupted file is sent again from
  the start. This may be revisited if it is needed.
- delta transfer: a changed file is sent whole, as with rsync's `--whole-file`.

## Open questions

1. **Typical workload.** How many files, what size distribution, and over what
   network: LAN, across data centres, or between clusters such as NCHC?
2. **Where rsync hurts in practice.** Is it a huge number of files taking a
   long time, or large files not filling the bandwidth?
