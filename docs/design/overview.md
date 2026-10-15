# Ferry design overview

Status: **draft, under discussion.** Each section says whether it is decided,
proposed, or still open. A proposal is a starting point, not a decision.

## Goal

Ferry replaces rsync. The first thing it has to be better at is speed.

## Design documents

- [Architecture](architecture.md): the client, the servers, the peers, and how
  they connect.
- [Pipeline](pipeline.md): how data moves through a transfer.

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
open and logged in, and every host has its server running, before Ferry walks,
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

1. **Atomic per file.** Ferry writes each file to a temporary name in the same
   directory, such as `.foo.ferry-XXXX`, and renames it to its final name only
   when the file is complete. There is no in-place mode.
   When a run is interrupted, the Destination deletes the temporary files of
   the files it did not finish. Only a crash or a killed process can leave
   one behind.
2. **Verified end to end.** The sender hashes each file with BLAKE3 as it reads
   it, and the receiver hashes it as it writes. The rename happens only if the
   two hashes match. If a file's size or mtime changes while it is being read,
   Ferry sends it again. A file that keeps changing is reported as an error. An
   optional `--verify` re-reads each written file from disk, for errors in the
   disk or filesystem, at the cost of reading everything twice.
3. **Durable by default.** Each temporary file is `fsync`ed before its rename.
   Without this, a power loss can leave the rename on disk but not the data.
   To keep this affordable with many small files, Ferry works in batches: it
   `fsync`s a batch of files in parallel, renames the whole batch, then
   `fsync`s each directory once. This is on by default, and `--no-fsync`
   turns it off.

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

Metadata is applied to the temporary file before its rename, so the new
content and its metadata appear together. Owner and group are set before the
mode, because `chown` clears setuid and setgid. A directory's mtime is set
after everything inside it is written, deepest directories first, because
writing into a directory changes its mtime.

The receiver never follows a symlink that already exists in the destination
while it writes. Each path is resolved one component at a time with `openat`
and `O_NOFOLLOW`, so a link in the destination cannot redirect a write outside
it.

If Ferry lacks the privilege to set something, it skips that item and warns,
where rsync stays silent. Examples: a user who is not root cannot set another
user as owner, cannot set a group they do not belong to, and cannot create
device files. The file itself is still transferred.

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

Linux releases are static musl builds, and musl reads users and
groups only from `/etc/passwd` and `/etc/group`. It does not support NSS, so
it cannot see accounts in LDAP or SSSD, which is where most clusters keep
them. To map owners and groups by name there, the Linux build looks names and
ids up by running the system's `getent`, which uses the system's own C library
and so sees everything NSS sees.

- **Lookups are batched and cached.** `getent passwd` and `getent group` take
  several keys in one call, and each id or name is looked up once per run, so
  a tree with millions of files costs a handful of processes, not millions.
- **Without `getent`, Ferry falls back** to `/etc/passwd` and `/etc/group`
  through musl, then to numeric ids, and warns once.
- **macOS needs none of this.** Its build uses the system C library, which
  sees directory services itself.

This makes `getent` the one program besides `ssh` that Ferry runs, and only on
Linux.

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
- **An owner that cannot be set is not a difference.** Otherwise every entry
  whose group a user cannot set would look changed on every run.
- **Privilege warnings are summarised.** Without root, every file may fail to
  keep its owner, so one warning per file would bury everything else. Ferry
  reports one warning per kind, with a count and a few example paths.
  Listing every path with `--verbose` is still to come.
- **Device numbers are copied as they are.** Major and minor numbers are
  encoded differently on Linux and macOS, so a device file copied between them
  is unlikely to be meaningful.

### Behaviour of `ferry sync`

**Built as described, but for the guard against a mistyped DESTINATION.**

- **Deletion runs after the transfer, and only if the transfer succeeded.** If
  any file failed, Ferry deletes nothing and says so. This keeps a read error
  on the source from making files look extra and then deleting them, the way
  rsync skips deletion after an I/O error.
- **Extra files are found during the transfer, one directory at a time.** When
  the Destination has a directory's complete listing from the manifest, it
  reads its own copy of that directory and records the entries the Source
  does not have. It keeps only that list, never the Source's whole tree, and
  deletes the entries once the transfer has succeeded.
- **`ferry sync --dry-run` lists what would be deleted**, as well as what
  would be transferred, in manifest order.
- **Guarding against a mistyped DESTINATION**, such as refusing the home
  directory or `/`, is to be decided when deleting is built.
- Deletion follows the same symlink rule as writing: it never follows a link
  out of the destination.

### Later, with room left for them in the design

- hardlinks (rsync `-H`)
- sparse files (rsync `-S`)
- xattrs (rsync `-X`)

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
