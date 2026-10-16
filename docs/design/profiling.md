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
- [Following a transfer as it goes](#following-a-transfer-as-it-goes)
- [A snapshot of every thread](#a-snapshot-of-every-thread)
- [What it leaves out](#what-it-leaves-out)

## What it answers

**Decided.** Profiling answers three questions about a transfer:

1. Where the time goes: how long each thread of the client and of each server
   spent on each kind of work.
2. Who waits for whom: which side, and which queue, the others wait for.
3. When a transfer hangs, where each thread is, and for how long it has been
   there.
4. As the transfer goes, a second at a time, where each side is held back:
   what the client follows, and will steer the transfer by.

## Threads and states

**Decided.** Ferry keeps time as a kernel does: at the few places every
piece of work passes, not around each piece of work.

- Each thread of Ferry's registers with a role, such as `scan`, `compress`,
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

**Decided.** Profiling is always on. It is part of what Ferry is: the client
follows a transfer by it, and it costs too little to leave out; see
[What it costs](#what-it-costs). It is not a user's to turn off. Only tests
and benchmarks do, to measure what it costs, with a hidden `--no-profile`,
which turns all of it off, on the client and the servers: no thread is
registered, nothing is timed, no metrics are sent, and there is no
snapshot. Nothing is estimated in its place.

## Where a thread changes state

**Decided.** These are the places, and the states they put a thread in:

| Where | Running | Waiting |
|---|---|---|
| Pools (stat, compress, decompress) | the pool's work | for a job; a submitter for room in the queue; a collector for the next result |
| The scan | `list directories`, `stat entries` | the lister for the stat threads; the sender for the next result |
| The mux and its pump | | for room for messages; for room for data; the pump for packets, and writing them |
| The striped transport | | each path's writer for pieces, and writing them; each path's reader reading, and waiting for the receiver; a receiver for the next piece |
| The relay | | reading from one server, writing to the other |
| The server | | reading the connection; writing to the peer; reading from the peer |
| The Source | `read files`, `look up owners' names` | for the readers; for files to send |
| The Destination | `list directories`, `compare`, `apply comparisons`, `write files`, `rename files` | for the compare thread; for the record stream; for the writers; for a writer; for a file's bytes; for the writers to finish |

## What it costs

**Decided.** Profiling must cost no more than 2% of a transfer's time.

Measured on a 24-core Linux host (an i7-13700K, whose clock source is the
TSC), both sides local, five interleaved runs of each, medians:

| Transfer | Before profiling | Built in, not timing | Timing |
|---|---|---|---|
| Dry run of 10.1 million entries | 9.42 s, 40.1 s CPU | 9.29 s, 40.3 s CPU | 9.35 s, 40.6 s CPU |
| Copy of 100,000 files of 1 KiB, `--no-fsync` | 1.42 s, 17.5 s CPU | 1.33 s, 16.6 s CPU | 1.38 s, 17.2 s CPU |

The differences in time are within the spread between runs of the same
build. Timing, the dry run takes about 1% more CPU, over a hundred
million changes of state. One change costs about 22 ns with 16 threads
changing at once, and about 2 ns with profiling off; on macOS, about 45 ns and
4 ns.

Always on, with metrics sent every second and the client following them, it
was measured again against `--no-profile`, medians of interleaved runs:

| Transfer | `--no-profile` | Always on |
|---|---|---|
| 100,000 files of 1 KiB on tmpfs, `--no-fsync --no-compress`, the worst case, on the Linux host | 0.91 s, 10.7 s CPU | 0.90 s, 10.5 s CPU |
| Dry run of 10.1 million entries, on the Linux host | 2.87 s, 17.0 s CPU | 2.80 s, 17.0 s CPU |
| 100,000 files of 1 KiB, on a Mac with an M-series chip | 23.07 s, 131 s CPU | 22.93 s, 129 s CPU |

No difference shows, in time or CPU, beyond the spread between runs.

## The profile

**Decided.** When the transfer ends, where the time went follows the
summary, wherever the summary is shown: after the dashboard, after the
progress line, or, where standard error is not a terminal, with
`--progress`. It is short: for the client, then for each server, each role
on a line, with its time, added over its threads, and the states that took
at least a twentieth of it, the longest first, four at most. A dry run of
10 million entries says it in under 30 lines. `--profile-file FILE` also
writes all of it to `FILE` as JSON, every state with its count, its longest
stay, and its histogram, so that two runs can be compared; it is written
whether or not the summary is shown. Shortened here:

Each server sends its profile to the client, in a report, once its peer is
done; the client keeps it with the name of its side.

```text
ferry: 10,141,738 entries compared, 10,116,010 files to send, in 2.9 s
ferry: where the time went
the client
  relay from the Source ×1, 2.92 s: read the connection 95%
  relay from the Destination ×1, 2.92 s: read the connection 82% · run 17%
the Source on this machine
  scan ×1, 2.88 s: list directories 83% · wait for the stat threads 14%
  stat ×8, 23.02 s: wait for work 59% · stat entries 41%
  manifest ×1, 2.88 s: wait for the next result 64% · run 36%
the Destination on this machine
  compare ×1, 2.91 s: wait for work 99%
  receive ×1, 2.91 s: apply comparisons 44% · read the connection 42% · run 13%
```

A p50 or p99 is the upper bound of its bucket in the histogram, so it is a
power of two of nanoseconds, but never more than the longest stay.

## Following a transfer as it goes

**Decided.** The client follows where the threads of each side spend their
time as the transfer goes, a second at a time.

- **Each server sends its metrics every second,** on its control
  connection, which never waits behind the transfer's data: for each role,
  how many threads it has, and the time in each state, and how many stays,
  over the second just past. States go by number, since a client and its
  servers are the same build. A second's metrics are a few hundred bytes.
- **They say how full each queue was, too:** for each queue of the
  process, what waited in it at the second's end, the most that did during
  the second, and its room. A server's queues are its messages, its
  manifest, by bytes, and its data, before the connection; the queues before
  its stages, from reading to writing; each path's queue, of pieces
  waiting to be written, as `send path N`; and the pieces received and not
  yet taken, as `receive`. The client's are its paths' and its `receive`,
  for each side it relays to. A queue is counted as things go into it, so
  one that shows more than its room has someone waiting to put more in.
  Counting costs an atomic add as each thing goes in or out; the peak, one
  more; reading them, a few loads a second.
- **The client takes its own** every second, and keeps each side's last 10
  seconds. It is the one place where both sides, and the network between
  them through the client, are seen at once. How full each side's queues
  were, it also shows on the dashboard, and keeps, every second of the
  run, for `--profile-file`, under `queues`: a list of each side's seconds,
  each with when it came, since the transfer started, and each queue's
  level, peak, and room.
- **What it sees goes to a policy,** each time a side's second comes. The
  policy is what will steer the transfer, such as its compression level or
  how many threads a stage has, by orders the client sends each server on
  its control connection; a server will keep hard limits of its own, such
  as the 8 stat threads that bound a shared filesystem's load, whatever it
  is told. No policy steers anything yet: they, and the orders, are to be
  designed. Until then, the client only watches.

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
ferry: the client: relay from the Source ×1: write the connection, for 9 ms
ferry: the Source on this machine: scan ×1: list directories, for 176 ms
ferry: the Source on this machine: stat ×8: wait for work, for up to 103 ms
ferry: the Source on this machine: manifest ×1: wait for the next result, for 91 ms
ferry: the Destination on this machine: compare ×1: wait for work, for 9 ms
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
- **CPU time.** A state's time is wall-clock time: a thread that stats on NFS
  is in `stat entries` while it waits for the server. Telling work from
  waiting inside a state would take each thread's CPU clock at each change,
  which costs more; it waits until a policy needs it.
- **Steering.** See [Following a transfer as it goes](#following-a-transfer-as-it-goes).
