# Ferry dashboard

Status: **draft, under discussion.** Each section says whether it is decided or
proposed. The [architecture](architecture.md) describes the client, which
shows the dashboard, and the reports the servers send it; the
[pipeline](pipeline.md), the stages it shows.

**Decided.** While a transfer runs on a terminal, the client shows a
dashboard: a full screen that updates in place. It is for you to follow the
transfer, and for finding what holds it back: how far it has got, how fast
each SSH connection goes, and which stage of the pipeline the others wait
for.

```text
ferry: ./data → ntu:/backup                                     00:42 elapsed
 ████████████████░░░░░░░░░░░░  58%   612 MiB of 1.0 GiB   ETA 00:31

 compared   10,000 entries        to send   8,000 files
 sent        4,620 files          failed        0

 SSH connections to ntu           rate       heartbeat
   1 of 4                      9.8 MiB/s     2 s ago
   2 of 4                     10.1 MiB/s     1 s ago
   3 of 4                      9.6 MiB/s     3 s ago
   4 of 4                      9.9 MiB/s     2 s ago

 pipeline   read ▮▮▯▯  compress ▮▯▯▯  network ▮▮▮▮  decompress ▮▯▯▯  write ▮▯▯▯
 bottleneck: network          compression level 1, 41% saved

 warnings
   cannot read sub/locked.txt: permission denied
```

## What it shows

**Decided.**

- **The whole transfer:** a bar, the bytes sent out of the bytes to send,
  the rate, and the time left.
- **The counts:** entries compared, files to send, files sent, and files
  that failed.
- **Each SSH connection:** its rate, and how long ago its last
  [heartbeat](architecture.md#connections) came, grouped by host. A
  location on this machine has none.
- **The pipeline:** how full the queue before each stage is, from reading
  to writing, and which stage is the bottleneck.
- **Warnings and failures:** the latest few, as they come.

**Not shown:** the files being sent. With many small files, the names would
change too fast to read.

## How it behaves

**Decided.**

- **On by default.** Where standard error is a terminal, a transfer shows
  the dashboard. `--no-dashboard` shows the progress line instead, as
  before the dashboard.
- **A full screen of its own.** The dashboard takes the terminal's
  alternate screen. When the transfer ends, however it ends, the terminal
  is put back as it was, and what stays is the summary line, then every
  warning and failure, written to standard error, so that they stay in the
  scrollback.
- **Nothing is printed over it.** Warnings and failures that would go to
  standard error during the transfer are shown in the dashboard, and
  written out when it closes.
- **A dry run has none.** Its listing, on standard output, is what it is
  for, and it is quick; it shows the summary as before.
- **Off a terminal, as before.** Where standard error is not a terminal,
  nothing is shown, or the progress line every few seconds with
  `--progress`.
- **Ctrl-C cancels the transfer,** as it does without the dashboard. No
  other key does, so that none cancels it by mistake.
- **It starts with the transfer.** Every login happens while preparing,
  before the dashboard opens, so no prompt ever lands in it.

## Where the numbers come from

**Decided.**

- **Counts** come from the peers' reports, as for the progress line.
- **The pipeline's queues** come from the peers too: each server reports how
  full its queues are, in a new kind of report. The Source has the queues
  before reading and compressing, the Destination those before decompressing
  and writing.
- **Each SSH connection's rate and heartbeat** come from the client's own
  transport, which carries every path and heartbeat channel.
- It is drawn with `ratatui` over `crossterm`.

**Proposed.**

- **The queue before the network** is the Source's queue of data packets
  waiting for the connection.
- **The bottleneck** is the first stage whose queue stays mostly full while
  the queue of the stage after it stays mostly empty, over the last few
  seconds.
- **The time left** is the bytes left over the rate of the last few
  seconds.
