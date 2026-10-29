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
  - [5. Decode](#5-decode)
  - [6. Write](#6-write)
  - [7. Results](#7-results)
  - [8. Finish](#8-finish)

![Pipeline: phase 1, the client prepares, from the arguments to every connection logged in, the servers started, and the stream made; then phase 2, the transfer between the Source and Destination peers, over the streams, with their stages numbered by the sections below and the pipes between them](../assets/pipeline.svg)

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

As built, each with up to 16 workers, or as many as the machine has cores, but
for scanning and comparing, which ask the filesystem at most nine things at
once, with one lister. On Linux the process runs on at most 8 of the cores, unless `--cpus` says
otherwise, all on one NUMA node where one has enough, as spread over two a
copy took half as long again; yet its stages keep as many workers as the
machine has: they mostly wait on the filesystem. With 200,000 files of 4 KiB on 96 cores, kept to 8 a
copy on ext4 was as fast, or faster, and spent half the CPU or less; on NFS it
was as fast, and with only 8 workers it took a tenth longer.

| Stage | Workers | Order |
|---|---|---|
| Scanning | one lists, or as many as `--listers` says, each a directory, 8 `stat` | written in the order handed over: with one lister, the manifest's fixed order |
| Comparing | one lists the Destination's directories, or as many lanes as `--listers` says, each a directory, 8 `stat`, one applies | applied in the manifest's order, as each is compared |
| Reading | open files, read those of up to 2 MiB whole, and larger ones a segment of 2 MiB at a time, and hash them | any order: each file carries its ID |
| Sending | one data sender writes each file, or segment, as a record on the data pipe | written in the order the owner hands them over |
| Compressing, in the transport | 8 compress the data's chunks, where the connection is compressed | put back in order by their numbers |
| Checking | 8 check each file, or segment, against its hash | handed on in the order they came |
| Writing | a whole file, or a segment of a large one, each | any order |

A file is read by a reader, whole if it is of 2 MiB or less, or else a
segment of 2 MiB at a time, in order: the next once the one before it is read, by
any reader free, so that the system reads ahead in it as in any file read
in order: from a spinning disk, one file's segments read by 16 readers at
once came at 60% of its speed. The 16 readers read 16 files at once,
whatever their size, in the order needed; up to 32 are handed out, so that
each reader has its next ready. Copying 16 files of 128 MiB to memory took
3.2 s from NFS when large files were read one after another, and takes
2.0 s; from a spinning disk, 16.2 s and 15.4 s; 20,000 files of 4 KiB as
fast as before. What is needed first is read first, each file by one
reader: as many large files ahead of small ones as there are readers take
them all, and the small ones wait; one large file takes one, and the small
ones are read beside it. A file's segments are written one at a time, in
order, each by any writer free, where it goes in the file. No file is ever
held in memory whole.

The threads of the stages, the queues between them, where order is kept,
and, in green, the [transport](architecture.md#transport):

![Workers, queues, and the transport of a transfer: on the Source's server, the main thread, which reads the answers, the owner with 16 readers, and the data sender, which writes each file as a record on the data pipe; in the transport, the data's chunks, compressed by 8 where the connection is, and a pump for each stream, with its lifeline; between the two, the 1 to 8 streams the client made, which it does not touch; on the Destination's server, a reader for each stream, the data's chunks decompressed by 8, the data reader, 8 checkers, the feeder, the decider with 16 writers, and 4 renamers, the pump that sends the answers, and its lifeline; with the bound of each queue, and which edges keep order](../assets/pipeline-workers.svg)

Everything that takes time per file or per chunk, reading, checking, and
writing, and the transport's compressing and decompressing, runs in a pool.
So does renaming, but one directory's files are renamed by one thread at a
time, as renames in a directory contend for its lock. The other single
threads hand work along. Two of them, the Source's owner, which hands every
file needed to be read and sent, and the Destination's decider, which hands
each to a writer, are points every file passes through. Neither does any I/O: each keeps
all its side knows of what is to be done, takes what the others tell it,
one thing at a time, and says what to do, so that what it does is tested
on events made up; the Destination's says too what each error means. On
events made up, it takes 200,000 small files, from their comparison to
their names, in 0.13 s of its own time. The peers write their
[pipes](architecture.md#transport), and the transport sends them over the
streams the client made between the two servers, as many as the round trip
to the farthest host needs, one for each 15 ms of it, up to eight, a pump
for each: the first carries every pipe, and the data's chunks go on any;
the other side reads each stream, a thread for each, which never waits for
a peer, and puts the data's chunks back in order. A pipe that ends without
its end is broken, and the transfer fails. Its silence breaks nothing: whether the far end is there is for the
[monitors](architecture.md#monitors) to judge, by their heartbeat. A
lifeline thread on each side waits on the server's standard error, and
stops the server once its monitor is gone.

**Decided.** On either side, a stage that fails stops every stage of its
side at once. Its failure is the cause, and only the first one is: what the others
then meet, as a queue or a stream closed under them, is a consequence of it,
never reported in its place. The cause is told to the other side once, as
the one `Error`, and `Done` never follows it. Nothing more is changed once
it is told: each stage checks before it creates, deletes, or changes
anything, and one that waits on a queue wakes as the stop comes. A failure
the other side tells is the cause too, and is not told back.

**Decided.** An error fails its entry alone, is a warning, or stops the
transfer, by what it is, wherever it is met:

| Kind | Such as | What becomes of it |
|---|---|---|
| An entry fails | a file or directory that cannot be opened, read, listed, compared, written, renamed, or finished; a hash that does not match; `EIO` | reported as it is met, as the answers' `Failed`, and counted; the transfer goes on, and ends with status 1 |
| A warning | an owner, permissions, or mtime this process may not set, or the filesystem does not keep, as `chown` where root is squashed, or of an owner with no ID here, or `chmod` on FAT; setuid or setgid taken away as their owner could not be set; an entry gone between listing and reading | one warning per kind, with a count and a few paths, at the end; the entry is still copied, or, gone, left out; not a failure |
| The transfer stops | a full drive, a quota reached, or a read-only one (`ENOSPC`, `EDQUOT`, `EROFS`), as every entry after would fail the same way; a broken protocol or manifest; a connection or stream that fails; a root that cannot be used; an internal error | the first cause stops both sides, as above |

On the Destination, a thread that meets an error, as a maker, a deleter, a
writer, a renamer, or a finisher, does not say which it is: it hands the
error back, with what it could not set. The decider says which it is.

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
2. **Open the SSH connection** to each host the run involves, logged in, one
   host after the other: one per host, which everything there goes over;
   see [Connections](architecture.md#connections). This is the first thing
   that reaches a host, and the transport does it alone. The SSH connection
   is opened with no task: it is only logged in, and ready. A password or an
   OTP is typed here.
3. **Probe each host, and install this build** where it is missing. Only now
   does work on the host begin: through the transport, the client opens the
   host's first session, its only one, which runs a script that probes the
   host, then, once every install is asked about, installs this build there;
   the session then becomes Ferry's [monitor](architecture.md#monitors)
   there, which starts all else on the host for the client, over channels that run no shell, sudo's checks among
   them, for `--remote-sudo`; see [control
   connections](architecture.md#transport). Each host is probed once,
   however many sides are there, and the probe also says what each side's
   root there is, which decides whether a file is copied into a directory.
   Every question is asked before anything is installed, and every host has
   this build before either server starts: a server that has started waits
   for the other.
4. **Start the two servers**, one for each side, each over a control
   connection of its own, which it ends with: on a host, the channel the
   monitor started it on; on this machine, the pipes the client started it
   with. And **shake hands** over it: the builds are compared, and the peers
   learn their roles and the options.
5. **Make the stream** for the transfer: a socket pair whose ends the
   client hands to the two servers, over the host's SSH connection to a
   server on a host. It may come before or after the server's control
   connection does. Should it break during the transfer, the transfer
   fails: nothing makes it again.

- **Everything that asks you is asked here.** A password, an OTP, or the
  question before a first install comes before the transfer; nothing asks
  during it.
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

![The scan and the comparison: on the Source, K listers, one unless --listers says more, list directories depth first, opening each relative to its parent, and hand their names in batches of 256 to 8 stat threads, whose results are put back in order for the scan to give IDs and write on the manifest pipe; the manifest goes over the stream to the Destination, whose compare thread, or K lanes, lists each directory the manifest names once and merges it with the batches, and whose 8 stat threads quick-check the entries both sides have, their verdicts put back in order for the decider, which the manifest reader tells of the root it applied, to have them applied and write the answers back; below, an example tree, its manifest's records in order, and how the Destination merges one directory](../assets/pipeline-scan.svg)

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
- **A directory is read as it was opened, many names at a time.** On Linux,
  the lister reads the descriptor it opened, from its start, with room for
  256 KiB of names in each read. rustix's `Dir` would open the directory
  again to read it, which on NFS asks the server once more whether it
  changed, and read it a few names at a time, while on WekaFS each read is a
  round trip of 7 to 10 ms that brings at most some 2,500 names. Listing a
  directory of 5,000 entries on WekaFS took 99 ms that way, against 22 ms
  now, and one of 100 entries 1.7 ms, against 0.9; on NFS, one of 10
  entries took 0.40 ms, against 0.28. The Destination's compare thread reads
  its directories the same way. macOS keeps rustix's `Dir`.
- **What a directory holds is held within a bound, past it on disk.** A
  directory's names are sorted in memory up to 64 MiB of them, each name's
  overhead counted, about a million names. Past that, each 64 MiB is sorted
  and written to a temporary file as a run, and the runs are merged as the
  names are taken, 128 at a time, in levels past that: the names come in
  the same order, so the manifest is the same. A run is front-coded, in
  blocks of up to 64 KiB, each compressed alone with zstd at level 1; what
  is held of it is where each block is and its first name, so merging 128
  runs holds 7.2 MiB. Whatever else grows with one directory is held the
  same way, each within the same bound: on the Source, the subdirectories
  still to list, those sent and not listed yet, and the entries sent and not
  said compared; on the Destination, its listing, the names of its
  subdirectories, what `sync` deletes in it, its batches held for `sync`,
  what the check keeps of those it announced, and what the decider is to
  know of them; see [Compare](#2-compare). Records kept in the order of
  their keys are found by key reading one block, the block last read kept,
  so that looked for in order each is read once. A directory under the bound
  touches no disk. The temporary files are in `TMPDIR`, or `/tmp`, as GNU
  sort's are: where `/tmp` is a tmpfs, they are in memory all the same, and
  `TMPDIR` moves them. Each has no name from the start, so nothing is left
  behind however the process ends; one that cannot be made fails the
  transfer, saying where, and that `TMPDIR` says where. In one directory
  of five million empty files, a first copy took the Source 146 to 152 MiB
  at its peak and the Destination 185 to 190, where the build before took
  550 to 568 and 2,702 to 2,801, in 25.8 to 26.6 s rather than 29.8 to
  30.6; a sync of it where 1% of the files differ, 160 to 168 MiB and 226
  to 235, against 376 to 377 and 806 to 826, in as long; and one deleting
  half of it, 150 to 168 and 307 to 336, against 248 to 250 and 903 to
  1,182, in as long: the temporary files on disk or on tmpfs alike. Trees
  of 100,000 directories of 10 files, and of 1,000 of 1,000, are copied as
  fast as before, or faster. Deleting a directory with all it holds, as
  `sync` does one the Source does not have, reads each directory in it a
  read at a time too; see [Compare](#2-compare).
- **Keeping within the bound costs a directory of many directories time.**
  A first copy of a million empty directories in one took 23.6 to 24.9 s,
  where it took 20.8 to 21.4 s before the bound, the Source 196 to 209 MiB
  rather than 260 to 274, and a dry run 22.6 to 23.2 s rather than 20.9
  to 21.3. Each
  directory announced is written down, serialized, and past the bound
  compressed to disk, by the check and by the stage that holds batches,
  and read back as it is entered, which takes the Destination longer than
  the maps it had: in a dry run, its thread that reads the manifest ran
  0.65 s longer, and that stage 1.08 s, and the Source waited for the
  manifest's window 1.33 s in all, 54 times, where it waited 0.02 s. In a
  copy, the decider's bound on the entries with the makers holds the
  Source back too, while the million directories are made, one at a time
  in their parent, which went on beside the scan of them before: without
  that bound, a copy took 22.0 to 22.7 s, and the Destination 554 to
  562 MiB rather than 280 to 317.
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
- **`--listers N` lists N directories at once, on each side.** On the
  Source, N listers each list a directory at a time, taking the next from
  those still to list, which they share: a stack of each listed
  directory's subdirectories, the first of the latest's, so that each goes
  down as far as it can, and they go down together. Each hands its
  directory's batches to the stat threads as it lists them, room kept in
  their pool and the next place taken for each, and a directory's batches
  all go before any of its subdirectories is listed, so that each comes
  after the batch that announced it, with nothing held back. What a lister
  does not go into it says: a name that was no directory as listed, with
  its batch; one on another filesystem, with `-x`, as it comes to it; and
  the sender keeps, for each directory sent, its subdirectories still to
  come, by name, within the bound. Each lister has the bound on what a
  directory holds for itself, so N of them hold at most N times as much;
  the directories held open for their subdirectories are 128 for all. On
  the Destination, N lanes compare N directories at once; see
  [Compare](#2-compare). One, the default, is the scan above. More is for
  trees of many small directories on a filesystem over the network, where
  each listing is a round trip. On fatcat's NFS, under a load of 21 to 45,
  a dry run of a tree shaped as `/usr`, 20,436 directories and 197,263
  entries, every directory changed first so that none was cached, took
  6.8 to 22.0 s with 16, a median of 12.6, and 7.9 to 51.6 s with four,
  against 28.7 to 128.3 s with one, a median of 40.7; in each round, 16
  were 1.7 to 10 times as fast as one. A sync of it to a copy of it there
  took 24.1 to 72.2 s with 16, against 44.9 to 291.4 s with one. Not for a
  first copy from a spinning disk: its small files are then read from
  places far apart, and a small program reading 80,000 files of 4 KiB from
  one so, 16 at once, took about five times as long as depth first. On a local disk it is
  slower, the listing cheap and the handing over not: on tmpfs, a dry run
  of 100,000 directories of 10 files took 3.43 to 3.46 s with four,
  against 3.01 to 3.09 s with one.
- **The manifest is records, each thing its own.** A directory is an `In`,
  its entries, sorted by name, with their metadata, and its `Listed`; one
  not listed, as one that could not be, or on another filesystem with
  `-x`, is `Unlistable` instead, and nothing in it is deleted. Owners'
  names go before they are first used, and paths that could not be read
  as they are met; see [What the pipes carry](architecture.md#what-the-pipes-carry).
  Nothing grows with a directory: the scan stats a directory's names 256 at
  a time, whatever its size, and writes each batch's records at once, so a
  large directory is statted by every stat thread, and sent as its batches
  are done, never held whole.
- **The transport compresses the manifest, as one stream.** Where the
  connection is compressed, the manifest pipe is one zstd stream, which the
  pump compresses as it takes what waits, and the Destination's reader
  decompresses as it reads: a directory alone, as most are small, has
  little to compress, and the entries before it are its context. On
  `/usr`, a crate registry's sources and a build's output, an entry took
  40 to 55 bytes, and 13 to 21 in the stream, 2.6 to 3.4 times fewer; each
  directory compressed alone gave 1.5 to 2. A sync with little to do sends
  little else. Compressing costs under a microsecond an entry: warm, on
  tmpfs, a dry run of 505,000 entries, compressed as asked, took 0.43 s
  rather than 0.40 s, as measured when the Source compressed it.
- **With one lister, the manifest has a fixed order: depth first, by
  name.** It is the order the lister goes in, so nothing reorders it, and
  the same tree always gives the same manifest, which makes tests
  reproducible and makes `--dry-run` and `--verbose` output comparable
  between runs. With more, the directories' batches come interleaved, as
  the listers hand them over, an `In` before each that follows another
  directory's, and a dry run lists in no fixed order. A name listed as a
  directory that its stat finds is no longer one is left out, with
  everything under it.
- **The scan and the transfer overlap.** The Source never waits for the
  scan to finish before sending, as rsync did before 3.0. Scanning first and
  sorting after would leave the network idle for the whole scan, and keep
  the whole tree in memory. A scan faster than the network waits while the
  manifest's window, 2 MiB on the wire, is used up, rather than piling it
  up in memory. That manifest does not hold the files back: once the
  sender has enough to send, at least a quarter of the data's send buffer,
  the data goes before the manifest, and below that, the manifest goes
  first again, so that the Destination finds more to need before the
  sender runs out. A sync that
  needs little keeps the manifest flowing at full speed.
- **The scan waits while too many needed files wait to be sent, or the
  manifest's window is used up.** Both sides keep track of each needed file
  until it is sent, and the manifest can run far ahead of the data, as on a
  first copy: so the scan waits while more than 100,000 do. The entries on
  their way, whose needs are still to come, are those in the manifest's
  window, and those the Destination took and has not compared: what waits
  for its stat threads, about 80 batches, and, for `sync` into a directory
  the Destination has, that directory's batches before its last, held past
  the bound on disk. There is no other bound on them; the window counts
  bytes on the wire, so where the manifest is compressed more entries are
  on their way, and the Source keeps more paths, past the bound on disk.
  Sending files needs no more of the manifest, and the Destination reads
  it as it goes, so the scan always goes on; once the files are all sent
  it waits no more, and a transfer that fails stops it at once. How many
  needed files wait at most has not been measured since the window took
  the place of a bound on the entries not compared. The thread that writes
  the manifest waits for the owner's word on each batch, a round trip
  between two threads, and a directory, empty or not, is a batch: in a dry
  run of a million empty directories in one, it waited for the owner
  13.5 s of 21, the owner idle nine tenths of the time.
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
  The other way too: a transfer that fails stops the scan at once, rather
  than once the whole tree is scanned and sent. The connection closing before the manifest's end fails the transfer too,
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
  takes it for extra. A directory that cannot be listed is sent as
  `Unlistable`, so nothing in it is deleted either.
  The Destination deletes only in a directory whose whole listing the
  Source sent: one it never sent, and all under it, is not even compared.

**Proposed.**

- **Each entry gets an ID in manifest order.** The answers refer to files
  by ID, not by path. Both peers see the same IDs without any sorting. The
  Source keeps each entry's path, to answer needs, only until the
  Destination says it compared it: once a batch's needs are written on the
  answers, the Destination says `Compared`, from its first ID to its last,
  and the Source lets go of its batches in order, as each is wholly said
  compared. A file needed takes its path with it, to be sent. So the
  Source keeps what of the manifest is on its way, which the window and
  the Destination's queues bound, not the whole tree; past the bound, on
  disk, where the thread that reads the answers, not the owner, which does
  no I/O, finds each file needed.
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
  else here, has everything in it needed, unlooked at. With `--listers`
  above one, it hands each directory's batches to one of as many lanes, in
  order, the one with the fewest, and the lanes open, list and merge them;
  a directory opened on a lane waits for the one it is in to compare the
  batches handed out before its own. The comparisons take their places in
  the manifest's order as they are handed out, room kept for each, and
  come out in it: a directory's first part still follows the one of its
  parent that announced it, as the decider needs, and with one lister on
  the Source a dry run says the same whatever the lanes. A slow directory
  holds back what comes out after it, up to 64 comparisons, compared
  meanwhile.
- **Each batch is merged with the listing.** A directory's entries are
  compared in batches as they come, 256 at a time, or fewer at its
  `Listed`. Both are sorted by name, the listing a stream, as the Source's
  names are, so they are read side by side. An entry the Destination does
  not have is needed, without a stat. One it has is statted, by a pool of 8
  threads, in order, as on the Source. A name the Destination has that the
  Source does not is what `sync` deletes; it is found as the batches come,
  and the whole of a directory's once its `Listed` is here, without a
  second listing, and deleted 4,096 at a time. The compare thread finds
  each directory by its ID.
- **The manifest is checked before anything of it is acted on.** Whatever
  a Source sends, by a bug or ill will, the Destination touches only what
  is under its root, by names the Source listed, record by record, as each
  comes: the root comes first, and once; each name is one, not `.` or
  `..`, nor with a `/` or a NUL in it; an `In` names only a directory
  announced as one by its parent's entries, and not yet ended; each
  directory announced ends once, by its `Listed`, or its `Unlistable`, which
  comes only before any of its entries; a directory's names come strictly
  in order, and IDs in turn; the end comes only once every directory has
  ended; an mtime's nanoseconds are fewer than a second's. Directories may
  come interleaved, in any order that keeps these. Anything else stops the
  transfer at once, as any stage failing does. What the check keeps of the
  directories announced and not begun is held by the one they are in, by
  ID, so that a directory of however many takes no more.
  Under that, every call that changes or goes into what a name names
  refuses what is not one name, and a path is never followed through `.`
  or `..`: should the check miss something, nothing out of the directory
  given is reached. Both are fuzzed, every night, with what a Source gone
  wrong could send, the manifest's records and the data's together, in any
  order, and nothing beside the root may change; so are the packets, the
  data's records, and the pipes' frames each side reads, compressed or
  not: see `fuzz/`.
- **A directory `sync` goes into is applied whole, extras first.** Where the
  Destination has the directory, its batches are compared as they come, but
  nothing of them is made until its last is compared; then what it has
  extra is deleted first, and its batches are applied after, in order, as
  rsync does: its files may be written meanwhile, without a name, but take
  their names only after.
  Nothing made for a directory is so ever taken for extra, even where the filesystem takes two
  names for one, as one that ignores case does; and an extra that is what
  the Source could not read, under such another name, is kept. Two of the
  Source's own names that the filesystem takes for one, as `A` and `a`, are
  never both written: the first made keeps the name, and the other fails,
  saying which it is taken for, with all under it, as nothing is written
  over, deleted, nor written into for it. Only a name the comparison did not
  list is so checked, and only once it is found taken, so a transfer pays
  nothing for it otherwise. Its batches are held until then by a stage of
  the comparison's, past the bound on disk. A `copy`, or a directory the
  Destination does not have, which has nothing extra, has each batch
  applied as it is compared. The decider takes
  the comparisons, in order, as each comes, and has what a batch changes, the directories, symlinks and special
  files to make and the attributes to set, made by up to 16 makers, a
  directory each, as within one the kernel makes one name at a time; it
  goes on meanwhile, and the thread that reads the manifest only reads
  and hands on, but for the root, which it applies before any batch comes.
  The stats only a dry run needs, of an entry that would replace a
  directory and of the extras a run would keep, are the stat threads'.
  A batch's files are asked for as the decider takes it, where the
  comparison found its directory, and take their names once the batch is
  made. It takes no more batches while 100,000 files it asked for have not
  come, nor while the batches with the makers hold 100,000 entries: the
  comparison waits for it, and the scan for the manifest's window. A first
  copy of a million empty files in one directory took the Destination 174
  to 188 MiB at its peak, where it took 710 to 759 MiB when all of a
  directory's files were asked for at once, and a tenth less time.
  A batch of a directory still to be made waits for its parent's
  batch to make it, and its files are asked for once it is; only then is
  it said `Compared`, which lets the Source forget what it sent. A
  directory is done once its own `Listed`, or `Unlistable`, came, and its
  batches, and its parent's that make or change it, are made. The decider
  keeps a directory only once its first batch, or its `Unlistable`, has
  come, or while its parent's batch makes it: before, it is one its parent
  waits for, and what its parent's batch announced of it, its entry and
  whether that batch changes it, comes with its first batch, kept until
  then by the stage that holds batches, by the directory it is in. A first
  copy of a million empty directories in one took the Destination 281 to
  298 MiB, where it took 647 to 661 MiB when the decider kept every
  directory announced, in the same time. A first copy
  of 20,200 empty directories took 6.5 s on NFS with one thread making them,
  and takes 2.0 s; on WekaFS, under a fifth of the time, measured against
  each other in turn.
- **What is deleted is deleted from the directory listed.** The comparison
  opens each directory once, relative to its parent, and its extras are
  deleted from that very directory, by its file descriptor, never by its
  path again; everything else under the root is reached from the root as
  the comparison opened it, once. A path that comes to name another
  directory meanwhile, as a symlink switched or a disk mounted over it,
  never has extras listed in one deleted from the other.
- **Deleting spreads over directories.** For `sync`, a directory's extras
  are deleted by up to 16 threads, the lane's own included, each
  emptying a directory of its own: within one directory the kernel takes
  its lock for each name, and on a filesystem over the network each name
  is a round trip. What a listing says is no directory is unlinked at
  once, and only a directory is opened, so that a file costs one round
  trip, not two. A directory is read 256 KiB of names at a time, in the
  order it gives them, and each read's names are deleted before the next:
  a read that brings directories queues them, and the next read waits
  beneath them, so that what waits to be gone into is never more than a
  read's worth for each directory being emptied, and one deleter at a time
  deletes in a directory. One found not empty once emptied, as a
  filesystem may skip names while others are deleted, is read again from
  its start, as long as the pass before read some names. Deleting a
  directory of two million empty files and 200,000 directories of one each
  took the Destination 142 to 164 MiB, where it took 394 to 396 MiB with
  each directory read whole, in as long; of the two million files alone,
  78 to 86 MiB, against 286 to 298. The deletion runs on a lane of its own, a directory's
  extras at a time, and the decider goes on meanwhile: only that
  directory's batches wait for it, as nothing is made, nor named, in a
  directory before its extras are deleted. Its files are asked for at once,
  and written meanwhile, where files are written without a name, as on
  Linux's own filesystems; where they would be written under a temporary
  name, as on NFS and macOS, they are asked for after, as names made and
  deleted in one directory only take turns at its lock. A sync deleting
  20,000 files in one directory and copying 20,000 into it took 0.62 s to
  ext4, and takes 0.52 s; on NFS, it took 19.3 s with its files written
  beside the deletion, and takes 17.0 s, as before. A sync that deleted
  20,000 files in one directory and copied 20,000 new ones into another
  took 24.6 s on WekaFS and 10.1 s on NFS with the thread that applied the
  comparisons waiting, and takes 13.5 s and 7.9 s, about the deletion
  alone. Deleting
  50,000 files in 500 directories took 44 s on WekaFS and 8.5 s on NFS,
  and takes 2.3 s and 1.7 s now; 20,000 in one directory took 5.8 s on
  NFS, and takes 3.5 s, and 10,000 in one took 8.7 s on WekaFS, and take
  6.7 s.
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
  the Destination writes back on the answers, one record a file.
- **What it costs.** For a directory that exists, a listing and a stat of
  each entry both sides have. Against a stat of every entry the manifest
  names, that is one more read of the directory for `copy`, and one fewer
  for `sync`; it saves a failed stat for each entry the Destination lacks,
  and on NFS, whose listings bring each entry's attributes, it may make the
  stats cheaper too. An empty destination costs nothing to compare. A
  directory's listing is read as the batches come, within the bound.

The only round trip is between the manifest and the needs, and it is
pipelined: the Source starts sending the first needed files while later ones
are still being compared.

**Decided.** A directory of the Destination's that cannot be opened or
listed, or an entry that cannot be statted, fails alone: each of its
entries is reported as failed and left as it is, a directory that cannot
be opened told once, and the rest is compared and copied all the same. Nor
is a difference what this process could not set: another's permissions,
group, or mtime.

**Not chosen** (2026-10-28 and 29, with the bound on a directory).

- **Sorting in memory always,** as before: memory had no bound, a
  directory of five million entries taking the Destination 2,702 to
  2,801 MiB.
- **The temporary files in `/var/tmp`,** where systemd and the FHS put
  large ones, on disk; or `temp_dir()` checked with `statfs`, and another
  directory used where it is a tmpfs. Either keeps them out of memory;
  `TMPDIR`, as GNU sort has it, is simpler.
- **Stat results written into the runs.** The Source stats the names in
  the order sorted, so a run needs only names and their kinds.
- **No sorting.** Comparing as the manifest comes needs both sides in one
  order.
- **The subdirectories still to list kept in memory,** with their number
  said to bound what the scan holds: about 70 bytes each, 350 MB for five
  million.
- **Every batch applied as it comes, for `sync` too,** each extra checked
  to be the one listed, the same inode, before it is deleted, so that a
  name just made is never taken for one. Every case of two names taken for
  one would be checked again, an extra directory and a file made under its
  other name among them.
- **No bound on the files asked for.** All of a directory's files asked
  for at once, and held for until each comes, took 500 to 600 MiB for a
  million, as the difference of a copy and its dry run puts it.
- **One store for every directory announced, in the check.** What is
  written down can only grow, so those begun would stay with those not,
  every directory of the transfer, in memory up to the bound, then on
  disk.
- **Each read of a directory being emptied from its start,** which would
  need no filesystem to keep its place while names are deleted. On ext4 a
  read walks the blocks emptied before it, and over NFS fetches them
  again, so a directory would be read over once for each read.

**Proposed.**

- **`--checksum`, later.** Compare files by their BLAKE3 hash instead of size
  and mtime, for when content must be confirmed identical. It reads every file
  on both sides, so it is never the default.
- **rsync's other comparison options wait for a need:** `--size-only`,
  `--update`, `--ignore-existing`, `--existing`, and `--ignore-times`.

### 3. Read and hash

**Decided.** The Source reads each needed file and hashes it with BLAKE3 as it
reads, in parallel, up to the size it had when opened, a file larger than
2 MiB a segment at a time, in order, 16 files at once whatever their
size; see the integrity rules in the
[overview](overview.md#every-file-is-complete-and-correct).
One thread, the owner, keeps everything the Source knows of what to send:
the entries sent and not compared, the files needed and not read, those
being read in segments, the records with the data sender, and how far the
scan may go. It does no I/O: the scan, the readers, the data sender and the
connection carry out what it says, and tell it what they did, each in the
order it happened, on one queue. The scan writes each batch of the
manifest once the owner says it may, and the owner keeps the batch's
entries before that, so that any the Destination needs are there.
It opens only what is still a regular file, and opening never waits: a file
made a FIFO, a device, or a directory since the scan is closed again unread
and reported as unreadable. Nothing is checked before the open, since such
files are rare and a `stat` per file is a round trip on a network
filesystem. On Linux, files are opened with `O_NOATIME` where the kernel
allows it, so reading them does not update their access time.

### 4. Serialise and compress

**Decided.** The data the Source sends is records on the data pipe, each
thing its own; see [What the pipes carry](architecture.md#what-the-pipes-carry):

- **A file of up to 2 MiB goes whole,** its bytes and its BLAKE3, as a
  `File`; one read short goes with why, in place of its hash.
- **A larger file goes in segments,** each 2 MiB of it from a multiple of
  2 MiB, or what is left of the file there, as a `Segment`, with the
  chaining value of its part of the file's BLAKE3.
- **A file's hash is the same as of all its bytes at once.** BLAKE3 hashes
  a tree, of which a segment, aligned and a power of two long, is a
  subtree. The Source merges a file's hash from its segments' chaining
  values once all are read, and writes it as a `Hash` after its last
  segment; the Destination checks each segment as it comes, and the hash
  merged from them before the file takes its name.
- **A file needed that could not be opened** is an `Unopened`, so that its
  directory is finished without it.
- **The data ends with its own record, `End`,** before the pipe's end. A
  data pipe that ends without it, as a Source's that failed do, is cut,
  and the decider says the data ended early; the Source's failure, said on
  control before its pipes ended, is the cause.
- **One data sender writes them,** in the order the owner hands them over,
  each encoded with the file's bytes copied once; those waiting together go
  in one write, up to 64 KiB, a larger one alone. Each small file written
  alone, 50,000 files of 4 KiB took a tenth longer on one machine.
- **The owner hands the data sender at most 16 records** it has not
  written: with all of the window there, a copy of a large file on one
  machine took a tenth longer, its buffers' page faults twice as many, as
  measured when 16 blocks waited for compressors.
- **No packs.** Small files go one after another, a record each; the
  transport cuts the data into chunks of 2 MiB whatever the records, so
  files next to each other are compressed together.
- **2 MiB, as the blocks before them were.** Measured on crate sources, a
  build's output, `/usr`, a tar of sources and large files of model
  weights, numbers and zip, each block compressed alone, packs and
  segments of 2 MiB sent as much as frames of 2 MiB of one stream of files
  did. Of 8 MiB, they sent 0.2 to 1.1% less at zstd 1, and 1.5 to 9% less
  at zstd 3; but a local copy of a 4 GiB file on tmpfs took 3.3 s with
  them, against 2.9 s. A segment is the unit the Source reads and hashes a
  large file in; a chunk, the unit the transport compresses.
- **A changed file is sent whole.** There is no delta transfer; see the
  [overview](overview.md#scope-one-way-only).
- **Flow control is the data pipe's window, and what the Destination
  holds.** The data's window, 64 MiB on the wire, is given back as the
  Destination's reader reads; what it has read and not yet let go of,
  written or failed, it bounds apart, by the same 64 MiB, counting each file
  as at least 4 KiB. The reader waits while that is held, so the window is
  not given back, and the Source waits. So the Destination never stops
  reading its connection for want of room: its manifest and the other
  pipes never wait behind data.
- **The Destination may hold twice the data's window.** Before the pipes,
  the credit bounded what was sent and not written, in all; now the
  window bounds what is in the pipe, and the same again bounds what was
  read, so a Destination writing slower than the data comes holds up to
  128 MiB of it. On nano4, writing to WekaFS, a Destination's peak was 210
  to 230 MiB, against 106 to 128 before the pipes; with `--data-credit` at
  16 MiB, 115 to 131, against 80 to 86. About 20 MiB of it does not change
  with the credit, and is not accounted for.
- **The Source fills each of its buffers.** While the Destination is
  slower, the reads, 32 at most, 64 MiB of large files, the 16 records
  with the data sender, 32 MiB, and the data's send buffer, 16 MiB, into
  which the records' bytes are copied again, all fill: about 112 MiB at
  most. On nano4 a Source's peak was 94 to 100 MiB, against 43 to 90 MiB
  before the pipes, whatever the credit.

D16 had kept one long record for a large file, weighing speed alone. Blocks
were decided on 2026-10-23, for the rest: a large file no longer holds back
the files after it, and a block could be sent again on its own, as resuming
would need; see the [redesign](../drafts/architecture-redesign.md). Sending
again went with the reconnecting, on 2026-10-25, and the blocks on
2026-10-29: the data pipe keeps the order, and the transport compresses.
What is left of them is the segment, and a record for each file.

#### Compression

**Decided.**

- **The transport compresses; the peers do not.** Each connection's pipes
  are compressed or not as the client decides for it: to a host at zstd
  level 1, on this machine not. `--compress-level <n>` sets a level for
  every connection, this machine's too, and `--no-compress` turns it off,
  for fast local networks where CPU matters more than bytes.
- **The data's chunks are compressed each alone, in parallel,** by 8
  threads, as pigz does: a chunk is cut once 2 MiB of the data wait, or
  as much as waits when the wire would idle, at most 8 compressed or being
  compressed ahead of the pumps. The other side decompresses them by 8
  threads, and puts them back in order, holding at most 16 MiB of them
  decompressed and not read, but for the chunk next in turn.
- **A chunk that does not shrink is sent as is.** Already compressed files
  (`.gz`, `.mp4`, `.zst`) do not cost CPU twice.
- **Every other pipe is one zstd stream:** the manifest, the answers, the
  reports, and control, each compressed as its pump takes what waits, at
  most 64 KiB, and flushed at once. Bytes gather in the send buffer while
  the wire is busy, and compress the better; while it is not, they go at
  once, waiting for nothing.
- **Decompression speed barely depends on the level,** so a higher level
  costs the Destination almost nothing.
- Over SSH to fatcat at level 1, a crate registry's sources, 522.5 MiB,
  took 89.9 MB on the wire, against 97.2 to 98.6 MB as blocks, each
  compressed alone: 84% saved, against 82%. They took 2.4 to 2.6 s,
  against 3.9 to 4.1 s.
- **On one machine, compressing costs the transport about a fifth.** With
  `--compress-level 1` on one machine, the transport alone was about a
  fifth slower than without, in the best runs: each chunk is copied once
  more, and handed between threads.
- **On a slow connection, a short transfer sends more than it would
  gathered.** Bytes gather only once the buffers past the pump, the
  socket's, SSH's channel window, and TCP's, are full. Where the
  connection is slower than what is compressed comes, and the transfer
  short, they wait there instead, compressed in small pieces: simulated at
  100 KB/s, a dry run's 53,362 paths took 4.06 s, against about 3.35 s had
  they been compressed as one; at 10 MB/s, no longer. The time lost is
  once, at most about 2 MiB over the rate.

**Decided.** Level 1, fixed. A level that adapted was tried first, rising
while the queue of data packets to the connection stayed full. It was too
slow: on one machine, or with a disk slower than the compressors, the queue
stays full whatever the level, so the level climbed until compressing was
the bottleneck. A fixed level of 1 or 3 copied 1 GiB of large files on one
machine in 1.4 s, against 4.5 to 17 s, and pushed them over one SSH
connection in 32 s, against 42 to 56 s.

### 5. Decode

**Decided.** The Destination's data reader reads the records in order.
Each file's, or segment's, it hands to 8 checkers, once it may hold it,
which check its bytes against its hash, or a segment's against its
chaining value; a feeder hands them on, in the order they came, to the
decider. The hashes of files in segments, and the files the Source could
not open, the reader tells the decider itself, and at the data's end, how
many records of files there were, which the decider waits for. A record
that does not decode, or is one the Source would not write, fails the
Destination at once. Nothing past the reader waits for room: what it holds
is bounded, so a disk slower than the network holds the Source back, by
the data's window, rather than filling memory, or holding back the
connection.

**Each record goes on alone.** Each file is handed to the checkers, and on
to the decider, by itself, where a block of several small files went at
once. On a crate registry's sources, 36,240 files of all sizes, copied on
one machine, where the client and both servers share the 8 cores `--cpus`
gives, nearly all of them busy, the checkers took 0.75 s of CPU against
0.40 s, waking 16,700 times against 9,100, and the copy took 7 to 15%
longer than before the pipes; given 24 cores, it was no slower.

### 6. Write

**Decided.** Each file is written without a name, on Linux where the
filesystem can, or else to a temporary name; its hash is verified, its
metadata is applied, and it is given its name, in a batch. Nothing is
`fsync`ed. A file under a temporary name is closed once its metadata is
applied, and fails if closing it does, as rsync checks it: on NFS, closing
it is where an error writing it back is told. These rules are in the
[overview](overview.md#every-file-is-complete-and-correct).

**Decided.** Writing runs in parallel, a whole file or a segment per
writer, each file by one writer at a time. A file in segments is made once
its first segment comes, and each segment is written where it goes in it,
with `pwrite`, one at a time, in the order they came: several writers in
one file only take turns at its lock, and on WekaFS, sixteen came at 60%
of one's speed. Copying a file of 4 GiB from memory to ext4 took 3.1 s
either way, and one of 2 GiB to NFS 3.6 s. Once all are written, and the
hash merged from their chaining values is the Source's, it is given its
attributes. A file not as the Source read it, or which it could not read
whole, fails, and nothing made of it is left. A file that fails goes back
to the Source on the answers, as `Failed`. The writers have at most two jobs each,
one each and one ready, which the decider hands them, and say what became
of each.

**Decided.** Files are renamed in batches of at most 256, by 4 renamers that
last the transfer: the decider hands each batch over as it fills,
or a second after its first file came, if it is not full by then, and goes
on, so that files written while no more come, as while a large file is
still sent, take their names then rather than at the end: of 20,000 files
of 4 KiB copied beside one of 4 GiB, from memory to ext4, the last took its
name after 1.8 to 2.0 s, where it waited for the end, at 3.6 s. One renamer renames the files of a batch's largest directory,
while the others rename the rest, a directory each. A directory's files are renamed by one renamer at a time:
the kernel renames in a directory one at a time, under its lock, so
renamers that share one only contend for it. A batch in one directory is
renamed by one renamer, and one in several, where each rename waits on a
server, by several at once. With 20,000 files of 4 KiB, 100 to a directory,
on WekaFS, this was 26% faster than one renamer, and with 200,000 written
without a name on ext4, 20% faster; with 1,000 to a directory, as fast on
both. At most two batches wait
for the renamers, beside the one filling, and a file's result goes once it
is renamed. A file without a name takes its own with one `linkat`, where
the directory had nothing by that name as compared; otherwise, or if
something took it since, it is linked to a temporary name and renamed over
what has it. A file waiting for its batch holds its directory's file
descriptor, which every file of that directory shares, and its
registration for removal on a signal too; and, without a name, its own,
so batches are half as large where few files may be open. Each directory is opened once from the root, a name at a time,
not by its path; the few most recently used stay open. A file with a
temporary name is closed once its attributes are set. Where few files may be open at once, batches are
smaller, so that no file fails for want of a descriptor. A file written
while its directory's extras are deleted waits for its batch to be made to
take its name: as many wait so as the descriptors left beside the batches
allow, at most 65,536, and no job more goes to the writers while they do.
The renamers let go of each file they renamed, and say what became of it.

### 7. Results

**Decided.** One file failing does not stop the transfer. The Destination
reports each entry it cannot make match, as it finds out, and the client
shows it to the user. The scan is taken as a snapshot, and nothing is sent
again, unlike rsync, which redoes a file once. Each record of a file
carries the size and mtime the Source found when it opened the file, and its content is read
up to that size, as rsync does: what the file gains while it is read is
left out, and the next run finds its mtime changed. Of a file that shrinks
while it is read, or whose reading fails, none of the bytes are sent, but
why, and it fails; of a file in segments, nothing more is sent once one
says so. A file that cannot be opened, including one that is no longer a
regular file, is reported by the Source and sends nothing; the Source tells
the Destination, so that its directory is finished without it.

### 8. Finish

**Decided.** The Destination finishes each directory, setting its owner,
permissions, and mtime, as soon as nothing more is made in it nor under it:
once its own `Listed`, or `Unlistable`, has come, every file in it is
written, and every directory in it is finished. A directory is found by its
ID, and has ended once its `Listed` or `Unlistable` came, whatever the
manifest goes on to; one whose `Unlistable` comes before its parent's
`Listed`, which the scan never writes but the check lets by, is finished
only at the end. Up to 16 threads finish them, each a
directory of its own, as the decider hands them over as they are done,
which is deepest first, and a directory only once every directory under it
is finished, so that one that cannot be gone into after is finished only
once nothing under it is left to make nor finish. The decider is done once
the last is finished. A first copy of 20,200 empty directories took 3.5 s on ext4,
where each directory was synced then, 13.2 s on NFS and 71 s on WekaFS, with one
thread, and takes 1.1, 6.8 and 49 s, before making them was spread over
threads too; see [2. Compare](#2-compare). A
needed file the Source could not open is settled when the Source says so,
as one it could not read whole is when it comes: its directory need not
wait for it. Only a directory with a file that never came, not even so, is
finished at the end, and that file fails, as never sent. A directory the Destination has, owns, and may not
read, write, or go into, is given all three for its owner as its scan opens
it, as rsync does, so that it can be worked in, and an extra one so before
it is deleted; such a directory is finished whatever else changes in it, so
that it ends with the Source's permissions. Once the Destination stops, no
directory more is finished; should the transfer fail before one is, or an
extra one not be deleted after all, it is given back the permissions it
had. What the
Destination keeps of directories so stays as small as the directories
announced and not ended yet, and those with files still to write. For `ferry sync`, what the Source does not have was deleted already,
a directory at a time, as each was compared. Last, it reports the summary to
the client.

**Decided.** A directory that cannot be finished fails alone, reported as
its entry, and the others are finished all the same. Its owner,
permissions, and mtime are each tried, whatever came of those before; what this process may not set is a warning, as for a file.
