# Ferry profiling

[中文](profiling.zh-TW.md)

Status: **decided, and built.** The [architecture](architecture.md) describes
the client and the servers whose threads this measures, and the
[pipeline](pipeline.md), the stages they run.

**Contents**

- [What it answers](#what-it-answers)
- [Threads and states](#threads-and-states)
- [Where a thread changes state](#where-a-thread-changes-state)
- [What it costs](#what-it-costs)
- [The profile](#the-profile)
- [A snapshot of every thread](#a-snapshot-of-every-thread)
- [What it leaves out](#what-it-leaves-out)

## What it answers

**Decided.** Profiling answers three questions about a transfer:

1. Where the time goes: how long each thread of the client and of each server
   spent on each kind of work.
2. Who waits for whom: which side, and which queue, the others wait for.
3. When a transfer hangs, where each thread is, and for how long it has been
   there.

## Threads and states

**Decided.** Ferry keeps time as a kernel does: at the few places every
piece of work passes, not around each piece of work.

- Each thread of Ferry's registers with a role, such as `walk`, `compress`,
  or `pump`. Threads of the same role are added up as one.
- A registered thread is always in exactly one state: running some named
  work, such as `stat entries` or `compress`, or waiting for something named,
  such as `wait for the compressors` or `read the connection`. Work not named
  is `run`.
- A thread changes state only where work passes: a queue, a lock, a
  connection, a pool's job. At each change, the time since the last one goes
  to the state the thread leaves. States nest: the time of the inner one is
  its own, and the outer one picks up again when it ends.
- For each state, a role keeps the total time, how many stays there were, the
  longest stay, and a histogram of stays in powers of two, from which the
  p50 and p99 are read.

Each thread writes only its own slot, with a load and a store rather than a
read-modify-write, so threads never contend over it. A thread's slot is
folded into its role's totals when the thread ends.

**Decided.** The state each thread is in, and since when, is always kept,
whether or not the transfer is profiled, so that a transfer that hangs can say
where its threads are without having been started for it; see
[A snapshot of every thread](#a-snapshot-of-every-thread). It is kept with the
coarse clock on Linux, which costs a few nanoseconds and moves every few
milliseconds. The time in each state is kept only with `--profile`: each
change then reads the precise clock.

## Where a thread changes state

**Decided.** These are the places, and the states they put a thread in:

| Where | Running | Waiting |
|---|---|---|
| Pools (compress, decompress, compare) | the pool's work | for a job; a submitter for room in the queue; a collector for the next result |
| The walk | `list directories`, `stat entries`, a directory at a time | for a directory to read; for room to read ahead; the sender for the next directory; for the walk's lock |
| The mux and its pump | | for room for the manifest; for room for data; the pump for packets, and writing them |
| The striped transport | | each path's writer for pieces, and writing them; each path's reader reading, and waiting for the receiver; a receiver for the next piece |
| The relay | | reading from one server, writing to the other |
| The server | | reading the connection; writing to the peer; reading from the peer |
| The Source | `read files`, `look up owners' names` | for the readers; for files to send |
| The Destination | `compare`, `apply comparisons`, `write files`, `rename files` | for the record stream; for the writers; for a writer; for a file's bytes; for the writers to finish |

## What it costs

**Decided.** Profiling must cost no more than 2% of a transfer's time.

Measured on a 24-core Linux host (an i7-13700K, whose clock source is the
TSC), both sides local, five interleaved runs of each, medians:

| Transfer | Before profiling | Built in, off | With `--profile` |
|---|---|---|---|
| Dry run of 10.1 million entries | 9.42 s, 40.1 s CPU | 9.29 s, 40.3 s CPU | 9.35 s, 40.6 s CPU |
| Copy of 100,000 files of 1 KiB, `--no-fsync` | 1.42 s, 17.5 s CPU | 1.33 s, 16.6 s CPU | 1.38 s, 17.2 s CPU |

The differences in time are within the spread between runs of the same
build. With `--profile`, the dry run takes about 1% more CPU, over a hundred
million changes of state. One change costs about 22 ns with 16 threads
changing at once, and about 2 ns with profiling off; on macOS, about 45 ns and
4 ns.

## The profile

**Decided.** `--profile` writes the profile to standard error when the
transfer ends: for the client, then for each server, every role, with the
states that took at least half a percent of its time, the longest first.
Times are added over the role's threads. `--profile=FILE` also writes all of
it to `FILE` as JSON, every state with its histogram, so that two runs can be
compared.

Each server sends its profile to the client, in a report, once its peer is
done; the client keeps it with the name of its side.

```text
ferry: profile
the client
  relay from the Destination ×1, 9.63 s in all
    run                                 6.02 s   63%       26,845×  p50   4.1 µs  p99   8.4 ms  max   9.0 ms
    read the connection                 3.59 s   37%        1,544×  p50  65.5 µs  p99 134.2 ms  max 250.3 ms
the Source on this machine
  walk ×16, 145.54 s in all
    wait for room to read ahead       109.66 s   75%          217×  p50   1.03 s  p99   1.03 s  max   1.03 s
    wait for work                      19.28 s   13%      133,278×  p50   8.2 µs  p99  32.8 µs  max 935.6 ms
    list directories                    7.31 s    5%       24,385×  p50   8.2 µs  p99  65.5 µs  max 496.3 ms
    stat entries                        6.31 s    4%       24,376×  p50   4.1 µs  p99 131.1 µs  max 755.1 ms
  manifest ×1, 9.10 s in all
    wait for the next directory         7.74 s   85%        3,151×  p50  32.8 µs  p99 131.1 µs  max 949.3 ms
```

A p50 or p99 is the upper bound of its bucket in the histogram, so it is a
power of two of nanoseconds, but never more than the longest stay.

## A snapshot of every thread

**Decided.** The client says where every thread is, in lines on standard
error, when:

- it gets `SIGQUIT`, as Ctrl-\ sends; or
- nothing has passed between the servers for 60 s, once each time the
  transfer stands still.

It says where its own threads are, and asks each server on its control
connection, which the server answers there, so that the answer does not wait
behind the transfer's own packets. Threads of the same role in the same state
make one line, with the longest any has been in it:

```text
ferry: where every thread is, as asked:
ferry: the client: relay from the Source ×1: read the connection, for 688.0 ms
ferry: the Source on this machine: walk ×15: wait for room to read ahead, for up to 777.0 ms
ferry: the Source on this machine: manifest ×1: wait for the next directory, for 688.0 ms
ferry: the Destination on this machine: receive ×1: read the connection, for 541.0 ms
```

`SIGQUIT` stays blocked in the `ssh` processes Ferry starts, so Ctrl-\ asks
for a snapshot without ending them; a local server takes it and does nothing.

## What it leaves out

- **Which function the time goes to.** The states say what a thread does,
  not which code; for that, a sampling profiler such as `perf` is the tool.
- **Each entry on its own.** Changing state for each of millions of entries
  would cost more than the work it times.
- **A timeline.** A record of each change, for a trace viewer, is not built;
  it could be added later, in a ring buffer for each thread.
