# Ferry transfer pipeline

Status: **draft, under discussion.** Each section says whether it is decided or
proposed. The [architecture](architecture.md) describes the client, servers,
and peers this pipeline runs in. The rules for integrity and metadata are in
the [design overview](overview.md).

![Transfer pipeline: the Source and Destination peers, with their stages numbered by the sections below and the packets between them](../assets/pipeline.svg)

## Principles

**Decided.** Every stage runs in parallel where its work allows it, and there
is no round trip per file.

**Proposed.** Each stage has its own pool of workers, and bounded queues
connect the stages. The slowest stage can then get more workers without
changing the others, and the bounded queues apply backpressure, so memory
use stays bounded. Each stage is limited by something different:

| Stage | Limited by | Gain from parallelism |
|---|---|---|
| Walking directories, `stat` | metadata syscalls | large, especially on NFS and Lustre |
| Hashing | CPU | large |
| Compression | CPU | large |
| Writing on the Destination (create, write, chmod, utime) | metadata and disk | large; often the bottleneck with small files |

## 1. Walk

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
  settles early.
- **System-specific calls where they help,** such as `statx` on Linux to ask
  only for the fields Ferry needs.
- **Exclude patterns, when they are added, can use the `ignore` crate's glob
  matching,** without its walker.

## 2. Compare

**Decided.** The comparison runs on the Destination, as in rsync. For each path
the manifest names, the Destination `stat`s its own copy, in parallel. A file
with the same size and mtime is skipped. A changed or missing file is a need,
which the Destination streams back in needs packets. The Destination never
has to hold its whole tree in memory to compare.

The only round trip is between the manifest and the needs, and it is
pipelined: the Source starts sending the first needed files while later ones
are still being compared.

## 3. Read and hash

**Proposed.** The Source reads each needed file and hashes it with BLAKE3 as it
reads, in parallel. If a file's size or mtime changes while it is being read,
the Source sends it again; see the integrity rules in the
[overview](overview.md#every-file-is-complete-correct-and-durable).

## 4. Serialise and compress

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

### Compression

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

**Proposed.** Adaptation starts at level 3 and stays between 1 and 19. It
changes the level only after several frames in a row point the same way, so
the level does not swing back and forth.

## 5. Decompress and parse

**Proposed.** The Destination decompresses frames in parallel and puts them
back in order. It then parses the records in order. Parsing only reads headers
and hands data on, so one thread is enough, and a file that spans two frames
is no special case.

## 6. Write

**Decided.** Each file is written to a temporary name, its hash is verified,
its metadata is applied, it is `fsync`ed in a batch, and it is renamed into
place. These rules are in the
[overview](overview.md#every-file-is-complete-correct-and-durable).

**Proposed.** Writing runs in parallel, one file per writer. Each file's result
goes back to the Source in a results packet.

## 7. Resend failed files

**Proposed.** Failed files are sent again from the start in a final pass.

## 8. Finish

**Proposed.** After every file is written, the Destination sets directory
mtimes, deepest directories first. It then runs `--delete` if it was asked for
and the transfer succeeded, and reports the summary.
