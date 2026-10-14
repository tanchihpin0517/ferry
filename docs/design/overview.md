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
| client | The `ferry` command you run. It holds the SSH connections and lives as long as the command does |
| server | A `ferry server` process that the client starts on a remote machine over SSH. It exits when the connection closes |
| peer | One side of a transfer, run in the client or in a server |
| Source, Destination | The peer that reads the files, and the peer that writes them |
| record | One file in the data stream: header, content, and BLAKE3 hash |
| frame | A 2 MiB piece of the record stream, compressed on its own with zstd. Frame boundaries ignore file boundaries |
| packet | The unit sent over the connection: `[type][length][payload]`. Its type is control, manifest, needs, data, or results. A data packet carries at most 64 KiB of a compressed frame |

## Decided

### Speed comes first

When speed conflicts with other goals, speed wins unless a section below says
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
3. **Durable by default.** Each temporary file is `fsync`ed before its rename,
   and the directory is `fsync`ed after it. Without this, a power loss can
   leave the rename on disk but not the data. To keep this affordable with many
   small files, Ferry works in batches: it `fsync`s a batch of files in
   parallel, renames the whole batch, then `fsync`s each directory once. This
   is on by default and can be turned off.

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

### `--delete` removes files that exist only in the destination

As in rsync, it is off by default.

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

### A new CLI, not an rsync-compatible one

Ferry gets a clean CLI with sensible defaults, in the way ripgrep relates to
grep. Common rsync invocations need a documented equivalent. If drop-in
compatibility turns out to matter, a compatibility shim can be added later.

### Behaviour of `-a` metadata

- **`-a` is the default.** Ferry preserves everything above without a flag.
- **Owner and group are mapped by name, as in rsync.** If a name does not
  exist on the receiver, the numeric id is used. A flag such as rsync's
  `--numeric-ids` turns name mapping off.
- **Privilege warnings are summarised.** Without root, every file fails to
  keep its owner, so one warning per file would bury everything else. Ferry
  reports one warning per kind, with a count and a few example paths, and
  lists every path when given `--verbose`.
- **Device numbers are copied as they are.** Major and minor numbers are
  encoded differently on Linux and macOS, so a device file copied between them
  is unlikely to be meaningful.

### Behaviour of `--delete`

- **Deletion runs after the transfer, and only if the transfer succeeded.** If
  any file failed, Ferry deletes nothing and says so. This keeps a read error
  on the source from making files look extra and then deleting them, the way
  rsync skips deletion after an I/O error.
- **Extra files are found during the transfer, one directory at a time.** When
  the Destination has a directory's complete listing from the manifest, it
  reads its own copy of that directory and records the entries the Source
  does not have. It keeps only that list, never the Source's whole tree, and
  deletes the entries once the transfer has succeeded.
- **`--dry-run` lists what would be deleted**, as well as what would be
  transferred, in manifest order.
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
