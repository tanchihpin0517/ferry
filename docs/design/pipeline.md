# Ferry transfer pipeline

[中文](pipeline.zh-TW.md)

Status: **draft, under discussion.** Each section says whether it is decided or
proposed. The [architecture](architecture.md) describes the client, servers,
and peers this pipeline runs in. The rules for integrity and metadata are in
the [design overview](overview.md).

**Decided.** A run has two phases: it [prepares](#phase-1-prepare), then it
[transfers](#phase-2-transfer). Everything the transfer needs is ready before it
starts, as the [overview](overview.md#reliability-comes-first-then-speed)
requires, and the pipeline's speed is the speed of the transfer.

**Contents**

- [Principles](#principles)
  - [Memory allocator](#memory-allocator)
- [Phase 1: Prepare](#phase-1-prepare)
- [Phase 2: Transfer](#phase-2-transfer)
  - [1. Scan](#1-scan)
  - [2. Compare](#2-compare)
  - [3. Read and hash](#3-read-and-hash)
  - [4. Serialise and compress](#4-serialise-and-compress)
    - [Compression](#compression)
  - [5. Decompress and parse](#5-decompress-and-parse)
  - [6. Write](#6-write)
  - [7. Results](#7-results)
  - [8. Finish](#8-finish)

![Pipeline: phase 1, the client prepares, from the arguments to every connection logged in and the servers started; then phase 2, the transfer between the Source and Destination peers, with their stages numbered by the sections below and the packets between them](../assets/pipeline.svg)

## Principles

**Decided.** Every stage runs in parallel where its work allows it, and there
is no round trip per file.

**Decided.** Each stage has its own pool of workers, and bounded queues
connect the stages. The slowest stage can then get more workers without
changing the others, and the bounded queues apply backpressure, so memory
use stays bounded. Each stage is limited by something different:

| Stage | Limited by | Gain from parallelism |
|---|---|---|
| Scanning directories, `stat` | metadata syscalls | large, especially on NFS and Lustre, but bounded there; see [Scan](#1-scan) |
| Hashing | CPU | large |
| Compression | CPU | large |
| Writing on the Destination (create, write, chmod, utime) | metadata and disk | large; often the bottleneck with small files |

As built, each with up to 16 workers, or as many as there are cores, but for
scanning and comparing, which ask the filesystem at most nine things at once:

| Stage | Workers | Order |
|---|---|---|
| Scanning | one lists, 8 `stat` | sent in the manifest's fixed order |
| Comparing | one lists the Destination's directories, 8 `stat`, one applies | applied in the manifest's order, as each is compared |
| Reading | open files, and read small ones (up to 1 MiB) whole, ahead | any order: each record carries its ID |
| Compressing | compress frames | sent in the stream's order |
| Decompressing | decompress frames | fed to the parser in order |
| Writing | one file each; a file's bytes reach its writer as they are parsed | any order |

A large file is read by the sender, and written by one writer, in order, so
it is never held in memory whole.

The threads of the stages, the queues between them, where order is kept,
and, in green, the [transport](architecture.md#transport):

![Workers, queues, and the transport of a transfer: on the Source's server, the main thread, the sender with 16 readers, 8 compressors, the frame sender, and the pump, with the transport's queues and path ends under it; in the middle, the client relaying between the two servers; on the Destination's server, the transport's path ends and queues, then the main thread, the pump, 8 decompressors, the frame feeder, the parser with 16 writers, and 8 renamers; with the bound of each queue, and which edges keep order](../assets/pipeline-workers.svg)

Everything that takes time per file or per frame, reading, compressing,
decompressing, writing, and renaming, runs in a pool. The single threads
hand work along. Two of them, the Source's sender, which writes the record
stream, and the Destination's parser, which reads it, are points every file
passes through in order. Each pump sends messages into the transport's
connection, the client relays them to the other server, and the other side's
main thread receives them, whole and in order; which paths they took, all of
them equal, is the transport's business.

**Decided.** On the Destination, a stage that fails stops every stage at
once. Its failure is the cause, and only the first one is: what the others
then meet, as a queue or a stream closed under them, is a consequence of it,
never reported in its place. The cause is told to the other side once, as
the one `Error`, and `Done` never follows it. Nothing more is changed once
it is told: each stage checks before it creates, deletes, or changes
anything, and one that waits on a queue wakes as the stop comes. A failure
the Source tells is the cause too, and is not told back.

### Memory allocator

**Decided.** Every build uses mimalloc as its global allocator, because every
stage allocates and frees buffers from many threads at once; see
[Static musl builds on Linux](architecture.md#mimalloc-is-the-allocator-everywhere).

## Phase 1: Prepare

**Decided.** Before any file is read or written, the client gets both peers
ready, one step after another. Every kind of run, local or remote on either
side, goes through the same steps; for a side on this machine, the
[local transport](architecture.md#transport) makes steps 2 and 3 empty.

1. **Read the arguments**, check the locations, and
   get a transport for each side's location.
2. **Open every SSH connection** to every host the run involves, each logged
   in, one at a time; see
   [Connections](architecture.md#connections). This is the first thing that
   reaches a host, and the transport does it alone. An SSH connection is
   opened with no task: it is only logged in, and ready. A password or an
   OTP is typed here.
3. **Probe each host, and install this build** where it is missing. Only now
   does work on the host begin: the client opens
   [control connections](architecture.md#transport) through the transport,
   each to the program it needs on the host, to probe and to install, asking
   before a first install. Each host is probed once, however many sides are
   there, and the probe also says what each side's root there is, which
   decides whether a file is copied into a directory. Every question is asked
   before anything is installed, and every host has this build before either
   server starts: a server that has started waits for the other.
4. **Start the two servers**, one for each side, each over a control
   connection of its own, which it lives as long as, and **shake hands** over
   it: the builds are compared, and the peers learn their roles and the
   options.
5. **Open the connections** for the transfer: the paths of each transport
   join its server, and the client holds a connection to each, to relay
   between them. Its heartbeats start as each opens, not when the transfer
   does: a server watches for them once its paths have joined, while the
   other side's may still be joining.

- **Everything that asks you is asked here.** A password, an OTP, or the
  question before a first install comes before the transfer, never during it.
- **Nothing is changed on either side.** A failure or Ctrl-C while preparing
  leaves both trees as they were, with nothing to clean up.
- **Preparing is not overlapped with the transfer.** The Source does not
  start scanning while connections log in, though that would save time: a
  transfer starts only once it has everything it needs.

## Phase 2: Transfer

**Decided.** Once every peer is ready, the Source starts its scan, and the
stages below run until the Destination finishes. Each stage's section has its
number in the figure's second phase.

### 1. Scan

![The scan and the comparison: on the Source, one lister lists directories depth first, opening each relative to its parent, and hands their names in parts of at most 10,000 to 8 stat threads, whose results are put back in order for the manifest sender to give IDs and queue for the pump; the client relays the manifest to the Destination, whose compare thread lists each directory the manifest names once and merges it with the parts, and whose 8 stat threads quick-check the entries both sides have, their verdicts put back in order for the main thread to send the needs back; below, an example tree, its manifest in order, and how the Destination merges one directory](../assets/pipeline-scan.svg)

**Decided.**

- **Ferry has its own scanner.** Existing crates do not fit. `ignore`, which
  ripgrep and fd use, reports entries one at a time in no fixed order, with no
  signal that a directory is complete. `jwalk` has the right shape but is no
  longer maintained.
- **One thread lists, and eight stat.** A scan has two parts. Listing a
  directory, opening it and reading its names, is done by one thread, one
  directory at a time, depth first in name order. Which names are
  directories, and so where to go next, it learns from the listing itself,
  without waiting for their stats; where a filesystem does not say, it stats
  those names itself. It hands each directory's names to a pool of 8
  threads, which stat them in the order they were handed over, and their
  results come back in that order. So the filesystem is asked at most one
  listing and 8 stats at once: on a server many people share, such as NFS,
  the scan's load is bounded, whatever the tree.
- **Scanning more in parallel was tried, and given up.** Up to 16 workers
  once read directories ahead in parallel, up to a million entries, with a
  reorder buffer to send them in order. On NFS that was 1.5 to 2 times
  faster, on a tree of 134,000 entries, but it asked the shared server for
  twice as much at its busiest, and 30 to 50% more in all when the
  Destination was there too. On a local disk it was slower, held back by
  its read-ahead: a dry run of 10 million entries took 9.4 s, against 6.6 s
  with one lister. And bounding its memory took special cases that kept growing:
  directories put back for want of room, and the one sent next always read.
  One lister needs none of it.
- **The unit of the manifest is a directory.** A directory's entries are
  sent sorted by name, with their metadata: at once, or, for a directory of
  more than 10,000 entries, in parts of at most 10,000, one after another,
  so that no message grows with a directory. Each part is statted as one
  piece of work, so a large directory is statted by every stat thread, and
  is sent as its parts are done, never held whole.
- **The manifest has a fixed order: depth first, by name.** It is the order
  the lister goes in, so nothing reorders it, and the same tree always gives
  the same manifest, which makes tests reproducible and makes `--dry-run`
  and `--verbose` output comparable between runs. A name listed as a
  directory that its stat finds is no longer one is left out, with
  everything under it.
- **The scan and the transfer overlap.** The Source never waits for the
  scan to finish before sending, as rsync did before 3.0. Scanning first and
  sorting after would leave the network idle for the whole scan, and keep
  the whole tree in memory. A scan faster than the network waits while
  8 MiB of the manifest wait to be sent, rather than piling it up in
  memory. That manifest does not hold the files back: once the sender has
  enough to send, its data, at least a quarter of its queue, goes before
  the manifest, and below that, the manifest goes first again, so that the
  Destination finds more to need before the sender runs out. A sync that
  needs little keeps the manifest flowing at full speed.
- **The scan waits while too many needed files wait to be sent.** Both
  sides keep track of each needed file until it is sent, and the manifest
  can run far ahead of the data, as on a first copy: so the scan waits
  while more than 100,000 do. What each side keeps then stays bounded, by
  that, a directory's own needs, which come at once, and the manifest on
  its way. Sending them needs no more of the manifest, so the scan always
  goes on; a transfer that ends, or fails, has it wait no more.
- **Only the manifest has a fixed order.** Reading and writing run in
  parallel, so files are sent and written in whatever order they finish.
- **Directories are opened relative to their parent's file descriptor,**
  with `openat`, rather than by full path. This follows the same rule as
  writing: no symlink is followed. It also saves path lookups, which matters
  on filesystems such as Lustre. A directory with subdirectories still to
  list is held open for them, which on the way down is one per level; below
  128 levels, they are opened again from the root, one component at a time,
  so that a deep tree does not run out of file descriptors. The files are
  read the same way: the root is opened once, for the scan and the readers,
  and a file's directory is reached from it, the few most recently used
  kept open, never by its path again.
- **A scan that fails stops the transfer at once,** with its reason. The
  Destination waits for the rest of the manifest, and the Source for the
  needs, so a scan that ended quietly would leave both waiting for ever.
  The connection closing before the manifest's end fails the transfer too,
  a dry run's as well: what was listed so far is not all there is.
- **The scan never goes into the Destination's root.** The client refuses
  roots that overlap before anything starts; see the
  [overview](overview.md#behaviour-of-ferry-sync). A mount can still make
  the Destination's root appear inside the Source, so on the same kernel the
  Source is told its device and inode, and a directory it reaches that is
  that root stops the transfer, rather than be read while it is written.
- **What cannot be read is reported, and the scan goes on.** A name listed
  whose metadata cannot be read is still sent, as unreadable, so that the
  Destination leaves what it has of that name as it is, and `sync` never
  takes it for extra. A directory that cannot be listed is sent as its
  parent listed it, with nothing in it, so nothing in it is deleted either.
  The Destination deletes only in a directory whose whole listing the
  Source sent: one it never sent, and all under it, is not even compared.

**Proposed.**

- **Each entry gets an ID in manifest order.** Needs and results refer to
  files by ID, not by path. Both peers see the same IDs without any sorting.
  The Source keeps each entry's path, to answer needs, only until the
  Destination says it has compared through it: every 4,096 IDs or so, once
  it has asked for what it needs of them. A file needed takes its path with
  it, to be sent. So the Source keeps what of the manifest is on its way,
  which the queues between bound, not the whole tree.
- **Progress counts as the scan goes,** as rsync does: files found so far,
  and files sent so far. Scanning is usually faster than transferring, so
  the total settles early. The peers count, and report the counts to the
  client, which shows them.
- **System-specific calls where they help,** such as `statx` on Linux to ask
  only for the fields Ferry needs.
- **Exclude patterns, when they are added, can use the `ignore` crate's glob
  matching,** without its walker.

### 2. Compare

**Decided.** The comparison runs on the Destination, as in rsync. The
Destination scans its own tree as the manifest comes, in the same order, so
that it compares the two as they come, a directory at a time, without
either side whole: an online diff. Its threads are in the lower half of
the [figure in Scan](#1-scan).

- **The Destination's scan follows the manifest.** One thread goes into
  each directory the manifest names, in the manifest's order, opening it
  relative to its parent, once, and listing it once. It goes into nothing
  the Source does not have: a directory missing here, or that is something
  else here, has everything in it needed, unlooked at.
- **Each part is merged with the listing.** Both are sorted by name, so
  they are read side by side. An entry the Destination does not have is
  needed, without a stat. One it has is statted, by a pool of 8 threads, in
  order, as on the Source. A name the Destination has that the Source does
  not is what `sync` deletes; it is found as the parts come, and the whole
  of a directory's once its last part is here, without a second listing.
- **The manifest is checked before anything of it is acted on.** Whatever
  a Source sends, by a bug or ill will, the Destination touches only what
  is under its root, by names the Source listed: a root comes before any
  directory, and once; each name is one, not `.` or `..`, nor with a `/` or
  a NUL in it; a directory comes only once announced as one by its
  parent's entries, and only once, its parts together; names come strictly
  in order, and IDs in turn. Anything else stops the transfer at once, as
  any stage failing does.
  Under that, every call that changes or goes into what a name names
  refuses what is not one name, and a path is never followed through `.`
  or `..`: should the check miss something, nothing out of the directory
  given is reached.
- **A directory is applied whole, extras first.** Its parts are compared as
  they come, but nothing of them is made, nor asked for, until its last is
  compared; then, for `sync`, what it has extra is deleted first, and its
  parts are applied after, in order, as rsync does. Nothing made for a
  directory is so ever taken for extra, even where the filesystem takes two
  names for one, as one that ignores case does; and an extra that is what
  the Source could not read, under such another name, is kept. `copy`
  goes the same way, deleting nothing. A directory's parts are held until
  then, so memory grows with the largest directory, as the Source's listing
  of it already does. A thread of their own applies the comparisons, in
  order, as each comes, so that what a directory needs is asked for once it
  is compared, not once the connection brings another message; the thread
  that reads the connection only reads and hands on.
- **What is deleted is deleted from the directory listed.** The comparison
  opens each directory once, relative to its parent, and its extras are
  deleted from that very directory, by its file descriptor, never by its
  path again; everything else under the root is reached from the root as
  the comparison opened it, once. A path that comes to name another
  directory meanwhile, as a symlink switched or a disk mounted over it,
  never has extras listed in one deleted from the other.
- **The Source's root is never gone into, nor deleted.** On the same
  kernel, the Destination is told the device and inode of the Source's root
  and of each directory above it. The comparison does not go into a
  directory that is the Source's root, and no deletion removes one of them,
  at any depth: either stops the transfer, as a mount that puts one root
  inside the other would otherwise have the Destination delete the Source.
- **Quick check.** A file with the same size and mtime is skipped, as in
  rsync's quick check. mtimes are compared in whole seconds by default, as
  rsync does, so that filesystems and tools that keep coarser times do not
  make unchanged files look changed; `--modify-window` widens the window,
  or narrows it to nanoseconds. A changed or missing file is a need, which
  the Destination streams back in needs packets.
- **What it costs.** For a directory that exists, a listing and a stat of
  each entry both sides have. Against a stat of every entry the manifest
  names, that is one more read of the directory for `copy`, and one fewer
  for `sync`; it saves a failed stat for each entry the Destination lacks,
  and on NFS, whose listings bring each entry's attributes, it may make the
  stats cheaper too. An empty destination costs nothing to compare. A
  directory's listing is held until its last part is here: about 75 MB for
  a directory of a million entries.

The only round trip is between the manifest and the needs, and it is
pipelined: the Source starts sending the first needed files while later ones
are still being compared.

**Proposed.**

- **`--checksum`, later.** Compare files by their BLAKE3 hash instead of size
  and mtime, for when content must be confirmed identical. It reads every file
  on both sides, so it is never the default.
- **rsync's other comparison options wait for a need:** `--size-only`,
  `--update`, `--ignore-existing`, `--existing`, and `--ignore-times`.

### 3. Read and hash

**Proposed.** The Source reads each needed file and hashes it with BLAKE3 as it
reads, in parallel, up to the size it had when opened; see the integrity
rules in the [overview](overview.md#every-file-is-complete-correct-and-durable).
It opens only what is still a regular file, and opening never waits: a file
made a FIFO, a device, or a directory since the scan is closed again unread
and reported as unreadable. Nothing is checked before the open, since such
files are rare and a `stat` per file is a round trip on a network
filesystem. On Linux, files are opened with `O_NOATIME` where the kernel
allows it, so reading them does not update their access time.

### 4. Serialise and compress

**Proposed.** The data the Source sends is one continuous stream of file
records, like tar:

```text
[header: path, metadata, size][content][BLAKE3][header][content][BLAKE3]...
```

- **Small files are merged naturally.** One record follows the next with no
  packing rules and no padding.
- **A large file is one long record.** One writer writes it in order, and
  sequential writes are fast.
- **A changed file is sent whole.** There is no delta transfer; see the
  [overview](overview.md#scope-one-way-only).
- **Flow control comes from the transport.** TCP and SSH apply backpressure
  between the peers.

#### Compression

**Decided.**

- **The stream is compressed in frames of 2 MiB, in parallel.** Frame
  boundaries ignore file boundaries. Beyond about 1 MiB, larger frames barely
  improve the ratio, and memory grows with frame size × frames in flight. The
  size is fixed; a hidden option changes it for benchmarks.
- **A frame is sent as soon as the stream goes idle,** not only when it is
  full. Otherwise the last few files of a run, and progress, would wait for a
  frame that never fills.
- **The zstd level is fixed, at 1 by default.** `--compress-level <n>` sets
  another. Decompression speed barely depends on the level, so a higher level
  costs the Destination almost nothing.
- **A frame that does not compress is sent as is.** Already compressed files
  (`.gz`, `.mp4`, `.zst`) do not cost CPU twice.
- **`--no-compress`** turns compression off, for fast local networks where CPU
  matters more than bytes.

**Decided.** Level 1, fixed. A level that adapted was tried first, rising
while the queue of data packets to the connection stayed full. It was too
slow: on one machine, or with a disk slower than the compressors, the queue
stays full whatever the level, so the level climbed until compressing was
the bottleneck. A fixed level of 1 or 3 copied 1 GiB of large files on one
machine in 1.4 s, against 4.5 to 17 s, and pushed them over one SSH
connection in 32 s, against 42 to 56 s.

### 5. Decompress and parse

**Proposed.** The Destination decompresses frames in parallel and puts them
back in order. It then parses the records in order. Parsing only reads headers
and hands data on, so one thread is enough, and a file that spans two frames
is no special case. At most 16 MiB of decompressed frames wait for it;
beyond, decompressing waits, and with it the reading of the connection, so
that a disk slower than the network holds the Source back rather than
filling memory.

### 6. Write

**Decided.** Each file is written to a temporary name, its hash is verified,
its metadata is applied, it is `fsync`ed in a batch, and it is renamed into
place. These rules are in the
[overview](overview.md#every-file-is-complete-correct-and-durable).

**Proposed.** Writing runs in parallel, one file per writer. Each file's result
goes back to the Source in a results packet.

**Decided.** Files are renamed in batches of at most 256. A file waiting for
its batch holds at most one file descriptor: its directory's, which every
file of that directory shares, and its registration for removal on a
signal too. Each directory is opened once from the root, a name at a time,
not by its path; the few most recently used stay open. The file itself is
kept open only where the drive's barrier needs one, on macOS, and only one
for each drive in a batch. Where few files may be open at once, batches are
smaller, so that no file fails for want of a descriptor.

### 7. Results

**Decided.** One file failing does not stop the transfer. The Destination
reports each entry it cannot make match, as it finds out, and the client
shows it to the user. The scan is taken as a snapshot, and nothing is sent
again, unlike rsync, which redoes a file once. Each record carries the size
and mtime the Source found when it opened the file, and its content is read
up to that size, as rsync does: what the file gains while it is read is
left out, and the next run finds its mtime changed. A file that shrinks
while it is read, or whose reading fails, is padded to that size, to keep
the stream in step, and marked unread; the Destination discards it, and it
fails. A file that cannot be opened, including one that is no longer a
regular file, is reported by the Source and sends no record; the Source
tells the Destination, so that its directory is finished without it.

### 8. Finish

**Decided.** The Destination finishes each directory, setting its owner,
permissions, and mtime, as soon as nothing more is made in it nor under it:
once the manifest has left it, and all under it, every file in it is
written, and every directory in it is finished. It keeps a stack of the
directories the manifest went into, depth first, and a directory leaves it
when the manifest goes on past it, as in a walk of the tree; one the Source
could not list leaves with its parent. A thread of its own finishes them in
the order they are done, which is deepest first, so that one that cannot be
gone into after is finished only once nothing under it is left to make. A
needed file the Source could not open is settled when the Source says so,
as one it could not read whole is when it comes: its directory need not
wait for it. Only a directory with a file that never came, not even so, is
finished at the end, and that file fails, as never sent. A directory the Destination has, owns, and may not
read, write, or go into, is given all three for its owner as its scan opens
it, as rsync does, so that it can be worked in, and an extra one so before
it is deleted; such a directory is finished whatever else changes in it, so
that it ends with the Source's permissions. Should the transfer fail before
one is finished, or an extra one not be deleted after all, it is given back
the permissions it had. What the
Destination keeps of directories so stays as small as the stack, the
directories announced and not come yet, and those with files still to
write. For `ferry sync`, what the Source does not have was deleted already,
a directory at a time, as each was compared. Last, it reports the summary to
the client.
