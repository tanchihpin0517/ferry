# Ferry transfer pipeline

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
  - [1. Walk](#1-walk)
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
| Walking directories, `stat` | metadata syscalls | large, especially on NFS and Lustre |
| Hashing | CPU | large |
| Compression | CPU | large |
| Writing on the Destination (create, write, chmod, utime) | metadata and disk | large; often the bottleneck with small files |

As built, each with up to 16 workers, or as many as there are cores:

| Stage | Workers | Order |
|---|---|---|
| Walking | read directories ahead | sent in the manifest's fixed order |
| Comparing | `stat` each directory's entries | applied in the manifest's order |
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

**Decided.** Every build uses mimalloc as its global allocator, on Linux, both
musl and glibc, and on macOS. Every stage allocates and frees buffers from
many threads at once. musl's own allocator favours hardening and low memory
use over that: its threads largely wait on one shared lock. mimalloc keeps
free memory per thread, so threads seldom contend. Using it everywhere gives
every platform the same performance characteristics, and one code path. Its C
code is compiled into the binary, like zstd's, so Ferry stays a single file.

**Proposed.** Once Ferry transfers files, benchmark mimalloc against jemalloc
and the system allocators, and revisit the choice if the numbers say so.

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
  start walking while connections log in, though that would save time: a
  transfer starts only once it has everything it needs.

## Phase 2: Transfer

**Decided.** Once every peer is ready, the Source starts its walk, and the
stages below run until the Destination finishes. Each stage's section has its
number in the figure's second phase.

### 1. Walk

**Decided.**

- **Ferry has its own walker.** Existing crates do not fit. `ignore`, which
  ripgrep and fd use, reports entries one at a time in no fixed order, with no
  signal that a directory is complete. `jwalk` has the right shape but is no
  longer maintained.
- **The unit of the manifest is a directory.** A worker reads one directory
  and sends all its entries at once, sorted by name, with their metadata. Its
  subdirectories go back on the queue for other workers.
- **The walk and the transfer overlap.** The Source never waits for the walk to
  finish before sending, as rsync did before 3.0. Walking first and sorting
  after would leave the network idle for the whole walk, and keep the whole
  tree in memory.
- **The manifest has a fixed order: depth first, by name.** Workers read
  directories ahead in parallel, and a reorder buffer sends them in that
  order. A directory that is slow to read holds back those after it, so
  read-ahead is bounded to bound memory. In exchange, the same tree always
  gives the same manifest, which makes tests reproducible and makes
  `--dry-run` and `--verbose` output comparable between runs.
- **Only the manifest has a fixed order.** Comparing, reading, and writing run
  in parallel, so files are sent and written in whatever order they finish.

**Proposed.**

- **Directories are read relative to their parent's file descriptor,** with
  `openat` and `fdopendir`, rather than by full path. This follows the same
  rule as writing: no symlink is followed. It also saves path lookups, which
  matters on filesystems such as Lustre.
- **Each entry gets an ID in manifest order.** Needs and results refer to
  files by ID, not by path. Both peers see the same IDs without any sorting.
- **Progress counts as the walk goes,** as rsync does: files found so far, and
  files sent so far. Walking is usually faster than transferring, so the total
  settles early. The peers count, and report the counts to the client, which
  shows them.
- **System-specific calls where they help,** such as `statx` on Linux to ask
  only for the fields Ferry needs.
- **Exclude patterns, when they are added, can use the `ignore` crate's glob
  matching,** without its walker.

### 2. Compare

**Decided.** The comparison runs on the Destination, as in rsync. For each path
the manifest names, the Destination `stat`s its own copy, in parallel. A file
with the same size and mtime is skipped, as in rsync's quick check. mtimes are
compared in whole seconds by default, as rsync does, so that filesystems and
tools that keep coarser times do not make unchanged files look changed;
`--modify-window` widens the window, or narrows it to nanoseconds. A changed
or missing file is a need, which the Destination streams back in needs
packets. The Destination never has to hold its whole tree in memory to
compare.

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
- **The zstd level adapts by default.** Each frame is compressed on its own,
  so each one can use a different level. Before compressing a frame, Ferry
  looks at the queues the pipeline already has. If the queue to the network
  stays full, the network is the bottleneck, and the level goes up. If that
  queue runs empty while frames wait to be compressed, the CPU is the
  bottleneck, and the level goes down. Decompression speed barely depends on
  the level, so a higher level costs the Destination almost nothing. zstd's
  library has no adaptive mode; the `zstd` command's `--adapt` is logic of the
  command, and Ferry implements the same idea itself.
- **A fixed level is an option.** Something like `--compress-level <n>` turns
  adaptation off.
- **A frame that does not compress is sent as is.** Already compressed files
  (`.gz`, `.mp4`, `.zst`) do not cost CPU twice.
- **`--no-compress`** turns compression off, for fast local networks where CPU
  matters more than bytes.

**Decided.** Adaptation starts at level 3 and stays between 1 and 19. It
changes the level only after four frames in a row point the same way, so the
level does not swing back and forth. The signal is the queue of data packets
waiting for the connection: more than three quarters full, the network is the
bottleneck and the level rises; less than a quarter, compressing is, and it
falls.

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
