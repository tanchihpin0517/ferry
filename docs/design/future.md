# Future improvements

[中文](future.zh-TW.md)

What Ferry may do better later. None of it is scheduled or decided: each,
once taken up, gets a draft in [docs/drafts/](../drafts/README.md), and is
decided there. Where the [overview](overview.md#scope-one-way-only) says
something is not done, it stands until then.

## Resuming a large file across runs

As rsync's `--partial` does: a large file cut short is taken up where it
was, the next time, rather than sent again from its start.

- **Why.** A host has one SSH connection, and should it drop, the transfer
  fails; a rerun skips the files that are complete, but sends a large one
  cut short whole again. The longer the files, and the less sure the
  connection, the more that costs.
- **Where it would go.** A file larger than a segment is sent a segment at
  a time, each checked against its hash as it comes, the file's hash
  chained from its segments'; so the segments already written and checked
  are known as such.
- **To be decided first.**
  - Whether the file being written has a name, for the next run to find it:
    it has none until it is complete where the filesystem allows, and
    otherwise a temporary one, which a failure removes.
  - From what size a file is resumed, rather than sent again.
  - Whether the chained hashes of the segments already written are read
    back from the disk and worked out again, to trust them, or taken as
    they were recorded.

## Sparse files

As rsync's `-S` does: a file's holes are kept holes, rather than written as
zeros. A segment that is all hole would go as a hole, with no bytes; the
Destination would leave room for it without writing it.

## A coarse delta of large files

A large file that changed in places would send only the segments that did:
the Destination sends the hashes of the segments of the file it has, and
the Source answers those that are the same with "keep it". Coarser than
rsync's rolling checksums, as it finds only segments that stayed where they
were; it costs a read of the Destination's copy, to hash it, rather than
sending the whole file.

## Hardlinks

As rsync's `-H` does: names that are one file on the Source are one file on
the Destination, rather than copies.

## Several segments of one large file read at once, by filesystem

A large file is read a segment at a time, in order, and 16 files at once,
whatever their size: from a spinning disk, segments of one file read at
once defeat its read-ahead, 16 threads reading at 60% of one's speed.
Where each read waits for a server, the opposite holds:

- On NFS (fatcat, NFSv4.1, `nconnect=8`), one file's 2 MiB segments read
  by 1, 4 and 16 threads came at 691–793, 881–1,104 and 1,065–1,077 MB/s,
  and a file of 4 GiB copied to memory in 3.74–3.99 s read so, against
  6.30–6.62 s in order.
- On WekaFS, 4 threads read 7.3–9.6 GB/s, against 3.9–4.0 for one, and 16
  only 0.8–1.0; Ferry itself reached about 0.7 GB/s there, held back
  elsewhere.

The Source would look once at the filesystem its root is on, with
`statfs`, and read up to so many segments of a file at once on those it
knows: 8 on NFS, 4 on WekaFS (whose magic, `0x18031977`, `stat` does not
name), one elsewhere, as now. A list of filesystems and their limits to
keep; a filesystem mounted under the root is taken for the root's; a
limit set wrong is slower than one. Not measured: what the small files
beside it lose, and other filesystems over the network.

## A single large file on WekaFS

On tmpfs, a file of 4 GiB copies in 1.6 to 2.0 s. On WekaFS, one thread
reads it cold at 3.9 to 4.0 GB/s, yet copying it to memory took about 6 s,
measured before the stream layer was thinned on 2026-10-25, and not
since. Why has not been looked into.

## Large files ahead not keeping small ones waiting

Every file needed is read in the order its need came, 16 at a time, the
data's window first come, first served; large files that come first take
the readers. On fatcat's NFS, 16 files of 128 MiB ahead of 20,000 of
4 KiB took the tree 2.37 s, where large files one at a time took 3.45 s,
but half the small files were written after 2.2 s rather than 0.5 s.
While small files wait, large ones would take at most half the readers.
Another rule, and large files later; on a spinning disk, more seeking
between the two. Not measured on WekaFS since 16 files are read at once.

## Small files handed to the writers in batches

The decider hands each small file to a writer as a job of its own, and
each writer says when it is done. With 200,000 files of 4 KiB on tmpfs,
the decider was busy 0.82 s of 1.5 to 1.7, and the writers about a fifth
of their time: no bottleneck now. A job would hold up to, say, 16 files or
1 MiB, of those already come, never waiting to fill it, and its results
come back together, each file's on its own. More for the decider to do and
test; a file's result waits for the rest of its batch.

## The Source's reads in path order, with several listers

With `--listers` above one, the files needed come from several branches,
read as they come; from a spinning disk, a small program reading 80,000
files of 4 KiB from 16 subtrees in turn took 105.7 s, against 21.7 s
depth first. The Source's owner would take the files waiting in the order
of their paths, as an elevator goes, so that a branch's are read
together, and none waits for ever; on a first copy up to 100,000 wait,
enough to sort. Not measured: how far listers sharing one stack actually
interleave, and what it gains.

## Peak memory back to before the pipes

The Destination returns the data's window as it reads, and holds what it
read and has not written within the data credit again, so that writing
slower than the data comes, it holds up to twice the credit, where it held
once before the pipes: writing to nano4's WekaFS, 210 to 230 MiB against
106 to 128. The Source holds up to some 112 MiB of reads, records and the
send buffer. Ways, to be taken together or not: the window and what is
held, each half the credit, which a 32 MiB window would cap at 250 ms of
round trip on 1 GbE; only what is held made smaller; the Source's sender
counting bytes, and handing the transport its buffers rather than a copy;
or saying so, as it is. With them, the window and the credit sized to the
link, rather than fixed. About 20 MiB on the Destination that does not
change with the credit has not been found.

## Control messages kept from waiting behind the data

The data on an SSH connection is bounded by its window, 64 MiB, and each
channel's; what is written later on the same TCP connection, a
heartbeat, a stop, a host's report, waits behind it. On a simulated link
of 250 ms and 1% loss, at about 0.1 MB/s, a heartbeat took a median of
6.2 s to cross, at worst 9.2, and a Ctrl-C's stop as long to reach the
host. The Destination would say every 256 KiB or so how far it has read,
and the Source's pump hold what is written and not yet said read under
the rate times the least round trip, plus 1 s, and no less than 256 KiB:
control messages would then be late by about a second and a round trip.
A wait and its state in the pump, a rate estimate that may slow a
transfer as it wavers, and some hundreds of small messages a second; not
measured whether clean links stay as fast.

## Gathering a pipe's bytes on a slow link

The transport compresses what a pipe holds as it takes it, waiting for
nothing more; on a link slower than what compressing makes, the bytes
queue in the socket, SSH's window and TCP's buffers rather than in the
pipe, and go less compressed: at a simulated 100 KB/s, a dry run listing
53,362 paths took 4.06 s against about 3.35, a first copy's manifest and
data 8.66 s against about 6.5; at 10 MB/s, the same. A long transfer
fills those buffers and gathers by itself. Each pipe would be flushed at
most every D, as 50 ms, or the window sized to the rate times the round
trip, so that the pump is held back sooner. Not chosen with the pipes
(see [architecture](architecture.md#transport)) as a message just after
a flush would wait up to D on a fast link too; kept here for a slow one.

## Copying within one filesystem by the kernel

Where both roots are on one filesystem on this machine, the kernel could
copy a file, or share its blocks, as `copy_file_range` and a reflink do,
rather than Ferry reading and writing it. What it would check of the copy,
and whether it is on by default, are to be decided; the suggestion was
off. Not measured.

## The manifest's writer waiting for the owner on each batch

The Source's thread that writes the manifest asks the owner whether each
batch may go, and waits for its word, a round trip between two threads; a
directory, empty or not, is a batch. In a dry run of a million empty
directories in one, it waited 13.5 s of 21, the owner idle nine tenths of
the time; see [Scan](pipeline.md#1-scan).
