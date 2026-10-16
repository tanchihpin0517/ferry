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
  - [7. Results and the resend pass](#7-results-and-the-resend-pass)
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
| Comparing | one lists the Destination's directories, 8 `stat` | applied in the manifest's order |
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
   before a first install.
4. **Start the two servers**, one for each side, each over a control
   connection of its own, which it lives as long as, and **shake hands** over
   it: the builds are compared, and the peers learn their roles and the
   options.
5. **Open the connections** for the transfer: the paths of each transport
   join its server, and the client holds a connection to each, to relay
   between them.

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
  64 MiB of the manifest wait to be sent, rather than piling it up in
  memory.
- **Only the manifest has a fixed order.** Reading and writing run in
  parallel, so files are sent and written in whatever order they finish.
- **Directories are opened relative to their parent's file descriptor,**
  with `openat`, rather than by full path. This follows the same rule as
  writing: no symlink is followed. It also saves path lookups, which matters
  on filesystems such as Lustre.
- **A scan that fails stops the transfer at once,** with its reason. The
  Destination waits for the rest of the manifest, and the Source for the
  needs, so a scan that ended quietly would leave both waiting for ever.

**Proposed.**

- **Each entry gets an ID in manifest order.** Needs and results refer to
  files by ID, not by path. Both peers see the same IDs without any sorting.
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
either side whole: an online diff.

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
reads, in parallel. If a file's size or mtime changes while it is being read,
the Source sends it again; see the integrity rules in the
[overview](overview.md#every-file-is-complete-correct-and-durable).

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
is no special case.

### 6. Write

**Decided.** Each file is written to a temporary name, its hash is verified,
its metadata is applied, it is `fsync`ed in a batch, and it is renamed into
place. These rules are in the
[overview](overview.md#every-file-is-complete-correct-and-durable).

**Proposed.** Writing runs in parallel, one file per writer. Each file's result
goes back to the Source in a results packet.

### 7. Results and the resend pass

**Decided.** One file failing does not stop the transfer. The Destination
reports each entry it cannot make match, as it finds out, and the client
shows it to the user. A file that changed while the Source read it is sent
again, once, from the start: the Source marks the end of the first pass, the
Destination answers with the files to send again, and the Source sends them
and ends the stream. A file that changes again fails. Each record carries the
size and mtime the Source found when it opened the file, so a file sent again
gets its new mtime.

### 8. Finish

**Proposed.** After every file is written, the Destination sets directory
mtimes, deepest directories first. For `ferry sync`, it then deletes what the
Source does not have, if the transfer succeeded. Last, it reports the summary
to the client.
