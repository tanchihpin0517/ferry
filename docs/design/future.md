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
