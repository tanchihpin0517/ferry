# Ferry benchmarks

Ferry against rsync, on generated trees, on one machine and over SSH. The
numbers are made by [`scripts/bench.sh`](../scripts/bench.sh), which makes the
trees, builds a release of Ferry on both sides, and runs every case once.

```text
scripts/bench.sh [HOST [DIR]]
```

## The trees

Made by `ferry fixture generate`, the same on every machine. Text compresses
about as prose does; random bytes do not compress.

| Tree | Files | Size | Content |
|---|---:|---:|---|
| small | 100,000 of 1 KiB, 100 to a directory | 98 MiB | text |
| large | 8 of 128 MiB | 1 GiB | half text, half random |
| mixed | 10,000, from 1 KiB to 16 MiB, most of them small | about 1 GiB | half text, half random |

## 2026-10-15

- **This machine:** Apple M1 Pro, 8 cores, 16 GB, macOS 26, APFS on its SSD;
  rsync 3.5.0.
- **The host:** Intel i7-13700K, 24 threads, 32 GB, Linux, ext4 on NVMe;
  rsync 3.4.1. The trees go to its home directory, on disk.
- **Between them:** a round trip of 5.7 ms; one TCP connection carries about
  44 MB/s, and four together about 60 MB/s.
- **Ferry** at commit `b79f1db`, with its defaults: `fsync`, and compression
  at a level that adapts. rsync does not `fsync`.
- Caches are warm, and each case ran once, so differences of a few tenths of
  a second, or of a few percent over SSH, say nothing.

Seconds; a fresh copy, unless *unchanged*: the same copy again, with nothing
to send.

| Tree | Case | rsync -a | rsync -az | ferry | ferry, 4 SSH connections | ferry --no-compress | ferry --no-fsync |
|---|---|---:|---:|---:|---:|---:|---:|
| small | local | 24.4 | | **16.2** | | 20.5 | 16.8 |
| small | local, unchanged | 0.5 | | 0.4 | | | |
| small | push | 11.7 | **10.1** | 12.1 | 12.7 | | |
| small | push, unchanged | 1.4 | | **0.3** | | | |
| small | pull | 25.5 | | **14.6** | 17.6 | | |
| large | local | 2.1 | | 4.5 | | **1.1** | 16.3 |
| large | local, unchanged | 0.1 | | 0.1 | | | |
| large | push | 79.7 | 32.7 | 56.4 | **30.4** | | |
| large | push, unchanged | 0.1 | | 0.1 | | | |
| large | pull | 32.4 | | 16.7 | **12.5** | | |
| mixed | local | 5.8 | | 4.4 | | **3.2** | 7.0 |
| mixed | local, unchanged | 0.1 | | 0.1 | | | |
| mixed | push | 81.0 | 34.3 | 34.5 | **18.7** | | |
| mixed | push, unchanged | 0.2 | | 0.1 | | | |
| mixed | pull | 35.4 | | 16.5 | **12.3** | | |

### What the numbers say

- **Pulling is about twice as fast as rsync** for every tree. Pushing the
  trees with large files over four SSH connections is about twice as fast as
  `rsync -a` too. Over one, they go no faster than `rsync -az`, which
  compresses at a fixed level, and the large tree slower, for the
  compression level; see below.
- **Many small files:** faster than rsync on one machine and pulled; pushed,
  as fast as rsync. Four SSH connections do not help them: the time goes to
  the files, not to the connection.
- **Nothing to send:** as fast as rsync or faster; 0.3 s against 1.4 s for
  100,000 files pushed.
- **The adaptive compression level is too slow.** It is why a large tree on
  one machine takes 4.5 s, or 16.3 s, against 1.1 s without compression, and
  why one SSH connection pushes large files slower than `rsync -az`. The
  `--no-fsync` column shows it too: that run was no faster for skipping
  `fsync`, its level having climbed higher. See below.

### The compression level

Run again, the cases where compression decides:

| Case | adaptive | `--compress-level 1` | `--compress-level 3` | `--no-compress` |
|---|---:|---:|---:|---:|
| large, local, three runs | 13.8, 12.1, 16.6 | 1.4, 1.4 | 1.5, 1.6 | 1.1, 1.2, 1.2 |
| large, pushed over one SSH connection | 42.5 | 32.1 | 32.3 | 70.0 |

At a fixed level of 1 or 3, a local copy is nearly as fast as without
compression, and a push as fast as `rsync -az`. The adaptive level raises
itself while the queue to the connection stays full, and on one machine, or
with a disk slower than the compressors, the queue stays full whatever the
level. It climbs until compressing is what the transfer waits for, and the
time it takes to come back down is time lost.

The adaptive level was removed after these runs; Ferry now compresses at a
fixed level of 1, unless `--compress-level` sets another.
